//! Decides what to do after a failed request.
//!
//! Getting this right is what keeps the engine from being banned: it backs
//! off instead of hammering, honours `Retry-After`, and gives up on errors
//! that retrying cannot fix.

use std::fmt;
use std::time::Duration;

use reqwest::StatusCode;

use crate::constants;
use crate::error::EngineError;

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Attempts per connection before the source is given up on. The counter
    /// resets whenever an attempt made progress.
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
    /// Cap for server-provided `Retry-After` values.
    pub max_retry_after: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: constants::MAX_ATTEMPTS,
            base_delay: constants::RETRY_BASE_DELAY,
            max_delay: constants::RETRY_MAX_DELAY,
            max_retry_after: constants::MAX_RETRY_AFTER,
        }
    }
}

pub(crate) type FetchResult<T> = std::result::Result<T, FetchError>;

/// Why a single request failed.
#[derive(Debug)]
pub(crate) enum FetchError {
    Transport(reqwest::Error),
    /// A transport failure on a protocol other than HTTP. Only the
    /// optional protocols raise it.
    #[cfg_attr(not(feature = "ftp"), allow(dead_code))]
    Connection(String),
    Status {
        status: StatusCode,
        retry_after: Option<Duration>,
    },
    /// The server closed the connection before sending the whole range.
    ClosedEarly,
    /// The connection was much slower than it should be and was dropped
    /// to try a fresh one.
    Slow,
    /// A ranged request got the whole file (`If-Range` mismatch or ranges
    /// ignored) or a 416: this is no longer the file we started with.
    Changed,
    Protocol(String),
    Fatal(EngineError),
    Cancelled,
}

impl From<EngineError> for FetchError {
    fn from(error: EngineError) -> Self {
        match error {
            EngineError::Cancelled => Self::Cancelled,
            other => Self::Fatal(other),
        }
    }
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "{}", describe_transport(error)),
            Self::Connection(message) => f.write_str(message),
            Self::Status { status, .. } => write!(f, "server answered {status}"),
            Self::ClosedEarly => f.write_str("connection closed early"),
            Self::Slow => f.write_str("connection too slow, reconnecting"),
            Self::Changed => f.write_str("file changed on the server"),
            Self::Protocol(message) => f.write_str(message),
            Self::Fatal(error) => write!(f, "{error}"),
            Self::Cancelled => f.write_str("stopped"),
        }
    }
}

impl FetchError {
    pub(crate) fn into_engine_error(self, url: &str) -> EngineError {
        match self {
            Self::Transport(error) => EngineError::network(error),
            Self::Connection(message) => EngineError::Connection(message),
            Self::Status { status, .. } => EngineError::HttpStatus {
                status: status.as_u16(),
                url: url.to_string(),
            },
            Self::ClosedEarly => EngineError::Protocol("connection closed early".into()),
            Self::Slow => EngineError::Protocol("connection too slow".into()),
            Self::Changed => EngineError::RemoteChanged,
            Self::Protocol(message) => EngineError::Protocol(message),
            Self::Fatal(error) => error,
            Self::Cancelled => EngineError::Cancelled,
        }
    }
}

fn describe_transport(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "connection timed out"
    } else if error.is_connect() {
        "could not connect"
    } else if error.is_redirect() {
        "redirect refused"
    } else if error.is_body() || error.is_decode() {
        "connection broke while receiving data"
    } else {
        "network error"
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RetryAction {
    Retry(Duration),
    /// Resolve the original link again (it may have expired), then retry.
    Refresh,
    /// This source is unusable; continue with the others, if any.
    DropSource,
    Abort,
}

impl RetryPolicy {
    pub(crate) fn action(&self, error: &FetchError, attempt: u32, refreshed: bool) -> RetryAction {
        match error {
            FetchError::Cancelled | FetchError::Fatal(_) => RetryAction::Abort,
            FetchError::Changed | FetchError::Protocol(_) => RetryAction::DropSource,
            FetchError::Transport(error) if error.is_redirect() => RetryAction::DropSource,
            FetchError::Transport(_) | FetchError::Connection(_) | FetchError::ClosedEarly => {
                self.retry_or_drop(attempt, None)
            }
            // Reconnecting right away is the point; the connection made progress.
            FetchError::Slow => RetryAction::Retry(Duration::ZERO),
            FetchError::Status {
                status,
                retry_after,
            } => match status.as_u16() {
                429 | 503 => self.retry_or_drop(attempt, *retry_after),
                408 | 425 | 500 | 502 | 504 => self.retry_or_drop(attempt, None),
                401 | 403 | 410 if !refreshed => RetryAction::Refresh,
                _ => RetryAction::DropSource,
            },
        }
    }

    fn retry_or_drop(&self, attempt: u32, server_hint: Option<Duration>) -> RetryAction {
        if attempt >= self.max_attempts {
            return RetryAction::DropSource;
        }
        let delay = match server_hint {
            Some(hint) => hint.min(self.max_retry_after),
            None => self.backoff(attempt),
        };
        RetryAction::Retry(delay)
    }

    /// Exponential backoff with jitter, so connections that failed together
    /// do not all come back at the same moment.
    pub(crate) fn backoff(&self, attempt: u32) -> Duration {
        let factor = 1u32 << attempt.clamp(1, 16).saturating_sub(1);
        let ceiling = self.base_delay.saturating_mul(factor).min(self.max_delay);
        let ceiling_ms = u64::try_from(ceiling.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        Duration::from_millis(fastrand::u64(ceiling_ms / 2..=ceiling_ms))
    }
}

pub(crate) fn is_throttle(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(code: u16, retry_after: Option<Duration>) -> FetchError {
        FetchError::Status {
            status: StatusCode::from_u16(code).unwrap(),
            retry_after,
        }
    }

    #[test]
    fn honours_retry_after_with_a_cap() {
        let policy = RetryPolicy::default();
        let asked = Duration::from_secs(30);
        assert_eq!(
            policy.action(&status(429, Some(asked)), 1, false),
            RetryAction::Retry(asked)
        );
        let huge = Duration::from_secs(86_400);
        assert_eq!(
            policy.action(&status(503, Some(huge)), 1, false),
            RetryAction::Retry(policy.max_retry_after)
        );
    }

    #[test]
    fn refreshes_expired_links_once() {
        let policy = RetryPolicy::default();
        assert_eq!(
            policy.action(&status(403, None), 1, false),
            RetryAction::Refresh
        );
        assert_eq!(
            policy.action(&status(403, None), 1, true),
            RetryAction::DropSource
        );
        assert_eq!(
            policy.action(&status(404, None), 1, false),
            RetryAction::DropSource
        );
    }

    #[test]
    fn gives_up_after_max_attempts() {
        let policy = RetryPolicy::default();
        let error = FetchError::ClosedEarly;
        assert!(matches!(
            policy.action(&error, 1, false),
            RetryAction::Retry(_)
        ));
        assert_eq!(
            policy.action(&error, policy.max_attempts, false),
            RetryAction::DropSource
        );
    }

    #[test]
    fn backoff_grows_and_stays_capped() {
        let policy = RetryPolicy::default();
        for attempt in 1..40 {
            let delay = policy.backoff(attempt);
            assert!(delay <= policy.max_delay);
            assert!(delay >= policy.base_delay / 2);
        }
        assert!(policy.backoff(1) <= policy.base_delay);
    }
}
