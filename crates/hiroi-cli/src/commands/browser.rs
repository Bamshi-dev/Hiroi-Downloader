//! Browser integration: the native-messaging host and its installation.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use hiroi_daemon::native::{self, Browser, HOST_NAME};

use crate::args::BrowserCommand;

/// The ID of the Firefox extension, set in its manifest.
pub const FIREFOX_EXTENSION_ID: &str = "{70e8535c-40cf-424f-907d-a682e59d03b4}";

/// Where Firefox looks for host manifests, relative to the home folder.
#[cfg(target_os = "macos")]
const FIREFOX_DIR: &str = "Library/Application Support/Mozilla/NativeMessagingHosts";
#[cfg(not(target_os = "macos"))]
const FIREFOX_DIR: &str = ".mozilla/native-messaging-hosts";

/// Profile folders of Chromium-based browsers, relative to the home
/// folder; each looks for host manifests in `NativeMessagingHosts`.
#[cfg(target_os = "macos")]
const CHROMIUM_DIRS: &[&str] = &[
    "Library/Application Support/Google/Chrome",
    "Library/Application Support/Chromium",
    "Library/Application Support/BraveSoftware/Brave-Browser",
    "Library/Application Support/Microsoft Edge",
    "Library/Application Support/Vivaldi",
];
#[cfg(not(target_os = "macos"))]
const CHROMIUM_DIRS: &[&str] = &[
    ".config/google-chrome",
    ".config/chromium",
    ".config/BraveSoftware/Brave-Browser",
    ".config/microsoft-edge",
    ".config/vivaldi",
];

/// Started by the browser; talks native messaging on stdin/stdout.
pub async fn native_host(socket: Option<PathBuf>) -> Result<ExitCode> {
    let socket = socket.unwrap_or_else(hiroi_daemon::paths::default_socket);
    native::serve(tokio::io::stdin(), tokio::io::stdout(), &socket).await?;
    Ok(ExitCode::SUCCESS)
}

pub fn run(command: BrowserCommand) -> Result<ExitCode> {
    if cfg!(windows) {
        bail!(
            "on Windows, browsers find native hosts through the registry, \
             which `hiroi browser` does not set up yet"
        );
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    match command {
        BrowserCommand::Install { firefox, chrome } => {
            if !firefox && chrome.is_none() {
                bail!("say which browser: --firefox and/or --chrome EXTENSION_ID");
            }
            let launcher = write_launcher()?;
            if firefox {
                let dir = home.join(FIREFOX_DIR);
                write_manifest(
                    &dir,
                    &native::manifest(&launcher, Browser::Firefox, FIREFOX_EXTENSION_ID),
                )?;
            }
            if let Some(id) = chrome {
                if !id.chars().all(|c| c.is_ascii_lowercase()) || id.len() != 32 {
                    bail!(
                        "`{id}` is not a Chromium extension ID (32 letters a-p, shown on chrome://extensions)"
                    );
                }
                let mut found = false;
                for dir in CHROMIUM_DIRS.iter().map(|dir| home.join(dir)) {
                    if dir.is_dir() {
                        let manifest = native::manifest(&launcher, Browser::Chromium, &id);
                        write_manifest(&dir.join("NativeMessagingHosts"), &manifest)?;
                        found = true;
                    }
                }
                if !found {
                    bail!("no Chromium-based browser profile found");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        BrowserCommand::Uninstall => {
            let file = format!("{HOST_NAME}.json");
            let mut dirs = vec![home.join(FIREFOX_DIR)];
            dirs.extend(
                CHROMIUM_DIRS
                    .iter()
                    .map(|dir| home.join(dir).join("NativeMessagingHosts")),
            );
            for path in dirs
                .into_iter()
                .map(|dir| dir.join(&file))
                .filter(|path| path.exists())
            {
                std::fs::remove_file(&path)?;
                println!("Removed {}", path.display());
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Browsers start the host without arguments of our choosing, so the
/// manifest points at a small script that runs `hiroi native-host`.
fn write_launcher() -> Result<PathBuf> {
    let program = std::env::current_exe().context("cannot find the hiroi program")?;
    let dir = hiroi_daemon::paths::data_dir();
    std::fs::create_dir_all(&dir)?;
    let launcher = dir.join("native-host");
    let quoted = shell_words::quote(&program.to_string_lossy()).into_owned();
    std::fs::write(&launcher, format!("#!/bin/sh\nexec {quoted} native-host\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(launcher)
}

fn write_manifest(dir: &Path, manifest: &serde_json::Value) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{HOST_NAME}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(manifest)?)?;
    println!("Wrote {}", path.display());
    Ok(())
}
