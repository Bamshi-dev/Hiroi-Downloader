//! One connection of a segmented download.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use futures_util::StreamExt;

use super::or_cancel;
use super::segmented::Transfer;
use super::segments::Segment;
use crate::constants::{SLOW_CONNECTION_RATIO, WRITE_BUFFER_SIZE};
use crate::error::{EngineError, Result};
use crate::net::protocol::{self, Body, Want};
use crate::net::retry::{FetchError, FetchResult, RetryAction};
use crate::net::source::Source;

/// Keeps taking segments until there is nothing left worth taking.
/// `slot` identifies the connection in the speed table of the transfer.
pub(super) async fn run(transfer: Arc<Transfer>, slot: usize) -> Result<()> {
    while let Some(segment) = transfer.table.claim() {
        let result = download_segment(&transfer, &segment, slot).await;
        segment.release();
        result?;
    }
    Ok(())
}

/// Fetches one segment, retrying and switching sources as the policy says.
async fn download_segment(transfer: &Transfer, segment: &Segment, slot: usize) -> Result<()> {
    let policy = &transfer.ctx.shared.config.retry;
    let mut attempt = 0;
    loop {
        let source = transfer
            .pool
            .pick()
            .ok_or_else(|| transfer.pool.exhausted())?;
        let before = segment.written();
        let error = match fetch(transfer, segment, &source, slot).await {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        attempt = if segment.written() > before {
            1
        } else {
            attempt + 1
        };

        match policy.action(&error, attempt, source.was_refreshed()) {
            RetryAction::Retry(delay) => {
                tracing::debug!(source = %source.display(), %error, ?delay, "retrying");
                transfer
                    .ctx
                    .observer
                    .retrying(transfer.id, &error.to_string(), delay);
                or_cancel(&transfer.cancel, tokio::time::sleep(delay)).await?;
            }
            RetryAction::Refresh => {
                if !refresh(transfer, &source).await? {
                    transfer
                        .pool
                        .drop_source(&source, &error.to_string(), false);
                }
            }
            RetryAction::DropSource => {
                let changed = matches!(error, FetchError::Changed);
                transfer
                    .pool
                    .drop_source(&source, &error.to_string(), changed);
                attempt = 0;
            }
            RetryAction::Abort => return Err(error.into_engine_error(&source.display())),
        }
    }
}

/// One ranged request for whatever is still missing of the segment.
async fn fetch(
    transfer: &Transfer,
    segment: &Segment,
    source: &Arc<Source>,
    slot: usize,
) -> FetchResult<()> {
    let Some((start, end)) = segment.begin() else {
        return Ok(());
    };
    let shared = &transfer.ctx.shared;
    let cancel = &transfer.cancel;

    let _connection = or_cancel(cancel, shared.connections.acquire())
        .await?
        .map_err(|_| FetchError::Cancelled)?;
    let gate = shared.hosts.gate(source.host());
    let _host = or_cancel(cancel, gate.acquire()).await?;
    let _source = source.track();
    let _active = ActiveConnection::new(transfer);

    let want = Want::Range {
        start,
        end,
        total: transfer.total_size,
    };
    let headers = &transfer.ctx.request.headers;
    let body = or_cancel(
        cancel,
        protocol::open(shared, &transfer.ctx.client, source, headers, &gate, want),
    )
    .await??;
    receive(transfer, segment, body, slot).await
}

async fn receive(
    transfer: &Transfer,
    segment: &Segment,
    mut body: Body,
    slot: usize,
) -> FetchResult<()> {
    let ctx = &transfer.ctx;
    let mut buffer = Vec::with_capacity(WRITE_BUFFER_SIZE);
    let mut writer = Writer::new(transfer, segment);
    let mut speed = SpeedCheck::new(transfer, slot);

    let outcome = loop {
        let next = tokio::select! {
            biased;
            () = transfer.cancel.cancelled() => break Err(FetchError::Cancelled),
            next = body.next() => next,
        };
        let chunk = match next {
            Some(Ok(chunk)) => chunk,
            Some(Err(error)) => break Err(error),
            None if segment.fully_reserved() => break Ok(()),
            None => break Err(FetchError::ClosedEarly),
        };

        let limited = async {
            ctx.rate.acquire(chunk.len()).await;
            ctx.shared.global_rate.acquire(chunk.len()).await;
        };
        if or_cancel(&transfer.cancel, limited).await.is_err() {
            break Err(FetchError::Cancelled);
        }

        let accepted = segment.reserve(chunk.len());
        buffer.extend_from_slice(&chunk[..accepted]);
        if buffer.len() >= WRITE_BUFFER_SIZE {
            buffer = writer.write(buffer).await?;
        }
        // Done, or another connection took over the rest of this segment.
        if segment.fully_reserved() {
            break Ok(());
        }
        if speed.record(accepted, segment) {
            tracing::debug!(slot, "replacing a slow connection");
            break Err(FetchError::Slow);
        }
    };

    // Bytes that arrived before a failure are still good: keep them.
    writer.write(buffer).await?;
    writer.finish().await?;
    outcome
}

/// Writes one buffer while the connection fills the next, so the
/// network is not idle while the disk works. At most one write is in
/// flight, and bytes count as written only once the write returned.
struct Writer<'a> {
    transfer: &'a Transfer,
    segment: &'a Segment,
    /// Where the next buffer goes: after everything written or in flight.
    offset: u64,
    pending: Option<(usize, tokio::task::JoinHandle<std::io::Result<Vec<u8>>>)>,
}

impl<'a> Writer<'a> {
    fn new(transfer: &'a Transfer, segment: &'a Segment) -> Self {
        Self {
            transfer,
            segment,
            offset: segment.write_offset(),
            pending: None,
        }
    }

