use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use tokio::sync::{Semaphore, watch};
use tokio_util::sync::CancellationToken;

use crate::config::EngineConfig;
use crate::error::{EngineError, Result};
use crate::inspect::{self, RemoteInfo};
use crate::limit::{HostLimiter, RateLimiter};
use crate::listener::DownloadListener;
use crate::net::{client, display_url};
use crate::request::DownloadRequest;
use crate::snapshot::{DownloadId, DownloadSnapshot, DownloadStatus};
use crate::task::{self, EngineShared, TaskContext, TaskObserver, TaskProgress};

/// The download manager: a queue of downloads sharing connection and speed
/// limits.
///
/// Cheap to clone; clones control the same engine. Must be used inside a
/// Tokio runtime.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<EngineInner>,
}

struct EngineInner {
    shared: Arc<EngineShared>,
    listeners: RwLock<Vec<Arc<dyn DownloadListener>>>,
    downloads: Mutex<BTreeMap<DownloadId, Entry>>,
    next_id: AtomicU64,
}

struct Entry {
    request: Arc<DownloadRequest>,
    status: DownloadStatus,
    progress: Arc<TaskProgress>,
    rate: Arc<RateLimiter>,
    running: Option<Running>,
    status_tx: watch::Sender<DownloadStatus>,
}

/// Handle of a task that is currently running.
struct Running {
    cancel: CancellationToken,
    stop: Option<Stop>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Pause,
    Cancel,
}

impl Engine {
    pub fn new(config: EngineConfig) -> Result<Self> {
        let config = config.normalized();
        let shared = EngineShared {
            client: client::build_client(&config)?,
            hosts: HostLimiter::new(config.max_connections_per_host),
            connections: Semaphore::new(config.max_total_connections),
            global_rate: RateLimiter::new(config.global_speed_limit),
            #[cfg(feature = "torrent")]
            torrents: crate::torrent::Torrents::default(),
            config,
        };
        let inner = EngineInner {
            shared: Arc::new(shared),
            listeners: RwLock::new(Vec::new()),
            downloads: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(1),
        };
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    pub fn add_listener(&self, listener: Arc<dyn DownloadListener>) {
        self.inner.listeners.write().push(listener);
    }

    /// Queues a download. It starts as soon as a slot is free.
    pub fn add(&self, request: DownloadRequest) -> DownloadId {
        self.insert(request, DownloadStatus::Queued)
    }

    /// Adds a download without starting it; [`Engine::resume`] starts it.
    pub fn add_paused(&self, request: DownloadRequest) -> DownloadId {
        self.insert(request, DownloadStatus::Paused)
    }

    fn insert(&self, request: DownloadRequest, status: DownloadStatus) -> DownloadId {
        let id = DownloadId(self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        let entry = Entry {
            rate: Arc::new(RateLimiter::new(request.speed_limit)),
            request: Arc::new(request),
            status: status.clone(),
            progress: Arc::default(),
            running: None,
            status_tx: watch::Sender::new(status),
        };
        let snapshot = {
            let mut downloads = self.inner.downloads.lock();
            let snapshot = snapshot(id, &entry);
            downloads.insert(id, entry);
            snapshot
        };
        self.inner.emit(|listener| listener.on_added(&snapshot));
        self.inner.schedule();
        id
    }

    /// Stops a download but keeps its progress. Returns false if the
    /// download is unknown or already settled.
    pub fn pause(&self, id: DownloadId) -> bool {
        self.inner.stop(id, Stop::Pause)
    }

    /// Stops a download and deletes its temporary files.
    pub fn cancel(&self, id: DownloadId) -> bool {
        self.inner.stop(id, Stop::Cancel)
    }

    /// Puts a paused or failed download back into the queue.
    pub fn resume(&self, id: DownloadId) -> bool {
        let snapshot = {
            let mut downloads = self.inner.downloads.lock();
            let Some(entry) = downloads.get_mut(&id) else {
                return false;
            };
            if !matches!(
                entry.status,
                DownloadStatus::Paused | DownloadStatus::Failed(_)
            ) {
                return false;
            }
            entry.set_status(DownloadStatus::Queued);
            snapshot(id, entry)
        };
        self.inner
            .emit(|listener| listener.on_status_changed(&snapshot));
        self.inner.schedule();
        true
    }

    /// Forgets a settled download. Its files stay where they are.
    pub fn remove(&self, id: DownloadId) -> bool {
        let mut downloads = self.inner.downloads.lock();
        let removable = downloads
            .get(&id)
            .is_some_and(|entry| entry.running.is_none() && entry.status.is_settled());
        removable && downloads.remove(&id).is_some()
    }

    /// Changes the speed limit of one download while it runs.
    pub fn set_speed_limit(&self, id: DownloadId, limit: Option<u64>) -> bool {
        let downloads = self.inner.downloads.lock();
        let Some(entry) = downloads.get(&id) else {
            return false;
        };
        entry.rate.set_limit(limit.filter(|&limit| limit > 0));
        true
    }

    pub fn set_global_speed_limit(&self, limit: Option<u64>) {
        self.inner
            .shared
            .global_rate
            .set_limit(limit.filter(|&limit| limit > 0));
    }

    pub fn snapshot(&self, id: DownloadId) -> Option<DownloadSnapshot> {
        let downloads = self.inner.downloads.lock();
        downloads.get(&id).map(|entry| snapshot(id, entry))
    }

    pub fn snapshots(&self) -> Vec<DownloadSnapshot> {
        let downloads = self.inner.downloads.lock();
        downloads
            .iter()
            .map(|(&id, entry)| snapshot(id, entry))
            .collect()
    }

    /// Asks the servers of `request` about the file without downloading
    /// it: name, size, resume support, and which mirrors agree.
    pub async fn inspect(&self, request: &DownloadRequest) -> RemoteInfo {
        let cancel = CancellationToken::new();
        let shared = &self.inner.shared;
        let results = inspect::probe_all(shared, request, &cancel, &|_, _| {}).await;
        inspect::describe(request, &results)
    }

    /// Waits until the download is paused, completed, failed, or cancelled.
    pub async fn wait(&self, id: DownloadId) -> Option<DownloadStatus> {
        let mut status = self.inner.downloads.lock().get(&id)?.status_tx.subscribe();
        loop {
            let current = status.borrow_and_update().clone();
            if current.is_settled() || status.changed().await.is_err() {
                return Some(current);
            }
        }
    }

    /// Pauses everything and waits until progress is saved. Call this before
    /// the program exits.
    pub async fn pause_all(&self) {
        let ids: Vec<_> = self.inner.downloads.lock().keys().copied().collect();
        for &id in &ids {
            self.pause(id);
        }
        for id in ids {
            self.wait(id).await;
        }
    }
}

impl fmt::Debug for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine")
            .field("downloads", &self.inner.downloads.lock().len())
            .finish_non_exhaustive()
    }
}

