//! Pulls links out of HTML as it streams in.
//!
//! lol_html parses chunks as they arrive and calls our handlers for the
//! elements we ask for, so memory use does not grow with the page. The
//! handlers record raw links; resolving and classifying them happens
//! once the page is done.

use std::sync::{Arc, Mutex, PoisonError};

use lol_html::send::{HtmlRewriter, Settings};
use lol_html::{element, text};
use reqwest::Url;

use crate::kind::LinkKind;

/// Where on the page a link was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Found {
    Anchor,
    Image,
    Video,
    Audio,
    Source,
    Track,
    Embed,
    Meta,
    Preload,
    Script,
}

#[derive(Debug, Clone)]
pub(crate) struct RawLink {
    pub url: Url,
    pub found: Found,
    /// Anchor text or `alt`, trimmed and shortened.
    pub label: Option<String>,
    /// Kind the markup implies (an `<img>` is an image whatever its URL).
    pub hint: Option<LinkKind>,
}

/// Longest anchor text kept per link.
const MAX_LABEL: usize = 120;
/// Inline script text scanned per `<script>` element.
const MAX_SCRIPT: usize = 2 * 1024 * 1024;

#[derive(Default)]
struct Collected {
    base: Option<Url>,
    links: Vec<RawLink>,
    open_anchor: Option<usize>,
    script: String,
}

pub(crate) struct Extractor {
    rewriter: HtmlRewriter<'static, fn(&[u8])>,
    collected: Arc<Mutex<Collected>>,
}

impl std::fmt::Debug for Extractor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Extractor").finish_non_exhaustive()
    }
}

fn discard(_: &[u8]) {}

