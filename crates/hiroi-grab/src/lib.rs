//! Finds what can be downloaded on web pages: links to files, pictures,
//! videos, audio, subtitles, and HLS/DASH streams.
//!
//! [`Grabber::grab`] fetches a page, streams it through an HTML parser,
//! and returns every link it found, classified. With a depth above zero
//! it also follows links to other pages of the same site, politely: it
//! honours `robots.txt`, opens at most two connections per host, waits
//! between requests, and stops at a page limit.
//!
//! Pages that build their content with JavaScript show little to a
//! parser; for those, the browser extension collects links from the live
//! page instead.

mod extract;
mod kind;
mod robots;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::{
    ACCEPT, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, RANGE,
};
use reqwest::redirect::Policy;
use reqwest::{Client, StatusCode, Url};
use tokio::sync::{Mutex, Semaphore};

pub use extract::Found;
use extract::{Extractor, RawLink};
pub use kind::LinkKind;
use robots::Robots;

/// Product token matched against `User-agent` lines of `robots.txt`.
pub const ROBOTS_AGENT: &str = "hiroi";

#[derive(Debug, Clone)]
pub struct GrabOptions {
    /// Pages to follow beyond the first one: 0 reads only the given page.
    pub depth: u32,
    /// Follow links to other hosts too (only matters with depth > 0).
    pub other_hosts: bool,
    /// Stop after this many pages.
    pub max_pages: usize,
    /// Pause between two requests to the same host while crawling.
    pub delay: Duration,
    /// Skip pages that `robots.txt` disallows (never for the first page,
    /// which the user asked for, as a browser would).
    pub respect_robots: bool,
    /// Bytes read from one page at most.
    pub max_page_bytes: u64,
    pub user_agent: String,
    pub proxy: Option<String>,
    /// Sent with every request: cookies, referer, and so on.
    pub headers: Vec<(String, String)>,
    pub timeout: Duration,
}

impl Default for GrabOptions {
    fn default() -> Self {
        Self {
            depth: 0,
            other_hosts: false,
            max_pages: 50,
            delay: Duration::from_millis(500),
            respect_robots: true,
            max_page_bytes: 16 * 1024 * 1024,
            user_agent: format!(
                "Mozilla/5.0 (compatible; {ROBOTS_AGENT}/{})",
                env!("CARGO_PKG_VERSION")
            ),
            proxy: None,
            headers: Vec::new(),
            timeout: Duration::from_secs(20),
        }
    }
}

/// One thing found on a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub url: Url,
    pub kind: LinkKind,
    /// Anchor text or image description, when the page has one.
    pub label: Option<String>,
    pub found: Found,
    /// The page it was found on.
    pub page: Url,
    /// Filled in by [`Grabber::probe`].
    pub size: Option<u64>,
    pub content_type: Option<String>,
}

impl Link {
    /// The file name the URL suggests.
    pub fn file_name(&self) -> Option<String> {
        self.url
            .path_segments()?
            .rfind(|s| !s.is_empty())
            .map(str::to_string)
    }
}

#[derive(Debug, Default)]
pub struct Report {
    /// Pages read, in order.
    pub pages: Vec<Url>,
    /// Files found, in page order, without duplicates and without pages.
    pub links: Vec<Link>,
    /// Links that look like pages and were not followed. Some are files
    /// behind addresses like `download.php?id=7`; [`Grabber::probe`]
    /// tells them apart.
    pub page_links: Vec<Link>,
    /// Pages that could not be read or were skipped, with the reason.
    pub skipped: Vec<(Url, String)>,
}

#[derive(Debug, thiserror::Error)]
pub enum GrabError {
    #[error("only http and https pages can be grabbed")]
    Scheme,
    #[error("invalid proxy: {0}")]
    Proxy(String),
    #[error("invalid header `{0}`")]
    Header(String),
    #[error("could not read {url}: {reason}")]
    Page { url: String, reason: String },
}

#[derive(Debug)]
pub struct Grabber {
    client: Client,
    options: GrabOptions,
}

