//! Lists of blocked addresses, read from the formats other programs
//! publish them in.
//!
//! A line is one of:
//!
//! - eMule `.dat`: `1.2.3.0 - 1.2.3.255 , 000 , description`. Entries
//!   with an access level of 128 or more are allowed, as in eMule.
//! - PeerGuardian `.p2p`: `description:1.2.3.0-1.2.3.255`.
//! - A range `first - last`, a CIDR block `10.0.0.0/8`, or one address.
//!
//! IPv4 and IPv6 both work. Blank lines and lines starting with `#` or
//! `//` are skipped; other lines that cannot be read are counted and
//! reported, so a broken file is not mistaken for an empty one.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;

use crate::error::{EngineError, Result};

/// eMule's levels: an entry blocks when its level is below this.
const DAT_ALLOW_LEVEL: u32 = 128;

/// Sorted, merged ranges of blocked addresses.
#[derive(Debug, Default)]
pub struct IpFilter {
    v4: Vec<(u32, u32)>,
    v6: Vec<(u128, u128)>,
    skipped: usize,
}

impl IpFilter {
    /// Reads a filter file.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read(path).map_err(|error| {
            EngineError::Config(format!(
                "cannot read the IP filter {}: {error}",
                path.display()
            ))
        })?;
        let filter = Self::parse(&String::from_utf8_lossy(&text));
        if filter.is_empty() {
            return Err(EngineError::Config(format!(
                "the IP filter {} blocks nothing ({} could not be read)",
                path.display(),
                lines(filter.skipped)
            )));
        }
        Ok(filter)
    }

    pub fn parse(text: &str) -> Self {
        let mut filter = Self::default();
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
                continue;
            }
            match parse_line(line) {
                Some(Entry::Blocked(first, last)) => filter.insert(first, last),
                Some(Entry::Allowed) => {}
                None => filter.skipped += 1,
            }
        }
        filter.v4 = merged(std::mem::take(&mut filter.v4));
        filter.v6 = merged(std::mem::take(&mut filter.v6));
        filter
    }

    pub fn blocks(&self, address: IpAddr) -> bool {
        match canonical(address) {
            IpAddr::V4(v4) => contains(&self.v4, u32::from(v4)),
            IpAddr::V6(v6) => contains(&self.v6, u128::from(v6)),
        }
    }

    /// Ranges after merging, IPv4 and IPv6 together.
    pub fn len(&self) -> usize {
        self.v4.len() + self.v6.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Lines that were neither entries nor comments, as `3 lines`.
    pub fn skipped_lines(&self) -> String {
        lines(self.skipped)
    }

    /// Lines that were neither entries nor comments.
    pub fn skipped(&self) -> usize {
        self.skipped
    }

    fn insert(&mut self, first: IpAddr, last: IpAddr) {
        match (canonical(first), canonical(last)) {
            (IpAddr::V4(a), IpAddr::V4(b)) => {
                let (a, b) = (u32::from(a), u32::from(b));
                self.v4.push((a.min(b), a.max(b)));
            }
            (IpAddr::V6(a), IpAddr::V6(b)) => {
                let (a, b) = (u128::from(a), u128::from(b));
                self.v6.push((a.min(b), a.max(b)));
            }
            // A range from an IPv4 to an IPv6 address means nothing.
            _ => self.skipped += 1,
        }
    }
}

fn lines(count: usize) -> String {
    if count == 1 {
        "1 line".into()
    } else {
        format!("{count} lines")
    }
}

enum Entry {
    Blocked(IpAddr, IpAddr),
    Allowed,
}

fn parse_line(line: &str) -> Option<Entry> {
    // eMule: the range, then the level, then a description.
    let mut fields = line.split(',');
    let range = fields.next()?.trim();
    if let Some(level) = fields.next() {
        let level: u32 = level.trim().parse().ok()?;
        if level >= DAT_ALLOW_LEVEL {
            return Some(Entry::Allowed);
        }
        let (first, last) = parse_range(range)?;
        return Some(Entry::Blocked(first, last));
    }
    if let Some((first, last)) = parse_range(line) {
        return Some(Entry::Blocked(first, last));
    }
    // PeerGuardian: the description may contain colons, the range not
    // (it is IPv4 only).
    let (_, range) = line.rsplit_once(':')?;
    let (first, last) = parse_range(range)?;
    Some(Entry::Blocked(first, last))
}

fn parse_range(text: &str) -> Option<(IpAddr, IpAddr)> {
    let text = text.trim();
    if let Some((first, last)) = text.split_once('-') {
        return Some((address(first)?, address(last)?));
    }
    if let Some((base, bits)) = text.split_once('/') {
        return cidr(address(base)?, bits.trim().parse().ok()?);
    }
    let one = address(text)?;
    Some((one, one))
}

