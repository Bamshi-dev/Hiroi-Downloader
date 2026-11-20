use std::path::{Path, PathBuf};

use reqwest::Url;
use reqwest::header::{COOKIE, HeaderMap, HeaderName, HeaderValue, REFERER, USER_AGENT};

use crate::checksum::Checksum;
use crate::constants::MAX_CONNECTIONS;
use crate::error::{EngineError, Result};

/// What to do when the target file already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConflictPolicy {
    /// Save as `name (1).ext`, `name (2).ext`, ...
    #[default]
    Rename,
    Overwrite,
    /// Fail with [`EngineError::AlreadyExists`].
    Skip,
}

/// Which variant of a stream to download, when it offers several.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Quality {
    #[default]
    Best,
    Worst,
    /// The best variant no taller than this many pixels.
    MaxHeight(u32),
}

impl std::str::FromStr for Quality {
    type Err = String;

    /// `best`, `worst`, or a height such as `720` or `720p`.
    fn from_str(text: &str) -> std::result::Result<Self, String> {
        match text.trim().to_ascii_lowercase().as_str() {
            "best" => Ok(Self::Best),
            "worst" => Ok(Self::Worst),
            other => other
                .strip_suffix('p')
                .unwrap_or(other)
                .parse()
                .ok()
                .filter(|&height| height > 0)
                .map(Self::MaxHeight)
                .ok_or_else(|| format!("`{text}` is not a quality (try best, worst, or 720)")),
        }
    }
}

/// Options for HLS and DASH streams.
#[derive(Debug, Clone, Default)]
pub struct StreamOptions {
    pub quality: Quality,
    /// Stop recording a live stream after this long.
    pub live_limit: Option<std::time::Duration>,
    /// The URL is a page for yt-dlp to find the media on.
    pub extract: bool,
    /// yt-dlp format selection, such as `bv*+ba/b`; implies `extract`.
    pub format: Option<String>,
}

/// Queue order: higher priorities start first, ties keep insertion order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Priority {
    Low,
    #[default]
    Normal,
    High,
}

/// Everything the engine needs to know about one download.
///
/// Built with chained setters:
///
/// ```
/// # fn main() -> hiroi_engine::Result<()> {
/// use hiroi_engine::DownloadRequest;
///
/// let request = DownloadRequest::new("https://example.com/big.iso")?
///     .mirror("https://mirror.example.org/big.iso")?
///     .connections(16)
///     .speed_limit(Some(5 * 1024 * 1024))
///     .referer("https://example.com/downloads")?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct DownloadRequest {
    pub(crate) sources: Vec<Url>,
    pub(crate) output_dir: PathBuf,
    pub(crate) file_name: Option<String>,
    pub(crate) connections: Option<usize>,
    pub(crate) headers: HeaderMap,
    pub(crate) speed_limit: Option<u64>,
    pub(crate) checksum: Option<Checksum>,
    pub(crate) conflict: ConflictPolicy,
    pub(crate) priority: Priority,
    pub(crate) stream: StreamOptions,
    /// Set on the downloads one download turns into (the files of a
    /// Metalink, the tracks of a video page), so they are not followed
    /// again.
    pub(crate) derived: bool,
    /// The user's Cookie header, taken out of `headers` when the download
    /// starts; the client sends it to its site only. Kept for yt-dlp,
    /// which reads the page itself.
    pub(crate) own_cookies: Option<String>,
    /// Hashes of every piece, from a Metalink file.
    pub(crate) pieces: Option<std::sync::Arc<crate::checksum::PieceHashes>>,
}

impl DownloadRequest {
    pub fn new(url: &str) -> Result<Self> {
        Ok(Self {
            sources: vec![parse_url(url)?],
            output_dir: PathBuf::from("."),
            file_name: None,
            connections: None,
            headers: HeaderMap::new(),
            speed_limit: None,
            checksum: None,
            conflict: ConflictPolicy::default(),
            priority: Priority::default(),
            stream: StreamOptions::default(),
            derived: false,
            own_cookies: None,
            pieces: None,
        })
    }

    /// Adds another server that hosts the same file. Mirrors that report a
    /// different size are ignored when the download starts.
    pub fn mirror(mut self, url: &str) -> Result<Self> {
        let url = parse_url(url)?;
        if !self.sources.contains(&url) {
            self.sources.push(url);
        }
        Ok(self)
    }

