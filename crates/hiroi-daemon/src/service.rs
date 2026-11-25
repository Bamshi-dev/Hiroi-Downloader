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
    BatchAdded, Categories, Event, GroupInfo, Job, JobId, JobStatus, NewBatch, NewJob, QueueInfo,
    Request, Response, Schedule, ScheduleAction, ServiceSettings,
};
use hiroi_store::{InsertJob, JobUpdate, Store, StoreError, StoredJob};
use parking_lot::Mutex;
use tokio::sync::{Notify, broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::awake::KeepAwake;
use crate::schedule::{self, Desired, Moment};
use crate::{actions, category, convert, folders, paths, server, unpack};

/// How often the progress of running jobs is written to the database.
const PROGRESS_SAVE_INTERVAL: Duration = Duration::from_secs(5);
/// Events a slow subscriber may fall behind before it starts missing some.
const EVENT_BUFFER: usize = 1024;
const DEFAULT_QUEUE: &str = "main";
const GLOBAL_LIMIT_KEY: &str = "global_speed_limit";
const CATEGORIES_KEY: &str = "categories";
const NOTIFICATIONS_KEY: &str = "notifications";
const SMART_FOLDERS_KEY: &str = "smart_folders";
const UNPACK_KEY: &str = "unpack_archives";
const KEEP_AWAKE_KEY: &str = "keep_awake";
/// The command run after every download, as a JSON argument list.
const AFTER_DOWNLOAD_KEY: &str = "after_download";

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub socket: PathBuf,
    pub database: PathBuf,
    /// Folder for jobs that do not name their own.
    pub download_dir: PathBuf,
    pub engine: EngineConfig,
    /// Command run for "power off when done".
    pub power_off_command: Vec<String>,
}

