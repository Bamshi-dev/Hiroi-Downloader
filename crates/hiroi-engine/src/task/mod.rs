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
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::checksum::PieceHashes;
use crate::config::EngineConfig;
use crate::error::{EngineError, Result};
use crate::inspect;
use crate::limit::{HostLimiter, RateLimiter};
use crate::media;
use crate::metalink;
use crate::net::client::{self, Http};
use crate::net::cookies::DownloadCookies;
use crate::net::guard::Network;
use crate::net::probe::RemoteFile;
use crate::net::source::SourcePool;
use crate::request::{ConflictPolicy, DownloadRequest};
use crate::snapshot::{DownloadId, DownloadStatus, SegmentSnapshot};
use crate::storage::control::{ControlFile, SavedSegment};
use crate::storage::file::{self, OutputFile};
use crate::storage::naming::DownloadPaths;
use crate::storage::plan::{self, Plan};
use crate::storage::remove_if_exists;

/// State shared by every download of an engine.
#[derive(Debug)]
pub(crate) struct EngineShared {
    pub config: EngineConfig,
    pub network: Network,
    /// For requests that belong to no download.
    pub client: Http,
    pub hosts: HostLimiter,
    pub connections: Semaphore,
    pub global_rate: RateLimiter,
    /// The BitTorrent session, started by the first torrent.
    #[cfg(feature = "torrent")]
    pub torrents: crate::torrent::Torrents,
}

/// How a task reports back to the engine.
pub(crate) trait TaskObserver: Send + Sync {
    fn status_changed(&self, id: DownloadId, status: DownloadStatus);
    fn progress(&self, id: DownloadId);
    fn retrying(&self, id: DownloadId, reason: &str, delay: Duration);
}

pub(crate) struct TaskContext {
    /// This download's HTTP client, with its own cookies and rules.
    pub client: Http,
    pub id: DownloadId,
    pub request: Arc<DownloadRequest>,
    pub shared: Arc<EngineShared>,
    pub progress: Arc<TaskProgress>,
    pub rate: Arc<RateLimiter>,
    pub cancel: CancellationToken,
    pub observer: Arc<dyn TaskObserver>,
}

impl TaskContext {
    /// Gives the task a client of its own, with the request's cookies
    /// scoped to its site and, when enabled, a jar for cookies servers
    /// set. The Cookie header leaves the request: the client adds it to
    /// the requests that may carry it.
    pub fn with_own_client(mut self) -> Result<Self> {
        let (request, client) = own_client(&self.shared, &self.request)?;
        self.client = client;
        self.request = Arc::new(request);
        Ok(self)
    }

    /// The same task working on another request, such as one of the
    /// tracks an extractor found. Progress, limits, and cancellation
    /// stay shared.
    pub fn derive(&self, mut request: DownloadRequest) -> Self {
        request.derived = true;
        Self {
            client: self.client.clone(),
            id: self.id,
            request: Arc::new(request),
            shared: Arc::clone(&self.shared),
            progress: Arc::clone(&self.progress),
            rate: Arc::clone(&self.rate),
            cancel: self.cancel.clone(),
            observer: Arc::clone(&self.observer),
        }
    }
}

