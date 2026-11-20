//! Everything that talks to servers.

pub(crate) mod client;
pub(crate) mod cookies;
#[cfg(feature = "ftp")]
pub(crate) mod ftp;
pub(crate) mod guard;
pub(crate) mod headers;
pub mod ipfilter;
pub(crate) mod probe;
pub(crate) mod protocol;
pub mod retry;
#[cfg(feature = "sftp")]
pub(crate) mod sftp;
pub(crate) mod source;
pub(crate) mod tls;
#[cfg(any(feature = "ftp", feature = "sftp"))]
pub(crate) mod tunnel;

use reqwest::Url;

/// URL for logs and the UI: no credentials, no query string (signed links
/// carry their secrets there).
pub(crate) fn display_url(url: &Url) -> String {
    let mut shown = source_key(url);
    if url.query().is_some() {
        shown.push_str("?…");
    }
    shown
}

/// Identifies "the same file" across runs. Credentials and the query string
/// are left out: they are secrets and change every time a link is re-signed.
pub(crate) fn source_key(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{}://{host}:{port}{}", url.scheme(), url.path()),
        None => format!("{}://{host}{}", url.scheme(), url.path()),
    }
}

/// Connection limits are tracked per `host:port`.
pub(crate) fn host_key(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    format!("{host}:{}", url.port_or_known_default().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_url_hides_credentials_and_query() {
        let url = Url::parse("https://user:secret@cdn.example.com:8443/f.iso?token=abc").unwrap();
        assert_eq!(display_url(&url), "https://cdn.example.com:8443/f.iso?…");
        assert_eq!(source_key(&url), "https://cdn.example.com:8443/f.iso");
        assert_eq!(host_key(&url), "cdn.example.com:8443");
    }
}
