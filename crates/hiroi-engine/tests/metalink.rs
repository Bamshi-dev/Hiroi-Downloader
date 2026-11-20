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

fn piece_hashes(data: &[u8], length: usize) -> String {
    data.chunks(length)
        .map(|piece| format!("<hash>{}</hash>", sha256(piece)))
        .collect()
}

fn metalink_with_pieces(name: &str, data: &[u8], length: usize, urls: &[String]) -> String {
    let urls: String = urls
        .iter()
        .enumerate()
        .map(|(i, url)| format!("<url priority=\"{}\">{url}</url>", i + 1))
        .collect();
    format!(
        "<metalink xmlns=\"urn:ietf:params:xml:ns:metalink\"><file name=\"{name}\">\
         <size>{}</size><pieces length=\"{length}\" type=\"sha-256\">{}</pieces>{urls}\
         </file></metalink>",
        data.len(),
        piece_hashes(data, length)
    )
}

/// `data` with a few bytes changed, as a bad mirror might send it.
fn tampered(data: &[u8]) -> Vec<u8> {
    let mut copy = data.to_vec();
    for at in [100_000, 900_000, 1_700_000] {
        copy[at] ^= 0xff;
    }
    copy
}

fn small_splits() -> Engine {
    Engine::new(EngineConfig {
        progress_interval: Duration::from_millis(50),
        min_split_size: 128 * 1024,
        ..EngineConfig::default()
    })
    .unwrap()
}

#[tokio::test]
async fn broken_pieces_are_fetched_again_from_a_good_mirror() {
    let bad = TestServer::start(Behavior::default()).await;
    let good = TestServer::start(Behavior {
        bytes_per_sec: Some(4_000_000),
        ..Behavior::default()
    })
    .await;
    let data = content(2_000_000, 21);
    bad.put("disk.iso", tampered(&data));
    good.put("disk.iso", data.clone());
    let urls = [bad.url("disk.iso"), good.url("disk.iso")];
    good.put_typed(
        "disk.meta4",
        metalink_with_pieces("disk.iso", &data, 256 * 1024, &urls),
        META4,
    );
    let dir = tempfile::tempdir().unwrap();

    let request = DownloadRequest::new(&good.url("disk.meta4"))
        .unwrap()
        .output_dir(dir.path())
        .connections(4);
    let snapshot = run(&small_splits(), request).await;

    assert_eq!(
        snapshot.status,
        DownloadStatus::Completed,
        "{:?}",
        snapshot.status
    );
    assert_eq!(std::fs::read(dir.path().join("disk.iso")).unwrap(), data);
    assert!(bad.bytes_served() > 0, "the bad mirror took part");
}

#[tokio::test]
async fn pieces_that_stay_broken_fail_the_download() {
    let server = TestServer::start(Behavior::default()).await;
    let data = content(2_000_000, 22);
    server.put("disk.iso", tampered(&data));
    let urls = [server.url("disk.iso")];
    server.put_typed(
        "disk.meta4",
        metalink_with_pieces("disk.iso", &data, 256 * 1024, &urls),
        META4,
    );
    let dir = tempfile::tempdir().unwrap();

    let request = DownloadRequest::new(&server.url("disk.meta4"))
        .unwrap()
        .output_dir(dir.path());
    let snapshot = run(&small_splits(), request).await;

    assert!(
        matches!(&snapshot.status, DownloadStatus::Failed(m) if m.contains("piece hashes")),
        "{:?}",
        snapshot.status
    );
    assert!(!dir.path().join("disk.iso").exists());
    // How often each 16 KiB block was asked for. Good pieces come once
    // (twice where a connection split another's range); broken ones come
    // in the first pass and in each of the three repairs.
    const BLOCK: u64 = 16 * 1024;
    let mut asked = vec![0u32; data.len().div_ceil(BLOCK as usize)];
    for received in server.received() {
        let Some(range) = received.headers.get("range").and_then(|r| r.to_str().ok()) else {
            continue;
        };
        let (start, end) = range.trim_start_matches("bytes=").split_once('-').unwrap();
        let (start, end): (u64, u64) = (start.parse().unwrap(), end.parse::<u64>().unwrap() + 1);
        if end - start <= 1 {
            continue; // the probe
        }
        for block in start.div_ceil(BLOCK)..end / BLOCK {
            asked[block as usize] += 1;
        }
    }
    let piece = 256 * 1024;
    let broken = |block: usize| {
        [100_000, 900_000, 1_700_000]
            .iter()
            .any(|&at| at / piece == block * BLOCK as usize / piece)
    };
    for (block, &count) in asked.iter().enumerate() {
        if broken(block) {
            assert!(count >= 4, "broken block {block} asked {count} times");
        } else {
            assert!(count <= 2, "good block {block} asked {count} times");
        }
    }
}
