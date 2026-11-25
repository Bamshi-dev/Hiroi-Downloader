//! `hiroi export` and `hiroi import`: the job list as a file, to move it
//! to another machine or keep a copy.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use hiroi_proto::{NewJob, Request, Response};
use serde::{Deserialize, Serialize};

use crate::commands::jobs;

/// Format of the file; a newer Hiroi may write a higher number.
const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct JobList {
    hiroi_jobs: u32,
    jobs: Vec<NewJob>,
}

pub async fn export(
    file: Option<PathBuf>,
    finished: bool,
    socket: Option<PathBuf>,
) -> Result<ExitCode> {
    let mut client = jobs::connect(socket).await?;
    let Response::Exported { jobs } = client.call(Request::Export { finished }).await? else {
        bail!("unexpected reply");
    };
    let count = jobs.len();
    let text = serde_json::to_string_pretty(&JobList {
        hiroi_jobs: VERSION,
        jobs,
    })?;
    match file {
        Some(path) => {
            std::fs::write(&path, text + "\n")
                .with_context(|| format!("cannot write {}", path.display()))?;
            eprintln!("{count} jobs written to {}", path.display());
        }
        None => println!("{text}"),
    }
    Ok(ExitCode::SUCCESS)
}

pub async fn import(file: &Path, socket: Option<PathBuf>) -> Result<ExitCode> {
    let text =
        std::fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))?;
    let list: JobList = serde_json::from_str(&text)
        .with_context(|| format!("{} is not a Hiroi job list", file.display()))?;
    if list.hiroi_jobs > VERSION {
        bail!(
            "{} comes from a newer Hiroi; update this one first",
            file.display()
        );
    }
    let mut client = jobs::connect(socket).await?;
    let mut failed = 0;
    for mut job in list.jobs {
        // A list may come from someone else: nothing in it runs commands
        // or sends headers, whatever it says.
        job.headers.clear();
        job.run_after = None;
        job.open_folder = false;
        job.allow_duplicate = false;
        let url = job.url.clone();
        match client.call(Request::Add(Box::new(job))).await {
            Ok(Response::Added { job }) => println!("{job}\t{url}"),
            Ok(_) => {}
            Err(error) => {
                failed += 1;
                eprintln!("not added {url}: {error}");
            }
        }
    }
    Ok(if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
