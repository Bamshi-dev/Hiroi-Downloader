//! End-to-end tests against a local server.

mod support;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use hiroi_engine::{
    Checksum, DownloadListener, DownloadRequest, DownloadSnapshot, DownloadStatus, Engine,
    EngineConfig, RetryPolicy,
};
use support::{Behavior, TestServer, content};

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;

/// Fast timings so tests don't wait on production-sized delays.
fn config() -> EngineConfig {
    EngineConfig {
        min_split_size: 64 * KIB as u64,
        progress_interval: Duration::from_millis(50),
        checkpoint_interval: Duration::from_millis(100),
        retry: RetryPolicy {
            base_delay: Duration::from_millis(20),
            max_delay: Duration::from_millis(200),
            ..RetryPolicy::default()
        },
        ..EngineConfig::default()
    }
}

fn engine() -> Engine {
    Engine::new(config()).unwrap()
}

fn request(server: &TestServer, name: &str, dir: &Path) -> DownloadRequest {
    DownloadRequest::new(&server.url(name))
        .unwrap()
        .output_dir(dir)
}

async fn run(engine: &Engine, request: DownloadRequest) -> DownloadSnapshot {
    let id = engine.add(request);
    tokio::time::timeout(Duration::from_secs(60), engine.wait(id))
        .await
        .expect("download timed out");
    engine.snapshot(id).unwrap()
}

fn temp_files(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".hiroi-part") || name.ends_with(".hiroi-state"))
        .collect()
}

#[tokio::test]
async fn segmented_download_matches_the_source() {
    let data = content(4 * MIB, 1);
    let behavior = Behavior {
        bytes_per_sec: Some(2 * MIB as u64),
        ..Behavior::default()
    };
    let server = TestServer::start(behavior).await;
    server.put("file.bin", data.clone());
    let dir = tempfile::tempdir().unwrap();

    let snapshot = run(
        &engine(),
        request(&server, "file.bin", dir.path()).connections(8),
    )
    .await;

    assert_eq!(snapshot.status, DownloadStatus::Completed);
    assert_eq!(std::fs::read(dir.path().join("file.bin")).unwrap(), data);
    assert!(
        server.peak_connections() > 1,
        "should use several connections"
    );
    assert!(
        temp_files(dir.path()).is_empty(),
        "temporary files must be gone"
    );
}

#[tokio::test]
async fn pause_and_resume_continue_where_they_stopped() {
    let size = 4 * MIB;
    let data = content(size, 2);
    let behavior = Behavior {
        bytes_per_sec: Some(512 * KIB as u64),
        ..Behavior::default()
    };
    let server = TestServer::start(behavior).await;
    server.put("big.bin", data.clone());
    let dir = tempfile::tempdir().unwrap();

    let first = engine();
    let id = first.add(request(&server, "big.bin", dir.path()).connections(4));
    while first.snapshot(id).unwrap().downloaded < MIB as u64 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(first.pause(id));
    assert_eq!(first.wait(id).await, Some(DownloadStatus::Paused));
    let paused_at = first.snapshot(id).unwrap().downloaded;
    assert!(dir.path().join("big.bin.hiroi-state").exists());

    server.set_behavior(Behavior::default());
    let served_before = server.bytes_served();
    let second = engine();
    let snapshot = run(
        &second,
        request(&server, "big.bin", dir.path()).connections(4),
    )
    .await;

    assert_eq!(snapshot.status, DownloadStatus::Completed);
    assert_eq!(std::fs::read(dir.path().join("big.bin")).unwrap(), data);
    let served_again = server.bytes_served() - served_before;
    assert!(
        served_again <= size as u64 - paused_at + 256 * KIB as u64,
        "resume fetched {served_again} bytes although {paused_at} were already saved"
    );
}

#[tokio::test]
async fn servers_without_ranges_use_one_connection() {
    let data = content(MIB, 3);
    let server = TestServer::start(Behavior {
        ranges: false,
        ..Behavior::default()
    })
    .await;
    server.put("plain.bin", data.clone());
    let dir = tempfile::tempdir().unwrap();

    let snapshot = run(
        &engine(),
        request(&server, "plain.bin", dir.path()).connections(8),
    )
    .await;

    assert_eq!(snapshot.status, DownloadStatus::Completed);
    assert!(!snapshot.resumable);
    assert_eq!(server.peak_connections(), 1);
    assert_eq!(std::fs::read(dir.path().join("plain.bin")).unwrap(), data);
}

#[tokio::test]
async fn dropped_connections_are_retried() {
    let data = content(2 * MIB, 4);
    let server = TestServer::start(Behavior {
        drop_after: Some(300 * KIB as u64),
        ..Behavior::default()
    })
    .await;
    server.put("flaky.bin", data.clone());
    let dir = tempfile::tempdir().unwrap();

    let snapshot = run(
        &engine(),
        request(&server, "flaky.bin", dir.path()).connections(4),
    )
    .await;

    assert_eq!(snapshot.status, DownloadStatus::Completed);
    assert_eq!(std::fs::read(dir.path().join("flaky.bin")).unwrap(), data);
}

