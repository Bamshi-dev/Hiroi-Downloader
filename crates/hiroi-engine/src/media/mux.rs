//! Joining separate video and audio tracks with ffmpeg, when installed.
//!
//! ffmpeg copies the streams (`-c copy`), so nothing is re-encoded and
//! the cost is a quick pass over the files. Without ffmpeg both tracks
//! are kept next to each other.

use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::error::{EngineError, Result};

pub(crate) async fn combine(video: &Path, audio: &Path, target: &Path) -> Result<()> {
    let partial = target.with_extension("muxing.mp4");
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(video)
        .arg("-i")
        .arg(audio)
        .args(["-map", "0:v:0?", "-map", "1:a:0?", "-c", "copy"])
        .arg(&partial)
        .stdin(std::process::Stdio::null())
        .status()
        .await;
    match status {
        Ok(status) if status.success() => {
            std::fs::rename(&partial, target).map_err(|e| EngineError::io(target, e))?;
            let _ = std::fs::remove_file(video);
            let _ = std::fs::remove_file(audio);
            Ok(())
        }
        result => {
            let _ = std::fs::remove_file(&partial);
            match result {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    tracing::warn!(
                        "ffmpeg is not installed: keeping video and audio as separate files"
                    );
                }
                _ => tracing::warn!("ffmpeg could not join the tracks: keeping them separate"),
            }
            let video_target = with_role(target, "video", video);
            let audio_target = with_role(target, "audio", audio);
            std::fs::rename(video, &video_target).map_err(|e| EngineError::io(&video_target, e))?;
            std::fs::rename(audio, &audio_target).map_err(|e| EngineError::io(&audio_target, e))?;
            // The job's file is the video; the audio sits next to it.
            std::fs::rename(&video_target, target).map_err(|e| EngineError::io(target, e))
        }
    }
}

/// `show.mp4` + `audio` + `x.audio.aac` -> `show.audio.aac`
fn with_role(target: &Path, role: &str, original: &Path) -> PathBuf {
    let extension = original
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
    target.with_extension(format!("{role}.{extension}"))
}
