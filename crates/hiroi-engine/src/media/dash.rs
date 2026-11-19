//! DASH manifests (`.mpd`, ISO/IEC 23009-1): periods hold adaptation
//! sets (one per kind of track), which hold representations (one per
//! quality). We pick one video and one audio representation and list
//! their segments.

use reqwest::Url;

use super::hls::DRM;
use super::pieces::Piece;
use crate::request::Quality;
use crate::xml::{self, Element};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Manifest {
    pub live: bool,
    /// How often a live manifest should be reloaded.
    pub update_period: f64,
    pub video: Option<TrackPlan>,
    pub audio: Option<TrackPlan>,
}

/// The segments of one chosen representation. Live tracks are listed
/// by time so a reload can tell new segments from known ones.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TrackPlan {
    pub id: String,
    pub init: Option<Piece>,
    pub segments: Vec<(u64, Piece)>,
    pub mp4: bool,
}

pub(crate) fn is_manifest(text: &str) -> bool {
    text.contains("<MPD")
}

pub(crate) fn parse(
    text: &str,
    url: &Url,
    quality: Quality,
    want: Option<(&str, &str)>,
) -> Result<Manifest, String> {
    if !is_manifest(text) {
        return Err("not a DASH manifest".into());
    }
    let document = xml::parse(text)?;
    let mpd = document.child("MPD").ok_or("not a DASH manifest")?;
    let live = mpd.attr("type") == Some("dynamic");
    let base = base_url(url, mpd);
    let period = mpd.child("Period").ok_or("MPD without a Period")?;
    let base = base_url(&base, period);
    let duration = period
        .attr("duration")
        .or(mpd.attr("mediaPresentationDuration"))
        .and_then(iso_duration);

    let mut video: Vec<(&Element, &Element)> = Vec::new();
    let mut audio: Vec<(&Element, &Element)> = Vec::new();
    for set in period.children("AdaptationSet") {
        let kind = set
            .attr("contentType")
            .or(set.attr("mimeType"))
            .or_else(|| set.child("Representation").and_then(|r| r.attr("mimeType")))
            .unwrap_or("");
        for representation in set.children("Representation") {
            let kind = representation.attr("mimeType").unwrap_or(kind);
            if kind.starts_with("video") {
                video.push((set, representation));
            } else if kind.starts_with("audio") {
                audio.push((set, representation));
            }
        }
    }
    let bandwidth = |r: &Element| {
        r.attr("bandwidth")
            .and_then(|b| b.parse::<u64>().ok())
            .unwrap_or(0)
    };
    let height = |r: &Element| r.attr("height").and_then(|h| h.parse::<u32>().ok());
    // A reload must keep the representations chosen the first time.
    let pick_video = || match want.map(|(video, _)| video) {
        Some(id) => video.iter().find(|(_, r)| r.attr("id") == Some(id)),
        None => match quality {
            Quality::Best => video.iter().max_by_key(|(_, r)| (bandwidth(r), height(r))),
            Quality::Worst => video.iter().min_by_key(|(_, r)| (bandwidth(r), height(r))),
            Quality::MaxHeight(limit) => video
                .iter()
                .filter(|(_, r)| height(r).is_none_or(|h| h <= limit))
                .max_by_key(|(_, r)| bandwidth(r))
                .or_else(|| video.iter().min_by_key(|(_, r)| bandwidth(r))),
        },
    };
    let pick_audio = || match want.map(|(_, audio)| audio) {
        Some(id) => audio.iter().find(|(_, r)| r.attr("id") == Some(id)),
        None => audio.iter().max_by_key(|(_, r)| bandwidth(r)),
    };
    let plan = |chosen: Option<&(&Element, &Element)>| -> Result<Option<TrackPlan>, String> {
        let Some((set, representation)) = chosen else {
            return Ok(None);
        };
        if set.child("ContentProtection").is_some()
            || representation.child("ContentProtection").is_some()
        {
            return Err(DRM.into());
        }
        track(&base, set, representation, duration).map(Some)
    };
    let video = plan(pick_video())?;
    let audio = plan(pick_audio())?;
    if video.is_none() && audio.is_none() {
        return Err("the manifest has no video or audio".into());
    }
    let update_period = mpd
        .attr("minimumUpdatePeriod")
        .and_then(iso_duration)
        .unwrap_or(2.0);
    Ok(Manifest {
        live,
        update_period,
        video,
        audio,
    })
}

