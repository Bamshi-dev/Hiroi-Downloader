//! FTP and FTPS sources, with the `ftp` feature.
//!
//! Every connection of a download is an FTP session of its own: log in,
//! switch to binary, `REST` to where the segment starts, `RETR`. When the
//! segment is done we close the session instead of reading the rest of
//! the file, as other download managers do; servers are used to it.
//!
//! A few choices keep this safe:
//!
//! - Data connections use `EPSV`, whose reply carries only a port. The
//!   older `PASV` reply carries an address too, and a hostile server can
//!   use it to make us connect to machines on our own network.
//! - `ftps://` is explicit TLS (`AUTH TLS`, then `PROT P` for the data),
//!   with the certificate checked against the system's trust store, as
//!   for HTTPS. Plain `ftp://` sends the password and the file in clear
//!   text, and we say so in the log.
//! - FTP does not go through the HTTP or SOCKS proxy, so with a proxy
//!   set we refuse it instead of connecting directly and exposing the
//!   user's address.
//! - Paths and credentials are checked for line breaks, which would let
//!   a URL smuggle extra commands into the session.

use std::sync::{Arc, OnceLock};

use bytes::{Bytes, BytesMut};
use futures_util::stream;
use percent_encoding::percent_decode_str;
use reqwest::Url;
use suppaftp::tokio::{AsyncRustlsConnector, AsyncRustlsFtpStream, TransferStream};
use suppaftp::types::FileType;
use suppaftp::{FtpError, Mode};
use tokio::io::AsyncReadExt;
use tokio_rustls::rustls::ClientConfig;

use crate::constants::{DEFAULT_THROTTLE_PAUSE, WRITE_BUFFER_SIZE};
use crate::error::EngineError;
use crate::limit::HostGate;
use crate::net::probe::RemoteFile;
use crate::net::protocol::{Body, Want};
use crate::net::retry::{FetchError, FetchResult};
use crate::net::source::Source;
use crate::net::{display_url, host_key};
use crate::storage::naming;
use crate::task::EngineShared;

type Session = AsyncRustlsFtpStream;
type Transfer = TransferStream<suppaftp::tokio::AsyncRustlsStream>;

const DEFAULT_PORT: u16 = 21;

pub(crate) async fn probe(shared: &EngineShared, url: &Url) -> FetchResult<RemoteFile> {
    let gate = shared.hosts.gate(&host_key(url));
    let mut session = connect(shared, url, &gate).await?;
    let path = remote_path(url)?;
    let ask = async {
        // Without REST in FEAT, a file can only be fetched in one go.
        let ranges = session
            .feat()
            .await
            .map(|features| {
                features
                    .keys()
                    .any(|name| name.eq_ignore_ascii_case("REST"))
            })
            .unwrap_or(false);
        let size = session
            .size(&path)
            .await
            .map_err(|e| reply_error(e, &gate))?;
        // MDTM changes when the file does, so it serves as a validator.
        let validator = session
            .mdtm(&path)
            .await
            .ok()
            .map(|time| format!("mdtm:{time}"));
        Ok::<_, FetchError>((ranges, size, validator))
    };
    let (ranges, size, validator) = timed(shared, ask).await??;
    let _ = timed(shared, session.quit()).await;
    Ok(RemoteFile {
        final_url: url.clone(),
        size: Some(size as u64),
        ranges,
        file_name: naming::file_name_from_url(url)
            .and_then(|name| naming::sanitize_file_name(&name)),
        validator,
        content_type: None,
    })
}

