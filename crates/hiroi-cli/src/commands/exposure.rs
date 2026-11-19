//! `hiroi exposure`: who would see a download, and what it would give
//! away, without downloading it.

use std::process::ExitCode;

use anyhow::Result;
use hiroi_engine::{Engine, ExposureOptions, ExposureReport, Hop, Level, ProxyKind};

use crate::args::ExposureArgs;
use crate::options;

pub async fn run(args: ExposureArgs) -> Result<ExitCode> {
    let engine = Engine::new(options::engine_config(&args.engine))?;
    let request = options::base_request(&args.url, &args.mirrors, &args.request)?;
    let check = ExposureOptions {
        check_ip: args.check_ip,
        compare_direct: args.direct,
        ip_service: args.ip_service,
    };
    let report = engine.exposure(&request, &check).await?;
    print(&report);
    Ok(ExitCode::SUCCESS)
}

fn print(report: &ExposureReport) {
    println!("Proxy: {}", proxy(report.proxy));
    for source in &report.sources {
        println!("\nSource: {}", source.url);
        for (index, hop) in source.hops.iter().enumerate() {
            println!("  {}. {}", index + 1, hop.url);
            println!("     {}", describe(hop));
        }
        if let Some(error) = &source.error {
            println!("  stopped: {error}");
        }
    }
    if let Some(check) = &report.public_ip {
        let shown = |seen: &Result<std::net::IpAddr, String>| match seen {
            Ok(address) => address.to_string(),
            Err(error) => format!("unknown ({error})"),
        };
        println!("\nServers see: {}", shown(&check.seen));
        if let Some(direct) = &check.direct {
            println!("Without the proxy: {}", shown(direct));
        }
    }
    println!("\nFindings:");
    for finding in &report.findings {
        let level = match finding.level {
            Level::Risk => "RISK   ",
            Level::Warning => "WARNING",
            Level::Note => "NOTE   ",
        };
        println!("  {level}  {}", finding.text);
    }
}

fn proxy(kind: ProxyKind) -> &'static str {
    match kind {
        ProxyKind::None => "none, servers see your address",
        ProxyKind::Http => "HTTP",
        ProxyKind::Socks5LocalDns => "SOCKS5, names looked up on this machine",
        ProxyKind::Socks5RemoteDns => "SOCKS5, names looked up by the proxy",
        ProxyKind::Socks4 => "SOCKS4",
        ProxyKind::Unknown => "unknown kind",
    }
}

/// `302, 127.0.0.1, not encrypted, Cloudflare, sets cookies: id`
fn describe(hop: &Hop) -> String {
    let mut parts = Vec::new();
    if let Some(status) = hop.status {
        parts.push(status.to_string());
    }
    if hop.addresses.is_empty() {
        parts.push(hop.host.clone());
    } else {
        let addresses: Vec<String> = hop.addresses.iter().map(ToString::to_string).collect();
        parts.push(format!("{} ({})", hop.host, addresses.join(", ")));
    }
    parts.push(
        if hop.encrypted {
            "encrypted"
        } else {
            "not encrypted"
        }
        .into(),
    );
    if hop.hsts {
        parts.push("HSTS".into());
    }
    if let Some(network) = hop.network {
        parts.push(format!("served by {network}"));
    } else if let Some(server) = &hop.server {
        parts.push(format!("server: {server}"));
    }
    if !hop.cookies_set.is_empty() {
        parts.push(format!("sets cookies: {}", hop.cookies_set.join(", ")));
    }
    parts.join(", ")
}
