//! BitTorrent, with the `torrent` feature: magnet links and `.torrent`
//! files, downloaded by librqbit.
//!
//! A torrent is a different kind of download. There is no server to be
//! polite to; there are peers, and every one of them sees the address
//! we connect from. That is how the protocol works, and no setting
//! changes it. So:
//!
//! - With a proxy set, torrents are refused. librqbit can send peer
//!   connections through SOCKS, but DHT and UDP trackers would still go
//!   out directly and give the address away.
//! - Local peer discovery is off, so we do not announce ourselves on
//!   the local network, and we do not listen for incoming peers.
//! - DHT is on by default, since magnet links rarely work without it.
//!   [`EngineConfig::torrent_dht`] turns it off. Nothing is written to
//!   disk for it.
//!
//! We upload while we download, as the protocol expects, and stop when
//! the download is complete: a download manager is not a seedbox.
//!
//! [`EngineConfig::torrent_dht`]: crate::EngineConfig::torrent_dht

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use librqbit::{AddTorrent, AddTorrentOptions, DhtSessionConfig, Session, SessionOptions};

use crate::error::{EngineError, Result};
use crate::net::probe::RemoteFile;
use crate::request::ConflictPolicy;
use crate::snapshot::DownloadStatus;
use crate::task::{EngineShared, TaskContext};

/// Largest `.torrent` file we read; big ones list many pieces, but
/// rarely more than a few MiB.
const MAX_TORRENT_FILE: usize = 16 * 1024 * 1024;

/// The engine's one librqbit session, started by the first torrent.
#[derive(Default)]
pub(crate) struct Torrents(tokio::sync::OnceCell<Arc<Session>>);

impl std::fmt::Debug for Torrents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Torrents")
            .field(&self.0.initialized())
            .finish()
    }
}

pub(crate) enum Source {
    Magnet(String),
    File(Bytes),
}

pub(crate) fn detect(remote: &RemoteFile) -> bool {
    let content_type = remote.content_type.as_deref().unwrap_or("");
    content_type.contains("bittorrent")
        || remote
            .final_url
            .path()
            .to_ascii_lowercase()
            .ends_with(".torrent")
}

pub(crate) async fn fetch_file(ctx: &TaskContext, url: &reqwest::Url) -> Result<Source> {
    let bytes = crate::media::fetch_small(ctx, url, MAX_TORRENT_FILE).await?;
    Ok(Source::File(bytes.into()))
}

async fn session(shared: &EngineShared) -> Result<Arc<Session>> {
    shared
        .torrents
        .0
        .get_or_try_init(|| async {
            let options = SessionOptions {
                dht: shared.config.torrent_dht.then(|| DhtSessionConfig {
                    persistence: None,
                    ..DhtSessionConfig::default()
                }),
                disable_local_service_discovery: true,
                listen: None,
                persistence: None,
                fastresume: false,
                ..SessionOptions::default()
            };
            Session::new_with_opts(std::env::temp_dir(), options)
                .await
                .map_err(|error| EngineError::Protocol(start_failure(&format!("{error:#}"))))
        })
        .await
        .cloned()
}

/// librqbit opens an IPv6 socket for UDP trackers, which fails where
/// the kernel has IPv6 turned off (connectivity does not matter).
fn start_failure(error: &str) -> String {
    if error.contains("Address family not supported") {
        "BitTorrent needs IPv6 enabled in the kernel, even without an IPv6 \
         connection (librqbit opens an IPv6 socket)"
            .into()
    } else {
        format!("could not start BitTorrent: {error}")
    }
}

pub(crate) async fn download(ctx: &Arc<TaskContext>, source: Source) -> Result<PathBuf> {
    if ctx.shared.config.proxy.is_some() {
        return Err(EngineError::Protocol(
            "BitTorrent cannot be kept inside the proxy (DHT and UDP trackers go out \
             directly); refusing so your address is not exposed"
                .into(),
        ));
    }
    tracing::info!("BitTorrent: every peer sees the address you download from");
    let session = session(&ctx.shared).await?;
    let dir = ctx.request.output_dir.clone();
    std::fs::create_dir_all(&dir).map_err(|e| EngineError::io(&dir, e))?;

    let limit = ctx
        .request
        .speed_limit
        .and_then(|limit| NonZeroU32::new(u32::try_from(limit).unwrap_or(u32::MAX)));
    let options = AddTorrentOptions {
        output_folder: Some(dir.to_string_lossy().into_owned()),
        // Needed to continue a torrent whose files are already there;
        // pieces are checked, so only missing or broken ones are fetched.
        overwrite: ctx.request.conflict != ConflictPolicy::Skip,
        ratelimits: librqbit::limits::LimitsConfig {
            download_bps: limit,
            upload_bps: None,
        },
        ..AddTorrentOptions::default()
    };
    let add = match source {
        Source::Magnet(link) => AddTorrent::from_url(link),
        Source::File(bytes) => AddTorrent::from_bytes(bytes),
    };
    let added = crate::task::or_cancel(&ctx.cancel, session.add_torrent(add, Some(options)))
        .await?
        .map_err(|e| EngineError::Protocol(format!("could not add the torrent: {e:#}")))?;
    let handle = added
        .into_handle()
        .ok_or_else(|| EngineError::Protocol("the torrent was only listed".into()))?;
    ctx.observer
        .status_changed(ctx.id, DownloadStatus::Downloading);

    let mut tick = tokio::time::interval(ctx.shared.config.progress_interval);
    let outcome = loop {
        tokio::select! {
            biased;
            () = ctx.cancel.cancelled() => break Err(EngineError::Cancelled),
            _ = tick.tick() => {}
        }
        let stats = handle.stats();
        if let Some(error) = stats.error {
            break Err(EngineError::Protocol(format!("BitTorrent: {error}")));
        }
        let name = handle.name();
        ctx.progress.update(|p| {
            p.total_size = Some(stats.total_bytes).filter(|&total| total > 0);
            p.downloaded = stats.progress_bytes;
            p.speed = stats
                .live
                .as_ref()
                .map_or(0, |live| live.download_speed.as_bytes());
            p.connections = stats
                .live
                .as_ref()
                .map_or(0, |live| live.snapshot.peer_stats.live as usize);
            p.resumable = true;
        });
        ctx.observer.progress(ctx.id);
        if stats.finished {
            break Ok(dir.join(name.unwrap_or_else(|| "torrent".into())));
        }
    };
    // Stops the transfer; the files stay, so a paused torrent continues.
    if let Err(error) = session.delete(handle.id().into(), false).await {
        tracing::warn!(%error, "could not stop the torrent");
    }
    outcome
}
