//! Keeping the computer awake while downloads run, so a long download
//! is not cut off by sleep. The system's own tool holds the lock while
//! it runs: `systemd-inhibit` on Linux, `caffeinate` on macOS. Without
//! one, nothing happens. Windows has no such tool; its downloads do not
//! hold the computer awake yet.

use std::process::Stdio;

use tokio::process::{Child, Command};

#[cfg(target_os = "macos")]
const HOLD: &[&str] = &["caffeinate", "-i"];
#[cfg(not(target_os = "macos"))]
const HOLD: &[&str] = &[
    "systemd-inhibit",
    "--what=sleep:idle",
    "--who=Hiroi",
    "--why=Downloads are running",
    "--mode=block",
    "sleep",
    "infinity",
];

/// Holds the computer awake while [`KeepAwake::set`] says so.
#[derive(Debug, Default)]
pub(crate) struct KeepAwake {
    holder: Option<Child>,
    /// The tool was missing once; trying again on every change is noise.
    unavailable: bool,
}

impl KeepAwake {
    pub fn set(&mut self, awake: bool) {
        if cfg!(windows) || self.unavailable {
            return;
        }
        match (awake, self.holder.is_some()) {
            (true, false) => {
                let spawned = Command::new(HOLD[0])
                    .args(&HOLD[1..])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .kill_on_drop(true)
                    .spawn();
                match spawned {
                    Ok(child) => self.holder = Some(child),
                    Err(error) => {
                        tracing::info!(%error, program = HOLD[0], "cannot keep the computer awake");
                        self.unavailable = true;
                    }
                }
            }
            (false, true) => {
                if let Some(mut child) = self.holder.take() {
                    let _ = child.start_kill();
                }
            }
            _ => {}
        }
    }

    #[cfg(test)]
    fn is_holding(&self) -> bool {
        self.holder.is_some()
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_lock_follows_the_downloads() {
        let mut awake = KeepAwake::default();
        awake.set(true);
        if awake.unavailable {
            return;
        }
        assert!(awake.is_holding());
        awake.set(true);
        awake.set(false);
        assert!(!awake.is_holding());
    }
}
