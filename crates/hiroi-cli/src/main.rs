//! `hiroi`: command-line front end of Hiroi Downloader.

mod args;
mod commands;
mod options;
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
    };
    result.unwrap_or_else(|error| {
        eprintln!("error: {error:#}");
        ExitCode::FAILURE
    })
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
