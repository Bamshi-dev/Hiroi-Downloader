//! FTP downloads from the test server's FTP side.
#![cfg(feature = "ftp")]

use std::time::Duration;

use hiroi_engine::{DownloadRequest, DownloadStatus, Engine, EngineConfig, RetryPolicy};
use hiroi_testserver::{Behavior, FtpBehavior, FtpServer, TestServer, content};

fn engine(config: EngineConfig) -> Engine {
    Engine::new(EngineConfig {
        progress_interval: Duration::from_millis(50),
        // Small test files still get several segments.
        min_split_size: 128 * 1024,
        retry: RetryPolicy {
            base_delay: Duration::from_millis(20),
            ..RetryPolicy::default()
        },
        ..config
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

#[tokio::test]
async fn files_are_split_across_sessions() {
    let server = FtpServer::start(FtpBehavior {
        bytes_per_sec: Some(1_000_000),
        ..FtpBehavior::default()
    })
    .await;
    let data = content(3_000_000, 4);
    server.put("pub/disk.iso", data.clone());
    let dir = tempfile::tempdir().unwrap();

    let request = DownloadRequest::new(&server.url("pub/disk.iso"))
        .unwrap()
        .output_dir(dir.path())
        .connections(4);
    let snapshot = run(&engine(EngineConfig::default()), request).await;

    assert_eq!(
        snapshot.status,
        DownloadStatus::Completed,
        "{:?}",
        snapshot.status
    );
    assert_eq!(std::fs::read(dir.path().join("disk.iso")).unwrap(), data);
    assert!(
        server.peak_sessions() > 2,
        "peak {}",
        server.peak_sessions()
    );
    // Sessions stop when their segment is done instead of sending the
    // rest of the file, so little more than the file goes over the wire.
    assert!(server.bytes_served() < data.len() as u64 * 3 / 2);
}

#[tokio::test]
async fn servers_without_rest_are_read_in_one_go() {
    let server = FtpServer::start(FtpBehavior {
        no_rest: true,
        ..FtpBehavior::default()
    })
    .await;
    let data = content(500_000, 5);
    server.put("one.bin", data.clone());
    let dir = tempfile::tempdir().unwrap();

    let request = DownloadRequest::new(&server.url("one.bin"))
        .unwrap()
        .output_dir(dir.path());
    let snapshot = run(&engine(EngineConfig::default()), request).await;

    assert_eq!(
        snapshot.status,
        DownloadStatus::Completed,
        "{:?}",
        snapshot.status
    );
    assert_eq!(std::fs::read(dir.path().join("one.bin")).unwrap(), data);
    assert_eq!(server.peak_sessions(), 1);
}

#[tokio::test]
async fn busy_servers_get_fewer_connections() {
    let server = FtpServer::start(FtpBehavior {
        bytes_per_sec: Some(2_000_000),
        max_sessions: Some(2),
        ..FtpBehavior::default()
    })
    .await;
    let data = content(2_000_000, 6);
    server.put("busy.bin", data.clone());
    let dir = tempfile::tempdir().unwrap();

    let request = DownloadRequest::new(&server.url("busy.bin"))
        .unwrap()
        .output_dir(dir.path())
        .connections(8);
    let snapshot = run(&engine(EngineConfig::default()), request).await;

    assert_eq!(
        snapshot.status,
        DownloadStatus::Completed,
        "{:?}",
        snapshot.status
    );
    assert_eq!(std::fs::read(dir.path().join("busy.bin")).unwrap(), data);
    assert!(server.refused() > 0, "the server did turn connections away");
}

#[tokio::test]
async fn logins_come_from_the_url_and_wrong_ones_stop() {
    let server = FtpServer::start(FtpBehavior {
        login: Some(("hiro".into(), "s3cret".into())),
        ..FtpBehavior::default()
    })
    .await;
    server.put("private.txt", b"hello".to_vec());
    let dir = tempfile::tempdir().unwrap();
    let with = |login: &str| {
        server
            .url("private.txt")
            .replace("ftp://", &format!("ftp://{login}@"))
    };

    let good = DownloadRequest::new(&with("hiro:s3cret"))
        .unwrap()
        .output_dir(dir.path());
    let snapshot = run(&engine(EngineConfig::default()), good).await;
    assert_eq!(
        snapshot.status,
        DownloadStatus::Completed,
        "{:?}",
        snapshot.status
    );
    assert_eq!(
        std::fs::read(dir.path().join("private.txt")).unwrap(),
        b"hello"
    );

    let bad = DownloadRequest::new(&with("hiro:wrong"))
        .unwrap()
        .output_dir(dir.path().join("bad"));
    let snapshot = run(&engine(EngineConfig::default()), bad).await;
    assert!(
        matches!(&snapshot.status, DownloadStatus::Failed(m) if m.contains("login refused")),
        "{:?}",
        snapshot.status
    );
}

#[tokio::test]
async fn ftp_mirrors_share_a_download_with_http() {
    let ftp = FtpServer::start(FtpBehavior {
        bytes_per_sec: Some(1_500_000),
        ..FtpBehavior::default()
    })
    .await;
    let http = TestServer::start(Behavior {
        bytes_per_sec: Some(1_500_000),
        ..Behavior::default()
    })
    .await;
    let data = content(3_000_000, 8);
    ftp.put("mixed.bin", data.clone());
    http.put("mixed.bin", data.clone());
    let dir = tempfile::tempdir().unwrap();

    let request = DownloadRequest::new(&http.url("mixed.bin"))
        .unwrap()
        .mirror(&ftp.url("mixed.bin"))
        .unwrap()
        .output_dir(dir.path())
        .connections(6);
    let snapshot = run(&engine(EngineConfig::default()), request).await;

    assert_eq!(
        snapshot.status,
        DownloadStatus::Completed,
        "{:?}",
        snapshot.status
    );
    assert_eq!(std::fs::read(dir.path().join("mixed.bin")).unwrap(), data);
    assert!(ftp.bytes_served() > 0 && http.bytes_served() > 0);
}

#[tokio::test]
async fn a_proxy_rules_ftp_out() {
    let server = FtpServer::start(FtpBehavior::default()).await;
    server.put("f.bin", b"x".to_vec());
    let dir = tempfile::tempdir().unwrap();
    let config = EngineConfig {
        proxy: Some("socks5h://127.0.0.1:9".into()),
        ..EngineConfig::default()
    };

    let request = DownloadRequest::new(&server.url("f.bin"))
        .unwrap()
        .output_dir(dir.path());
    let snapshot = run(&engine(config), request).await;

    assert!(
        matches!(&snapshot.status, DownloadStatus::Failed(m) if m.contains("proxy")),
        "{:?}",
        snapshot.status
    );
    assert_eq!(server.peak_sessions(), 0, "nothing reached the server");
}
