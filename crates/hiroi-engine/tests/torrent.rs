//! Torrents from a local swarm: a librqbit seeder on 127.0.0.1, and the
//! test server answering as the tracker. No DHT, nothing leaves the
//! machine.
#![cfg(feature = "torrent")]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use hiroi_engine::{DownloadRequest, DownloadStatus, Engine, EngineConfig};
use hiroi_testserver::{Behavior, TestServer, content};
use librqbit::spawn_utils::BlockingSpawner;
use librqbit::{
    AddTorrent, AddTorrentOptions, CreateTorrentOptions, ListenerOptions, Session, SessionOptions,
};

/// A seeder for `file`, announced by a tracker on `server`.
struct Swarm {
    torrent: Bytes,
    magnet: String,
    _seeder: Arc<Session>,
}

/// librqbit needs the kernel's IPv6 support, even for an IPv4 swarm.
/// Machines without it skip the swarm tests.
fn ipv6_available() -> bool {
    let available = std::net::UdpSocket::bind("[::]:0").is_ok();
    if !available {
        eprintln!("IPv6 is disabled in this kernel; skipping");
    }
    available
}

async fn swarm(server: &TestServer, file: &Path) -> Swarm {
    let tracker = server.url("announce");
    let created = librqbit::create_torrent(
        file,
        CreateTorrentOptions {
            name: None,
            trackers: vec![tracker],
            piece_length: Some(64 * 1024),
        },
        &BlockingSpawner::new(1),
    )
    .await
    .unwrap();
    let torrent = created.as_bytes().unwrap();
    let magnet = created.as_magnet().to_string();

    let folder = file.parent().unwrap().to_path_buf();
    let seeder = Session::new_with_opts(
        folder.clone(),
        SessionOptions {
            dht: None,
            disable_local_service_discovery: true,
            listen: Some(ListenerOptions {
                listen_addr: "127.0.0.1:0".parse().unwrap(),
                ..ListenerOptions::default()
            }),
            ..SessionOptions::default()
        },
    )
    .await
    .unwrap();
    let handle = seeder
        .add_torrent(
            AddTorrent::from_bytes(torrent.clone()),
            Some(AddTorrentOptions {
                output_folder: Some(folder.to_string_lossy().into_owned()),
                overwrite: true,
                ..AddTorrentOptions::default()
            }),
        )
        .await
        .unwrap()
        .into_handle()
        .unwrap();
    // The seeder checks its copy before it offers pieces.
    tokio::time::timeout(Duration::from_secs(30), async {
        while !handle.stats().finished {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("seeder ready");

    let port = seeder.listen_addr().expect("seeder listens").port();
    server.put_typed("announce", tracker_reply(port), "text/plain");
    Swarm {
        torrent,
        magnet,
        _seeder: seeder,
    }
}

/// A bencoded tracker answer with one peer, 127.0.0.1:`port`, in the
/// compact form (BEP 23).
fn tracker_reply(port: u16) -> Vec<u8> {
    let mut reply = b"d8:intervali60e5:peers6:".to_vec();
    reply.extend_from_slice(&[127, 0, 0, 1]);
    reply.extend_from_slice(&port.to_be_bytes());
    reply.push(b'e');
    reply
}

fn engine(config: EngineConfig) -> Engine {
    Engine::new(EngineConfig {
        progress_interval: Duration::from_millis(50),
        torrent_dht: false,
        ..config
    })
    .unwrap()
}

async fn run(engine: &Engine, request: DownloadRequest) -> hiroi_engine::DownloadSnapshot {
    let id = engine.add(request);
    tokio::time::timeout(Duration::from_secs(90), engine.wait(id))
        .await
        .expect("timed out");
    engine.snapshot(id).unwrap()
}

#[tokio::test]
async fn torrent_files_are_downloaded_from_the_swarm() {
    if !ipv6_available() {
        return;
    }
    let server = TestServer::start(Behavior::default()).await;
    let seed = tempfile::tempdir().unwrap();
    let data = content(1_500_000, 11);
    std::fs::write(seed.path().join("film.mkv"), &data).unwrap();
    let swarm = swarm(&server, &seed.path().join("film.mkv")).await;
    server.put_typed(
        "film.torrent",
        swarm.torrent.to_vec(),
        "application/x-bittorrent",
    );
    let dir = tempfile::tempdir().unwrap();

    let request = DownloadRequest::new(&server.url("film.torrent"))
        .unwrap()
        .output_dir(dir.path());
    let snapshot = run(&engine(EngineConfig::default()), request).await;

    assert_eq!(
        snapshot.status,
        DownloadStatus::Completed,
        "{:?}",
        snapshot.status
    );
    assert_eq!(std::fs::read(dir.path().join("film.mkv")).unwrap(), data);
    assert!(
        !dir.path().join("film.torrent").exists(),
        "the .torrent itself is not kept"
    );
}

#[tokio::test]
async fn magnet_links_fetch_their_metadata_from_peers() {
    if !ipv6_available() {
        return;
    }
    let server = TestServer::start(Behavior::default()).await;
    let seed = tempfile::tempdir().unwrap();
    let data = content(700_000, 12);
    std::fs::write(seed.path().join("notes.pdf"), &data).unwrap();
    let swarm = swarm(&server, &seed.path().join("notes.pdf")).await;
    let dir = tempfile::tempdir().unwrap();

    let request = DownloadRequest::new(&swarm.magnet)
        .unwrap()
        .output_dir(dir.path());
    let snapshot = run(&engine(EngineConfig::default()), request).await;

    assert_eq!(
        snapshot.status,
        DownloadStatus::Completed,
        "{:?}",
        snapshot.status
    );
    assert_eq!(std::fs::read(dir.path().join("notes.pdf")).unwrap(), data);
}

#[tokio::test]
async fn a_proxy_rules_torrents_out() {
    let dir = tempfile::tempdir().unwrap();
    let config = EngineConfig {
        proxy: Some("socks5h://127.0.0.1:9".into()),
        ..EngineConfig::default()
    };
    let magnet = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567";

    let request = DownloadRequest::new(magnet).unwrap().output_dir(dir.path());
    let snapshot = run(&engine(config), request).await;

    assert!(
        matches!(&snapshot.status, DownloadStatus::Failed(m) if m.contains("proxy")),
        "{:?}",
        snapshot.status
    );
}
