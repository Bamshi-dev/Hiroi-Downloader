//! Streams: HLS and DASH playlists, live or on demand, and their
//! assembly into one file.

pub(crate) mod dash;
pub(crate) mod hls;
pub(crate) mod mux;
pub(crate) mod pieces;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use reqwest::Url;

use crate::error::{EngineError, Result};
use crate::net::probe::RemoteFile;
use crate::net::{display_url, source_key};
use crate::request::Quality;
use crate::snapshot::DownloadStatus;
use crate::storage::naming::{DownloadPaths, numbered, sanitize_file_name};
use crate::task::meter::SpeedMeter;
use crate::task::{TaskContext, or_cancel};
use hls::Playlist;
use pieces::{Batch, Piece, PieceKey, PieceSource, Track, TrackProgress};

/// Largest playlist or key we read into memory.
const MAX_SMALL_BODY: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamKind {
    Hls,
    Dash,
}

/// Recognizes a stream from the probe: its Content-Type, or failing
/// that, the file extension.
pub(crate) fn detect(remote: &RemoteFile) -> Option<StreamKind> {
    let content_type = remote
        .content_type
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase();
    if content_type.contains("mpegurl") {
        return Some(StreamKind::Hls);
    }
    if content_type.contains("dash+xml") {
        return Some(StreamKind::Dash);
    }
    let path = remote.final_url.path().to_ascii_lowercase();
    if path.ends_with(".m3u8") {
        Some(StreamKind::Hls)
    } else if path.ends_with(".mpd") {
        Some(StreamKind::Dash)
    } else {
        None
    }
}

pub(crate) async fn download(
    ctx: &Arc<TaskContext>,
    url: Url,
    kind: StreamKind,
) -> Result<PathBuf> {
    match kind {
        StreamKind::Hls => download_hls(ctx, url).await,
        StreamKind::Dash => download_dash(ctx, url).await,
    }
}

async fn download_dash(ctx: &Arc<TaskContext>, url: Url) -> Result<PathBuf> {
    let text = fetch_text(ctx, &url).await?;
    let manifest = dash::parse(&text, &url, ctx.request.stream.quality, None)
        .map_err(EngineError::Protocol)?;
    let want = (
        manifest
            .video
            .as_ref()
            .map(|t| t.id.clone())
            .unwrap_or_default(),
        manifest
            .audio
            .as_ref()
            .map(|t| t.id.clone())
            .unwrap_or_default(),
    );
    let live = manifest.live;
    let mp4 = manifest
        .video
        .as_ref()
        .or(manifest.audio.as_ref())
        .is_none_or(|t| t.mp4);
    let extension = if mp4 { "mp4" } else { "ts" };
    let paths = plan_stream(ctx, &url, extension)?;
    report_start(ctx, &paths, live);
    let progress = Arc::new(TrackProgress::default());
    let source = |video: bool, first: Option<dash::TrackPlan>| DashSource {
        ctx,
        url: url.clone(),
        video,
        want: want.clone(),
        first,
        last: None,
        init_sent: false,
        live,
        period: manifest.update_period,
    };

    let result = match (manifest.video.clone(), manifest.audio.clone()) {
        (Some(video), Some(audio)) => {
            let video_track = track(ctx, &paths, "video", extension, &url, live, &progress);
            let audio_track = track(ctx, &paths, "audio", extension, &url, live, &progress);
            let both = async {
                tokio::try_join!(
                    video_track.run(source(true, Some(video))),
                    audio_track.run(source(false, Some(audio)))
                )
            };
            reporting(ctx, &progress, both).await?;
            mux::combine(&video_track.output, &audio_track.output, &paths.final_path).await
        }
        (video, audio) => {
            let is_video = video.is_some();
            let only = track(
                ctx,
                &paths,
                if is_video { "video" } else { "audio" },
                extension,
                &url,
                live,
                &progress,
            );
            reporting(ctx, &progress, only.run(source(is_video, video.or(audio)))).await?;
            finalize(ctx, &only.output, &paths)
        }
    };
    result.map(|()| paths.final_path.clone())
}

/// One track of a DASH manifest; live manifests are reloaded and only
/// segments newer than the last one handed out are passed on.
struct DashSource<'a> {
    ctx: &'a Arc<TaskContext>,
    url: Url,
    video: bool,
    want: (String, String),
    first: Option<dash::TrackPlan>,
    last: Option<u64>,
    init_sent: bool,
    live: bool,
    period: f64,
}

