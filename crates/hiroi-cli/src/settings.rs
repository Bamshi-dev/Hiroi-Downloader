//! Every setting in one list, and the settings file.
//!
//! Settings live in one of two places:
//!
//! - **The file**, `config.toml` in Hiroi's config folder (or the file
//!   `HIROI_CONFIG` names). These are the engine's settings; each has a
//!   command-line flag of the same name, which wins over the file. The
//!   service reads them when it starts.
//! - **The service**, for settings it changes while running
//!   (notifications, smart folders, the speed limit). Changing one
//!   applies at once.
//!
//! The file can hold a proxy password, so it is written readable by its
//! owner only.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::size::parse_rate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Switch,
    Number,
    /// A speed such as `2M`.
    Rate,
    Text,
    Path,
    Address,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Home {
    File,
    Service,
}

#[derive(Debug)]
pub struct Setting {
    pub key: &'static str,
    pub section: &'static str,
    pub kind: Kind,
    pub home: Home,
    pub summary: &'static str,
    /// Shown when nothing is set; empty for "none".
    pub default: &'static str,
}

const fn file(
    key: &'static str,
    section: &'static str,
    kind: Kind,
    default: &'static str,
    summary: &'static str,
) -> Setting {
    Setting {
        key,
        section,
        kind,
        home: Home::File,
        summary,
        default,
    }
}

const fn service(
    key: &'static str,
    kind: Kind,
    default: &'static str,
    summary: &'static str,
) -> Setting {
    Setting {
        key,
        section: "service",
        kind,
        home: Home::Service,
        summary,
        default,
    }
}

pub const SETTINGS: &[Setting] = &[
    file(
        "proxy",
        "network",
        Kind::Text,
        "",
        "Proxy for all requests, e.g. socks5h://127.0.0.1:9050.",
    ),
    file(
        "per-host",
        "network",
        Kind::Number,
        "16",
        "Most connections to one server, across all downloads.",
    ),
    file(
        "min-speed",
        "network",
        Kind::Rate,
        "",
        "Reconnect connections slower than this, e.g. 50K.",
    ),
    file(
        "interface",
        "network",
        Kind::Text,
        "",
        "Connect only through this interface (a VPN's tun0 or wg0); downloads stop when it goes down.",
    ),
    file(
        "bind-address",
        "network",
        Kind::Address,
        "",
        "Connect from this local address.",
    ),
    file(
        "ip-filter",
        "network",
        Kind::Path,
        "",
        "Never connect to the addresses in this file (.dat, .p2p, or CIDR lines).",
    ),
    file(
        "allow-local-redirects",
        "network",
        Kind::Switch,
        "false",
        "Let remote sites redirect downloads into the local network.",
    ),
    file(
        "allow-insecure-redirects",
        "network",
        Kind::Switch,
        "false",
        "Follow redirects from HTTPS to plain HTTP (unsafe).",
    ),
    file(
        "no-check-certificate",
        "network",
        Kind::Switch,
        "false",
        "Accept TLS certificates that do not check out (unsafe).",
    ),
    file(
        "require-proxy",
        "privacy",
        Kind::Switch,
        "false",
        "Refuse to run without a proxy, so a forgotten setting cannot expose your address.",
    ),
    file(
        "strip-tracking",
        "privacy",
        Kind::Switch,
        "false",
        "Remove tracking parameters (utm_source, fbclid, ...) from links.",
    ),
    file(
        "no-referer",
        "privacy",
        Kind::Switch,
        "false",
        "Never send a Referer, not even one the browser handed over.",
    ),
    file(
        "no-cookie-jar",
        "privacy",
        Kind::Switch,
        "false",
        "Ignore cookies servers set during a download.",
    ),
    file(
        "anonymous",
        "privacy",
        Kind::Switch,
        "false",
        "No Hiroi User-Agent, no Referer, no tracking parameters.",
    ),
    file(
        "no-prealloc",
        "disk",
        Kind::Switch,
        "false",
        "Don't reserve disk space before downloading.",
    ),
    file(
        "no-sync",
        "disk",
        Kind::Switch,
        "false",
        "Don't flush downloads to disk before calling them saved; faster, less safe on power loss.",
    ),
    file(
        "yt-dlp",
        "programs",
        Kind::Path,
        "",
        "The yt-dlp program for video sites, when the one on the PATH is not the right one.",
    ),
    service(
        "notifications",
        Kind::Switch,
        "true",
        "Desktop notifications when downloads finish.",
    ),
    service(
        "smart-folders",
        Kind::Switch,
        "true",
        "Move finished episodes of a series into a folder named after it.",
    ),
    service(
        "speed-limit",
        Kind::Rate,
        "",
        "Combined speed limit of the service's downloads, e.g. 2M.",
    ),
    service(
        "keep-awake",
        Kind::Switch,
        "true",
        "Keep the computer from sleeping while downloads run.",
    ),
    service(
        "unpack-archives",
        Kind::Switch,
        "false",
        "Unpack finished archives (zip, 7z, rar, tar; split ones too) into a folder next to them.",
    ),
    service(
        "after-download",
        Kind::Text,
        "",
        "Command run after every download, e.g. a virus scanner: clamscan --no-summary {path}",
    ),
];

pub fn find(key: &str) -> Result<&'static Setting> {
    SETTINGS
        .iter()
        .find(|setting| setting.key == key)
        .with_context(|| format!("no setting `{key}`; `hiroi settings list` shows them all"))
}

