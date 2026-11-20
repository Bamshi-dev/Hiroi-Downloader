use std::time::{Duration, SystemTime};

use reqwest::header::{
    CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, HeaderMap,
    LAST_MODIFIED, RANGE, RETRY_AFTER,
};
use reqwest::{Response, StatusCode, Url};

use crate::net::client::Http;

use crate::net::headers;
use crate::net::retry::{FetchError, FetchResult};
use crate::storage::naming;

/// What a server told us about a file.
#[derive(Debug, Clone)]
pub(crate) struct RemoteFile {
    /// URL after redirects. Segment requests go here directly instead of
    /// through the redirector every time.
    pub final_url: Url,
    pub size: Option<u64>,
    /// Byte ranges work, so the file can be split and resumed.
    pub ranges: bool,
    pub file_name: Option<String>,
    /// `ETag` or `Last-Modified`, used to notice when the file changes.
    pub validator: Option<String>,
    pub content_type: Option<String>,
}

/// Asks for the first byte only. A server that supports ranges answers 206
/// with the full size in `Content-Range`, all in one round trip. This works
/// on more servers than a `HEAD` request.
pub(crate) async fn probe(
    client: &Http,
    url: &Url,
    headers: &HeaderMap,
) -> FetchResult<RemoteFile> {
    let request = || Ok::<_, FetchError>(client.get(url)?.headers(headers.clone()));
    let mut response = request()?
        .header(RANGE, "bytes=0-0")
        .send()
        .await
        .map_err(FetchError::from)?;

    // A 416 for the very first byte means the file is empty.
    if response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
        return Ok(describe(&response, Some(0), false));
    }

    // A few servers reject range requests outright; ask once more without.
    if rejects_ranges(response.status()) {
        response = request()?.send().await.map_err(FetchError::from)?;
    }

    let status = response.status();
    if !status.is_success() {
        return Err(FetchError::Status {
            status,
            retry_after: retry_after(&response),
        });
    }

    let remote = if status == StatusCode::PARTIAL_CONTENT {
        let range = header(&response, CONTENT_RANGE.as_str())
            .and_then(|value| headers::parse_content_range(&value))
            .filter(|range| range.start == 0);
        match range.and_then(|range| range.total) {
            Some(total) => describe(&response, Some(total), true),
            None => describe(&response, None, false),
        }
    } else {
        let size = header(&response, CONTENT_LENGTH.as_str()).and_then(|v| v.parse().ok());
        describe(&response, size, false)
    };
    // Dropping the response closes the connection without reading the body.
    Ok(remote)
}

pub(crate) fn retry_after(response: &Response) -> Option<Duration> {
    header(response, RETRY_AFTER.as_str())
        .and_then(|value| headers::parse_retry_after(&value, SystemTime::now()))
}

fn rejects_ranges(status: StatusCode) -> bool {
    status.is_client_error()
        && !matches!(
            status,
            StatusCode::NOT_FOUND | StatusCode::GONE | StatusCode::TOO_MANY_REQUESTS
        )
}

fn describe(response: &Response, size: Option<u64>, ranges: bool) -> RemoteFile {
    let final_url = response.url().clone();
    let file_name = header(response, CONTENT_DISPOSITION.as_str())
        .and_then(|value| headers::content_disposition_file_name(&value))
        .or_else(|| naming::file_name_from_url(&final_url));
    let validator = header(response, ETAG.as_str())
        .filter(|etag| !headers::is_weak_etag(etag))
        .or_else(|| header(response, LAST_MODIFIED.as_str()));
    let content_type = header(response, CONTENT_TYPE.as_str());
    RemoteFile {
        final_url,
        size,
        ranges,
        file_name,
        validator,
        content_type,
    }
}

fn header(response: &Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)?
        .to_str()
        .ok()
        .map(str::to_string)
}