impl PieceSource for DashSource<'_> {
    async fn next_batch(&mut self) -> Result<Batch> {
        let plan = match self.first.take() {
            Some(plan) => plan,
            None => {
                let text = fetch_text(self.ctx, &self.url).await?;
                let want = Some((self.want.0.as_str(), self.want.1.as_str()));
                let manifest = dash::parse(&text, &self.url, Quality::Best, want)
                    .map_err(EngineError::Protocol)?;
                self.live = manifest.live;
                let plan = if self.video {
                    manifest.video
                } else {
                    manifest.audio
                };
                plan.ok_or_else(|| {
                    EngineError::Protocol("the chosen track disappeared from the manifest".into())
                })?
            }
        };
        let mut pieces = Vec::new();
        if !self.init_sent
            && let Some(init) = plan.init
        {
            pieces.push(init);
            self.init_sent = true;
        }
        for (position, piece) in plan.segments {
            if self.last.is_none_or(|last| position > last) {
                pieces.push(piece);
                self.last = Some(position);
            }
        }
        let wait = Duration::from_secs_f64(self.period.clamp(0.5, 30.0));
        Ok(Batch {
            pieces,
            finished: !self.live,
            wait,
        })
    }
}

async fn download_hls(ctx: &Arc<TaskContext>, url: Url) -> Result<PathBuf> {
    let text = fetch_text(ctx, &url).await?;
    let playlist = hls::parse(&text, &url).map_err(EngineError::Protocol)?;
    let quality = ctx.request.stream.quality;
    let (video_url, audio_url) = match playlist {
        Playlist::Media(_) => (url.clone(), None),
        Playlist::Master(master) => {
            let variant = choose(&master.variants, quality)
                .ok_or(EngineError::Protocol("no variant".into()))?;
            let audio = variant.audio_group.as_ref().and_then(|group| {
                let renditions: Vec<_> = master
                    .audio
                    .iter()
                    .filter(|r| &r.group == group && r.uri.is_some())
                    .collect();
                renditions
                    .iter()
                    .find(|r| r.default)
                    .or(renditions.first())
                    .and_then(|r| r.uri.clone())
            });
            tracing::info!(variant = %display_url(&variant.uri), height = ?variant.height, "chose a variant");
            (variant.uri.clone(), audio)
        }
    };

    let mut video = HlsSource::new(ctx, video_url.clone());
    let first = video.peek().await?;
    let live = !first.ended;
    let extension = if first.segments.iter().any(|s| s.map.is_some()) {
        "mp4"
    } else {
        "ts"
    };
    let combined = if audio_url.is_some() {
        "mp4"
    } else {
        extension
    };
    let paths = plan_stream(ctx, &url, combined)?;
    report_start(ctx, &paths, live);

    let progress = Arc::new(TrackProgress::default());
    let video_track = track(ctx, &paths, "video", extension, &url, live, &progress);
    let result = match audio_url {
        None => {
            reporting(ctx, &progress, video_track.run(video)).await?;
            finalize(ctx, &video_track.output, &paths)
        }
        Some(audio_url) => {
            let audio = HlsSource::new(ctx, audio_url);
            let audio_track = track(ctx, &paths, "audio", extension, &url, live, &progress);
            let both = async { tokio::try_join!(video_track.run(video), audio_track.run(audio)) };
            reporting(ctx, &progress, both).await?;
            mux::combine(&video_track.output, &audio_track.output, &paths.final_path).await
        }
    };
    result.map(|()| paths.final_path.clone())
}

fn track<'a>(
    ctx: &'a Arc<TaskContext>,
    paths: &DownloadPaths,
    role: &str,
    extension: &str,
    source: &Url,
    live: bool,
    progress: &Arc<TrackProgress>,
) -> Track<'a> {
    let work = paths.part_path.with_extension(format!("{role}.d"));
    Track {
        ctx,
        output: paths
            .part_path
            .with_extension(format!("{role}.{extension}")),
        work,
        source_key: format!("{}#{role}", source_key(source)),
        live,
        limit: ctx.request.stream.live_limit,
        progress: Arc::clone(progress),
    }
}

/// Picks a variant: highest bandwidth, lowest, or the best one that is
/// no taller than a limit.
fn choose(variants: &[hls::Variant], quality: Quality) -> Option<&hls::Variant> {
    match quality {
        Quality::Best => variants.iter().max_by_key(|v| (v.bandwidth, v.height)),
        Quality::Worst => variants.iter().min_by_key(|v| (v.bandwidth, v.height)),
        Quality::MaxHeight(limit) => variants
            .iter()
            .filter(|v| v.height.is_none_or(|h| h <= limit))
            .max_by_key(|v| (v.bandwidth, v.height))
            .or_else(|| variants.iter().min_by_key(|v| v.bandwidth)),
    }
}