pub(crate) async fn open(
    shared: &EngineShared,
    source: &Source,
    gate: &HostGate,
    want: Want,
) -> FetchResult<Body> {
    let url = source.url();
    let path = remote_path(&url)?;
    let mut session = connect(shared, &url, gate).await?;
    let start = match want {
        Want::Range { start, .. } => start,
        Want::Whole => 0,
    };
    let retrieve = async {
        if start > 0 {
            let offset = usize::try_from(start)
                .map_err(|_| FetchError::Protocol("offset too large for this system".into()))?;
            session
                .resume_transfer(offset)
                .await
                .map_err(|e| reply_error(e, gate))?;
        }
        session
            .retr_as_stream(&path)
            .await
            .map_err(|e| reply_error(e, gate))
    };
    let transfer = timed(shared, retrieve).await??;
    Ok(body(shared, session, transfer))
}

/// Streams the data connection. The session travels with it, since
/// closing the control connection would end the transfer.
fn body(shared: &EngineShared, session: Session, transfer: Transfer) -> Body {
    let timeout = shared.config.read_timeout;
    let state = (
        session,
        transfer,
        BytesMut::with_capacity(WRITE_BUFFER_SIZE),
    );
    let chunks = stream::unfold(
        state,
        move |(session, mut transfer, mut buffer)| async move {
            buffer.reserve(WRITE_BUFFER_SIZE);
            match tokio::time::timeout(timeout, transfer.read_buf(&mut buffer)).await {
                Ok(Ok(0)) => None,
                Ok(Ok(_)) => {
                    let chunk: Bytes = buffer.split().freeze();
                    Some((Ok(chunk), (session, transfer, buffer)))
                }
                Ok(Err(error)) => Some((
                    Err(FetchError::Connection(error.to_string())),
                    (session, transfer, buffer),
                )),
                Err(_) => Some((
                    Err(FetchError::Connection("no data for too long".into())),
                    (session, transfer, buffer),
                )),
            }
        },
    );
    Box::pin(chunks)
}

/// Opens a logged-in session in binary mode.
async fn connect(shared: &EngineShared, url: &Url, gate: &HostGate) -> FetchResult<Session> {
    if shared.config.proxy.is_some() {
        return Err(FetchError::Fatal(EngineError::Protocol(
            "FTP cannot go through the proxy; refusing to connect directly".into(),
        )));
    }
    let host = url
        .host_str()
        .ok_or_else(|| FetchError::Fatal(EngineError::InvalidUrl(display_url(url))))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = url.port().unwrap_or(DEFAULT_PORT);
    let (user, password) = credentials(url)?;
    let secure = url.scheme() == "ftps";
    if !secure {
        tracing::debug!(url = %display_url(url), "plain FTP: the password and the data are not encrypted");
    }

    let address = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| FetchError::Connection(format!("could not resolve the server: {e}")))?
        .next()
        .ok_or_else(|| FetchError::Connection("the server has no address".into()))?;
    let session = async {
        let mut session = Session::connect_timeout(address, shared.config.connect_timeout)
            .await
            .map_err(|e| reply_error(e, gate))?;
        session.set_mode(Mode::ExtendedPassive);
        if secure {
            session = session
                .into_secure(AsyncRustlsConnector::from(tls()?), &host)
                .await
                .map_err(|e| reply_error(e, gate))?;
        }
        session
            .login(&user, &password)
            .await
            .map_err(|e| reply_error(e, gate))?;
        session
            .transfer_type(FileType::Binary)
            .await
            .map_err(|e| reply_error(e, gate))?;
        Ok::<_, FetchError>(session)
    };
    timed(shared, session).await?
}

/// Commands on the control connection get the read timeout, so a server
/// that stops answering cannot hold a connection forever.
async fn timed<F: Future>(shared: &EngineShared, future: F) -> FetchResult<F::Output> {
    tokio::time::timeout(shared.config.read_timeout, future)
        .await
        .map_err(|_| FetchError::Connection("the FTP server stopped answering".into()))
}

