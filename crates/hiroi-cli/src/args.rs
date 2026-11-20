//! Command-line arguments.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use hiroi_engine::{ConflictPolicy, Quality};

use crate::size::{parse_duration, parse_rate};

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

    /// Socket of the background service.
    #[arg(long, global = true, value_name = "PATH", env = "HIROI_SOCKET")]
    pub socket: Option<PathBuf>,

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
    /// Show who would see a download, and what it would give away,
    /// without downloading it.
    Exposure(ExposureArgs),
    /// List what a web page offers (files, pictures, videos, streams),
    /// choose, and download.
    Grab(GrabArgs),
    /// Run the background service in the foreground.
    Daemon(DaemonArgs),
    /// Set up the browser extension's connection to Hiroi.
    #[command(subcommand)]
    Browser(BrowserCommand),
    /// Bridge for the browser extension; browsers start this themselves.
    #[command(hide = true)]
    NativeHost {
        /// Browsers pass their own arguments; they are ignored.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        ignored: Vec<String>,
    },
    /// Add downloads to the background service.
    Add(AddArgs),
    /// List the service's jobs.
    List,
    /// Pause jobs (an id, or `all`).
    Pause(JobsArg),
    /// Resume paused or failed jobs (an id, or `all`).
    Resume(JobsArg),
    /// Stop jobs and delete their temporary files.
    Cancel(IdsArg),
    /// Forget finished jobs; their files stay.
    Remove(IdsArg),
    /// Change the speed limit of a job, or of the service.
    Limit(LimitArgs),
    /// Show live progress of the service's jobs.
    Watch,
    /// List, start, or hold queues.
    #[command(subcommand)]
    Queue(QueueCommand),
    /// Start or hold queues and limit the speed at set times.
    #[command(subcommand)]
    Schedule(ScheduleCommand),
    /// Folders by file type for downloads that name no folder.
    #[command(subcommand)]
    Category(CategoryCommand),
    /// Offer copied download links to the service.
    Clipboard {
        /// Add links without asking.
        #[arg(long)]
        auto: bool,
        /// Seconds between clipboard checks.
        #[arg(long, default_value_t = 1.0)]
        interval: f64,
    },
    /// Desktop notifications when downloads finish.
    Notifications { state: Switch },
    /// Power the computer off once every download is done.
    PowerOffWhenDone { state: Switch },
    /// Stop the background service; running jobs continue next time.
    Stop,
    /// List, pause, or resume groups of jobs.
    #[command(subcommand)]
    Group(GroupCommand),
    /// Move finished episodes of a series (`talk 1`, `talk 2`, ...) into
    /// a folder named after it. On by default.
    SmartFolders { state: Switch },
    /// Network interfaces, for binding downloads to a VPN.
    #[command(subcommand)]
    Network(NetworkCommand),
}

#[derive(Debug, Subcommand)]
pub enum GroupCommand {
    /// Groups with their progress.
    List,
    /// Pause every job of a group.
    Pause { name: String },
    /// Resume every job of a group.
    Resume { name: String },
}

#[derive(Debug, Subcommand)]
pub enum NetworkCommand {
    /// List interfaces and their addresses, with likely VPNs marked.
    Interfaces,
    /// Check an IP filter file and count what it blocks.
    CheckFilter {
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },
}

#[derive(Debug, Args)]
pub struct GrabArgs {
    /// The page to read.
    #[arg(value_name = "URL")]
    pub url: String,

    /// Also read pages it links to, this many levels deep.
    #[arg(long, default_value_t = 0)]
    pub depth: u32,

    /// Follow links to other sites too.
    #[arg(long)]
    pub other_hosts: bool,

    /// Stop after reading this many pages.
    #[arg(long, value_name = "N", default_value_t = 50)]
    pub max_pages: usize,

    /// Ignore robots.txt while following links (be considerate).
    #[arg(long)]
    pub ignore_robots: bool,

    /// Only these types: video, audio, image, stream, archive, document,
    /// program, disk-image, subtitle, torrent, metalink, other.
    #[arg(
        short = 't',
        long = "type",
        value_name = "TYPES",
        value_delimiter = ','
    )]
    pub kinds: Vec<String>,

    /// Only these extensions, e.g. mp4,zip.
    #[arg(long = "ext", value_name = "EXTS", value_delimiter = ',')]
    pub extensions: Vec<String>,

    /// Only links whose URL or text contains this.
    #[arg(long, value_name = "TEXT")]
    pub contains: Option<String>,

    /// Ask each server for size and type first (needed for size filters).
    #[arg(long)]
    pub probe: bool,

    #[arg(long, value_name = "SIZE", value_parser = parse_rate)]
    pub min_size: Option<u64>,

    #[arg(long, value_name = "SIZE", value_parser = parse_rate)]
    pub max_size: Option<u64>,

    /// Only list what was found.
    #[arg(long)]
    pub list: bool,

    /// Download everything that matches.
    #[arg(long, conflicts_with_all = ["first", "pick"])]
    pub all: bool,

    /// Download the first N matches.
    #[arg(long, value_name = "N", conflicts_with = "pick")]
    pub first: Option<usize>,

    /// Download these, by number: 1,3,5-9.
    #[arg(long, value_name = "LIST")]
    pub pick: Option<String>,

    /// Send the chosen links to the background service instead.
    #[arg(long)]
    pub queue: bool,

    /// Group for the queued links; named after them or the page when
    /// not given.
    #[arg(long, value_name = "NAME", requires = "queue")]
    pub group: Option<String>,

    /// Downloads running at the same time.
    #[arg(short, long, default_value_t = 3)]
    pub jobs: usize,

    #[command(flatten)]
    pub target: TargetArgs,

    #[command(flatten)]
    pub request: RequestArgs,

    #[command(flatten)]
    pub engine: EngineArgs,
}

