//! Opening TCP connections for the protocols reqwest does not carry
//! (FTP and SFTP): directly, or through the configured SOCKS5 proxy.
//!
//! HTTP proxies are refused here. They can tunnel with CONNECT, but many
//! only allow port 443, and FTP's data connections would then fail in
//! ways that are hard to explain. A refusal says plainly why.

use std::time::Duration;

use reqwest::Url;
use tokio::net::TcpStream;
use tokio_socks::TargetAddr;
use tokio_socks::tcp::Socks5Stream;

use crate::config::EngineConfig;
use crate::error::EngineError;
use crate::net::guard::{Guard, refusal};
use crate::net::retry::{FetchError, FetchResult};
use crate::privacy::ProxyKind;

/// How connections leave this machine.
#[derive(Debug, Clone)]
pub(crate) enum Route {
    Direct,
    Socks5 {
        proxy: String,
        login: Option<(String, String)>,
        /// Let the proxy resolve host names (`socks5h://`).
        remote_dns: bool,
    },
}

impl Route {
    /// The route `config` asks for, or why `protocol` cannot take it.
    pub fn from_config(config: &EngineConfig, protocol: &str) -> FetchResult<Self> {
        let Some(proxy) = &config.proxy else {
            return Ok(Self::Direct);
        };
        let kind = ProxyKind::of(Some(proxy));
        let remote_dns = match kind {
            ProxyKind::Socks5RemoteDns => true,
            ProxyKind::Socks5LocalDns => false,
            _ => {
                return Err(FetchError::Fatal(EngineError::Protocol(format!(
                    "{protocol} can only go through a SOCKS5 proxy; refusing to connect directly"
                ))));
            }
        };
        let url = Url::parse(proxy).map_err(|_| {
            FetchError::Fatal(EngineError::InvalidProxy("unreadable proxy URL".into()))
        })?;
        let host = url
            .host_str()
            .unwrap_or_default()
            .trim_start_matches('[')
            .trim_end_matches(']');
        let address = format!("{host}:{}", url.port().unwrap_or(1080));
        let login = (!url.username().is_empty()).then(|| {
            let decode = |text: &str| {
                percent_encoding::percent_decode_str(text)
                    .decode_utf8_lossy()
                    .into_owned()
            };
            (decode(url.username()), decode(url.password().unwrap_or("")))
        });
        Ok(Self::Socks5 {
            proxy: address,
            login,
            remote_dns,
        })
    }

    /// Connects to `host:port` within `timeout`, by `guard`'s rules.
    pub async fn connect(
        &self,
        guard: &Guard,
        host: &str,
        port: u16,
        timeout: Duration,
    ) -> FetchResult<TcpStream> {
        let attempt = async {
            match self {
                Self::Direct => {
                    let found = guard.lookup(host, port).await?;
                    guard.connect(&found, timeout).await
                }
                Self::Socks5 {
                    proxy,
                    login,
                    remote_dns,
                } => {
                    let target = if *remote_dns {
                        TargetAddr::Domain(host.into(), port)
                    } else {
                        // socks5:// means the name is looked up here.
                        let found = guard.lookup(host, port).await?;
                        TargetAddr::Ip(found[0])
                    };
                    // The proxy is the user's choice: bound like any
                    // connection, but not filtered.
                    let at = tokio::net::lookup_host(proxy.as_str())
                        .await?
                        .collect::<Vec<_>>();
                    let socket = guard.connect(&at, timeout).await?;
                    let stream = match login {
                        Some((user, password)) => {
                            Socks5Stream::connect_with_password_and_socket(
                                socket, target, user, password,
                            )
                            .await
                        }
                        None => Socks5Stream::connect_with_socket(socket, target).await,
                    };
                    stream
                        .map(Socks5Stream::into_inner)
                        .map_err(|e| std::io::Error::other(format!("SOCKS5 proxy: {e}")))
                }
            }
        };
        match tokio::time::timeout(timeout, attempt).await {
            Ok(Ok(stream)) => {
                let _ = stream.set_nodelay(true);
                Ok(stream)
            }
            Ok(Err(error)) => match refusal(&error) {
                Some(refused) => Err(FetchError::Refused(refused.0.clone())),
                None => Err(FetchError::Connection(error.to_string())),
            },
            Err(_) => Err(FetchError::Connection("connecting took too long".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(proxy: Option<&str>) -> FetchResult<Route> {
        let config = EngineConfig {
            proxy: proxy.map(str::to_string),
            ..EngineConfig::default()
        };
        Route::from_config(&config, "FTP")
    }

    #[test]
    fn socks_proxies_are_read_and_others_refused() {
        assert!(matches!(route(None), Ok(Route::Direct)));
        match route(Some("socks5h://me:p%40ss@10.0.0.2:9050")).unwrap() {
            Route::Socks5 {
                proxy,
                login,
                remote_dns,
            } => {
                assert_eq!(proxy, "10.0.0.2:9050");
                assert_eq!(login, Some(("me".into(), "p@ss".into())));
                assert!(remote_dns);
            }
            Route::Direct => panic!("expected SOCKS"),
        }
        assert!(matches!(
            route(Some("socks5://h")).unwrap(),
            Route::Socks5 {
                remote_dns: false,
                ..
            }
        ));
        assert!(route(Some("http://proxy:3128")).is_err());
    }
}
