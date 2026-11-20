//! Where downloads may connect, and from which interface.
//!
//! Three rules, read once when the engine starts:
//!
//! - **Interface binding.** With `interface` set, every connection
//!   leaves through that interface, and fails when it is gone. Bound to
//!   a VPN's interface, a dropped VPN stops downloads instead of letting
//!   them carry on over the normal connection. Linux binds the socket to
//!   the device; elsewhere the socket is bound to the interface's
//!   address, looked up for every download.
//! - **IP filter.** Addresses in the filter file are never connected to,
//!   whatever name led there.
//! - **No redirects into the local network.** A public site cannot send
//!   a download to `localhost`, the router, or other machines on the
//!   LAN, through a redirect or a playlist. Hosts the user named, and
//!   every host once a download started on the local network, are
//!   exempt.
//!
//! Names are looked up through [`Guard`], which drops forbidden
//! addresses before anything connects. Addresses written into a URL are
//! not looked up, so [`Guard::check_url`] covers those.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::{ClientBuilder, Url};

use crate::config::EngineConfig;
use crate::error::Result;
use crate::net::ipfilter::{IpFilter, canonical};

/// The engine's network rules.
#[derive(Debug)]
pub(crate) struct Network {
    filter: Option<Arc<IpFilter>>,
    interface: Option<String>,
    bind_address: Option<IpAddr>,
    allow_local_redirects: bool,
    /// The proxy's host, which the user chose, so it may be local.
    proxy_host: Option<String>,
}

impl Network {
    pub fn from_config(config: &EngineConfig) -> Result<Self> {
        let filter = match &config.ip_filter {
            Some(path) => {
                let filter = IpFilter::load(path)?;
                tracing::info!(
                    ranges = filter.len(),
                    unreadable = filter.skipped(),
                    "IP filter loaded"
                );
                Some(Arc::new(filter))
            }
            None => None,
        };
        let proxy_host = config
            .proxy
            .as_deref()
            .and_then(|proxy| Url::parse(proxy).ok())
            .and_then(|url| url.host_str().map(bare_host));
        let network = Self {
            filter,
            interface: config.interface.clone(),
            bind_address: config.bind_address,
            allow_local_redirects: config.allow_local_redirects,
            proxy_host,
        };
        // Catches a misspelled interface at start. A VPN that is down
        // now is not an error: downloads wait for it.
        if let Some(name) = &network.interface
            && !interfaces().iter().any(|i| &i.name == name)
        {
            tracing::warn!(interface = %name, "the interface is not up; downloads fail until it is");
        }
        Ok(network)
    }

    /// The rules for one download, whose `chosen` hosts came from the
    /// user (or a Metalink file) and may be local.
    pub fn guard<'a>(&self, chosen: impl IntoIterator<Item = &'a Url>) -> Arc<Guard> {
        let mut hosts: Vec<String> = chosen
            .into_iter()
            .filter_map(|url| url.host_str().map(bare_host))
            .collect();
        hosts.extend(self.proxy_host.clone());
        let guard = Guard {
            filter: self.filter.clone(),
            chosen: (!self.allow_local_redirects).then_some(hosts),
            binding: self.binding(),
            local_start: AtomicBool::new(false),
        };
        Arc::new(guard)
    }

    fn binding(&self) -> Binding {
        let Some(name) = &self.interface else {
            return self.bind_address.map_or(Binding::Any, Binding::Address);
        };
        if cfg!(any(
            target_os = "linux",
            target_os = "android",
            target_os = "fuchsia"
        )) {
            return Binding::Device {
                name: name.clone(),
                address: self.bind_address,
            };
        }
        let found: Vec<IpAddr> = interfaces()
            .into_iter()
            .filter(|i| &i.name == name)
            .map(|i| i.address)
            .collect();
        let address = match self.bind_address {
            Some(wanted) if found.contains(&wanted) => Some(wanted),
            Some(wanted) => {
                return Binding::Unavailable(format!(
                    "{wanted} is not an address of the interface {name}"
                ));
            }
            None => found
                .iter()
                .find(|a| a.is_ipv4())
                .or(found.first())
                .copied(),
        };
        match address {
            Some(address) => Binding::Address(address),
            None => Binding::Unavailable(format!(
                "the interface {name} is not up, and connections must use it"
            )),
        }
    }
}

/// How sockets are tied to the machine's side of a connection.
#[derive(Debug, Clone)]
enum Binding {
    Any,
    Address(IpAddr),
    /// Linux: the device itself, so routing cannot move the connection
    /// to another interface.
    Device {
        name: String,
        address: Option<IpAddr>,
    },
    /// The interface to bind to is gone: nothing may connect.
    Unavailable(String),
}

/// One download's rules. Also its DNS resolver.
#[derive(Debug)]
pub(crate) struct Guard {
    filter: Option<Arc<IpFilter>>,
    /// Hosts that may be local; `None` when local addresses are allowed
    /// everywhere.
    chosen: Option<Vec<String>>,
    binding: Binding,
    /// A chosen host turned out to be local, so the download lives on
    /// the local network and may stay there.
    local_start: AtomicBool,
}

