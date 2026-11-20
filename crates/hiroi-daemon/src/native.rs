//! The bridge between the browser extension and the service.
//!
//! Browsers start a "native messaging host" program and talk to it over
//! its standard input and output: each message is a 32-bit length in
//! native byte order followed by that many bytes of JSON. Only the
//! extension IDs listed in the host's manifest may start it.
//!
//! The bridge accepts three messages and nothing else (a ping, one
//! download, and a batch of downloads), checks every URL, limits message
//! size, and turns downloads into jobs on the service. It never runs
//! commands and never touches files itself.

use std::path::Path;

use hiroi_proto::{NewBatch, NewJob, Quality, Request, Response};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::client::{Client, ClientError};

/// Name the manifests register the host under.
pub const HOST_NAME: &str = "dev.hiroi.downloader";
/// Messages from the extension larger than this are refused.
pub const MAX_MESSAGE: usize = 1024 * 1024;
/// Most downloads in one batch.
pub const MAX_BATCH: usize = 2000;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    /// Is the service reachable?
    Ping,
    Download(BrowserDownload),
    /// Downloads chosen together, such as the videos of a page. Several
    /// form a group, named after them or the page when `group` is absent.
    Batch {
        items: Vec<BrowserDownload>,
        #[serde(default)]
        group: Option<String>,
        /// The page's title, for naming the group.
        #[serde(default)]
        title: Option<String>,
    },
}

/// A download handed over by the browser, with what the browser would
/// have sent: cookies, referrer, and user agent.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct BrowserDownload {
    pub url: String,
    pub referrer: Option<String>,
    pub cookies: Option<String>,
    pub user_agent: Option<String>,
    pub file_name: Option<String>,
    pub queue: Option<String>,
    /// The URL is a video page: let the site extractor find the media.
    pub extract: bool,
    /// `best`, `worst`, or a height such as 720.
    pub quality: Option<String>,
    pub paused: bool,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Answer {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// For a batch: its group, its jobs, and what was not added.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

impl Answer {
    fn ok() -> Self {
        Self {
            ok: true,
            job: None,
            version: None,
            error: None,
            group: None,
            jobs: Vec::new(),
            errors: Vec::new(),
        }
    }

    fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            ..Self::ok()
        }
    }
}

/// Serves one browser connection until the browser closes it.
pub async fn serve<R, W>(mut input: R, mut output: W, socket: &Path) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut client: Option<Client> = None;
    loop {
        let message = match read_message(&mut input).await? {
            None => return Ok(()),
            Some(Err(answer)) => answer,
            Some(Ok(message)) => handle(message, &mut client, socket).await,
        };
        write_message(&mut output, &message).await?;
    }
}

async fn handle(message: Message, client: &mut Option<Client>, socket: &Path) -> Answer {
    let request = match message {
        Message::Ping => Request::Version,
        Message::Download(download) => match job(download) {
            Ok(job) => Request::Add(Box::new(job)),
            Err(reason) => return Answer::error(reason),
        },
        Message::Batch {
            items,
            group,
            title,
        } => match batch(items, group, title) {
            Ok(batch) => Request::AddBatch(Box::new(batch)),
            Err(reason) => return Answer::error(reason),
        },
    };
    if client.is_none() {
        match Client::connect(socket).await {
            Ok(connected) => *client = Some(connected),
            Err(error) => return Answer::error(error.to_string()),
        }
    }
    let connection = client.as_mut().expect("connected above");
    match connection.call(request).await {
        Ok(Response::Added { job }) => Answer {
            job: Some(job.0),
            ..Answer::ok()
        },
        Ok(Response::Version { program, .. }) => Answer {
            version: Some(program),
            ..Answer::ok()
        },
        Ok(Response::Batch(added)) => Answer {
            ok: !added.jobs.is_empty() || added.errors.is_empty(),
            group: added.group,
            jobs: added.jobs.iter().map(|job| job.0).collect(),
            errors: added.errors,
            ..Answer::ok()
        },
        Ok(_) => Answer::ok(),
        Err(ClientError::Server(message)) => Answer::error(message),
        Err(error) => {
            // The service may have restarted; reconnect next time.
            *client = None;
            Answer::error(error.to_string())
        }
    }
}

