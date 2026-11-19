use std::time::{Duration, Instant};

/// Weight of the newest sample in the moving average.
const SMOOTHING: f64 = 0.3;

/// Smoothed transfer speed. Only bytes moved during this run count, so a
/// resumed download does not report an inflated speed.
#[derive(Debug)]
pub(crate) struct SpeedMeter {
    last_bytes: u64,
    last_at: Instant,
    rate: f64,
}

impl SpeedMeter {
    pub fn new(bytes: u64) -> Self {
        Self {
            last_bytes: bytes,
            last_at: Instant::now(),
            rate: 0.0,
        }
    }

    /// Records the current byte count and returns bytes per second.
    pub fn sample(&mut self, bytes: u64) -> u64 {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_at).as_secs_f64();
        if elapsed >= 0.05 {
            let current = bytes.saturating_sub(self.last_bytes) as f64 / elapsed;
            self.rate = if self.rate == 0.0 {
                current
            } else {
                SMOOTHING * current + (1.0 - SMOOTHING) * self.rate
            };
            self.last_bytes = bytes;
            self.last_at = now;
        }
        self.rate as u64
    }

    pub fn eta(&self, remaining: u64) -> Option<Duration> {
        (self.rate >= 1.0).then(|| Duration::from_secs_f64(remaining as f64 / self.rate))
    }
}
