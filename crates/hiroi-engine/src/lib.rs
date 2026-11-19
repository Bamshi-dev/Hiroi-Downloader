//! Download engine of Hiroi Downloader.
//!
//! The engine downloads files over HTTP(S) using several connections per file,
//! splits work dynamically so every connection stays busy until the end, and
//! keeps a small control file next to each download so it can be resumed after
//! a pause, a crash, or a reboot.
//!
//! The public surface is small:
//!
//! - [`Engine`] owns the download queue. Add a [`DownloadRequest`], then pause,
//!   resume, cancel, or change its speed limit by [`DownloadId`].
//! - [`DownloadListener`] receives events; [`DownloadSnapshot`] describes the
//!   state of one download at a point in time.
//! - [`EngineConfig`] holds the engine-wide settings.
//!
//! ```no_run
//! # async fn example() -> hiroi_engine::Result<()> {
//! use hiroi_engine::{DownloadRequest, Engine, EngineConfig};
//!
//! let engine = Engine::new(EngineConfig::default())?;
//! let id = engine.add(DownloadRequest::new("https://example.com/file.iso")?.connections(16));
//! let status = engine.wait(id).await;
//! println!("{id}: {status:?}");
//! # Ok(())
//! # }
//! ```

mod checksum;
mod config;
mod constants;
mod engine;
mod error;
mod exposure;
#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub mod fuzzing;
mod inspect;
mod limit;
mod listener;
mod media;
mod metalink;
mod net;
mod privacy;
mod request;
mod snapshot;
mod storage;
mod task;
#[cfg(feature = "torrent")]
mod torrent;
mod xml;

pub use checksum::{Checksum, HashAlgorithm};
pub use config::EngineConfig;
pub use constants::{CONTROL_EXTENSION, MAX_CONNECTIONS, PART_EXTENSION};
pub use engine::Engine;
pub use error::{EngineError, Result};
pub use exposure::{ExposureOptions, ExposureReport, Finding, Hop, IpCheck, Level, SourceTrace};
pub use inspect::{RemoteInfo, SourceInfo};
pub use listener::DownloadListener;
pub use net::retry::RetryPolicy;
pub use privacy::ProxyKind;
pub use request::{ConflictPolicy, DownloadRequest, Priority, Quality, StreamOptions};
/// URL type used throughout the API (re-exported from `reqwest`).
pub use reqwest::Url;
pub use snapshot::{DownloadId, DownloadSnapshot, DownloadStatus, SegmentSnapshot};
pub use storage::naming::sanitize_file_name;
