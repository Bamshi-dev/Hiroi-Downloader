//! Per-host connection limits that adapt to the server.
//!
//! Every connection to a host needs a permit from that host's gate. When the
//! host answers 429 or 503 the gate halves its limit and stops handing out
//! permits until the requested pause is over. After a quiet minute it lets
//! one more connection through again, up to the configured maximum
//! (additive increase, multiplicative decrease, as in TCP).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::constants::HOST_RECOVERY_WINDOW;

/// Throttle responses that arrive this close together count as one signal:
/// all connections usually get rejected at the same moment.
const THROTTLE_DEBOUNCE: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub(crate) struct HostLimiter {
    max_per_host: usize,
    gates: Mutex<HashMap<String, Arc<HostGate>>>,
}

impl HostLimiter {
    pub fn new(max_per_host: usize) -> Self {
        Self {
            max_per_host,
            gates: Mutex::new(HashMap::new()),
        }
    }

    pub fn gate(&self, host: &str) -> Arc<HostGate> {
        let mut gates = self.gates.lock();
        let gate = gates
            .entry(host.to_string())
            .or_insert_with(|| Arc::new(HostGate::new(self.max_per_host)));
        Arc::clone(gate)
    }
}

#[derive(Debug)]
pub(crate) struct HostGate {
    max: usize,
    state: Mutex<GateState>,
    released: Notify,
}

#[derive(Debug)]
struct GateState {
    limit: usize,
    active: usize,
    paused_until: Option<Instant>,
    last_change: Option<Instant>,
}

impl HostGate {
    fn new(max: usize) -> Self {
        let state = GateState {
            limit: max,
            active: 0,
            paused_until: None,
            last_change: None,
        };
        Self {
            max,
            state: Mutex::new(state),
            released: Notify::new(),
        }
    }

    /// Waits for a free connection slot on this host.
    pub async fn acquire(self: &Arc<Self>) -> HostPermit {
        loop {
            // Registered before checking, so a release in between is not missed.
            let notified = self.released.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let wait = {
                let mut state = self.state.lock();
                let now = Instant::now();
                state.recover(now, self.max);
                match state.paused_until {
                    Some(until) if until > now => Some(until - now),
                    _ if state.active < state.limit => {
                        state.active += 1;
                        return HostPermit {
                            gate: Arc::clone(self),
                        };
                    }
                    _ => None,
                }
            };
            match wait {
                Some(pause) => tokio::select! {
                    () = tokio::time::sleep(pause) => {}
                    () = &mut notified => {}
                },
                None => notified.await,
            }
        }
    }

    /// The host said "too many requests": back off.
    pub fn throttle(&self, pause: Duration) {
        let mut state = self.state.lock();
        let now = Instant::now();
        let recent = state
            .last_change
            .is_some_and(|at| now - at < THROTTLE_DEBOUNCE);
        if !recent {
            let in_use = state.active.max(1);
            state.limit = (state.limit.min(in_use) / 2).max(1);
            state.last_change = Some(now);
            tracing::info!(
                limit = state.limit,
                "host is throttling, reducing connections"
            );
        }
        let until = now + pause;
        state.paused_until = Some(
            state
                .paused_until
                .map_or(until, |current| current.max(until)),
        );
    }

    #[cfg(test)]
    pub fn limit(&self) -> usize {
        self.state.lock().limit
    }

    fn release(&self) {
        self.state.lock().active -= 1;
        self.released.notify_waiters();
    }
}

impl GateState {
    fn recover(&mut self, now: Instant, max: usize) {
        if self.limit >= max {
            return;
        }
        if self
            .last_change
            .is_some_and(|at| now - at >= HOST_RECOVERY_WINDOW)
        {
            self.limit += 1;
            self.last_change = Some(now);
        }
    }
}

/// A connection slot; given back on drop.
#[derive(Debug)]
pub(crate) struct HostPermit {
    gate: Arc<HostGate>,
}

impl Drop for HostPermit {
    fn drop(&mut self) {
        self.gate.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn never_exceeds_the_limit() {
        let gate = HostLimiter::new(2).gate("a:443");
        let first = gate.acquire().await;
        let _second = gate.acquire().await;
        let third = tokio::time::timeout(Duration::from_millis(50), gate.acquire()).await;
        assert!(third.is_err(), "a third permit must wait");
        drop(first);
        let third = tokio::time::timeout(Duration::from_millis(50), gate.acquire()).await;
        assert!(third.is_ok(), "a released permit must be handed on");
    }

    #[tokio::test]
    async fn throttling_halves_the_limit_and_pauses() {
        let gate = HostLimiter::new(8).gate("a:443");
        let permits: Vec<_> = futures_util::future::join_all((0..8).map(|_| gate.acquire())).await;
        gate.throttle(Duration::from_millis(100));
        gate.throttle(Duration::from_millis(100));
        assert_eq!(gate.limit(), 4, "two signals at once count as one");
        drop(permits);

        let started = Instant::now();
        let _permit = gate.acquire().await;
        assert!(
            started.elapsed() >= Duration::from_millis(90),
            "must wait out the pause"
        );
    }
}
