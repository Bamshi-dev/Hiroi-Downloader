//! Command-line arguments.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use hiroi_engine::ConflictPolicy;

use crate::size::parse_rate;

#[derive(Debug, Parser)]
#[command(
    name = "hiroi",
    version,
    about = "Hiroi Downloader: fast, resumable multi-connection downloads",
    max_term_width = 100
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Log what the engine does (-v info, -vv debug). Replaces the progress bars.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Download files in the foreground.
    Get(GetArgs),
    /// Ask the servers about a file without downloading it.
    Info(InfoArgs),
}

#[derive(Debug, Args)]
pub struct GetArgs {
    /// URLs to download.
    #[arg(value_name = "URL", required_unless_present = "input")]
    pub urls: Vec<String>,

    /// Read URLs from a file, one per line (`#` starts a comment).
    #[arg(short, long, value_name = "FILE")]
    pub input: Option<PathBuf>,

    #[command(flatten)]
    pub target: TargetArgs,

    /// Downloads running at the same time.
    #[arg(short, long, default_value_t = 3)]
    pub jobs: usize,

    /// Speed limit for all downloads together.
    #[arg(long, value_name = "RATE", value_parser = parse_rate)]
    pub global_limit: Option<u64>,

    #[command(flatten)]
    pub network: NetworkArgs,

    /// No progress output.
    #[arg(short, long)]
    pub quiet: bool,
}

#[derive(Debug, Args)]
pub struct InfoArgs {
    /// The URL to look at.
    #[arg(value_name = "URL")]
    pub url: String,

    /// Another server with the same file (repeatable).
    #[arg(short, long = "mirror", value_name = "URL")]
    pub mirrors: Vec<String>,

    #[command(flatten)]
    pub network: NetworkArgs,
}

/// Where and how one download is saved.
#[derive(Debug, Args)]
pub struct TargetArgs {
    /// Folder to save into.
    #[arg(short, long, value_name = "DIR", default_value = ".")]
    pub dir: PathBuf,

    /// File name to save as (single download only).
    #[arg(short, long, value_name = "NAME")]
    pub output: Option<String>,

    /// Another server with the same file (single download only, repeatable).
    #[arg(short, long = "mirror", value_name = "URL")]
    pub mirrors: Vec<String>,

    /// Connections per download (1-32).
    #[arg(short, long, default_value_t = 8, value_parser = clap::value_parser!(u8).range(1..=32))]
    pub connections: u8,

    /// Speed limit per download, e.g. 500K, 2M, 1.5G.
    #[arg(short, long, value_name = "RATE", value_parser = parse_rate)]
    pub limit: Option<u64>,

    /// Verify the finished file, e.g. sha256:<hex> (single download only).
    #[arg(long, value_name = "ALGO:HEX")]
    pub checksum: Option<String>,

    /// What to do when the file already exists.
    #[arg(long, value_enum, default_value_t = Conflict::Rename)]
    pub on_conflict: Conflict,
}

/// How requests are sent. Shared by every command that talks to servers.
#[derive(Debug, Args)]
pub struct NetworkArgs {
    /// Extra request header (repeatable).
    #[arg(short = 'H', long = "header", value_name = "NAME: VALUE")]
    pub headers: Vec<String>,

    /// Cookies to send, as copied from the browser: `a=1; b=2`.
    #[arg(long, value_name = "COOKIES")]
    pub cookie: Option<String>,

    /// Referer to send.
    #[arg(long, value_name = "URL")]
    pub referer: Option<String>,

    /// User agent to send instead of the default.
    #[arg(long, value_name = "UA")]
    pub user_agent: Option<String>,

    /// Proxy for all requests, e.g. http://host:3128 or socks5h://host:1080.
    #[arg(long, value_name = "URL")]
    pub proxy: Option<String>,

    /// Most connections to one server, across all downloads.
    #[arg(long, value_name = "N", default_value_t = 16)]
    pub per_host: usize,

    /// Reconnect connections slower than this, e.g. 50K.
    #[arg(long, value_name = "RATE", value_parser = parse_rate)]
    pub min_speed: Option<u64>,

    /// Don't reserve disk space before downloading.
    #[arg(long)]
    pub no_prealloc: bool,

    /// Follow redirects from HTTPS to plain HTTP (unsafe).
    #[arg(long)]
    pub allow_insecure_redirects: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Conflict {
    /// Save as `name (1).ext`.
    Rename,
    Overwrite,
    /// Leave the existing file and report an error.
    Skip,
}

impl From<Conflict> for ConflictPolicy {
    fn from(conflict: Conflict) -> Self {
        match conflict {
            Conflict::Rename => Self::Rename,
            Conflict::Overwrite => Self::Overwrite,
            Conflict::Skip => Self::Skip,
        }
    }
}
