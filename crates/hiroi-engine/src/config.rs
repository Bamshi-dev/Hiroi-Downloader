use std::time::Duration;

use crate::constants;
use crate::net::retry::RetryPolicy;

/// Engine-wide settings. Per-download settings live in [`crate::DownloadRequest`].
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Downloads transferring at the same time. Others wait in the queue.
    pub max_active_downloads: usize,
    /// Upper bound for connections to a single host across all downloads.
    /// The engine lowers it on its own when a host answers 429 or 503.
    pub max_connections_per_host: usize,
    /// Upper bound for connections across all hosts.
    pub max_total_connections: usize,
    /// Connections per download when the request does not set its own.
    pub default_connections: usize,
    /// Combined speed limit in bytes per second, `None` for unlimited.
    pub global_speed_limit: Option<u64>,
    pub user_agent: String,
    /// Proxy for all requests, e.g. `http://host:3128` or `socks5h://host:1080`.
    pub proxy: Option<String>,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub retry: RetryPolicy,
    /// Segments are never split into halves smaller than this.
    pub min_split_size: u64,
    /// Reserve the full file size on disk before downloading.
    pub preallocate: bool,
    /// Follow redirects from HTTPS to plain HTTP. Off by default: such a
    /// redirect exposes the download to tampering on the network.
    pub allow_insecure_redirects: bool,
    pub checkpoint_interval: Duration,
    pub progress_interval: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_active_downloads: constants::DEFAULT_ACTIVE_DOWNLOADS,
            max_connections_per_host: constants::DEFAULT_CONNECTIONS_PER_HOST,
            max_total_connections: constants::DEFAULT_TOTAL_CONNECTIONS,
            default_connections: constants::DEFAULT_CONNECTIONS,
            global_speed_limit: None,
            user_agent: constants::DEFAULT_USER_AGENT.to_string(),
            proxy: None,
            connect_timeout: constants::CONNECT_TIMEOUT,
            read_timeout: constants::READ_TIMEOUT,
            retry: RetryPolicy::default(),
            min_split_size: constants::MIN_SPLIT_SIZE,
            preallocate: true,
            allow_insecure_redirects: false,
            checkpoint_interval: constants::CHECKPOINT_INTERVAL,
            progress_interval: constants::PROGRESS_INTERVAL,
        }
    }
}

impl EngineConfig {
    /// Clamps values that would make the engine misbehave (zero limits and so on).
    pub(crate) fn normalized(mut self) -> Self {
        self.max_active_downloads = self.max_active_downloads.max(1);
        self.max_connections_per_host = self.max_connections_per_host.max(1);
        self.max_total_connections = self.max_total_connections.max(1);
        self.default_connections = self
            .default_connections
            .clamp(1, constants::MAX_CONNECTIONS);
        self.min_split_size = self.min_split_size.max(64 * 1024);
        self.global_speed_limit = self.global_speed_limit.filter(|&limit| limit > 0);
        self
    }
}
