//! Asking the servers about a file without downloading it.
//!
//! Every download starts here: each source gets one probe request, and the
//! answers decide which sources are used. [`crate::Engine::inspect`] exposes
//! the same step on its own, for `hiroi info` and for user interfaces that
//! want to show a file's size and name before the download starts.

use std::sync::Arc;
use std::time::Duration;

use reqwest::Url;
use tokio_util::sync::CancellationToken;

use crate::constants::DEFAULT_THROTTLE_PAUSE;
use crate::error::{EngineError, Result};
use crate::net::probe::{self, RemoteFile};
use crate::net::retry::{FetchError, RetryAction, is_throttle};
use crate::net::source::{Source, SourcePool};
use crate::net::{display_url, host_key};
use crate::request::DownloadRequest;
use crate::storage::naming::sanitize_file_name;
use crate::task::{EngineShared, or_cancel};

/// What the servers said about a download.
#[derive(Debug, Clone)]
pub struct RemoteInfo {
    /// Name the file would get, already sanitized.
    pub file_name: Option<String>,
    pub size: Option<u64>,
    /// Whether the download can use several connections and be resumed.
    pub resumable: bool,
    pub content_type: Option<String>,
    /// One entry per URL of the request, the main URL first.
    pub sources: Vec<SourceInfo>,
}

/// The answer of one server.
#[derive(Debug, Clone)]
pub struct SourceInfo {
    /// Display form of the URL, without credentials or query string.
    pub url: String,
    /// Where redirects led, in the same display form.
    pub final_url: Option<String>,
    pub size: Option<u64>,
    pub resumable: bool,
    /// Whether a download would use this source. Mirrors that disagree
    /// with the main source on size or range support are left out.
    pub usable: bool,
    pub error: Option<String>,
}

/// Called before each retry with the reason and the delay.
pub(crate) type RetryHook<'a> = &'a (dyn Fn(&str, Duration) + Sync);

/// Probes every source of `request` at the same time.
pub(crate) async fn probe_all(
    shared: &EngineShared,
    request: &DownloadRequest,
    cancel: &CancellationToken,
    on_retry: RetryHook<'_>,
) -> Vec<Result<RemoteFile>> {
    let probes = request
        .sources
        .iter()
        .map(|url| probe_with_retry(shared, request, url, cancel, on_retry));
    futures_util::future::join_all(probes).await
}

/// Picks the sources a download uses. The first source that answered
/// defines the file; mirrors must agree with it.
pub(crate) fn select(
    request: &DownloadRequest,
    results: Vec<Result<RemoteFile>>,
) -> Result<(SourcePool, RemoteFile)> {
    let mut reference: Option<RemoteFile> = None;
    let mut sources = Vec::new();
    let mut first_error = None;
    for (url, result) in request.sources.iter().zip(results) {
        match result {
            Ok(remote) => match &reference {
                None => {
                    sources.push(Arc::new(Source::new(url.clone(), &remote)));
                    reference = Some(remote);
                }
                Some(base) if agrees(base, &remote) => {
                    sources.push(Arc::new(Source::new(url.clone(), &remote)));
                }
                Some(_) => {
                    tracing::warn!(mirror = %display_url(url), "mirror differs from the main source, ignoring it");
                }
            },
            Err(EngineError::Cancelled) => return Err(EngineError::Cancelled),
            Err(error) => {
                tracing::warn!(source = %display_url(url), %error, "source unavailable");
                first_error.get_or_insert(error);
            }
        }
    }
    match reference {
        Some(remote) => Ok((SourcePool::new(sources), remote)),
        None => Err(first_error.unwrap_or(EngineError::NoSources("no sources given".into()))),
    }
}

/// Builds the public summary of a probe round.
pub(crate) fn describe(request: &DownloadRequest, results: &[Result<RemoteFile>]) -> RemoteInfo {
    let reference = results.iter().find_map(|result| result.as_ref().ok());
    let sources = request
        .sources
        .iter()
        .zip(results)
        .map(|(url, result)| match result {
            Ok(remote) => {
                let main = reference.is_some_and(|base| std::ptr::eq(base, remote));
                let disagreement = reference
                    .filter(|_| !main)
                    .and_then(|base| disagreement(base, remote));
                SourceInfo {
                    url: display_url(url),
                    final_url: Some(display_url(&remote.final_url)),
                    size: remote.size,
                    resumable: remote.ranges,
                    usable: disagreement.is_none(),
                    error: disagreement.map(str::to_string),
                }
            }
            Err(error) => SourceInfo {
                url: display_url(url),
                final_url: None,
                size: None,
                resumable: false,
                usable: false,
                error: Some(error.to_string()),
            },
        })
        .collect();
    let name = request
        .file_name
        .as_deref()
        .or(reference.and_then(|r| r.file_name.as_deref()));
    RemoteInfo {
        file_name: name.and_then(sanitize_file_name),
        size: reference.and_then(|r| r.size),
        resumable: reference.is_some_and(|r| r.ranges && r.size.is_some_and(|size| size > 0)),
        content_type: reference.and_then(|r| r.content_type.clone()),
        sources,
    }
}

fn agrees(base: &RemoteFile, mirror: &RemoteFile) -> bool {
    disagreement(base, mirror).is_none()
}

/// Why a mirror cannot be combined with the main source.
fn disagreement(base: &RemoteFile, mirror: &RemoteFile) -> Option<&'static str> {
    if !base.ranges {
        Some("the main source does not support ranges")
    } else if !mirror.ranges {
        Some("this mirror does not support ranges")
    } else if base.size != mirror.size {
        Some("size differs from the main source")
    } else {
        None
    }
}

async fn probe_with_retry(
    shared: &EngineShared,
    request: &DownloadRequest,
    url: &Url,
    cancel: &CancellationToken,
    on_retry: RetryHook<'_>,
) -> Result<RemoteFile> {
    let gate = shared.hosts.gate(&host_key(url));
    let mut attempt = 0;
    loop {
        let result = {
            let _host = or_cancel(cancel, gate.acquire()).await?;
            let probe = probe::probe(&shared.client, url, &request.headers);
            or_cancel(cancel, probe).await?
        };
        let error = match result {
            Ok(remote) => return Ok(remote),
            Err(error) => error,
        };
        if let FetchError::Status {
            status,
            retry_after,
        } = &error
            && is_throttle(*status)
        {
            gate.throttle(retry_after.unwrap_or(DEFAULT_THROTTLE_PAUSE));
        }
        attempt += 1;
        match shared.config.retry.action(&error, attempt, true) {
            RetryAction::Retry(delay) => {
                on_retry(&error.to_string(), delay);
                or_cancel(cancel, tokio::time::sleep(delay)).await?;
            }
            _ => return Err(error.into_engine_error(&display_url(url))),
        }
    }
}
