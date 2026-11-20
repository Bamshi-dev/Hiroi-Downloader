//! Single-connection download for servers without range support. Nothing can
//! be resumed in this mode, so a failed attempt starts over from zero.

use std::sync::Arc;
use std::time::Instant;

use futures_util::StreamExt;

use super::meter::SpeedMeter;
use super::{TaskContext, discard, or_cancel};
use crate::constants::WRITE_BUFFER_SIZE;
use crate::error::{EngineError, Result};
use crate::net::protocol::{self, Want};
use crate::net::retry::{FetchError, FetchResult, RetryAction};
use crate::net::source::{Source, SourcePool};
use crate::snapshot::SegmentSnapshot;
use crate::storage::file::OutputFile;
use crate::storage::naming::DownloadPaths;

pub(super) async fn download(
    ctx: &Arc<TaskContext>,
    pool: &SourcePool,
    size: Option<u64>,
    paths: &DownloadPaths,
) -> Result<()> {
    let source = pool.primary();
    let policy = &ctx.shared.config.retry;
    let mut attempt = 0;
    loop {
        let file = OutputFile::open(&paths.part_path, true)?;
        if let Some(size) = size {
            file.reserve(size, ctx.shared.config.preallocate)?;
        }
        let error = match fetch(ctx, source, &file, size).await {
            Ok(()) if ctx.shared.config.sync_to_disk => return file.sync().await,
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        drop(file);
        attempt += 1;
        match policy.action(&error, attempt, true) {
            RetryAction::Retry(delay) => {
                ctx.observer.retrying(ctx.id, &error.to_string(), delay);
                if let Err(cancelled) = or_cancel(&ctx.cancel, tokio::time::sleep(delay)).await {
                    discard(paths);
                    return Err(cancelled);
                }
            }
            _ => {
                discard(paths);
                return Err(error.into_engine_error(&source.display()));
            }
        }
    }
}

async fn fetch(
    ctx: &TaskContext,
    source: &Arc<Source>,
    file: &OutputFile,
    size: Option<u64>,
) -> FetchResult<()> {
    let shared = &ctx.shared;
    let _connection = or_cancel(&ctx.cancel, shared.connections.acquire())
        .await?
        .map_err(|_| FetchError::Cancelled)?;
    let gate = shared.hosts.gate(source.host());
    let _host = or_cancel(&ctx.cancel, gate.acquire()).await?;

    let headers = &ctx.request.headers;
    let open = protocol::open(shared, &ctx.client, source, headers, &gate, Want::Whole);
    let mut body = or_cancel(&ctx.cancel, open).await??;
    let mut buffer = Vec::with_capacity(WRITE_BUFFER_SIZE);
    let mut written = 0u64;
    let mut meter = SpeedMeter::new(0);
    let mut last_report = Instant::now();

    loop {
        let next = tokio::select! {
            biased;
            () = ctx.cancel.cancelled() => return Err(FetchError::Cancelled),
            next = body.next() => next,
        };
        let Some(chunk) = next.transpose()? else {
            break;
        };
        or_cancel(&ctx.cancel, async {
            ctx.rate.acquire(chunk.len()).await;
            shared.global_rate.acquire(chunk.len()).await;
        })
        .await?;
        buffer.extend_from_slice(&chunk);
        if buffer.len() >= WRITE_BUFFER_SIZE {
            let len = buffer.len() as u64;
            buffer = file.write_at(buffer, written).await?;
            buffer.clear();
            written += len;
        }
        if last_report.elapsed() >= shared.config.progress_interval {
            report(ctx, &mut meter, written + buffer.len() as u64, size);
            last_report = Instant::now();
        }
    }
    if !buffer.is_empty() {
        let len = buffer.len() as u64;
        file.write_at(buffer, written).await?;
        written += len;
    }
    report(ctx, &mut meter, written, size);

    match size {
        Some(expected) if written < expected => Err(FetchError::ClosedEarly),
        Some(expected) if written > expected => {
            Err(EngineError::Protocol("server sent more data than announced".into()).into())
        }
        _ => Ok(()),
    }
}

fn report(ctx: &TaskContext, meter: &mut SpeedMeter, downloaded: u64, size: Option<u64>) {
    let speed = meter.sample(downloaded);
    let eta = size.and_then(|size| meter.eta(size.saturating_sub(downloaded)));
    ctx.progress.update(|p| {
        p.downloaded = downloaded;
        p.speed = speed;
        p.eta = eta;
        p.connections = 1;
        p.segments = size
            .map(|end| {
                vec![SegmentSnapshot {
                    start: 0,
                    end,
                    downloaded,
                }]
            })
            .unwrap_or_default();
    });
    ctx.observer.progress(ctx.id);
}
