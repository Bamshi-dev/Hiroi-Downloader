//! The `.hiroi-state` file that makes a download resumable.
//!
//! It records which byte ranges are on disk, plus enough about the remote
//! file to tell whether it is still the same one. Cookies, credentials, and
//! query strings are never written here.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::constants::CONTROL_FILE_VERSION;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ControlFile {
    pub version: u32,
    /// See [`crate::net::source_key`].
    pub source: String,
    pub total_size: u64,
    pub validator: Option<String>,
    pub segments: Vec<SavedSegment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SavedSegment {
    pub start: u64,
    pub end: u64,
    pub written: u64,
}

impl ControlFile {
    pub fn new(source: String, total_size: u64, validator: Option<String>) -> Self {
        Self {
            version: CONTROL_FILE_VERSION,
            source,
            total_size,
            validator,
            segments: Vec::new(),
        }
    }

    /// Loads a control file; `None` if it is missing, from another version,
    /// or inconsistent (a damaged file must never cause a corrupt download).
    pub fn load(path: &Path) -> Option<Self> {
        Self::from_bytes(&std::fs::read(path).ok()?)
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let control: Self = serde_json::from_slice(bytes).ok()?;
        (control.version == CONTROL_FILE_VERSION && control.is_consistent()).then_some(control)
    }

    pub fn matches(&self, source: &str, total_size: u64, validator: Option<&str>) -> bool {
        self.source == source
            && self.total_size == total_size
            && self.validator.as_deref() == validator
    }

    /// Writes atomically: a crash mid-save leaves the previous version intact.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let tmp = tmp_path(path);
        std::fs::write(&tmp, serde_json::to_vec(self)?)?;
        std::fs::rename(&tmp, path)
    }

    pub fn written(&self) -> u64 {
        self.segments.iter().map(|s| s.written).sum()
    }

    fn is_consistent(&self) -> bool {
        let mut segments = self.segments.clone();
        segments.sort_by_key(|s| s.start);
        let bounded = segments
            .iter()
            .all(|s| s.start <= s.end && s.end <= self.total_size && s.written <= s.end - s.start);
        // Only bounded segments can be measured without overflowing.
        if !bounded || !segments.windows(2).all(|pair| pair[0].end <= pair[1].start) {
            return false;
        }
        let covered = segments
            .iter()
            .try_fold(0u64, |sum, s| sum.checked_add(s.end - s.start));
        covered == Some(self.total_size)
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    PathBuf::from(tmp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damaged_segments_are_refused_not_panicked_on() {
        let backwards = br#"{"version":1,"source":"s","total_size":100,"validator":null,
            "segments":[{"start":90,"end":10,"written":0}]}"#;
        assert!(ControlFile::from_bytes(backwards).is_none());
        let huge =
            br#"{"version":1,"source":"s","total_size":18446744073709551615,"validator":null,
            "segments":[{"start":0,"end":18446744073709551615,"written":0},
                        {"start":0,"end":18446744073709551615,"written":0}]}"#;
        assert!(ControlFile::from_bytes(huge).is_none());
    }

    fn sample() -> ControlFile {
        let mut control = ControlFile::new("https://e.com/f".into(), 100, Some("\"v1\"".into()));
        control.segments = vec![
            SavedSegment {
                start: 0,
                end: 60,
                written: 60,
            },
            SavedSegment {
                start: 60,
                end: 100,
                written: 10,
            },
        ];
        control
    }

    #[test]
    fn round_trips_and_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.hiroi-state");
        sample().save(&path).unwrap();
        let loaded = ControlFile::load(&path).unwrap();
        assert_eq!(loaded, sample());
        assert_eq!(loaded.written(), 70);
        assert!(loaded.matches("https://e.com/f", 100, Some("\"v1\"")));
        assert!(!loaded.matches("https://e.com/f", 100, Some("\"v2\"")));
        assert!(!loaded.matches("https://e.com/f", 101, Some("\"v1\"")));
    }

    #[test]
    fn rejects_damaged_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.hiroi-state");

        let mut overlapping = sample();
        overlapping.segments[1].start = 50;
        overlapping.save(&path).unwrap();
        assert!(ControlFile::load(&path).is_none());

        let mut gap = sample();
        gap.segments[1].end = 90;
        gap.save(&path).unwrap();
        assert!(ControlFile::load(&path).is_none());

        std::fs::write(&path, b"{not json").unwrap();
        assert!(ControlFile::load(&path).is_none());
    }
}
