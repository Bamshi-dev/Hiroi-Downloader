//! SFTP sources, with the `sftp` feature.
//!
//! Every connection of a download is an SSH session with one SFTP
//! channel. SFTP reads at an offset, so every server can be split and
//! resumed; nothing has to be negotiated as with HTTP ranges or FTP's
//! REST.
//!
//! The server's host key must already be in known_hosts
//! ([`EngineConfig::ssh_known_hosts`], `~/.ssh/known_hosts` by default).
//! We never add keys ourselves:
//!
//! - An unknown key is refused, and the message shows its fingerprint,
//!   so the user can compare it with the one the server's owner
//!   publishes and then add it (connecting once with `ssh` does that).
//! - A changed key is refused outright. That is what a machine in the
//!   middle of the connection looks like.
//!
//! The login is the password in the URL, else the keys ssh-agent
//! offers, else the unencrypted keys in `~/.ssh`. Like FTP, SFTP goes
//! through a SOCKS5 proxy when one is set, and is refused with an HTTP
//! proxy rather than connecting around it.
//!
//! [`EngineConfig::ssh_known_hosts`]: crate::EngineConfig::ssh_known_hosts

use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use futures_util::stream;
use parking_lot::Mutex;
use percent_encoding::percent_decode_str;
use reqwest::Url;
use russh::client::{self, AuthResult, Handle};
use russh::keys::agent::client::AgentClient;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh_sftp::client::SftpSession;
use russh_sftp::client::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::constants::WRITE_BUFFER_SIZE;
use crate::error::EngineError;
use crate::net::display_url;
use crate::net::probe::RemoteFile;
use crate::net::protocol::{Body, Want};
use crate::net::retry::{FetchError, FetchResult};
use crate::net::source::Source;
use crate::net::tunnel::Route;
use crate::storage::naming;
use crate::task::EngineShared;

const DEFAULT_PORT: u16 = 22;
/// Key files tried when neither a password nor ssh-agent logs us in.
const KEY_FILES: [&str; 3] = ["id_ed25519", "id_ecdsa", "id_rsa"];

/// An open session: the SSH connection and its SFTP channel. Dropping
/// it closes both.
struct Session {
    _ssh: Handle<HostCheck>,
    sftp: SftpSession,
}

pub(crate) async fn probe(shared: &EngineShared, url: &Url) -> FetchResult<RemoteFile> {
    let path = remote_path(url);
    let session = connect(shared, url).await?;
    let attributes = timed(shared, session.sftp.metadata(&path))
        .await?
        .map_err(|e| FetchError::Protocol(format!("the server cannot read {path}: {e}")))?;
    let size = attributes.size.ok_or_else(|| {
        FetchError::Protocol("the server did not say how large the file is".into())
    })?;
    let _ = timed(shared, session.sftp.close()).await;
    Ok(RemoteFile {
        final_url: url.clone(),
        size: Some(size),
        ranges: true,
        file_name: naming::file_name_from_url(url)
            .and_then(|name| naming::sanitize_file_name(&name)),
        // The modification time and size change when the file does.
        validator: attributes
            .mtime
            .map(|mtime| format!("mtime:{mtime}:{size}")),
        content_type: None,
    })
}

pub(crate) async fn open(shared: &EngineShared, source: &Source, want: Want) -> FetchResult<Body> {
    let url = source.url();
    let path = remote_path(&url);
    let session = connect(shared, &url).await?;
    let start = match want {
        Want::Range { start, .. } => start,
        Want::Whole => 0,
    };
    let opened = async {
        let mut file = session.sftp.open(&path).await?;
        file.seek(SeekFrom::Start(start)).await?;
        Ok::<_, russh_sftp::client::error::Error>(file)
    };
    let file = timed(shared, opened)
        .await?
        .map_err(|e| FetchError::Protocol(format!("the server cannot read {path}: {e}")))?;
    Ok(body(shared, session, file))
}

fn body(shared: &EngineShared, session: Session, file: File) -> Body {
    let timeout = shared.config.read_timeout;
    let state = (session, file, BytesMut::with_capacity(WRITE_BUFFER_SIZE));
    let chunks = stream::unfold(state, move |(session, mut file, mut buffer)| async move {
        buffer.reserve(WRITE_BUFFER_SIZE);
        let read = tokio::time::timeout(timeout, file.read_buf(&mut buffer)).await;
        let item = match read {
            Ok(Ok(0)) => return None,
            Ok(Ok(_)) => Ok::<Bytes, FetchError>(buffer.split().freeze()),
            Ok(Err(error)) => Err(FetchError::Connection(error.to_string())),
            Err(_) => Err(FetchError::Connection("no data for too long".into())),
        };
        Some((item, (session, file, buffer)))
    });
    Box::pin(chunks)
}

