//! Leaving fewer traces: what a URL or a proxy gives away, and the
//! settings that take some of it back.
//!
//! None of this makes anyone anonymous. Every server we connect to sees
//! the address we connect from; a proxy moves that from the server to
//! the proxy's operator. What we can do is not send more than needed
//! (tracking parameters, a Referer), refuse to run without the proxy
//! when the user asked for one, and say plainly what is still visible.

use reqwest::Url;
use reqwest::header::REFERER;

use crate::config::EngineConfig;
use crate::error::{EngineError, Result};
use crate::request::DownloadRequest;

/// Query parameters added by ad networks, newsletters, and social sites
/// to follow a click. Servers do not need them to send a file.
const TRACKING_PREFIXES: &[&str] = &["utm_", "pk_", "mtm_"];
const TRACKING_NAMES: &[&str] = &[
    "fbclid",
    "gclid",
    "gclsrc",
    "dclid",
    "gbraid",
    "wbraid",
    "msclkid",
    "yclid",
    "twclid",
    "ttclid",
    "igshid",
    "mc_cid",
    "mc_eid",
    "_ga",
    "_gl",
    "_hsenc",
    "_hsmi",
    "mkt_tok",
    "oly_anon_id",
    "oly_enc_id",
    "vero_id",
    "li_fat_id",
    "s_cid",
    "ref_src",
];

pub(crate) fn is_tracking(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    TRACKING_NAMES.contains(&name.as_str())
        || TRACKING_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// Names of the tracking parameters in `url`.
pub(crate) fn tracking_params(url: &Url) -> Vec<String> {
    url.query_pairs()
        .map(|(name, _)| name.into_owned())
        .filter(|name| is_tracking(name))
        .collect()
}

/// `url` without its tracking parameters; other parameters keep their
/// order, since signed links depend on them.
pub(crate) fn strip_tracking(url: &Url) -> Url {
    if tracking_params(url).is_empty() {
        return url.clone();
    }
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(name, _)| !is_tracking(name))
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    let mut stripped = url.clone();
    if kept.is_empty() {
        stripped.set_query(None);
    } else {
        stripped.query_pairs_mut().clear().extend_pairs(kept);
    }
    stripped
}

/// What kind of proxy a URL describes, and so what it hides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyKind {
    None,
    /// `http://` or `https://`: sees the host names, and the content of
    /// plain-HTTP downloads.
    Http,
    /// `socks5://`: host names are resolved on this machine, so the
    /// DNS server learns every host we visit.
    Socks5LocalDns,
    /// `socks5h://`: the proxy resolves host names.
    Socks5RemoteDns,
    /// `socks4://` and `socks4a://`.
    Socks4,
    Unknown,
}

impl ProxyKind {
    pub fn of(proxy: Option<&str>) -> Self {
        let Some(proxy) = proxy else {
            return Self::None;
        };
        let scheme = proxy.split_once("://").map_or("", |(scheme, _)| scheme);
        match scheme.to_ascii_lowercase().as_str() {
            "http" | "https" => Self::Http,
            "socks5" => Self::Socks5LocalDns,
            "socks5h" => Self::Socks5RemoteDns,
            "socks4" | "socks4a" => Self::Socks4,
            _ => Self::Unknown,
        }
    }

    /// Whether host names are looked up on this machine.
    pub fn resolves_locally(self) -> bool {
        matches!(self, Self::None | Self::Socks5LocalDns | Self::Socks4)
    }
}

/// Refuses to build an engine without a proxy when one is required, so
/// a forgotten setting cannot quietly expose the user's address.
pub(crate) fn check_config(config: &EngineConfig) -> Result<()> {
    if config.require_proxy && config.proxy.is_none() {
        return Err(EngineError::InvalidProxy(
            "a proxy is required (require_proxy) but none is set".into(),
        ));
    }
    Ok(())
}

/// Applies the engine's privacy settings to a request before it runs.
pub(crate) fn apply(config: &EngineConfig, request: &mut DownloadRequest) {
    if config.strip_tracking {
        for url in &mut request.sources {
            let stripped = strip_tracking(url);
            if stripped != *url {
                tracing::debug!(removed = ?tracking_params(url), "removed tracking parameters");
                *url = stripped;
            }
        }
    }
    if config.no_referer {
        request.headers.remove(REFERER);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracking_parameters_go_and_the_rest_stays() {
        let url =
            Url::parse("https://cdn.example/f.iso?utm_source=x&token=abc&fbclid=1&e=2").unwrap();
        assert_eq!(tracking_params(&url), ["utm_source", "fbclid"]);
        assert_eq!(
            strip_tracking(&url).as_str(),
            "https://cdn.example/f.iso?token=abc&e=2"
        );
        let only = Url::parse("https://cdn.example/f.iso?gclid=1").unwrap();
        assert_eq!(strip_tracking(&only).as_str(), "https://cdn.example/f.iso");
        let clean = Url::parse("https://cdn.example/f.iso?a=1").unwrap();
        assert_eq!(strip_tracking(&clean), clean);
    }

    #[test]
    fn proxies_are_told_apart_by_scheme() {
        assert_eq!(ProxyKind::of(None), ProxyKind::None);
        assert_eq!(
            ProxyKind::of(Some("socks5://h:1080")),
            ProxyKind::Socks5LocalDns
        );
        assert_eq!(
            ProxyKind::of(Some("SOCKS5H://h:1080")),
            ProxyKind::Socks5RemoteDns
        );
        assert_eq!(ProxyKind::of(Some("http://h:3128")), ProxyKind::Http);
        assert!(ProxyKind::of(Some("socks5://h:1")).resolves_locally());
        assert!(!ProxyKind::of(Some("socks5h://h:1")).resolves_locally());
    }

    #[test]
    fn a_required_proxy_must_be_set() {
        let config = EngineConfig {
            require_proxy: true,
            ..EngineConfig::default()
        };
        assert!(check_config(&config).is_err());
        let config = EngineConfig {
            proxy: Some("socks5h://127.0.0.1:9050".into()),
            ..config
        };
        assert!(check_config(&config).is_ok());
    }
}