#[tokio::test]
async fn backs_off_when_the_server_limits_connections() {
    let data = content(2 * MIB, 5);
    let behavior = Behavior {
        max_connections: Some(2),
        bytes_per_sec: Some(2 * MIB as u64),
        ..Behavior::default()
    };
    let server = TestServer::start(behavior).await;
    server.put("limited.bin", data.clone());
    let dir = tempfile::tempdir().unwrap();

    let snapshot = run(
        &engine(),
        request(&server, "limited.bin", dir.path()).connections(8),
    )
    .await;

    assert_eq!(snapshot.status, DownloadStatus::Completed);
    assert_eq!(std::fs::read(dir.path().join("limited.bin")).unwrap(), data);
    assert!(
        server.throttled() <= 12,
        "kept hammering after being throttled: {} rejected requests",
        server.throttled()
    );
}

#[tokio::test]
async fn a_changed_remote_file_is_never_mixed_with_old_data() {
    let old = content(2 * MIB, 6);
    let new = content(2 * MIB, 7);
    // Dropped connections force new requests after the change.
    let behavior = Behavior {
        bytes_per_sec: Some(256 * KIB as u64),
        drop_after: Some(128 * KIB as u64),
        ..Behavior::default()
    };
    let server = TestServer::start(behavior).await;
    server.put("moving.bin", old);
    let dir = tempfile::tempdir().unwrap();

    // The file changes while the download is running.
    let engine = engine();
    let id = engine.add(request(&server, "moving.bin", dir.path()).connections(4));
    while engine.snapshot(id).unwrap().downloaded < 256 * KIB as u64 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    server.put("moving.bin", new.clone());
    engine.wait(id).await;
    let status = engine.snapshot(id).unwrap().status;
    assert!(
        matches!(&status, DownloadStatus::Failed(message) if message.contains("changed")),
        "expected a 'file changed' failure, got {status:?}"
    );

    // Running it again starts over with the new version.
    server.set_behavior(Behavior::default());
    let snapshot = run(&engine, request(&server, "moving.bin", dir.path())).await;
    assert_eq!(snapshot.status, DownloadStatus::Completed);
    assert_eq!(std::fs::read(dir.path().join("moving.bin")).unwrap(), new);
}

#[tokio::test]
async fn mirrors_take_over_when_the_main_server_fails() {
    let data = content(MIB, 8);
    let broken = TestServer::start(Behavior::default()).await;
    let mirror = TestServer::start(Behavior::default()).await;
    mirror.put("m.bin", data.clone());
    let dir = tempfile::tempdir().unwrap();

    let request = DownloadRequest::new(&broken.url("m.bin"))
        .unwrap()
        .mirror(&mirror.url("m.bin"))
        .unwrap()
        .output_dir(dir.path());
    let snapshot = run(&engine(), request).await;

    assert_eq!(snapshot.status, DownloadStatus::Completed);
    assert_eq!(std::fs::read(dir.path().join("m.bin")).unwrap(), data);
}

#[tokio::test]
async fn mirrors_share_the_work() {
    let data = content(4 * MIB, 9);
    let behavior = Behavior {
        bytes_per_sec: Some(MIB as u64),
        ..Behavior::default()
    };
    let first = TestServer::start(behavior.clone()).await;
    let second = TestServer::start(behavior).await;
    first.put("shared.bin", data.clone());
    second.put("shared.bin", data.clone());
    let dir = tempfile::tempdir().unwrap();

    let request = DownloadRequest::new(&first.url("shared.bin"))
        .unwrap()
        .mirror(&second.url("shared.bin"))
        .unwrap()
        .output_dir(dir.path())
        .connections(8);
    let snapshot = run(&engine(), request).await;

    assert_eq!(snapshot.status, DownloadStatus::Completed);
    assert_eq!(std::fs::read(dir.path().join("shared.bin")).unwrap(), data);
    assert!(
        first.bytes_served() > 0 && second.bytes_served() > 0,
        "both mirrors should serve data"
    );
}

#[tokio::test]
async fn checksums_are_verified() {
    let data = b"hello hiroi".to_vec();
    let server = TestServer::start(Behavior::default()).await;
    server.put("hello.txt", data);
    let dir = tempfile::tempdir().unwrap();

    let good: Checksum = "sha256:5c5a4e4dee8e0b71b0bbe7a1ca8b4c6f4b5e64c8e6d3b9f9b1b1b6e1d7f2c0a1"
        .parse()
        .unwrap();
    let snapshot = run(
        &engine(),
        request(&server, "hello.txt", dir.path()).checksum(good),
    )
    .await;
    assert!(matches!(&snapshot.status, DownloadStatus::Failed(m) if m.contains("checksum")));
    assert!(
        !dir.path().join("hello.txt").exists(),
        "a corrupt file must not be kept"
    );
    assert!(temp_files(dir.path()).is_empty());

    let right: Checksum = format!("sha256:{}", sha256_hex(b"hello hiroi"))
        .parse()
        .unwrap();
    let snapshot = run(
        &engine(),
        request(&server, "hello.txt", dir.path()).checksum(right),
    )
    .await;
    assert_eq!(snapshot.status, DownloadStatus::Completed);
}

