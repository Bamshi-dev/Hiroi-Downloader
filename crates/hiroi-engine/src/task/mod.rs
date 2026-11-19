//! One download, from probing the servers to the finished file.

mod meter;
mod segmented;
mod segments;
mod stream;
mod worker;

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use reqwest::{Client, Url};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::config::EngineConfig;
use crate::error::{EngineError, Result};
use crate::limit::{HostLimiter, RateLimiter};
use crate::net::display_url;
use crate::net::probe::{self, RemoteFile};
use crate::net::retry::{FetchError, RetryAction, is_throttle};
use crate::net::source::{Source, SourcePool};
use crate::request::{ConflictPolicy, DownloadRequest};
use crate::snapshot::{DownloadId, DownloadStatus, SegmentSnapshot};
use crate::storage::file::{self, OutputFile};
use crate::storage::naming::DownloadPaths;
use crate::storage::plan::{self, Plan};
use crate::storage::remove_if_exists;

/// State shared by every download of an engine.
#[derive(Debug)]
pub(crate) struct EngineShared {
    pub config: EngineConfig,
    pub client: Client,
    pub hosts: HostLimiter,
    pub connections: Semaphore,
    pub global_rate: RateLimiter,
}

/// How a task reports back to the engine.
pub(crate) trait TaskObserver: Send + Sync {
    fn status_changed(&self, id: DownloadId, status: DownloadStatus);
    fn progress(&self, id: DownloadId);
    fn retrying(&self, id: DownloadId, reason: &str, delay: Duration);
}

pub(crate) struct TaskContext {
    pub id: DownloadId,
    pub request: Arc<DownloadRequest>,
    pub shared: Arc<EngineShared>,
    pub progress: Arc<TaskProgress>,
    pub rate: Arc<RateLimiter>,
    pub cancel: CancellationToken,
    pub observer: Arc<dyn TaskObserver>,
}

impl std::fmt::Debug for TaskContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskContext")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// Live numbers of a task, read by the engine to build snapshots.
#[derive(Debug, Default)]
pub(crate) struct TaskProgress {
    info: Mutex<ProgressInfo>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ProgressInfo {
    pub paths: Option<DownloadPaths>,
    pub total_size: Option<u64>,
    pub downloaded: u64,
    pub speed: u64,
    pub eta: Option<Duration>,
    pub connections: usize,
    pub resumable: bool,
    pub segments: Vec<SegmentSnapshot>,
}

impl TaskProgress {
    pub fn get(&self) -> ProgressInfo {
        self.info.lock().clone()
    }

    pub fn update(&self, change: impl FnOnce(&mut ProgressInfo)) {
        change(&mut self.info.lock());
    }
}

pub(crate) async fn run(ctx: Arc<TaskContext>) -> Result<PathBuf> {
    ctx.observer
        .status_changed(ctx.id, DownloadStatus::Connecting);
    let (pool, remote) = probe_sources(&ctx).await?;
    let plan = plan::prepare(&ctx.request, &remote, &pool.key())?;
    let segmented = remote.ranges && remote.size.is_some_and(|size| size > 0);

    if let Some(control) = &plan.resume {
        tracing::info!(file = %plan.paths.file_name(), bytes = control.written(), "resuming");
    }
    ctx.progress.update(|p| {
        p.paths = Some(plan.paths.clone());
        p.total_size = remote.size;
        p.resumable = segmented;
        p.downloaded = plan.resume.as_ref().map_or(0, |control| control.written());
    });
    ctx.observer
        .status_changed(ctx.id, DownloadStatus::Downloading);

    let pool = Arc::new(pool);
    match remote.size {
        Some(0) => drop(OutputFile::open(&plan.paths.part_path, true)?),
        Some(size) if segmented => segmented::download(&ctx, pool, size, &plan).await?,
        size => stream::download(&ctx, &pool, size, &plan.paths).await?,
    }

    complete(&ctx, &plan).await
}

/// Verifies the checksum, then gives the file its real name.
async fn complete(ctx: &TaskContext, plan: &Plan) -> Result<PathBuf> {
    let paths = &plan.paths;
    if let Some(checksum) = ctx.request.checksum.clone() {
        ctx.observer
            .status_changed(ctx.id, DownloadStatus::Verifying);
        let part = paths.part_path.clone();
        let verified = tokio::task::spawn_blocking(move || checksum.verify(&part))
            .await
            .map_err(|e| EngineError::io(&paths.part_path, std::io::Error::other(e)))?;
        if let Err(error) = verified {
            // The data is wrong, so resuming it later would be pointless.
            discard(paths);
            return Err(error);
        }
    }
    let overwrite = ctx.request.conflict == ConflictPolicy::Overwrite;
    file::finalize(&paths.part_path, &paths.final_path, overwrite)?;
    if let Err(error) = remove_if_exists(&paths.control_path) {
        tracing::warn!(%error, "could not remove control file");
    }
    Ok(paths.final_path.clone())
}

/// Removes the temporary files of a download.
pub(crate) fn discard(paths: &DownloadPaths) {
    for path in [&paths.part_path, &paths.control_path] {
        if let Err(error) = remove_if_exists(path) {
            tracing::warn!(path = %path.display(), %error, "could not remove temporary file");
        }
    }
}

/// Probes all sources at once. The first one that answers defines the file;
/// mirrors that disagree on size or range support are left out.
async fn probe_sources(ctx: &TaskContext) -> Result<(SourcePool, RemoteFile)> {
    let probes = ctx
        .request
        .sources
        .iter()
        .map(|url| probe_with_retry(ctx, url));
    let results = futures_util::future::join_all(probes).await;

    let mut reference: Option<RemoteFile> = None;
    let mut sources = Vec::new();
    let mut first_error = None;
    for (url, result) in ctx.request.sources.iter().zip(results) {
        match result {
            Ok(remote) => match &reference {
                None => {
                    sources.push(Arc::new(Source::new(url.clone(), &remote)));
                    reference = Some(remote);
                }
                Some(base) if base.ranges && remote.ranges && base.size == remote.size => {
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

async fn probe_with_retry(ctx: &TaskContext, url: &Url) -> Result<RemoteFile> {
    let shared = &ctx.shared;
    let gate = shared.hosts.gate(&crate::net::host_key(url));
    let mut attempt = 0;
    loop {
        let result = {
            let _host = or_cancel(&ctx.cancel, gate.acquire()).await?;
            let probe = probe::probe(&shared.client, url, &ctx.request.headers);
            or_cancel(&ctx.cancel, probe).await?
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
            gate.throttle(retry_after.unwrap_or(crate::constants::DEFAULT_THROTTLE_PAUSE));
        }
        attempt += 1;
        match shared.config.retry.action(&error, attempt, true) {
            RetryAction::Retry(delay) => {
                ctx.observer.retrying(ctx.id, &error.to_string(), delay);
                or_cancel(&ctx.cancel, tokio::time::sleep(delay)).await?;
            }
            _ => return Err(error.into_engine_error(&display_url(url))),
        }
    }
}

/// Runs `future` unless the token is cancelled first.
pub(crate) async fn or_cancel<F: Future>(
    cancel: &CancellationToken,
    future: F,
) -> Result<F::Output> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(EngineError::Cancelled),
        output = future => Ok(output),
    }
}
