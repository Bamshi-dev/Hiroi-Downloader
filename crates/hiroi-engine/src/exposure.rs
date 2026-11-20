//! What a download would reveal, and to whom, before it starts.
//!
//! We follow each source's redirects one hop at a time, as a download
//! would, and look at every host on the way: whether the hop is
//! encrypted, which network serves it, what it asks us to store, and
//! what we would send it. On request we also ask an address-echo
//! service which address the servers see, through the proxy and,
//! separately, without it.
//!
//! The report shows what can be observed from here. It cannot show
//! whether a server keeps logs, shares them, or reports the visit; it
//! can only list who is in a position to.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{
    AUTHORIZATION, COOKIE, HeaderMap, HeaderName, LOCATION, RANGE, REFERER, SERVER, SET_COOKIE,
    STRICT_TRANSPORT_SECURITY,
};
use reqwest::redirect::Policy;
use reqwest::{Client, Url};

use crate::config::EngineConfig;
use crate::constants::MAX_REDIRECTS;
use crate::error::{EngineError, Result};
use crate::net::client;
use crate::net::cookies::DownloadCookies;
use crate::net::display_url;
use crate::privacy::{self, ProxyKind};
use crate::request::DownloadRequest;

/// Longest answer we accept from an address-echo service.
const MAX_ECHO: usize = 256;

/// What [`crate::Engine::exposure`] checks besides the sources.
#[derive(Debug, Clone)]
pub struct ExposureOptions {
    /// Ask `ip_service` which address it sees. Off by default: it
    /// contacts a third party.
    pub check_ip: bool,
    /// Ask once more without the proxy, to compare. This shows your
    /// real address to `ip_service`.
    pub compare_direct: bool,
    /// Answers a GET with the caller's address as plain text.
    pub ip_service: String,
}

