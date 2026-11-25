//! Unpacking finished archives, with the programs the system already
//! has: 7-Zip, unzip, unrar, and tar. Off unless the user turns it on.
//!
//! Each archive goes into a new folder named after it, next to it, so
//! nothing that is already there is replaced. An archive split into
//! parts (`name.part1.rar`, `name.7z.001`, ...) is unpacked once every
//! part the service knows of has finished. Programs get an argument
//! list, never a shell, and no terminal, so one that wants a password
//! fails instead of waiting for it.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Zip,
    SevenZip,
    Rar,
    Tar,
}

/// An archive, or one part of a split one, read from its file name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Part {
    /// The name without part number and extension: `name` for
    /// `name.part2.rar`. Parts of one archive share it.
    pub set: String,
    pub number: u32,
    pub kind: Kind,
}

const TAR_SUFFIXES: &[&str] = &[
    ".tar", ".tar.gz", ".tgz", ".tar.bz2", ".tbz2", ".tar.xz", ".txz", ".tar.zst",
];

pub(crate) fn part_of(file_name: &str) -> Option<Part> {
    let lower = file_name.to_ascii_lowercase();
    let cut = |suffix_len: usize| file_name[..file_name.len() - suffix_len].to_string();
    // name.7z.001, name.zip.001
    if let Some((rest, number)) = lower.rsplit_once('.')
        && number.len() == 3
        && number.chars().all(|c| c.is_ascii_digit())
    {
        let kind = if rest.ends_with(".7z") {
            Kind::SevenZip
        } else if rest.ends_with(".zip") {
            Kind::Zip
        } else {
            return None;
        };
        let set = file_name[..rest.rfind('.')?].to_string();
        return Some(Part {
            set,
            number: number.parse().ok()?,
            kind,
        });
    }
    if let Some(stem) = lower.strip_suffix(".rar") {
        // name.part3.rar
        if let Some((set, number)) = stem.rsplit_once(".part")
            && !number.is_empty()
            && number.chars().all(|c| c.is_ascii_digit())
        {
            return Some(Part {
                set: file_name[..set.len()].to_string(),
                number: number.parse().ok()?,
                kind: Kind::Rar,
            });
        }
        return Some(Part {
            set: cut(4),
            number: 1,
            kind: Kind::Rar,
        });
    }
    if lower.ends_with(".zip") {
        return Some(Part {
            set: cut(4),
            number: 1,
            kind: Kind::Zip,
        });
    }
    if lower.ends_with(".7z") {
        return Some(Part {
            set: cut(3),
            number: 1,
            kind: Kind::SevenZip,
        });
    }
    TAR_SUFFIXES
        .iter()
        .find(|suffix| lower.ends_with(*suffix))
        .map(|suffix| Part {
            set: cut(suffix.len()),
            number: 1,
            kind: Kind::Tar,
        })
}

/// The commands that unpack `archive` into `folder`, best first; the
/// first one installed is used.
fn commands(kind: Kind, archive: &Path, folder: &Path) -> Vec<Vec<std::ffi::OsString>> {
    let a = archive.as_os_str().to_owned();
    let f = folder.as_os_str().to_owned();
    let mut out_flag = std::ffi::OsString::from("-o");
    out_flag.push(&f);
    // 7-Zip: extract with paths, skip files that exist, no questions.
    let seven = |program: &str| {
        vec![
            program.into(),
            "x".into(),
            "-y".into(),
            "-aos".into(),
            out_flag.clone(),
            a.clone(),
        ]
    };
    match kind {
        Kind::SevenZip => vec![seven("7z"), seven("7zz"), seven("7za")],
        Kind::Zip => vec![
            seven("7z"),
            seven("7zz"),
            vec![
                "unzip".into(),
                "-n".into(),
                "-q".into(),
                a.clone(),
                "-d".into(),
                f.clone(),
            ],
        ],
        Kind::Rar => vec![
            seven("7z"),
            seven("7zz"),
            vec![
                "unrar".into(),
                "x".into(),
                "-o-".into(),
                "-p-".into(),
                "-y".into(),
                a.clone(),
                f.clone(),
            ],
        ],
        Kind::Tar => vec![vec![
            "tar".into(),
            "-xf".into(),
            a.clone(),
            "-C".into(),
            f.clone(),
        ]],
    }
}

