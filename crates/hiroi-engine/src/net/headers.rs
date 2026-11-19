//! Parsers for the response headers the engine relies on.

use std::time::{Duration, SystemTime};

use percent_encoding::percent_decode_str;

/// A parsed `Content-Range: bytes start-end/total` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContentRange {
    pub start: u64,
    /// Inclusive, as in the header.
    pub end: u64,
    pub total: Option<u64>,
}

pub(crate) fn parse_content_range(value: &str) -> Option<ContentRange> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    let (range, total) = rest.split_once('/')?;
    let total = match total.trim() {
        "*" => None,
        total => Some(total.parse().ok()?),
    };
    let (start, end) = range.trim().split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end: u64 = end.trim().parse().ok()?;
    if end < start || total.is_some_and(|total| end >= total) {
        return None;
    }
    Some(ContentRange { start, end, total })
}

/// `Retry-After` is either a number of seconds or an HTTP date.
pub(crate) fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(at.duration_since(now).unwrap_or(Duration::ZERO))
}

/// File name from `Content-Disposition`. `filename*` (RFC 6266, may hold
/// UTF-8) wins over the plain `filename` parameter. The result is not
/// sanitized yet.
pub(crate) fn content_disposition_file_name(value: &str) -> Option<String> {
    let mut plain = None;
    for param in split_params(value) {
        let Some((key, raw)) = param.split_once('=') else {
            continue;
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "filename*" => {
                if let Some(name) = decode_extended_value(raw.trim()) {
                    return Some(name);
                }
            }
            "filename" => plain = Some(unquote(raw.trim())),
            _ => {}
        }
    }
    plain.filter(|name| !name.is_empty())
}

/// `If-Range` only works with strong validators.
pub(crate) fn is_weak_etag(value: &str) -> bool {
    value.trim_start().starts_with("W/")
}

/// Splits on `;` outside of quoted strings.
fn split_params(value: &str) -> Vec<&str> {
    let mut params = Vec::new();
    let (mut start, mut quoted, mut escaped) = (0, false, false);
    for (i, c) in value.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ';' if !quoted => {
                params.push(&value[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    params.push(&value[start..]);
    params
}

fn unquote(value: &str) -> String {
    let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else {
        return value.to_string();
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `UTF-8''na%C3%AFve.txt` -> `naïve.txt` (RFC 8187).
fn decode_extended_value(value: &str) -> Option<String> {
    let mut parts = value.splitn(3, '\'');
    let charset = parts.next()?.to_ascii_lowercase();
    let _language = parts.next()?;
    let encoded = parts.next()?;
    let bytes: Vec<u8> = percent_decode_str(encoded).collect();
    let name = match charset.as_str() {
        "utf-8" => String::from_utf8(bytes).ok()?,
        "iso-8859-1" => bytes.into_iter().map(char::from).collect(),
        _ => return None,
    };
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range() {
        let range = parse_content_range("bytes 100-199/1000").unwrap();
        assert_eq!(
            range,
            ContentRange {
                start: 100,
                end: 199,
                total: Some(1000)
            }
        );
        assert_eq!(parse_content_range("bytes 0-0/*").unwrap().total, None);
        assert!(parse_content_range("bytes 200-100/1000").is_none());
        assert!(parse_content_range("bytes 0-1000/1000").is_none());
        assert!(parse_content_range("items 0-1/2").is_none());
    }

    #[test]
    fn retry_after_seconds_and_dates() {
        let now = httpdate::parse_http_date("Wed, 21 Oct 2026 07:28:00 GMT").unwrap();
        assert_eq!(
            parse_retry_after("120", now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2026 07:28:30 GMT", now),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2026 07:00:00 GMT", now),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("soon", now), None);
    }

    #[test]
    fn disposition_prefers_extended_name() {
        let header =
            r#"attachment; filename="fallback.txt"; filename*=UTF-8''na%C3%AFve%20file.txt"#;
        assert_eq!(
            content_disposition_file_name(header).as_deref(),
            Some("naïve file.txt")
        );
    }

    #[test]
    fn disposition_handles_quotes_and_semicolons() {
        let header = r#"attachment; filename="report; final \"v2\".pdf""#;
        assert_eq!(
            content_disposition_file_name(header).as_deref(),
            Some(r#"report; final "v2".pdf"#)
        );
        assert_eq!(content_disposition_file_name("inline"), None);
    }

    #[test]
    fn weak_etags() {
        assert!(is_weak_etag(r#"W/"abc""#));
        assert!(!is_weak_etag(r#""abc""#));
    }
}
