//! Grabbing pages from a local server.

use std::time::Duration;

use hiroi_grab::{GrabOptions, Grabber, LinkKind};
use hiroi_testserver::{Behavior, TestServer};
use reqwest::Url;

const HTML: &str = "text/html; charset=utf-8";

async fn site() -> TestServer {
    let server = TestServer::start(Behavior::default()).await;
    server.put_typed(
        "index.html",
        r#"<h1>Files</h1>
           <a href="setup.iso">Installer</a>
           <a href="gallery.html">Gallery</a>
           <a href="private.html">Private</a>
           <a href="https://other.example/page.html">Elsewhere</a>
           <img src="logo.png" alt="Logo">
           <video src="intro.mp4"></video>
           <a href="download?id=7">Get the manual</a>"#,
        HTML,
    );
    server.put_typed(
        "gallery.html",
        r#"<img srcset="cat-small.jpg 1x, cat-big.jpg 2x"><a href="index.html">back</a>
           <script>play({"src":"https:\/\/cdn.example\/live.m3u8"})</script>"#,
        HTML,
    );
    server.put_typed("private.html", r#"<a href="secret.zip">secret</a>"#, HTML);
    server.put_typed(
        "robots.txt",
        "User-agent: *\nDisallow: /private\n",
        "text/plain",
    );
    server.put_typed("setup.iso", vec![0u8; 3000], "application/x-iso9660-image");
    server.put_typed("download", b"%PDF-1.7".to_vec(), "application/pdf");
    server
}

fn options(depth: u32) -> GrabOptions {
    GrabOptions {
        depth,
        delay: Duration::from_millis(10),
        ..GrabOptions::default()
    }
}

fn names(report: &hiroi_grab::Report) -> Vec<String> {
    report
        .links
        .iter()
        .map(|link| link.url.path().trim_start_matches('/').to_string())
        .collect()
}

#[tokio::test]
async fn reads_one_page() {
    let server = site().await;
    let start = Url::parse(&server.url("index.html")).unwrap();
    let report = Grabber::new(options(0))
        .unwrap()
        .grab(&start)
        .await
        .unwrap();

    assert_eq!(report.pages.len(), 1);
    assert_eq!(names(&report), ["setup.iso", "logo.png", "intro.mp4"]);
    let kinds: Vec<_> = report.links.iter().map(|l| l.kind).collect();
    assert_eq!(
        kinds,
        [LinkKind::DiskImage, LinkKind::Image, LinkKind::Video]
    );
    assert_eq!(report.links[0].label.as_deref(), Some("Installer"));
}

#[tokio::test]
async fn follows_pages_on_the_same_site_and_honours_robots() {
    let server = site().await;
    let start = Url::parse(&server.url("index.html")).unwrap();
    let report = Grabber::new(options(1))
        .unwrap()
        .grab(&start)
        .await
        .unwrap();

    let names = names(&report);
    assert!(names.contains(&"cat-big.jpg".to_string()), "{names:?}");
    assert!(
        names.contains(&"live.m3u8".to_string()),
        "stream inside a script: {names:?}"
    );
    assert!(
        !names.contains(&"secret.zip".to_string()),
        "robots.txt disallows /private"
    );
    assert!(
        report
            .skipped
            .iter()
            .any(|(url, why)| url.path() == "/private.html" && why.contains("robots"))
    );
    assert!(
        !report
            .pages
            .iter()
            .any(|page| page.host_str() == Some("other.example"))
    );
}

#[tokio::test]
async fn files_behind_page_like_addresses_are_found() {
    let server = site().await;
    let start = Url::parse(&server.url("index.html")).unwrap();
    let grabber = Grabber::new(options(0)).unwrap();
    let mut report = grabber.grab(&start).await.unwrap();
    // At depth 0, "download?id=7" is only a candidate until probed.
    assert!(!names(&report).contains(&"download".to_string()));
    let candidate = report
        .page_links
        .iter()
        .position(|l| l.url.path() == "/download")
        .unwrap();
    grabber.probe(&mut report.page_links, 4).await;
    assert_eq!(report.page_links[candidate].kind, LinkKind::Document);
    grabber.probe(&mut report.links, 4).await;
    let iso = &report.links[0];
    assert_eq!(
        (iso.size, iso.content_type.as_deref()),
        (Some(3000), Some("application/x-iso9660-image"))
    );

    // While crawling, the same link is recognised as a file directly.
    let crawled = Grabber::new(options(1))
        .unwrap()
        .grab(&start)
        .await
        .unwrap();
    let manual = crawled
        .links
        .iter()
        .find(|l| l.url.path() == "/download")
        .unwrap();
    assert_eq!(manual.kind, LinkKind::Document);
}