impl Extractor {
    pub fn new(page: Url) -> Self {
        let collected = Arc::new(Mutex::new(Collected {
            base: Some(page),
            ..Collected::default()
        }));
        let c = |collected: &Arc<Mutex<Collected>>| Arc::clone(collected);

        let (
            base,
            anchors,
            anchor_text,
            images,
            sources,
            media,
            tracks,
            embeds,
            metas,
            preloads,
            scripts,
        ) = (
            c(&collected),
            c(&collected),
            c(&collected),
            c(&collected),
            c(&collected),
            c(&collected),
            c(&collected),
            c(&collected),
            c(&collected),
            c(&collected),
            c(&collected),
        );
        let first_base = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let settings = Settings::new_send()
            .append_element_content_handler(element!("base[href]", move |el| {
                // Only the first <base> counts, as in browsers.
                if first_base.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    let mut state = lock(&base);
                    let resolved = el
                        .get_attribute("href")
                        .and_then(|href| resolve(&state.base, &href));
                    if resolved.is_some() {
                        state.base = resolved;
                    }
                }
                Ok(())
            }))
            .append_element_content_handler(element!("a[href], area[href]", move |el| {
                let mut state = lock(&anchors);
                let label = el
                    .get_attribute("title")
                    .or_else(|| el.get_attribute("download"));
                state.open_anchor = None;
                if let Some(href) = el.get_attribute("href") {
                    let index = state.push(&href, Found::Anchor, label, None);
                    state.open_anchor = index;
                }
                Ok(())
            }))
            .append_element_content_handler(text!("a", move |chunk| {
                let mut state = lock(&anchor_text);
                if let Some(index) = state.open_anchor {
                    let label = state.links[index].label.get_or_insert_with(String::new);
                    if label.len() < MAX_LABEL {
                        label.push_str(chunk.as_str());
                    }
                }
                Ok(())
            }))
            .append_element_content_handler(element!("img", move |el| {
                let mut state = lock(&images);
                let alt = el.get_attribute("alt");
                // Lazy-loading scripts keep the real address in data-*.
                for name in [
                    "src",
                    "data-src",
                    "data-original",
                    "data-lazy-src",
                    "data-full",
                ] {
                    if let Some(src) = el.get_attribute(name) {
                        state.push(&src, Found::Image, alt.clone(), Some(LinkKind::Image));
                    }
                }
                for name in ["srcset", "data-srcset"] {
                    if let Some(srcset) = el.get_attribute(name)
                        && let Some(best) = largest_candidate(&srcset)
                    {
                        state.push(&best, Found::Image, alt.clone(), Some(LinkKind::Image));
                    }
                }
                Ok(())
            }))
            .append_element_content_handler(element!("source", move |el| {
                let mut state = lock(&sources);
                let hint = el
                    .get_attribute("type")
                    .and_then(|t| LinkKind::from_content_type(&t));
                if let Some(src) = el.get_attribute("src") {
                    state.push(&src, Found::Source, None, hint.or(Some(LinkKind::Video)));
                }
                if let Some(srcset) = el.get_attribute("srcset")
                    && let Some(best) = largest_candidate(&srcset)
                {
                    state.push(&best, Found::Image, None, Some(LinkKind::Image));
                }
                Ok(())
            }))
            .append_element_content_handler(element!("video, audio", move |el| {
                let mut state = lock(&media);
                let (found, kind) = if el.tag_name() == "audio" {
                    (Found::Audio, LinkKind::Audio)
                } else {
                    (Found::Video, LinkKind::Video)
                };
                if let Some(src) = el.get_attribute("src") {
                    state.push(&src, found, None, Some(kind));
                }
                if let Some(poster) = el.get_attribute("poster") {
                    state.push(&poster, Found::Image, None, Some(LinkKind::Image));
                }
                Ok(())
            }))
            .append_element_content_handler(element!("track[src]", move |el| {
                if let Some(src) = el.get_attribute("src") {
                    lock(&tracks).push(
                        &src,
                        Found::Track,
                        el.get_attribute("label"),
                        Some(LinkKind::Subtitle),
                    );
                }
                Ok(())
            }))
            .append_element_content_handler(element!(
                "embed[src], object[data], iframe[src]",
                move |el| {
                    let mut state = lock(&embeds);
                    let hint = (el.tag_name() == "iframe").then_some(LinkKind::Page);
                    if let Some(src) = el.get_attribute("src").or_else(|| el.get_attribute("data"))
                    {
                        state.push(&src, Found::Embed, None, hint);
                    }
                    Ok(())
                }
            ))
            .append_element_content_handler(element!("meta[content]", move |el| {
                let key = el
                    .get_attribute("property")
                    .or_else(|| el.get_attribute("name"))
                    .unwrap_or_default();
                let hint = match key.to_ascii_lowercase().as_str() {
                    "og:video"
                    | "og:video:url"
                    | "og:video:secure_url"
                    | "twitter:player:stream" => LinkKind::Video,
                    "og:audio" | "og:audio:url" | "og:audio:secure_url" => LinkKind::Audio,
                    "og:image" | "og:image:url" | "og:image:secure_url" | "twitter:image" => {
                        LinkKind::Image
                    }
                    _ => return Ok(()),
                };
                if let Some(content) = el.get_attribute("content") {
                    lock(&metas).push(&content, Found::Meta, None, Some(hint));
                }
                Ok(())
            }))
            .append_element_content_handler(element!("link[href]", move |el| {
                let rel = el
                    .get_attribute("rel")
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let as_ = el
                    .get_attribute("as")
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let hint = match (rel.as_str(), as_.as_str()) {
                    ("preload" | "prefetch", "video") | ("video_src", _) => LinkKind::Video,
                    ("preload" | "prefetch", "image") | ("image_src", _) => LinkKind::Image,
                    ("preload" | "prefetch", "audio") => LinkKind::Audio,
                    _ => return Ok(()),
                };
                if let Some(href) = el.get_attribute("href") {
                    lock(&preloads).push(&href, Found::Preload, None, Some(hint));
                }
                Ok(())
            }))
            .append_element_content_handler(text!("script", move |chunk| {
                let mut state = lock(&scripts);
                if state.script.len() < MAX_SCRIPT {
                    state.script.push_str(chunk.as_str());
                }
                if chunk.last_in_text_node() {
                    let script = std::mem::take(&mut state.script);
                    for url in urls_in_script(&script) {
                        state.push(&url, Found::Script, None, None);
                    }
                }
                Ok(())
            }));
        let rewriter = HtmlRewriter::new(settings, discard as fn(&[u8]));
        Self {
            rewriter,
            collected,
        }
    }

    /// Feeds the next chunk of the page. A parse error ends extraction
    /// early; what was found so far is kept.
    pub fn feed(&mut self, chunk: &[u8]) -> bool {
        self.rewriter.write(chunk).is_ok()
    }

    pub fn finish(self) -> Vec<RawLink> {
        let Self {
            rewriter,
            collected,
        } = self;
        let _ = rewriter.end();
        std::mem::take(&mut lock(&collected).links)
    }
}

impl Collected {
    /// Resolves and records a link; returns its index.
    fn push(
        &mut self,
        raw: &str,
        found: Found,
        label: Option<String>,
        hint: Option<LinkKind>,
    ) -> Option<usize> {
        let url = resolve(&self.base, raw)?;
        let label = label
            .map(|l| l.trim().chars().take(MAX_LABEL).collect())
            .filter(|l: &String| !l.is_empty());
        self.links.push(RawLink {
            url,
            found,
            label,
            hint,
        });
        Some(self.links.len() - 1)
    }
}

fn lock(collected: &Mutex<Collected>) -> std::sync::MutexGuard<'_, Collected> {
    collected.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Resolves `raw` against the page; only http(s) results count.
pub(crate) fn resolve(base: &Option<Url>, raw: &str) -> Option<Url> {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with('#') {
        return None;
    }
    let mut url = match base {
        Some(base) => base.join(raw).ok()?,
        None => Url::parse(raw).ok()?,
    };
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.set_fragment(None);
    Some(url)
}

/// The candidate with the largest width (`640w`) or density (`2x`) of a
/// `srcset` attribute.
pub(crate) fn largest_candidate(srcset: &str) -> Option<String> {
    srcset
        .split(',')
        .filter_map(|candidate| {
            let mut parts = candidate.split_whitespace();
            let url = parts.next()?;
            let size = parts
                .next()
                .and_then(|descriptor| {
                    let number = descriptor.trim_end_matches(['w', 'x', 'h']);
                    let value: f64 = number.parse().ok()?;
                    Some(if descriptor.ends_with('x') {
                        value * 1000.0
                    } else {
                        value
                    })
                })
                .unwrap_or(1.0);
            Some((size, url.to_string()))
        })
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, url)| url)
}

