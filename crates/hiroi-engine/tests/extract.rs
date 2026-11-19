//! Pages handed to yt-dlp. Skipped when yt-dlp (or, where needed,
//! ffmpeg) is not installed.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use hiroi_engine::{DownloadRequest, DownloadStatus, Engine, EngineConfig};
use hiroi_testserver::{Behavior, TestServer, content};

fn installed(program: &str) -> bool {
    let found = Command::new(program).arg("--version").output().is_ok();
    if !found {
        eprintln!("{program} not installed; skipping");
    }
    found
}

fn engine(config: EngineConfig) -> Engine {
    Engine::new(EngineConfig {
        progress_interval: Duration::from_millis(50),
        ..config
    })
    .unwrap()
}

async fn run(engine: &Engine, request: DownloadRequest) -> hiroi_engine::DownloadSnapshot {
    let id = engine.add(request);
    tokio::time::timeout(Duration::from_secs(120), engine.wait(id))
        .await
        .expect("timed out");
    engine.snapshot(id).unwrap()
}

fn page(title: &str, source: &str, kind: &str) -> String {
    format!(
        "<html><head><title>{title}</title></head><body>\
         <video controls><source src=\"{source}\" type=\"{kind}\"></video></body></html>"
    )
}

/// The only file left in `dir`.
fn only_file(dir: &Path) -> PathBuf {
    let files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    files[0].clone()
}

fn streams(path: &Path) -> String {
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&probe.stdout).into_owned()
}

/// Writes test media with ffmpeg into a fresh folder.
fn media(commands: &[&str]) -> tempfile::TempDir {
    let work = tempfile::tempdir().unwrap();
    for args in commands {
        let status = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(args.split_whitespace())
            .current_dir(work.path())
            .status()
            .unwrap();
        assert!(status.success(), "ffmpeg {args}");
    }
    work
}

fn serve_folder(server: &TestServer, folder: &Path, prefix: &str) {
    for entry in std::fs::read_dir(folder).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let kind = match path.extension().and_then(|e| e.to_str()) {
            Some("m3u8") => "application/vnd.apple.mpegurl",
            Some("mpd") => "application/dash+xml",
            _ => "video/mp4",
        };
        server.put_typed(
            &format!("{prefix}{name}"),
            std::fs::read(&path).unwrap(),
            kind,
        );
    }
}

#[tokio::test]
async fn direct_files_found_by_yt_dlp_are_downloaded_by_the_engine() {
    if !installed("yt-dlp") {
        return;
    }
    // Slow enough per connection that splitting the file pays off.
    let server = TestServer::start(Behavior {
        bytes_per_sec: Some(2_000_000),
        ..Behavior::default()
    })
    .await;
    let clip = content(3_000_000, 7);
    server.put_typed("clip.mp4", clip.clone(), "video/mp4");
    server.put_typed(
        "watch.html",
        page("Holiday", "clip.mp4", "video/mp4"),
        "text/html",
    );
    let dir = tempfile::tempdir().unwrap();

    let request = DownloadRequest::new(&server.url("watch.html"))
        .unwrap()
        .output_dir(dir.path())
        .connections(4)
        .extract(true);
    let snapshot = run(&engine(EngineConfig::default()), request).await;

    assert_eq!(
        snapshot.status,
        DownloadStatus::Completed,
        "{:?}",
        snapshot.status
    );
    let file = only_file(dir.path());
    assert!(
        file.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("Holiday")
    );
    assert_eq!(std::fs::read(&file).unwrap(), clip);
    // The engine fetched it in parts, not yt-dlp in one go.
    assert!(server.peak_connections() > 1);
}

#[tokio::test]
async fn separate_hls_tracks_are_fetched_and_joined() {
    if !installed("yt-dlp") || !installed("ffmpeg") {
        return;
    }
    let work = media(&[
        "-f lavfi -i testsrc=size=160x90:rate=10:duration=3 -c:v libx264 -preset ultrafast -an \
         -f hls -hls_time 1 -hls_playlist_type vod v.m3u8",
        "-f lavfi -i sine=frequency=440:duration=3 -c:a aac -vn \
         -f hls -hls_time 1 -hls_playlist_type vod a.m3u8",
    ]);
    let server = TestServer::start(Behavior::default()).await;
    serve_folder(&server, work.path(), "");
    let master = "#EXTM3U\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"en\",DEFAULT=YES,URI=\"a.m3u8\"\n\
        #EXT-X-STREAM-INF:BANDWIDTH=300000,RESOLUTION=160x90,CODECS=\"avc1.42c00d,mp4a.40.2\",AUDIO=\"aud\"\n\
        v.m3u8\n";
    server.put_typed("film.m3u8", master, "application/vnd.apple.mpegurl");
    server.put_typed(
        "film.html",
        page("Film", "film.m3u8", "application/x-mpegURL"),
        "text/html",
    );
    let dir = tempfile::tempdir().unwrap();

    let request = DownloadRequest::new(&server.url("film.html"))
        .unwrap()
        .output_dir(dir.path())
        .extract(true);
    let snapshot = run(&engine(EngineConfig::default()), request).await;

    assert_eq!(
        snapshot.status,
        DownloadStatus::Completed,
        "{:?}",
        snapshot.status
    );
    let file = only_file(dir.path());
    assert_eq!(file.extension().unwrap(), "mp4");
    let found = streams(&file);
    assert!(
        found.contains("video") && found.contains("audio"),
        "{found}"
    );
}

#[tokio::test]
async fn other_protocols_are_left_to_yt_dlp() {
    if !installed("yt-dlp") || !installed("ffmpeg") {
        return;
    }
    let work = media(&[
        "-f lavfi -i testsrc=size=160x90:rate=10:duration=3 -f lavfi -i sine=duration=3 \
         -map 0:v -map 1:a -c:v libx264 -preset ultrafast -c:a aac \
         -f dash -seg_duration 1 show.mpd",
    ]);
    let server = TestServer::start(Behavior::default()).await;
    serve_folder(&server, work.path(), "");
    server.put_typed(
        "show.html",
        page("Show", "show.mpd", "application/dash+xml"),
        "text/html",
    );
    let dir = tempfile::tempdir().unwrap();

    let request = DownloadRequest::new(&server.url("show.html"))
        .unwrap()
        .output_dir(dir.path())
        .extract(true);
    let snapshot = run(&engine(EngineConfig::default()), request).await;

    assert_eq!(
        snapshot.status,
        DownloadStatus::Completed,
        "{:?}",
        snapshot.status
    );
    let file = only_file(dir.path());
    assert!(
        file.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("Show")
    );
    let found = streams(&file);
    assert!(
        found.contains("video") && found.contains("audio"),
        "{found}"
    );
    assert!(snapshot.downloaded > 0, "progress was read from yt-dlp");
}

#[tokio::test]
async fn a_missing_yt_dlp_is_reported_plainly() {
    let server = TestServer::start(Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let config = EngineConfig {
        yt_dlp: PathBuf::from("/nonexistent/yt-dlp"),
        ..EngineConfig::default()
    };

    let request = DownloadRequest::new(&server.url("watch.html"))
        .unwrap()
        .output_dir(dir.path())
        .extract(true);
    let snapshot = run(&engine(config), request).await;

    assert!(
        matches!(&snapshot.status, DownloadStatus::Failed(m) if m.contains("not installed")),
        "{:?}",
        snapshot.status
    );
}
