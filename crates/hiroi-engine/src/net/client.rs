use reqwest::header::{ACCEPT, ACCEPT_ENCODING, HeaderMap, HeaderValue};
use reqwest::redirect::Policy;
use reqwest::{Client, Proxy, Url};

use crate::config::EngineConfig;
use crate::constants::MAX_REDIRECTS;
use crate::error::{EngineError, Result};

pub(crate) fn build_client(config: &EngineConfig) -> Result<Client> {
    let allow_insecure = config.allow_insecure_redirects;
    let policy = Policy::custom(move |attempt| {
        match check_redirect(attempt.previous(), attempt.url(), allow_insecure) {
            Ok(()) => attempt.follow(),
            Err(reason) => attempt.error(reason),
        }
    });
    build_client_with(config, policy)
}

/// The engine's client with another redirect policy, for the exposure
/// report, which looks at every hop itself.
pub(crate) fn build_client_with(config: &EngineConfig, policy: Policy) -> Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
    // Byte offsets only line up with the file on the server when the
    // transfer is not compressed.
    headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));

    let mut builder = Client::builder()
        .user_agent(config.user_agent.as_str())
        .default_headers(headers)
        .connect_timeout(config.connect_timeout)
        .read_timeout(config.read_timeout)
        .pool_max_idle_per_host(config.max_connections_per_host)
        .tcp_nodelay(true)
        .redirect(policy);

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
