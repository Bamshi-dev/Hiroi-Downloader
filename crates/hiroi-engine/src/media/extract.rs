//! Pages whose media only a site extractor can find. We ask yt-dlp
//! where the media is, and then download it ourselves whenever we can.
//!
//! yt-dlp knows thousands of sites, and keeping up with them is its whole
//! job; we do not try to repeat that. It answers with the address of
//! every track it chose, the protocol, and the headers it used. Plain
//! HTTP files and HLS playlists come back to the engine, so they get the
//! usual connections, limits, retries, and resume. Anything else (DASH
//! with a chosen representation, RTMP, and so on) is left to yt-dlp, and
//! we only read its progress.
//!
//! Options that may carry secrets (cookies, proxy credentials) go to
//! yt-dlp in a private config file, never on its command line, where
//! every local user could read them.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Url;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::error::{EngineError, Result};
use crate::request::{ConflictPolicy, DownloadRequest, Quality};
use crate::snapshot::DownloadStatus;
use crate::storage::naming::sanitize_file_name;
use crate::task::{self, TaskContext, or_cancel};

use super::{mux, plan_stream};

/// Longest we wait for yt-dlp to look at a page.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(120);
/// Largest answer we read; a page with every format listed is a few MiB.
const MAX_ANSWER: u64 = 64 * 1024 * 1024;
/// Marks our progress lines among yt-dlp's other output.
const PROGRESS_MARK: &str = "hiroi-progress";

/// What yt-dlp found on a page.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Media {
    pub title: String,
    /// Extension of the finished file, after joining the tracks.
    pub ext: String,
    pub live: bool,
    /// One track, or video and audio to be joined.
    pub tracks: Vec<Track>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Track {
    pub format_id: String,
    pub url: Url,
    pub protocol: String,
    pub ext: String,
    pub headers: Vec<(String, String)>,
}

impl Track {
    /// Whether the engine can download this track itself.
    fn is_ours(&self) -> bool {
        matches!(
            self.protocol.as_str(),
            "http" | "https" | "m3u8" | "m3u8_native"
        )
    }
}

/// The parts of yt-dlp's `-J` answer we use.
#[derive(Debug, Deserialize)]
struct Answer {
    #[serde(rename = "_type")]
    kind: Option<String>,
    title: Option<String>,
    ext: Option<String>,
    is_live: Option<bool>,
    #[serde(flatten)]
    format: Option<Format>,
    requested_formats: Option<Vec<Format>>,
}

#[derive(Debug, Deserialize)]
struct Format {
    format_id: Option<String>,
    url: String,
    protocol: Option<String>,
    ext: Option<String>,
    #[serde(default)]
    http_headers: std::collections::BTreeMap<String, String>,
}

