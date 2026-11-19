//! Entry points for fuzzing the parsers that read what servers and
//! files hand us. `cargo fuzz` drives them from `fuzz/` (nightly); the
//! tests below feed them mangled copies of real samples on every
//! `cargo test`, on stable.
//!
//! Each entry point checks that parsing never panics, and, where the
//! result makes a promise, that the promise holds.

use std::time::{Duration, SystemTime};

use reqwest::Url;

use crate::constants::MAX_FILE_NAME_BYTES;
use crate::media::{dash, extract, hls};
use crate::request::Quality;
use crate::storage::control::ControlFile;
use crate::storage::naming::sanitize_file_name;
use crate::{metalink, net, xml};

fn base() -> Url {
    Url::parse("https://cdn.example/media/index").expect("valid")
}

/// Content-Range, Retry-After, Content-Disposition, and ETag values.
pub fn headers(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Some(range) = net::headers::parse_content_range(text) {
        assert!(range.start <= range.end);
    }
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let _ = net::headers::parse_retry_after(text, now);
    if let Some(name) = net::headers::content_disposition_file_name(text) {
        file_name(name.as_bytes());
    }
    let _ = net::headers::is_weak_etag(text);
}

/// A sanitized name stays inside the download folder.
pub fn file_name(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    if let Some(name) = sanitize_file_name(&text) {
        assert!(!name.is_empty());
        assert!(!name.contains(['/', '\\', '\0']), "{name:?}");
        assert!(!name.starts_with('.'), "{name:?}");
        assert!(name.len() <= MAX_FILE_NAME_BYTES, "{} bytes", name.len());
    }
}

/// A control file is either refused or consistent.
pub fn control_file(data: &[u8]) {
    let _ = ControlFile::from_bytes(data);
}

pub fn hls(data: &[u8]) {
    let _ = hls::parse(&String::from_utf8_lossy(data), &base());
}

pub fn dash(data: &[u8]) {
    let _ = dash::parse(&String::from_utf8_lossy(data), &base(), Quality::Best, None);
}

pub fn xml(data: &[u8]) {
    let _ = xml::parse(&String::from_utf8_lossy(data));
}

/// Metalink paths never leave the download folder.
pub fn metalink(data: &[u8]) {
    let Ok(files) = metalink::parse(&String::from_utf8_lossy(data)) else {
        return;
    };
    for file in files {
        assert!(!file.path.is_empty());
        for part in &file.path {
            assert!(
                part != "." && part != ".." && !part.contains(['/', '\\']),
                "{part:?}"
            );
        }
    }
}

/// yt-dlp's `-J` answer.
pub fn ytdlp(data: &[u8]) {
    let _ = extract::parse(data);
}

#[cfg(test)]
mod tests {
    use super::*;
    use hiroi_testserver::mutations;

    /// Copies per sample; enough to shake out shallow bugs on every run.
    const ROUNDS: usize = 3000;

    fn shake(target: fn(&[u8]), samples: &[&[u8]]) {
        for (index, sample) in samples.iter().enumerate() {
            target(sample);
            for input in mutations(sample, ROUNDS, index as u64) {
                target(&input);
            }
        }
    }

    #[test]
    fn headers_survive_mangling() {
        shake(
            headers,
            &[
                b"bytes 0-1023/4096",
                b"bytes */4096",
                b"Wed, 21 Oct 2015 07:28:00 GMT",
                b"120",
                b"attachment; filename=\"a b.iso\"; filename*=UTF-8''%E2%82%AC.iso",
                b"W/\"abc\"",
            ],
        );
    }

    #[test]
    fn file_names_stay_in_their_folder() {
        shake(
            file_name,
            &[
                b"../../etc/passwd",
                b"..\\boot.ini",
                b"report.final.pdf",
                b"CON.txt",
            ],
        );
    }

    #[test]
    fn control_files_survive_mangling() {
        let sample = br#"{"version":1,"source":"https://a.example/f","total_size":100,"validator":"\"e\"","segments":[{"start":0,"end":60,"written":60},{"start":60,"end":100,"written":10}]}"#;
        shake(control_file, &[sample]);
        // Backwards and oversized segments, which mangling rarely makes.
        control_file(br#"{"version":1,"source":"s","total_size":100,"validator":null,"segments":[{"start":90,"end":10,"written":0}]}"#);
        control_file(br#"{"version":1,"source":"s","total_size":18446744073709551615,"validator":null,"segments":[{"start":0,"end":18446744073709551615,"written":0},{"start":0,"end":18446744073709551615,"written":0}]}"#);
    }

    #[test]
    fn playlists_survive_mangling() {
        shake(
            hls,
            &[
                b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,AUDIO=\"a\"\nlow.m3u8\n\
                  #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",DEFAULT=YES,URI=\"a.m3u8\"\n",
                b"#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:7\n\
                  #EXT-X-KEY:METHOD=AES-128,URI=\"k.bin\",IV=0x00112233445566778899aabbccddeeff\n\
                  #EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"720@0\"\n#EXTINF:4.0,\n#EXT-X-BYTERANGE:1000@720\nseg.m4s\n\
                  #EXT-X-ENDLIST\n",
            ],
        );
    }

    #[test]
    fn manifests_survive_mangling() {
        let mpd = br#"<MPD type="dynamic" minimumUpdatePeriod="PT2S" mediaPresentationDuration="PT1H2M3.5S"><Period>
            <AdaptationSet mimeType="video/mp4"><SegmentTemplate media="v-$Number%05d$-$Time$.m4s" initialization="i.mp4" timescale="90000" duration="180000" startNumber="5">
            <SegmentTimeline><S t="0" d="180000" r="3"/></SegmentTimeline></SegmentTemplate>
            <Representation id="v" bandwidth="1" height="720"/></AdaptationSet>
            <AdaptationSet mimeType="audio/mp4"><Representation id="a" bandwidth="1"><BaseURL>a.mp4</BaseURL>
            <SegmentList><Initialization range="0-99"/><SegmentURL mediaRange="100-199"/></SegmentList></Representation></AdaptationSet>
            </Period></MPD>"#;
        shake(dash, &[mpd]);
        shake(xml, &[mpd, b"<a b='&amp;&#x41;'>t<c/></a>"]);
    }

    #[test]
    fn metalinks_survive_mangling() {
        shake(
            metalink,
            &[br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="dir/a.iso"><size>3</size>
                <hash type="sha-256">ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad</hash>
                <url priority="1">https://a.example/a.iso</url></file></metalink>"#],
        );
    }

    #[test]
    fn extractor_answers_survive_mangling() {
        shake(
            ytdlp,
            &[br#"{"title":"T","ext":"mp4","requested_formats":[{"format_id":"1","url":"https://a.example/v","protocol":"https","http_headers":{"Referer":"x"}},{"format_id":"2","url":"https://a.example/a","protocol":"m3u8_native"}]}"#],
        );
    }
}
