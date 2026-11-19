//! One download, from probing the servers to the finished file.

pub(crate) mod meter;
mod segmented;
mod segments;
mod stream;
mod worker;

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use reqwest::Client;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::config::EngineConfig;
use crate::error::{EngineError, Result};
use crate::inspect;
use crate::limit::{HostLimiter, RateLimiter};
use crate::media;
use crate::net::probe::RemoteFile;
use crate::net::source::SourcePool;
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
    let on_retry = |reason: &str, delay| ctx.observer.retrying(ctx.id, reason, delay);
    let results = inspect::probe_all(&ctx.shared, &ctx.request, &ctx.cancel, &on_retry).await;
    let (pool, remote) = inspect::select(&ctx.request, results)?;
    if let Some(kind) = media::detect(&remote) {
        return media::download(&ctx, remote.final_url.clone(), kind).await;
    }
    download_file(&ctx, pool, remote).await
}

/// The regular path: one file, split across connections when the
/// server allows it.
pub(crate) async fn download_file(
    ctx: &Arc<TaskContext>,
    pool: SourcePool,
    remote: RemoteFile,
) -> Result<PathBuf> {
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
        Some(size) if segmented => segmented::download(ctx, pool, size, &plan).await?,
        size => stream::download(ctx, &pool, size, &plan.paths).await?,
    }

    complete(ctx, &plan).await
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