/// Checks `value` for `setting` and returns it as the file stores it.
pub fn parse(setting: &Setting, value: &str) -> Result<toml::Value> {
    let value = value.trim();
    Ok(match setting.kind {
        Kind::Switch => toml::Value::Boolean(match value {
            "on" | "true" | "yes" | "1" => true,
            "off" | "false" | "no" | "0" => false,
            _ => bail!("`{}` takes on or off", setting.key),
        }),
        Kind::Number => toml::Value::Integer(
            value
                .parse::<u32>()
                .map_err(|_| anyhow::anyhow!("`{}` takes a whole number, such as 8", setting.key))?
                .into(),
        ),
        Kind::Rate => {
            parse_rate(value).map_err(anyhow::Error::msg)?;
            toml::Value::String(value.to_string())
        }
        Kind::Address => {
            value
                .parse::<std::net::IpAddr>()
                .with_context(|| format!("`{value}` is not an IP address"))?;
            toml::Value::String(value.to_string())
        }
        Kind::Text | Kind::Path => {
            if value.is_empty() {
                bail!(
                    "`{}` needs a value; `hiroi settings reset` clears it",
                    setting.key
                );
            }
            toml::Value::String(value.to_string())
        }
    })
}

/// The settings file's contents.
#[derive(Debug, Default)]
pub struct SettingsFile {
    values: BTreeMap<String, toml::Value>,
}

impl SettingsFile {
    pub fn path() -> PathBuf {
        std::env::var_os("HIROI_CONFIG")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| hiroi_daemon::paths::config_dir().join("config.toml"))
    }

    /// Reads the file; a missing file is an empty one.
    pub fn load() -> Result<Self> {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read {}", path.display()));
            }
        };
        let table: toml::Table = toml::from_str(&text)
            .with_context(|| format!("{} is not valid TOML", path.display()))?;
        let mut values = BTreeMap::new();
        for (key, value) in table {
            let setting = find(&key).with_context(|| format!("in {}", path.display()))?;
            if setting.home == Home::Service {
                bail!(
                    "`{key}` is kept by the service, not the file; use `hiroi settings set {key}`"
                );
            }
            // Values are checked as `set` checks them.
            let text = match &value {
                toml::Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            parse(setting, &text).with_context(|| format!("in {}", path.display()))?;
            values.insert(key, value);
        }
        Ok(Self { values })
    }

    pub fn get(&self, key: &str) -> Option<&toml::Value> {
        self.values.get(key)
    }

    pub fn switch(&self, key: &str) -> bool {
        self.get(key)
            .and_then(toml::Value::as_bool)
            .unwrap_or(false)
    }

    pub fn text(&self, key: &str) -> Option<String> {
        self.get(key)
            .and_then(toml::Value::as_str)
            .map(str::to_string)
    }

    pub fn number(&self, key: &str) -> Option<u64> {
        self.get(key)
            .and_then(toml::Value::as_integer)
            .and_then(|n| u64::try_from(n).ok())
    }

    pub fn rate(&self, key: &str) -> Option<u64> {
        match self.get(key)? {
            toml::Value::String(text) => parse_rate(text).ok(),
            toml::Value::Integer(n) => u64::try_from(*n).ok(),
            _ => None,
        }
    }

    pub fn set(&mut self, key: &str, value: toml::Value) {
        self.values.insert(key.to_string(), value);
    }

    pub fn remove(&mut self, key: &str) -> bool {
        self.values.remove(key).is_some()
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let table: toml::Table = self.values.clone().into_iter().collect();
        let text = format!(
            "# Hiroi settings. `hiroi settings list` shows every setting;\n\
             # a flag on the command line wins over this file.\n\n{}",
            toml::to_string(&table)?
        );
        write_private(&path, text.as_bytes())
            .with_context(|| format!("cannot write {}", path.display()))
    }
}

/// Writes through a temporary file, so a crash cannot leave half a
/// file, readable by the owner only.
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let temporary = path.with_extension("toml.new");
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&temporary)?;
        std::io::Write::write_all(&mut file, data)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_file_setting_is_a_flag() {
        use clap::CommandFactory;
        let command = crate::args::Cli::command();
        let get = command.find_subcommand("get").unwrap();
        for setting in SETTINGS.iter().filter(|s| s.home == Home::File) {
            assert!(
                get.get_arguments()
                    .any(|arg| arg.get_long() == Some(setting.key)),
                "--{} is missing",
                setting.key
            );
        }
    }

    #[test]
    fn values_are_checked() {
        let setting = |key| find(key).unwrap();
        assert_eq!(
            parse(setting("anonymous"), "on").unwrap(),
            toml::Value::Boolean(true)
        );
        assert!(parse(setting("anonymous"), "maybe").is_err());
        assert!(parse(setting("per-host"), "-3").is_err());
        assert!(parse(setting("min-speed"), "fast").is_err());
        assert!(parse(setting("bind-address"), "10.0.0.300").is_err());
        assert!(find("nonsense").is_err());
    }

    #[test]
    fn the_file_round_trips_and_rejects_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "proxy = \"socks5h://127.0.0.1:9050\"\nanonymous = true\nper-host = 4\n",
        )
        .unwrap();
        let file = SettingsFile::load_from(&path).unwrap();
        assert_eq!(
            file.text("proxy").as_deref(),
            Some("socks5h://127.0.0.1:9050")
        );
        assert!(file.switch("anonymous"));
        assert_eq!(file.number("per-host"), Some(4));

        std::fs::write(&path, "prxy = \"x\"\n").unwrap();
        assert!(SettingsFile::load_from(&path).is_err());
        std::fs::write(&path, "per-host = \"many\"\n").unwrap();
        assert!(SettingsFile::load_from(&path).is_err());
    }
}
