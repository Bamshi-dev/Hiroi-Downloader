//! `hiroi daemon`: run the background service in the foreground.
//!
//! Run it from a systemd user unit (see `contrib/hiroi.service`) to have
//! it start with the session.

use std::process::ExitCode;

use anyhow::Result;
use hiroi_daemon::DaemonConfig;

use crate::args::DaemonArgs;
use crate::options;

pub async fn run(args: DaemonArgs, socket: Option<std::path::PathBuf>) -> Result<ExitCode> {
    let mut engine = options::engine_config(&args.engine)?;
    engine.max_active_downloads = args.jobs;
    let mut config = DaemonConfig::with_default_paths(engine);
    if let Some(socket) = socket {
        config.socket = socket;
    }
    if let Some(database) = args.database {
        config.database = database;
    }
    if let Some(dir) = args.dir {
        config.download_dir = std::path::absolute(dir)?;
    }
    // Printed once the socket accepts connections, so a script that
    // waits for this line can talk to the service right away.
    let ready = |socket: &std::path::Path| {
        eprintln!("Hiroi service listening on {}", socket.display());
    };
    hiroi_daemon::run(config, ready, shutdown_signal()).await?;
    eprintln!("Hiroi service stopped; downloads were saved.");
    Ok(ExitCode::SUCCESS)
}

/// Ctrl+C, or SIGTERM as sent by systemd.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate()).expect("signal handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