pub(crate) fn parse(answer: &[u8]) -> Result<Media> {
    let answer: Answer = serde_json::from_slice(answer)
        .map_err(|e| EngineError::Extractor(format!("unreadable answer from yt-dlp: {e}")))?;
    if answer.kind.as_deref() == Some("playlist") {
        return Err(EngineError::Extractor(
            "the page is a playlist; grab it to pick the videos".into(),
        ));
    }
    let formats = match answer.requested_formats {
        Some(formats) if !formats.is_empty() => formats,
        _ => answer.format.into_iter().collect(),
    };
    let tracks = formats
        .into_iter()
        .map(|format| {
            let url = Url::parse(&format.url)
                .map_err(|_| EngineError::Extractor("yt-dlp gave an invalid URL".into()))?;
            Ok(Track {
                format_id: format.format_id.unwrap_or_default(),
                url,
                protocol: format.protocol.unwrap_or_else(|| "https".into()),
                ext: format.ext.unwrap_or_else(|| "mp4".into()),
                headers: format.http_headers.into_iter().collect(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if tracks.is_empty() || tracks.len() > 2 {
        return Err(EngineError::Extractor(format!(
            "expected one or two tracks, yt-dlp chose {}",
            tracks.len()
        )));
    }
    Ok(Media {
        title: answer.title.unwrap_or_else(|| "video".into()),
        ext: answer.ext.unwrap_or_else(|| tracks[0].ext.clone()),
        live: answer.is_live.unwrap_or(false),
        tracks,
    })
}

/// yt-dlp's format selection for a quality, unless the user wrote one.
fn selector(request: &DownloadRequest) -> Option<String> {
    if let Some(format) = &request.stream.format {
        return Some(format.clone());
    }
    match request.stream.quality {
        // yt-dlp's own default is already the best.
        Quality::Best => None,
        Quality::Worst => Some("wv*+wa/w".into()),
        Quality::MaxHeight(height) => Some(format!(
            "bv*[height<={height}]+ba/b[height<={height}]/wv*+ba/w"
        )),
    }
}

/// The formats chosen when yt-dlp was asked, then the same choice again:
/// sites such as YouTube can answer the second request with other
/// format numbers, and the exact ones are then gone.
fn delegate_selector(formats: &[&str], request: &DownloadRequest) -> String {
    let again = selector(request).unwrap_or_else(|| "bv*+ba/b".into());
    format!("{}/{again}", formats.join("+"))
}

pub(crate) async fn download(ctx: &Arc<TaskContext>) -> Result<PathBuf> {
    let page = ctx.request.url().clone();
    // Without the browser's cookies first. Signed in, some sites (YouTube
    // among them) offer yt-dlp fewer formats, or none it can use; the
    // cookies are for pages that need a login, and are tried next.
    let mut config = ConfigFile::write(ctx, false)?;
    let mut media = or_cancel(&ctx.cancel, resolve(ctx, &page, &config)).await?;
    if let Err(error) = &media
        && ctx.request.own_cookies.is_some()
    {
        tracing::info!(%error, "trying again with the browser's cookies");
        config = ConfigFile::write(ctx, true)?;
        media = or_cancel(&ctx.cancel, resolve(ctx, &page, &config)).await?;
    }
    let media = media?;
    tracing::info!(
        title = %media.title,
        formats = %media.tracks.iter().map(|t| t.format_id.as_str()).collect::<Vec<_>>().join("+"),
        "yt-dlp found the media"
    );
    let name = ctx
        .request
        .file_name
        .clone()
        .unwrap_or_else(|| format!("{}.{}", media.title, media.ext));
    let name = sanitize_file_name(&name).unwrap_or_else(|| format!("video.{}", media.ext));

    if !media.tracks.iter().all(Track::is_ours) {
        return delegate(ctx, &page, &media, &name, &config).await;
    }
    match media.tracks.as_slice() {
        [only] => fetch(ctx, only, name, ctx.request.conflict).await,
        [video, audio] => {
            let named = ctx.derive(with_name(&ctx.request, name));
            let paths = plan_stream(&named, &page, &media.ext)?;
            let stem = paths.file_name();
            let stem = Path::new(&stem)
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy();
            let track_name = |role: &str, track: &Track| format!("{stem}.{role}.{}", track.ext);
            // One after the other: the progress shows one track at a time,
            // and each gets every connection the request allows.
            let overwrite = ConflictPolicy::Overwrite;
            let video = fetch(ctx, video, track_name("video", video), overwrite).await?;
            let audio = fetch(ctx, audio, track_name("audio", audio), overwrite).await?;
            mux::combine(&video, &audio, &paths.final_path).await?;
            ctx.progress.update(|p| p.paths = Some(paths.clone()));
            Ok(paths.final_path)
        }
        _ => unreachable!("parse() checked the number of tracks"),
    }
}

/// Downloads one track as a download of its own, sharing the task's
/// progress, limits, and cancellation.
async fn fetch(
    ctx: &Arc<TaskContext>,
    track: &Track,
    name: String,
    conflict: ConflictPolicy,
) -> Result<PathBuf> {
    let mut request = with_name(&ctx.request, name);
    request.sources = vec![track.url.clone()];
    request.conflict = conflict;
    request.headers = track_headers(track)?;
    // A checksum belongs to the file the user named, not to one track.
    request.checksum = None;
    Box::pin(task::run(Arc::new(ctx.derive(request)))).await
}

fn with_name(request: &DownloadRequest, name: String) -> DownloadRequest {
    let mut request = request.clone();
    request.file_name = Some(name);
    request.stream.extract = false;
    request.stream.format = None;
    request
}

/// The headers yt-dlp used for the track. The user's cookies are not
/// among them: the download's client sends those to their site only.
fn track_headers(track: &Track) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for (name, value) in &track.headers {
        let name = HeaderName::try_from(name.as_str())
            .map_err(|_| EngineError::InvalidHeader(name.clone()))?;
        let value = HeaderValue::try_from(value.as_str())
            .map_err(|_| EngineError::InvalidHeader(name.to_string()))?;
        headers.insert(name, value);
    }
    Ok(headers)
}

async fn resolve(ctx: &TaskContext, page: &Url, config: &ConfigFile) -> Result<Media> {
    let mut command = config.command(ctx);
    command.args(["-J", "--no-warnings", "--no-playlist"]);
    if let Some(selector) = selector(&ctx.request) {
        command.arg("-f").arg(selector);
    }
    command.arg("--").arg(page.as_str());
    let mut child = spawn(ctx, command)?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let read = async {
        let (answer, errors) = tokio::join!(read_limited(stdout), read_limited(stderr));
        Ok::<_, std::io::Error>((answer?, errors?))
    };
    let (answer, errors) = tokio::time::timeout(RESOLVE_TIMEOUT, read)
        .await
        .map_err(|_| EngineError::Extractor("yt-dlp took too long to answer".into()))?
        .map_err(|e| EngineError::Extractor(format!("could not read yt-dlp: {e}")))?;
    let status = child
        .wait()
        .await
        .map_err(|e| EngineError::Extractor(format!("yt-dlp: {e}")))?;
    if !status.success() {
        return Err(EngineError::Extractor(last_error(&errors)));
    }
    parse(&answer)
}

/// Lets yt-dlp download the tracks it chose, reading its progress.
async fn delegate(
    ctx: &Arc<TaskContext>,
    page: &Url,
    media: &Media,
    name: &str,
    config: &ConfigFile,
) -> Result<PathBuf> {
    let named = ctx.derive(with_name(&ctx.request, name.to_string()));
    let paths = plan_stream(&named, page, &media.ext)?;
    let formats: Vec<&str> = media.tracks.iter().map(|t| t.format_id.as_str()).collect();
    tracing::info!(protocol = %media.tracks[0].protocol, "yt-dlp downloads this one itself");
    ctx.progress.update(|p| {
        p.paths = Some(paths.clone());
        p.resumable = !media.live;
    });
    ctx.observer
        .status_changed(ctx.id, DownloadStatus::Downloading);

    let mut command = config.command(ctx);
    command
        .args(["--no-warnings", "--no-playlist", "--newline", "--progress"])
        .args(["--print", "after_move:filepath"])
        .arg("--progress-template")
        .arg(format!(
            "download:{PROGRESS_MARK} %(progress.downloaded_bytes)s %(progress.total_bytes)s \
             %(progress.total_bytes_estimate)s %(progress.speed)s"
        ))
        .arg("-f")
        .arg(delegate_selector(&formats, &ctx.request))
        .arg("--merge-output-format")
        .arg(&media.ext)
        .arg("-o")
        // `%` would start a yt-dlp template field.
        .arg(paths.part_path.to_string_lossy().replace('%', "%%"));
    if let Some(limit) = ctx.request.speed_limit {
        command.arg("--limit-rate").arg(limit.to_string());
    }
    command.arg("--").arg(page.as_str());
    let mut child = spawn(ctx, command)?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");

    let follow = async {
        let mut lines = BufReader::new(stdout).lines();
        let mut written = None;
        while let Ok(Some(line)) = lines.next_line().await {
            match line.strip_prefix(PROGRESS_MARK) {
                Some(numbers) => report(ctx, numbers),
                None if !line.trim().is_empty() => written = Some(PathBuf::from(line.trim())),
                None => {}
            }
        }
        let errors = read_limited(stderr).await.unwrap_or_default();
        (written, errors)
    };
    let (written, errors) = or_cancel(&ctx.cancel, follow).await?;
    let status = child
        .wait()
        .await
        .map_err(|e| EngineError::Extractor(format!("yt-dlp: {e}")))?;
    let written = match (status.success(), written) {
        (true, Some(written)) => written,
        _ => return Err(EngineError::Extractor(last_error(&errors))),
    };
    std::fs::rename(&written, &paths.final_path)
        .map_err(|e| EngineError::io(&paths.final_path, e))?;
    Ok(paths.final_path)
}

/// Reads a pipe to its end, keeping at most [`MAX_ANSWER`] bytes.
async fn read_limited(pipe: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut data = Vec::new();
    pipe.take(MAX_ANSWER).read_to_end(&mut data).await?;
    Ok(data)
}

/// `downloaded total estimate speed`, each a number or `NA`.
fn report(ctx: &TaskContext, numbers: &str) {
    let mut fields = numbers
        .split_whitespace()
        .map(|field| field.parse::<f64>().ok().map(|n| n as u64));
    let downloaded = fields.next().flatten();
    let total = fields.next().flatten();
    let estimate = fields.next().flatten();
    let speed = fields.next().flatten();
    ctx.progress.update(|p| {
        if let Some(downloaded) = downloaded {
            p.downloaded = downloaded;
        }
        p.total_size = total.or(estimate);
        p.speed = speed.unwrap_or(0);
        p.connections = 1;
    });
    ctx.observer.progress(ctx.id);
}

fn spawn(ctx: &TaskContext, mut command: Command) -> Result<tokio::process::Child> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Stopping the download stops yt-dlp with it.
        .kill_on_drop(true);
    command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            EngineError::Extractor(format!(
                "`{}` is not installed; install yt-dlp to download from video sites",
                ctx.shared.config.yt_dlp.display()
            ))
        } else {
            EngineError::Extractor(format!("could not start yt-dlp: {error}"))
        }
    })
}