#[derive(Debug, Args)]
pub struct DaemonArgs {
    /// Database file (default: ~/.local/share/hiroi/hiroi.db).
    #[arg(long, value_name = "PATH")]
    pub database: Option<PathBuf>,

    /// Folder for downloads that don't name one (default: ~/Downloads).
    #[arg(short, long, value_name = "DIR")]
    pub dir: Option<PathBuf>,

    /// Downloads running at the same time.
    #[arg(short, long, default_value_t = 3)]
    pub jobs: usize,

    #[command(flatten)]
    pub engine: EngineArgs,
}

#[derive(Debug, Args)]
pub struct AddArgs {
    /// URLs to add.
    #[arg(value_name = "URL", required_unless_present = "input")]
    pub urls: Vec<String>,

    /// Read URLs from a file, one per line (`#` starts a comment).
    #[arg(short, long, value_name = "FILE")]
    pub input: Option<PathBuf>,

    #[command(flatten)]
    pub target: TargetArgs,

    #[command(flatten)]
    pub request: RequestArgs,

    /// Queue to put the jobs in.
    #[arg(short, long, value_name = "NAME")]
    pub queue: Option<String>,

    /// Group to put the jobs in. Several URLs get a group named after
    /// them when this is not given.
    #[arg(short, long, value_name = "NAME")]
    pub group: Option<String>,

    /// Add without starting.
    #[arg(long)]
    pub paused: bool,

    /// Start before other queued jobs.
    #[arg(long)]
    pub high_priority: bool,

    /// Add even if the same file is already in the list.
    #[arg(long)]
    pub allow_duplicate: bool,

    /// Run a command when the download completes, e.g.
    /// `--then 'sha256sum {path}'`. Split like a shell would, but run
    /// without one; {path}, {name}, and {dir} are replaced.
    #[arg(long, value_name = "COMMAND")]
    pub then: Option<String>,

    /// Open the folder in the file manager when the download completes.
    #[arg(long)]
    pub open_folder: bool,
}

#[derive(Debug, Subcommand)]
pub enum BrowserCommand {
    /// Register the native-messaging host with your browsers.
    Install {
        /// Firefox.
        #[arg(long)]
        firefox: bool,
        /// Chrome, Chromium, Brave, Edge, or Vivaldi, with the extension's
        /// ID from chrome://extensions.
        #[arg(long, value_name = "EXTENSION_ID")]
        chrome: Option<String>,
    },
    /// Remove the host registrations.
    Uninstall,
}

#[derive(Debug, Subcommand)]
pub enum QueueCommand {
    /// Queues with their state and number of jobs.
    List,
    /// Let a queue's jobs run.
    Start { name: String },
    /// Hold a queue: its jobs pause and wait until it starts again.
    Stop { name: String },
}

