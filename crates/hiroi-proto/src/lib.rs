//! Messages exchanged between the Hiroi service (`hiroi daemon`) and its
//! clients: the command line, the browser's native-messaging host, and
//! later a graphical interface.
//!
//! The protocol is one JSON object per line over a Unix domain socket.
//! A client sends [`RequestEnvelope`]s and receives [`ServerMessage`]s:
//! a [`ServerMessage::Reply`] for each request, carrying the same `id`,
//! and, after [`Request::Subscribe`], a stream of [`ServerMessage::Event`]s.
//!
//! ```text
//! -> {"id":1,"method":"add","params":{"url":"https://example.com/a.iso"}}
//! <- {"reply":{"id":1,"ok":{"type":"added","job":7}}}
//! -> {"id":2,"method":"subscribe"}
//! <- {"reply":{"id":2,"ok":{"type":"done"}}}
//! <- {"event":{"type":"progress","job":7,"downloaded":1048576,...}}
//! ```
//!
//! This crate only holds data types, so a client can use it without
//! pulling in the download engine.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Version of this protocol. Clients compare it with [`Reply::Version`].
pub const PROTOCOL_VERSION: u32 = 1;

/// Longest line either side accepts. Larger lines close the connection.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Identifies a job across restarts of the service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JobId(pub i64);

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Delegating keeps width and alignment flags working.
        std::fmt::Display::fmt(&self.0, f)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    /// Protocol and program version.
    Version,
    Add(Box<NewJob>),
    /// Several jobs added together, such as the links of a page. Two or
    /// more get a group, named after them when no name is given.
    AddBatch(Box<NewBatch>),
    List,
    Get {
        job: JobId,
    },
    Pause {
        job: JobId,
    },
    Resume {
        job: JobId,
    },
    PauseAll,
    ResumeAll,
    /// Stops the job and deletes its temporary files.
    Cancel {
        job: JobId,
    },
    /// Forgets a finished, failed, or cancelled job. Files stay.
    Remove {
        job: JobId,
    },
    /// Speed limit of one job, or of everything when `job` is absent.
    SetSpeedLimit {
        job: Option<JobId>,
        limit: Option<u64>,
    },
    /// Queues with their state and job counts.
    Queues,
    /// Let a queue's jobs run.
    StartQueue {
        queue: String,
    },
    /// Hold a queue: its running jobs pause and wait; new jobs wait too.
    StopQueue {
        queue: String,
    },
    Categories,
    SetCategories(Categories),
    /// Desktop notifications when jobs finish (on by default).
    SetNotifications {
        enabled: bool,
    },
    /// Power the computer off once no job is left to run. Fires once.
    PowerOffWhenDone {
        enabled: bool,
    },
    /// Groups with their job counts.
    Groups,
    PauseGroup {
        group: String,
    },
    ResumeGroup {
        group: String,
    },
    /// Move finished episodes of a series into a folder named after it
    /// (on by default). Only files the service downloaded are moved.
    SetSmartFolders {
        enabled: bool,
    },
    /// The settings the service keeps itself.
    ServiceSettings,
    Schedules,
    AddSchedule(Schedule),
    RemoveSchedule {
        id: i64,
    },
    /// Start receiving events on this connection.
    Subscribe,
    /// Pause everything, save, and exit.
    Shutdown,
}

/// A download to add. Only `url` is required.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NewJob {
    pub url: String,
    pub mirrors: Vec<String>,
    /// Folder to save into; the service's default when absent.
    pub dir: Option<PathBuf>,
    pub file_name: Option<String>,
    pub connections: Option<u8>,
    pub speed_limit: Option<u64>,
    /// `algorithm:hex`, e.g. `sha256:9f86...`.
    pub checksum: Option<String>,
    pub on_conflict: Option<Conflict>,
    pub priority: Option<Priority>,
    /// Queue the job belongs to; `main` when absent.
    pub queue: Option<String>,
    /// Add the job without starting it.
    pub paused: bool,
    /// Extra request headers, cookies included. The service keeps them
    /// in memory only and never writes them to disk.
    pub headers: Vec<(String, String)>,
    /// Add the job even if the same file is already in the list.
    pub allow_duplicate: bool,
    /// Program and arguments to run when the job completes. `{path}`,
    /// `{name}`, and `{dir}` are replaced. It runs without a shell.
    pub run_after: Option<Vec<String>>,
    /// Open the folder in the file manager when the job completes.
    pub open_folder: bool,
    /// Which variant of a stream to fetch; the best when absent.
    pub quality: Option<Quality>,
    /// Stop recording a live stream after this many seconds.
    pub live_limit_secs: Option<u64>,
    /// The URL is a page for yt-dlp to find the media on.
    pub extract: bool,
    /// yt-dlp format selection; implies `extract`.
    pub format: Option<String>,
    /// Group the job belongs to, for listing and pausing together.
    pub group: Option<String>,
}

/// Jobs to add together.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NewBatch {
    pub jobs: Vec<NewJob>,
    /// The group's name; made from the jobs, the page title, or the site
    /// when absent. A job's own group wins.
    pub group: Option<String>,
    /// Title of the page the links came from, for naming the group.
    pub title: Option<String>,
}

/// Which variant of a stream to download, when it offers several.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    Best,
    Worst,
    /// The best variant no taller than this many pixels.
    MaxHeight(u32),
}

