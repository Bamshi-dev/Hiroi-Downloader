//! `hiroi`: command-line front end of Hiroi Downloader.

mod args;
mod commands;
mod options;
mod ranges;
mod settings;
mod size;
mod ui;

use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::args::{Cli, Command};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    let verbose = cli.verbose > 0;
    let result = match cli.command {
        Command::Get(args) => commands::get::run(args, verbose).await,
        Command::Info(args) => commands::info::run(args).await,
        Command::Exposure(args) => commands::exposure::run(args).await,
        Command::Grab(args) => commands::grab::run(args, cli.socket, verbose).await,
        Command::Daemon(args) => commands::daemon::run(args, cli.socket).await,
        Command::Browser(command) => commands::browser::run(command),
        Command::NativeHost { .. } => commands::browser::native_host(cli.socket).await,
        Command::Network(command) => commands::network::run(command),
        Command::Settings { command } => {
            let command = command.unwrap_or(args::SettingsCommand::List { section: None });
            commands::settings::run(command, cli.socket).await
        }
        Command::Search { words } => commands::settings::search(&words.join(" ")),
        command => client_command(command, cli.socket).await,
    };
    result.unwrap_or_else(|error| {
        eprintln!("error: {error:#}");
        ExitCode::FAILURE
    })
}

/// Commands that need a connection to the background service.
async fn client_command(
    command: Command,
    socket: Option<std::path::PathBuf>,
) -> anyhow::Result<ExitCode> {
    use commands::jobs;
    let mut client = jobs::connect(socket).await?;
    match command {
        Command::Add(args) => jobs::add(&mut client, args).await,
        Command::List => jobs::list(&mut client).await,
        Command::Pause(args) => jobs::pause_or_resume(&mut client, args, false).await,
        Command::Resume(args) => jobs::pause_or_resume(&mut client, args, true).await,
        Command::Cancel(args) => jobs::cancel_or_remove(&mut client, args, false).await,
        Command::Remove(args) => jobs::cancel_or_remove(&mut client, args, true).await,
        Command::Limit(args) => jobs::limit(&mut client, args).await,
        Command::Watch => jobs::watch(&mut client).await,
        Command::Queue(command) => jobs::queue(&mut client, command).await,
        Command::Schedule(command) => jobs::schedule(&mut client, command).await,
        Command::Category(command) => jobs::category(&mut client, command).await,
        Command::Clipboard { auto, interval } => {
            let interval = std::time::Duration::from_secs_f64(interval.clamp(0.2, 60.0));
            commands::clipboard::run(&mut client, auto, interval).await
        }
        Command::Notifications { state } => {
            let enabled = matches!(state, args::Switch::On);
            jobs::switch(
                &mut client,
                hiroi_proto::Request::SetNotifications { enabled },
            )
            .await
        }
        Command::PowerOffWhenDone { state } => {
            let enabled = matches!(state, args::Switch::On);
            jobs::switch(
                &mut client,
                hiroi_proto::Request::PowerOffWhenDone { enabled },
            )
            .await
        }
        Command::Stop => jobs::stop(&mut client).await,
        Command::Group(command) => jobs::group(&mut client, command).await,
        Command::SmartFolders { state } => {
            let enabled = matches!(state, args::Switch::On);
            jobs::switch(
                &mut client,
                hiroi_proto::Request::SetSmartFolders { enabled },
            )
            .await
        }
        Command::Get(_)
        | Command::Info(_)
        | Command::Exposure(_)
        | Command::Grab(_)
        | Command::Daemon(_)
        | Command::Browser(_)
        | Command::NativeHost { .. }
        | Command::Network(_)
        | Command::Settings { .. }
        | Command::Search { .. } => {
            unreachable!("handled in main")
        }
    }
}

fn init_logging(verbose: u8) {
    let default = match verbose {
        0 => "off",
        1 => "hiroi_engine=info",
        _ => "hiroi_engine=debug",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