/// Validates a browser download and turns it into a job.
pub fn job(download: BrowserDownload) -> Result<NewJob, String> {
    let url =
        hiroi_engine::Url::parse(download.url.trim()).map_err(|_| "invalid URL".to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("only http and https downloads can be handed over".to_string());
    }
    let mut headers = Vec::new();
    let mut header = |name: &str, value: Option<String>| {
        if let Some(value) = value.filter(|v| !v.is_empty() && !v.contains(['\r', '\n'])) {
            headers.push((name.to_string(), value));
        }
    };
    header("Cookie", download.cookies);
    header(
        "Referer",
        download
            .referrer
            .filter(|r| r.starts_with("http://") || r.starts_with("https://")),
    );
    header("User-Agent", download.user_agent);
    let quality = match download.quality.as_deref().map(str::trim) {
        None | Some("") | Some("best") => None,
        Some("worst") => Some(Quality::Worst),
        Some(height) => Some(Quality::MaxHeight(
            height
                .trim_end_matches('p')
                .parse()
                .map_err(|_| "quality must be best, worst, or a height".to_string())?,
        )),
    };
    Ok(NewJob {
        url: url.to_string(),
        file_name: download.file_name.filter(|name| !name.is_empty()),
        queue: download.queue,
        headers,
        extract: download.extract,
        quality,
        paused: download.paused,
        ..NewJob::default()
    })
}

/// Validates every download of a batch. One bad URL fails the batch, so
/// the user sees it instead of a group with a gap.
pub fn batch(
    items: Vec<BrowserDownload>,
    group: Option<String>,
    title: Option<String>,
) -> Result<NewBatch, String> {
    if items.is_empty() {
        return Err("the batch is empty".to_string());
    }
    if items.len() > MAX_BATCH {
        return Err(format!("at most {MAX_BATCH} downloads at once"));
    }
    let clean = |text: Option<String>| {
        text.map(|text| text.chars().filter(|c| !c.is_control()).collect::<String>())
            .filter(|text| !text.trim().is_empty())
    };
    Ok(NewBatch {
        jobs: items.into_iter().map(job).collect::<Result<_, _>>()?,
        group: clean(group),
        title: clean(title),
    })
}

/// Reads one length-prefixed message. `None` at end of input; an inner
/// `Err` is an answer to send back instead of a message.
async fn read_message<R: AsyncRead + Unpin>(
    input: &mut R,
) -> std::io::Result<Option<Result<Message, Answer>>> {
    let mut length = [0u8; 4];
    match input.read_exact(&mut length).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = u32::from_ne_bytes(length) as usize;
    if length > MAX_MESSAGE {
        // The rest of the stream cannot be trusted to line up again.
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "message too large",
        ));
    }
    let mut body = vec![0u8; length];
    input.read_exact(&mut body).await?;
    Ok(Some(serde_json::from_slice(&body).map_err(|error| {
        Answer::error(format!("unknown message: {error}"))
    })))
}

async fn write_message<W: AsyncWrite + Unpin>(
    output: &mut W,
    answer: &Answer,
) -> std::io::Result<()> {
    let body = serde_json::to_vec(answer).expect("answers always serialize");
    output.write_all(&(body.len() as u32).to_ne_bytes()).await?;
    output.write_all(&body).await?;
    output.flush().await
}