/// Follows a media playlist. On demand: one batch with everything. Live:
/// the new segments each time, asked again about once per target
/// duration.
struct HlsSource<'a> {
    ctx: &'a Arc<TaskContext>,
    url: Url,
    first: Option<hls::MediaPlaylist>,
    last_sequence: Option<u64>,
    init_sent: Option<hls::Init>,
}

impl<'a> HlsSource<'a> {
    fn new(ctx: &'a Arc<TaskContext>, url: Url) -> Self {
        Self {
            ctx,
            url,
            first: None,
            last_sequence: None,
            init_sent: None,
        }
    }

    async fn fetch(&self) -> Result<hls::MediaPlaylist> {
        let text = fetch_text(self.ctx, &self.url).await?;
        match hls::parse(&text, &self.url).map_err(EngineError::Protocol)? {
            Playlist::Media(media) => Ok(media),
            Playlist::Master(_) => Err(EngineError::Protocol("expected a media playlist".into())),
        }
    }

    async fn peek(&mut self) -> Result<hls::MediaPlaylist> {
        let playlist = self.fetch().await?;
        self.first = Some(playlist.clone());
        Ok(playlist)
    }
}

impl PieceSource for HlsSource<'_> {
    async fn next_batch(&mut self) -> Result<Batch> {
        let playlist = match self.first.take() {
            Some(playlist) => playlist,
            None => self.fetch().await?,
        };
        let mut pieces = Vec::new();
        let last = self.last_sequence;
        let fresh = playlist
            .segments
            .iter()
            .filter(|s| last.is_none_or(|last| s.sequence > last));
        for segment in fresh {
            // The initialization section goes in front, once per change.
            if let Some(map) = &segment.map
                && self.init_sent.as_ref() != Some(map)
            {
                pieces.push(Piece {
                    url: map.uri.clone(),
                    range: map.range,
                    key: None,
                });
                self.init_sent = Some(map.clone());
            }
            let key = segment.key.as_ref().map(|key| PieceKey {
                uri: key.uri.clone(),
                iv: key
                    .iv
                    .unwrap_or_else(|| pieces::sequence_iv(segment.sequence)),
            });
            pieces.push(Piece {
                url: segment.uri.clone(),
                range: segment.range,
                key,
            });
            self.last_sequence = Some(segment.sequence);
        }
        // RFC 8216: wait a target duration, or half of it after a reload
        // that brought nothing new.
        let factor = if pieces.is_empty() { 0.5 } else { 1.0 };
        let wait = Duration::from_secs_f64((playlist.target_duration * factor).clamp(0.2, 30.0));
        Ok(Batch {
            pieces,
            finished: playlist.ended,
            wait,
        })
    }
}

/// Final name for a stream: the request's name, else one from the URL
/// (a generic `index.m3u8` takes its folder's name).
fn plan_stream(ctx: &TaskContext, url: &Url, extension: &str) -> Result<DownloadPaths> {
    let request = &ctx.request;
    let dir = request.output_dir.as_path();
    std::fs::create_dir_all(dir).map_err(|e| EngineError::io(dir, e))?;
    let name = request
        .file_name
        .clone()
        .or_else(|| stream_name(url).map(|stem| format!("{stem}.{extension}")))
        .and_then(|name| sanitize_file_name(&name))
        .unwrap_or_else(|| format!("stream.{extension}"));
    let paths = DownloadPaths::new(dir, &name);
    if !paths.final_path.exists() {
        return Ok(paths);
    }
    (1..10_000)
        .map(|n| DownloadPaths::new(dir, &numbered(&name, n)))
        .find(|paths| !paths.final_path.exists())
        .ok_or_else(|| EngineError::AlreadyExists(dir.join(&name)))
}

fn stream_name(url: &Url) -> Option<String> {
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let last = segments.last()?;
    let stem = last.rsplit_once('.').map_or(*last, |(stem, _)| stem);
    let generic = [
        "index",
        "master",
        "playlist",
        "manifest",
        "main",
        "chunklist",
        "stream",
    ];
    if generic.iter().any(|g| stem.eq_ignore_ascii_case(g)) && segments.len() >= 2 {
        Some(segments[segments.len() - 2].to_string())
    } else {
        Some(stem.to_string())
    }
}

fn report_start(ctx: &TaskContext, paths: &DownloadPaths, live: bool) {
    ctx.progress.update(|p| {
        p.paths = Some(paths.clone());
        p.total_size = None;
        p.resumable = !live;
    });
    ctx.observer
        .status_changed(ctx.id, DownloadStatus::Downloading);
}