impl Guard {
    /// Rules that only filter and bind, for requests that belong to no
    /// download.
    pub fn open(network: &Network) -> Arc<Self> {
        Arc::new(Self {
            filter: network.filter.clone(),
            chosen: None,
            binding: network.binding(),
            local_start: AtomicBool::new(false),
        })
    }

    /// Ties `builder`'s connections to the interface or address, and
    /// sends its lookups through this guard.
    pub fn apply(self: &Arc<Self>, mut builder: ClientBuilder) -> ClientBuilder {
        match &self.binding {
            // Lookups and URL checks refuse everything instead.
            Binding::Any | Binding::Unavailable(_) => {}
            Binding::Address(address) => builder = builder.local_address(*address),
            Binding::Device { name, address } => {
                #[cfg(any(target_os = "linux", target_os = "android", target_os = "fuchsia"))]
                {
                    builder = builder.interface(name);
                }
                #[cfg(not(any(
                    target_os = "linux",
                    target_os = "android",
                    target_os = "fuchsia"
                )))]
                let _ = name;
                if let Some(address) = address {
                    builder = builder.local_address(*address);
                }
            }
        }
        builder.dns_resolver(Resolver(Arc::clone(self)))
    }

    /// Refuses URLs whose host is a forbidden address. Host names are
    /// checked when they are looked up.
    pub fn check_url(&self, url: &Url) -> std::result::Result<(), Refusal> {
        let Some(host) = url.host_str().map(bare_host) else {
            return Ok(());
        };
        let Ok(address) = host.parse::<IpAddr>() else {
            return Ok(());
        };
        let port = url.port_or_known_default().unwrap_or(0);
        self.screen(&host, vec![SocketAddr::new(address, port)])
            .map(drop)
    }

    /// Drops the addresses `host` may not reach, or says why none is left.
    pub fn screen(
        &self,
        host: &str,
        found: Vec<SocketAddr>,
    ) -> std::result::Result<Vec<SocketAddr>, Refusal> {
        if let Binding::Unavailable(reason) = &self.binding {
            return Err(Refusal(reason.clone()));
        }
        let chosen = self
            .chosen
            .as_ref()
            .is_some_and(|hosts| hosts.iter().any(|h| h.eq_ignore_ascii_case(host)));
        if chosen && found.iter().any(|a| is_local(a.ip())) {
            self.local_start.store(true, Ordering::Relaxed);
        }
        let local_ok = self.chosen.is_none() || chosen || self.local_start.load(Ordering::Relaxed);
        let mut reason = None;
        let kept: Vec<SocketAddr> = found
            .into_iter()
            .filter(|address| {
                let ip = address.ip();
                if self.filter.as_ref().is_some_and(|f| f.blocks(ip)) {
                    reason = Some("is blocked by the IP filter");
                    return false;
                }
                if !local_ok && is_local(ip) {
                    reason = Some("is on the local network, and a remote site led there");
                    return false;
                }
                true
            })
            .collect();
        match (kept.is_empty(), reason) {
            (true, Some(reason)) => Err(Refusal(format!("{host} {reason}"))),
            _ => Ok(kept),
        }
    }
}

/// Connections for the protocols reqwest does not carry.
#[cfg(any(feature = "ftp", feature = "sftp"))]
impl Guard {
    /// Looks `host` up and screens the answer.
    pub async fn lookup(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
        let found = tokio::net::lookup_host((host, port)).await?.collect();
        self.screen(host, found).map_err(std::io::Error::other)
    }

    /// Opens a TCP connection to one of `addresses`, bound as configured.
    pub async fn connect(
        &self,
        addresses: &[SocketAddr],
        timeout: std::time::Duration,
    ) -> std::io::Result<tokio::net::TcpStream> {
        let mut last = None;
        for &address in addresses {
            match tokio::time::timeout(timeout, self.connect_one(address)).await {
                Ok(Ok(stream)) => return Ok(stream),
                Ok(Err(error)) => last = Some(error),
                Err(_) => last = Some(std::io::ErrorKind::TimedOut.into()),
            }
        }
        Err(last.unwrap_or_else(|| std::io::Error::other("the server has no address")))
    }

    async fn connect_one(&self, address: SocketAddr) -> std::io::Result<tokio::net::TcpStream> {
        use tokio::net::TcpSocket;
        let socket = if address.is_ipv4() {
            TcpSocket::new_v4()?
        } else {
            TcpSocket::new_v6()?
        };
        let local = match &self.binding {
            Binding::Any => None,
            Binding::Unavailable(reason) => return Err(std::io::Error::other(reason.clone())),
            Binding::Address(local) => Some(*local),
            Binding::Device { name, address } => {
                #[cfg(any(target_os = "linux", target_os = "android", target_os = "fuchsia"))]
                socket.bind_device(Some(name.as_bytes()))?;
                #[cfg(not(any(
                    target_os = "linux",
                    target_os = "android",
                    target_os = "fuchsia"
                )))]
                let _ = name;
                *address
            }
        };
        if let Some(local) = local {
            socket.bind(SocketAddr::new(local, 0))?;
        }
        socket.connect(address).await
    }
}

