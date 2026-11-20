//! Network settings: the IP filter, interface and address binding,
//! redirects into the local network, and anonymous mode.

use std::time::Duration;

use hiroi_engine::{DownloadRequest, DownloadSnapshot, DownloadStatus, Engine, EngineConfig};
use hiroi_testserver::{Behavior, TestServer, content};

fn engine(config: EngineConfig) -> Engine {
    Engine::new(EngineConfig {
        progress_interval: Duration::from_millis(50),
        retry: hiroi_engine::RetryPolicy {
            max_attempts: 2,
            base_delay: Duration::from_millis(10),
            ..Default::default()
        },
        ..config
    })
    .unwrap()
}

async fn served() -> (TestServer, Vec<u8>) {
    let server = TestServer::start(Behavior::default()).await;
    let data = content(200_000, 9);
    server.put("file.bin", data.clone());
    (server, data)
}

async fn fetch(config: EngineConfig, url: &str) -> (DownloadSnapshot, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let request = DownloadRequest::new(url)
        .unwrap()
        .output_dir(dir.path())
        .file_name("file.bin");
    let engine = engine(config);
    let id = engine.add(request);
    tokio::time::timeout(Duration::from_secs(30), engine.wait(id))
        .await
        .expect("timed out");
    (engine.snapshot(id).unwrap(), dir)
}

fn failure(snapshot: &DownloadSnapshot) -> &str {
    match &snapshot.status {
        DownloadStatus::Failed(reason) => reason,
        other => panic!("expected a failure, got {other:?}"),
    }
}

fn filter_file(text: &str) -> tempfile::NamedTempFile {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), text).unwrap();
    file
}

#[tokio::test]
async fn the_ip_filter_keeps_downloads_away() {
    let (server, _) = served().await;
    let filter = filter_file("# loopback\n127.0.0.0/8\n");
    let config = EngineConfig {
        ip_filter: Some(filter.path().into()),
        ..EngineConfig::default()
    };
    let (snapshot, _dir) = fetch(config, &server.url("file.bin")).await;

    assert!(failure(&snapshot).contains("IP filter"), "{snapshot:?}");
    assert_eq!(server.requests(), 0);
}

#[tokio::test]
async fn names_are_filtered_by_their_addresses() {
    let (server, _) = served().await;
    let filter = filter_file("Loopback:127.0.0.0-127.255.255.255\n::1\n");
    let config = EngineConfig {
        ip_filter: Some(filter.path().into()),
        ..EngineConfig::default()
    };
    let url = server.url("file.bin").replace("127.0.0.1", "localhost");
    let (snapshot, _dir) = fetch(config, &url).await;

    assert!(failure(&snapshot).contains("IP filter"), "{snapshot:?}");
    assert_eq!(server.requests(), 0);
}

#[tokio::test]
async fn other_addresses_pass_the_filter() {
    let (server, data) = served().await;
    let filter = filter_file("192.0.2.0/24\n");
    let config = EngineConfig {
        ip_filter: Some(filter.path().into()),
        ..EngineConfig::default()
    };
    let (snapshot, dir) = fetch(config, &server.url("file.bin")).await;

    assert_eq!(snapshot.status, DownloadStatus::Completed);
    assert_eq!(std::fs::read(dir.path().join("file.bin")).unwrap(), data);
}

#[test]
fn an_unreadable_filter_stops_the_engine() {
    let filter = filter_file("nothing here\n");
    let config = EngineConfig {
        ip_filter: Some(filter.path().into()),
        ..EngineConfig::default()
    };
    let Err(error) = Engine::new(config) else {
        panic!("a filter that blocks nothing was accepted");
    };
    assert!(error.to_string().contains("blocks nothing"), "{error}");
}

#[tokio::test]
async fn downloads_use_the_bound_interface() {
    let (server, data) = served().await;
    let Some(loopback) = hiroi_engine::interfaces()
        .into_iter()
        .find(|i| i.loopback && i.address.is_ipv4())
    else {
        eprintln!("no IPv4 loopback interface; skipping");
        return;
    };
    let config = EngineConfig {
        interface: Some(loopback.name),
        ..EngineConfig::default()
    };
    let (snapshot, dir) = fetch(config, &server.url("file.bin")).await;

    assert_eq!(snapshot.status, DownloadStatus::Completed, "{snapshot:?}");
    assert_eq!(std::fs::read(dir.path().join("file.bin")).unwrap(), data);
}

#[tokio::test]
async fn a_missing_interface_stops_downloads() {
    let (server, _) = served().await;
    let config = EngineConfig {
        interface: Some("hiroi-missing0".into()),
        ..EngineConfig::default()
    };
    let (snapshot, _dir) = fetch(config, &server.url("file.bin")).await;

    failure(&snapshot);
    assert_eq!(server.requests(), 0);
}

#[tokio::test]
async fn downloads_use_the_bound_address() {
    let (server, data) = served().await;
    let config = EngineConfig {
        bind_address: Some("127.0.0.1".parse().unwrap()),
        ..EngineConfig::default()
    };
    let (snapshot, dir) = fetch(config, &server.url("file.bin")).await;

    assert_eq!(snapshot.status, DownloadStatus::Completed, "{snapshot:?}");
    assert_eq!(std::fs::read(dir.path().join("file.bin")).unwrap(), data);
}

#[tokio::test]
async fn downloads_that_start_locally_may_redirect_locally() {
    let (server, data) = served().await;
    let target = server.url("file.bin").replace("127.0.0.1", "localhost");
    server.redirect("start", &target);
    let (snapshot, dir) = fetch(EngineConfig::default(), &server.url("start")).await;

    assert_eq!(snapshot.status, DownloadStatus::Completed, "{snapshot:?}");
    assert_eq!(std::fs::read(dir.path().join("file.bin")).unwrap(), data);
}

#[tokio::test]
async fn anonymous_mode_sends_no_user_agent_or_referer() {
    let (server, _) = served().await;
    let dir = tempfile::tempdir().unwrap();
    let request = DownloadRequest::new(&server.url("file.bin"))
        .unwrap()
        .output_dir(dir.path())
        .referer("https://example.com/page")
        .unwrap();
    let engine = engine(EngineConfig {
        anonymous: true,
        ..EngineConfig::default()
    });
    let id = engine.add(request);
    engine.wait(id).await;

    let received = server.received();
    assert!(!received.is_empty());
    for request in received {
        assert!(request.headers.get("user-agent").is_none(), "{request:?}");
        assert!(request.headers.get("referer").is_none(), "{request:?}");
    }
}
