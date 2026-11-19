//! `hiroi`: command-line front end of Hiroi Downloader.

mod args;
mod size;
mod ui;

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::Parser;
use hiroi_engine::{Checksum, DownloadId, DownloadRequest, DownloadStatus, Engine, EngineConfig};
use indicatif::HumanBytes;
use tracing_subscriber::EnvFilter;

use crate::args::Args;
use crate::ui::ProgressView;

/// Exit code after Ctrl+C, as shells expect.
const EXIT_INTERRUPTED: u8 = 130;

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    init_logging(args.verbose);
    match run(args).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<ExitCode> {
    let urls = collect_urls(&args)?;
    let single = urls.len() == 1;
    if !single && (args.output.is_some() || !args.mirrors.is_empty() || args.checksum.is_some()) {
        bail!("--output, --mirror, and --checksum only work with a single URL");
    }

    let engine = Engine::new(engine_config(&args))?;
    let view = Arc::new(ProgressView::new(args.quiet || args.verbose > 0));
    engine.add_listener(view.clone());

    let requests = urls
        .iter()
        .map(|url| build_request(&args, url).with_context(|| format!("bad download `{url}`")))
        .collect::<Result<Vec<_>>>()?;
    let ids: Vec<DownloadId> = requests
        .into_iter()
        .map(|request| engine.add(request))
        .collect();

    let interrupted = tokio::select! {
        () = wait_all(&engine, &ids) => false,
        _ = tokio::signal::ctrl_c() => true,
    };
    if interrupted {
        view.note("Pausing, saving progress…");
        engine.pause_all().await;
        view.note("Paused. Run the same command again to resume.");
        return Ok(ExitCode::from(EXIT_INTERRUPTED));
    }
    Ok(report(&engine, &ids, &view, args.quiet))
}

fn engine_config(args: &Args) -> EngineConfig {
    let mut config = EngineConfig {
        max_active_downloads: args.jobs,
        max_connections_per_host: args.per_host,
        default_connections: usize::from(args.connections),
        global_speed_limit: args.global_limit,
        proxy: args.proxy.clone(),
        preallocate: !args.no_prealloc,
        allow_insecure_redirects: args.allow_insecure_redirects,
        ..EngineConfig::default()
    };
    if let Some(user_agent) = &args.user_agent {
        config.user_agent = user_agent.clone();
    }
    config
}

fn build_request(args: &Args, url: &str) -> Result<DownloadRequest> {
    let mut request = DownloadRequest::new(url)?
        .output_dir(&args.dir)
        .connections(usize::from(args.connections))
        .speed_limit(args.limit)
        .on_conflict(args.on_conflict.into());
    for mirror in &args.mirrors {
        request = request.mirror(mirror)?;
    }
    for header in &args.headers {
        let (name, value) = header
            .split_once(':')
            .with_context(|| format!("header `{header}` must look like `Name: value`"))?;
        request = request.header(name, value)?;
    }
    if let Some(cookie) = &args.cookie {
        request = request.cookies(cookie)?;
    }
    if let Some(referer) = &args.referer {
        request = request.referer(referer)?;
    }
    if let Some(name) = &args.output {
        request = request.file_name(name);
    }
    if let Some(checksum) = &args.checksum {
        request = request.checksum(checksum.parse::<Checksum>()?);
    }
    Ok(request)
}

fn collect_urls(args: &Args) -> Result<Vec<String>> {
    let mut urls = args.urls.clone();
    if let Some(path) = &args.input {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("could not read {}", path.display()))?;
        let lines = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'));
        urls.extend(lines.map(str::to_string));
    }
    if urls.is_empty() {
        bail!("no URLs to download");
    }
    Ok(urls)
}

async fn wait_all(engine: &Engine, ids: &[DownloadId]) {
    for &id in ids {
        engine.wait(id).await;
    }
}

/// Prints where each file went and picks the exit code.
fn report(engine: &Engine, ids: &[DownloadId], view: &ProgressView, quiet: bool) -> ExitCode {
    view.finish();
    let mut failed = 0;
    for snapshot in ids.iter().filter_map(|&id| engine.snapshot(id)) {
        match &snapshot.status {
            DownloadStatus::Completed if !quiet => {
                let path = snapshot.path.as_deref().map(|p| p.display().to_string());
                let size = HumanBytes(snapshot.downloaded);
                println!("Saved {} ({size})", path.unwrap_or_default());
            }
            DownloadStatus::Failed(message) => {
                failed += 1;
                eprintln!("Failed: {message}");
            }
            _ => {}
        }
    }
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
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
