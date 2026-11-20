//! `hiroi network`: the interfaces downloads can be bound to, and IP
//! filter files.

use std::collections::BTreeMap;
use std::process::ExitCode;

use anyhow::Result;
use hiroi_engine::IpFilter;

use crate::args::NetworkCommand;

/// Name parts VPN software gives its interfaces. Only a hint: any
/// interface can be bound to.
const VPN_HINTS: &[&str] = &[
    "tun",
    "tap",
    "wg",
    "utun",
    "ppp",
    "ipsec",
    "nordlynx",
    "proton",
    "mullvad",
    "tailscale",
    "zerotier",
    "openvpn",
    "wireguard",
    "vpn",
];

pub fn run(command: NetworkCommand) -> Result<ExitCode> {
    match command {
        NetworkCommand::Interfaces => interfaces(),
        NetworkCommand::CheckFilter { file } => {
            let filter = IpFilter::load(&file)?;
            println!(
                "{} ranges blocked; {} lines could not be read.",
                filter.len(),
                filter.skipped()
            );
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn interfaces() -> Result<ExitCode> {
    let mut by_name: BTreeMap<String, (bool, Vec<String>)> = BTreeMap::new();
    for interface in hiroi_engine::interfaces() {
        let entry = by_name.entry(interface.name).or_default();
        entry.0 |= interface.loopback;
        entry.1.push(interface.address.to_string());
    }
    if by_name.is_empty() {
        println!("No interface is up.");
        return Ok(ExitCode::SUCCESS);
    }
    for (name, (loopback, addresses)) in &by_name {
        let lower = name.to_ascii_lowercase();
        let note = if *loopback {
            "  (this machine only)"
        } else if VPN_HINTS.iter().any(|hint| lower.contains(hint)) {
            "  (looks like a VPN)"
        } else {
            ""
        };
        println!("{name}{note}");
        for address in addresses {
            println!("    {address}");
        }
    }
    println!();
    println!("Bind downloads to one with --interface NAME; they stop when it goes down.");
    Ok(ExitCode::SUCCESS)
}
