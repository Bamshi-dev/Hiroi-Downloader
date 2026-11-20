//! Multi-connection download of a file whose server supports ranges.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use tokio::task::JoinSet;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;

use super::meter::SpeedMeter;
use super::segments::SegmentTable;
use super::{TaskContext, worker};
use crate::error::{EngineError, Result};
use crate::net::source::SourcePool;
use crate::storage::control::ControlFile;
use crate::storage::file::OutputFile;
use crate::storage::plan::Plan;
use reqwest::Url;

/// Everything the connections of one download share.
#[derive(Debug)]
pub(super) struct Transfer {
    pub id: crate::snapshot::DownloadId,
    pub ctx: Arc<TaskContext>,
    pub pool: Arc<SourcePool>,
    pub table: SegmentTable,
    pub file: OutputFile,
    pub total_size: u64,
    pub active: AtomicUsize,
    /// Recent speed of each connection slot, 0 while idle.
    pub rates: Vec<AtomicU64>,
    /// Typical speed of connections that finished their work, so a slow
    /// connection can be judged even when it is the last one running.
    pub reference_rate: AtomicU64,
    /// Child of the task's token: also cancelled when one connection fails
    /// for good, so the others stop too.
    pub cancel: CancellationToken,
    /// Which source wrote which bytes in this run.
    pub provenance: Provenance,
}

/// Byte ranges and the source that wrote them, so a source that sent
/// broken pieces can be found. Neighbouring ranges from the same source
/// are merged, which keeps this to a few entries per connection.
#[derive(Debug, Default)]
pub(crate) struct Provenance(parking_lot::Mutex<Vec<(u64, u64, Url)>>);

impl Provenance {
    pub fn record(&self, start: u64, end: u64, source: &Url) {
        let mut ranges = self.0.lock();
        if let Some(last) = ranges
            .iter_mut()
            .rev()
            .find(|(_, last_end, url)| *last_end == start && url == source)
        {
            last.1 = end;
            return;
        }
        ranges.push((start, end, source.clone()));
    }

    fn take(&self) -> Self {
        Self(parking_lot::Mutex::new(std::mem::take(&mut *self.0.lock())))
    }

    /// The sources that wrote any byte of `ranges`, with how many of the
    /// ranges each one touched.
    pub fn blame(&self, ranges: &[(u64, u64)]) -> Vec<(Url, usize)> {
        let mut counts: Vec<(Url, usize)> = Vec::new();
        for &(start, end) in ranges {
            let mut touched: Vec<&Url> = Vec::new();
            let recorded = self.0.lock();
            for (from, to, url) in recorded.iter() {
                if *from < end && start < *to && !touched.contains(&url) {
                    touched.push(url);
                }
            }
            for url in touched {
                match counts.iter_mut().find(|(known, _)| known == url) {
                    Some(entry) => entry.1 += 1,
                    None => counts.push((url.clone(), 1)),
                }
            }
        }
        counts
    }
}

pub(super) async fn download(
    ctx: &Arc<TaskContext>,
    pool: Arc<SourcePool>,
    total_size: u64,
    plan: &Plan,
) -> Result<Provenance> {
    let config = &ctx.shared.config;
    let connections = ctx
        .request
        .connections
        .unwrap_or(config.default_connections);
    let table = match &plan.resume {
        Some(control) => SegmentTable::restore(&control.segments, config.min_split_size),
        None => SegmentTable::new(total_size, connections, config.min_split_size),
    };
    let file = OutputFile::open(&plan.paths.part_path, plan.resume.is_none())?;
    file.reserve(total_size, config.preallocate)?;

    let control = ControlFile::new(
        pool.key(),
        total_size,
        pool.primary().validator().map(str::to_string),
    );
    let transfer = Arc::new(Transfer {
        id: ctx.id,
        ctx: Arc::clone(ctx),
        pool,
        table,
        file,
        total_size,
        active: AtomicUsize::new(0),
        rates: (0..connections).map(|_| AtomicU64::new(0)).collect(),
        reference_rate: AtomicU64::new(0),
        cancel: ctx.cancel.child_token(),
        provenance: Provenance::default(),
    });
    // Save the starting layout right away, so even an early crash resumes.
    checkpoint(&transfer, &control, plan).await?;

    let mut workers = JoinSet::new();
    for slot in 0..connections {
        workers.spawn(worker::run(Arc::clone(&transfer), slot));
    }
    let result = supervise(&transfer, &mut workers, &control, plan).await;
    let saved = checkpoint(&transfer, &control, plan).await;
    report(&transfer, &mut SpeedMeter::new(transfer.table.received()));
    result.and(saved)?;

    if transfer.table.written() != total_size {
        return Err(EngineError::Protocol(
            "transfer ended with missing data".into(),
        ));
    }
    Ok(transfer.provenance.take())
}

/// Waits for the connections while reporting progress and saving
/// checkpoints. Returns the first real error, if any.
async fn supervise(
    transfer: &Arc<Transfer>,
    workers: &mut JoinSet<Result<()>>,
    control: &ControlFile,
    plan: &Plan,
) -> Result<()> {
    let config = &transfer.ctx.shared.config;
    let mut progress_tick = interval(config.progress_interval);
    let mut checkpoint_tick = interval(config.checkpoint_interval);
    progress_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    checkpoint_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut meter = SpeedMeter::new(transfer.table.received());
    let mut failure: Option<EngineError> = None;

    loop {
        tokio::select! {
            joined = workers.join_next() => match joined {
                None => break,
                Some(Ok(Ok(()) | Err(EngineError::Cancelled))) => {}
                Some(Ok(Err(error))) => {
                    if failure.is_none() {
                        failure = Some(error);
                        transfer.cancel.cancel();
                    }
                }
                Some(Err(join_error)) => std::panic::resume_unwind(join_error.into_panic()),
            },
            _ = progress_tick.tick() => report(transfer, &mut meter),
            _ = checkpoint_tick.tick() => {
                if let Err(error) = checkpoint(transfer, control, plan).await {
                    failure.get_or_insert(error);
                    transfer.cancel.cancel();
                }
            }
        }
    }

    match failure {
        Some(error) => Err(error),
        None if transfer.ctx.cancel.is_cancelled() => Err(EngineError::Cancelled),
        None => Ok(()),
    }
}

/// Flushes data to disk first, then records it in the control file.
async fn checkpoint(transfer: &Transfer, control: &ControlFile, plan: &Plan) -> Result<()> {
    if transfer.ctx.shared.config.sync_to_disk {
        transfer.file.sync().await?;
    }
    let mut control = control.clone();
    control.segments = transfer.table.saved();
    let path = &plan.paths.control_path;
    control.save(path).map_err(|e| EngineError::io(path, e))
}

fn report(transfer: &Transfer, meter: &mut SpeedMeter) {
    let downloaded = transfer.table.received();
    let speed = meter.sample(downloaded);
    let eta = meter.eta(transfer.total_size.saturating_sub(downloaded));
    let connections = transfer.active.load(Ordering::Relaxed);
    let segments = transfer.table.snapshot();
    transfer.ctx.progress.update(|p| {
        p.downloaded = downloaded;
        p.speed = speed;
        p.eta = eta;
        p.connections = connections;
        p.segments = segments;
    });
    transfer.ctx.observer.progress(transfer.id);
}
