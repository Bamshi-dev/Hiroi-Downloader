//! Cookies: the jar for cookies servers set, and the user's own cookies
//! kept to their site.

use std::time::Duration;

use hiroi_engine::{DownloadRequest, DownloadStatus, Engine, EngineConfig};
use hiroi_testserver::{Behavior, TestServer, content};

fn engine(config: EngineConfig) -> Engine {
    Engine::new(EngineConfig {
        progress_interval: Duration::from_millis(50),
        ..config
    })
    .unwrap()
}

async fn run(engine: &Engine, request: DownloadRequest) -> hiroi_engine::DownloadSnapshot {
    let id = engine.add(request);
    tokio::time::timeout(Duration::from_secs(30), engine.wait(id))
        .await
        .expect("timed out");
    engine.snapshot(id).unwrap()
}

/// A download page that sets a session cookie and redirects to a file
/// only that session may fetch.
async fn gated() -> (TestServer, Vec<u8>) {
    let server = TestServer::start(Behavior {
        require_cookie: Some("session=abc".into()),
        ..Behavior::default()
    })
    .await;
    let data = content(300_000, 4);
    server.put("file.bin", data.clone());
    server.redirect_setting_cookie("start", &server.url("file.bin"), "session=abc; Path=/");
    (server, data)
}

#[tokio::test]
async fn cookies_set_on_the_way_are_sent_back() {
    let (server, data) = gated().await;
    let dir = tempfile::tempdir().unwrap();

    let request = DownloadRequest::new(&server.url("start"))
        .unwrap()
        .output_dir(dir.path())
        .file_name("file.bin");
    let snapshot = run(&engine(EngineConfig::default()), request).await;

    assert_eq!(
        snapshot.status,
        DownloadStatus::Completed,
        "{:?}",
        snapshot.status
    );
    assert_eq!(std::fs::read(dir.path().join("file.bin")).unwrap(), data);
}

#[tokio::test]
async fn without_the_jar_such_pages_fail() {
    let (server, _) = gated().await;
    let dir = tempfile::tempdir().unwrap();
    let config = EngineConfig {
        cookie_jar: false,
        ..EngineConfig::default()
    };

    let request = DownloadRequest::new(&server.url("start"))
        .unwrap()
        .output_dir(dir.path());
    let snapshot = run(&engine(config), request).await;

    assert!(
        matches!(snapshot.status, DownloadStatus::Failed(_)),
        "{:?}",
        snapshot.status
    );
}

#[tokio::test]
async fn own_cookies_do_not_reach_other_hosts() {
    let main = TestServer::start(Behavior::default()).await;
    let mirror = TestServer::start(Behavior::default()).await;
    let data = content(3_000_000, 5);
    main.put("f.bin", data.clone());
    mirror.put("f.bin", data.clone());
    // Another host name for the same machine.
    let elsewhere = mirror.url("f.bin").replace("127.0.0.1", "localhost");
    let dir = tempfile::tempdir().unwrap();

    let request = DownloadRequest::new(&main.url("f.bin"))
        .unwrap()
        .mirror(&elsewhere)
        .unwrap()
        .header("Cookie", "login=secret")
        .unwrap()
        .connections(4)
        .output_dir(dir.path());
    let snapshot = run(&engine(EngineConfig::default()), request).await;

    assert_eq!(
        snapshot.status,
        DownloadStatus::Completed,
        "{:?}",
        snapshot.status
    );
    assert!(
        main.received()
            .iter()
            .all(|r| r.headers.get("cookie").is_some_and(|c| c == "login=secret"))
    );
    assert!(!mirror.received().is_empty(), "the mirror was used");
    assert!(
        mirror
            .received()
            .iter()
            .all(|r| r.headers.get("cookie").is_none())
    );
}