fn tls() -> FetchResult<tokio_rustls::TlsConnector> {
    static CONFIG: OnceLock<Result<Arc<ClientConfig>, String>> = OnceLock::new();
    let config = CONFIG.get_or_init(|| {
        use rustls_platform_verifier::BuilderVerifierExt;
        let provider = Arc::new(tokio_rustls::rustls::crypto::aws_lc_rs::default_provider());
        ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .and_then(|builder| builder.with_platform_verifier())
            .map(|builder| Arc::new(builder.with_no_client_auth()))
            .map_err(|e| e.to_string())
    });
    match config {
        Ok(config) => Ok(tokio_rustls::TlsConnector::from(Arc::clone(config))),
        Err(error) => Err(FetchError::Fatal(EngineError::Protocol(format!(
            "TLS setup failed: {error}"
        )))),
    }
}

/// The path to `RETR`, as RFC 1738 reads it: relative to the login
/// folder, and absolute when it starts with `%2F`.
fn remote_path(url: &Url) -> FetchResult<String> {
    let encoded = url.path().strip_prefix('/').unwrap_or(url.path());
    let path = percent_decode_str(encoded).decode_utf8_lossy().into_owned();
    if path.is_empty() || path.ends_with('/') {
        return Err(FetchError::Fatal(EngineError::Protocol(
            "the URL names a folder, not a file".into(),
        )));
    }
    no_line_breaks(path)
}

/// User and password from the URL, or the anonymous login.
fn credentials(url: &Url) -> FetchResult<(String, String)> {
    let decode = |text: &str| percent_decode_str(text).decode_utf8_lossy().into_owned();
    let user = decode(url.username());
    if user.is_empty() {
        return Ok(("anonymous".into(), "anonymous@".into()));
    }
    let password = decode(url.password().unwrap_or(""));
    Ok((no_line_breaks(user)?, no_line_breaks(password)?))
}

fn no_line_breaks(text: String) -> FetchResult<String> {
    if text.contains(['\r', '\n']) {
        return Err(FetchError::Fatal(EngineError::Protocol(
            "line breaks are not allowed in FTP paths or logins".into(),
        )));
    }
    Ok(text)
}

/// Sorts FTP failures the way HTTP ones are sorted: 421 means the server
/// is busy (slow down and retry), other 4xx replies are temporary, a
/// refused login stops the download, and other 5xx replies drop the
/// source.
fn reply_error(error: FtpError, gate: &HostGate) -> FetchError {
    match error {
        FtpError::UnexpectedResponse(response) => {
            let code = response.status.code();
            let text = String::from_utf8_lossy(&response.body).trim().to_string();
            match code {
                421 => {
                    gate.throttle(DEFAULT_THROTTLE_PAUSE);
                    FetchError::Connection(format!("server is busy: {text}"))
                }
                530 => FetchError::Fatal(EngineError::Protocol(format!("login refused: {text}"))),
                400..=499 => FetchError::Connection(format!("server answered {code}: {text}")),
                _ => FetchError::Protocol(format!("server answered {code}: {text}")),
            }
        }
        FtpError::ConnectionError(error) => FetchError::Connection(error.to_string()),
        FtpError::SecureError(error) => {
            FetchError::Fatal(EngineError::Protocol(format!("TLS: {error}")))
        }
        other => FetchError::Protocol(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn paths_follow_rfc_1738() {
        assert_eq!(
            remote_path(&url("ftp://h/pub/a%20b.iso")).unwrap(),
            "pub/a b.iso"
        );
        assert_eq!(remote_path(&url("ftp://h/%2Fetc/x")).unwrap(), "/etc/x");
        assert!(remote_path(&url("ftp://h/pub/")).is_err());
        assert!(remote_path(&url("ftp://h/a%0D%0ADELE%20b")).is_err());
    }

    #[test]
    fn logins_default_to_anonymous() {
        assert_eq!(credentials(&url("ftp://h/f")).unwrap().0, "anonymous");
        let (user, password) = credentials(&url("ftp://me:p%40ss@h/f")).unwrap();
        assert_eq!((user.as_str(), password.as_str()), ("me", "p@ss"));
        assert!(credentials(&url("ftp://me:a%0Ab@h/f")).is_err());
    }
}