/// A folder for `set` next to the archive that does not exist yet.
pub(crate) fn folder_for(dir: &Path, set: &str) -> PathBuf {
    let base = dir.join(set);
    if !base.exists() {
        return base;
    }
    (2..)
        .map(|n| dir.join(format!("{set} ({n})")))
        .find(|candidate| !candidate.exists())
        .expect("some numbered name is free")
}

/// Unpacks `archive` into `folder`. Returns the program that did it, or
/// why it could not be done.
pub(crate) async fn unpack(kind: Kind, archive: &Path, folder: &Path) -> Result<String, String> {
    let candidates = commands(kind, archive, folder);
    let Some(argv) = candidates.iter().find(|argv| on_path(&argv[0])) else {
        let names: Vec<String> = candidates
            .iter()
            .map(|argv| argv[0].to_string_lossy().into_owned())
            .collect();
        return Err(format!(
            "no program to unpack it; install one of: {}",
            names.join(", ")
        ));
    };
    std::fs::create_dir_all(folder)
        .map_err(|e| format!("cannot create {}: {e}", folder.display()))?;
    let program = argv[0].to_string_lossy().into_owned();
    let output = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("{program} did not start: {e}"))?;
    if output.status.success() {
        return Ok(program);
    }
    // An empty folder left behind would only be confusing.
    let _ = std::fs::remove_dir(folder);
    let detail = String::from_utf8_lossy(&output.stderr);
    let last = detail
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    Err(format!(
        "{program} failed ({}): {}",
        output.status,
        last.trim()
    ))
}

fn on_path(program: &std::ffi::OsStr) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            let candidate = dir.join(program);
            candidate.is_file() || (cfg!(windows) && candidate.with_extension("exe").is_file())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(set: &str, number: u32, kind: Kind) -> Option<Part> {
        Some(Part {
            set: set.into(),
            number,
            kind,
        })
    }

    #[test]
    fn names_of_archives_and_parts() {
        assert_eq!(part_of("Course.part02.rar"), part("Course", 2, Kind::Rar));
        assert_eq!(part_of("Course.rar"), part("Course", 1, Kind::Rar));
        assert_eq!(part_of("Data.7z.001"), part("Data", 1, Kind::SevenZip));
        assert_eq!(part_of("Data.zip.003"), part("Data", 3, Kind::Zip));
        assert_eq!(part_of("photos.ZIP"), part("photos", 1, Kind::Zip));
        assert_eq!(part_of("src-1.2.tar.gz"), part("src-1.2", 1, Kind::Tar));
        assert_eq!(part_of("movie.mp4"), None);
        assert_eq!(part_of("notes.001"), None);
    }

    #[tokio::test]
    async fn zips_are_unpacked_into_a_new_folder() {
        if !on_path("zip".as_ref()) || !(on_path("unzip".as_ref()) || on_path("7z".as_ref())) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"inside").unwrap();
        let status = std::process::Command::new("zip")
            .current_dir(dir.path())
            .args(["-q", "pack.zip", "a.txt"])
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::create_dir(dir.path().join("pack")).unwrap();

        let folder = folder_for(dir.path(), "pack");
        assert_eq!(folder, dir.path().join("pack (2)"));
        unpack(Kind::Zip, &dir.path().join("pack.zip"), &folder)
            .await
            .unwrap();
        assert_eq!(std::fs::read(folder.join("a.txt")).unwrap(), b"inside");
    }

    #[tokio::test]
    async fn a_broken_archive_says_why() {
        if !(on_path("unzip".as_ref()) || on_path("7z".as_ref())) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("broken.zip");
        std::fs::write(&archive, b"not a zip").unwrap();
        let folder = dir.path().join("broken");
        let error = unpack(Kind::Zip, &archive, &folder).await.unwrap_err();
        assert!(error.contains("failed"), "{error}");
        assert!(!folder.exists(), "the empty folder is removed");
    }
}
