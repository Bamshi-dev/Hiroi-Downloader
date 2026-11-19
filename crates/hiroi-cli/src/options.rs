//! Turns command-line arguments into engine settings and requests.

use anyhow::{Context, Result};
use hiroi_engine::{Checksum, DownloadRequest, EngineConfig};

use crate::args::{NetworkArgs, TargetArgs};

pub fn engine_config(network: &NetworkArgs) -> EngineConfig {
    let mut config = EngineConfig {
        max_connections_per_host: network.per_host,
        proxy: network.proxy.clone(),
        min_connection_speed: network.min_speed,
        preallocate: !network.no_prealloc,
        allow_insecure_redirects: network.allow_insecure_redirects,
        ..EngineConfig::default()
    };
    if let Some(user_agent) = &network.user_agent {
        config.user_agent = user_agent.clone();
    }
    config
}

/// A request with the network options (headers, cookies, referer) applied.
pub fn base_request(
    url: &str,
    mirrors: &[String],
    network: &NetworkArgs,
) -> Result<DownloadRequest> {
    let mut request = DownloadRequest::new(url)?;
    for mirror in mirrors {
        request = request.mirror(mirror)?;
    }
    for header in &network.headers {
        let (name, value) = header
            .split_once(':')
            .with_context(|| format!("header `{header}` must look like `Name: value`"))?;
        request = request.header(name, value)?;
    }
    if let Some(cookie) = &network.cookie {
        request = request.cookies(cookie)?;
    }
    if let Some(referer) = &network.referer {
        request = request.referer(referer)?;
    }
    Ok(request)
}

/// A full download request: network options plus where to save.
pub fn download_request(
    url: &str,
    target: &TargetArgs,
    network: &NetworkArgs,
) -> Result<DownloadRequest> {
    let mut request = base_request(url, &target.mirrors, network)?
        .output_dir(&target.dir)
        .connections(usize::from(target.connections))
        .speed_limit(target.limit)
        .on_conflict(target.on_conflict.into());
    if let Some(name) = &target.output {
        request = request.file_name(name);
    }
    if let Some(checksum) = &target.checksum {
        request = request.checksum(checksum.parse::<Checksum>()?);
    }
    Ok(request)
}
