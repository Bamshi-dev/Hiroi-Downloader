//! `hiroi grab`: list what a page offers, choose, and download.

use std::io::IsTerminal;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use hiroi_grab::{GrabOptions, Grabber, Link, LinkKind};
use hiroi_proto::NewBatch;
use indicatif::HumanBytes;

use crate::args::GrabArgs;
use crate::commands::{get, jobs};
use crate::options;

pub async fn run(
    args: GrabArgs,
    socket: Option<std::path::PathBuf>,
    verbose: bool,
) -> Result<ExitCode> {
    let start = hiroi_engine::Url::parse(&args.url)
        .with_context(|| format!("`{}` is not a URL", args.url))?;
    let grabber = Grabber::new(grab_options(&args)?)?;
    eprintln!("Reading {}…", args.url);
    let mut report = grabber.grab(&start).await?;
    if args.probe {
        eprintln!(
            "Asking the servers about {} links…",
            report.links.len() + report.page_links.len()
        );
        grabber.probe(&mut report.links, 8).await;
        grabber.probe(&mut report.page_links, 8).await;
        report.links.extend(
            report
                .page_links
                .drain(..)
                .filter(|link| link.kind != LinkKind::Page),
        );
    }
    for (page, reason) in &report.skipped {
        eprintln!("skipped {page}: {reason}");
    }

    let links: Vec<Link> = report
        .links
        .into_iter()
        .filter(|link| keep(link, &args))
        .collect();
    if links.is_empty() {
        eprintln!("Nothing matching found on {} page(s).", report.pages.len());
        return Ok(ExitCode::FAILURE);
    }
    print_table(&links);
    if args.list {
        return Ok(ExitCode::SUCCESS);
    }

    let chosen = choose(&links, &args)?;
    if chosen.is_empty() {
        eprintln!("Nothing selected.");
        return Ok(ExitCode::SUCCESS);
    }
    let chosen: Vec<&Link> = chosen.into_iter().map(|index| &links[index]).collect();
    if args.queue {
        queue(&chosen, &args, report.title.clone(), socket).await
    } else {
        let requests = chosen
            .iter()
            .map(|link| {
                let mut request =
                    options::download_request(link.url.as_str(), &args.target, &args.request)?;
                if args.request.referer.is_none() {
                    request = request.referer(link.page.as_str())?;
                }
                Ok(request)
            })
            .collect::<Result<Vec<_>>>()?;
        let mut config = options::engine_config(&args.engine)?;
        config.max_active_downloads = args.jobs;
        get::download(config, requests, verbose, false).await
    }
}

fn grab_options(args: &GrabArgs) -> Result<GrabOptions> {
    let headers = options::headers(&args.request)?;
    let mut grab = GrabOptions {
        depth: args.depth,
        other_hosts: args.other_hosts,
        max_pages: args.max_pages,
        respect_robots: !args.ignore_robots,
        proxy: args.engine.proxy.clone(),
        headers,
        ..GrabOptions::default()
    };
    if let Some(user_agent) = &args.request.user_agent {
        grab.user_agent.clone_from(user_agent);
    }
    Ok(grab)
}

fn keep(link: &Link, args: &GrabArgs) -> bool {
    let name = link.file_name().unwrap_or_default().to_ascii_lowercase();
    let kind_ok = args.kinds.is_empty()
        || args
            .kinds
            .iter()
            .any(|kind| LinkKind::parse(kind) == Some(link.kind));
    let ext_ok = args.extensions.is_empty()
        || args.extensions.iter().any(|ext| {
            name.ends_with(&format!(
                ".{}",
                ext.trim_start_matches('.').to_ascii_lowercase()
            ))
        });
    let text_ok = args.contains.as_ref().is_none_or(|needle| {
        let needle = needle.to_ascii_lowercase();
        link.url.as_str().to_ascii_lowercase().contains(&needle)
            || link
                .label
                .as_ref()
                .is_some_and(|label| label.to_ascii_lowercase().contains(&needle))
    });
    // Sizes are only known after --probe; unknown sizes pass.
    let size_ok = link.size.is_none_or(|size| {
        args.min_size.is_none_or(|min| size >= min) && args.max_size.is_none_or(|max| size <= max)
    });
    kind_ok && ext_ok && text_ok && size_ok
}

