//! Entry points for fuzzing the grabber's parsers: HTML pages, fed in
//! pieces as they arrive from the network, and robots.txt files. See
//! the engine's module of the same name.

use reqwest::Url;

use crate::extract::Extractor;
use crate::robots::Robots;

/// Feeds a page in two pieces, split where the input says, as a slow
/// server would. Every link found is an absolute HTTP or HTTPS URL.
pub fn page(data: &[u8]) {
    let page = Url::parse("https://site.example/dir/page.html").expect("valid");
    let split = data
        .first()
        .map_or(0, |&b| usize::from(b) % (data.len() + 1));
    let mut extractor = Extractor::new(page);
    if extractor.feed(&data[..split]) {
        extractor.feed(&data[split..]);
    }
    for link in extractor.finish().links {
        assert!(
            matches!(link.url.scheme(), "http" | "https"),
            "{}",
            link.url
        );
    }
}

pub fn robots(data: &[u8]) {
    let robots = Robots::parse(&String::from_utf8_lossy(data), "hiroi");
    for path in ["/", "/private/x", "/a*b$", ""] {
        let _ = robots.allows(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hiroi_testserver::mutations;

    #[test]
    fn pages_survive_mangling() {
        let sample = br#"<html><head><base href="/media/"><meta property="og:video" content="v.mp4"></head>
            <body><a href="file.iso">Disk</a><a href="javascript:alert(1)">x</a>
            <img src="a.jpg" srcset="b.jpg 2x, c.jpg 640w"><video poster="p.jpg"><source src="s.m3u8"></video>
            <a href="//other.example/x.zip">Elsewhere</a><a href="data:text/plain,hi">d</a></body></html>"#;
        page(sample);
        for input in mutations(sample, 3000, 1) {
            page(&input);
        }
    }

    #[test]
    fn robots_files_survive_mangling() {
        let sample = b"User-agent: *\nDisallow: /private\nAllow: /private/ok$\nCrawl-delay: 2.5\n\nUser-agent: hiroi\nDisallow: /*.php\n";
        robots(sample);
        for input in mutations(sample, 3000, 2) {
            robots(&input);
        }
    }
}