async fn connect(shared: &EngineShared, url: &Url) -> FetchResult<Session> {
    let route = Route::from_config(&shared.config, "SFTP")?;
    let host = url
        .host_str()
        .ok_or_else(|| FetchError::Fatal(EngineError::InvalidUrl(display_url(url))))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = url.port().unwrap_or(DEFAULT_PORT);
    let verdict = Arc::new(Mutex::new(None));
    let check = HostCheck {
        host: host.clone(),
        port,
        known_hosts: shared.config.ssh_known_hosts.clone(),
        verdict: Arc::clone(&verdict),
    };
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(shared.config.read_timeout),
        ..client::Config::default()
    });

    let guard = shared.network.guard([url]);
    let stream = route
        .connect(&guard, &host, port, shared.config.connect_timeout)
        .await?;
    let session = async {
        let mut ssh = client::connect_stream(config, stream, check)
            .await
            .map_err(|error| match verdict.lock().take() {
                // The host check failed: that is the reason, not the
                // dropped connection that followed.
                Some(reason) => FetchError::Fatal(EngineError::Protocol(reason)),
                None => FetchError::Connection(error.to_string()),
            })?;
        login(&mut ssh, url).await?;
        let channel = ssh
            .channel_open_session()
            .await
            .map_err(|e| FetchError::Connection(e.to_string()))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| FetchError::Connection(e.to_string()))?;
        let sftp = SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| FetchError::Protocol(format!("SFTP is not available: {e}")))?;
        Ok::<_, FetchError>(Session { _ssh: ssh, sftp })
    };
    timed(shared, session).await?
}

async fn login(ssh: &mut Handle<HostCheck>, url: &Url) -> FetchResult<()> {
    let decode = |text: &str| percent_decode_str(text).decode_utf8_lossy().into_owned();
    let user = match url.username() {
        "" => std::env::var("USER").unwrap_or_else(|_| "anonymous".into()),
        user => decode(user),
    };
    let refused =
        |what: &str| FetchError::Fatal(EngineError::Protocol(format!("login refused: {what}")));
    let failed = |e: russh::Error| FetchError::Connection(e.to_string());

    if let Some(password) = url.password() {
        let result = ssh
            .authenticate_password(user, decode(password))
            .await
            .map_err(failed)?;
        return match result {
            AuthResult::Success => Ok(()),
            _ => Err(refused("wrong user or password")),
        };
    }
    let hash = ssh
        .best_supported_rsa_hash()
        .await
        .map_err(failed)?
        .flatten();
    if let Ok(mut agent) = AgentClient::connect_env().await {
        for identity in agent.request_identities().await.unwrap_or_default() {
            let key = identity.public_key().into_owned();
            let result = ssh
                .authenticate_publickey_with(&user, key, hash, &mut agent)
                .await;
            if result.is_ok_and(|result| result.success()) {
                return Ok(());
            }
        }
    }
    for path in key_files() {
        // Keys with a passphrase are left to ssh-agent.
        let Ok(key) = russh::keys::load_secret_key(&path, None) else {
            continue;
        };
        let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
        if ssh
            .authenticate_publickey(&user, key)
            .await
            .map_err(failed)?
            .success()
        {
            return Ok(());
        }
    }
    Err(refused(
        "no password in the URL, and no key from ssh-agent or ~/.ssh was accepted",
    ))
}

fn key_files() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    KEY_FILES
        .iter()
        .map(|name| home.join(".ssh").join(name))
        .collect()
}

/// The path on the server. As in curl, it is absolute, and `/~/` starts
/// from the login's home folder.
fn remote_path(url: &Url) -> String {
    let path = percent_decode_str(url.path())
        .decode_utf8_lossy()
        .into_owned();
    match path.strip_prefix("/~/") {
        Some(relative) => relative.to_string(),
        None => path,
    }
}

async fn timed<F: Future>(shared: &EngineShared, future: F) -> FetchResult<F::Output> {
    tokio::time::timeout(shared.config.read_timeout, future)
        .await
        .map_err(|_| FetchError::Connection("the SSH server stopped answering".into()))
}

/// Accepts the server only if known_hosts already knows its key. The
/// reason for a refusal is kept for the error message.
struct HostCheck {
    host: String,
    port: u16,
    known_hosts: Option<PathBuf>,
    verdict: Arc<Mutex<Option<String>>>,
}

impl client::Handler for HostCheck {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        offered: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = offered else {
            *self.verdict.lock() = Some("SSH host certificates are not supported".into());
            return Ok(false);
        };
        let known = match &self.known_hosts {
            Some(path) => russh::keys::check_known_hosts_path(&self.host, self.port, key, path),
            None => russh::keys::check_known_hosts(&self.host, self.port, key),
        };
        let fingerprint = key.fingerprint(HashAlg::Sha256);
        let refusal = match known {
            Ok(true) => return Ok(true),
            Ok(false) => format!(
                "unknown SSH host key {fingerprint} for {}; check it with the server's owner, \
                 then add it to known_hosts (for example by connecting once with ssh)",
                self.host
            ),
            Err(russh::keys::Error::KeyChanged { line }) => format!(
                "the SSH host key of {} CHANGED (known_hosts line {line}, now {fingerprint}); \
                 someone may be intercepting the connection",
                self.host
            ),
            Err(error) => format!("could not read known_hosts: {error}"),
        };
        *self.verdict.lock() = Some(refusal);
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_absolute_unless_they_start_at_home() {
        let path = |text: &str| remote_path(&Url::parse(text).unwrap());
        assert_eq!(path("sftp://h/srv/a%20b.iso"), "/srv/a b.iso");
        assert_eq!(path("sftp://h/~/backups/x.tar"), "backups/x.tar");
    }
}
