//! `hiroi info`: what the servers say about a file.

use std::process::ExitCode;

use anyhow::Result;
use hiroi_engine::Engine;
use indicatif::HumanBytes;

use crate::args::InfoArgs;
use crate::options;

pub async fn run(args: InfoArgs) -> Result<ExitCode> {
    let engine = Engine::new(options::engine_config(&args.engine))?;
    let request = options::base_request(&args.url, &args.mirrors, &args.request)?;
    let info = engine.inspect(&request).await;

    let unknown = || "unknown".to_string();
    println!(
        "Name:      {}",
        info.file_name.clone().unwrap_or_else(unknown)
    );
    let size = info
        .size
        .map(|size| format!("{} ({size} bytes)", HumanBytes(size)));
    println!("Size:      {}", size.unwrap_or_else(unknown));
    println!(
        "Type:      {}",
        info.content_type.clone().unwrap_or_else(unknown)
    );
    let resumable = if info.resumable {
        "yes, several connections"
    } else {
        "no, one connection"
    };
    println!("Resumable: {resumable}");

    println!("Sources:");
    for source in &info.sources {
        let state = match &source.error {
            Some(error) => format!("not used: {error}"),
            None => "ok".to_string(),
        };
        println!("  {}  [{state}]", source.url);
        if let Some(final_url) = source.final_url.as_ref().filter(|u| **u != source.url) {
            println!("    redirects to {final_url}");
        }
    }
    let any_usable = info.sources.iter().any(|source| source.usable);
    Ok(if any_usable {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
