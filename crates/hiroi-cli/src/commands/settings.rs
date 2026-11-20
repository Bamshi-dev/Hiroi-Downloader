//! `hiroi settings`: show and change settings. `hiroi search`: find a
//! setting, command, or option by what it does.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::CommandFactory;
use hiroi_proto::{Request, Response, ServiceSettings};
use indicatif::HumanBytes;

use crate::args::{Cli, SettingsCommand};
use crate::commands::jobs;
use crate::settings::{self, Home, Kind, SETTINGS, Setting, SettingsFile};
use crate::size::parse_rate;

pub async fn run(command: SettingsCommand, socket: Option<PathBuf>) -> Result<ExitCode> {
    match command {
        SettingsCommand::List { section } => list(section.as_deref(), socket).await,
        SettingsCommand::Get { key } => {
            let setting = settings::find(&key)?;
            let service = service_settings(setting, socket).await;
            println!(
                "{}",
                current(setting, &SettingsFile::load()?, service.as_ref())
            );
            Ok(ExitCode::SUCCESS)
        }
        SettingsCommand::Set { key, value } => set(&key, Some(&value), socket).await,
        SettingsCommand::Reset { key } => set(&key, None, socket).await,
        SettingsCommand::Path => {
            println!("{}", SettingsFile::path().display());
            Ok(ExitCode::SUCCESS)
        }
        SettingsCommand::Search { words } => search(&words.join(" ")),
    }
}

async fn list(section: Option<&str>, socket: Option<PathBuf>) -> Result<ExitCode> {
    let file = SettingsFile::load()?;
    let service = match jobs::connect(socket).await {
        Ok(mut client) => match client.call(Request::ServiceSettings).await {
            Ok(Response::ServiceSettings(settings)) => Some(settings),
            _ => None,
        },
        Err(_) => None,
    };
    let mut shown = 0;
    let mut last_section = "";
    for setting in SETTINGS
        .iter()
        .filter(|s| section.is_none_or(|wanted| s.section == wanted))
    {
        if setting.section != last_section {
            if !last_section.is_empty() {
                println!();
            }
            println!("[{}]", setting.section);
            last_section = setting.section;
        }
        println!(
            "  {:<26} {}",
            setting.key,
            current(setting, &file, service.as_ref())
        );
        println!("  {:<26} {}", "", setting.summary);
        shown += 1;
    }
    if shown == 0 {
        bail!("no section `{}`", section.unwrap_or_default());
    }
    println!();
    println!("File: {}", SettingsFile::path().display());
    Ok(ExitCode::SUCCESS)
}

/// The setting's value, and where it comes from.
fn current(setting: &Setting, file: &SettingsFile, service: Option<&ServiceSettings>) -> String {
    match setting.home {
        Home::File => match file.get(setting.key) {
            Some(value) => match value {
                toml::Value::String(text) => text.clone(),
                other => other.to_string(),
            },
            None => default(setting),
        },
        Home::Service => match service {
            None => format!("{} (service not running)", default(setting)),
            Some(service) => match setting.key {
                "notifications" => on_off(service.notifications),
                "smart-folders" => on_off(service.smart_folders),
                "speed-limit" => service
                    .global_speed_limit
                    .map_or("unlimited".into(), |limit| {
                        format!("{}/s", HumanBytes(limit))
                    }),
                _ => default(setting),
            },
        },
    }
}

fn default(setting: &Setting) -> String {
    match (setting.default, setting.kind) {
        ("", Kind::Rate) => "unlimited (default)".into(),
        ("", _) => "not set".into(),
        ("true", _) => "on (default)".into(),
        ("false", _) => "off (default)".into(),
        (value, _) => format!("{value} (default)"),
    }
}

fn on_off(on: bool) -> String {
    if on { "on" } else { "off" }.into()
}