/// The last `ERROR:` line, or the last line, kept short.
fn last_error(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .rev()
        .find(|line| line.starts_with("ERROR:"))
        .or_else(|| text.lines().rev().find(|line| !line.trim().is_empty()))
        .unwrap_or("yt-dlp failed without saying why");
    let line = line.trim_start_matches("ERROR:").trim();
    line.chars().take(300).collect()
}

/// A yt-dlp config file only we can read, removed when dropped. It
/// holds the request headers and the proxy.
struct ConfigFile {
    path: PathBuf,
}

impl ConfigFile {
    fn write(ctx: &TaskContext, cookies: bool) -> Result<Self> {
        let mut lines = Vec::new();
        if let Some(proxy) = &ctx.shared.config.proxy {
            lines.push(format!("--proxy {}", quote(proxy)));
        }
        for (name, value) in &ctx.request.headers {
            let value = String::from_utf8_lossy(value.as_bytes());
            lines.push(format!(
                "--add-header {}",
                quote(&format!("{name}:{value}"))
            ));
        }
        // yt-dlp reads the page itself, so a page behind a login needs
        // the user's cookies.
        if let Some(cookies) = ctx.request.own_cookies.as_ref().filter(|_| cookies) {
            lines.push(format!(
                "--add-header {}",
                quote(&format!("Cookie:{cookies}"))
            ));
        }
        let unique = format!(
            "hiroi-yt-dlp-{}-{}.conf",
            std::process::id(),
            fastrand::u64(..)
        );
        let path = std::env::temp_dir().join(unique);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&path).map_err(|e| EngineError::io(&path, e))?;
        file.write_all((lines.join("\n") + "\n").as_bytes())
            .map_err(|e| EngineError::io(&path, e))?;
        Ok(Self { path })
    }

    fn command(&self, ctx: &TaskContext) -> Command {
        let mut command = Command::new(&ctx.shared.config.yt_dlp);
        // --ignore-config skips the user's own yt-dlp settings, and this
        // file is read in their place.
        command
            .arg("--ignore-config")
            .arg("--config-locations")
            .arg(&self.path);
        command
    }
}