fn track(
    base: &Url,
    set: &Element,
    representation: &Element,
    duration: Option<f64>,
) -> Result<TrackPlan, String> {
    let base = base_url(&base_url(base, set), representation);
    let id = representation.attr("id").unwrap_or("").to_string();
    let bandwidth = representation.attr("bandwidth").unwrap_or("0");
    let mime = representation
        .attr("mimeType")
        .or(set.attr("mimeType"))
        .unwrap_or("");
    let mp4 = !mime.contains("mp2t");
    let template = representation
        .child("SegmentTemplate")
        .or(set.child("SegmentTemplate"));
    let list = representation
        .child("SegmentList")
        .or(set.child("SegmentList"));
    let join = |path: &str| {
        base.join(path)
            .map_err(|_| format!("bad segment URL `{path}`"))
    };

    if let Some(template) = template {
        let fill =
            |pattern: &str, number: u64, time: u64| expand(pattern, &id, bandwidth, number, time);
        let init = template
            .attr("initialization")
            .map(|pattern| join(&fill(pattern, 0, 0)))
            .transpose()?;
        let media = template
            .attr("media")
            .ok_or("SegmentTemplate without media")?;
        let start: u64 = template
            .attr("startNumber")
            .and_then(|n| n.parse().ok())
            .unwrap_or(1);
        let timescale: f64 = template
            .attr("timescale")
            .and_then(|t| t.parse().ok())
            .unwrap_or(1.0);
        let mut segments = Vec::new();
        if let Some(timeline) = template.child("SegmentTimeline") {
            let (mut time, mut number) = (0u64, start);
            for s in timeline.children("S") {
                time = s.attr("t").and_then(|t| t.parse().ok()).unwrap_or(time);
                let d: u64 = s
                    .attr("d")
                    .and_then(|d| d.parse().ok())
                    .ok_or("S without d")?;
                let repeat: i64 = s.attr("r").and_then(|r| r.parse().ok()).unwrap_or(0);
                for _ in 0..=repeat.max(0) {
                    segments.push((time, piece(join(&fill(media, number, time))?)));
                    time += d;
                    number += 1;
                }
            }
        } else {
            let length: f64 = template
                .attr("duration")
                .and_then(|d| d.parse().ok())
                .ok_or("SegmentTemplate without duration")?;
            // Live templates without a timeline need wall-clock maths
            // against availabilityStartTime; few servers use them.
            let total = duration.ok_or("live manifests need a SegmentTimeline")?;
            let count = (total * timescale / length).ceil() as u64;
            for index in 0..count {
                let number = start + index;
                segments.push((
                    number,
                    piece(join(&fill(media, number, index * length as u64))?),
                ));
            }
        }
        return Ok(TrackPlan {
            id,
            init: init.map(piece),
            segments,
            mp4,
        });
    }
    if let Some(list) = list {
        let init = list.child("Initialization").and_then(|i| {
            let url = i
                .attr("sourceURL")
                .map_or_else(|| Ok(base.clone()), join)
                .ok()?;
            Some(Piece {
                url,
                range: i.attr("range").and_then(byte_range),
                key: None,
            })
        });
        let mut segments = Vec::new();
        for (index, segment) in list.children("SegmentURL").enumerate() {
            let url = segment
                .attr("media")
                .map_or_else(|| Ok(base.clone()), join)?;
            segments.push((
                index as u64,
                Piece {
                    url,
                    range: segment.attr("mediaRange").and_then(byte_range),
                    key: None,
                },
            ));
        }
        return Ok(TrackPlan {
            id,
            init,
            segments,
            mp4,
        });
    }
    // A single file per representation (SegmentBase or bare BaseURL).
    Ok(TrackPlan {
        id,
        init: None,
        segments: vec![(0, piece(base))],
        mp4,
    })
}

fn piece(url: Url) -> Piece {
    Piece {
        url,
        range: None,
        key: None,
    }
}

fn base_url(parent: &Url, element: &Element) -> Url {
    element
        .child("BaseURL")
        .and_then(|base| parent.join(base.text.trim()).ok())
        .unwrap_or_else(|| parent.clone())
}

/// Fills `$RepresentationID$`, `$Number$` (optionally `%05d`), `$Time$`,
/// `$Bandwidth$`, and `$$`.
fn expand(pattern: &str, id: &str, bandwidth: &str, number: u64, time: u64) -> String {
    let mut out = String::new();
    let mut parts = pattern.split('$');
    out.push_str(parts.next().unwrap_or(""));
    let mut inside = true;
    for part in parts {
        if inside {
            let (name, format) = part
                .split_once('%')
                .map_or((part, None), |(n, f)| (n, Some(f)));
            let value = match name {
                "" => "$".to_string(),
                "RepresentationID" => id.to_string(),
                "Bandwidth" => bandwidth.to_string(),
                "Number" => pad(number, format),
                "Time" => pad(time, format),
                other => format!("${other}$"),
            };
            out.push_str(&value);
        } else {
            out.push_str(part);
        }
        inside = !inside;
    }
    out
}

fn pad(value: u64, format: Option<&str>) -> String {
    let width = format
        .and_then(|f| f.trim_start_matches('0').trim_end_matches('d').parse().ok())
        .unwrap_or(0);
    format!("{value:0width$}")
}

/// `start-end` inclusive to an exclusive range.
fn byte_range(text: &str) -> Option<(u64, u64)> {
    let (start, end) = text.split_once('-')?;
    Some((
        start.trim().parse().ok()?,
        end.trim().parse::<u64>().ok()? + 1,
    ))
}