    pub fn output_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.output_dir = dir.as_ref().to_path_buf();
        self
    }

    /// Overrides the name suggested by the server. The name is sanitized, so
    /// it can never point outside the output directory.
    pub fn file_name(mut self, name: impl Into<String>) -> Self {
        self.file_name = Some(name.into());
        self
    }

    /// Connections for this download, clamped to `1..=32`.
    pub fn connections(mut self, connections: usize) -> Self {
        self.connections = Some(connections.clamp(1, MAX_CONNECTIONS));
        self
    }

    pub fn header(mut self, name: &str, value: &str) -> Result<Self> {
        let name = HeaderName::from_bytes(name.trim().as_bytes())
            .map_err(|_| EngineError::InvalidHeader(format!("bad name `{name}`")))?;
        let mut value = HeaderValue::from_str(value.trim())
            .map_err(|_| EngineError::InvalidHeader(format!("bad value for `{name}`")))?;
        if is_sensitive(&name) {
            value.set_sensitive(true);
        }
        self.headers.insert(name, value);
        Ok(self)
    }

    /// Cookies as sent by a browser: `name=value; other=value`.
    pub fn cookies(self, cookies: &str) -> Result<Self> {
        self.header(COOKIE.as_str(), cookies)
    }

    pub fn referer(self, referer: &str) -> Result<Self> {
        self.header(REFERER.as_str(), referer)
    }

    /// Overrides the engine's user agent for this download only.
    pub fn user_agent(self, user_agent: &str) -> Result<Self> {
        self.header(USER_AGENT.as_str(), user_agent)
    }

    /// Speed limit in bytes per second, `None` for unlimited.
    pub fn speed_limit(mut self, limit: Option<u64>) -> Self {
        self.speed_limit = limit.filter(|&limit| limit > 0);
        self
    }

    pub fn checksum(mut self, checksum: Checksum) -> Self {
        self.checksum = Some(checksum);
        self
    }

    pub fn on_conflict(mut self, policy: ConflictPolicy) -> Self {
        self.conflict = policy;
        self
    }

    pub fn priority(mut self, priority: Priority) -> Self {
        self.priority = priority;
        self
    }

    /// Variant to pick from streams that offer several.
    pub fn quality(mut self, quality: Quality) -> Self {
        self.stream.quality = quality;
        self
    }

    /// Let yt-dlp find the media on the page at the URL.
    pub fn extract(mut self, extract: bool) -> Self {
        self.stream.extract = extract;
        self
    }

    /// Ask yt-dlp for these formats instead of choosing by quality.
    pub fn format(mut self, format: Option<String>) -> Self {
        self.stream.extract |= format.is_some();
        self.stream.format = format;
        self
    }

    /// Stop recording a live stream after `limit`.
    pub fn live_limit(mut self, limit: Option<std::time::Duration>) -> Self {
        self.stream.live_limit = limit;
        self
    }

    pub fn url(&self) -> &Url {
        &self.sources[0]
    }
}

/// URL schemes this build can download.
const SCHEMES: &[&str] = &[
    "http",
    "https",
    #[cfg(feature = "ftp")]
    "ftp",
    #[cfg(feature = "ftp")]
    "ftps",
    #[cfg(feature = "sftp")]
    "sftp",
    #[cfg(feature = "torrent")]
    "magnet",
];

pub(crate) fn is_supported(scheme: &str) -> bool {
    SCHEMES.contains(&scheme)
}

/// `http and https`, or `http, https, ftp, and ftps`, for messages.
pub(crate) fn scheme_list() -> String {
    match SCHEMES {
        [] => String::new(),
        [only] => only.to_string(),
        [first, second] => format!("{first} and {second}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

fn parse_url(url: &str) -> Result<Url> {
    let parsed = Url::parse(url.trim()).map_err(|_| EngineError::InvalidUrl(url.to_string()))?;
    match parsed.scheme() {
        scheme if !is_supported(scheme) => {
            Err(EngineError::UnsupportedScheme(parsed.scheme().to_string()))
        }
        // A magnet link names a torrent, not a server.
        "magnet" => Ok(parsed),
        _ if parsed.host().is_none() => Err(EngineError::InvalidUrl(url.to_string())),
        _ => Ok(parsed),
    }
}

/// Headers that must never show up in logs or debug output.
fn is_sensitive(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "authorization" | "cookie" | "proxy-authorization"
    ) || name.as_str().contains("token")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualities_parse_from_text() {
        assert_eq!("best".parse(), Ok(Quality::Best));
        assert_eq!("Worst".parse(), Ok(Quality::Worst));
        assert_eq!("720p".parse(), Ok(Quality::MaxHeight(720)));
        assert!("0".parse::<Quality>().is_err());
        assert!("huge".parse::<Quality>().is_err());
    }

    #[test]
    fn accepts_only_http_and_https() {
        assert!(DownloadRequest::new("https://example.com/a.iso").is_ok());
        assert!(matches!(
            DownloadRequest::new("file:///etc/passwd"),
            Err(EngineError::UnsupportedScheme(_))
        ));
        assert!(matches!(
            DownloadRequest::new("not a url"),
            Err(EngineError::InvalidUrl(_))
        ));
    }

    #[test]
    fn marks_cookies_sensitive_and_rejects_header_injection() {
        let request = DownloadRequest::new("https://example.com/a")
            .unwrap()
            .cookies("a=b")
            .unwrap();
        assert!(request.headers[COOKIE].is_sensitive());
        assert!(
            request
                .clone()
                .header("X-Test", "a\r\nInjected: 1")
                .is_err()
        );
    }

    #[test]
    fn ignores_duplicate_mirrors_and_clamps_connections() {
        let request = DownloadRequest::new("https://a.example/f")
            .unwrap()
            .mirror("https://a.example/f")
            .unwrap()
            .connections(500);
        assert_eq!(request.sources.len(), 1);
        assert_eq!(request.connections, Some(MAX_CONNECTIONS));
    }
}