impl Drop for ConfigFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Quotes a value for yt-dlp's config files, which split like a POSIX shell.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_and_joined_answers() {
        let single = br#"{"_type":"video","title":"Clip","ext":"mp4","format_id":"0",
            "url":"https://cdn.example/clip.mp4","protocol":"https",
            "http_headers":{"Referer":"https://site.example/watch"}}"#;
        let media = parse(single).unwrap();
        assert_eq!(media.title, "Clip");
        assert_eq!(media.tracks.len(), 1);
        assert!(media.tracks[0].is_ours());
        assert_eq!(
            media.tracks[0].headers,
            [(
                "Referer".to_string(),
                "https://site.example/watch".to_string()
            )]
        );

        let joined = br#"{"title":"Film","ext":"webm","is_live":false,"requested_formats":[
            {"format_id":"303","url":"https://cdn.example/v","protocol":"https","ext":"webm"},
            {"format_id":"251","url":"https://cdn.example/a","protocol":"http_dash_segments","ext":"webm"}]}"#;
        let media = parse(joined).unwrap();
        assert_eq!(media.ext, "webm");
        assert_eq!(media.tracks[1].format_id, "251");
        assert!(!media.tracks[1].is_ours());
    }

    #[test]
    fn playlists_are_refused() {
        let answer = br#"{"_type":"playlist","title":"Channel","entries":[]}"#;
        assert!(matches!(parse(answer), Err(EngineError::Extractor(_))));
    }

    #[test]
    fn config_values_are_quoted_for_a_shell_splitter() {
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn track_headers_are_yt_dlps_own() {
        let track = Track {
            format_id: "0".into(),
            url: Url::parse("https://cdn.example/v.mp4").unwrap(),
            protocol: "https".into(),
            ext: "mp4".into(),
            headers: vec![("Referer".into(), "https://site.example/watch".into())],
        };
        let headers = track_headers(&track).unwrap();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers["referer"], "https://site.example/watch");
    }

    #[test]
    fn errors_are_the_last_error_line() {
        let stderr = b"[generic] page: Downloading webpage\nERROR: Unsupported URL: x\n";
        assert_eq!(last_error(stderr), "Unsupported URL: x");
    }
}
