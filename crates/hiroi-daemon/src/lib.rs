//! The background service of Hiroi Downloader.
//!
//! The service owns one [`hiroi_engine::Engine`], keeps its queue in a
//! SQLite database (`hiroi-store`), and answers clients on a Unix domain
//! socket using the protocol of `hiroi-proto`. Downloads keep running
//! when the terminal that started them is closed, and the queue is
//! restored when the service starts again.
//!
//! A web page cannot open a Unix socket, so unlike a local HTTP API the
//! service is out of reach of the browser by construction. The socket
//! is readable by its owner only, and every connection is checked with
//! the kernel's peer credentials.

pub mod client;
mod convert;
pub mod paths;
pub mod schedule;
mod server;
mod service;

pub use client::{Client, ClientError};
pub use service::{DaemonConfig, DaemonError, run};