impl Default for ExposureOptions {
    fn default() -> Self {
        Self {
            check_ip: false,
            compare_direct: false,
            ip_service: "https://api.ipify.org".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExposureReport {
    pub proxy: ProxyKind,
    /// One trace per URL of the request, the main URL first.
    pub sources: Vec<SourceTrace>,
    pub public_ip: Option<IpCheck>,
    /// The conclusions, worst first.
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone)]
pub struct SourceTrace {
    /// Display form, without credentials or query values.
    pub url: String,
    pub hops: Vec<Hop>,
    /// Why the trace stopped early, if it did.
    pub error: Option<String>,
}

/// One request of a redirect chain.
#[derive(Debug, Clone)]
pub struct Hop {
    pub url: String,
    pub host: String,
    pub encrypted: bool,
    /// Addresses the host name resolves to here. Empty when the proxy
    /// resolves names, since looking them up here would leak them.
    pub addresses: Vec<IpAddr>,
    pub status: Option<u16>,
    /// The network serving the host, when its headers give it away.
    pub network: Option<&'static str>,
    pub server: Option<String>,
    /// Names of the cookies the host tried to set. We store none.
    pub cookies_set: Vec<String>,
    pub hsts: bool,
    pub tracking_params: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct IpCheck {
    /// The address the service saw, through the proxy if there is one.
    pub seen: std::result::Result<IpAddr, String>,
    /// The address it saw without the proxy, when asked for.
    pub direct: Option<std::result::Result<IpAddr, String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Something to know.
    Note,
    /// Gives away more than needed; a setting can fix it.
    Warning,
    /// Exposes the user's address or data to someone.
    Risk,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub level: Level,
    pub text: String,
}

impl Finding {
    fn new(level: Level, text: impl Into<String>) -> Self {
        Self {
            level,
            text: text.into(),
        }
    }
}

pub(crate) async fn report(
    config: &EngineConfig,
    request: &DownloadRequest,
    options: &ExposureOptions,
) -> Result<ExposureReport> {
    let proxy = ProxyKind::of(config.proxy.as_deref());
    let removed: Vec<String> = if config.strip_tracking {
        request
            .sources
            .iter()
            .flat_map(privacy::tracking_params)
            .collect()
    } else {
        Vec::new()
    };
    // Trace what a download would send, settings applied.
    let mut request = request.clone();
    privacy::apply(config, &mut request);
    // Cookies travel as they would in a download: the user's own to
    // their site only, and those servers set kept for the trace.
    let own = request
        .headers
        .remove(COOKIE)
        .and_then(|value| value.to_str().ok().map(str::to_string));
    let cookies = DownloadCookies::new(request.url(), own.as_deref(), config.cookie_jar);
    let request = &request;
    // A client that stops at every redirect, so each hop can be seen.
    let tracer = client::build_client_with(config, Policy::none(), Some(Arc::new(cookies)))?;
    let mut sources = Vec::new();
    for url in &request.sources {
        sources.push(trace(&tracer, config, request, url, proxy).await);
    }
    let public_ip = if options.check_ip {
        Some(check_ip(config, options).await?)
    } else {
        None
    };
    let mut report = ExposureReport {
        proxy,
        sources,
        public_ip,
        findings: Vec::new(),
    };
    report.findings = findings(config, request, &report);
    if own.is_some() {
        let site = request.url().host_str().unwrap_or_default();
        report.findings.push(Finding::new(
            Level::Note,
            format!("Your cookies go only to {site} and its subdomains, not to mirrors or CDNs."),
        ));
    }
    if !removed.is_empty() {
        report.findings.push(Finding::new(
            Level::Note,
            format!(
                "Tracking parameters removed from the link: {}.",
                removed.join(", ")
            ),
        ));
    }
    report
        .findings
        .sort_by_key(|finding| std::cmp::Reverse(finding.level));
    Ok(report)
}

async fn trace(
    client: &Client,
    config: &EngineConfig,
    request: &DownloadRequest,
    start: &Url,
    proxy: ProxyKind,
) -> SourceTrace {
    let mut trace = SourceTrace {
        url: display_url(start),
        hops: Vec::new(),
        error: None,
    };
    if !matches!(start.scheme(), "http" | "https") {
        trace.error = Some(format!("{} is not traced; see the notes", start.scheme()));
        return trace;
    }
    let mut url = start.clone();
    for _ in 0..=MAX_REDIRECTS {
        let mut hop = Hop {
            url: display_url(&url),
            host: url.host_str().unwrap_or_default().to_string(),
            encrypted: url.scheme() == "https",
            addresses: Vec::new(),
            status: None,
            network: None,
            server: None,
            cookies_set: Vec::new(),
            hsts: false,
            tracking_params: privacy::tracking_params(&url),
        };
        if proxy.resolves_locally() {
            let port = url.port_or_known_default().unwrap_or(443);
            if let Ok(found) = tokio::net::lookup_host((hop.host.as_str(), port)).await {
                hop.addresses = found.map(|address| address.ip()).collect();
                hop.addresses.dedup();
            }
        }
        // The first byte only, like the probe of a real download.
        let sent = client
            .get(url.clone())
            .headers(request.headers.clone())
            .header(RANGE, "bytes=0-0")
            .timeout(config.connect_timeout + config.read_timeout)
            .send()
            .await;
        let response = match sent {
            Ok(response) => response,
            Err(error) => {
                trace.error = Some(EngineError::network(error).to_string());
                trace.hops.push(hop);
                return trace;
            }
        };
        let headers = response.headers();
        hop.status = Some(response.status().as_u16());
        hop.network = network(headers);
        hop.server = text(headers, &SERVER);
        hop.hsts = headers.contains_key(STRICT_TRANSPORT_SECURITY);
        hop.cookies_set = headers
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .filter_map(|cookie| {
                cookie
                    .split_once('=')
                    .map(|(name, _)| name.trim().to_string())
            })
            .collect();
        let next = response
            .status()
            .is_redirection()
            .then(|| text(headers, &LOCATION))
            .flatten()
            .and_then(|location| url.join(&location).ok());
        trace.hops.push(hop);
        match next {
            Some(next) => url = next,
            None => return trace,
        }
    }
    trace.error = Some("too many redirects".into());
    trace
}

fn text(headers: &HeaderMap, name: &HeaderName) -> Option<String> {
    headers.get(name)?.to_str().ok().map(str::to_string)
}

/// Content delivery networks and edge services that announce
/// themselves in their headers. They see every request they serve.
fn network(headers: &HeaderMap) -> Option<&'static str> {
    let has = |name: &str| headers.contains_key(name);
    let server = text(headers, &SERVER)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let via = headers
        .get("via")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if has("cf-ray") || server.contains("cloudflare") {
        Some("Cloudflare")
    } else if has("x-amz-cf-id") || via.contains("cloudfront") {
        Some("Amazon CloudFront")
    } else if server.contains("akamai") || has("x-akamai-transformed") {
        Some("Akamai")
    } else if has("x-fastly-request-id") || via.contains("varnish") && has("x-served-by") {
        Some("Fastly")
    } else if server == "gws" || server.contains("google") || has("x-goog-generation") {
        Some("Google")
    } else if has("x-azure-ref") || has("x-msedge-ref") {
        Some("Microsoft Azure")
    } else if has("x-bunny-cache") || server.contains("bunnycdn") {
        Some("Bunny CDN")
    } else {
        None
    }
}

async fn check_ip(config: &EngineConfig, options: &ExposureOptions) -> Result<IpCheck> {
    let service = Url::parse(&options.ip_service)
        .map_err(|_| EngineError::InvalidUrl(options.ip_service.clone()))?;
    let through = client::build_client_with(config, Policy::limited(3), None)?;
    let seen = echo(&through, &service).await;
    let direct = if options.compare_direct {
        let config = EngineConfig {
            proxy: None,
            require_proxy: false,
            ..config.clone()
        };
        let direct = client::build_client_with(&config, Policy::limited(3), None)?;
        Some(echo(&direct, &service).await)
    } else {
        None
    };
    Ok(IpCheck { seen, direct })
}

async fn echo(client: &Client, service: &Url) -> std::result::Result<IpAddr, String> {
    let response = client
        .get(service.clone())
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| EngineError::network(e).to_string())?;
    if !response.status().is_success() {
        return Err(format!("the service answered {}", response.status()));
    }
    let body = response
        .bytes()
        .await
        .map_err(|e| EngineError::network(e).to_string())?;
    let body = body.get(..MAX_ECHO.min(body.len())).unwrap_or_default();
    String::from_utf8_lossy(body)
        .trim()
        .parse()
        .map_err(|_| "the service did not answer with an address".into())
}

fn findings(
    config: &EngineConfig,
    request: &DownloadRequest,
    report: &ExposureReport,
) -> Vec<Finding> {
    let mut found = Vec::new();
    let hosts: Vec<&str> = {
        let mut hosts: Vec<&str> = report
            .sources
            .iter()
            .flat_map(|source| source.hops.iter().map(|hop| hop.host.as_str()))
            .collect();
        hosts.sort_unstable();
        hosts.dedup();
        hosts
    };

    match report.proxy {
        ProxyKind::None => found.push(Finding::new(
            Level::Risk,
            format!(
                "No proxy: {} see{} your IP address.",
                list(&hosts),
                if hosts.len() == 1 { "s" } else { "" }
            ),
        )),
        ProxyKind::Socks5LocalDns => found.push(Finding::new(
            Level::Risk,
            "socks5:// looks host names up on this machine, so your DNS server learns every \
             host you download from. Use socks5h:// to let the proxy resolve them.",
        )),
        ProxyKind::Socks4 => found.push(Finding::new(
            Level::Risk,
            "SOCKS4 looks host names up on this machine; prefer socks5h://.",
        )),
        ProxyKind::Http => found.push(Finding::new(
            Level::Warning,
            "An HTTP proxy sees every host name, and the content of plain-HTTP downloads.",
        )),
        ProxyKind::Socks5RemoteDns => found.push(Finding::new(
            Level::Note,
            "The servers see the proxy's address, not yours; the proxy's operator sees both.",
        )),
        ProxyKind::Unknown => found.push(Finding::new(
            Level::Warning,
            "The proxy's scheme is not one we know.",
        )),
    }

    for source in &report.sources {
        for pair in source.hops.windows(2) {
            if pair[0].host != pair[1].host {
                found.push(Finding::new(
                    Level::Warning,
                    format!(
                        "{} sends you on to {}, another party that sees the request.",
                        pair[0].host, pair[1].host
                    ),
                ));
            }
        }
        for (index, hop) in source.hops.iter().enumerate() {
            if !hop.encrypted {
                found.push(Finding::new(
                    Level::Risk,
                    format!(
                        "{} is plain HTTP: anyone on the way sees what you download and can change it.",
                        hop.host
                    ),
                ));
            }
            if let Some(network) = hop.network {
                found.push(Finding::new(
                    Level::Note,
                    format!(
                        "{} is served by {network}, which sees the request too.",
                        hop.host
                    ),
                ));
            }
            if !hop.cookies_set.is_empty() {
                found.push(Finding::new(
                    Level::Note,
                    format!(
                        "{} sets cookies ({}); {}",
                        hop.host,
                        hop.cookies_set.join(", "),
                        if config.cookie_jar {
                            "Hiroi keeps them in memory for this download only."
                        } else {
                            "Hiroi does not keep them."
                        }
                    ),
                ));
            }
            if !hop.tracking_params.is_empty() {
                // Stripping cleans the links we are given; a redirect
                // can add parameters of its own.
                let fix = if index == 0 {
                    "; the strip-tracking setting removes them"
                } else {
                    ", added by the redirect"
                };
                found.push(Finding::new(
                    Level::Warning,
                    format!(
                        "The link to {} carries tracking parameters ({}){fix}.",
                        hop.host,
                        hop.tracking_params.join(", ")
                    ),
                ));
            }
        }
    }

    let sent: Vec<&str> = [(REFERER, "Referer"), (AUTHORIZATION, "credentials")]
        .into_iter()
        .filter(|(name, _)| request.headers.contains_key(name))
        .filter(|(name, _)| !(config.no_referer && *name == REFERER))
        .map(|(_, label)| label)
        .collect();
    if !sent.is_empty() {
        found.push(Finding::new(
            Level::Warning,
            format!(
                "The request carries {} for every server above, mirrors included.",
                list(&sent)
            ),
        ));
    }
    if request.sources.len() > 1 {
        found.push(Finding::new(
            Level::Note,
            "Every mirror is one more server that sees your address and what you download.",
        ));
    }
    for url in &request.sources {
        match url.scheme() {
            "ftp" => found.push(Finding::new(
                Level::Risk,
                format!(
                    "{}: plain FTP sends the password and the file unencrypted.",
                    display_url(url)
                ),
            )),
            "magnet" => found.push(Finding::new(
                Level::Risk,
                "BitTorrent: every peer sees your address, whatever the proxy settings.",
            )),
            _ => {}
        }
    }

    if let Some(check) = &report.public_ip {
        match (&check.seen, &check.direct) {
            (Ok(seen), Some(Ok(direct))) if seen == direct && report.proxy != ProxyKind::None => {
                found.push(Finding::new(
                    Level::Risk,
                    format!("The proxy does not hide your address: both ways, servers see {seen}."),
                ));
            }
            (Ok(seen), Some(Ok(direct))) => found.push(Finding::new(
                Level::Note,
                format!("Servers see {seen}; without the proxy they would see {direct}."),
            )),
            (Ok(seen), _) => found.push(Finding::new(
                Level::Note,
                format!("Servers see the address {seen}."),
            )),
            (Err(error), _) => found.push(Finding::new(
                Level::Warning,
                format!("Could not ask which address servers see: {error}"),
            )),
        }
    }

    found.push(Finding::new(
        Level::Note,
        "This shows who can see the download, not what they do with it. Whether a server \
         keeps logs, shares them, or reports the visit cannot be observed from here.",
    ));
    found
}

/// `a`, `a and b`, `a, b, and c`.
fn list(items: &[&str]) -> String {
    match items {
        [] => "the servers".into(),
        [one] => (*one).to_string(),
        [one, two] => format!("{one} and {two}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_use_the_serial_comma() {
        assert_eq!(list(&["a"]), "a");
        assert_eq!(list(&["a", "b"]), "a and b");
        assert_eq!(list(&["a", "b", "c"]), "a, b, and c");
    }

    #[test]
    fn networks_are_recognized_by_their_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("cf-ray", "8a1b".parse().unwrap());
        assert_eq!(network(&headers), Some("Cloudflare"));
        let mut headers = HeaderMap::new();
        headers.insert(SERVER, "AkamaiGHost".parse().unwrap());
        assert_eq!(network(&headers), Some("Akamai"));
        assert_eq!(network(&HeaderMap::new()), None);
    }
}