/// The manifest that tells a browser where the host is and who may use it.
pub fn manifest(host_path: &Path, browser: Browser, extension: &str) -> serde_json::Value {
    let mut manifest = serde_json::json!({
        "name": HOST_NAME,
        "description": "Hiroi Downloader",
        "path": host_path,
        "type": "stdio",
    });
    match browser {
        Browser::Chromium => {
            manifest["allowed_origins"] =
                serde_json::json!([format!("chrome-extension://{extension}/")])
        }
        Browser::Firefox => manifest["allowed_extensions"] = serde_json::json!([extension]),
    }
    manifest
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Browser {
    Chromium,
    Firefox,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downloads_become_jobs_with_browser_headers() {
        let download = BrowserDownload {
            url: "https://cdn.example/file.zip".into(),
            referrer: Some("https://site.example/page".into()),
            cookies: Some("session=1".into()),
            user_agent: Some("Firefox".into()),
            ..BrowserDownload::default()
        };
        let job = job(download).unwrap();
        let names: Vec<&str> = job.headers.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["Cookie", "Referer", "User-Agent"]);
    }

    #[test]
    fn rejects_other_schemes_and_header_injection() {
        let file = BrowserDownload {
            url: "file:///etc/passwd".into(),
            ..BrowserDownload::default()
        };
        assert!(job(file).is_err());
        let sneaky = BrowserDownload {
            url: "https://e.com/a".into(),
            cookies: Some("a=1\r\nX-Evil: 1".into()),
            referrer: Some("javascript:alert(1)".into()),
            ..BrowserDownload::default()
        };
        assert!(job(sneaky).unwrap().headers.is_empty());
    }

    #[test]
    fn batches_are_checked_whole() {
        let item = |url: &str| BrowserDownload {
            url: url.into(),
            ..BrowserDownload::default()
        };
        let batch = batch(
            vec![item("https://e.com/1.mp4"), item("https://e.com/2.mp4")],
            Some("  ".into()),
            Some("Talks\u{7}".into()),
        )
        .unwrap();
        assert_eq!(batch.jobs.len(), 2);
        assert_eq!(batch.group, None);
        assert_eq!(batch.title.as_deref(), Some("Talks"));
        assert!(
            super::batch(
                vec![item("https://e.com/1"), item("ftp://e.com/2")],
                None,
                None
            )
            .is_err()
        );
        assert!(super::batch(Vec::new(), None, None).is_err());
        let many = (0..=MAX_BATCH)
            .map(|i| item(&format!("https://e.com/{i}")))
            .collect();
        assert!(super::batch(many, None, None).is_err());
    }

    #[test]
    fn video_pages_and_qualities() {
        let page = BrowserDownload {
            url: "https://video.example/watch?v=1".into(),
            extract: true,
            quality: Some("720p".into()),
            ..BrowserDownload::default()
        };
        let job = job(page).unwrap();
        assert!(job.extract);
        assert_eq!(job.quality, Some(Quality::MaxHeight(720)));
        let bad = BrowserDownload {
            url: "https://video.example/".into(),
            quality: Some("sharp".into()),
            ..BrowserDownload::default()
        };
        assert!(super::job(bad).is_err());
    }

    #[tokio::test]
    async fn frames_messages_and_answers_without_a_service() {
        let mut input = Vec::new();
        for body in [r#"{"type":"ping"}"#.as_bytes(), b"{nonsense"] {
            input.extend_from_slice(&(body.len() as u32).to_ne_bytes());
            input.extend_from_slice(body);
        }
        let mut output = Vec::new();
        serve(
            &input[..],
            &mut output,
            Path::new("/nonexistent/hiroid.sock"),
        )
        .await
        .unwrap();

        let mut answers = Vec::new();
        let mut rest = &output[..];
        while !rest.is_empty() {
            let length = u32::from_ne_bytes(rest[..4].try_into().unwrap()) as usize;
            answers.push(String::from_utf8(rest[4..4 + length].to_vec()).unwrap());
            rest = &rest[4 + length..];
        }
        assert_eq!(answers.len(), 2);
        assert!(answers[0].contains("not running"), "{}", answers[0]);
        assert!(answers[1].contains("unknown message"), "{}", answers[1]);
    }

    #[tokio::test]
    async fn oversized_messages_end_the_session() {
        let input = ((MAX_MESSAGE + 1) as u32).to_ne_bytes();
        let result = serve(&input[..], Vec::new(), Path::new("/x")).await;
        assert!(result.is_err());
    }
}
