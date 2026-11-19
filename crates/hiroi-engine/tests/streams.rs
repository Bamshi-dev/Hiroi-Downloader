//! HLS streams from a local server.

use std::path::Path;
use std::time::Duration;

use aes::cipher::{BlockModeEncrypt, KeyIvInit, block_padding::Pkcs7};
use hiroi_engine::{DownloadRequest, DownloadStatus, Engine, EngineConfig, Quality, RetryPolicy};
use hiroi_testserver::{Behavior, TestServer, content};

const M3U8: &str = "application/vnd.apple.mpegurl";

fn engine() -> Engine {
    Engine::new(EngineConfig {
        progress_interval: Duration::from_millis(50),
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

fn encrypt(key: &[u8; 16], iv: &[u8; 16], plain: &[u8]) -> Vec<u8> {
    let mut buffer = plain.to_vec();
    buffer.resize(plain.len() + 16, 0);
    let length = cbc::Encryptor::<aes::Aes128>::new(key.into(), iv.into())
        .encrypt_padded::<Pkcs7>(&mut buffer, plain.len())
        .expect("room for padding")
        .len();
    buffer.truncate(length);
    buffer
}

fn segments(count: usize, size: usize) -> Vec<Vec<u8>> {
    (0..count).map(|i| content(size, 100 + i as u64)).collect()
}

fn media_playlist(names: &[String], ended: bool, sequence: usize, extra: &str) -> String {
    let mut text =
        format!("#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:{sequence}\n{extra}");
    for name in names {
        text.push_str(&format!("#EXTINF:1.0,\n{name}\n"));
    }
    if ended {
        text.push_str("#EXT-X-ENDLIST\n");
    }
    text
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

#[tokio::test]
async fn encrypted_streams_are_decrypted_and_joined_in_order() {
    let server = TestServer::start(Behavior::default()).await;
    let key = [0x42u8; 16];
    let parts = segments(12, 50_000);
    let mut names = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        // No IV in the playlist: the media sequence number is the IV.
        let mut iv = [0u8; 16];
        iv[8..].copy_from_slice(&(i as u64).to_be_bytes());
        let encrypted = encrypt(&key, &iv, part);
        server.put(&format!("seg{i}.ts"), encrypted);
        names.push(format!("seg{i}.ts"));
    }
    server.put("key.bin", key.to_vec());
    let key_line = "#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n";
    server.put_typed("show.m3u8", media_playlist(&names, true, 0, key_line), M3U8);
    let dir = tempfile::tempdir().unwrap();

    let snapshot = run(
        &engine(),
        DownloadRequest::new(&server.url("show.m3u8"))
            .unwrap()
            .output_dir(dir.path()),
    )
    .await;

    assert_eq!(snapshot.status, DownloadStatus::Completed);
    assert_eq!(read(&dir.path().join("show.ts")), parts.concat());
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        1,
        "no work files left"
    );
}

#[tokio::test]
async fn master_playlists_pick_the_requested_quality() {
    let server = TestServer::start(Behavior::default()).await;
    for (variant, seed) in [("low", 1), ("high", 2)] {
        server.put(&format!("{variant}0.ts"), content(1000, seed));
        let playlist = media_playlist(&[format!("{variant}0.ts")], true, 0, "");
        server.put_typed(&format!("{variant}.m3u8"), playlist, M3U8);
    }
    let master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=500000,RESOLUTION=640x360\nlow.m3u8\n\
                  #EXT-X-STREAM-INF:BANDWIDTH=4000000,RESOLUTION=1920x1080\nhigh.m3u8\n";
    server.put_typed("master.m3u8", master, M3U8);
    let dir = tempfile::tempdir().unwrap();
    let request = || {
        DownloadRequest::new(&server.url("master.m3u8"))
            .unwrap()
            .output_dir(dir.path())
    };

    run(&engine(), request().file_name("best.ts")).await;
    run(
        &engine(),
        request()
            .file_name("small.ts")
            .quality(Quality::MaxHeight(480)),
    )
    .await;

    assert_eq!(read(&dir.path().join("best.ts")), content(1000, 2));
    assert_eq!(read(&dir.path().join("small.ts")), content(1000, 1));
}

#[tokio::test]
async fn fragmented_mp4_with_byte_ranges() {
    let server = TestServer::start(Behavior::default()).await;
    let init = content(700, 9);
    let media = content(3000, 10);
    server.put("all.mp4", [init.clone(), media.clone()].concat());
    let playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MAP:URI=\"all.mp4\",BYTERANGE=\"700@0\"\n\
                    #EXTINF:1,\n#EXT-X-BYTERANGE:1000@700\nall.mp4\n\
                    #EXTINF:1,\n#EXT-X-BYTERANGE:2000\nall.mp4\n#EXT-X-ENDLIST\n";
    server.put_typed("clip.m3u8", playlist, M3U8);
    let dir = tempfile::tempdir().unwrap();

    run(
        &engine(),
        DownloadRequest::new(&server.url("clip.m3u8"))
            .unwrap()
            .output_dir(dir.path()),
    )
    .await;

    assert_eq!(read(&dir.path().join("clip.mp4")), [init, media].concat());
}

#[tokio::test]
async fn live_streams_are_followed_until_they_end() {
    let server = TestServer::start(Behavior::default()).await;
    let parts = segments(8, 20_000);
    for (i, part) in parts.iter().enumerate() {
        server.put(&format!("live{i}.ts"), part.clone());
    }
    let names: Vec<String> = (0..8).map(|i| format!("live{i}.ts")).collect();
    // A sliding window of three segments, as live servers publish them.
    server.put_typed("live.m3u8", media_playlist(&names[..3], false, 0, ""), M3U8);
    let publisher = {
        let names = names.clone();
        let url = server.base_url().to_string();
        let server = &server;
        async move {
            for end in 4..=8 {
                tokio::time::sleep(Duration::from_millis(400)).await;
                let start = end - 3;
                server.put_typed(
                    "live.m3u8",
                    media_playlist(&names[start..end], end == 8, start, ""),
                    M3U8,
                );
            }
            drop(url);
        }
    };
    let dir = tempfile::tempdir().unwrap();
    let engine = engine();
    let request = DownloadRequest::new(&server.url("live.m3u8"))
        .unwrap()
        .output_dir(dir.path());
    let (snapshot, ()) = tokio::join!(run(&engine, request), publisher);

    assert_eq!(snapshot.status, DownloadStatus::Completed);
    assert_eq!(read(&dir.path().join("live.ts")), parts.concat());
}

#[tokio::test]
async fn live_recordings_stop_at_their_limit_and_keep_what_they_have() {
    let server = TestServer::start(Behavior::default()).await;
    for i in 0..3 {
        server.put(&format!("n{i}.ts"), content(5000, i));
    }
    let names: Vec<String> = (0..3).map(|i| format!("n{i}.ts")).collect();
    server.put_typed("never.m3u8", media_playlist(&names, false, 0, ""), M3U8);
    let dir = tempfile::tempdir().unwrap();

    let request = DownloadRequest::new(&server.url("never.m3u8"))
        .unwrap()
        .output_dir(dir.path())
        .live_limit(Some(Duration::from_millis(800)));
    let snapshot = run(&engine(), request).await;

    assert_eq!(snapshot.status, DownloadStatus::Completed);
    assert_eq!(read(&dir.path().join("never.ts")).len(), 15_000);
}

#[tokio::test]
async fn drm_streams_are_refused() {
    let server = TestServer::start(Behavior::default()).await;
    let playlist = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://key\"\n#EXTINF:1,\na.ts\n#EXT-X-ENDLIST\n";
    server.put_typed("drm.m3u8", playlist, M3U8);
    let dir = tempfile::tempdir().unwrap();

    let snapshot = run(
        &engine(),
        DownloadRequest::new(&server.url("drm.m3u8"))
            .unwrap()
            .output_dir(dir.path()),
    )
    .await;

    assert!(
        matches!(&snapshot.status, DownloadStatus::Failed(m) if m.contains("DRM")),
        "{:?}",
        snapshot.status
    );
}

#[tokio::test]
async fn paused_streams_continue_with_the_next_segment() {
    let server = TestServer::start(Behavior {
        bytes_per_sec: Some(100_000),
        ..Behavior::default()
    })
    .await;
    let parts = segments(20, 25_000);
    let names: Vec<String> = (0..20).map(|i| format!("p{i}.ts")).collect();
    for (name, part) in names.iter().zip(&parts) {
        server.put(name, part.clone());
    }
    server.put_typed("vod.m3u8", media_playlist(&names, true, 0, ""), M3U8);
    let dir = tempfile::tempdir().unwrap();
    let request = || {
        DownloadRequest::new(&server.url("vod.m3u8"))
            .unwrap()
            .output_dir(dir.path())
            .connections(2)
    };

    let first = engine();
    let id = first.add(request());
    while first.snapshot(id).unwrap().downloaded < 150_000 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    first.pause(id);
    assert_eq!(first.wait(id).await, Some(DownloadStatus::Paused));

    server.set_behavior(Behavior::default());
    let served = server.bytes_served();
    let snapshot = run(&engine(), request()).await;
    assert_eq!(snapshot.status, DownloadStatus::Completed);
    assert_eq!(read(&dir.path().join("vod.ts")), parts.concat());
    assert!(
        server.bytes_served() - served < 450_000,
        "a resume must not start over"
    );
}

#[tokio::test]
async fn separate_audio_is_joined_with_ffmpeg() {
    if std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_err()
    {
        eprintln!("ffmpeg not installed; skipping");
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let make = |args: &str| {
        let status = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(args.split_whitespace())
            .current_dir(work.path())
            .status()
            .unwrap();
        assert!(status.success(), "ffmpeg {args}");
    };
    make(
        "-f lavfi -i testsrc=size=160x90:rate=10:duration=2 -c:v libx264 -preset ultrafast -an -f hls -hls_time 1 -hls_playlist_type vod v.m3u8",
    );
    make(
        "-f lavfi -i sine=frequency=440:duration=2 -c:a aac -vn -f hls -hls_time 1 -hls_playlist_type vod a.m3u8",
    );
    let server = TestServer::start(Behavior::default()).await;
    for entry in std::fs::read_dir(work.path()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let kind = if name.ends_with(".m3u8") {
            M3U8
        } else {
            "video/mp2t"
        };
        server.put_typed(&name, read(&path), kind);
    }
    let master = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",DEFAULT=YES,URI=\"a.m3u8\"\n\
                  #EXT-X-STREAM-INF:BANDWIDTH=300000,RESOLUTION=160x90,AUDIO=\"aud\"\nv.m3u8\n";
    server.put_typed("film.m3u8", master, M3U8);
    let dir = tempfile::tempdir().unwrap();

    let snapshot = run(
        &engine(),
        DownloadRequest::new(&server.url("film.m3u8"))
            .unwrap()
            .output_dir(dir.path()),
    )
    .await;

    assert_eq!(
        snapshot.status,
        DownloadStatus::Completed,
        "{:?}",
        snapshot.status
    );
    let probe = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(dir.path().join("film.mp4"))
        .output()
        .unwrap();
    let streams = String::from_utf8_lossy(&probe.stdout);
    assert!(
        streams.contains("video") && streams.contains("audio"),
        "{streams}"
    );
}