/// Why the guard refused a connection, carried through reqwest's errors
/// so the reason reaches the user.
#[derive(Debug)]
pub(crate) struct Refusal(pub String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refusal {}

/// Finds a [`Refusal`] among the causes of `error`.
pub(crate) fn refusal<'a>(error: &'a (dyn std::error::Error + 'static)) -> Option<&'a Refusal> {
    let mut next = Some(error);
    while let Some(error) = next {
        if let Some(refusal) = error.downcast_ref::<Refusal>() {
            return Some(refusal);
        }
        // io::Error hides the error it wraps from source().
        if let Some(inner) = error
            .downcast_ref::<std::io::Error>()
            .and_then(|io| io.get_ref())
            .and_then(|inner| inner.downcast_ref::<Refusal>())
        {
            return Some(inner);
        }
        next = error.source();
    }
    None
}

/// The guard as reqwest's resolver.
struct Resolver(Arc<Guard>);

impl Resolve for Resolver {
    fn resolve(&self, name: Name) -> Resolving {
        let guard = Arc::clone(&self.0);
        let host = name.as_str().to_string();
        Box::pin(async move {
            let found = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            match guard.screen(&host, found) {
                Ok(kept) => Ok(Box::new(kept.into_iter()) as Addrs),
                Err(refused) => Err(Box::new(refused) as _),
            }
        })
    }
}

/// Addresses that lead into this machine or its network: loopback,
/// private ranges, link-local, carrier-grade NAT, and unique local IPv6.
pub(crate) fn is_local(address: IpAddr) -> bool {
    match canonical(address) {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
        }
    }
}

/// A network interface and one of its addresses.
#[derive(Debug, Clone)]
pub struct InterfaceAddress {
    pub name: String,
    pub address: IpAddr,
    pub loopback: bool,
}

/// Every address of every interface that is up.
pub fn interfaces() -> Vec<InterfaceAddress> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .map(|i| InterfaceAddress {
            loopback: i.is_loopback(),
            address: i.ip(),
            name: i.name,
        })
        .collect()
}

fn bare_host(host: &str) -> String {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard(chosen: &[&str], filter: Option<&str>) -> Guard {
        Guard {
            filter: filter.map(|text| Arc::new(IpFilter::parse(text))),
            chosen: Some(chosen.iter().map(|h| h.to_string()).collect()),
            binding: Binding::Any,
            local_start: AtomicBool::new(false),
        }
    }

    fn at(ip: &str) -> Vec<SocketAddr> {
        vec![SocketAddr::new(ip.parse().unwrap(), 80)]
    }

    #[test]
    fn remote_sites_cannot_lead_into_the_lan() {
        let guard = guard(&["example.com"], None);
        assert!(guard.screen("example.com", at("93.184.216.34")).is_ok());
        let refused = guard.screen("evil.example", at("192.168.1.1")).unwrap_err();
        assert!(refused.to_string().contains("local network"), "{refused}");
        assert!(
            guard
                .check_url(&"http://127.0.0.1/".parse().unwrap())
                .is_err()
        );
        assert!(
            guard
                .check_url(&"http://[::1]:8080/".parse().unwrap())
                .is_err()
        );
        assert!(guard.check_url(&"http://8.8.8.8/".parse().unwrap()).is_ok());
    }

    #[test]
    fn downloads_that_start_on_the_lan_may_stay_there() {
        let guard = guard(&["nas.home"], None);
        assert!(guard.screen("nas.home", at("192.168.1.5")).is_ok());
        assert!(
            guard
                .check_url(&"http://192.168.1.6/f".parse().unwrap())
                .is_ok()
        );
    }

    #[test]
    fn the_filter_applies_to_every_host() {
        let guard = guard(&["127.0.0.1"], Some("127.0.0.0/8\n"));
        let refused = guard
            .check_url(&"http://127.0.0.1/".parse().unwrap())
            .unwrap_err();
        assert!(refused.to_string().contains("IP filter"), "{refused}");
        let mixed = [at("127.0.0.1"), at("8.8.8.8")].concat();
        assert_eq!(guard.screen("x", mixed).unwrap().len(), 1);
    }

    #[test]
    fn local_ranges() {
        for ip in [
            "10.1.2.3",
            "172.16.0.1",
            "100.64.0.1",
            "169.254.1.1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(is_local(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "100.128.0.1", "2001:4860::8888", "172.32.0.1"] {
            assert!(!is_local(ip.parse().unwrap()), "{ip}");
        }
    }
}