impl EngineInner {
    /// Starts queued downloads while slots are free, highest priority first.
    fn schedule(self: &Arc<Self>) {
        let mut started = Vec::new();
        let mut snapshots = Vec::new();
        {
            let mut downloads = self.downloads.lock();
            let running = downloads
                .values()
                .filter(|entry| entry.running.is_some())
                .count();
            let free = self
                .shared
                .config
                .max_active_downloads
                .saturating_sub(running);
            let mut queued: Vec<_> = downloads
                .iter()
                .filter(|(_, entry)| entry.status == DownloadStatus::Queued)
                .map(|(&id, entry)| (std::cmp::Reverse(entry.request.priority), id))
                .collect();
            queued.sort();

            for (_, id) in queued.into_iter().take(free) {
                let Some(entry) = downloads.get_mut(&id) else {
                    continue;
                };
                let cancel = CancellationToken::new();
                entry.running = Some(Running {
                    cancel: cancel.clone(),
                    stop: None,
                });
                entry.set_status(DownloadStatus::Connecting);
                snapshots.push(snapshot(id, entry));
                started.push(TaskContext {
                    id,
                    request: Arc::clone(&entry.request),
                    shared: Arc::clone(&self.shared),
                    progress: Arc::clone(&entry.progress),
                    rate: Arc::clone(&entry.rate),
                    cancel,
                    observer: Arc::clone(self) as Arc<dyn TaskObserver>,
                });
            }
        }

        for snapshot in &snapshots {
            self.emit(|listener| listener.on_status_changed(snapshot));
        }
        for ctx in started {
            let engine = Arc::clone(self);
            let id = ctx.id;
            tokio::spawn(async move {
                let result = task::run(Arc::new(ctx)).await;
                engine.finish(id, result);
            });
        }
    }

