use std::io;
use std::path::PathBuf;

/// Everything that can go wrong in the engine.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("invalid URL `{0}`")]
    InvalidUrl(String),

    #[error("unsupported URL scheme `{0}` (this build downloads {list})", list = crate::request::scheme_list())]
    UnsupportedScheme(String),

    #[error("invalid header: {0}")]
    InvalidHeader(String),

    #[error("invalid checksum `{0}` (expected e.g. sha256:<hex>)")]
    InvalidChecksum(String),

    #[error("invalid proxy: {0}")]
    InvalidProxy(String),

    #[error("server answered {status} for {url}")]
    HttpStatus { status: u16, url: String },

    /// A connection failed on a protocol other than HTTP.
    #[error("connection failed: {0}")]
    Connection(String),

    /// Network failure. The URL is stripped so it cannot leak into logs.
    #[error("network error: {0}")]
    Network(#[source] reqwest::Error),

    #[error("server sent an invalid response: {0}")]
    Protocol(String),

    #[error("the file changed on the server, restart the download to get the new version")]
    RemoteChanged,

    #[error("no working source left (last error: {0})")]
    NoSources(String),

    #[error("not enough disk space: {needed} bytes needed, {available} available")]
    InsufficientSpace { needed: u64, available: u64 },

    #[error("`{}` already exists", .0.display())]
    AlreadyExists(PathBuf),

    #[error("`{}` is being downloaded by another process", .0.display())]
    FileLocked(PathBuf),

    #[error("checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },

    #[error("{bytes} bytes still fail their piece hashes after fetching them again")]
    BrokenPieces { bytes: u64 },

    #[error("I/O error on `{}`: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("download was stopped")]
    Cancelled,

    #[error("site extractor: {0}")]
    Extractor(String),
}

pub type Result<T> = std::result::Result<T, EngineError>;

impl EngineError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    pub(crate) fn network(error: reqwest::Error) -> Self {
        Self::Network(error.without_url())
    }
}
