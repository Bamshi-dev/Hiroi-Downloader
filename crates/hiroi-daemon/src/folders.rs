//! Smart folders: once two finished downloads in one folder turn out to
//! be episodes of one series (`gardening vid 1.mp4`, `gardening vid
//! 2.mp4`), they move into a folder named after it, and later episodes
//! follow them there.
//!
//! Only files this service downloaded are moved, as recorded in its
//! jobs; other files in the folder are never touched. Nothing is moved
//! over an existing file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use hiroi_organize::{Episode, episode};
use hiroi_proto::{JobId, JobStatus};
use hiroi_store::StoredJob;

/// One file to move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Move {
    pub job: JobId,
    pub from: PathBuf,
    pub to: PathBuf,
}

/// The moves that the job `finished` calls for.
pub(crate) fn plan(finished: JobId, rows: &BTreeMap<JobId, StoredJob>) -> Vec<Move> {
    let Some((path, own)) = rows.get(&finished).and_then(member) else {
        return Vec::new();
    };
    let Some(parent) = path.parent() else {
        return Vec::new();
    };
    let others: Vec<(&StoredJob, &Path)> = rows
        .values()
        .filter(|row| row.id != finished && row.status == JobStatus::Completed)
        .filter_map(|row| {
            let (path, episode) = member(row)?;
            episode.same_series(&own).then_some((row, path))
        })
        .collect();

    // A folder made for this series earlier, holding earlier episodes.
    let existing = others
        .iter()
        .filter_map(|(_, path)| path.parent())
        .find(|folder| {
            folder.parent() == Some(parent)
                && folder
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| own.is_named(name))
        })
        .map(Path::to_path_buf);
    let siblings: Vec<&(&StoredJob, &Path)> = others
        .iter()
        .filter(|(_, path)| path.parent() == Some(parent))
        .collect();
    let folder = match existing {
        Some(folder) => folder,
        None if siblings.is_empty() => return Vec::new(),
        None => {
            // Named after the series as its first episode wrote it.
            let first = siblings
                .iter()
                .min_by_key(|(row, _)| (row.finished_at, row.id))
                .and_then(|(_, path)| member_episode(path))
                .unwrap_or_else(|| own.clone());
            let Some(name) = hiroi_engine::sanitize_file_name(&first.series) else {
                return Vec::new();
            };
            parent.join(name)
        }
    };
    // Saved into the series' folder already.
    if parent == folder {
        return Vec::new();
    }

    std::iter::once((finished, path))
        .chain(siblings.iter().map(|(row, path)| (row.id, *path)))
        .filter_map(|(job, from)| {
            let to = folder.join(from.file_name()?);
            Some(Move {
                job,
                from: from.to_path_buf(),
                to,
            })
        })
        .collect()
}

/// Carries out `moves`, skipping any whose target exists, and returns
/// those that happened.
pub(crate) fn apply(moves: Vec<Move>) -> Vec<Move> {
    let Some(folder) = moves.first().and_then(|first| first.to.parent()) else {
        return Vec::new();
    };
    if folder.exists() && !folder.is_dir() {
        tracing::warn!(folder = %folder.display(), "a file has the series' name; not grouping");
        return Vec::new();
    }
    if let Err(error) = std::fs::create_dir_all(folder) {
        tracing::warn!(folder = %folder.display(), %error, "cannot create the series folder");
        return Vec::new();
    }
    moves
        .into_iter()
        .filter(|step| {
            if step.to.exists() {
                return false;
            }
            match std::fs::rename(&step.from, &step.to) {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(file = %step.from.display(), %error, "cannot move into the series folder");
                    false
                }
            }
        })
        .collect()
}

/// A finished file of the service that is an episode, and still where
/// the service left it.
fn member(row: &StoredJob) -> Option<(&Path, Episode)> {
    let path = row.path.as_deref()?;
    let episode = member_episode(path)?;
    path.is_file().then_some((path, episode))
}

fn member_episode(path: &Path) -> Option<Episode> {
    episode(path.file_name()?.to_str()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hiroi_store::JobOptions;

    fn row(id: i64, path: &Path) -> StoredJob {
        std::fs::write(path, b"x").unwrap();
        StoredJob {
            id: JobId(id),
            url: format!("https://example.com/{id}"),
            mirrors: Vec::new(),
            dir: path.parent().unwrap().to_path_buf(),
            options: JobOptions::default(),
            queue: "main".into(),
            status: JobStatus::Completed,
            error: None,
            file_name: None,
            path: Some(path.to_path_buf()),
            total_size: None,
            downloaded: 1,
            added_at: 0,
            finished_at: Some(id),
        }
    }

    fn run(rows: &mut BTreeMap<JobId, StoredJob>, id: i64) {
        for step in apply(plan(JobId(id), rows)) {
            rows.get_mut(&step.job).unwrap().path = Some(step.to);
        }
    }

    #[test]
    fn episodes_gather_in_a_folder_and_others_stay() {
        let dir = tempfile::tempdir().unwrap();
        let at = |name: &str| dir.path().join(name);
        std::fs::write(at("gardening vid 9.mp4"), b"not ours").unwrap();
        let mut rows = BTreeMap::new();
        rows.insert(JobId(1), row(1, &at("gardening vid 1.mp4")));
        run(&mut rows, 1);
        assert!(at("gardening vid 1.mp4").is_file(), "one episode stays put");

        rows.insert(JobId(2), row(2, &at("Gardening_vid_2.mp4")));
        run(&mut rows, 2);
        let folder = at("gardening");
        assert!(folder.join("gardening vid 1.mp4").is_file());
        assert!(folder.join("Gardening_vid_2.mp4").is_file());
        assert!(
            at("gardening vid 9.mp4").is_file(),
            "files of others are not moved"
        );

        rows.insert(JobId(3), row(3, &at("gardening vid 3.mp4")));
        run(&mut rows, 3);
        assert!(folder.join("gardening vid 3.mp4").is_file());

        rows.insert(JobId(4), row(4, &at("unrelated.pdf")));
        run(&mut rows, 4);
        assert!(at("unrelated.pdf").is_file());
    }

    #[test]
    fn nothing_is_moved_over_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let at = |name: &str| dir.path().join(name);
        std::fs::create_dir(at("algebra")).unwrap();
        std::fs::write(at("algebra").join("algebra 1.pdf"), b"theirs").unwrap();
        let mut rows = BTreeMap::new();
        rows.insert(JobId(1), row(1, &at("algebra 1.pdf")));
        rows.insert(JobId(2), row(2, &at("algebra 2.pdf")));
        run(&mut rows, 2);
        assert_eq!(
            std::fs::read(at("algebra").join("algebra 1.pdf")).unwrap(),
            b"theirs"
        );
        assert!(at("algebra 1.pdf").is_file());
        assert!(at("algebra").join("algebra 2.pdf").is_file());
    }
}
