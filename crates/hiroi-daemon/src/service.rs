//! The service: one engine, one store, and the bookkeeping between them.
//!
//! The in-memory job table is the source of truth while the service
//! runs; every change is also sent, in order, to a writer task that
//! applies it to SQLite. The engine knows nothing about jobs: it sees
//! downloads, and `Recorder` maps its events back to jobs.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hiroi_engine::{
    DownloadId, DownloadListener, DownloadSnapshot, Engine, EngineConfig, EngineError,
};
use hiroi_proto::{
    Categories, Event, Job, JobId, JobStatus, NewJob, QueueInfo, Request, Response, Schedule,
    ScheduleAction,
};
use hiroi_store::{InsertJob, JobUpdate, Store, StoreError, StoredJob};
use parking_lot::Mutex;
use tokio::sync::{Notify, broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::schedule::{self, Desired, Moment};
use crate::{category, convert, paths, server};

/// How often the progress of running jobs is written to the database.
const PROGRESS_SAVE_INTERVAL: Duration = Duration::from_secs(5);
/// Events a slow subscriber may fall behind before it starts missing some.
const EVENT_BUFFER: usize = 1024;
const DEFAULT_QUEUE: &str = "main";
const GLOBAL_LIMIT_KEY: &str = "global_speed_limit";
const CATEGORIES_KEY: &str = "categories";

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub socket: PathBuf,
    pub database: PathBuf,
    /// Folder for jobs that do not name their own.
    pub download_dir: PathBuf,
    pub engine: EngineConfig,
}