impl DaemonConfig {
    /// The standard locations; see [`crate::paths`].
    pub fn with_default_paths(engine: EngineConfig) -> Self {
        Self {
            socket: paths::default_socket(),
            database: paths::default_database(),
            download_dir: paths::default_download_dir(),
            engine,
            power_off_command: actions::POWER_OFF.iter().map(|s| s.to_string()).collect(),
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
    #[error("{}", crate::transport::UNSUPPORTED)]
    Unsupported,
}

/// Runs the service until `shutdown` resolves or a client asks it to
/// stop. `ready` is called once clients can connect. Every download is
/// paused and saved before this returns.
pub async fn run(
    config: DaemonConfig,
    ready: impl FnOnce(&std::path::Path),
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
    let service = Service::start(
        engine,
        store,
        config.download_dir.clone(),
        config.power_off_command.clone(),
    )
    .await?;
    let listener = server::bind(&config.socket)?;
    tracing::info!(socket = %config.socket.display(), "service ready");
    ready(&config.socket);

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
    power_off_command: Vec<String>,
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
    notifications: bool,
    smart_folders: bool,
    unpack_archives: bool,
    after_download: Option<Vec<String>>,
    keep_awake: bool,
    awake: KeepAwake,
    /// Archives unpacked since the service started, so a set of parts is
    /// unpacked once.
    unpacked: HashSet<PathBuf>,
    power_off_when_done: bool,
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
        power_off_command: Vec<String>,
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
            power_off_command,
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
        let (rows, limit, stopped, schedules, categories, switches) =
            tokio::task::spawn_blocking(move || {
                Ok::<_, StoreError>((
                    store.jobs()?,
                    store.setting(GLOBAL_LIMIT_KEY)?,
                    store.stopped_queues()?,
                    store.schedules()?,
                    store.setting(CATEGORIES_KEY)?,
                    (
                        store.setting(NOTIFICATIONS_KEY)?,
                        store.setting(SMART_FOLDERS_KEY)?,
                        store.setting(UNPACK_KEY)?,
                        store.setting(AFTER_DOWNLOAD_KEY)?,
                        store.setting(KEEP_AWAKE_KEY)?,
                    ),
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
            state.notifications = switches.0.as_deref() != Some("off");
            state.smart_folders = switches.1.as_deref() != Some("off");
            state.unpack_archives = switches.2.as_deref() == Some("on");
            state.keep_awake = switches.4.as_deref() != Some("off");
            state.after_download = switches
                .3
                .as_deref()
                .and_then(|json| serde_json::from_str::<Vec<String>>(json).ok())
                .filter(|argv| !argv.is_empty());
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
            Request::Add(job) => self.add(*job).await.map(|job| Response::Added { job }),
            Request::AddBatch(batch) => Ok(Response::Batch(self.add_batch(*batch).await)),
            Request::Refresh { job, url } => self.refresh(job, &url).map(done),
            Request::Export { finished } => {
                let state = self.state.lock();
                let jobs = state
                    .rows
                    .values()
                    .filter(|row| row.status != JobStatus::Cancelled)
                    .filter(|row| finished || row.status != JobStatus::Completed)
                    .map(convert::exported)
                    .collect();
                Ok(Response::Exported { jobs })
            }
            Request::Groups => Ok(Response::Groups {
                groups: self.groups(),
            }),
            Request::PauseGroup { group } => self.pause_group(&group, true).map(done),
            Request::ResumeGroup { group } => self.pause_group(&group, false).map(done),
            Request::SetSmartFolders { enabled } => {
                self.state.lock().smart_folders = enabled;
                self.save_switch(SMART_FOLDERS_KEY, enabled);
                Ok(Response::Done)
            }
            Request::SetKeepAwake { enabled } => {
                self.state.lock().keep_awake = enabled;
                self.save_switch(KEEP_AWAKE_KEY, enabled);
                self.update_awake();
                Ok(Response::Done)
            }
            Request::SetUnpackArchives { enabled } => {
                self.state.lock().unpack_archives = enabled;
                self.save_switch(UNPACK_KEY, enabled);
                Ok(Response::Done)
            }
            Request::SetAfterDownload { command } => {
                let command = command.filter(|argv| !argv.is_empty());
                let json =
                    serde_json::to_string(&command.clone().unwrap_or_default()).unwrap_or_default();
                self.state.lock().after_download = command;
                let _ = self
                    .writes
                    .send(Write::Setting(AFTER_DOWNLOAD_KEY.into(), json));
                Ok(Response::Done)
            }
            Request::ServiceSettings => {
                let state = self.state.lock();
                Ok(Response::ServiceSettings(ServiceSettings {
                    notifications: state.notifications,
                    smart_folders: state.smart_folders,
                    global_speed_limit: state.base_limit,
                    unpack_archives: state.unpack_archives,
                    after_download: state.after_download.clone(),
                    keep_awake: state.keep_awake,
                }))
            }
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
            Request::SetNotifications { enabled } => {
                self.state.lock().notifications = enabled;
                self.save_switch(NOTIFICATIONS_KEY, enabled);
                Ok(Response::Done)
            }
            Request::PowerOffWhenDone { enabled } => {
                self.state.lock().power_off_when_done = enabled;
                self.check_power_off();
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

    fn save_switch(&self, key: &str, enabled: bool) {
        let value = if enabled { "on" } else { "off" };
        let _ = self.writes.send(Write::Setting(key.into(), value.into()));
    }

    /// Adds the jobs of a batch, in one group when there are several.
    async fn add_batch(&self, batch: NewBatch) -> BatchAdded {
        let group = (batch.jobs.len() > 1).then(|| {
            batch
                .group
                .clone()
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| group_name(&batch))
        });
        let mut added = BatchAdded {
            group: group.clone(),
            ..BatchAdded::default()
        };
        for mut job in batch.jobs {
            if job.group.is_none() {
                job.group.clone_from(&group);
            }
            let url = convert::display_url(&job.url);
            match self.add(job).await {
                Ok(id) => added.jobs.push(id),
                Err(error) => added.errors.push(format!("{url}: {error}")),
            }
        }
        // No job joined it, so there is no group to speak of.
        if added.jobs.is_empty() {
            added.group = None;
        }
        added
    }

    fn groups(&self) -> Vec<GroupInfo> {
        let jobs = self.jobs();
        let mut groups: BTreeMap<String, GroupInfo> = BTreeMap::new();
        for job in &jobs {
            let Some(name) = &job.group else {
                continue;
            };
            let info = groups.entry(name.clone()).or_insert_with(|| GroupInfo {
                name: name.clone(),
                jobs: 0,
                completed: 0,
                downloaded: 0,
                total_size: Some(0),
            });
            info.jobs += 1;
            info.completed += usize::from(job.status == JobStatus::Completed);
            info.downloaded += job.downloaded;
            info.total_size = info.total_size.zip(job.total_size).map(|(a, b)| a + b);
        }
        groups.into_values().collect()
    }

    /// Pauses, or resumes, every job of `group` that can be.
    fn pause_group(&self, group: &str, pause: bool) -> Result<(), String> {
        let members: Vec<JobId> = {
            let state = self.state.lock();
            state
                .rows
                .values()
                .filter(|row| row.options.group.as_deref() == Some(group))
                .map(|row| row.id)
                .collect()
        };
        if members.is_empty() {
            return Err(format!("no group named {group}"));
        }
        for id in members {
            // Finished jobs cannot be paused or resumed; that is fine here.
            let _ = if pause {
                self.pause(id)
            } else {
                self.resume(id)
            };
        }
        Ok(())
    }

    /// Moves the job's file, and earlier episodes of its series, into a
    /// folder of their own. See [`folders`].
    fn gather_series(&self, id: JobId) {
        let moves = {
            let state = self.state.lock();
            if !state.smart_folders {
                return;
            }
            folders::plan(id, &state.rows)
        };
        for step in folders::apply(moves) {
            tracing::info!(from = %step.from.display(), to = %step.to.display(), "moved into the series folder");
            self.update(
                step.job,
                JobUpdate {
                    path: Some(step.to),
                    ..JobUpdate::default()
                },
            );
        }
    }

    /// Notification, the job's own command, and opening its folder.
    fn after_finish(&self, id: JobId, status: JobStatus) {
        if status == JobStatus::Completed {
            self.gather_series(id);
        }
        let (row, notify) = {
            let state = self.state.lock();
            (state.rows.get(&id).cloned(), state.notifications)
        };
        let Some(row) = row else {
            return;
        };
        let name = row
            .file_name
            .clone()
            .unwrap_or_else(|| convert::display_url(&row.url));
        if notify {
            match status {
                JobStatus::Completed => actions::notify("Download finished", &name),
                _ => actions::notify(
                    "Download failed",
                    &format!("{name}: {}", row.error.unwrap_or_default()),
                ),
            }
        }
        if status == JobStatus::Completed
            && let Some(path) = &row.path
        {
            // The command for every download first, such as a virus
            // scanner, then the job's own.
            let after_download = self.state.lock().after_download.clone();
            if let Some(argv) = &after_download {
                actions::run_after(argv, path);
            }
            if let Some(argv) = &row.options.run_after {
                actions::run_after(argv, path);
            }
            if row.options.open_folder {
                actions::open_folder(path);
            }
            self.unpack_finished(id, notify);
        }
        self.check_power_off();
    }

    /// Unpacks the archive the job finished, once every part of it the
    /// service knows of has finished. See [`unpack`].
    fn unpack_finished(&self, id: JobId, notify: bool) {
        let (kind, first, folder) = {
            let mut state = self.state.lock();
            if !state.unpack_archives {
                return;
            }
            let Some(row) = state.rows.get(&id) else {
                return;
            };
            let Some(part) = row
                .path
                .as_deref()
                .and_then(|path| path.file_name()?.to_str())
                .and_then(unpack::part_of)
            else {
                return;
            };
            let dir = row.dir.clone();
            // Parts are found by the name they will have, so those that
            // have not started yet count too.
            let members: Vec<&StoredJob> = state
                .rows
                .values()
                .filter(|other| other.dir == dir && other.status != JobStatus::Cancelled)
                .filter(|other| {
                    expected_name(other)
                        .and_then(|name| unpack::part_of(&name))
                        .is_some_and(|p| p.set == part.set && p.kind == part.kind)
                })
                .collect();
            if members
                .iter()
                .any(|other| other.status != JobStatus::Completed)
            {
                return;
            }
            let Some(first) = members
                .iter()
                .filter_map(|other| {
                    let path = other.path.clone()?;
                    let number = unpack::part_of(path.file_name()?.to_str()?)?.number;
                    Some((number, path))
                })
                .min_by_key(|(number, _)| *number)
                .map(|(_, path)| path)
            else {
                return;
            };
            if !state.unpacked.insert(first.clone()) {
                return;
            }
            let parent = first.parent().map(PathBuf::from).unwrap_or_default();
            (part.kind, first, unpack::folder_for(&parent, &part.set))
        };
        tokio::spawn(async move {
            let name = folder.display().to_string();
            match unpack::unpack(kind, &first, &folder).await {
                Ok(program) => {
                    tracing::info!(archive = %first.display(), %program, "unpacked");
                    if notify {
                        actions::notify("Unpacked", &name);
                    }
                }
                Err(reason) => {
                    tracing::warn!(archive = %first.display(), %reason, "not unpacked");
                    if notify {
                        actions::notify("Could not unpack", &reason);
                    }
                }
            }
        });
    }

    /// Powers off once nothing is left to run, if the user asked for it.
    fn check_power_off(&self) {
        let mut state = self.state.lock();
        if !state.power_off_when_done {
            return;
        }
        let busy = state.rows.values().any(|row| {
            matches!(
                row.status,
                JobStatus::Queued
                    | JobStatus::Connecting
                    | JobStatus::Downloading
                    | JobStatus::Verifying
            ) && !state.stopped.contains(&row.queue)
        });
        if !busy {
            state.power_off_when_done = false;
            drop(state);
            actions::power_off(&self.power_off_command);
        }
    }

    fn categories(&self) -> Categories {
        self.state
            .lock()
            .categories
            .clone()
            .unwrap_or_else(category::defaults)
    }

    /// A job that is not cancelled and fetches the same file into the
    /// same place. URLs are compared whole, without credentials and the
    /// fragment: the query often names the file (`watch?v=...`,
    /// `download.php?id=7`). A signed link copied twice therefore counts
    /// as two downloads, which is the safer mistake.
    fn duplicate_of(
        &self,
        url: &str,
        dir: &std::path::Path,
        file_name: Option<&str>,
    ) -> Option<StoredJob> {
        let key = convert::duplicate_key(url);
        let state = self.state.lock();
        state
            .rows
            .values()
            .filter(|row| row.status != JobStatus::Cancelled && row.dir == dir)
            .find(|row| {
                convert::duplicate_key(&row.url) == key
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

    /// Points a paused or failed job at a new address. The job is loaded
    /// again, paused, so the next resume probes the new address; the
    /// engine keeps the progress if the file there has the same size.
    fn refresh(&self, id: JobId, url: &str) -> Result<(), String> {
        hiroi_engine::DownloadRequest::new(url).map_err(|e| e.to_string())?;
        let (row, download) = {
            let state = self.state.lock();
            let row = state.rows.get(&id).cloned().ok_or_else(|| unknown(id))?;
            (row, state.loaded.get(&id).copied())
        };
        match row.status {
            JobStatus::Paused | JobStatus::Failed => {}
            JobStatus::Completed | JobStatus::Cancelled => {
                return Err(format!("job {id} is {}", row.status.as_str()));
            }
            _ => return Err(format!("pause job {id} first: hiroi pause {id}")),
        }
        let mut options = row.options.clone();
        // The new address may name the file differently; the progress
        // belongs to the old name.
        options.file_name = options
            .file_name
            .or_else(|| row.file_name.clone())
            .or_else(|| {
                row.path
                    .as_deref()
                    .and_then(|path| path.file_name())
                    .map(|name| name.to_string_lossy().into_owned())
            });
        options.replaces = Some(row.url.clone());
        if let Some(download) = download {
            let mut state = self.state.lock();
            state.loaded.remove(&id);
            state.jobs_of.remove(&download);
            drop(state);
            self.engine.remove(download);
        }
        self.update(
            id,
            JobUpdate {
                url: Some(convert::strip_credentials(url)),
                options: Some(Box::new(options)),
                status: Some(JobStatus::Paused),
                error: Some(None),
                ..JobUpdate::default()
            },
        );
        // The engine gets the address as given, credentials included.
        let Some(updated) = self.state.lock().rows.get(&id).cloned() else {
            return Ok(());
        };
        let in_memory = StoredJob {
            url: url.to_string(),
            ..updated
        };
        self.load(&in_memory, &[], false);
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
        let status_changed = update.status.is_some();
        let _ = self.writes.send(Write::Update(id, update));
        self.publish(id);
        if status_changed {
            self.update_awake();
        }
    }

    /// Holds the computer awake while a job transfers, if the user lets
    /// the service; see [`KeepAwake`].
    fn update_awake(&self) {
        let mut state = self.state.lock();
        let transferring = state.rows.values().any(|row| {
            matches!(
                row.status,
                JobStatus::Connecting | JobStatus::Downloading | JobStatus::Verifying
            )
        });
        let awake = state.keep_awake && transferring;
        state.awake.set(awake);
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
            ..JobUpdate::default()
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
        if matches!(status, JobStatus::Completed | JobStatus::Failed) {
            self.after_finish(id, status);
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
    if let Some(url) = &update.url {
        row.url.clone_from(url);
    }
    if let Some(options) = &update.options {
        row.options = options.as_ref().clone();
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

/// The name a job's file has or will have.
fn expected_name(row: &StoredJob) -> Option<String> {
    row.path
        .as_deref()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .or_else(|| row.file_name.clone())
        .or_else(|| row.options.file_name.clone())
        .or_else(|| category::name_from_url(&row.url))
}

/// A name for a batch the user did not name; see
/// [`hiroi_organize::group_name`].
fn group_name(batch: &NewBatch) -> String {
    let names: Vec<String> = batch
        .jobs
        .iter()
        .filter_map(|job| {
            job.file_name
                .clone()
                .or_else(|| category::name_from_url(&job.url))
        })
        .collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let host = batch
        .jobs
        .first()
        .and_then(|job| hiroi_engine::Url::parse(&job.url).ok())
        .and_then(|url| url.host_str().map(str::to_string));
    let date = jiff::ZonedDateTime::now().date().to_string();
    hiroi_organize::group_name(hiroi_organize::Batch {
        names: &names,
        title: batch.title.as_deref(),
        host: host.as_deref(),
        date: &date,
    })
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
