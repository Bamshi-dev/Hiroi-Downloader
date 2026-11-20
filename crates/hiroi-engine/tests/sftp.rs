//! SFTP downloads from an SSH server that runs inside the test.
#![cfg(feature = "sftp")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use hiroi_engine::{DownloadRequest, DownloadStatus, Engine, EngineConfig, RetryPolicy};
use hiroi_testserver::content;
use russh::keys::PrivateKey;
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::server::{Auth, Msg, Server as _, Session};
use russh::{Channel, ChannelId};
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version,
};
use tokio::net::TcpListener;

const USER: &str = "hiro";
const PASSWORD: &str = "s3cret";

/// A host key from a fixed seed: tests need no randomness, and no
/// private key is stored in the repository.
fn host_key(seed: u8) -> PrivateKey {
    PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32]))
}

#[derive(Clone)]
struct SshServer {
    files: Arc<HashMap<String, Vec<u8>>>,
    sessions: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl SshServer {
    /// Serves `files` and returns the port.
    async fn start(files: HashMap<String, Vec<u8>>) -> (Self, u16) {
        let server = Self {
            files: Arc::new(files),
            sessions: Arc::default(),
            peak: Arc::default(),
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = Arc::new(russh::server::Config {
            keys: vec![host_key(7)],
            auth_rejection_time: Duration::from_millis(10),
            auth_rejection_time_initial: Some(Duration::ZERO),
            ..russh::server::Config::default()
        });
        let mut running = server.clone();
        tokio::spawn(async move {
            let _ = running.run_on_socket(config, &listener).await;
        });
        (server, port)
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }
}

impl russh::server::Server for SshServer {
    type Handler = Connection;

    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Connection {
        let open = self.sessions.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak.fetch_max(open, Ordering::Relaxed);
        Connection {
            server: self.clone(),
            channels: HashMap::new(),
        }
    }
}

struct Connection {
    server: SshServer,
    channels: HashMap<ChannelId, Channel<Msg>>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.server.sessions.fetch_sub(1, Ordering::Relaxed);
    }
}

impl russh::server::Handler for Connection {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if user == USER && password == PASSWORD {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        match (name, self.channels.remove(&id)) {
            ("sftp", Some(channel)) => {
                session.channel_success(id)?;
                let files = Arc::clone(&self.server.files);
                russh_sftp::server::run(channel.into_stream(), Sftp { files }).await;
            }
            _ => session.channel_failure(id)?,
        }
        Ok(())
    }
}

struct Sftp {
    files: Arc<HashMap<String, Vec<u8>>>,
}

impl Sftp {
    fn attributes(&self, path: &str) -> Result<FileAttributes, StatusCode> {
        let data = self.files.get(path).ok_or(StatusCode::NoSuchFile)?;
        Ok(FileAttributes {
            size: Some(data.len() as u64),
            mtime: Some(1_767_225_600),
            ..FileAttributes::default()
        })
    }
}

impl russh_sftp::server::Handler for Sftp {
    type Error = StatusCode;

    fn unimplemented(&self) -> StatusCode {
        StatusCode::OpUnsupported
    }