#[tokio::test]
async fn speed_limit_is_respected() {
    let server = TestServer::start(Behavior::default()).await;
    server.put("slow.bin", content(512 * KIB, 10));
    let dir = tempfile::tempdir().unwrap();

    let started = Instant::now();
    let request = request(&server, "slow.bin", dir.path()).speed_limit(Some(256 * KIB as u64));
    let snapshot = run(&engine(), request).await;

    assert_eq!(snapshot.status, DownloadStatus::Completed);
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(1400),
        "512 KiB at 256 KiB/s took only {elapsed:?}"
    );
}

#[tokio::test]
async fn hostile_file_names_stay_inside_the_folder() {
    let behavior = Behavior {
        content_disposition: Some(r#"attachment; filename="../../evil.sh""#.into()),
        ..Behavior::default()
    };
    let server = TestServer::start(behavior).await;
    server.put("download", b"#!/bin/sh".to_vec());
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("inner");

    let snapshot = run(&engine(), request(&server, "download", &dir)).await;

    assert_eq!(snapshot.status, DownloadStatus::Completed);
    assert!(dir.join("evil.sh").exists());
    assert!(!root.path().join("evil.sh").exists());
}

#[tokio::test]
async fn existing_files_are_never_overwritten_by_default() {
    let server = TestServer::start(Behavior::default()).await;
    server.put("report.pdf", b"new".to_vec());
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("report.pdf"), b"mine").unwrap();

    let snapshot = run(&engine(), request(&server, "report.pdf", dir.path())).await;

    assert_eq!(snapshot.status, DownloadStatus::Completed);
    assert_eq!(
        std::fs::read(dir.path().join("report.pdf")).unwrap(),
        b"mine"
    );
    assert_eq!(
        std::fs::read(dir.path().join("report (1).pdf")).unwrap(),
        b"new"
    );
}

#[tokio::test]
async fn cancel_removes_temporary_files() {
    let behavior = Behavior {
        bytes_per_sec: Some(256 * KIB as u64),
        ..Behavior::default()
    };
    let server = TestServer::start(behavior).await;
    server.put("gone.bin", content(2 * MIB, 11));
    let dir = tempfile::tempdir().unwrap();

    let engine = engine();
    let id = engine.add(request(&server, "gone.bin", dir.path()));
    while engine.snapshot(id).unwrap().downloaded == 0 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let before = engine.snapshot(id).unwrap();
    assert!(engine.cancel(id), "cancel refused, snapshot: {before:?}");

    assert_eq!(engine.wait(id).await, Some(DownloadStatus::Cancelled));
    assert!(temp_files(dir.path()).is_empty());
    assert!(!dir.path().join("gone.bin").exists());
}

#[tokio::test]
async fn empty_files_complete() {
    let server = TestServer::start(Behavior::default()).await;
    server.put("empty.txt", Vec::new());
    let dir = tempfile::tempdir().unwrap();

    let snapshot = run(&engine(), request(&server, "empty.txt", dir.path())).await;

    assert_eq!(snapshot.status, DownloadStatus::Completed);
    assert_eq!(std::fs::read(dir.path().join("empty.txt")).unwrap(), b"");
}

/// Counts how many downloads transfer at the same time.
#[derive(Default)]
struct Concurrency {
    running: AtomicUsize,
    peak: AtomicUsize,
}

impl DownloadListener for Concurrency {
    fn on_status_changed(&self, snapshot: &DownloadSnapshot) {
        match snapshot.status {
            DownloadStatus::Connecting => {
                let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(now, Ordering::SeqCst);
            }
            DownloadStatus::Completed | DownloadStatus::Failed(_) => {
                self.running.fetch_sub(1, Ordering::SeqCst);
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn queue_limits_active_downloads() {
    let behavior = Behavior {
        bytes_per_sec: Some(MIB as u64),
        ..Behavior::default()
    };
    let server = TestServer::start(behavior).await;
    for i in 0..4 {
        server.put(&format!("q{i}.bin"), content(256 * KIB, 20 + i));
    }
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(EngineConfig {
        max_active_downloads: 2,
        ..config()
    })
    .unwrap();
    let counter = Arc::new(Concurrency::default());
    engine.add_listener(counter.clone());

    let ids: Vec<_> = (0..4)
        .map(|i| engine.add(request(&server, &format!("q{i}.bin"), dir.path())))
        .collect();
    for id in ids {
        assert_eq!(engine.wait(id).await, Some(DownloadStatus::Completed));
    }
    assert_eq!(counter.peak.load(Ordering::SeqCst), 2);
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(data))
}
