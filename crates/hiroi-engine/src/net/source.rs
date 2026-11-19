use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use parking_lot::{Mutex, RwLock};
use reqwest::Url;

use crate::error::EngineError;
use crate::net::probe::RemoteFile;
use crate::net::{display_url, host_key, source_key};

/// One server that hosts the file: the primary URL or a mirror.
#[derive(Debug)]
pub(crate) struct Source {
    original: Url,
    current: RwLock<Url>,
    host: String,
    validator: Option<String>,
    dead: AtomicBool,
    refreshed: AtomicBool,
    in_use: AtomicUsize,
}

impl Source {
    pub fn new(original: Url, remote: &RemoteFile) -> Self {
        Self {
            host: host_key(&remote.final_url),
            current: RwLock::new(remote.final_url.clone()),
            original,
            validator: remote.validator.clone(),
            dead: AtomicBool::new(false),
            refreshed: AtomicBool::new(false),
            in_use: AtomicUsize::new(0),
        }
    }

    /// The link as given by the user, before any redirects.
    pub fn original(&self) -> &Url {
        &self.original
    }

    pub fn url(&self) -> Url {
        self.current.read().clone()
    }

    pub fn display(&self) -> String {
        display_url(&self.original)
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn validator(&self) -> Option<&str> {
        self.validator.as_deref()
    }

    /// Replaces the resolved URL after a successful refresh.
    pub fn refresh_to(&self, url: Url) {
        *self.current.write() = url;
    }

    pub fn mark_refreshed(&self) {
        self.refreshed.store(true, Ordering::Relaxed);
    }

    pub fn was_refreshed(&self) -> bool {
        self.refreshed.load(Ordering::Relaxed)
    }

    pub fn mark_dead(&self) {
        self.dead.store(true, Ordering::Relaxed);
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }

    /// Counts this connection until the guard is dropped.
    pub fn track(self: &Arc<Self>) -> SourceUse {
        self.in_use.fetch_add(1, Ordering::Relaxed);
        SourceUse {
            source: Arc::clone(self),
        }
    }
}

#[derive(Debug)]
pub(crate) struct SourceUse {
    source: Arc<Source>,
}

impl Drop for SourceUse {
    fn drop(&mut self) {
        self.source.in_use.fetch_sub(1, Ordering::Relaxed);
    }
}

/// All sources of one download. Connections are spread over the sources that
/// still work, least busy first.
#[derive(Debug)]
pub(crate) struct SourcePool {
    sources: Vec<Arc<Source>>,
    last_failure: Mutex<Option<Failure>>,
}

#[derive(Debug)]
enum Failure {
    Changed,
    Other(String),
}

impl SourcePool {
    pub fn new(sources: Vec<Arc<Source>>) -> Self {
        debug_assert!(!sources.is_empty());
        Self {
            sources,
            last_failure: Mutex::new(None),
        }
    }

    pub fn primary(&self) -> &Arc<Source> {
        &self.sources[0]
    }

    /// Key written to the control file; see [`source_key`].
    pub fn key(&self) -> String {
        source_key(self.primary().original())
    }

    pub fn pick(&self) -> Option<Arc<Source>> {
        self.sources
            .iter()
            .filter(|source| !source.is_dead())
            .min_by_key(|source| source.in_use.load(Ordering::Relaxed))
            .cloned()
    }

    pub fn drop_source(&self, source: &Source, reason: &str, changed: bool) {
        source.mark_dead();
        tracing::warn!(source = %source.display(), %reason, "giving up on source");
        let failure = if changed {
            Failure::Changed
        } else {
            Failure::Other(format!("{reason} ({})", source.display()))
        };
        *self.last_failure.lock() = Some(failure);
    }

    /// The error to report once no source is left.
    pub fn exhausted(&self) -> EngineError {
        match &*self.last_failure.lock() {
            Some(Failure::Changed) => EngineError::RemoteChanged,
            Some(Failure::Other(message)) => EngineError::NoSources(message.clone()),
            None => EngineError::NoSources("all sources failed".into()),
        }
    }
}
