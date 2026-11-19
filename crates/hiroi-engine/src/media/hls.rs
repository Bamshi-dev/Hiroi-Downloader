//! HLS playlists (RFC 8216): master playlists list variants of a stream
//! at different qualities; media playlists list the segments.

use reqwest::Url;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Playlist {
    Master(Master),
    Media(MediaPlaylist),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Master {
    pub variants: Vec<Variant>,
    pub audio: Vec<Rendition>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Variant {
    pub uri: Url,
    pub bandwidth: u64,
    pub height: Option<u32>,
    pub audio_group: Option<String>,
}

/// An alternative audio track of a master playlist.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Rendition {
    pub group: String,
    pub uri: Option<Url>,
    pub default: bool,
    pub language: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct MediaPlaylist {
    pub target_duration: f64,
    pub media_sequence: u64,
    pub segments: Vec<Segment>,
    /// `#EXT-X-ENDLIST`: no more segments will come. Without it, the
    /// playlist is live and must be fetched again.
    pub ended: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Segment {
    pub sequence: u64,
    pub uri: Url,
    /// `(start, end)` exclusive, from `#EXT-X-BYTERANGE`.
    pub range: Option<(u64, u64)>,
    pub key: Option<Key>,
    /// Initialization section (fMP4 streams) this segment needs.
    pub map: Option<Init>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Init {
    pub uri: Url,
    pub range: Option<(u64, u64)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Key {
    pub uri: Url,
    /// Explicit IV; otherwise the media sequence number is the IV.
    pub iv: Option<[u8; 16]>,
}

pub(crate) fn is_playlist(text: &str) -> bool {
    text.trim_start_matches('\u{feff}')
        .trim_start()
        .starts_with("#EXTM3U")
}

pub(crate) fn parse(text: &str, base: &Url) -> Result<Playlist, String> {
    if !is_playlist(text) {
        return Err("not an HLS playlist".into());
    }
    if text.contains("#EXT-X-STREAM-INF") {
        return parse_master(text, base).map(Playlist::Master);
    }
    parse_media(text, base).map(Playlist::Media)
}

fn parse_master(text: &str, base: &Url) -> Result<Master, String> {
    let mut master = Master::default();
    let mut pending: Option<Vec<(String, String)>> = None;
    for line in lines(text) {
        if let Some(attributes) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(attribute_list(attributes));
        } else if let Some(attributes) = line.strip_prefix("#EXT-X-MEDIA:") {
            let attributes = attribute_list(attributes);
            if get(&attributes, "TYPE") == Some("AUDIO") {
                master.audio.push(Rendition {
                    group: get(&attributes, "GROUP-ID").unwrap_or_default().to_string(),
                    uri: get(&attributes, "URI").and_then(|uri| base.join(uri).ok()),
                    default: get(&attributes, "DEFAULT") == Some("YES"),
                    language: get(&attributes, "LANGUAGE").map(str::to_string),
                });
            }
        } else if !line.starts_with('#')
            && let Some(attributes) = pending.take()
        {
            let uri = base
                .join(line)
                .map_err(|_| format!("bad variant URI `{line}`"))?;
            let height = get(&attributes, "RESOLUTION")
                .and_then(|r| r.split_once('x'))
                .and_then(|(_, height)| height.parse().ok());
            master.variants.push(Variant {
                uri,
                bandwidth: get(&attributes, "BANDWIDTH")
                    .and_then(|b| b.parse().ok())
                    .unwrap_or(0),
                height,
                audio_group: get(&attributes, "AUDIO").map(str::to_string),
            });
        }
    }
    if master.variants.is_empty() {
        return Err("master playlist without variants".into());
    }
    Ok(master)
}

fn parse_media(text: &str, base: &Url) -> Result<MediaPlaylist, String> {
    let mut playlist = MediaPlaylist {
        target_duration: 6.0,
        ..MediaPlaylist::default()
    };
    let mut key: Option<Key> = None;
    let mut map: Option<Init> = None;
    let mut range: Option<(u64, Option<u64>)> = None;
    // Where the previous byte range ended, per URI, for ranges without offset.
    let mut last_end: Option<(Url, u64)> = None;
    let mut sequence = None;

    for line in lines(text) {
        if let Some(value) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            playlist.target_duration = value.trim().parse().unwrap_or(6.0);
        } else if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            playlist.media_sequence = value.trim().parse().map_err(|_| "bad media sequence")?;
        } else if line.starts_with("#EXT-X-ENDLIST") || line.starts_with("#EXT-X-PLAYLIST-TYPE:VOD")
        {
            playlist.ended = true;
        } else if let Some(attributes) = line.strip_prefix("#EXT-X-KEY:") {
            key = parse_key(&attribute_list(attributes), base)?;
        } else if let Some(attributes) = line.strip_prefix("#EXT-X-MAP:") {
            let attributes = attribute_list(attributes);
            let uri = get(&attributes, "URI").ok_or("EXT-X-MAP without URI")?;
            let uri = base.join(uri).map_err(|_| "bad EXT-X-MAP URI")?;
            let range =
                get(&attributes, "BYTERANGE")
                    .and_then(parse_byterange)
                    .map(|(len, offset)| {
                        let start = offset.unwrap_or(0);
                        (start, start + len)
                    });
            map = Some(Init { uri, range });
        } else if let Some(value) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            range = Some(parse_byterange(value).ok_or("bad EXT-X-BYTERANGE")?);
        } else if !line.starts_with('#') {
            let uri = base
                .join(line)
                .map_err(|_| format!("bad segment URI `{line}`"))?;
            let number = sequence.map_or(playlist.media_sequence, |n: u64| n + 1);
            sequence = Some(number);
            let range = range.take().map(|(len, offset)| {
                let start = offset.unwrap_or_else(|| match &last_end {
                    Some((previous, end)) if *previous == uri => *end,
                    _ => 0,
                });
                (start, start + len)
            });
            last_end = range.map(|(_, end)| (uri.clone(), end));
            playlist.segments.push(Segment {
                sequence: number,
                uri,
                range,
                key: key.clone(),
                map: map.clone(),
            });
        }
    }
    Ok(playlist)
}

/// `METHOD=AES-128` is plain encryption with a published key, which any
/// player decrypts. Every other method (SAMPLE-AES, key formats such as
/// FairPlay or Widevine) is DRM, which Hiroi does not touch.
fn parse_key(attributes: &[(String, String)], base: &Url) -> Result<Option<Key>, String> {
    match get(attributes, "METHOD") {
        None | Some("NONE") => Ok(None),
        Some("AES-128") => {
            let format = get(attributes, "KEYFORMAT").unwrap_or("identity");
            if format != "identity" {
                return Err(DRM.into());
            }
            let uri = get(attributes, "URI").ok_or("EXT-X-KEY without URI")?;
            let uri = base.join(uri).map_err(|_| "bad key URI")?;
            let iv = get(attributes, "IV").map(parse_iv).transpose()?;
            Ok(Some(Key { uri, iv }))
        }
        Some(_) => Err(DRM.into()),
    }
}

pub(crate) const DRM: &str = "this stream is protected by DRM, which Hiroi does not download";

fn parse_iv(value: &str) -> Result<[u8; 16], String> {
    let hex = value.trim_start_matches("0x").trim_start_matches("0X");
    let bytes = hex::decode(format!("{hex:0>32}")).map_err(|_| "bad IV")?;
    bytes
        .try_into()
        .map_err(|_| "IV is not 16 bytes".to_string())
}

/// `<length>[@<offset>]`
fn parse_byterange(value: &str) -> Option<(u64, Option<u64>)> {
    let value = value.trim().trim_matches('"');
    match value.split_once('@') {
        Some((length, offset)) => Some((length.parse().ok()?, Some(offset.parse().ok()?))),
        None => Some((value.parse().ok()?, None)),
    }
}

fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().map(str::trim).filter(|line| !line.is_empty())
}

