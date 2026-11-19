use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// Smallest burst allowed, so very low limits still move data in useful chunks.
const MIN_BURST: f64 = 16.0 * 1024.0;

/// Token-bucket speed limiter. One instance per download, plus one shared by
/// all downloads for the global limit. The limit can change at any time.
#[derive(Debug)]
pub(crate) struct RateLimiter {
    /// Bytes per second, 0 for unlimited.
    limit: AtomicU64,
    bucket: Mutex<Bucket>,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    updated: Instant,
}

impl RateLimiter {
    pub fn new(limit: Option<u64>) -> Self {
        Self {
            limit: AtomicU64::new(limit.unwrap_or(0)),
            bucket: Mutex::new(Bucket {
                tokens: 0.0,
                updated: Instant::now(),
            }),
        }
    }

    pub fn set_limit(&self, limit: Option<u64>) {
        self.limit.store(limit.unwrap_or(0), Ordering::Relaxed);
    }

    pub fn limit(&self) -> Option<u64> {
        Some(self.limit.load(Ordering::Relaxed)).filter(|&limit| limit > 0)
    }

    /// Waits until `bytes` may pass. Bursts of up to a quarter second of
    /// traffic go through immediately.
    pub async fn acquire(&self, bytes: usize) {
        let Some(delay) = self.take(bytes) else {
            return;
        };
        tokio::time::sleep(delay).await;
    }

    fn take(&self, bytes: usize) -> Option<Duration> {
        let limit = self.limit()? as f64;
        let mut bucket = self.bucket.lock();
        let now = Instant::now();
        let burst = (limit / 4.0).max(MIN_BURST);
        let refill = now.duration_since(bucket.updated).as_secs_f64() * limit;
        bucket.tokens = (bucket.tokens + refill).min(burst) - bytes as f64;
        bucket.updated = now;
        // A negative balance is a debt the caller pays by sleeping.
        (bucket.tokens < 0.0).then(|| Duration::from_secs_f64(-bucket.tokens / limit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_never_waits() {
        let limiter = RateLimiter::new(None);
        assert_eq!(limiter.take(usize::MAX / 2), None);
    }

    #[test]
    fn debt_matches_the_limit() {
        let limiter = RateLimiter::new(Some(1024 * 1024));
        let delay = limiter.take(1024 * 1024).unwrap();
        assert!(
            (delay.as_secs_f64() - 1.0).abs() < 0.05,
            "1 MiB at 1 MiB/s takes about 1 s"
        );
    }

    #[test]
    fn limit_can_be_lifted() {
        let limiter = RateLimiter::new(Some(1));
        limiter.set_limit(None);
        assert_eq!(limiter.take(1_000_000), None);
    }
}
