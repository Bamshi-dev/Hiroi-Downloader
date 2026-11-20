//! Turns command-line arguments into engine settings, requests, and
//! service jobs.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use hiroi_engine::{Checksum, DownloadRequest, EngineConfig, Quality};
use hiroi_proto::{Conflict as ProtoConflict, NewJob, Priority, Quality as ProtoQuality};

use crate::args::{Conflict, EngineArgs, RequestArgs, TargetArgs};

pub fn engine_config(engine: &EngineArgs) -> EngineConfig {
    EngineConfig {
        max_connections_per_host: engine.per_host,
        proxy: engine.proxy.clone(),
        min_connection_speed: engine.min_speed,
        preallocate: !engine.no_prealloc,
        allow_insecure_redirects: engine.allow_insecure_redirects,
        require_proxy: engine.require_proxy,
        strip_tracking: engine.strip_tracking,
        no_referer: engine.no_referer,
        cookie_jar: !engine.no_cookie_jar,
        sync_to_disk: !engine.no_sync,
        interface: engine.interface.clone(),
        bind_address: engine.bind_address,
        ip_filter: engine.ip_filter.clone(),
        allow_local_redirects: engine.allow_local_redirects,
        anonymous: engine.anonymous,
        accept_invalid_certificates: engine.no_check_certificate,
        ..EngineConfig::default()
    }
}

/// Request headers as name/value pairs, cookies and referer included.
pub fn headers(request: &RequestArgs) -> Result<Vec<(String, String)>> {
    let mut headers = Vec::new();
    for header in &request.headers {
        let (name, value) = header
            .split_once(':')
            .with_context(|| format!("header `{header}` must look like `Name: value`"))?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    if let Some(cookie) = &request.cookie {
        headers.push(("Cookie".into(), cookie.clone()));
    }
    if let Some(referer) = &request.referer {
        headers.push(("Referer".into(), referer.clone()));
    }
    if let Some(user_agent) = &request.user_agent {
        headers.push(("User-Agent".into(), user_agent.clone()));
    }
    Ok(headers)
}

/// A request with the request options (headers, cookies, referer) applied.
pub fn base_request(url: &str, mirrors: &[String], args: &RequestArgs) -> Result<DownloadRequest> {
    let mut request = DownloadRequest::new(url)?;
    for mirror in mirrors {
        request = request.mirror(mirror)?;
    }
    for (name, value) in headers(args)? {
        request = request.header(&name, &value)?;
    }
    Ok(request)
}

/// A full download request for `hiroi get`.
pub fn download_request(
    url: &str,
    target: &TargetArgs,
    args: &RequestArgs,
) -> Result<DownloadRequest> {
    let dir = target.dir.clone().unwrap_or_else(|| PathBuf::from("."));
    let mut request = base_request(url, &target.mirrors, args)?
        .output_dir(dir)
        .connections(usize::from(target.connections))
        .speed_limit(target.limit)
        .on_conflict(target.on_conflict.into())
        .quality(target.quality)
        .live_limit(target.live_limit)
        .extract(target.extract)
        .format(target.format.clone());
    if let Some(name) = &target.output {
        request = request.file_name(name);
    }
    if let Some(checksum) = &target.checksum {
        request = request.checksum(checksum.parse::<Checksum>()?);
    }
    Ok(request)
}

/// A job for `hiroi add`. A relative folder is made absolute here, since
/// the service runs in a different working directory.
pub fn new_job(url: &str, target: &TargetArgs, args: &RequestArgs) -> Result<NewJob> {
    if let Some(checksum) = &target.checksum {
        checksum.parse::<Checksum>()?;
    }
    let dir = target.dir.as_deref().map(absolute).transpose()?;
    Ok(NewJob {
        url: url.to_string(),
        mirrors: target.mirrors.clone(),
        dir,
        file_name: target.output.clone(),
        connections: Some(target.connections),
        speed_limit: target.limit,
        checksum: target.checksum.clone(),
        on_conflict: Some(match target.on_conflict {
            Conflict::Rename => ProtoConflict::Rename,
            Conflict::Overwrite => ProtoConflict::Overwrite,
            Conflict::Skip => ProtoConflict::Skip,
        }),
        priority: Some(Priority::Normal),
        headers: headers(args)?,
        quality: Some(match target.quality {
            Quality::Best => ProtoQuality::Best,
            Quality::Worst => ProtoQuality::Worst,
            Quality::MaxHeight(height) => ProtoQuality::MaxHeight(height),
        }),
        live_limit_secs: target.live_limit.map(|limit| limit.as_secs().max(1)),
        extract: target.extract || target.format.is_some(),
        format: target.format.clone(),
        ..NewJob::default()
    })
}

fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).with_context(|| format!("bad folder {}", path.display()))
}
