//! The service side of the local socket: one task per client, JSON
//! lines in both directions.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use hiroi_proto::{Event, MAX_LINE_BYTES, Outcome, Reply, Request, RequestEnvelope, ServerMessage};
use tokio::sync::broadcast;
use tokio_util::codec::{Framed, LinesCodec, LinesCodecError};
use tokio_util::sync::CancellationToken;

use crate::service::{DaemonError, Service};
use crate::transport::{self, Listener, Stream};

/// Takes the single-instance lock. It is released when the file closes,
/// so a crashed service never leaves a stale lock behind.
pub(crate) fn lock(path: &Path) -> Result<File, DaemonError> {
    let io = |source| DaemonError::Io {
        path: path.to_path_buf(),
        source,
    };
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(io)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(DaemonError::AlreadyRunning(path.to_path_buf())),
        Err(TryLockError::Error(error)) => Err(io(error)),
    }
}

/// Binds the socket. We hold the lock, so an existing socket file is a
/// leftover of a crash and can go.
pub(crate) fn bind(path: &Path) -> Result<Listener, DaemonError> {
    let io = |source: std::io::Error| match source.kind() {
        std::io::ErrorKind::Unsupported => DaemonError::Unsupported,
        _ => DaemonError::Io {
            path: path.to_path_buf(),
            source,
        },
    };
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(io(error)),
        _ => {}
    }
    transport::bind(path).map_err(io)
}

pub(crate) async fn serve(listener: Listener, service: Arc<Service>, stop: CancellationToken) {
    loop {
        let stream = tokio::select! {
            () = stop.cancelled() => return,
            accepted = transport::accept(&listener) => match accepted {
                Ok(stream) => stream,
                Err(error) => {
                    tracing::warn!(%error, "could not accept a connection");
                    continue;
                }
            },
        };
        let service = Arc::clone(&service);
        let stop = stop.clone();
        tokio::spawn(async move { connection(stream, service, stop).await });
    }
}

async fn connection(stream: Stream, service: Arc<Service>, stop: CancellationToken) {
    let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(MAX_LINE_BYTES));
    let mut events: Option<broadcast::Receiver<Event>> = None;
    loop {
        let message = tokio::select! {
            () = stop.cancelled() => return,
            line = framed.next() => match line {
                None => return,
                Some(Err(LinesCodecError::MaxLineLengthExceeded)) => {
                    let _ = send(&mut framed, &reply(0, Err("line too long".into()))).await;
                    return;
                }
                Some(Err(LinesCodecError::Io(_))) => return,
                Some(Ok(line)) => answer(&service, &line, &mut events).await,
            },
            event = next_event(&mut events) => match event {
                Some(event) => ServerMessage::Event(event),
                None => continue,
            },
        };
        if send(&mut framed, &message).await.is_err() {
            return;
        }
    }
}

async fn answer(
    service: &Service,
    line: &str,
    events: &mut Option<broadcast::Receiver<Event>>,
) -> ServerMessage {
    let envelope: RequestEnvelope = match serde_json::from_str(line) {
        Ok(envelope) => envelope,
        Err(error) => return reply(0, Err(format!("invalid request: {error}"))),
    };
    if matches!(envelope.request, Request::Subscribe) {
        *events = Some(service.subscribe());
    }
    reply(envelope.id, service.handle(envelope.request).await)
}

/// Waits for the next event; never resolves without a subscription.
async fn next_event(events: &mut Option<broadcast::Receiver<Event>>) -> Option<Event> {
    let Some(receiver) = events else {
        return std::future::pending().await;
    };
    match receiver.recv().await {
        Ok(event) => Some(event),
        // A slow client skips what it missed; the next job event catches it up.
        Err(broadcast::error::RecvError::Lagged(_)) => None,
        Err(broadcast::error::RecvError::Closed) => std::future::pending().await,
    }
}

fn reply(id: u64, result: Result<hiroi_proto::Response, String>) -> ServerMessage {
    let outcome = match result {
        Ok(response) => Outcome::Ok(response),
        Err(message) => Outcome::Error(message),
    };
    ServerMessage::Reply(Reply { id, outcome })
}

async fn send(
    framed: &mut Framed<Stream, LinesCodec>,
    message: &ServerMessage,
) -> Result<(), LinesCodecError> {
    let line = serde_json::to_string(message).expect("messages always serialize");
    framed.send(line).await
}
