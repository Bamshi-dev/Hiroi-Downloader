//! The exposure report and the privacy settings, against local servers.

use std::time::Duration;

use hiroi_engine::{
    DownloadRequest, DownloadStatus, Engine, EngineConfig, ExposureOptions, Level, ProxyKind,
};
use hiroi_testserver::{Behavior, TestServer, content};

fn engine(config: EngineConfig) -> Engine {
    Engine::new(EngineConfig {
        progress_interval: Duration::from_millis(50),
        ..config
    })
    .unwrap()
}

fn has(report: &hiroi_engine::ExposureReport, level: Level, text: &str) -> bool {
    report
        .findings
        .iter()
        .any(|finding| finding.level == level && finding.text.contains(text))
}

#[tokio::test]
async fn redirects_networks_cookies_and_tracking_are_reported() {
    let start = TestServer::start(Behavior::default()).await;
    let cdn = TestServer::start(Behavior::default()).await;
    cdn.put_with_headers(
        "file.iso",
        content(1000, 1),
        "application/octet-stream",
        vec![
            ("cf-ray", "8a1b2c3d4e5f-AMS".into()),
            ("set-cookie", "visitor=abc; Path=/".into()),
        ],
    );
    // Another host name for the same machine, so the hop changes hands.
    let target = cdn.url("file.iso").replace("127.0.0.1", "localhost");
    start.redirect("file.iso", &target);
    let url = format!("{}?utm_source=news&token=1", start.url("file.iso"));

    let request = DownloadRequest::new(&url)
        .unwrap()
        .header("Referer", "https://forum.example/thread")
        .unwrap();
    let report = engine(EngineConfig::default())
        .exposure(&request, &ExposureOptions::default())
        .await
        .unwrap();

    assert_eq!(report.proxy, ProxyKind::None);
    let hops = &report.sources[0].hops;
    assert_eq!(hops.len(), 2, "{hops:?}");
    assert_eq!(hops[0].status, Some(302));
    assert_eq!(hops[1].host, "localhost");
    assert_eq!(hops[1].network, Some("Cloudflare"));
    assert_eq!(hops[1].cookies_set, ["visitor"]);
    assert!(
        !hops[0].addresses.is_empty(),
        "names are resolved without a proxy"
    );
    assert!(has(
        &report,
        Level::Risk,
        "No proxy: 127.0.0.1 and localhost see"
    ));
    assert!(has(&report, Level::Risk, "plain HTTP"));
    assert!(has(&report, Level::Warning, "sends you on to localhost"));
    assert!(has(
        &report,
        Level::Warning,
        "tracking parameters (utm_source)"
    ));
    assert!(has(&report, Level::Warning, "carries Referer"));
    assert!(has(&report, Level::Note, "served by Cloudflare"));
    assert_eq!(report.findings[0].level, Level::Risk, "worst first");
}

#[tokio::test]
async fn tracking_parameters_and_referers_can_be_left_out() {
    let server = TestServer::start(Behavior::default()).await;
    let data = content(50_000, 2);
    server.put("clean.bin", data.clone());
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(EngineConfig {
        strip_tracking: true,
        no_referer: true,
        ..EngineConfig::default()
    });

    let url = format!("{}?fbclid=xyz&sig=keep", server.url("clean.bin"));
    let request = DownloadRequest::new(&url)
        .unwrap()
        .header("Referer", "https://forum.example/thread")
        .unwrap()
        .output_dir(dir.path());
    let id = engine.add(request);
    engine.wait(id).await;

    assert_eq!(
        engine.snapshot(id).unwrap().status,
        DownloadStatus::Completed
    );
    assert_eq!(std::fs::read(dir.path().join("clean.bin")).unwrap(), data);
    for received in server.received() {
        assert_eq!(received.query.as_deref(), Some("sig=keep"));
        assert!(!received.headers.contains_key("referer"));
    }
}

#[tokio::test]
async fn the_address_check_asks_the_echo_service() {
    let echo = TestServer::start(Behavior::default()).await;
    echo.put_typed("ip", "203.0.113.7\n", "text/plain");
    let request = DownloadRequest::new(&echo.url("ip")).unwrap();
    let options = ExposureOptions {
        check_ip: true,
        compare_direct: true,
        ip_service: echo.url("ip"),
    };

    let report = engine(EngineConfig::default())
        .exposure(&request, &options)
        .await
        .unwrap();

    let check = report.public_ip.expect("asked for");
    assert_eq!(check.seen.unwrap().to_string(), "203.0.113.7");
    assert!(check.direct.unwrap().is_ok());
}

#[test]
fn a_required_proxy_stops_the_engine_from_starting() {
    let config = EngineConfig {
        require_proxy: true,
        ..EngineConfig::default()
    };
    assert!(Engine::new(config).is_err());
}
