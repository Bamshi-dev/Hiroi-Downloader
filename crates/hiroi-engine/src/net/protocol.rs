//! Where protocols differ: asking a server about a file, and opening a
//! range of it. Everything after that (the segment table, retries,
//! speed limits, writing to disk) works on a stream of chunks and does
//! not care where they came from, and a mirror on another protocol can
//! share a download with an HTTP one.

use std::pin::Pin;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use reqwest::header::{CONTENT_RANGE, HeaderMap, IF_RANGE, RANGE};
use reqwest::{Response, StatusCode, Url};

use crate::constants::DEFAULT_THROTTLE_PAUSE;
use crate::limit::HostGate;
use crate::net::headers::parse_content_range;
use crate::net::probe::{self, RemoteFile, retry_after};
use crate::net::retry::{FetchError, FetchResult, is_throttle};
use crate::net::source::Source;
use crate::task::EngineShared;

/// The data of one request, chunk by chunk.
pub(crate) type Body = Pin<Box<dyn Stream<Item = FetchResult<Bytes>> + Send>>;

/// What to ask for: a byte range of a file we know the size of, or the
/// whole file from a server that cannot do ranges.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Want {
    /// `start..end` of a file of `total` bytes.
    Range {
        start: u64,
        end: u64,
        total: u64,
    },
    Whole,
}

pub(crate) async fn probe(
    shared: &EngineShared,
    url: &Url,
    headers: &HeaderMap,
) -> FetchResult<RemoteFile> {
    match url.scheme() {
        #[cfg(feature = "ftp")]
        "ftp" | "ftps" => super::ftp::probe(shared, url).await,
        _ => probe::probe(&shared.client, url, headers).await,
    }
}

/// Opens what `want` asks for. The caller holds the connection and host
/// permits; `gate` is told when the server asks us to slow down.
pub(crate) async fn open(
    shared: &EngineShared,
    source: &Source,
    headers: &HeaderMap,
    gate: &HostGate,
    want: Want,
) -> FetchResult<Body> {
    match source.url().scheme() {
        #[cfg(feature = "ftp")]
        "ftp" | "ftps" => super::ftp::open(shared, source, gate, want).await,
        _ => open_http(shared, source, headers, gate, want).await,
    }
}

async fn open_http(
    shared: &EngineShared,
    source: &Source,
    headers: &HeaderMap,
    gate: &HostGate,
    want: Want,
) -> FetchResult<Body> {
    let mut request = shared.client.get(source.url()).headers(headers.clone());
    if let Want::Range { start, end, .. } = want {
        request = request.header(RANGE, format!("bytes={start}-{}", end - 1));
        // If the file changed, the server answers 200 instead of mixing
        // versions.
        if let Some(validator) = source.validator() {
            request = request.header(IF_RANGE, validator);
        }
    }
    let response = request.send().await.map_err(FetchError::Transport)?;

    let status = response.status();
    match (want, status) {
        (Want::Range { .. }, StatusCode::PARTIAL_CONTENT) => {}
        (Want::Range { .. }, StatusCode::OK | StatusCode::RANGE_NOT_SATISFIABLE) => {
            return Err(FetchError::Changed);
        }
        (Want::Whole, status) if status.is_success() => {}
        (_, status) => {
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
    if let Want::Range { start, total, .. } = want {
        check_range(&response, start, total)?;
    }
    let body = response
        .bytes_stream()
        .map(|chunk| chunk.map_err(FetchError::Transport));
    Ok(Box::pin(body))
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