#[derive(Debug, Subcommand)]
pub enum CategoryCommand {
    List,
    /// Sort new downloads into category folders.
    On,
    /// Save new downloads directly into the download folder.
    Off,
    /// Add or replace a category.
    Set {
        name: String,
        /// Folder inside the download folder.
        #[arg(long)]
        folder: String,
        /// Extensions, comma separated: mp4,mkv
        #[arg(long, value_delimiter = ',', required = true)]
        ext: Vec<String>,
    },
    Remove {
        name: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum ScheduleCommand {
    List,
    /// Add a rule, e.g. `--queue night --from 01:00 --to 07:00 --days weekdays`.
    Add(ScheduleArgs),
    Remove {
        id: i64,
    },
}

#[derive(Debug, Args)]
#[group(id = "action", required = true, multiple = false, args = ["queue", "limit"])]
pub struct ScheduleArgs {
    /// Run this queue during the window (or start it at --from).
    #[arg(long, value_name = "NAME")]
    pub queue: Option<String>,

    /// Limit the speed of everything during the window, e.g. 1M.
    #[arg(long, value_name = "RATE", value_parser = parse_rate)]
    pub limit: Option<u64>,

    /// Start time, HH:MM.
    #[arg(long, value_name = "HH:MM")]
    pub from: String,

    /// End time, HH:MM. Without it, the action happens once at --from.
    #[arg(long, value_name = "HH:MM")]
    pub to: Option<String>,

    /// Days: daily, weekdays, weekends, or e.g. mon-fri, sat,sun.
    #[arg(long, value_name = "DAYS", default_value = "daily")]
    pub days: String,
}

#[derive(Debug, Args)]
pub struct JobsArg {
    /// A job id, or `all`.
    #[arg(value_name = "ID|all")]
    pub job: String,
}

#[derive(Debug, Args)]
pub struct IdsArg {
    #[arg(value_name = "ID", required = true)]
    pub jobs: Vec<i64>,
}

#[derive(Debug, Args)]
pub struct LimitArgs {
    /// A speed such as 2M, or `off`.
    #[arg(value_name = "RATE|off")]
    pub rate: String,

    /// Limit one job instead of the whole service.
    #[arg(long, value_name = "ID")]
    pub job: Option<i64>,
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
    pub request: RequestArgs,

    #[command(flatten)]
    pub engine: EngineArgs,

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
    pub request: RequestArgs,

    #[command(flatten)]
    pub engine: EngineArgs,
}

#[derive(Debug, Args)]
pub struct ExposureArgs {
    /// The URL to look at.
    #[arg(value_name = "URL")]
    pub url: String,

    /// Another server with the same file (repeatable).
    #[arg(short, long = "mirror", value_name = "URL")]
    pub mirrors: Vec<String>,

    /// Also ask an address-echo service which address servers see.
    /// This contacts that service.
    #[arg(long)]
    pub check_ip: bool,

    /// With --check-ip, ask once more without the proxy to compare.
    /// This shows your real address to the service.
    #[arg(long, requires = "check_ip")]
    pub direct: bool,

    /// Address-echo service for --check-ip; it must answer a GET with
    /// the caller's address as plain text.
    #[arg(long, value_name = "URL", default_value = "https://api.ipify.org")]
    pub ip_service: String,

    #[command(flatten)]
    pub request: RequestArgs,

    #[command(flatten)]
    pub engine: EngineArgs,
}

/// Where and how one download is saved.
#[derive(Debug, Args)]
pub struct TargetArgs {
    /// Folder to save into (default: the current folder for `get`, the
    /// service's download folder for `add`).
    #[arg(short, long, value_name = "DIR")]
    pub dir: Option<PathBuf>,

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

    /// Stream variant to fetch: best, worst, or a height such as 720.
    #[arg(long, value_name = "QUALITY", default_value = "best")]
    pub quality: Quality,

    /// Stop recording a live stream after this long, e.g. 30m or 2h.
    #[arg(long, value_name = "TIME", value_parser = parse_duration)]
    pub live_limit: Option<Duration>,

    /// The URL is a video page: let yt-dlp find the media on it.
    #[arg(short = 'x', long)]
    pub extract: bool,

    /// yt-dlp format selection, e.g. `bv*+ba/b` (implies --extract).
    #[arg(short = 'f', long, value_name = "SPEC")]
    pub format: Option<String>,
}

/// What each request carries. Applies to one download.
#[derive(Debug, Args)]
pub struct RequestArgs {
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
}

/// How the engine talks to servers. Applies to everything it downloads.
#[derive(Debug, Args)]
pub struct EngineArgs {
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

    /// Refuse to run without --proxy, so a forgotten setting cannot
    /// expose your address.
    #[arg(long, env = "HIROI_REQUIRE_PROXY", value_parser = clap::builder::BoolishValueParser::new())]
    pub require_proxy: bool,

    /// Remove tracking parameters (utm_source, fbclid, ...) from links.
    #[arg(long)]
    pub strip_tracking: bool,

    /// Never send a Referer, not even one the browser handed over.
    #[arg(long)]
    pub no_referer: bool,

    /// Ignore cookies servers set during a download. Some download pages
    /// set a session cookie first and fail without it.
    #[arg(long)]
    pub no_cookie_jar: bool,

    /// Do not flush downloads to disk before calling them saved or done.
    /// Faster on slow disks; a power cut can then leave holes in a file.
    #[arg(long)]
    pub no_sync: bool,

    /// Connect only through this network interface, e.g. a VPN's tun0 or
    /// wg0; downloads stop when it goes down. See `hiroi network interfaces`.
    #[arg(long, value_name = "NAME")]
    pub interface: Option<String>,

    /// Connect from this local address.
    #[arg(long, value_name = "IP")]
    pub bind_address: Option<std::net::IpAddr>,

    /// Never connect to the addresses in this file (eMule .dat,
    /// PeerGuardian .p2p, or one CIDR range per line).
    #[arg(long, value_name = "FILE")]
    pub ip_filter: Option<PathBuf>,

    /// Let remote sites redirect downloads into the local network
    /// (localhost, the router, the LAN).
    #[arg(long)]
    pub allow_local_redirects: bool,

    /// No Hiroi User-Agent, no Referer, no tracking parameters.
    #[arg(long)]
    pub anonymous: bool,

    /// Accept TLS certificates that do not check out (unsafe: anyone on
    /// the way can read and change the download).
    #[arg(long)]
    pub no_check_certificate: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Switch {
    On,
    Off,
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
