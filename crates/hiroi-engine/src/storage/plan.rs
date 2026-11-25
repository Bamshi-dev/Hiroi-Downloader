//! Decides where a download goes and whether it resumes earlier progress.

use std::path::Path;

use crate::constants::FALLBACK_FILE_NAME;
use crate::error::{EngineError, Result};
use crate::net::probe::RemoteFile;
use crate::request::{ConflictPolicy, DownloadRequest};
use crate::storage::control::ControlFile;
use crate::storage::naming::{DownloadPaths, numbered, sanitize_file_name};

/// Upper bound for `name (n).ext` before giving up.
const MAX_RENAME_ATTEMPTS: u32 = 9_999;

#[derive(Debug)]
pub(crate) struct Plan {
    pub paths: DownloadPaths,
    /// Progress from an earlier run that can be continued.
    pub resume: Option<ControlFile>,
}

pub(crate) fn prepare(
    request: &DownloadRequest,
    remote: &RemoteFile,
    source: &str,
) -> Result<Plan> {
    let dir = request.output_dir.as_path();
    std::fs::create_dir_all(dir).map_err(|e| EngineError::io(dir, e))?;

    let name = request
        .file_name
        .as_deref()
        .and_then(sanitize_file_name)
        .or_else(|| remote.file_name.as_deref().and_then(sanitize_file_name))
        .unwrap_or_else(|| FALLBACK_FILE_NAME.to_string());

    // Compared as progress records name sources: without the query.
    let previous = request
        .replaces
        .as_deref()
        .and_then(|address| reqwest::Url::parse(address).ok())
        .map(|url| crate::net::source_key(&url));
    let resumable = |paths: &DownloadPaths| resumable(paths, remote, source, previous.as_deref());
    let fresh = |paths| Plan {
        paths,
        resume: None,
    };

    let paths = DownloadPaths::new(dir, &name);
    if let Some(control) = resumable(&paths) {
        return Ok(Plan {
            paths,
            resume: Some(control),
        });
    }
    if !paths.final_path.exists() {
        return Ok(fresh(paths));
    }
    match request.conflict {
        ConflictPolicy::Overwrite => Ok(fresh(paths)),
        ConflictPolicy::Skip => Err(EngineError::AlreadyExists(paths.final_path)),
        ConflictPolicy::Rename => rename(dir, &name, resumable),
    }
}

fn rename(
    dir: &Path,
    name: &str,
    resumable: impl Fn(&DownloadPaths) -> Option<ControlFile>,
) -> Result<Plan> {
    for n in 1..=MAX_RENAME_ATTEMPTS {
        let paths = DownloadPaths::new(dir, &numbered(name, n));
        if let Some(control) = resumable(&paths) {
            return Ok(Plan {
                paths,
                resume: Some(control),
            });
        }
        // A part file without a matching control file may belong to another
        // download in progress, so that name is skipped as well.
        if !paths.final_path.exists() && !paths.part_path.exists() {
            return Ok(Plan {
                paths,
                resume: None,
            });
        }
    }
    Err(EngineError::AlreadyExists(dir.join(name)))
}

fn resumable(
    paths: &DownloadPaths,
    remote: &RemoteFile,
    source: &str,
    previous: Option<&str>,
) -> Option<ControlFile> {
    let size = remote.size.filter(|_| remote.ranges)?;
    if !paths.part_path.exists() {
        return None;
    }
    let mut control = ControlFile::load(&paths.control_path)?;
    if control.matches(source, size, remote.validator.as_deref()) {
        return Some(control);
    }
    // A refreshed address: the user says it serves the same file, and
    // the size has to agree. Versions are not compared, since another
    // server, or another signed link, names the same file differently.
    let refreshed = previous == Some(control.source.as_str()) && control.total_size == size;
    refreshed.then(|| {
        if control.validator.is_some() && control.validator != remote.validator {
            tracing::warn!(
                "the new address names its version differently; continuing on the user's word"
            );
        }
        control.source = source.to_string();
        control.validator = remote.validator.clone();
        control
    })
}