/// `PT1H2M3.5S` (and `P1D...`) to seconds.
fn iso_duration(text: &str) -> Option<f64> {
    let text = text.strip_prefix('P')?;
    let (days, time) = text.split_once('T').unwrap_or((text, ""));
    let mut seconds = 0.0;
    if let Some(days) = days.strip_suffix('D') {
        seconds += days.parse::<f64>().ok()? * 86_400.0;
    }
    let mut number = String::new();
    for c in time.chars() {
        match c {
            'H' => seconds += number.parse::<f64>().ok()? * 3600.0,
            'M' => seconds += number.parse::<f64>().ok()? * 60.0,
            'S' => seconds += number.parse::<f64>().ok()?,
            _ => {
                number.push(c);
                continue;
            }
        }
        number.clear();
    }
    Some(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url() -> Url {
        Url::parse("https://cdn.example/film/manifest.mpd").unwrap()
    }

    #[test]
    fn templates_with_numbers_and_durations() {
        let mpd = r#"<?xml version="1.0"?>
        <MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S">
          <Period><AdaptationSet contentType="video" mimeType="video/mp4">
            <SegmentTemplate initialization="$RepresentationID$/init.mp4" media="$RepresentationID$/seg-$Number%03d$.m4s" startNumber="1" duration="4" timescale="1"/>
            <Representation id="v720" bandwidth="2000000" height="720"/>
            <Representation id="v1080" bandwidth="5000000" height="1080"/>
          </AdaptationSet>
          <AdaptationSet contentType="audio"><Representation id="a" bandwidth="128000" mimeType="audio/mp4">
            <BaseURL>audio.mp4</BaseURL></Representation></AdaptationSet></Period></MPD>"#;
        let manifest = parse(mpd, &url(), Quality::Best, None).unwrap();
        let video = manifest.video.unwrap();
        assert_eq!(video.id, "v1080");
        assert_eq!(video.init.unwrap().url.path(), "/film/v1080/init.mp4");
        let names: Vec<_> = video
            .segments
            .iter()
            .map(|(_, p)| p.url.path().to_string())
            .collect();
        assert_eq!(
            names,
            [
                "/film/v1080/seg-001.m4s",
                "/film/v1080/seg-002.m4s",
                "/film/v1080/seg-003.m4s"
            ]
        );
        assert_eq!(
            manifest.audio.unwrap().segments[0].1.url.path(),
            "/film/audio.mp4"
        );
        let small = parse(mpd, &url(), Quality::MaxHeight(720), None).unwrap();
        assert_eq!(small.video.unwrap().id, "v720");
    }

    #[test]
    fn timelines_lists_and_live_manifests() {
        let mpd = r#"<MPD type="dynamic" minimumUpdatePeriod="PT1S"><Period>
          <AdaptationSet mimeType="video/mp4"><Representation id="v" bandwidth="1">
            <SegmentTemplate media="t-$Time$.m4s" initialization="i.mp4">
              <SegmentTimeline><S t="1000" d="500" r="2"/><S d="400"/></SegmentTimeline>
            </SegmentTemplate></Representation></AdaptationSet></Period></MPD>"#;
        let manifest = parse(mpd, &url(), Quality::Best, None).unwrap();
        assert!(manifest.live);
        assert_eq!(manifest.update_period, 1.0);
        let times: Vec<u64> = manifest
            .video
            .unwrap()
            .segments
            .iter()
            .map(|(t, _)| *t)
            .collect();
        assert_eq!(times, [1000, 1500, 2000, 2500]);

        let list = r#"<MPD><Period><AdaptationSet mimeType="audio/mp4"><Representation id="a">
          <BaseURL>track.mp4</BaseURL><SegmentList><Initialization range="0-99"/>
          <SegmentURL mediaRange="100-199"/><SegmentURL mediaRange="200-299"/></SegmentList>
          </Representation></AdaptationSet></Period></MPD>"#;
        let audio = parse(list, &url(), Quality::Best, None)
            .unwrap()
            .audio
            .unwrap();
        assert_eq!(audio.init.unwrap().range, Some((0, 100)));
        assert_eq!(audio.segments[1].1.range, Some((200, 300)));
    }

    #[test]
    fn protected_content_is_refused() {
        let mpd = r#"<MPD><Period><AdaptationSet mimeType="video/mp4">
          <ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"/>
          <Representation id="v"><BaseURL>v.mp4</BaseURL></Representation></AdaptationSet></Period></MPD>"#;
        assert_eq!(parse(mpd, &url(), Quality::Best, None).unwrap_err(), DRM);
    }

    #[test]
    fn helpers() {
        assert_eq!(iso_duration("PT1H2M3.5S"), Some(3723.5));
        assert_eq!(iso_duration("P1DT1S"), Some(86_401.0));
        assert_eq!(
            expand("a$$b-$Number%05d$-$Time$", "x", "1", 7, 9),
            "a$b-00007-9"
        );
    }
}
