//! One connection of a segmented download.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures_util::StreamExt;
use reqwest::header::{CONTENT_RANGE, IF_RANGE, RANGE};
use reqwest::{Response, StatusCode};

use super::or_cancel;
use super::segmented::Transfer;
use super::segments::Segment;
use crate::constants::{DEFAULT_THROTTLE_PAUSE, WRITE_BUFFER_SIZE};
use crate::error::Result;
use crate::net::headers::parse_content_range;
use crate::net::probe::{self, retry_after};
use crate::net::retry::{FetchError, FetchResult, RetryAction, is_throttle};
use crate::net::source::Source;

/// Keeps taking segments until there is nothing left worth taking.
pub(super) async fn run(transfer: Arc<Transfer>) -> Result<()> {
    while let Some(segment) = transfer.table.claim() {
        let result = download_segment(&transfer, &segment).await;
        segment.release();
        result?;
    }
    Ok(())
}

/// Fetches one segment, retrying and switching sources as the policy says.
async fn download_segment(transfer: &Transfer, segment: &Segment) -> Result<()> {
    let policy = &transfer.ctx.shared.config.retry;
    let mut attempt = 0;
    loop {
        let source = transfer
            .pool
            .pick()
            .ok_or_else(|| transfer.pool.exhausted())?;
        let before = segment.written();
        let error = match fetch(transfer, segment, &source).await {
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
async fn fetch(transfer: &Transfer, segment: &Segment, source: &Arc<Source>) -> FetchResult<()> {
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

    let mut request = shared
        .client
        .get(source.url())
        .headers(transfer.ctx.request.headers.clone())
        .header(RANGE, format!("bytes={start}-{}", end - 1));
    // If the file changed, the server answers 200 instead of mixing versions.
    if let Some(validator) = source.validator() {
        request = request.header(IF_RANGE, validator);
    }
    let response = or_cancel(cancel, request.send())
        .await?
        .map_err(FetchError::Transport)?;

    match response.status() {
        StatusCode::PARTIAL_CONTENT => {}
        StatusCode::OK | StatusCode::RANGE_NOT_SATISFIABLE => return Err(FetchError::Changed),
        status => {
            let retry_after = retry_after(&response);
            if is_throttle(status) {
                gate.throttle(retry_after.unwrap_or(DEFAULT_THROTTLE_PAUSE));
            }
            return Err(FetchError::Status {
                status,
                retry_after,
            });
        }
    }
    check_range(&response, start, transfer.total_size)?;
    receive(transfer, segment, response).await
}

/// The server must send exactly the bytes we asked for, of the file we know.
fn check_range(response: &Response, start: u64, total_size: u64) -> FetchResult<()> {
    let header = response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| FetchError::Protocol("206 response without Content-Range".into()))?;
    let range = parse_content_range(header)
        .ok_or_else(|| FetchError::Protocol(format!("unreadable Content-Range `{header}`")))?;
    if range.total.is_some_and(|total| total != total_size) {
        return Err(FetchError::Changed);
    }
    if range.start != start {
        return Err(FetchError::Protocol(format!(
            "asked for bytes from {start}, server sent from {}",
            range.start
        )));
    }
    Ok(())
}

async fn receive(transfer: &Transfer, segment: &Segment, response: Response) -> FetchResult<()> {
    let ctx = &transfer.ctx;
    let mut body = response.bytes_stream();
    let mut buffer = Vec::with_capacity(WRITE_BUFFER_SIZE);

    let outcome = loop {
        let next = tokio::select! {
            biased;
            () = transfer.cancel.cancelled() => break Err(FetchError::Cancelled),
            next = body.next() => next,
        };
        let chunk = match next {
            Some(Ok(chunk)) => chunk,
            Some(Err(error)) => break Err(FetchError::Transport(error)),
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
            buffer = flush(transfer, segment, buffer).await?;
        }
        // Done, or another connection took over the rest of this segment.
        if segment.fully_reserved() {
            break Ok(());
        }
    };

    // Bytes that arrived before a failure are still good: keep them.
    flush(transfer, segment, buffer).await?;
    outcome
}

async fn flush(transfer: &Transfer, segment: &Segment, buffer: Vec<u8>) -> FetchResult<Vec<u8>> {
    if buffer.is_empty() {
        return Ok(buffer);
    }
    let len = buffer.len();
    let mut buffer = transfer
        .file
        .write_at(buffer, segment.write_offset())
        .await?;
    segment.commit(len);
    buffer.clear();
    Ok(buffer)
}

/// Resolves the original link again, e.g. after a signed URL expired.
/// Returns whether the source is usable again.
async fn refresh(transfer: &Transfer, source: &Source) -> Result<bool> {
    source.mark_refreshed();
    let ctx = &transfer.ctx;
    let probe = probe::probe(&ctx.shared.client, source.original(), &ctx.request.headers);
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
