//! The bridge between the browser extension and the service.
//!
//! Browsers start a "native messaging host" program and talk to it over
//! its standard input and output: each message is a 32-bit length in
//! native byte order followed by that many bytes of JSON. Only the
//! extension IDs listed in the host's manifest may start it.
//!
//! The bridge accepts two messages and nothing else, checks every URL,
//! limits message size, and turns a download into a job on the service.
//! It never runs commands and never touches files itself.

use std::path::Path;

use hiroi_proto::{NewJob, Request, Response};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::client::{Client, ClientError};

/// Name the manifests register the host under.
pub const HOST_NAME: &str = "dev.hiroi.downloader";
/// Messages from the extension larger than this are refused.
pub const MAX_MESSAGE: usize = 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    /// Is the service reachable?
    Ping,
    Download(BrowserDownload),
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
}

impl Answer {
    fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            job: None,
            version: None,
            error: Some(message.into()),
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
            Err(answer) => return answer,
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
            ok: true,
            job: Some(job.0),
            version: None,
            error: None,
        },
        Ok(Response::Version { program, .. }) => Answer {
            ok: true,
            job: None,
            version: Some(program),
            error: None,
        },
        Ok(_) => Answer {
            ok: true,
            job: None,
            version: None,
            error: None,
        },
        Err(ClientError::Server(message)) => Answer::error(message),
        Err(error) => {
            // The service may have restarted; reconnect next time.
            *client = None;
            Answer::error(error.to_string())
        }
    }
}

/// Validates a browser download and turns it into a job.
pub fn job(download: BrowserDownload) -> Result<NewJob, Answer> {
    let url =
        hiroi_engine::Url::parse(download.url.trim()).map_err(|_| Answer::error("invalid URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Answer::error(
            "only http and https downloads can be handed over",
        ));
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
    Ok(NewJob {
        url: url.to_string(),
        file_name: download.file_name.filter(|name| !name.is_empty()),
        queue: download.queue,
        headers,
        ..NewJob::default()
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
