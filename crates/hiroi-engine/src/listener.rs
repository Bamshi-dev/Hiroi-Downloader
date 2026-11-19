use std::time::Duration;

use crate::error::EngineError;
use crate::snapshot::{DownloadId, DownloadSnapshot};

/// Receives engine events. Every method has an empty default, so implement
/// only what you need.
///
/// Methods are called from the engine's tasks: keep them short and hand
/// heavier work to another thread or channel.
pub trait DownloadListener: Send + Sync {
    /// A download was added to the queue.
    fn on_added(&self, _snapshot: &DownloadSnapshot) {}

    /// Status changed (queued, connecting, downloading, paused, ...).
    fn on_status_changed(&self, _snapshot: &DownloadSnapshot) {}

    /// Periodic progress update while downloading.
    fn on_progress(&self, _snapshot: &DownloadSnapshot) {}

    /// A connection failed and will be retried after `delay`.
    fn on_retry(&self, _id: DownloadId, _reason: &str, _delay: Duration) {}

    fn on_completed(&self, _snapshot: &DownloadSnapshot) {}

    fn on_failed(&self, _snapshot: &DownloadSnapshot, _error: &EngineError) {}
}