/// Runs `work` while sending progress updates.
async fn reporting<T>(
    ctx: &TaskContext,
    progress: &TrackProgress,
    work: impl Future<Output = Result<T>>,
) -> Result<T> {
    let mut meter = SpeedMeter::new(progress.bytes.load(Ordering::Relaxed));
    let mut tick = tokio::time::interval(ctx.shared.config.progress_interval);
    tokio::pin!(work);
    loop {
        tokio::select! {
            result = &mut work => return result,
            _ = tick.tick() => {
                let downloaded = progress.bytes.load(Ordering::Relaxed);
                let speed = meter.sample(downloaded);
                ctx.progress.update(|p| {
                    p.downloaded = downloaded;
                    p.speed = speed;
                    p.connections = ctx.shared.config.default_connections.min(16);
                });
                ctx.observer.progress(ctx.id);
            }
        }
    }
}

fn finalize(ctx: &TaskContext, output: &Path, paths: &DownloadPaths) -> Result<()> {
    let overwrite = ctx.request.conflict == crate::request::ConflictPolicy::Overwrite;
    crate::storage::file::finalize(output, &paths.final_path, overwrite)
}

/// GETs a small resource (a playlist, a key) into memory, with retries.
pub(crate) async fn fetch_small(ctx: &TaskContext, url: &Url, limit: usize) -> Result<Vec<u8>> {
    let policy = &ctx.shared.config.retry;
    let mut attempt = 0;
    loop {
        let error = match fetch_small_once(ctx, url, limit).await {
            Ok(body) => return Ok(body),
            Err(error) => error,
        };
        attempt += 1;
        match policy.action(&error, attempt, true) {
            crate::net::retry::RetryAction::Retry(delay) => {
                ctx.observer.retrying(ctx.id, &error.to_string(), delay);
                or_cancel(&ctx.cancel, tokio::time::sleep(delay)).await?;
            }
            _ => return Err(error.into_engine_error(&display_url(url))),
        }
    }
}

async fn fetch_small_once(
    ctx: &TaskContext,
    url: &Url,
    limit: usize,
) -> crate::net::retry::FetchResult<Vec<u8>> {
    use crate::net::retry::FetchError;
    use futures_util::StreamExt;
    let gate = ctx.shared.hosts.gate(&crate::net::host_key(url));
    let _host = or_cancel(&ctx.cancel, gate.acquire()).await?;
    let request = ctx
        .shared
        .client
        .get(url.clone())
        .headers(ctx.request.headers.clone());
    let response = or_cancel(&ctx.cancel, request.send())
        .await?
        .map_err(FetchError::Transport)?;
    let status = response.status();
    if !status.is_success() {
        let retry_after = crate::net::probe::retry_after(&response);
        if crate::net::retry::is_throttle(status) {
            gate.throttle(retry_after.unwrap_or(crate::constants::DEFAULT_THROTTLE_PAUSE));
        }
        return Err(FetchError::Status {
            status,
            retry_after,
        });
    }
    let mut body = response.bytes_stream();
    let mut data = Vec::new();
    while let Some(chunk) = or_cancel(&ctx.cancel, body.next()).await? {
        data.extend_from_slice(&chunk.map_err(FetchError::Transport)?);
        if data.len() > limit {
            return Err(FetchError::Protocol(format!(
                "{} is larger than {limit} bytes",
                display_url(url)
            )));
        }
    }
    Ok(data)
}

async fn fetch_text(ctx: &TaskContext, url: &Url) -> Result<String> {
    let bytes = fetch_small(ctx, url, MAX_SMALL_BODY).await?;
    String::from_utf8(bytes).map_err(|_| EngineError::Protocol("playlist is not UTF-8 text".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_playlist_names_take_their_folder() {
        let name = |url: &str| stream_name(&Url::parse(url).unwrap());
        assert_eq!(
            name("https://cdn.example/shows/episode-3/index.m3u8").as_deref(),
            Some("episode-3")
        );
        assert_eq!(
            name("https://cdn.example/talk.m3u8").as_deref(),
            Some("talk")
        );
    }

    #[test]
    fn quality_choices() {
        let variant = |bandwidth, height| hls::Variant {
            uri: Url::parse("https://e.com/v.m3u8").unwrap(),
            bandwidth,
            height: Some(height),
            audio_group: None,
        };
        let variants = [
            variant(800_000, 360),
            variant(5_000_000, 1080),
            variant(2_500_000, 720),
        ];
        assert_eq!(choose(&variants, Quality::Best).unwrap().height, Some(1080));
        assert_eq!(choose(&variants, Quality::Worst).unwrap().height, Some(360));
        assert_eq!(
            choose(&variants, Quality::MaxHeight(720)).unwrap().height,
            Some(720)
        );
        assert_eq!(
            choose(&variants, Quality::MaxHeight(100)).unwrap().height,
            Some(360)
        );
    }
}
