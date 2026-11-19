//! What happens after a job finishes: a desktop notification, the
//! user's own command, opening the folder, powering off.
//!
//! Every program runs directly with an argument list, never through a
//! shell, so a file name chosen by a server cannot inject commands.
//! Placeholders are replaced inside single arguments only.

use std::path::Path;

use tokio::process::Command;

/// Default command for "power off when done".
pub const POWER_OFF: &[&str] = &["systemctl", "poweroff"];

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

pub fn open_folder(path: &Path) {
    if let Some(dir) = path.parent() {
        spawn(
            &["xdg-open".into(), dir.to_string_lossy().into_owned()],
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
