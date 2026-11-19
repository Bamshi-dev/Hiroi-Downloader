//! The local connection between the service and its clients: a Unix
//! socket, private to the user (mode 0600, in a 0700 folder, and the
//! peer's user id checked on every connection).
//!
//! Other systems have no service yet. There, binding and connecting
//! fail with [`std::io::ErrorKind::Unsupported`], and the listener type
//! has no values, so the code that serves it compiles to nothing.

use std::io;
use std::path::Path;

#[cfg(unix)]
pub(crate) type Stream = tokio::net::UnixStream;
#[cfg(unix)]
pub(crate) type Listener = tokio::net::UnixListener;

/// Never created: connecting always fails here.
#[cfg(not(unix))]
pub(crate) type Stream = tokio::io::DuplexStream;
#[cfg(not(unix))]
#[derive(Debug)]
pub(crate) enum Listener {}

pub(crate) const UNSUPPORTED: &str =
    "the background service runs on Linux and macOS for now; `hiroi get` works everywhere";

#[cfg(unix)]
pub(crate) async fn connect(path: &Path) -> io::Result<Stream> {
    Stream::connect(path).await
}

#[cfg(not(unix))]
pub(crate) async fn connect(_path: &Path) -> io::Result<Stream> {
    Err(io::Error::new(io::ErrorKind::Unsupported, UNSUPPORTED))
}

#[cfg(unix)]
pub(crate) fn bind(path: &Path) -> io::Result<Listener> {
    use std::os::unix::fs::PermissionsExt;
    let listener = Listener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

#[cfg(not(unix))]
pub(crate) fn bind(_path: &Path) -> io::Result<Listener> {
    Err(io::Error::new(io::ErrorKind::Unsupported, UNSUPPORTED))
}

/// Waits for the next client that runs as the same user. The socket is
/// already private; checking the peer as well protects against a path
/// that was made reachable by mistake.
#[cfg(unix)]
pub(crate) async fn accept(listener: &Listener) -> io::Result<Stream> {
    loop {
        let (stream, _) = listener.accept().await?;
        let ours = stream
            .peer_cred()
            .is_ok_and(|credentials| credentials.uid() == crate::paths::user_id());
        if ours {
            return Ok(stream);
        }
        tracing::warn!("refused a connection from another user");
    }
}

#[cfg(not(unix))]
pub(crate) async fn accept(listener: &Listener) -> io::Result<Stream> {
    match *listener {}
}
