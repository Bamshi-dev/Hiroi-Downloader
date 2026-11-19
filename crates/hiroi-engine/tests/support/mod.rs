//! An HTTP file server for tests that can misbehave on purpose.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use axum::routing::get;

const CHUNK: usize = 16 * 1024;

/// How the server behaves. Defaults to a well-behaved range server.
#[derive(Debug, Clone)]
pub struct Behavior {
    pub ranges: bool,
    /// Speed limit per connection.
    pub bytes_per_sec: Option<u64>,
    /// Cut every response after this many bytes.
    pub drop_after: Option<u64>,
    /// Answer 429 when more connections than this are open.
    pub max_connections: Option<usize>,
    pub retry_after_secs: u64,
    pub content_disposition: Option<String>,
}

impl Default for Behavior {
    fn default() -> Self {
        Self {
            ranges: true,
            bytes_per_sec: None,
            drop_after: None,
            max_connections: None,
            retry_after_secs: 1,
            content_disposition: None,
        }
    }
}

pub struct TestServer {
    base: String,
    state: Arc<ServerState>,
}

#[derive(Default)]
struct ServerState {
    files: Mutex<HashMap<String, FileEntry>>,
    behavior: Mutex<Behavior>,
    active: AtomicUsize,
    peak: AtomicUsize,
    served: AtomicU64,
    throttled: AtomicUsize,
    versions: AtomicU64,
}

#[derive(Clone)]
struct FileEntry {
    data: Bytes,
    etag: String,
}

impl TestServer {
    pub async fn start(behavior: Behavior) -> Self {
        let state = Arc::new(ServerState::default());
        *state.behavior.lock().unwrap() = behavior;
        let router = Router::new()
            .route("/{name}", get(serve))
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { base, state }
    }

    /// Adds or replaces a file. Every version gets a new ETag.
    pub fn put(&self, name: &str, data: Vec<u8>) {
        let version = self.state.versions.fetch_add(1, Ordering::Relaxed);
        let entry = FileEntry {
            etag: format!("\"v{version}-{}\"", data.len()),
            data: data.into(),
        };
        self.state
            .files
            .lock()
            .unwrap()
            .insert(name.to_string(), entry);
    }

    pub fn url(&self, name: &str) -> String {
        format!("{}/{name}", self.base)
    }

    pub fn set_behavior(&self, behavior: Behavior) {
        *self.state.behavior.lock().unwrap() = behavior;
    }

    pub fn peak_connections(&self) -> usize {
        self.state.peak.load(Ordering::Relaxed)
    }

    pub fn bytes_served(&self) -> u64 {
        self.state.served.load(Ordering::Relaxed)
    }

    pub fn throttled(&self) -> usize {
        self.state.throttled.load(Ordering::Relaxed)
    }
}

/// Deterministic pseudo-random content.
pub fn content(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = fastrand::Rng::with_seed(seed);
    (0..len).map(|_| rng.u8(..)).collect()
}

struct ActiveGuard(Arc<ServerState>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn serve(
    State(state): State<Arc<ServerState>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let behavior = state.behavior.lock().unwrap().clone();
    let Some(file) = state.files.lock().unwrap().get(&name).cloned() else {
        return empty(StatusCode::NOT_FOUND);
    };

    let open = state.active.fetch_add(1, Ordering::Relaxed) + 1;
    let guard = ActiveGuard(Arc::clone(&state));
    if behavior.max_connections.is_some_and(|max| open > max) {
        state.throttled.fetch_add(1, Ordering::Relaxed);
        return Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header(header::RETRY_AFTER, behavior.retry_after_secs.to_string())
            .body(Body::empty())
            .unwrap();
    }
    state.peak.fetch_max(open, Ordering::Relaxed);

    let size = file.data.len() as u64;
    let if_range_ok = headers
        .get(header::IF_RANGE)
        .is_none_or(|value| value.to_str().ok() == Some(file.etag.as_str()));
    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .filter(|_| behavior.ranges && if_range_ok)
        .and_then(parse_range);

    let mut response = Response::builder().header(header::ETAG, &file.etag).header(
        header::ACCEPT_RANGES,
        if behavior.ranges { "bytes" } else { "none" },
    );
    if let Some(disposition) = &behavior.content_disposition {
        response = response.header(header::CONTENT_DISPOSITION, disposition);
    }

    let (start, end) = match range {
        Some((start, _)) if start >= size => {
            return response
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{size}"))
                .body(Body::empty())
                .unwrap();
        }
        Some((start, end)) => {
            let end = end.unwrap_or(size - 1).min(size - 1);
            response = response
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
            (start, end + 1)
        }
        None => (0, size),
    };
    let body = file.data.slice(start as usize..end as usize);
    response
        .header(header::CONTENT_LENGTH, body.len())
        .body(stream_body(body, behavior, state, guard))
        .unwrap()
}

fn stream_body(
    data: Bytes,
    behavior: Behavior,
    state: Arc<ServerState>,
    guard: ActiveGuard,
) -> Body {
    let delay = behavior
        .bytes_per_sec
        .map(|rate| Duration::from_secs_f64(CHUNK as f64 / rate as f64));
    let stream = futures_util::stream::unfold((0usize, guard), move |(sent, guard)| {
        let data = data.clone();
        let state = Arc::clone(&state);
        async move {
            if sent >= data.len() {
                return None;
            }
            if behavior
                .drop_after
                .is_some_and(|limit| sent as u64 >= limit)
            {
                let error = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "dropped");
                return Some((Err(error), (data.len(), guard)));
            }
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            let end = (sent + CHUNK).min(data.len());
            state
                .served
                .fetch_add((end - sent) as u64, Ordering::Relaxed);
            Some((Ok(data.slice(sent..end)), (end, guard)))
        }
    });
    Body::from_stream(stream)
}

fn parse_range(value: &str) -> Option<(u64, Option<u64>)> {
    let (start, end) = value.strip_prefix("bytes=")?.split_once('-')?;
    let end = if end.is_empty() {
        None
    } else {
        Some(end.parse().ok()?)
    };
    Some((start.parse().ok()?, end))
}

fn empty(status: StatusCode) -> Response {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .unwrap()
}
