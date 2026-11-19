//! `hiroi clipboard`: offer copied download links to the service.
//!
//! There is no portable clipboard API on Linux, so we ask the tools the
//! desktop already has: `wl-paste` on Wayland, `xclip` or `xsel` on X11.
//! Asking once a second costs a short-lived process per second and
//! nothing in between; a link is only offered when it changes.

use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Result, bail};
use hiroi_daemon::Client;
use hiroi_proto::{NewJob, Request, Response};

#[cfg(not(any(target_os = "macos", windows)))]
const READERS: &[&[&str]] = &[
    &["wl-paste", "--no-newline", "--type", "text"],
    &["xclip", "-out", "-selection", "clipboard"],
    &["xsel", "--clipboard", "--output"],
];
#[cfg(target_os = "macos")]
const READERS: &[&[&str]] = &[&["pbpaste"]];
#[cfg(windows)]
const READERS: &[&[&str]] = &[&["powershell", "-NoProfile", "-Command", "Get-Clipboard"]];

/// Copied text larger than this is not a link.
const MAX_TEXT: usize = 8 * 1024;

pub async fn run(client: &mut Client, auto: bool, interval: Duration) -> Result<ExitCode> {
    let Some(reader) = find_reader().await else {
        bail!("no clipboard tool found; install wl-clipboard (Wayland) or xclip (X11)");
    };
    let extensions = known_extensions();
    eprintln!("Watching the clipboard with {}. Ctrl+C to stop.", reader[0]);
    let mut last = read(reader).await.unwrap_or_default();
    loop {
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            _ = tokio::signal::ctrl_c() => return Ok(ExitCode::SUCCESS),
        }
        let Some(text) = read(reader).await else {
            continue;
        };
        if text == last {
            continue;
        }
        last.clone_from(&text);
        let Some(url) = downloadable_link(&text, &extensions) else {
            continue;
        };
        if !auto && !confirm(&url)? {
            continue;
        }
        match client
            .call(Request::Add(Box::new(NewJob {
                url: url.clone(),
                ..NewJob::default()
            })))
            .await
        {
            Ok(Response::Added { job }) => eprintln!("Added job {job}: {url}"),
            Ok(_) => {}
            Err(error) => eprintln!("Not added: {error}"),
        }
    }
}

/// A single http(s) URL whose path ends in a downloadable extension.
pub fn downloadable_link(text: &str, extensions: &[String]) -> Option<String> {
    let text = text.trim();
    if text.is_empty() || text.len() > MAX_TEXT || text.contains(char::is_whitespace) {
        return None;
    }
    let url = hiroi_engine::Url::parse(text).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let name = url.path_segments()?.rfind(|segment| !segment.is_empty())?;
    let (_, extension) = name.rsplit_once('.')?;
    extensions
        .contains(&extension.to_ascii_lowercase())
        .then(|| url.to_string())
}

fn known_extensions() -> Vec<String> {
    let mut extensions: Vec<String> = hiroi_daemon::category::defaults()
        .categories
        .into_iter()
        .flat_map(|category| category.extensions)
        .collect();
    extensions.extend(["torrent", "meta4", "metalink", "m3u8", "mpd"].map(String::from));
    extensions
}

async fn find_reader() -> Option<&'static [&'static str]> {
    for reader in READERS {
        if read(reader).await.is_some() {
            return Some(reader);
        }
    }
    None
}

async fn read(argv: &[&str]) -> Option<String> {
    let output = tokio::process::Command::new(argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn confirm(url: &str) -> Result<bool> {
    eprint!("Download {url}? [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_single_download_links_count() {
        let extensions = known_extensions();
        let link = |text| downloadable_link(text, &extensions);
        assert!(link("https://example.com/ubuntu.iso").is_some());
        assert!(link("  https://example.com/a/clip.MP4?x=1\n").is_some());
        assert!(link("https://example.com/article").is_none());
        assert!(link("https://example.com/").is_none());
        assert!(link("see https://example.com/a.zip").is_none());
        assert!(link("file:///home/me/a.zip").is_none());
        assert!(link("hello world").is_none());
    }
}