    fn stop(self: &Arc<Self>, id: DownloadId, stop: Stop) -> bool {
        let snapshot = {
            let mut downloads = self.downloads.lock();
            let Some(entry) = downloads.get_mut(&id) else {
                return false;
            };
            if let Some(running) = &mut entry.running {
                // The task saves its progress and reports back through `finish`.
                running.stop = Some(stop);
                running.cancel.cancel();
                return true;
            }
            match (stop, &entry.status) {
                (_, DownloadStatus::Completed | DownloadStatus::Cancelled) => return false,
                (Stop::Pause, DownloadStatus::Queued) => entry.set_status(DownloadStatus::Paused),
                (Stop::Pause, _) => return false,
                (Stop::Cancel, _) => {
                    if let Some(paths) = entry.progress.get().paths {
                        task::discard(&paths);
                    }
                    entry.set_status(DownloadStatus::Cancelled);
                }
            }
            snapshot(id, entry)
        };
        self.emit(|listener| listener.on_status_changed(&snapshot));
        true
    }

    fn finish(self: &Arc<Self>, id: DownloadId, result: Result<PathBuf>) {
        // Streams and joined tracks only know their size once written.
        let size = result
            .as_ref()
            .ok()
            .and_then(|path| std::fs::metadata(path).ok())
            .map(|metadata| metadata.len());
        let (snapshot, error) = {
            let mut downloads = self.downloads.lock();
            let Some(entry) = downloads.get_mut(&id) else {
                return;
            };
            let stop = entry.running.take().and_then(|running| running.stop);
            let status = match (&result, stop) {
                (Ok(_), _) => DownloadStatus::Completed,
                (Err(_), Some(Stop::Cancel)) => {
                    if let Some(paths) = entry.progress.get().paths {
                        task::discard(&paths);
                    }
                    DownloadStatus::Cancelled
                }
                (Err(EngineError::Cancelled), _) => DownloadStatus::Paused,
                (Err(error), _) => DownloadStatus::Failed(error.to_string()),
            };
            if status == DownloadStatus::Completed {
                entry.progress.update(|p| {
                    p.total_size = size.or(p.total_size);
                    p.downloaded = p.total_size.unwrap_or(p.downloaded);
                    p.speed = 0;
                    p.eta = None;
                    p.connections = 0;
                });
            }
            entry.set_status(status);
            (snapshot(id, entry), result.err())
        };

        self.emit(|listener| listener.on_status_changed(&snapshot));
        match &snapshot.status {
            DownloadStatus::Completed => self.emit(|listener| listener.on_completed(&snapshot)),
            DownloadStatus::Failed(_) => {
                if let Some(error) = &error {
                    tracing::warn!(%id, %error, "download failed");
                    self.emit(|listener| listener.on_failed(&snapshot, error));
                }
            }
            _ => {}
        }
        self.schedule();
    }

    /// Calls every listener without holding any engine lock, so listeners
    /// may call back into the engine.
    fn emit(&self, event: impl Fn(&dyn DownloadListener)) {
        let listeners = self.listeners.read().clone();
        for listener in &listeners {
            event(listener.as_ref());
        }
    }
}

impl TaskObserver for EngineInner {
    fn status_changed(&self, id: DownloadId, status: DownloadStatus) {
        let snapshot = {
            let mut downloads = self.downloads.lock();
            let Some(entry) = downloads.get_mut(&id) else {
                return;
            };
            // Once a stop was requested, only `finish` decides the status.
            let stopping = entry
                .running
                .as_ref()
                .is_some_and(|running| running.stop.is_some());
            if stopping || entry.status == status {
                return;
            }
            entry.set_status(status);
            snapshot(id, entry)
        };
        self.emit(|listener| listener.on_status_changed(&snapshot));
    }

    fn progress(&self, id: DownloadId) {
        let Some(snapshot) = self
            .downloads
            .lock()
            .get(&id)
            .map(|entry| snapshot(id, entry))
        else {
            return;
        };
        self.emit(|listener| listener.on_progress(&snapshot));
    }

    fn retrying(&self, id: DownloadId, reason: &str, delay: Duration) {
        self.emit(|listener| listener.on_retry(id, reason, delay));
    }
}

impl Entry {
    fn set_status(&mut self, status: DownloadStatus) {
        self.status = status.clone();
        self.status_tx.send_replace(status);
    }
}

fn snapshot(id: DownloadId, entry: &Entry) -> DownloadSnapshot {
    let progress = entry.progress.get();
    let running = entry.status.is_running();
    DownloadSnapshot {
        id,
        url: display_url(entry.request.url()),
        file_name: progress.paths.as_ref().map(|paths| paths.file_name()),
        path: progress.paths.map(|paths| paths.final_path),
        total_size: progress.total_size,
        downloaded: progress.downloaded,
        speed: if running { progress.speed } else { 0 },
        eta: progress.eta.filter(|_| running),
        connections: if running { progress.connections } else { 0 },
        resumable: progress.resumable,
        status: entry.status.clone(),
        segments: progress.segments,
    }
}