impl DaemonConfig {
    /// The standard locations; see [`crate::paths`].
    pub fn with_default_paths(engine: EngineConfig) -> Self {
        Self {
            socket: paths::default_socket(),
            database: paths::default_database(),
            download_dir: paths::default_download_dir(),
            engine,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("another Hiroi service is already running (lock: {})", .0.display())]
    AlreadyRunning(PathBuf),
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Engine(#[from] EngineError),
}

/// Runs the service until `shutdown` resolves or a client asks it to
/// stop. Every download is paused and saved before this returns.
pub async fn run(
    config: DaemonConfig,
    shutdown: impl Future<Output = ()>,
) -> Result<(), DaemonError> {
    let runtime = config
        .socket
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(paths::runtime_dir);
    let io = |path: &PathBuf| {
        let path = path.clone();
        move |source| DaemonError::Io { path, source }
    };
    paths::prepare_private_dir(&runtime).map_err(io(&runtime))?;
    let _lock = server::lock(&runtime.join(paths::LOCK_NAME))?;

    let store = Arc::new(Store::open(&config.database)?);
    let engine = Engine::new(config.engine.clone())?;
    let service = Service::start(engine, store, config.download_dir.clone()).await?;
    let listener = server::bind(&config.socket)?;
    tracing::info!(socket = %config.socket.display(), "service ready");

    let stop = CancellationToken::new();
    tokio::select! {
        () = server::serve(listener, Arc::clone(&service), stop.clone()) => {}
        () = shutdown => {}
        () = service.shutdown.notified() => {}
    }
    stop.cancel();
    tracing::info!("stopping: saving downloads");
    service.stopping.store(true, Ordering::Relaxed);
    service.engine.pause_all().await;
    service.save_progress();
    service.flush().await;
    let _ = std::fs::remove_file(&config.socket);
    Ok(())
}

pub(crate) struct Service {
    engine: Engine,
    default_dir: PathBuf,
    state: Mutex<State>,
    events: broadcast::Sender<Event>,
    writes: mpsc::UnboundedSender<Write>,
    writer: Mutex<Option<tokio::task::JoinHandle<()>>>,
    store: Arc<Store>,
    pub(crate) shutdown: Notify,
    /// Set while the service pauses everything to exit. Those pauses
    /// are not the user's: the jobs must start again on the next run.
    stopping: AtomicBool,
    schedules_changed: Arc<Notify>,
}

#[derive(Default)]
struct State {
    rows: BTreeMap<JobId, StoredJob>,
    loaded: HashMap<JobId, DownloadId>,
    jobs_of: HashMap<DownloadId, JobId>,
    /// Queues that are held.
    stopped: HashSet<String>,
    /// Jobs paused because their queue is held, not by the user.
    held: HashSet<JobId>,
    schedules: Vec<Schedule>,
    /// What the window rules wanted at the last check.
    last_desired: Option<Desired>,
    /// The speed limit set by the user; windows may lower it for a while.
    base_limit: Option<u64>,
    categories: Option<Categories>,
}

enum Write {
    Update(JobId, JobUpdate),
    Delete(JobId),
    Setting(String, String),
    /// Queue name and whether it is held.
    Queue(String, bool),
    Flush(tokio::sync::oneshot::Sender<()>),
}

impl Service {
    async fn start(
        engine: Engine,
        store: Arc<Store>,
        default_dir: PathBuf,
    ) -> Result<Arc<Self>, DaemonError> {
        let (writes, queue) = mpsc::unbounded_channel();
        let writer = tokio::spawn(write_loop(Arc::clone(&store), queue));
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let service = Arc::new(Self {
            engine,
            default_dir,
            state: Mutex::new(State::default()),
            events,
            writes,
            writer: Mutex::new(Some(writer)),
            store: Arc::clone(&store),
            shutdown: Notify::new(),
            stopping: AtomicBool::new(false),
            schedules_changed: Arc::new(Notify::new()),
        });
        service.engine.add_listener(Arc::new(Recorder {
            service: Arc::downgrade(&service),
        }));
        service.restore().await?;
        tokio::spawn(save_progress_loop(Arc::downgrade(&service)));
        tokio::spawn(schedule_loop(
            Arc::downgrade(&service),
            Arc::clone(&service.schedules_changed),
        ));
        Ok(service)
    }

    /// Loads the stored queue: unfinished jobs go back into the engine,
    /// paused and failed ones without starting.
    async fn restore(&self) -> Result<(), DaemonError> {
        let store = Arc::clone(&self.store);
        let (rows, limit, stopped, schedules, categories) =
            tokio::task::spawn_blocking(move || {
                Ok::<_, StoreError>((
                    store.jobs()?,
                    store.setting(GLOBAL_LIMIT_KEY)?,
                    store.stopped_queues()?,
                    store.schedules()?,
                    store.setting(CATEGORIES_KEY)?,
                ))
            })
            .await
            .expect("store task panicked")?;
        let limit = limit.and_then(|value| value.parse().ok());
        self.engine.set_global_speed_limit(limit);
        {
            let mut state = self.state.lock();
            state.base_limit = limit;
            state.stopped = stopped.into_iter().collect();
            state.schedules = schedules;
            state.categories = categories.and_then(|json| serde_json::from_str(&json).ok());
        }
        for row in rows {
            let id = row.id;
            let unfinished = !matches!(row.status, JobStatus::Completed | JobStatus::Cancelled);
            self.state.lock().rows.insert(id, row.clone());
            if unfinished {
                let start = !matches!(row.status, JobStatus::Paused | JobStatus::Failed);
                self.load(&row, &[], start);
            }
        }
        Ok(())
    }

    /// Hands a job to the engine. Bad stored data marks the job failed.
    fn load(&self, row: &StoredJob, headers: &[(String, String)], start: bool) {
        let request = match convert::request(row, headers) {
            Ok(request) => request,
            Err(error) => {
                self.update(
                    row.id,
                    JobUpdate {
                        status: Some(JobStatus::Failed),
                        error: Some(Some(error.to_string())),
                        ..JobUpdate::default()
                    },
                );
                return;
            }
        };
        // A job of a held queue waits; it starts with its queue.
        let held = start && self.state.lock().stopped.contains(&row.queue);
        // The engine reports some changes synchronously, and the listener
        // takes the state lock: never call the engine while holding it.
        let download = if start && !held {
            self.engine.add(request)
        } else {
            self.engine.add_paused(request)
        };
        let mut state = self.state.lock();
        if held {
            state.held.insert(row.id);
        }
        state.loaded.insert(row.id, download);
        state.jobs_of.insert(download, row.id);
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub(crate) async fn handle(&self, request: Request) -> Result<Response, String> {
        match request {
            Request::Version => Ok(Response::Version {
                protocol: hiroi_proto::PROTOCOL_VERSION,
                program: env!("CARGO_PKG_VERSION").to_string(),
            }),
            Request::Add(job) => self.add(job).await.map(|job| Response::Added { job }),
            Request::List => Ok(Response::Jobs { jobs: self.jobs() }),
            Request::Get { job } => self
                .job(job)
                .map(|job| Response::Job { job: Box::new(job) })
                .ok_or_else(|| unknown(job)),
            Request::Pause { job } => self.pause(job).map(done),
            Request::Resume { job } => self.resume(job).map(done),
            Request::PauseAll => {
                for job in self.loaded_jobs() {
                    let _ = self.pause(job);
                }
                Ok(Response::Done)
            }
            Request::ResumeAll => {
                for job in self.loaded_jobs() {
                    let _ = self.resume(job);
                }
                Ok(Response::Done)
            }
            Request::Cancel { job } => self.cancel(job).map(done),
            Request::Remove { job } => self.remove(job).map(done),
            Request::SetSpeedLimit { job, limit } => self.set_speed_limit(job, limit).map(done),
            Request::Queues => Ok(Response::Queues {
                queues: self.queues(),
            }),
            Request::StartQueue { queue } => {
                self.set_queue_running(&queue, true);
                Ok(Response::Done)
            }
            Request::StopQueue { queue } => {
                self.set_queue_running(&queue, false);
                Ok(Response::Done)
            }
            Request::Categories => Ok(Response::Categories(self.categories())),
            Request::SetCategories(categories) => {
                let json = serde_json::to_string(&categories).expect("plain data serializes");
                self.state.lock().categories = Some(categories);
                let _ = self
                    .writes
                    .send(Write::Setting(CATEGORIES_KEY.into(), json));
                Ok(Response::Done)
            }
            Request::Schedules => Ok(Response::Schedules {
                schedules: self.state.lock().schedules.clone(),
            }),
            Request::AddSchedule(schedule) => self
                .add_schedule(schedule)
                .await
                .map(|id| Response::ScheduleAdded { id }),
            Request::RemoveSchedule { id } => self.remove_schedule(id).await.map(done),
            // The server answers these itself.
            Request::Subscribe => Ok(Response::Done),
            Request::Shutdown => {
                self.shutdown.notify_one();
                Ok(Response::Done)
            }
        }
    }

    async fn add(&self, mut job: NewJob) -> Result<JobId, String> {
        let dir = match job.dir.take() {
            Some(dir) => dir,
            None => {
                let name = job
                    .file_name
                    .clone()
                    .or_else(|| category::name_from_url(&job.url));
                category::folder_for(&self.categories(), &self.default_dir, name.as_deref())
            }
        };
        convert::validate(&job, &dir).map_err(|e| e.to_string())?;
        if !job.allow_duplicate
            && let Some(existing) = self.duplicate_of(&job.url, &dir, job.file_name.as_deref())
        {
            return Err(format!(
                "already in the list as job {} ({}); add it with --allow-duplicate to download it again",
                existing.id,
                existing.status.as_str()
            ));
        }
        let insert = InsertJob {
            url: convert::strip_credentials(&job.url),
            mirrors: job
                .mirrors
                .iter()
                .map(|url| convert::strip_credentials(url))
                .collect(),
            dir,
            options: convert::job_options(&job),
            queue: job
                .queue
                .clone()
                .unwrap_or_else(|| DEFAULT_QUEUE.to_string()),
            status: if job.paused {
                JobStatus::Paused
            } else {
                JobStatus::Queued
            },
            added_at: now(),
        };
        let store = Arc::clone(&self.store);
        let row_insert = insert.clone();
        let id = tokio::task::spawn_blocking(move || store.insert_job(&row_insert))
            .await
            .expect("store task panicked")
            .map_err(|e| e.to_string())?;
        let row = StoredJob {
            id,
            url: insert.url,
            mirrors: insert.mirrors,
            dir: insert.dir,
            options: insert.options,
            queue: insert.queue,
            status: insert.status,
            error: None,
            file_name: None,
            path: None,
            total_size: None,
            downloaded: 0,
            added_at: insert.added_at,
            finished_at: None,
        };
        self.state.lock().rows.insert(id, row.clone());
        // The engine gets the URL as given, credentials included; only
        // the stored copy is stripped.
        let in_memory = StoredJob {
            url: job.url.clone(),
            mirrors: job.mirrors.clone(),
            ..row
        };
        self.load(&in_memory, &job.headers, !job.paused);
        self.publish(id);
        Ok(id)
    }

    fn categories(&self) -> Categories {
        self.state
            .lock()
            .categories
            .clone()
            .unwrap_or_else(category::defaults)
    }

    /// A job that is not cancelled and fetches the same file into the
    /// same place. URLs are compared without credentials and query
    /// string, since signed links differ on every copy.
    fn duplicate_of(
        &self,
        url: &str,
        dir: &std::path::Path,
        file_name: Option<&str>,
    ) -> Option<StoredJob> {
        let key = convert::display_url(url);
        let state = self.state.lock();
        state
            .rows
            .values()
            .filter(|row| row.status != JobStatus::Cancelled && row.dir == dir)
            .find(|row| {
                convert::display_url(&row.url) == key
                    && row.options.file_name.as_deref() == file_name
            })
            .cloned()
    }

    fn jobs(&self) -> Vec<Job> {
        let state = self.state.lock();
        state
            .rows
            .values()
            .map(|row| self.view(&state, row))
            .collect()
    }

    fn job(&self, id: JobId) -> Option<Job> {
        let state = self.state.lock();
        state.rows.get(&id).map(|row| self.view(&state, row))
    }

    fn view(&self, state: &State, row: &StoredJob) -> Job {
        let live = state
            .loaded
            .get(&row.id)
            .and_then(|&download| self.engine.snapshot(download));
        let mut job = convert::job(row, live.as_ref());
        // The engine holds a waiting job paused; to the user it is queued.
        if state.held.contains(&row.id) && job.status == JobStatus::Paused {
            job.status = JobStatus::Queued;
        }
        job
    }

    fn loaded_jobs(&self) -> Vec<JobId> {
        self.state.lock().loaded.keys().copied().collect()
    }

    fn download_of(&self, id: JobId) -> Result<DownloadId, String> {
        let state = self.state.lock();
        if !state.rows.contains_key(&id) {
            return Err(unknown(id));
        }
        state
            .loaded
            .get(&id)
            .copied()
            .ok_or_else(|| format!("job {id} is finished"))
    }

    fn pause(&self, id: JobId) -> Result<(), String> {
        let download = self.download_of(id)?;
        if self.engine.pause(download) {
            Ok(())
        } else {
            Err(format!("job {id} is not running"))
        }
    }

    fn resume(&self, id: JobId) -> Result<(), String> {
        let download = self.download_of(id)?;
        if !self.engine.resume(download) {
            return Err(format!("job {id} is not paused or failed"));
        }
        self.update(
            id,
            JobUpdate {
                status: Some(JobStatus::Queued),
                error: Some(None),
                ..JobUpdate::default()
            },
        );
        Ok(())
    }

    fn cancel(&self, id: JobId) -> Result<(), String> {
        let download = self.download_of(id)?;
        if self.engine.cancel(download) {
            Ok(())
        } else {
            Err(format!("job {id} cannot be cancelled"))
        }
    }

    fn remove(&self, id: JobId) -> Result<(), String> {
        let mut state = self.state.lock();
        let row = state.rows.get(&id).ok_or_else(|| unknown(id))?;
        if !row.status.is_settled() {
            return Err(format!(
                "job {id} is still running; pause or cancel it first"
            ));
        }
        let download = state.loaded.remove(&id);
        if let Some(download) = download {
            state.jobs_of.remove(&download);
        }
        state.rows.remove(&id);
        drop(state);
        if let Some(download) = download {
            self.engine.remove(download);
        }
        let _ = self.writes.send(Write::Delete(id));
        let _ = self.events.send(Event::Removed { job: id });
        Ok(())
    }

    fn set_speed_limit(&self, job: Option<JobId>, limit: Option<u64>) -> Result<(), String> {
        match job {
            None => {
                let window = {
                    let mut state = self.state.lock();
                    state.base_limit = limit;
                    state
                        .last_desired
                        .as_ref()
                        .and_then(|desired| desired.limit)
                };
                // An active speed window keeps its lower limit until it ends.
                self.engine.set_global_speed_limit(lowest(limit, window));
                let value = limit.map(|l| l.to_string()).unwrap_or_default();
                let _ = self
                    .writes
                    .send(Write::Setting(GLOBAL_LIMIT_KEY.into(), value));
                Ok(())
            }
            Some(id) => {
                let download = self.download_of(id)?;
                self.engine.set_speed_limit(download, limit);
                Ok(())
            }
        }
    }

    fn queues(&self) -> Vec<QueueInfo> {
        let state = self.state.lock();
        let mut names: BTreeSet<String> =
            state.rows.values().map(|row| row.queue.clone()).collect();
        names.extend(state.stopped.iter().cloned());
        names.insert(DEFAULT_QUEUE.to_string());
        for schedule in &state.schedules {
            if let ScheduleAction::RunQueue { queue } = &schedule.action {
                names.insert(queue.clone());
            }
        }
        names
            .into_iter()
            .map(|name| {
                let jobs: Vec<_> = state
                    .rows
                    .values()
                    .filter(|row| row.queue == name)
                    .collect();
                let pending = jobs
                    .iter()
                    .filter(|row| {
                        !matches!(row.status, JobStatus::Completed | JobStatus::Cancelled)
                    })
                    .count();
                QueueInfo {
                    running: !state.stopped.contains(&name),
                    jobs: jobs.len(),
                    pending,
                    name,
                }
            })
            .collect()
    }

    /// Holds or releases a queue. Holding pauses its active jobs and
    /// remembers them; releasing resumes exactly those.
    fn set_queue_running(&self, queue: &str, running: bool) {
        let mut state = self.state.lock();
        let changed = if running {
            state.stopped.remove(queue)
        } else {
            state.stopped.insert(queue.to_string())
        };
        if !changed {
            return;
        }
        let members: Vec<(JobId, DownloadId)> = state
            .loaded
            .iter()
            .filter(|(id, _)| state.rows.get(id).is_some_and(|row| row.queue == queue))
            .map(|(&id, &download)| (id, download))
            .collect();
        let mut affected = Vec::new();
        for (id, download) in members {
            if running {
                if state.held.remove(&id) {
                    affected.push(download);
                }
            } else {
                let active = self
                    .engine
                    .snapshot(download)
                    .is_some_and(|s| !s.status.is_settled());
                if active {
                    state.held.insert(id);
                    affected.push(download);
                }
            }
        }
        // Engine calls happen after the lock is released (see `load`).
        drop(state);
        for download in affected {
            if running {
                self.engine.resume(download);
            } else {
                self.engine.pause(download);
            }
        }
        tracing::info!(queue, running, "queue changed");
        let _ = self.writes.send(Write::Queue(queue.to_string(), !running));
    }

    async fn add_schedule(&self, mut schedule: Schedule) -> Result<i64, String> {
        let valid_time = |minute: u16| minute < 24 * 60;
        if schedule.days == 0
            || !valid_time(schedule.from_minute)
            || !schedule.to_minute.is_none_or(valid_time)
        {
            return Err(
                "a schedule needs at least one day and times between 00:00 and 23:59".into(),
            );
        }
        let store = Arc::clone(&self.store);
        let stored = schedule.clone();
        let id = tokio::task::spawn_blocking(move || store.insert_schedule(&stored))
            .await
            .expect("store task panicked")
            .map_err(|e| e.to_string())?;
        schedule.id = id;
        self.state.lock().schedules.push(schedule);
        self.schedules_changed.notify_one();
        Ok(id)
    }

    async fn remove_schedule(&self, id: i64) -> Result<(), String> {
        let store = Arc::clone(&self.store);
        let deleted = tokio::task::spawn_blocking(move || store.delete_schedule(id))
            .await
            .expect("store task panicked")
            .map_err(|e| e.to_string())?;
        if !deleted {
            return Err(format!("no schedule {id}"));
        }
        self.state
            .lock()
            .schedules
            .retain(|schedule| schedule.id != id);
        self.schedules_changed.notify_one();
        Ok(())
    }

    /// Applies the scheduler rules for `at`: window rules when what they
    /// want changed since the last check, one-shot rules on their minute.
    fn run_schedules(&self, at: Moment) {
        let (desired, previous, firing, base) = {
            let mut state = self.state.lock();
            let desired = schedule::desired(&state.schedules, at);
            let previous = state.last_desired.replace(desired.clone());
            let firing: Vec<Schedule> = schedule::firing(&state.schedules, at).cloned().collect();
            (desired, previous, firing, state.base_limit)
        };
        for (queue, &run) in &desired.queues {
            let before = previous.as_ref().and_then(|p| p.queues.get(queue).copied());
            if before != Some(run) {
                self.set_queue_running(queue, run);
            }
        }
        if previous.as_ref().map(|p| p.limit) != Some(desired.limit) {
            self.engine
                .set_global_speed_limit(lowest(base, desired.limit));
        }
        for rule in firing {
            match rule.action {
                ScheduleAction::RunQueue { queue } => self.set_queue_running(&queue, true),
                ScheduleAction::SpeedLimit { limit } => {
                    let _ = self.set_speed_limit(None, Some(limit));
                }
            }
        }
    }

    /// Applies an update to memory and queues it for the database.
    fn update(&self, id: JobId, update: JobUpdate) {
        {
            let mut state = self.state.lock();
            let Some(row) = state.rows.get_mut(&id) else {
                return;
            };
            apply(row, &update);
        }
        let _ = self.writes.send(Write::Update(id, update));
        self.publish(id);
    }

    fn publish(&self, id: JobId) {
        if let Some(job) = self.job(id) {
            let _ = self.events.send(Event::Job { job });
        }
    }

    fn save_progress(&self) {
        let state = self.state.lock();
        for (&id, &download) in &state.loaded {
            let Some(snapshot) = self.engine.snapshot(download) else {
                continue;
            };
            let update = JobUpdate {
                downloaded: Some(snapshot.downloaded),
                total_size: snapshot.total_size,
                ..JobUpdate::default()
            };
            let _ = self.writes.send(Write::Update(id, update));
        }
    }

    /// Waits until every queued write has reached the database.
    async fn flush(&self) {
        let (done, wait) = tokio::sync::oneshot::channel();
        if self.writes.send(Write::Flush(done)).is_ok() {
            let _ = wait.await;
        }
        drop(self.writer.lock().take());
    }

    /// Maps an engine status change onto the job and its stored row.
    fn record(&self, snapshot: &DownloadSnapshot) {
        let Some(id) = self.state.lock().jobs_of.get(&snapshot.id).copied() else {
            return;
        };
        let (status, error) = convert::status(&snapshot.status);
        let stored_failed = self
            .state
            .lock()
            .rows
            .get(&id)
            .is_some_and(|row| row.status == JobStatus::Failed);
        // A restored failed job sits paused in the engine; it stays failed.
        if status == JobStatus::Paused && stored_failed {
            return;
        }
        // A job held by its queue waits; it is not paused by the user.
        let held = self.state.lock().held.contains(&id);
        if status == JobStatus::Paused && held {
            self.update(
                id,
                JobUpdate {
                    status: Some(JobStatus::Queued),
                    downloaded: Some(snapshot.downloaded),
                    ..JobUpdate::default()
                },
            );
            return;
        }
        // Keep the status a job had before the shutdown pause, but save
        // how far it got.
        if status == JobStatus::Paused && self.stopping.load(Ordering::Relaxed) {
            let progress = JobUpdate {
                downloaded: Some(snapshot.downloaded),
                total_size: snapshot.total_size,
                ..JobUpdate::default()
            };
            let _ = self.writes.send(Write::Update(id, progress));
            return;
        }
        let finished = matches!(status, JobStatus::Completed | JobStatus::Cancelled);
        let update = JobUpdate {
            status: Some(status),
            error: Some(error),
            file_name: snapshot.file_name.clone(),
            path: snapshot.path.clone(),
            total_size: snapshot.total_size,
            downloaded: Some(snapshot.downloaded),
            finished_at: finished.then(now),
        };
        self.update(id, update);
        if finished {
            // Finished jobs leave the engine; the row keeps their history.
            let mut state = self.state.lock();
            if let Some(download) = state.loaded.remove(&id) {
                state.jobs_of.remove(&download);
                drop(state);
                self.engine.remove(download);
            }
        }
    }
}

fn apply(row: &mut StoredJob, update: &JobUpdate) {
    if let Some(status) = update.status {
        row.status = status;
    }
    if let Some(error) = &update.error {
        row.error.clone_from(error);
    }
    if let Some(name) = &update.file_name {
        row.file_name = Some(name.clone());
    }
    if let Some(path) = &update.path {
        row.path = Some(path.clone());
    }
    if let Some(total) = update.total_size {
        row.total_size = Some(total);
    }
    if let Some(downloaded) = update.downloaded {
        row.downloaded = downloaded;
    }
    if let Some(finished) = update.finished_at {
        row.finished_at = Some(finished);
    }
}

/// Engine listener that forwards events to the service.
struct Recorder {
    service: Weak<Service>,
}

impl DownloadListener for Recorder {
    fn on_status_changed(&self, snapshot: &DownloadSnapshot) {
        if let Some(service) = self.service.upgrade() {
            service.record(snapshot);
        }
    }

    fn on_progress(&self, snapshot: &DownloadSnapshot) {
        let Some(service) = self.service.upgrade() else {
            return;
        };
        let Some(&job) = service.state.lock().jobs_of.get(&snapshot.id) else {
            return;
        };
        let _ = service.events.send(Event::Progress {
            job,
            downloaded: snapshot.downloaded,
            total_size: snapshot.total_size,
            speed: snapshot.speed,
            eta_secs: snapshot.eta.map(|eta| eta.as_secs()),
            connections: snapshot.connections,
        });
    }

    fn on_retry(&self, id: DownloadId, reason: &str, delay: Duration) {
        let Some(service) = self.service.upgrade() else {
            return;
        };
        let Some(&job) = service.state.lock().jobs_of.get(&id) else {
            return;
        };
        let delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
        let _ = service.events.send(Event::Retry {
            job,
            reason: reason.to_string(),
            delay_ms,
        });
    }
}

/// Applies writes to SQLite one at a time, in the order they were made.
async fn write_loop(store: Arc<Store>, mut queue: mpsc::UnboundedReceiver<Write>) {
    while let Some(write) = queue.recv().await {
        let store = Arc::clone(&store);
        let result = tokio::task::spawn_blocking(move || match write {
            Write::Update(id, update) => store.update_job(id, &update).map(|()| None),
            Write::Delete(id) => store.delete_job(id).map(|_| None),
            Write::Setting(key, value) => store.set_setting(&key, &value).map(|()| None),
            Write::Queue(name, stopped) => store.set_queue_stopped(&name, stopped).map(|()| None),
            Write::Flush(done) => Ok(Some(done)),
        })
        .await;
        match result {
            Ok(Ok(Some(done))) => {
                let _ = done.send(());
            }
            Ok(Ok(None)) => {}
            Ok(Err(error)) => tracing::error!(%error, "could not save to the database"),
            Err(error) => tracing::error!(%error, "database task failed"),
        }
    }
}

async fn save_progress_loop(service: Weak<Service>) {
    let mut tick = tokio::time::interval(PROGRESS_SAVE_INTERVAL);
    tick.tick().await;
    loop {
        tick.tick().await;
        let Some(service) = service.upgrade() else {
            return;
        };
        service.save_progress();
    }
}

/// Checks the rules at every minute boundary and whenever they change.
async fn schedule_loop(service: Weak<Service>, changed: Arc<Notify>) {
    loop {
        let Some(strong) = service.upgrade() else {
            return;
        };
        strong.run_schedules(Moment::now());
        drop(strong);
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        // Time zones are whole minutes, so UTC and local minutes start together.
        let until_next_minute = Duration::from_secs(60 - seconds % 60) + Duration::from_millis(50);
        tokio::select! {
            () = tokio::time::sleep(until_next_minute) => {}
            () = changed.notified() => {}
        }
    }
}

fn lowest(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn unknown(id: JobId) -> String {
    format!("no job {id}")
}

fn done(_: ()) -> Response {
    Response::Done
}
