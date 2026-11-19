//! Metalink files from a local server.

use std::time::Duration;

use hiroi_engine::{DownloadRequest, DownloadStatus, Engine, EngineConfig, RetryPolicy};
use hiroi_testserver::{Behavior, TestServer, content};
use sha2::{Digest, Sha256};

const META4: &str = "application/metalink4+xml";

fn engine() -> Engine {
    Engine::new(EngineConfig {
        progress_interval: Duration::from_millis(50),
        retry: RetryPolicy {
            base_delay: Duration::from_millis(20),
            max_attempts: 2,
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

fn sha256(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn metalink(files: &[(&str, &str, Vec<String>)]) -> String {
    let mut text = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <metalink xmlns=\"urn:ietf:params:xml:ns:metalink\">\n",
    );
    for (name, hash, urls) in files {
        text.push_str(&format!(
            "<file name=\"{name}\"><hash type=\"sha-256\">{hash}</hash>\n"
        ));
        for (priority, url) in urls.iter().enumerate() {
            text.push_str(&format!("<url priority=\"{}\">{url}</url>\n", priority + 1));
        }
        text.push_str("</file>\n");
    }
    text.push_str("</metalink>\n");
    text
}

#[tokio::test]
async fn files_come_from_the_working_servers_and_are_verified() {
    let good = TestServer::start(Behavior::default()).await;
    let other = TestServer::start(Behavior::default()).await;
    let data = content(2_000_000, 3);
    good.put("disk.iso", data.clone());
    other.put("disk.iso", data.clone());
    // The best-ranked server does not have the file.
    let urls = vec![
        good.url("missing/disk.iso"),
        good.url("disk.iso"),
        other.url("disk.iso"),
    ];
    let hash = sha256(&data);
    good.put_typed("disk.meta4", metalink(&[("disk.iso", &hash, urls)]), META4);
    let dir = tempfile::tempdir().unwrap();

    let request = DownloadRequest::new(&good.url("disk.meta4"))
        .unwrap()
        .output_dir(dir.path());
    let snapshot = run(&engine(), request).await;

    assert_eq!(
        snapshot.status,
        DownloadStatus::Completed,
        "{:?}",
        snapshot.status
    );
    assert_eq!(std::fs::read(dir.path().join("disk.iso")).unwrap(), data);
    assert!(
        !dir.path().join("disk.meta4").exists(),
        "the Metalink itself is not saved"
    );
    assert!(other.bytes_served() > 0, "both servers shared the work");
}

#[tokio::test]
async fn a_tampered_file_fails_its_hash() {
    let server = TestServer::start(Behavior::default()).await;
    server.put("tool.tar", content(100_000, 1));
    let wrong = sha256(b"something else");
    let urls = vec![server.url("tool.tar")];
    server.put_typed("tool.meta4", metalink(&[("tool.tar", &wrong, urls)]), META4);
    let dir = tempfile::tempdir().unwrap();

    let request = DownloadRequest::new(&server.url("tool.meta4"))
        .unwrap()
        .output_dir(dir.path());
    let snapshot = run(&engine(), request).await;

    assert!(
        matches!(&snapshot.status, DownloadStatus::Failed(m) if m.contains("checksum")),
        "{:?}",
        snapshot.status
    );
    assert!(!dir.path().join("tool.tar").exists());
}

#[tokio::test]
async fn every_file_is_downloaded_into_its_folder() {
    let server = TestServer::start(Behavior::default()).await;
    let first = content(50_000, 1);
    let second = content(70_000, 2);
    server.put("a.bin", first.clone());
    server.put("b.bin", second.clone());
    let files = [
        ("set/a.bin", sha256(&first), vec![server.url("a.bin")]),
        // A name that tries to leave the download folder stays inside it.
        ("../../b.bin", sha256(&second), vec![server.url("b.bin")]),
    ];
    let files: Vec<_> = files
        .iter()
        .map(|(name, hash, urls)| (*name, hash.as_str(), urls.clone()))
        .collect();
    server.put_typed("set.meta4", metalink(&files), META4);
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("downloads");

    let request = DownloadRequest::new(&server.url("set.meta4"))
        .unwrap()
        .output_dir(&dir);
    let snapshot = run(&engine(), request).await;

    assert_eq!(
        snapshot.status,
        DownloadStatus::Completed,
        "{:?}",
        snapshot.status
    );
    assert_eq!(std::fs::read(dir.join("set/a.bin")).unwrap(), first);
    assert_eq!(std::fs::read(dir.join("b.bin")).unwrap(), second);
    assert!(!root.path().join("b.bin").exists());
}