impl Grabber {
    pub fn new(options: GrabOptions) -> Result<Self, GrabError> {
        let mut headers = HeaderMap::new();
        for (name, value) in &options.headers {
            let bad = || GrabError::Header(name.clone());
            let name = HeaderName::from_bytes(name.trim().as_bytes()).map_err(|_| bad())?;
            let mut value = HeaderValue::from_str(value.trim()).map_err(|_| bad())?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        let mut builder = Client::builder()
            .user_agent(options.user_agent.as_str())
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(options.timeout)
            .redirect(Policy::custom(|attempt| {
                let downgrade = attempt
                    .previous()
                    .last()
                    .is_some_and(|p| p.scheme() == "https")
                    && attempt.url().scheme() == "http";
                if attempt.previous().len() >= 10 || downgrade {
                    attempt.stop()
                } else {
                    attempt.follow()
                }
            }));
        if let Some(proxy) = &options.proxy {
            let proxy = reqwest::Proxy::all(proxy)
                .map_err(|e| GrabError::Proxy(e.without_url().to_string()))?;
            builder = builder.proxy(proxy);
        }
        let client = builder
            .build()
            .map_err(|e| GrabError::Proxy(e.without_url().to_string()))?;
        Ok(Self { client, options })
    }

    /// Reads `start`, and with a depth, the pages it links to.
    pub async fn grab(&self, start: &Url) -> Result<Report, GrabError> {
        if !matches!(start.scheme(), "http" | "https") {
            return Err(GrabError::Scheme);
        }
        let crawl = Crawl::new(self, start);
        let mut report = Report::default();
        let mut seen_links: HashSet<Url> = HashSet::new();
        let mut visited: HashSet<Url> = HashSet::from([start.clone()]);
        let mut level = vec![start.clone()];

        for depth in 0..=self.options.depth {
            let pages = futures_util::future::join_all(
                level.iter().map(|page| crawl.page(page, depth == 0)),
            )
            .await;
            let mut next = Vec::new();
            for (page, result) in level.into_iter().zip(pages) {
                let raw = match result {
                    Ok(PageResult::Html(raw)) => raw,
                    // A "page" link that turned out to be a file.
                    Ok(PageResult::File(link)) => {
                        if seen_links.insert(link.url.clone()) {
                            report.links.push(*link);
                        }
                        continue;
                    }
                    Err(reason) if depth == 0 => {
                        return Err(GrabError::Page {
                            url: page.to_string(),
                            reason,
                        });
                    }
                    Err(reason) => {
                        report.skipped.push((page, reason));
                        continue;
                    }
                };
                report.pages.push(page.clone());
                for link in classify(raw, &page) {
                    if link.kind == LinkKind::Page {
                        let follow =
                            self.options.other_hosts || link.url.host_str() == start.host_str();
                        let deeper = depth < self.options.depth;
                        if deeper
                            && follow
                            && visited.len() < self.options.max_pages
                            && visited.insert(link.url.clone())
                        {
                            next.push(link.url);
                        } else if !visited.contains(&link.url)
                            && seen_links.insert(link.url.clone())
                        {
                            report.page_links.push(link);
                        }
                    } else if seen_links.insert(link.url.clone()) {
                        report.links.push(link);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            level = next;
        }
        Ok(report)
    }

    /// Asks the server for each link's size and type with a one-byte
    /// request, `parallel` at a time, and refines the kind of links whose
    /// URL said nothing.
    pub async fn probe(&self, links: &mut [Link], parallel: usize) {
        let limit = Arc::new(Semaphore::new(parallel.max(1)));
        let answers = futures_util::future::join_all(links.iter().map(|link| {
            let limit = Arc::clone(&limit);
            async move {
                let _permit = limit.acquire().await.ok()?;
                self.probe_one(&link.url).await
            }
        }))
        .await;
        for (link, answer) in links.iter_mut().zip(answers) {
            let Some((size, content_type)) = answer else {
                continue;
            };
            link.size = size;
            if matches!(link.kind, LinkKind::Other | LinkKind::Page)
                && let Some(kind) = content_type
                    .as_deref()
                    .and_then(LinkKind::from_content_type)
            {
                link.kind = kind;
            }
            link.content_type = content_type;
        }
    }

    async fn probe_one(&self, url: &Url) -> Option<(Option<u64>, Option<String>)> {
        let response = self
            .client
            .get(url.clone())
            .header(RANGE, "bytes=0-0")
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let header = |name| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let size = if response.status() == StatusCode::PARTIAL_CONTENT {
            header(CONTENT_RANGE).and_then(|v| v.rsplit('/').next()?.parse().ok())
        } else {
            header(CONTENT_LENGTH).and_then(|v| v.parse().ok())
        };
        Some((size, header(CONTENT_TYPE)))
    }
}

/// State shared by the pages of one grab: per-host limits and robots.
struct Crawl<'a> {
    grabber: &'a Grabber,
    hosts: Mutex<HashMap<String, Arc<Semaphore>>>,
    robots: Mutex<HashMap<String, Arc<Robots>>>,
}

enum PageResult {
    Html(Vec<RawLink>),
    File(Box<Link>),
}

/// Connections per host while crawling.
const PER_HOST: usize = 2;

impl<'a> Crawl<'a> {
    fn new(grabber: &'a Grabber, _start: &Url) -> Self {
        Self {
            grabber,
            hosts: Mutex::default(),
            robots: Mutex::default(),
        }
    }

    async fn page(&self, url: &Url, first: bool) -> Result<PageResult, String> {
        let options = &self.grabber.options;
        let host = url.host_str().unwrap_or_default().to_string();
        if !first && options.respect_robots {
            let robots = self.robots_for(url).await;
            if !robots.allows(&path_and_query(url)) {
                return Err("disallowed by robots.txt".into());
            }
        }
        let gate = Arc::clone(
            self.hosts
                .lock()
                .await
                .entry(host)
                .or_insert_with(|| Arc::new(Semaphore::new(PER_HOST))),
        );
        let _permit = gate.acquire().await.map_err(|e| e.to_string())?;
        if !first {
            let delay = self
                .robots_for(url)
                .await
                .crawl_delay
                .map(Duration::from_secs_f64);
            tokio::time::sleep(delay.unwrap_or(options.delay).min(Duration::from_secs(30))).await;
        }
        let response = self
            .grabber
            .client
            .get(url.clone())
            .header(
                ACCEPT,
                HeaderValue::from_static("text/html,application/xhtml+xml;q=0.9,*/*;q=0.5"),
            )
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?;
        if !response.status().is_success() {
            return Err(format!("server answered {}", response.status()));
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !(content_type.is_empty() || content_type.contains("html")) {
            if first {
                return Err(format!("not a web page ({content_type})"));
            }
            // Dropping the response stops the transfer; we only needed the headers.
            let link = Link {
                url: url.clone(),
                kind: LinkKind::from_content_type(content_type).unwrap_or(LinkKind::Other),
                label: None,
                found: Found::Anchor,
                page: url.clone(),
                size: response.content_length(),
                content_type: Some(content_type.to_string()),
            };
            return Ok(PageResult::File(Box::new(link)));
        }
        let page = response.url().clone();
        let mut extractor = Extractor::new(page);
        let mut body = response.bytes_stream();
        let mut read = 0u64;
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| e.without_url().to_string())?;
            read += chunk.len() as u64;
            if !extractor.feed(&chunk) || read > options.max_page_bytes {
                break;
            }
        }
        Ok(PageResult::Html(extractor.finish()))
    }

    async fn robots_for(&self, url: &Url) -> Arc<Robots> {
        let origin = url.origin().ascii_serialization();
        if let Some(robots) = self.robots.lock().await.get(&origin) {
            return Arc::clone(robots);
        }
        let text = match url.join("/robots.txt") {
            Ok(robots_url) => match self.grabber.client.get(robots_url).send().await {
                Ok(response) if response.status().is_success() => {
                    response.text().await.unwrap_or_default()
                }
                _ => String::new(),
            },
            Err(_) => String::new(),
        };
        let robots = Arc::new(Robots::parse(&text, ROBOTS_AGENT));
        self.robots.lock().await.insert(origin, Arc::clone(&robots));
        robots
    }
}

fn path_and_query(url: &Url) -> String {
    match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    }
}

/// Turns raw links into classified ones, first occurrence first.
fn classify(raw: Vec<RawLink>, page: &Url) -> Vec<Link> {
    let mut best: BTreeMap<usize, Link> = BTreeMap::new();
    let mut index_of: HashMap<Url, usize> = HashMap::new();
    for (position, raw) in raw.into_iter().enumerate() {
        let by_extension = extension(&raw.url).and_then(|ext| LinkKind::from_extension(&ext));
        // The URL's extension beats a hint from the markup, except that a
        // plain anchor without a file extension is another page.
        let kind = by_extension.or(raw.hint).unwrap_or(match raw.found {
            Found::Anchor | Found::Embed => LinkKind::Page,
            _ => LinkKind::Other,
        });
        let link = Link {
            url: raw.url.clone(),
            kind,
            label: raw.label,
            found: raw.found,
            page: page.clone(),
            size: None,
            content_type: None,
        };
        match index_of.get(&raw.url) {
            // Keep the first position but take a label found later.
            Some(&at) => {
                let existing = best.get_mut(&at).expect("indexed link exists");
                if existing.label.is_none() {
                    existing.label = link.label;
                }
            }
            None => {
                index_of.insert(raw.url, position);
                best.insert(position, link);
            }
        }
    }
    best.into_values().collect()
}

fn extension(url: &Url) -> Option<String> {
    let name = url.path_segments()?.rfind(|s| !s.is_empty())?;
    let (_, extension) = name.rsplit_once('.')?;
    Some(extension.to_ascii_lowercase())
}
