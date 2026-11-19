//! `hiroi get`: download files in the foreground.

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use hiroi_engine::{DownloadId, DownloadStatus, Engine};
use indicatif::HumanBytes;

use crate::args::GetArgs;
use crate::options;
use crate::ui::ProgressView;

/// Exit code after Ctrl+C, as shells expect.
const EXIT_INTERRUPTED: u8 = 130;

pub async fn run(args: GetArgs, verbose: bool) -> Result<ExitCode> {
    let urls = collect_urls(&args)?;
    let target = &args.target;
    let single = urls.len() == 1;
    if !single
        && (target.output.is_some() || !target.mirrors.is_empty() || target.checksum.is_some())
    {
        bail!("--output, --mirror, and --checksum only work with a single URL");
    }

    let mut config = options::engine_config(&args.network);
    config.max_active_downloads = args.jobs;
    config.global_speed_limit = args.global_limit;
    let engine = Engine::new(config)?;
    let view = Arc::new(ProgressView::new(args.quiet || verbose));
    engine.add_listener(view.clone());

    let requests = urls
        .iter()
        .map(|url| {
            options::download_request(url, target, &args.network)
                .with_context(|| format!("bad download `{url}`"))
        })
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

fn collect_urls(args: &GetArgs) -> Result<Vec<String>> {
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
