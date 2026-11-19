use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

/// Identifies a download inside one [`crate::Engine`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DownloadId(pub(crate) u64);

impl DownloadId {
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for DownloadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadStatus {
    /// Waiting for a free slot in the queue.
    Queued,
    /// Asking the servers about the file.
    Connecting,
    Downloading,
    /// Stopped by the user; progress is kept.
    Paused,
    /// Checking the checksum of the finished file.
    Verifying,
    Completed,
    /// Stopped by an error. The message is safe to show (no secrets).
    Failed(String),
    /// Stopped by the user; temporary files were removed.
    Cancelled,
}

impl DownloadStatus {
    /// True when the download no longer runs and will not start on its own.
    pub fn is_settled(&self) -> bool {
        matches!(
            self,
            Self::Paused | Self::Completed | Self::Failed(_) | Self::Cancelled
        )
    }

    pub(crate) fn is_running(&self) -> bool {
        matches!(self, Self::Connecting | Self::Downloading | Self::Verifying)
    }
}

/// One contiguous byte range of the file, as shown in IDM's segment bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentSnapshot {
    pub start: u64,
    /// Exclusive.
    pub end: u64,
    pub downloaded: u64,
}

/// State of one download at a point in time.
#[derive(Debug, Clone)]
pub struct DownloadSnapshot {
    pub id: DownloadId,
    /// Display form of the URL, without credentials or query string.
    pub url: String,
    pub file_name: Option<String>,
    /// Where the finished file will be (or is).
    pub path: Option<PathBuf>,
    pub total_size: Option<u64>,
    pub downloaded: u64,
    /// Bytes per second, smoothed.
    pub speed: u64,
    pub eta: Option<Duration>,
    /// Connections transferring right now.
    pub connections: usize,
    /// Whether a pause keeps the progress (the server supports ranges).
    pub resumable: bool,
    pub status: DownloadStatus,
    pub segments: Vec<SegmentSnapshot>,
}

impl DownloadSnapshot {
    /// Progress between 0.0 and 1.0, if the size is known.
    pub fn fraction(&self) -> Option<f64> {
        match self.total_size {
            Some(0) => Some(1.0),
            Some(total) => Some(self.downloaded as f64 / total as f64),
            None => None,
        }
    }
}