    async fn init(&mut self, _: u32, _: HashMap<String, String>) -> Result<Version, StatusCode> {
        Ok(Version::new())
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, StatusCode> {
        Ok(Name {
            id,
            files: vec![File::dummy(path)],
        })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, StatusCode> {
        let attrs = self.attributes(&path)?;
        Ok(Attrs { id, attrs })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, StatusCode> {
        self.stat(id, path).await
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, StatusCode> {
        self.stat(id, handle).await
    }

    async fn open(
        &mut self,
        id: u32,
        path: String,
        _: OpenFlags,
        _: FileAttributes,
    ) -> Result<Handle, StatusCode> {
        self.attributes(&path)?;
        Ok(Handle { id, handle: path })
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, StatusCode> {
        let data = self.files.get(&handle).ok_or(StatusCode::NoSuchFile)?;
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        if start >= data.len() {
            return Err(StatusCode::Eof);
        }
        // Slow enough that splitting the file pays off.
        tokio::time::sleep(Duration::from_millis(2)).await;
        let end = data.len().min(start + len as usize);
        Ok(Data {
            id,
            data: data[start..end].to_vec(),
        })
    }

    async fn close(&mut self, id: u32, _: String) -> Result<Status, StatusCode> {
        Ok(Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".into(),
            language_tag: "en-US".into(),
        })
    }
}

/// A known_hosts file that lists `key` for the test server.
fn known_hosts(dir: &std::path::Path, port: u16, key: &PrivateKey) -> PathBuf {
    let path = dir.join("known_hosts");
    let public = key.public_key().to_openssh().unwrap();
    std::fs::write(&path, format!("[127.0.0.1]:{port} {public}\n")).unwrap();
    path
}

fn engine(known_hosts: PathBuf) -> Engine {
    Engine::new(EngineConfig {
        progress_interval: Duration::from_millis(50),
        min_split_size: 128 * 1024,
        ssh_known_hosts: Some(known_hosts),
        retry: RetryPolicy {
            base_delay: Duration::from_millis(20),
            ..RetryPolicy::default()
        },
        ..EngineConfig::default()
    })
    .unwrap()
}

async fn run(engine: &Engine, request: DownloadRequest) -> hiroi_engine::DownloadSnapshot {
    let id = engine.add(request);
    tokio::time::timeout(Duration::from_secs(60), engine.wait(id))
        .await
        .expect("timed out");
    engine.snapshot(id).unwrap()
}

fn url(port: u16, login: &str, path: &str) -> String {
    format!("sftp://{login}@127.0.0.1:{port}{path}")
}

#[tokio::test]
async fn files_are_split_across_sessions() {
    let data = content(2_000_000, 9);
    let (server, port) =
        SshServer::start(HashMap::from([("/srv/disk.iso".into(), data.clone())])).await;
    let dir = tempfile::tempdir().unwrap();
    let hosts = known_hosts(dir.path(), port, &host_key(7));

    let request = DownloadRequest::new(&url(port, "hiro:s3cret", "/srv/disk.iso"))
        .unwrap()
        .output_dir(dir.path())
        .connections(4);
    let snapshot = run(&engine(hosts), request).await;

    assert_eq!(
        snapshot.status,
        DownloadStatus::Completed,
        "{:?}",
        snapshot.status
    );
    assert_eq!(std::fs::read(dir.path().join("disk.iso")).unwrap(), data);
    assert!(server.peak() > 1, "peak {}", server.peak());
}

#[tokio::test]
async fn unknown_hosts_are_refused_with_their_fingerprint() {
    let (_server, port) = SshServer::start(HashMap::from([("/f".into(), vec![1])])).await;
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty_known_hosts");
    std::fs::write(&empty, "").unwrap();

    let request = DownloadRequest::new(&url(port, "hiro:s3cret", "/f"))
        .unwrap()
        .output_dir(dir.path());
    let snapshot = run(&engine(empty), request).await;

    assert!(
        matches!(&snapshot.status, DownloadStatus::Failed(m) if m.contains("unknown SSH host key SHA256:")),
        "{:?}",
        snapshot.status
    );
}

#[tokio::test]
async fn changed_host_keys_are_refused() {
    let (_server, port) = SshServer::start(HashMap::from([("/f".into(), vec![1])])).await;
    let dir = tempfile::tempdir().unwrap();
    let hosts = known_hosts(dir.path(), port, &host_key(8));

    let request = DownloadRequest::new(&url(port, "hiro:s3cret", "/f"))
        .unwrap()
        .output_dir(dir.path());
    let snapshot = run(&engine(hosts), request).await;

    assert!(
        matches!(&snapshot.status, DownloadStatus::Failed(m) if m.contains("CHANGED")),
        "{:?}",
        snapshot.status
    );
}

#[tokio::test]
async fn wrong_passwords_stop_the_download() {
    let (_server, port) = SshServer::start(HashMap::from([("/f".into(), vec![1])])).await;
    let dir = tempfile::tempdir().unwrap();
    let hosts = known_hosts(dir.path(), port, &host_key(7));

    let request = DownloadRequest::new(&url(port, "hiro:wrong", "/f"))
        .unwrap()
        .output_dir(dir.path());
    let snapshot = run(&engine(hosts), request).await;

    assert!(
        matches!(&snapshot.status, DownloadStatus::Failed(m) if m.contains("login refused")),
        "{:?}",
        snapshot.status
    );
}

#[tokio::test]
async fn socks_proxies_carry_sftp() {
    let data = content(300_000, 10);
    let (_server, port) = SshServer::start(HashMap::from([("/f.bin".into(), data.clone())])).await;
    let proxy = hiroi_testserver::SocksProxy::start().await;
    let dir = tempfile::tempdir().unwrap();
    let hosts = dir.path().join("known_hosts");
    let public = host_key(7).public_key().to_openssh().unwrap();
    std::fs::write(&hosts, format!("[localhost]:{port} {public}\n")).unwrap();
    let engine = Engine::new(EngineConfig {
        progress_interval: Duration::from_millis(50),
        ssh_known_hosts: Some(hosts),
        proxy: Some(proxy.url(true)),
        ..EngineConfig::default()
    })
    .unwrap();

    let url = format!("sftp://hiro:s3cret@localhost:{port}/f.bin");
    let request = DownloadRequest::new(&url).unwrap().output_dir(dir.path());
    let snapshot = run(&engine, request).await;

    assert_eq!(
        snapshot.status,
        DownloadStatus::Completed,
        "{:?}",
        snapshot.status
    );
    assert_eq!(std::fs::read(dir.path().join("f.bin")).unwrap(), data);
    assert!(
        proxy
            .targets()
            .iter()
            .all(|t| *t == format!("localhost:{port}"))
    );
    assert!(!proxy.targets().is_empty());
}