/// Sets `key` to `value`, or back to its default without one.
async fn set(key: &str, value: Option<&str>, socket: Option<PathBuf>) -> Result<ExitCode> {
    let setting = settings::find(key)?;
    match setting.home {
        Home::File => {
            let mut file = SettingsFile::load()?;
            match value {
                Some(value) => file.set(key, settings::parse(setting, value)?),
                None => {
                    file.remove(key);
                }
            }
            file.save()?;
            println!("Saved. A running service uses it once restarted (`hiroi stop`).");
        }
        Home::Service => {
            let value = value.unwrap_or(setting.default);
            let enabled = || -> Result<bool> {
                Ok(settings::parse(setting, value)?.as_bool().unwrap_or(false))
            };
            let request = match key {
                "notifications" => Request::SetNotifications {
                    enabled: enabled()?,
                },
                "smart-folders" => Request::SetSmartFolders {
                    enabled: enabled()?,
                },
                "speed-limit" => Request::SetSpeedLimit {
                    job: None,
                    limit: match value {
                        "" | "unlimited" | "0" => None,
                        rate => Some(parse_rate(rate).map_err(anyhow::Error::msg)?),
                    },
                },
                _ => unreachable!("every service setting is handled"),
            };
            let mut client = jobs::connect(socket).await?;
            client.call(request).await?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn service_settings(setting: &Setting, socket: Option<PathBuf>) -> Option<ServiceSettings> {
    if setting.home != Home::Service {
        return None;
    }
    let mut client = jobs::connect(socket).await.ok()?;
    match client.call(Request::ServiceSettings).await.ok()? {
        Response::ServiceSettings(settings) => Some(settings),
        _ => None,
    }
}

/// Something `hiroi search` can find.
struct Entry {
    group: &'static str,
    name: String,
    text: String,
}

/// Settings, commands, and options whose name or description contain
/// every word of `query`, closest matches first.
pub fn search(query: &str) -> Result<ExitCode> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        bail!("search for what? e.g. `hiroi search proxy`");
    }
    let mut found: Vec<(u8, &Entry)> = Vec::new();
    let entries = entries();
    for entry in &entries {
        let name = entry.name.to_lowercase();
        let haystack = format!("{name} {}", entry.text.to_lowercase());
        if !words.iter().all(|word| haystack.contains(word.as_str())) {
            continue;
        }
        let rank = if words.iter().all(|word| name.contains(word.as_str())) {
            0
        } else {
            1
        };
        found.push((rank, entry));
    }
    if found.is_empty() {
        println!("Nothing found for `{query}`.");
        return Ok(ExitCode::FAILURE);
    }
    found.sort_by_key(|(rank, entry)| (*rank, entry.name.len()));
    for group in ["Settings", "Commands", "Options"] {
        let in_group: Vec<_> = found.iter().filter(|(_, e)| e.group == group).collect();
        if in_group.is_empty() {
            continue;
        }
        println!("{group}:");
        for (_, entry) in in_group {
            println!("  {:<32} {}", entry.name, entry.text);
        }
        println!();
    }
    Ok(ExitCode::SUCCESS)
}

fn entries() -> Vec<Entry> {
    let mut entries: Vec<Entry> = SETTINGS
        .iter()
        .map(|setting| Entry {
            group: "Settings",
            name: setting.key.to_string(),
            text: format!("{} [{}]", setting.summary, setting.section),
        })
        .collect();
    // Options appear under many commands; list each once.
    let mut options: BTreeMap<String, (String, Vec<String>)> = BTreeMap::new();
    let mut stack = vec![(String::from("hiroi"), Cli::command())];
    while let Some((path, command)) = stack.pop() {
        for sub in command.get_subcommands() {
            if sub.is_hide_set() {
                continue;
            }
            let sub_path = format!("{path} {}", sub.get_name());
            entries.push(Entry {
                group: "Commands",
                name: sub_path.clone(),
                text: sub.get_about().map(|a| a.to_string()).unwrap_or_default(),
            });
            for arg in sub.get_arguments().filter(|arg| !arg.is_hide_set()) {
                let Some(long) = arg.get_long() else {
                    continue;
                };
                let help = arg.get_help().map(|h| h.to_string()).unwrap_or_default();
                let entry = options
                    .entry(format!("--{long}"))
                    .or_insert_with(|| (help, Vec::new()));
                entry.1.push(sub.get_name().to_string());
            }
            stack.push((sub_path, sub.clone()));
        }
    }
    entries.extend(options.into_iter().map(|(name, (help, mut commands))| {
        commands.sort();
        commands.dedup();
        Entry {
            group: "Options",
            name,
            text: format!("{help} ({})", commands.join(", ")),
        }
    }));
    entries
}
