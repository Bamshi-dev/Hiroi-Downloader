//! Commands that talk to the background service.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use hiroi_daemon::Client;
use hiroi_daemon::schedule::{format_days, format_time, parse_days, parse_time};
use hiroi_proto::{
    Event, Job, JobId, JobStatus, Priority, Request, Response, Schedule, ScheduleAction,
};
use indicatif::{HumanBytes, MultiProgress, ProgressBar, ProgressStyle};

use crate::args::{
    AddArgs, CategoryCommand, IdsArg, JobsArg, LimitArgs, QueueCommand, ScheduleCommand,
};
use crate::commands::get::collect_urls;
use crate::options;
use crate::size::parse_rate;

pub async fn connect(socket: Option<PathBuf>) -> Result<Client> {
    let socket = socket.unwrap_or_else(hiroi_daemon::paths::default_socket);
    Ok(Client::connect(&socket).await?)
}

pub async fn add(client: &mut Client, args: AddArgs) -> Result<ExitCode> {
    let urls = collect_urls(&args.urls, args.input.as_deref())?;
    if urls.len() > 1 && (args.target.output.is_some() || args.target.checksum.is_some()) {
        bail!("--output and --checksum only work with a single URL");
    }
    for url in urls {
        let mut job = options::new_job(&url, &args.target, &args.request)?;
        job.queue = args.queue.clone();
        job.paused = args.paused;
        job.allow_duplicate = args.allow_duplicate;
        if args.high_priority {
            job.priority = Some(Priority::High);
        }
        match client
            .call(Request::Add(job))
            .await
            .with_context(|| format!("could not add {url}"))?
        {
            Response::Added { job } => println!("{job}\t{url}"),
            other => bail!("unexpected reply {other:?}"),
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub async fn list(client: &mut Client) -> Result<ExitCode> {
    let Response::Jobs { jobs } = client.call(Request::List).await? else {
        bail!("unexpected reply");
    };
    if jobs.is_empty() {
        println!("No jobs.");
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "{:>4}  {:<11}  {:>17}  {:>11}  NAME",
        "ID", "STATUS", "PROGRESS", "SPEED"
    );
    for job in &jobs {
        let speed = if job.speed > 0 {
            format!("{}/s", HumanBytes(job.speed))
        } else {
            String::new()
        };
        let name = job.file_name.clone().unwrap_or_else(|| job.url.clone());
        println!(
            "{:>4}  {:<11}  {:>17}  {:>11}  {name}",
            job.id,
            job.status.as_str(),
            progress(job),
            speed,
        );
        if let Some(error) = &job.error {
            println!("{:>4}  {error}", "");
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub async fn pause_or_resume(client: &mut Client, args: JobsArg, resume: bool) -> Result<ExitCode> {
    let request = match (args.job.as_str(), resume) {
        ("all", false) => Request::PauseAll,
        ("all", true) => Request::ResumeAll,
        (id, resume) => {
            let job = JobId(
                id.parse()
                    .with_context(|| format!("`{id}` is not a job id or `all`"))?,
            );
            if resume {
                Request::Resume { job }
            } else {
                Request::Pause { job }
            }
        }
    };
    client.call(request).await?;
    Ok(ExitCode::SUCCESS)
}

pub async fn cancel_or_remove(client: &mut Client, args: IdsArg, remove: bool) -> Result<ExitCode> {
    let mut failed = false;
    for id in args.jobs {
        let job = JobId(id);
        let request = if remove {
            Request::Remove { job }
        } else {
            Request::Cancel { job }
        };
        if let Err(error) = client.call(request).await {
            eprintln!("{error}");
            failed = true;
        }
    }
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

pub async fn limit(client: &mut Client, args: LimitArgs) -> Result<ExitCode> {
    let limit = match args.rate.as_str() {
        "off" | "none" | "0" => None,
        rate => Some(parse_rate(rate).map_err(anyhow::Error::msg)?),
    };
    client
        .call(Request::SetSpeedLimit {
            job: args.job.map(JobId),
            limit,
        })
        .await?;
    Ok(ExitCode::SUCCESS)
}

pub async fn stop(client: &mut Client) -> Result<ExitCode> {
    client.call(Request::Shutdown).await?;
    Ok(ExitCode::SUCCESS)
}

/// Live progress bars until every job is settled or Ctrl+C.
pub async fn watch(client: &mut Client) -> Result<ExitCode> {
    client.call(Request::Subscribe).await?;
    let Response::Jobs { jobs } = client.call(Request::List).await? else {
        bail!("unexpected reply");
    };
    let multi = MultiProgress::new();
    let mut bars: HashMap<JobId, ProgressBar> = HashMap::new();
    let mut running: HashMap<JobId, bool> = HashMap::new();
    for job in &jobs {
        if !job.status.is_settled() {
            show(&multi, &mut bars, job);
            running.insert(job.id, true);
        }
    }
    if running.is_empty() {
        println!("Nothing is running.");
        return Ok(ExitCode::SUCCESS);
    }
    loop {
        let event = tokio::select! {
            event = client.next_event() => event?,
            _ = tokio::signal::ctrl_c() => break,
        };
        match event {
            None => break,
            Some(Event::Job { job }) => {
                show(&multi, &mut bars, &job);
                running.insert(job.id, !job.status.is_settled());
            }
            Some(Event::Progress {
                job,
                downloaded,
                speed,
                connections,
                ..
            }) => {
                if let Some(bar) = bars.get(&job) {
                    bar.set_position(downloaded);
                    bar.set_message(format!("{}/s  {connections} conn", HumanBytes(speed)));
                }
            }
            Some(Event::Retry { job, reason, .. }) => {
                let _ = multi.println(format!("{job}: {reason}"));
            }
            Some(Event::Removed { job }) => {
                running.remove(&job);
            }
        }
        if !running.values().any(|&running| running) {
            break;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn show(multi: &MultiProgress, bars: &mut HashMap<JobId, ProgressBar>, job: &Job) {
    let bar = bars.entry(job.id).or_insert_with(|| {
        let style = ProgressStyle::with_template(
            "{prefix:>4} {wide_msg:.dim} [{bar:24.cyan/blue}] {percent:>3}%",
        )
        .expect("valid template")
        .progress_chars("=> ");
        multi.add(ProgressBar::new(0).with_style(style))
    });
    bar.set_prefix(job.id.to_string());
    bar.set_length(job.total_size.unwrap_or(0));
    bar.set_position(job.downloaded);
    let name = job.file_name.clone().unwrap_or_else(|| job.url.clone());
    match job.status {
        JobStatus::Completed => bar.finish_with_message(format!("{name}: done")),
        JobStatus::Failed => bar.abandon_with_message(format!(
            "{name}: failed: {}",
            job.error.as_deref().unwrap_or("unknown error")
        )),
        status if status.is_settled() => {
            bar.abandon_with_message(format!("{name}: {}", status.as_str()))
        }
        status => bar.set_message(format!("{name}: {}", status.as_str())),
    }
}

fn progress(job: &Job) -> String {
    match job.total_size {
        Some(total) if total > 0 => {
            format!(
                "{:>5.1}% {:>9}",
                job.downloaded as f64 * 100.0 / total as f64,
                HumanBytes(total).to_string()
            )
        }
        _ => HumanBytes(job.downloaded).to_string(),
    }
}

pub async fn queue(client: &mut Client, command: QueueCommand) -> Result<ExitCode> {
    let request = match command {
        QueueCommand::List => {
            let Response::Queues { queues } = client.call(Request::Queues).await? else {
                bail!("unexpected reply");
            };
            println!(
                "{:<16}  {:<7}  {:>5}  {:>7}",
                "QUEUE", "STATE", "JOBS", "PENDING"
            );
            for queue in queues {
                let state = if queue.running { "running" } else { "held" };
                println!(
                    "{:<16}  {state:<7}  {:>5}  {:>7}",
                    queue.name, queue.jobs, queue.pending
                );
            }
            return Ok(ExitCode::SUCCESS);
        }
        QueueCommand::Start { name } => Request::StartQueue { queue: name },
        QueueCommand::Stop { name } => Request::StopQueue { queue: name },
    };
    client.call(request).await?;
    Ok(ExitCode::SUCCESS)
}

pub async fn schedule(client: &mut Client, command: ScheduleCommand) -> Result<ExitCode> {
    match command {
        ScheduleCommand::List => {
            let Response::Schedules { schedules } = client.call(Request::Schedules).await? else {
                bail!("unexpected reply");
            };
            if schedules.is_empty() {
                println!("No schedules.");
            }
            for rule in schedules {
                let when = match rule.to_minute {
                    Some(to) => format!("{}-{}", format_time(rule.from_minute), format_time(to)),
                    None => format!("at {}", format_time(rule.from_minute)),
                };
                let what = match rule.action {
                    ScheduleAction::RunQueue { queue } => format!("run queue {queue}"),
                    ScheduleAction::SpeedLimit { limit } => {
                        format!("limit to {}/s", HumanBytes(limit))
                    }
                };
                println!(
                    "{:>4}  {:<9}  {:<11}  {what}",
                    rule.id,
                    format_days(rule.days),
                    when
                );
            }
        }
        ScheduleCommand::Add(args) => {
            let parse = |text: &str| {
                parse_time(text).with_context(|| format!("`{text}` is not a time like 07:30"))
            };
            let action = match (args.queue, args.limit) {
                (Some(queue), _) => ScheduleAction::RunQueue { queue },
                (None, Some(limit)) => ScheduleAction::SpeedLimit { limit },
                (None, None) => unreachable!("clap requires one of them"),
            };
            let schedule = Schedule {
                id: 0,
                days: parse_days(&args.days)
                    .with_context(|| format!("`{}` is not a list of days", args.days))?,
                from_minute: parse(&args.from)?,
                to_minute: args.to.as_deref().map(parse).transpose()?,
                action,
            };
            let Response::ScheduleAdded { id } =
                client.call(Request::AddSchedule(schedule)).await?
            else {
                bail!("unexpected reply");
            };
            println!("{id}");
        }
        ScheduleCommand::Remove { id } => {
            client.call(Request::RemoveSchedule { id }).await?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub async fn category(client: &mut Client, command: CategoryCommand) -> Result<ExitCode> {
    let Response::Categories(mut categories) = client.call(Request::Categories).await? else {
        bail!("unexpected reply");
    };
    match command {
        CategoryCommand::List => {
            println!(
                "Sorting into category folders is {}.",
                if categories.enabled { "on" } else { "off" }
            );
            for category in &categories.categories {
                println!(
                    "{:<12}  {:<12}  {}",
                    category.name,
                    category.folder,
                    category.extensions.join(",")
                );
            }
            return Ok(ExitCode::SUCCESS);
        }
        CategoryCommand::On => categories.enabled = true,
        CategoryCommand::Off => categories.enabled = false,
        CategoryCommand::Set { name, folder, ext } => {
            let extensions = ext
                .iter()
                .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
                .collect();
            categories
                .categories
                .retain(|category| category.name != name);
            categories.categories.push(hiroi_proto::Category {
                name,
                folder,
                extensions,
            });
        }
        CategoryCommand::Remove { name } => {
            let before = categories.categories.len();
            categories
                .categories
                .retain(|category| category.name != name);
            if categories.categories.len() == before {
                bail!("no category {name}");
            }
        }
    }
    client.call(Request::SetCategories(categories)).await?;
    Ok(ExitCode::SUCCESS)
}