fn print_table(links: &[Link]) {
    println!("{:>4}  {:<10}  {:>10}  NAME", "#", "TYPE", "SIZE");
    for (index, link) in links.iter().enumerate() {
        let size = link
            .size
            .map(|size| HumanBytes(size).to_string())
            .unwrap_or_default();
        let name = link.file_name().unwrap_or_else(|| link.url.to_string());
        let label = link
            .label
            .as_ref()
            .filter(|label| **label != name)
            .map(|l| format!("  ({l})"))
            .unwrap_or_default();
        println!(
            "{:>4}  {:<10}  {size:>10}  {name}{label}",
            index + 1,
            link.kind.name()
        );
    }
}

/// Indices of the links to download: from --all, --first, --pick, or
/// the interactive picker.
fn choose(links: &[Link], args: &GrabArgs) -> Result<Vec<usize>> {
    if args.all {
        return Ok((0..links.len()).collect());
    }
    if let Some(first) = args.first {
        return Ok((0..links.len().min(first)).collect());
    }
    if let Some(pick) = &args.pick {
        return parse_pick(pick, links.len());
    }
    if !std::io::stdin().is_terminal() {
        bail!("choose what to download with --all, --first N, or --pick 1,3,5-9");
    }
    let items: Vec<String> = links
        .iter()
        .map(|link| {
            format!(
                "[{}] {}",
                link.kind.name(),
                link.file_name().unwrap_or_else(|| link.url.to_string())
            )
        })
        .collect();
    let picked = dialoguer::MultiSelect::new()
        .with_prompt("Space selects, a selects all, Enter downloads")
        .items(&items)
        .interact_opt()?;
    Ok(picked.unwrap_or_default())
}

/// `1,3,5-9` (1-based, as printed) to 0-based indices.
pub fn parse_pick(text: &str, count: usize) -> Result<Vec<usize>> {
    let mut chosen = Vec::new();
    for part in text
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let (first, last) = match part.split_once('-') {
            Some((a, b)) => (a.trim().parse::<usize>()?, b.trim().parse::<usize>()?),
            None => {
                let n = part.parse::<usize>()?;
                (n, n)
            }
        };
        if first == 0 || last < first || last > count {
            bail!("`{part}` is outside 1-{count}");
        }
        chosen.extend(first - 1..last);
    }
    chosen.sort_unstable();
    chosen.dedup();
    Ok(chosen)
}

async fn queue(
    links: &[&Link],
    args: &GrabArgs,
    title: Option<String>,
    socket: Option<std::path::PathBuf>,
) -> Result<ExitCode> {
    let mut client = jobs::connect(socket).await?;
    let mut batch = NewBatch {
        group: args.group.clone(),
        title,
        ..NewBatch::default()
    };
    for link in links {
        let mut job = options::new_job(link.url.as_str(), &args.target, &args.request)?;
        if args.request.referer.is_none() {
            job.headers.push(("Referer".into(), link.page.to_string()));
        }
        batch.jobs.push(job);
    }
    jobs::add_batch(&mut client, batch).await
}

#[cfg(test)]
mod tests {
    use super::parse_pick;

    #[test]
    fn picks_ranges_and_rejects_out_of_range() {
        assert_eq!(parse_pick("1,3,5-7", 9).unwrap(), [0, 2, 4, 5, 6]);
        assert_eq!(parse_pick("2-2, 2", 3).unwrap(), [1]);
        assert!(parse_pick("0", 3).is_err());
        assert!(parse_pick("2-9", 3).is_err());
        assert!(parse_pick("x", 3).is_err());
    }
}