/// A client for `request`, and the request without its Cookie header,
/// which the client now adds where it belongs.
pub(crate) fn own_client(
    shared: &EngineShared,
    request: &DownloadRequest,
) -> Result<(DownloadRequest, Http)> {
    let config = &shared.config;
    let mut request = request.clone();
    let own = request
        .headers
        .remove(reqwest::header::COOKIE)
        .and_then(|value| value.to_str().ok().map(str::to_string));
    let cookies = DownloadCookies::new(request.url(), own.as_deref(), config.cookie_jar);
    if own.is_some() {
        request.own_cookies = own;
    }
    let guard = shared.network.guard(&request.sources);
    Ok((request, client::download_client(config, cookies, guard)?))
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
    if ctx.request.stream.extract {
        return media::extract::download(&ctx).await;
    }
    #[cfg(feature = "torrent")]
    if ctx.request.url().scheme() == "magnet" {
        let link = ctx.request.url().to_string();
        return crate::torrent::download(&ctx, crate::torrent::Source::Magnet(link)).await;
    }
    let on_retry = |reason: &str, delay| ctx.observer.retrying(ctx.id, reason, delay);
    let results = inspect::probe_all(
        &ctx.shared,
        &ctx.client,
        &ctx.request,
        &ctx.cancel,
        &on_retry,
    )
    .await;
    let (pool, remote) = inspect::select(&ctx.request, results)?;
    if let Some(kind) = media::detect(&remote) {
        return media::download(&ctx, remote.final_url.clone(), kind).await;
    }
    if !ctx.request.derived && metalink::detect(&remote) {
        return metalink::download(&ctx, &remote.final_url).await;
    }
    #[cfg(feature = "torrent")]
    if !ctx.request.derived && crate::torrent::detect(&remote) {
        let file = crate::torrent::fetch_file(&ctx, &remote.final_url).await?;
        return crate::torrent::download(&ctx, file).await;
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
        Some(size) if segmented => {
            let provenance = segmented::download(ctx, Arc::clone(&pool), size, &plan).await?;
            match ctx.request.pieces.clone() {
                Some(pieces) if pieces.fits(size) => {
                    repair(ctx, &pool, size, &plan, &pieces, provenance).await?;
                }
                Some(_) => tracing::warn!("piece hashes do not fit the file; ignoring them"),
                None => {}
            }
        }
        size => stream::download(ctx, &pool, size, &plan.paths).await?,
    }

    complete(ctx, &plan).await
}

/// How often broken pieces are fetched again before giving up.
const MAX_REPAIRS: usize = 3;

/// Checks every piece against its hash and fetches the broken ones
/// again, from other sources when one is to blame.
async fn repair(
    ctx: &Arc<TaskContext>,
    pool: &Arc<SourcePool>,
    size: u64,
    plan: &Plan,
    pieces: &PieceHashes,
    mut provenance: segmented::Provenance,
) -> Result<()> {
    let part = &plan.paths.part_path;
    for attempt in 0..=MAX_REPAIRS {
        ctx.observer
            .status_changed(ctx.id, DownloadStatus::Verifying);
        let check = pieces.clone();
        let path = part.clone();
        let broken = tokio::task::spawn_blocking(move || check.broken(&path))
            .await
            .map_err(|e| EngineError::io(part, std::io::Error::other(e)))?
            .map_err(|e| EngineError::io(part, e))?;
        if broken.is_empty() {
            return Ok(());
        }
        let bytes: u64 = broken
            .iter()
            .map(|(start, end)| end.min(&size) - start)
            .sum();
        if attempt == MAX_REPAIRS {
            discard(&plan.paths);
            return Err(EngineError::BrokenPieces { bytes });
        }
        tracing::warn!(ranges = broken.len(), bytes, "fetching broken pieces again");
        blame(pool, &provenance, &broken);

        // The usual resume path fetches what the state file says is
        // missing, so the broken pieces are marked as such.
        let mut control = ControlFile::new(
            pool.key(),
            size,
            pool.primary().validator().map(str::to_string),
        );
        control.segments = missing(&broken, size);
        control
            .save(&plan.paths.control_path)
            .map_err(|e| EngineError::io(&plan.paths.control_path, e))?;
        let again = Plan {
            paths: plan.paths.clone(),
            resume: Some(control),
        };
        ctx.observer
            .status_changed(ctx.id, DownloadStatus::Downloading);
        provenance = segmented::download(ctx, Arc::clone(pool), size, &again).await?;
    }
    unreachable!("the last attempt returns")
}

/// Drops a source that wrote into every broken piece, as long as some
/// other source is left to fetch them from.
fn blame(pool: &SourcePool, provenance: &segmented::Provenance, broken: &[(u64, u64)]) {
    for (url, touched) in provenance.blame(broken) {
        if touched < broken.len() || pool.live() < 2 {
            continue;
        }
        if let Some(source) = pool.find(&url) {
            pool.drop_source(source, "sent broken pieces", false);
        }
    }
}

/// A segment layout where `broken` is missing and the rest is written.
fn missing(broken: &[(u64, u64)], size: u64) -> Vec<SavedSegment> {
    let mut segments = Vec::new();
    let mut at = 0;
    for &(start, end) in broken {
        let end = end.min(size);
        if start > at {
            segments.push(SavedSegment {
                start: at,
                end: start,
                written: start - at,
            });
        }
        segments.push(SavedSegment {
            start,
            end,
            written: 0,
        });
        at = end;
    }
    if at < size {
        segments.push(SavedSegment {
            start: at,
            end: size,
            written: size - at,
        });
    }
    segments
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