/// `KEY=VALUE,KEY="quoted, value",...`
fn attribute_list(text: &str) -> Vec<(String, String)> {
    let mut attributes = Vec::new();
    let mut rest = text.trim();
    while let Some((key, after)) = rest.split_once('=') {
        let (value, remaining) = if let Some(quoted) = after.strip_prefix('"') {
            match quoted.split_once('"') {
                Some((value, remaining)) => (value, remaining.trim_start_matches(',')),
                None => (quoted, ""),
            }
        } else {
            match after.split_once(',') {
                Some((value, remaining)) => (value, remaining),
                None => (after, ""),
            }
        };
        attributes.push((key.trim().to_ascii_uppercase(), value.trim().to_string()));
        rest = remaining.trim();
    }
    attributes
}

fn get<'a>(attributes: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attributes
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://cdn.example/show/index.m3u8").unwrap()
    }

    #[test]
    fn master_playlists_list_variants_and_audio() {
        let text = "#EXTM3U\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",LANGUAGE=\"en\",DEFAULT=YES,URI=\"audio/en.m3u8\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS=\"avc1.4d401e,mp4a.40.2\",AUDIO=\"aac\"\n\
            low/index.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080\n\
            https://other.example/high.m3u8\n";
        let Playlist::Master(master) = parse(text, &base()).unwrap() else {
            panic!("not a master")
        };
        assert_eq!(master.variants.len(), 2);
        assert_eq!(
            master.variants[0].uri.as_str(),
            "https://cdn.example/show/low/index.m3u8"
        );
        assert_eq!(
            (
                master.variants[0].height,
                master.variants[0].audio_group.as_deref()
            ),
            (Some(360), Some("aac"))
        );
        assert_eq!(master.variants[1].bandwidth, 5_000_000);
        assert!(master.audio[0].default);
        assert_eq!(
            master.audio[0].uri.as_ref().unwrap().path(),
            "/show/audio/en.m3u8"
        );
    }

    #[test]
    fn media_playlists_with_keys_maps_and_ranges() {
        let text = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:10\n\
            #EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"700@0\"\n\
            #EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x000102030405060708090a0b0c0d0e0f\n\
            #EXTINF:4.0,\n#EXT-X-BYTERANGE:1000@700\nmedia.mp4\n\
            #EXTINF:4.0,\n#EXT-X-BYTERANGE:500\nmedia.mp4\n\
            #EXT-X-KEY:METHOD=NONE\n#EXTINF:2.5,\nlast.mp4\n#EXT-X-ENDLIST\n";
        let Playlist::Media(media) = parse(text, &base()).unwrap() else {
            panic!("not media")
        };
        assert!(media.ended);
        assert_eq!(media.target_duration, 4.0);
        let sequences: Vec<_> = media.segments.iter().map(|s| s.sequence).collect();
        assert_eq!(sequences, [10, 11, 12]);
        assert_eq!(media.segments[0].range, Some((700, 1700)));
        assert_eq!(
            media.segments[1].range,
            Some((1700, 2200)),
            "offset continues from the previous range"
        );
        assert_eq!(media.segments[0].key.as_ref().unwrap().iv.unwrap()[15], 15);
        assert!(media.segments[2].key.is_none());
        assert_eq!(
            media.segments[2].map.as_ref().unwrap().range,
            Some((0, 700))
        );
    }

    #[test]
    fn live_playlists_have_no_end_and_drm_is_refused() {
        let live = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:5\n#EXTINF:6,\na.ts\n";
        let Playlist::Media(media) = parse(live, &base()).unwrap() else {
            panic!()
        };
        assert!(!media.ended);
        let drm = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\"\n#EXTINF:6,\na.ts\n";
        assert_eq!(parse(drm, &base()).unwrap_err(), DRM);
        let widevine = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\",KEYFORMAT=\"com.widevine\"\n#EXTINF:6,\na.ts\n";
        assert_eq!(parse(widevine, &base()).unwrap_err(), DRM);
        assert!(parse("<html>", &base()).is_err());
    }
}