/// Absolute media and stream URLs inside script text, including JSON
/// with escaped slashes (`https:\/\/cdn\/a.m3u8`).
pub(crate) fn urls_in_script(script: &str) -> Vec<String> {
    const INTERESTING: [&str; 12] = [
        ".m3u8", ".mpd", ".mp4", ".webm", ".mkv", ".mp3", ".m4a", ".zip", ".pdf", ".iso",
        ".torrent", ".jpg",
    ];
    let unescaped = script.replace("\\/", "/");
    let mut found = Vec::new();
    let mut rest = unescaped.as_str();
    while let Some(start) = rest.find("http") {
        let candidate = &rest[start..];
        let end = candidate
            .find(|c: char| {
                c.is_whitespace()
                    || matches!(c, '"' | '\'' | '`' | '<' | '>' | '\\' | ')' | ']' | '}')
            })
            .unwrap_or(candidate.len());
        let url = &candidate[..end];
        let path = url
            .split(['?', '#'])
            .next()
            .unwrap_or(url)
            .to_ascii_lowercase();
        let wanted = INTERESTING.iter().any(|ext| path.ends_with(ext));
        if (url.starts_with("http://") || url.starts_with("https://")) && wanted {
            found.push(url.to_string());
        }
        rest = &candidate[end.max(4)..];
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(html: &str) -> Vec<RawLink> {
        let mut extractor =
            Extractor::new(Url::parse("https://site.example/dir/page.html").unwrap());
        // Feed in small pieces, as the network would.
        for chunk in html.as_bytes().chunks(7) {
            assert!(extractor.feed(chunk));
        }
        extractor.finish()
    }

    fn urls(links: &[RawLink]) -> Vec<String> {
        links.iter().map(|l| l.url.to_string()).collect()
    }

    #[test]
    fn finds_links_media_and_resolves_them() {
        let html = r##"<html><head>
            <meta property="og:video" content="https://cdn.example/v.mp4">
            <link rel="preload" as="image" href="/hero.webp">
            </head><body>
            <a href="files/a.zip">The <b>archive</b></a>
            <a href="mailto:x@y">mail</a><a href="#top">top</a>
            <img src="pic.jpg" alt="A picture" srcset="small.jpg 320w, big.jpg 1280w">
            <img data-src="lazy.png">
            <video poster="poster.jpg"><source src="clip.webm" type="video/webm">
              <track src="subs.vtt" label="English"></video>
            <iframe src="/embed/1"></iframe>
            </body></html>"##;
        let links = extract(html);
        let urls = urls(&links);
        for expected in [
            "https://cdn.example/v.mp4",
            "https://site.example/hero.webp",
            "https://site.example/dir/files/a.zip",
            "https://site.example/dir/pic.jpg",
            "https://site.example/dir/big.jpg",
            "https://site.example/dir/lazy.png",
            "https://site.example/dir/poster.jpg",
            "https://site.example/dir/clip.webm",
            "https://site.example/dir/subs.vtt",
            "https://site.example/embed/1",
        ] {
            assert!(
                urls.contains(&expected.to_string()),
                "missing {expected} in {urls:?}"
            );
        }
        assert!(
            !urls
                .iter()
                .any(|u| u.contains("mailto") || u.ends_with("#top"))
        );
        let anchor = links
            .iter()
            .find(|l| l.url.path().ends_with("a.zip"))
            .unwrap();
        assert_eq!(anchor.label.as_deref(), Some("The archive"));
    }

    #[test]
    fn honours_base_href() {
        let links = extract(r#"<base href="https://mirror.example/files/"><a href="x.iso">x</a>"#);
        assert_eq!(urls(&links), ["https://mirror.example/files/x.iso"]);
    }

    #[test]
    fn finds_streams_inside_scripts() {
        let script = r#"<script>var player = {"hls":"https:\/\/cdn.example\/live\/index.m3u8?token=abc",
            "dash":'https://cdn.example/v/manifest.mpd', "page":"https://site.example/about"};</script>"#;
        let urls = urls(&extract(script));
        assert_eq!(
            urls,
            [
                "https://cdn.example/live/index.m3u8?token=abc",
                "https://cdn.example/v/manifest.mpd"
            ]
        );
    }

    #[test]
    fn picks_the_largest_srcset_candidate() {
        assert_eq!(
            largest_candidate("a.jpg 1x, b.jpg 2x").as_deref(),
            Some("b.jpg")
        );
        assert_eq!(
            largest_candidate("a.jpg 640w, b.jpg 320w").as_deref(),
            Some("a.jpg")
        );
        assert_eq!(largest_candidate("only.jpg").as_deref(), Some("only.jpg"));
        assert_eq!(largest_candidate(""), None);
    }
}