    /// Starts writing `buffer` and hands back an empty one to fill.
    async fn write(&mut self, buffer: Vec<u8>) -> FetchResult<Vec<u8>> {
        let spare = self.finish().await?;
        if buffer.is_empty() {
            return Ok(buffer);
        }
        let len = buffer.len();
        let write = self.transfer.file.start_write(buffer, self.offset);
        self.offset += len as u64;
        self.pending = Some((len, write));
        Ok(spare.unwrap_or_else(|| Vec::with_capacity(WRITE_BUFFER_SIZE)))
    }

    /// Waits for the write in flight, if any, and returns its buffer.
    async fn finish(&mut self) -> FetchResult<Option<Vec<u8>>> {
        let Some((len, write)) = self.pending.take() else {
            return Ok(None);
        };
        let path = self.transfer.file.path();
        let mut buffer = write
            .await
            .map_err(|e| EngineError::io(path, std::io::Error::other(e)))?
            .map_err(|e| EngineError::io(path, e))?;
        self.segment.commit(len);
        buffer.clear();
        Ok(Some(buffer))
    }
}

/// Resolves the original link again, e.g. after a signed URL expired.
/// Returns whether the source is usable again.
async fn refresh(transfer: &Transfer, source: &Source) -> Result<bool> {
    source.mark_refreshed();
    let ctx = &transfer.ctx;
    let probe = protocol::probe(
        &ctx.shared,
        &ctx.client,
        source.original(),
        &ctx.request.headers,
    );
    let fresh = match or_cancel(&transfer.cancel, probe).await? {
        Ok(remote) => remote,
        Err(_) => return Ok(false),
    };
    let same_file =
        fresh.size == Some(transfer.total_size) && fresh.validator.as_deref() == source.validator();
    if same_file {
        tracing::info!(source = %source.display(), "link refreshed");
        source.refresh_to(fresh.final_url);
    }
    Ok(same_file)
}

/// Measures the speed of one connection over fixed windows and decides
/// when it is slow enough to be worth a fresh connection.
struct SpeedCheck<'a> {
    transfer: &'a Transfer,
    slot: usize,
    window_start: Instant,
    window_bytes: u64,
    opened: Instant,
    total: u64,
    flagged: bool,
}

/// Connections that moved less than this say little about the server.
const MIN_REFERENCE_BYTES: u64 = 64 * 1024;

impl<'a> SpeedCheck<'a> {
    fn new(transfer: &'a Transfer, slot: usize) -> Self {
        let now = Instant::now();
        Self {
            transfer,
            slot,
            window_start: now,
            window_bytes: 0,
            opened: now,
            total: 0,
            flagged: false,
        }
    }

    /// Records `len` received bytes. Returns true when the connection
    /// should be dropped.
    fn record(&mut self, len: usize, segment: &Segment) -> bool {
        self.window_bytes += len as u64;
        self.total += len as u64;
        let elapsed = self.window_start.elapsed();
        let config = &self.transfer.ctx.shared.config;
        if elapsed < config.slow_connection_window {
            return false;
        }
        let rate = (self.window_bytes as f64 / elapsed.as_secs_f64()) as u64;
        self.transfer.rates[self.slot].store(rate.max(1), Ordering::Relaxed);
        self.window_start = Instant::now();
        self.window_bytes = 0;

        // Reconnecting costs a round trip; only worth it when this
        // connection would need more than another window to finish.
        let window = config.slow_connection_window.as_secs_f64();
        let still_long = segment.unreserved() as f64 > rate as f64 * window;
        self.flagged = still_long && self.is_slow(rate);
        self.flagged
    }

    fn is_slow(&self, rate: u64) -> bool {
        let ctx = &self.transfer.ctx;
        let config = &ctx.shared.config;
        // Our own speed limit slows connections on purpose.
        let limited = ctx.rate.limit().is_some() || ctx.shared.global_rate.limit().is_some();
        if !config.replace_slow_connections || limited {
            return false;
        }
        if config.min_connection_speed.is_some_and(|min| rate < min) {
            return true;
        }
        let others: Vec<u64> = self
            .transfer
            .rates
            .iter()
            .enumerate()
            .filter(|&(slot, _)| slot != self.slot)
            .map(|(_, rate)| rate.load(Ordering::Relaxed))
            .filter(|&rate| rate > 0)
            .collect();
        let expected = if others.is_empty() {
            self.transfer.reference_rate.load(Ordering::Relaxed)
        } else {
            others.iter().sum::<u64>() / others.len() as u64
        };
        expected > 0 && rate.saturating_mul(SLOW_CONNECTION_RATIO) < expected
    }
}

impl Drop for SpeedCheck<'_> {
    fn drop(&mut self) {
        self.transfer.rates[self.slot].store(0, Ordering::Relaxed);
        let elapsed = self.opened.elapsed().as_secs_f64();
        if self.flagged || self.total < MIN_REFERENCE_BYTES || elapsed <= 0.0 {
            return;
        }
        // Moving average over finished connections, newest weighted 1/4.
        let rate = (self.total as f64 / elapsed) as u64;
        let reference = &self.transfer.reference_rate;
        let old = reference.load(Ordering::Relaxed);
        let new = if old == 0 { rate } else { (old * 3 + rate) / 4 };
        reference.store(new, Ordering::Relaxed);
    }
}

/// Counts connections that are transferring right now.
struct ActiveConnection<'a> {
    transfer: &'a Transfer,
}

impl<'a> ActiveConnection<'a> {
    fn new(transfer: &'a Transfer) -> Self {
        transfer.active.fetch_add(1, Ordering::Relaxed);
        Self { transfer }
    }
}

impl Drop for ActiveConnection<'_> {
    fn drop(&mut self) {
        self.transfer.active.fetch_sub(1, Ordering::Relaxed);
    }
}
