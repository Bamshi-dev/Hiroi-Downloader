//! Defaults and limits shared across the engine.

use std::time::Duration;

/// Connections per download unless the request says otherwise.
pub const DEFAULT_CONNECTIONS: usize = 8;
/// Hard upper bound for connections per download (same ceiling as IDM).
pub const MAX_CONNECTIONS: usize = 32;
/// Connections the engine opens to one host, across all downloads.
pub const DEFAULT_CONNECTIONS_PER_HOST: usize = 16;
/// Connections the engine opens in total, across all hosts.
pub const DEFAULT_TOTAL_CONNECTIONS: usize = 64;
/// Downloads that transfer at the same time; the rest wait in the queue.
pub const DEFAULT_ACTIVE_DOWNLOADS: usize = 3;

/// A segment is only split if both halves would be at least this big.
pub const MIN_SPLIT_SIZE: u64 = 1024 * 1024;
/// Bytes a connection buffers before writing them to disk.
pub const WRITE_BUFFER_SIZE: usize = 256 * 1024;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// A connection that delivers nothing for this long is treated as stalled.
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_REDIRECTS: usize = 10;

pub const MAX_ATTEMPTS: u32 = 8;
pub const RETRY_BASE_DELAY: Duration = Duration::from_millis(500);
pub const RETRY_MAX_DELAY: Duration = Duration::from_secs(60);
/// Longest `Retry-After` we accept; servers asking for more are capped.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(600);
/// Pause applied to a host that throttled us without sending `Retry-After`.
pub const DEFAULT_THROTTLE_PAUSE: Duration = Duration::from_secs(2);
/// After this long without throttling, a host may get one connection back.
pub const HOST_RECOVERY_WINDOW: Duration = Duration::from_secs(60);

/// Window over which each connection's speed is measured.
pub const SLOW_CONNECTION_WINDOW: Duration = Duration::from_secs(10);
/// A connection is replaced when it is this many times slower than the
/// average of the other connections of the same download.
pub const SLOW_CONNECTION_RATIO: u64 = 5;

/// How often progress is flushed to disk and the control file is rewritten.
pub const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(2);
/// How often listeners receive progress updates.
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

/// Extension of the file that holds data while a download is in progress.
pub const PART_EXTENSION: &str = "hiroi-part";
/// Extension of the control file that makes a download resumable.
pub const CONTROL_EXTENSION: &str = "hiroi-state";
pub const CONTROL_FILE_VERSION: u32 = 1;

/// File name used when neither the server nor the URL provides one.
pub const FALLBACK_FILE_NAME: &str = "download";
/// Longest file name (in bytes) most Linux file systems accept.
pub const MAX_FILE_NAME_BYTES: usize = 255;

/// Sites that block unknown clients still serve browsers, so by default the
/// engine identifies as one. Browser integrations should pass the real value.
pub const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0";
