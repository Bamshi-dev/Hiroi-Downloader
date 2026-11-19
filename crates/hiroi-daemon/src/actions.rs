//! What happens after a job finishes: a desktop notification, the
//! user's own command, opening the folder, powering off.
//!
//! Every program runs directly with an argument list, never through a
//! shell, so a file name chosen by a server cannot inject commands.
//! Placeholders are replaced inside single arguments only.

use std::path::Path;

use tokio::process::Command;

/// Default command for "power off when done".
#[cfg(target_os = "macos")]
pub const POWER_OFF: &[&str] = &[
    "osascript",
    "-e",
    "tell application \"System Events\" to shut down",
];
/// Default command for "power off when done". A minute's grace, which
/// `shutdown /a` cancels.
#[cfg(windows)]
pub const POWER_OFF: &[&str] = &["shutdown", "/s", "/t", "60"];
/// Default command for "power off when done".
#[cfg(not(any(target_os = "macos", windows)))]
pub const POWER_OFF: &[&str] = &["systemctl", "poweroff"];

#[cfg(target_os = "macos")]
pub fn notify(title: &str, body: &str) {
    // The text goes in as arguments, not into the script, so a file
    // name cannot change what the script does.
    let script =
        "on run argv\ndisplay notification (item 2 of argv) with title (item 1 of argv)\nend run";
    spawn(
        &[
            "osascript".into(),
            "-e".into(),
            script.into(),
            title.into(),
            body.into(),
        ],
        "notification",
    );
}

#[cfg(windows)]
pub fn notify(title: &str, body: &str) {
    // No notification program ships with Windows that takes plain
    // arguments; the log has it instead.
    tracing::info!(title, body, "notification");
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn notify(title: &str, body: &str) {
    spawn(
        &[
            "notify-send".into(),
            "--app-name=Hiroi".into(),
            title.into(),
            body.into(),
        ],
        "notification",
    );
}

/// Runs the user's command for a finished file.
pub fn run_after(argv: &[String], path: &Path) {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dir = path
        .parent()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default();
    let full = path.to_string_lossy();
    let argv: Vec<String> = argv
        .iter()
        .map(|arg| {
            arg.replace("{path}", &full)
                .replace("{name}", &name)
                .replace("{dir}", &dir)
        })
        .collect();
    spawn(&argv, "command after download");
}

/// The program that opens a folder in the file manager.
#[cfg(target_os = "macos")]
const OPENER: &str = "open";
#[cfg(windows)]
const OPENER: &str = "explorer";
#[cfg(not(any(target_os = "macos", windows)))]
const OPENER: &str = "xdg-open";

pub fn open_folder(path: &Path) {
    if let Some(dir) = path.parent() {
        spawn(
            &[OPENER.into(), dir.to_string_lossy().into_owned()],
            "file manager",
        );
    }
}

pub fn power_off(argv: &[String]) {
    tracing::warn!(?argv, "every job is done: powering off as requested");
    spawn(argv, "power off");
}

/// Starts a program without waiting for it. A missing program (no
/// notify-send installed, say) is logged, not an error.
fn spawn(argv: &[String], what: &'static str) {
    let Some((program, args)) = argv.split_first() else {
        return;
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match command.spawn() {
        Ok(mut child) => {
            tokio::spawn(async move {
                if let Ok(status) = child.wait().await
                    && !status.success()
                {
                    tracing::warn!(what, %status, "program failed");
                }
            });
        }
        Err(error) => tracing::debug!(what, program, %error, "could not start program"),
    }
}
