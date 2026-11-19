//! Conversions between the protocol, the store, and the engine.

use std::path::Path;

use hiroi_engine::{
    Checksum, ConflictPolicy, DownloadRequest, DownloadSnapshot, DownloadStatus, Priority,
};
use hiroi_proto::{Conflict, Job, JobStatus, NewJob};
use hiroi_store::{JobOptions, StoredJob};

pub fn job_options(job: &NewJob) -> JobOptions {
    JobOptions {
        file_name: job.file_name.clone(),
        connections: job.connections,
        speed_limit: job.speed_limit,
        checksum: job.checksum.clone(),
        on_conflict: job.on_conflict,
        priority: job.priority,
        run_after: job.run_after.clone().filter(|argv| !argv.is_empty()),
        open_folder: job.open_folder,
    }
}

/// Builds the engine request for a stored job. `headers` come from the
/// client that added the job; after a restart there are none.
pub fn request(
    job: &StoredJob,
    headers: &[(String, String)],
) -> Result<DownloadRequest, hiroi_engine::EngineError> {
    let options = &job.options;
    let mut request = DownloadRequest::new(&job.url)?.output_dir(&job.dir);
    for mirror in &job.mirrors {
        request = request.mirror(mirror)?;
    }
    for (name, value) in headers {
        request = request.header(name, value)?;
    }
    if let Some(name) = &options.file_name {
        request = request.file_name(name);
    }
    if let Some(connections) = options.connections {
        request = request.connections(usize::from(connections));
    }
    if let Some(checksum) = &options.checksum {
        request = request.checksum(checksum.parse::<Checksum>()?);
    }
    request = request
        .speed_limit(options.speed_limit)
        .on_conflict(conflict(options.on_conflict))
        .priority(priority(options.priority));
    Ok(request)
}

/// Checks a new job before it is stored, so bad input is rejected
/// with a clear message instead of failing later.
pub fn validate(job: &NewJob, dir: &Path) -> Result<(), hiroi_engine::EngineError> {
    let stored = StoredJob {
        id: hiroi_proto::JobId(0),
        url: job.url.clone(),
        mirrors: job.mirrors.clone(),
        dir: dir.to_path_buf(),
        options: job_options(job),
        queue: String::new(),
        status: JobStatus::Queued,
        error: None,
        file_name: None,
        path: None,
        total_size: None,
        downloaded: 0,
        added_at: 0,
        finished_at: None,
    };
    request(&stored, &job.headers).map(drop)
}

pub fn status(status: &DownloadStatus) -> (JobStatus, Option<String>) {
    match status {
        DownloadStatus::Queued => (JobStatus::Queued, None),
        DownloadStatus::Connecting => (JobStatus::Connecting, None),
        DownloadStatus::Downloading => (JobStatus::Downloading, None),
        DownloadStatus::Paused => (JobStatus::Paused, None),
        DownloadStatus::Verifying => (JobStatus::Verifying, None),
        DownloadStatus::Completed => (JobStatus::Completed, None),
        DownloadStatus::Failed(message) => (JobStatus::Failed, Some(message.clone())),
        DownloadStatus::Cancelled => (JobStatus::Cancelled, None),
    }
}

/// The client's view of a job: what the store knows, overlaid with the
/// engine's live numbers while the job is loaded.
pub fn job(stored: &StoredJob, live: Option<&DownloadSnapshot>) -> Job {
    let mut job = Job {
        id: stored.id,
        url: display_url(&stored.url),
        file_name: stored.file_name.clone(),
        path: stored.path.clone(),
        queue: stored.queue.clone(),
        status: stored.status,
        error: stored.error.clone(),
        total_size: stored.total_size,
        downloaded: stored.downloaded,
        speed: 0,
        eta_secs: None,
        connections: 0,
        resumable: false,
        added_at: stored.added_at,
        finished_at: stored.finished_at,
        segments: Vec::new(),
    };
    if let Some(live) = live {
        let (status, error) = status(&live.status);
        // A failed job is loaded paused so it can be resumed; keep showing
        // it as failed until it runs again.
        if !(stored.status == JobStatus::Failed && status == JobStatus::Paused) {
            job.status = status;
            job.error = error.or(job.error);
        }
        job.file_name = live.file_name.clone().or(job.file_name);
        job.path = live.path.clone().or(job.path);
        job.total_size = live.total_size.or(job.total_size);
        job.downloaded = live.downloaded.max(job.downloaded);
        job.speed = live.speed;
        job.eta_secs = live.eta.map(|eta| eta.as_secs());
        job.connections = live.connections;
        job.resumable = live.resumable;
        job.segments = live
            .segments
            .iter()
            .map(|s| (s.start, s.end, s.downloaded))
            .collect();
    }
    job
}

/// Removes user name and password; they are never stored.
pub fn strip_credentials(url: &str) -> String {
    match hiroi_engine::Url::parse(url) {
        Ok(mut parsed) if !parsed.username().is_empty() || parsed.password().is_some() => {
            let _ = parsed.set_username("");
            let _ = parsed.set_password(None);
            parsed.to_string()
        }
        _ => url.to_string(),
    }
}

/// The URL as shown to clients: no credentials, no query string.
pub fn display_url(url: &str) -> String {
    match hiroi_engine::Url::parse(url) {
        Ok(mut parsed) => {
            let had_query = parsed.query().is_some();
            let _ = parsed.set_username("");
            let _ = parsed.set_password(None);
            parsed.set_query(None);
            parsed.set_fragment(None);
            let mut shown = parsed.to_string();
            if had_query {
                shown.push_str("?…");
            }
            shown
        }
        Err(_) => url.to_string(),
    }
}

fn conflict(conflict: Option<Conflict>) -> ConflictPolicy {
    match conflict {
        None | Some(Conflict::Rename) => ConflictPolicy::Rename,
        Some(Conflict::Overwrite) => ConflictPolicy::Overwrite,
        Some(Conflict::Skip) => ConflictPolicy::Skip,
    }
}

fn priority(priority: Option<hiroi_proto::Priority>) -> Priority {
    match priority {
        Some(hiroi_proto::Priority::Low) => Priority::Low,
        None | Some(hiroi_proto::Priority::Normal) => Priority::Normal,
        Some(hiroi_proto::Priority::High) => Priority::High,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_never_reach_the_store_or_clients() {
        let url = "ftp://alice:secret@files.example.org/a.iso?token=xyz";
        assert_eq!(
            strip_credentials(url),
            "ftp://files.example.org/a.iso?token=xyz"
        );
        assert_eq!(display_url(url), "ftp://files.example.org/a.iso?…");
        assert_eq!(strip_credentials("https://e.com/a"), "https://e.com/a");
    }
}
