//! Client side of the service protocol, used by the command line and
//! the browser's native-messaging host.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use futures_util::{SinkExt, StreamExt};
use hiroi_proto::{
    Event, MAX_LINE_BYTES, Outcome, Request, RequestEnvelope, Response, ServerMessage,
};
use tokio_util::codec::{Framed, LinesCodec, LinesCodecError};

use crate::transport::{self, Stream};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("the Hiroi service is not running (no socket at {}); start it with `hiroi daemon`", .0.display())]
    NotRunning(PathBuf),
    #[error("{}", transport::UNSUPPORTED)]
    Unsupported,
    #[error("connection to the service failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("the service sent something unexpected: {0}")]
    Protocol(String),
    #[error("{0}")]
    Server(String),
}

#[derive(Debug)]
pub struct Client {
    framed: Framed<Stream, LinesCodec>,
    next_id: u64,
    /// Events that arrived while waiting for a reply.
    pending: VecDeque<Event>,
}

impl Client {
    pub async fn connect(socket: &Path) -> Result<Self, ClientError> {
        let stream = transport::connect(socket)
            .await
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                    ClientError::NotRunning(socket.to_path_buf())
                }
                std::io::ErrorKind::Unsupported => ClientError::Unsupported,
                _ => ClientError::Io(error),
            })?;
        let framed = Framed::new(stream, LinesCodec::new_with_max_length(MAX_LINE_BYTES));
        Ok(Self {
            framed,
            next_id: 1,
            pending: VecDeque::new(),
        })
    }

    /// Sends a request and waits for its reply.
    pub async fn call(&mut self, request: Request) -> Result<Response, ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        let line = serde_json::to_string(&RequestEnvelope { id, request })
            .expect("requests always serialize");
        self.framed.send(line).await.map_err(codec_error)?;
        loop {
            match self.read().await? {
                ServerMessage::Reply(reply) if reply.id == id => {
                    return match reply.outcome {
                        Outcome::Ok(response) => Ok(response),
                        Outcome::Error(message) => Err(ClientError::Server(message)),
                    };
                }
                ServerMessage::Reply(reply) => {
                    return Err(ClientError::Protocol(format!(
                        "reply for request {}",
                        reply.id
                    )));
                }
                ServerMessage::Event(event) => self.pending.push_back(event),
            }
        }
    }

    /// The next event after [`Request::Subscribe`]; `None` when the
    /// service closed the connection.
    pub async fn next_event(&mut self) -> Result<Option<Event>, ClientError> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(Some(event));
        }
        loop {
            match self.framed.next().await {
                None => return Ok(None),
                Some(line) => match parse(&line.map_err(codec_error)?)? {
                    ServerMessage::Event(event) => return Ok(Some(event)),
                    ServerMessage::Reply(_) => continue,
                },
            }
        }
    }

    async fn read(&mut self) -> Result<ServerMessage, ClientError> {
        match self.framed.next().await {
            None => Err(ClientError::Protocol(
                "the service closed the connection".into(),
            )),
            Some(line) => parse(&line.map_err(codec_error)?),
        }
    }
}

fn parse(line: &str) -> Result<ServerMessage, ClientError> {
    serde_json::from_str(line).map_err(|error| ClientError::Protocol(error.to_string()))
}

fn codec_error(error: LinesCodecError) -> ClientError {
    match error {
        LinesCodecError::Io(error) => ClientError::Io(error),
        LinesCodecError::MaxLineLengthExceeded => ClientError::Protocol("line too long".into()),
    }
}