/// Parses an address, also with the zero padding `.dat` files use
/// (`001.009.096.105`), which the standard parser rejects.
fn address(text: &str) -> Option<IpAddr> {
    let text = text.trim();
    if let Ok(address) = text.parse() {
        return Some(address);
    }
    let parts: Vec<u8> = text
        .split('.')
        .map(|part| part.parse().ok())
        .collect::<Option<_>>()?;
    let [a, b, c, d] = parts[..] else {
        return None;
    };
    Some(IpAddr::V4(Ipv4Addr::new(a, b, c, d)))
}

fn cidr(base: IpAddr, bits: u32) -> Option<(IpAddr, IpAddr)> {
    match base {
        IpAddr::V4(v4) if bits <= 32 => {
            let mask = u32::MAX.checked_shl(32 - bits).unwrap_or(0);
            let first = u32::from(v4) & mask;
            Some((IpAddr::V4(first.into()), IpAddr::V4((first | !mask).into())))
        }
        IpAddr::V6(v6) if bits <= 128 => {
            let mask = u128::MAX.checked_shl(128 - bits).unwrap_or(0);
            let first = u128::from(v6) & mask;
            Some((
                IpAddr::V6(Ipv6Addr::from(first)),
                IpAddr::V6(Ipv6Addr::from(first | !mask)),
            ))
        }
        _ => None,
    }
}

/// IPv4 addresses written as IPv6 (`::ffff:1.2.3.4`) are checked as IPv4,
/// so a list of IPv4 ranges cannot be dodged by the other notation.
pub(crate) fn canonical(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    }
}

fn merged<T: Ord + Copy + num_step::Step>(mut ranges: Vec<(T, T)>) -> Vec<(T, T)> {
    ranges.sort_unstable();
    let mut out: Vec<(T, T)> = Vec::with_capacity(ranges.len());
    for (first, last) in ranges {
        match out.last_mut() {
            Some(previous) if first <= previous.1.next() => previous.1 = previous.1.max(last),
            _ => out.push((first, last)),
        }
    }
    out.shrink_to_fit();
    out
}

fn contains<T: Ord + Copy>(ranges: &[(T, T)], value: T) -> bool {
    let after = ranges.partition_point(|&(first, _)| first <= value);
    after > 0 && ranges[after - 1].1 >= value
}

mod num_step {
    /// The next value, saturating, so touching ranges merge.
    pub trait Step {
        fn next(self) -> Self;
    }
    impl Step for u32 {
        fn next(self) -> Self {
            self.saturating_add(1)
        }
    }
    impl Step for u128 {
        fn next(self) -> Self {
            self.saturating_add(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn reads_every_format() {
        let filter = IpFilter::parse(
            "# comment\n\
             001.009.096.105 - 001.009.096.110 , 000 , Some list\n\
             002.000.000.000 - 002.255.255.255 , 200 , allowed\n\
             Bad people: inc.:3.0.0.0-3.0.0.255\n\
             10.0.0.0/8\n\
             192.0.2.7\n\
             2001:db8::/32\n\
             not an entry\n",
        );
        assert!(filter.blocks(ip("1.9.96.107")));
        assert!(!filter.blocks(ip("1.9.96.111")));
        assert!(!filter.blocks(ip("2.1.1.1")));
        assert!(filter.blocks(ip("3.0.0.200")));
        assert!(filter.blocks(ip("10.200.0.1")));
        assert!(filter.blocks(ip("192.0.2.7")));
        assert!(!filter.blocks(ip("192.0.2.8")));
        assert!(filter.blocks(ip("2001:db8:1::5")));
        assert!(!filter.blocks(ip("2001:db9::1")));
        assert_eq!(filter.skipped(), 1);
    }

    #[test]
    fn mapped_addresses_are_checked_as_ipv4() {
        let filter = IpFilter::parse("127.0.0.0/8\n");
        assert!(filter.blocks(ip("::ffff:127.0.0.1")));
    }

    #[test]
    fn touching_ranges_merge() {
        let filter = IpFilter::parse("1.0.0.0-1.0.0.9\n1.0.0.10-1.0.0.20\n1.0.0.5-1.0.0.6\n");
        assert_eq!(filter.len(), 1);
        assert!(filter.blocks(ip("1.0.0.15")));
        let all = IpFilter::parse("0.0.0.0/0\n");
        assert!(all.blocks(ip("255.255.255.255")));
    }
}