/// Files whose extension matches go into `folder` (relative to the
/// service's download folder) when a job names no folder of its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Category {
    pub name: String,
    /// Lower case, without the dot.
    pub extensions: Vec<String>,
    pub folder: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Categories {
    pub enabled: bool,
    pub categories: Vec<Category>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Conflict {
    Rename,
    Overwrite,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    Normal,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Connecting,
    Downloading,
    Paused,
    Verifying,
    Completed,
    Failed,
    Cancelled,
}

impl JobStatus {
    /// True when the job does not run and will not start on its own.
    pub fn is_settled(self) -> bool {
        matches!(
            self,
            Self::Paused | Self::Completed | Self::Failed | Self::Cancelled
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Connecting => "connecting",
            Self::Downloading => "downloading",
            Self::Paused => "paused",
            Self::Verifying => "verifying",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        let all = [
            Self::Queued,
            Self::Connecting,
            Self::Downloading,
            Self::Paused,
            Self::Verifying,
            Self::Completed,
            Self::Failed,
            Self::Cancelled,
        ];
        all.into_iter().find(|status| status.as_str() == value)
    }
}

/// The state of one job as clients see it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    /// Display form of the URL: no credentials, no query string.
    pub url: String,
    pub file_name: Option<String>,
    pub path: Option<PathBuf>,
    pub queue: String,
    pub group: Option<String>,
    pub status: JobStatus,
    /// Why the job failed, when it did.
    pub error: Option<String>,
    pub total_size: Option<u64>,
    pub downloaded: u64,
    /// Bytes per second while running.
    pub speed: u64,
    pub eta_secs: Option<u64>,
    pub connections: usize,
    pub resumable: bool,
    /// Unix time in seconds.
    pub added_at: i64,
    pub finished_at: Option<i64>,
    /// `(start, end, downloaded)` per segment, for a segment bar.
    pub segments: Vec<(u64, u64, u64)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerMessage {
    Reply(Reply),
    Event(Event),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reply {
    pub id: u64,
    #[serde(flatten)]
    pub outcome: Outcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok(Response),
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Version { protocol: u32, program: String },
    Added { job: JobId },
    Jobs { jobs: Vec<Job> },
    Job { job: Box<Job> },
    Queues { queues: Vec<QueueInfo> },
    Schedules { schedules: Vec<Schedule> },
    Categories(Categories),
    ScheduleAdded { id: i64 },
    Batch(BatchAdded),
    Groups { groups: Vec<GroupInfo> },
    ServiceSettings(ServiceSettings),
    Done,
}

/// What became of a batch.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BatchAdded {
    pub group: Option<String>,
    pub jobs: Vec<JobId>,
    /// Jobs that were not added, and why.
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupInfo {
    pub name: String,
    pub jobs: usize,
    pub completed: usize,
    pub downloaded: u64,
    /// Known only when every job's size is.
    pub total_size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceSettings {
    pub notifications: bool,
    pub smart_folders: bool,
    pub global_speed_limit: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueInfo {
    pub name: String,
    pub running: bool,
    pub jobs: usize,
    /// Jobs not yet completed or cancelled.
    pub pending: usize,
}

/// A rule of the scheduler. Times are minutes after local midnight.
///
/// With `to_minute`, the rule describes a window: the action holds from
/// `from_minute` until `to_minute` (a window may cross midnight) and is
/// undone afterwards. Without it, the action happens once at
/// `from_minute` and stays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    /// Assigned by the service.
    #[serde(default)]
    pub id: i64,
    /// Days the rule starts on: bit 0 is Monday, bit 6 Sunday.
    pub days: u8,
    pub from_minute: u16,
    pub to_minute: Option<u16>,
    pub action: ScheduleAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScheduleAction {
    /// Run the queue (during the window).
    RunQueue { queue: String },
    /// Limit the speed of everything (during the window).
    SpeedLimit { limit: u64 },
}

/// Every day of the week, for [`Schedule::days`].
pub const EVERY_DAY: u8 = 0b111_1111;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A job was added or its status changed.
    Job {
        job: Job,
    },
    Progress {
        job: JobId,
        downloaded: u64,
        total_size: Option<u64>,
        speed: u64,
        eta_secs: Option<u64>,
        connections: usize,
    },
    Retry {
        job: JobId,
        reason: String,
        delay_ms: u64,
    },
    Removed {
        job: JobId,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_have_a_readable_wire_format() {
        let line =
            r#"{"id":1,"method":"add","params":{"url":"https://e.com/a.iso","paused":true}}"#;
        let envelope: RequestEnvelope = serde_json::from_str(line).unwrap();
        assert_eq!(envelope.id, 1);
        let Request::Add(job) = envelope.request else {
            panic!("not an add")
        };
        assert_eq!(job.url, "https://e.com/a.iso");
        assert!(job.paused);
        assert!(job.mirrors.is_empty());

        let list: RequestEnvelope = serde_json::from_str(r#"{"id":2,"method":"list"}"#).unwrap();
        assert!(matches!(list.request, Request::List));
    }

    #[test]
    fn replies_and_events_round_trip() {
        let reply = ServerMessage::Reply(Reply {
            id: 3,
            outcome: Outcome::Ok(Response::Added { job: JobId(9) }),
        });
        let text = serde_json::to_string(&reply).unwrap();
        assert_eq!(text, r#"{"reply":{"id":3,"ok":{"type":"added","job":9}}}"#);

        let error = ServerMessage::Reply(Reply {
            id: 4,
            outcome: Outcome::Error("no".into()),
        });
        assert_eq!(
            serde_json::to_string(&error).unwrap(),
            r#"{"reply":{"id":4,"error":"no"}}"#
        );

        let event = ServerMessage::Event(Event::Removed { job: JobId(9) });
        let back: ServerMessage =
            serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap();
        assert!(matches!(
            back,
            ServerMessage::Event(Event::Removed { job: JobId(9) })
        ));
    }

    #[test]
    fn status_names_round_trip() {
        for name in ["queued", "downloading", "failed", "cancelled"] {
            assert_eq!(JobStatus::parse(name).unwrap().as_str(), name);
        }
        assert!(JobStatus::parse("bogus").is_none());
    }
}
