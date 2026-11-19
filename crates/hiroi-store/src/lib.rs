//! SQLite persistence for the Hiroi service.
//!
//! The store keeps what must survive a restart: the jobs with their
//! options and last known state, and the service's settings. Progress
//! inside a file is not stored here; each download keeps its own
//! `.hiroi-state` file next to the data, written by the engine.
//!
//! What is never stored: request headers, which hold cookies and
//! tokens, and credentials embedded in URLs. A job that needs them must
//! get them again from its client after a restart.
//!
//! The store is synchronous. The service calls it from a blocking
//! thread; writes are rare (a status change, a progress checkpoint every
//! few seconds), so one connection behind a mutex is enough.

mod migrations;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use hiroi_proto::{Conflict, JobId, JobStatus, Priority, Schedule};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("could not prepare `{}`: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("stored data is damaged: {0}")]
    Corrupt(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Everything needed to start a job again after a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredJob {
    pub id: JobId,
    /// The URL as given, without user name or password.
    pub url: String,
    pub mirrors: Vec<String>,
    pub dir: PathBuf,
    pub options: JobOptions,
    pub queue: String,
    pub status: JobStatus,
    pub error: Option<String>,
    pub file_name: Option<String>,
    pub path: Option<PathBuf>,
    pub total_size: Option<u64>,
    pub downloaded: u64,
    pub added_at: i64,
    pub finished_at: Option<i64>,
}

/// Per-job options, stored as one JSON column so new options need no
/// schema change.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct JobOptions {
    pub file_name: Option<String>,
    pub connections: Option<u8>,
    pub speed_limit: Option<u64>,
    pub checksum: Option<String>,
    pub on_conflict: Option<Conflict>,
    pub priority: Option<Priority>,
}

/// A job about to be inserted; the store assigns the id.
#[derive(Debug, Clone)]
pub struct InsertJob {
    pub url: String,
    pub mirrors: Vec<String>,
    pub dir: PathBuf,
    pub options: JobOptions,
    pub queue: String,
    pub status: JobStatus,
    pub added_at: i64,
}

/// Fields that change while a job runs.
#[derive(Debug, Clone, Default)]
pub struct JobUpdate {
    pub status: Option<JobStatus>,
    /// `Some(None)` clears the error.
    pub error: Option<Option<String>>,
    pub file_name: Option<String>,
    pub path: Option<PathBuf>,
    pub total_size: Option<u64>,
    pub downloaded: Option<u64>,
    pub finished_at: Option<i64>,
}

#[derive(Debug)]
pub struct Store {
    connection: Mutex<Connection>,
}

impl Store {
    /// Opens or creates the database and brings its schema up to date.
    /// The file is created readable by its owner only.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            create_private_dir(parent)?;
        }
        let connection = Connection::open(path)?;
        restrict_permissions(path)?;
        Self::prepare(connection)
    }

    /// An in-memory database, for tests.
    pub fn in_memory() -> Result<Self> {
        Self::prepare(Connection::open_in_memory()?)
    }

    fn prepare(mut connection: Connection) -> Result<Self> {
        // WAL lets readers proceed during a write; NORMAL sync is safe
        // with WAL and avoids an fsync per transaction.
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        migrations::apply(&mut connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn insert_job(&self, job: &InsertJob) -> Result<JobId> {
        let connection = self.lock();
        connection.execute(
            "INSERT INTO jobs (url, mirrors, dir, options, queue, status, added_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                job.url,
                to_json(&job.mirrors),
                path_text(&job.dir),
                to_json(&job.options),
                job.queue,
                job.status.as_str(),
                job.added_at,
            ],
        )?;
        Ok(JobId(connection.last_insert_rowid()))
    }

    pub fn update_job(&self, id: JobId, update: &JobUpdate) -> Result<()> {
        let connection = self.lock();
        let transaction = connection.unchecked_transaction()?;
        if let Some(status) = update.status {
            transaction.execute(
                "UPDATE jobs SET status = ?2 WHERE id = ?1",
                params![id.0, status.as_str()],
            )?;
        }
        if let Some(error) = &update.error {
            transaction.execute(
                "UPDATE jobs SET error = ?2 WHERE id = ?1",
                params![id.0, error],
            )?;
        }
        if let Some(name) = &update.file_name {
            transaction.execute(
                "UPDATE jobs SET file_name = ?2 WHERE id = ?1",
                params![id.0, name],
            )?;
        }
        if let Some(path) = &update.path {
            transaction.execute(
                "UPDATE jobs SET path = ?2 WHERE id = ?1",
                params![id.0, path_text(path)],
            )?;
        }
        if let Some(total) = update.total_size {
            transaction.execute(
                "UPDATE jobs SET total_size = ?2 WHERE id = ?1",
                params![id.0, total as i64],
            )?;
        }
        if let Some(downloaded) = update.downloaded {
            transaction.execute(
                "UPDATE jobs SET downloaded = ?2 WHERE id = ?1",
                params![id.0, downloaded as i64],
            )?;
        }
        if let Some(finished) = update.finished_at {
            transaction.execute(
                "UPDATE jobs SET finished_at = ?2 WHERE id = ?1",
                params![id.0, finished],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn job(&self, id: JobId) -> Result<Option<StoredJob>> {
        let connection = self.lock();
        let mut statement = connection.prepare(&format!("{SELECT_JOB} WHERE id = ?1"))?;
        statement
            .query_row(params![id.0], read_job)
            .optional()?
            .transpose()
    }

    /// All jobs, oldest first.
    pub fn jobs(&self) -> Result<Vec<StoredJob>> {
        let connection = self.lock();
        let mut statement = connection.prepare(&format!("{SELECT_JOB} ORDER BY id"))?;
        let rows = statement.query_map([], read_job)?;
        rows.map(|row| row?).collect()
    }

    pub fn delete_job(&self, id: JobId) -> Result<bool> {
        let deleted = self
            .lock()
            .execute("DELETE FROM jobs WHERE id = ?1", params![id.0])?;
        Ok(deleted > 0)
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        let connection = self.lock();
        let value = connection
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.lock().execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Names of the queues that are held.
    pub fn stopped_queues(&self) -> Result<Vec<String>> {
        let connection = self.lock();
        let mut statement =
            connection.prepare("SELECT name FROM queues WHERE stopped = 1 ORDER BY name")?;
        let names = statement.query_map([], |row| row.get(0))?;
        Ok(names.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_queue_stopped(&self, name: &str, stopped: bool) -> Result<()> {
        self.lock().execute(
            "INSERT INTO queues (name, stopped) VALUES (?1, ?2)
             ON CONFLICT (name) DO UPDATE SET stopped = excluded.stopped",
            params![name, stopped],
        )?;
        Ok(())
    }

    pub fn schedules(&self) -> Result<Vec<Schedule>> {
        let connection = self.lock();
        let mut statement = connection.prepare(
            "SELECT id, days, from_minute, to_minute, action FROM schedules ORDER BY id",
        )?;
        let rows = statement.query_map([], |row| {
            let action: String = row.get(4)?;
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, action))
        })?;
        let mut schedules = Vec::new();
        for row in rows {
            let (id, days, from_minute, to_minute, action) = row?;
            let action = serde_json::from_str(&action)
                .map_err(|_| StoreError::Corrupt(format!("unreadable schedule {id}")))?;
            schedules.push(Schedule {
                id,
                days,
                from_minute,
                to_minute,
                action,
            });
        }
        Ok(schedules)
    }

    pub fn insert_schedule(&self, schedule: &Schedule) -> Result<i64> {
        let connection = self.lock();
        connection.execute(
            "INSERT INTO schedules (days, from_minute, to_minute, action) VALUES (?1, ?2, ?3, ?4)",
            params![
                schedule.days,
                schedule.from_minute,
                schedule.to_minute,
                to_json(&schedule.action)
            ],
        )?;
        Ok(connection.last_insert_rowid())
    }

    pub fn delete_schedule(&self, id: i64) -> Result<bool> {
        Ok(self
            .lock()
            .execute("DELETE FROM schedules WHERE id = ?1", params![id])?
            > 0)
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

const SELECT_JOB: &str = "SELECT id, url, mirrors, dir, options, queue, status, error,
    file_name, path, total_size, downloaded, added_at, finished_at FROM jobs";

/// Row mapping. The outer `Result` is SQLite's, the inner one ours, so
/// a damaged JSON column is reported instead of panicking.
fn read_job(row: &Row<'_>) -> rusqlite::Result<Result<StoredJob>> {
    let status: String = row.get(6)?;
    let mirrors: String = row.get(2)?;
    let options: String = row.get(4)?;
    let Some(status) = JobStatus::parse(&status) else {
        return Ok(Err(StoreError::Corrupt(format!(
            "unknown status `{status}`"
        ))));
    };
    let (Ok(mirrors), Ok(options)) = (
        serde_json::from_str(&mirrors),
        serde_json::from_str(&options),
    ) else {
        return Ok(Err(StoreError::Corrupt("unreadable job options".into())));
    };
    Ok(Ok(StoredJob {
        id: JobId(row.get(0)?),
        url: row.get(1)?,
        mirrors,
        dir: PathBuf::from(row.get::<_, String>(3)?),
        options,
        queue: row.get(5)?,
        status,
        error: row.get(7)?,
        file_name: row.get(8)?,
        path: row.get::<_, Option<String>>(9)?.map(PathBuf::from),
        total_size: row.get::<_, Option<i64>>(10)?.map(|size| size as u64),
        downloaded: row.get::<_, i64>(11)? as u64,
        added_at: row.get(12)?,
        finished_at: row.get(13)?,
    }))
}

fn to_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("plain data always serializes")
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn create_private_dir(dir: &Path) -> Result<()> {
    let io = |source| StoreError::Io {
        path: dir.to_path_buf(),
        source,
    };
    std::fs::create_dir_all(dir).map_err(io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(io)?;
    }
    Ok(())
}

fn restrict_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let io = |source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(io)?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(url: &str) -> InsertJob {
        InsertJob {
            url: url.into(),
            mirrors: vec!["https://mirror.example.org/a.iso".into()],
            dir: PathBuf::from("/tmp/downloads"),
            options: JobOptions {
                connections: Some(16),
                ..JobOptions::default()
            },
            queue: "main".into(),
            status: JobStatus::Queued,
            added_at: 1_700_000_000,
        }
    }

    #[test]
    fn jobs_round_trip_and_update() {
        let store = Store::in_memory().unwrap();
        let id = store
            .insert_job(&sample("https://example.com/a.iso"))
            .unwrap();
        let update = JobUpdate {
            status: Some(JobStatus::Failed),
            error: Some(Some("server answered 404".into())),
            total_size: Some(1000),
            downloaded: Some(250),
            ..JobUpdate::default()
        };
        store.update_job(id, &update).unwrap();

        let job = store.job(id).unwrap().unwrap();
        assert_eq!(job.status, JobStatus::Failed);
        assert_eq!(job.error.as_deref(), Some("server answered 404"));
        assert_eq!((job.total_size, job.downloaded), (Some(1000), 250));
        assert_eq!(job.options.connections, Some(16));
        assert_eq!(job.mirrors.len(), 1);

        store
            .update_job(
                id,
                &JobUpdate {
                    error: Some(None),
                    ..JobUpdate::default()
                },
            )
            .unwrap();
        assert_eq!(store.job(id).unwrap().unwrap().error, None);
    }

    #[test]
    fn lists_in_order_and_deletes() {
        let store = Store::in_memory().unwrap();
        let first = store.insert_job(&sample("https://example.com/1")).unwrap();
        let second = store.insert_job(&sample("https://example.com/2")).unwrap();
        let ids: Vec<_> = store.jobs().unwrap().iter().map(|job| job.id).collect();
        assert_eq!(ids, [first, second]);
        assert!(store.delete_job(first).unwrap());
        assert!(!store.delete_job(first).unwrap());
        assert_eq!(store.jobs().unwrap().len(), 1);
    }

    #[test]
    fn queues_and_schedules_persist() {
        use hiroi_proto::{EVERY_DAY, ScheduleAction};
        let store = Store::in_memory().unwrap();
        store.set_queue_stopped("night", true).unwrap();
        store.set_queue_stopped("main", false).unwrap();
        assert_eq!(store.stopped_queues().unwrap(), ["night"]);

        let rule = Schedule {
            id: 0,
            days: EVERY_DAY,
            from_minute: 60,
            to_minute: Some(420),
            action: ScheduleAction::RunQueue {
                queue: "night".into(),
            },
        };
        let id = store.insert_schedule(&rule).unwrap();
        assert_eq!(store.schedules().unwrap(), [Schedule { id, ..rule }]);
        assert!(store.delete_schedule(id).unwrap());
        assert!(store.schedules().unwrap().is_empty());
    }

    #[test]
    fn settings_overwrite() {
        let store = Store::in_memory().unwrap();
        assert_eq!(store.setting("speed").unwrap(), None);
        store.set_setting("speed", "1").unwrap();
        store.set_setting("speed", "2").unwrap();
        assert_eq!(store.setting("speed").unwrap().as_deref(), Some("2"));
    }

    #[test]
    fn database_file_is_private_and_survives_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("hiroi.db");
        let id = Store::open(&path)
            .unwrap()
            .insert_job(&sample("https://example.com/x"))
            .unwrap();
        assert!(Store::open(&path).unwrap().job(id).unwrap().is_some());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
