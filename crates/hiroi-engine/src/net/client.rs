use std::sync::Arc;

use reqwest::header::{ACCEPT, ACCEPT_ENCODING, HeaderMap, HeaderValue};
use reqwest::redirect::Policy;
use reqwest::{Client, Proxy, RequestBuilder, Url};

use crate::config::EngineConfig;
use crate::constants::MAX_REDIRECTS;
use crate::error::{EngineError, Result};
use crate::net::cookies::DownloadCookies;
use crate::net::guard::Guard;
use crate::net::retry::{FetchError, FetchResult};
use crate::net::tls::{self, Purpose};

/// An HTTP client and the network rules its connections follow.
#[derive(Debug, Clone)]
pub(crate) struct Http {
    client: Client,
    guard: Arc<Guard>,
}

impl Http {
    /// A GET for `url`, unless the guard forbids its address. Host names
    /// are checked when the client looks them up.
    pub fn get(&self, url: &Url) -> FetchResult<RequestBuilder> {
        self.guard
            .check_url(url)
            .map_err(|refused| FetchError::Refused(refused.0))?;
        Ok(self.client.get(url.clone()))
    }
}

/// The engine's shared client, for requests that belong to no download.
pub(crate) fn build_client(config: &EngineConfig, guard: Arc<Guard>) -> Result<Http> {
    let client = build_client_with(config, download_policy(config, &guard), None, &guard)?;
    Ok(Http { client, guard })
}

/// A client for one download, with its own cookies. Connections are not
/// shared with other downloads, which also keeps one site's connection
/// from carrying another's cookies.
pub(crate) fn download_client(
    config: &EngineConfig,
    cookies: DownloadCookies,
    guard: Arc<Guard>,
) -> Result<Http> {
    let policy = download_policy(config, &guard);
    let client = build_client_with(config, policy, Some(Arc::new(cookies)), &guard)?;
    Ok(Http { client, guard })
}

fn download_policy(config: &EngineConfig, guard: &Arc<Guard>) -> Policy {
    let allow_insecure = config.allow_insecure_redirects;
    let guard = Arc::clone(guard);
    Policy::custom(move |attempt| {
        if let Err(reason) = check_redirect(attempt.previous(), attempt.url(), allow_insecure) {
            return attempt.error(reason);
        }
        match guard.check_url(attempt.url()) {
            Ok(()) => attempt.follow(),
            Err(refused) => attempt.error(refused),
        }
    })
}

/// The engine's client with another redirect policy, for the exposure
/// report, which looks at every hop itself.
pub(crate) fn build_client_with(
    config: &EngineConfig,
    policy: Policy,
    cookies: Option<Arc<DownloadCookies>>,
    guard: &Arc<Guard>,
) -> Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
    // Byte offsets only line up with the file on the server when the
    // transfer is not compressed.
    headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));

    let tls = tls::config(Purpose::Http, config.accept_invalid_certificates)?;
    let mut builder = Client::builder()
        .default_headers(headers)
        .connect_timeout(config.connect_timeout)
        .read_timeout(config.read_timeout)
        .pool_max_idle_per_host(config.max_connections_per_host)
        .tcp_nodelay(true)
        .tls_backend_preconfigured(tls.as_ref().clone())
        .redirect(policy);
    // Anonymous mode clears it: no User-Agent at all.
    if !config.user_agent.is_empty() {
        builder = builder.user_agent(config.user_agent.as_str());
    }
    if let Some(cookies) = cookies {
        builder = builder.cookie_provider(cookies);
    }
    builder = guard.apply(builder);

    if let Some(proxy) = &config.proxy {
        let proxy = Proxy::all(proxy)
            .map_err(|e| EngineError::InvalidProxy(e.without_url().to_string()))?;
        builder = builder.proxy(proxy);
    }
    builder.build().map_err(EngineError::network)
}

fn check_redirect(
    previous: &[Url],
    next: &Url,
    allow_insecure: bool,
) -> std::result::Result<(), &'static str> {
    if previous.len() >= MAX_REDIRECTS {
        return Err("too many redirects");
    }
    if !matches!(next.scheme(), "http" | "https") {
        return Err("redirect to a non-HTTP URL");
    }
    let from_https = previous.last().is_some_and(|url| url.scheme() == "https");
    if from_https && next.scheme() == "http" && !allow_insecure {
        return Err("refused to follow a redirect from HTTPS to plain HTTP");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(value: &str) -> Url {
        Url::parse(value).unwrap()
    }

    #[test]
    fn blocks_https_downgrade_unless_allowed() {
        let previous = [url("https://a.example/f")];
        assert!(check_redirect(&previous, &url("http://b.example/f"), false).is_err());
        assert!(check_redirect(&previous, &url("http://b.example/f"), true).is_ok());
        assert!(check_redirect(&previous, &url("https://b.example/f"), false).is_ok());
    }

    #[test]
    fn blocks_other_schemes_and_loops() {
        assert!(check_redirect(&[], &url("file:///etc/passwd"), true).is_err());
        let chain = vec![url("https://a.example/"); MAX_REDIRECTS];
        assert!(check_redirect(&chain, &url("https://a.example/"), false).is_err());
    }
}
