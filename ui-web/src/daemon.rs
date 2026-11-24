//! The way to the daemon: `host.request`, as a trait so the handlers can be tested without one.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use oxidepm_adapter_sdk::{Host, HostError};
use serde_json::Value;

/// The most a request waits for the daemon.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Why a request got no response from the daemon (an `error` response is an answer, not this).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fail {
    /// The daemon refused the call itself (the grant, the rate limit, a busy or stopping daemon).
    Refused {
        name: Option<String>,
        message: String,
    },
    /// The daemon went away.
    Gone,
    /// It did not answer in time.
    Timeout,
}

#[async_trait]
pub trait Daemon: Send + Sync {
    /// Send `request` (the object the CLI would send) and return the response object.
    async fn call(&self, request: Value) -> Result<Value, Fail>;
}

/// The daemon behind `host.request`.
pub struct HostDaemon {
    pub host: Host,
}

#[async_trait]
impl Daemon for HostDaemon {
    async fn call(&self, request: Value) -> Result<Value, Fail> {
        match tokio::time::timeout(REQUEST_TIMEOUT, self.host.request(request)).await {
            Err(_) => Err(Fail::Timeout),
            Ok(Ok(response)) => Ok(response),
            Ok(Err(HostError::Gone)) => Err(Fail::Gone),
            Ok(Err(HostError::Refused(e))) => Err(Fail::Refused {
                name: e.name().map(str::to_string),
                message: e.message,
            }),
        }
    }
}

/// How fast this adapter asks the daemon. The daemon allows an adapter 50 requests a second and
/// strikes one that keeps asking over that; this stays well below it however many browsers poll.
pub struct Limiter {
    rate: f64,
    burst: f64,
    state: Mutex<(f64, Instant)>,
}

impl Limiter {
    pub fn new(per_second: f64, burst: f64) -> Limiter {
        Limiter {
            rate: per_second,
            burst,
            state: Mutex::new((burst, Instant::now())),
        }
    }

    pub fn standard() -> Limiter {
        Limiter::new(20.0, 40.0)
    }

    /// Take one token if there is one.
    pub fn take(&self) -> bool {
        self.take_at(Instant::now())
    }

    fn take_at(&self, now: Instant) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let elapsed = now.saturating_duration_since(state.1).as_secs_f64();
        state.0 = (state.0 + elapsed * self.rate).min(self.burst);
        state.1 = now;
        if state.0 >= 1.0 {
            state.0 -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limiter_allows_a_burst_then_the_rate() {
        let limiter = Limiter::new(10.0, 3.0);
        let t0 = Instant::now();
        assert!(limiter.take_at(t0) && limiter.take_at(t0) && limiter.take_at(t0));
        assert!(!limiter.take_at(t0), "the burst is spent");
        assert!(
            !limiter.take_at(t0 + Duration::from_millis(50)),
            "half a token is not one"
        );
        assert!(
            limiter.take_at(t0 + Duration::from_millis(120)),
            "a token came back"
        );
        assert!(!limiter.take_at(t0 + Duration::from_millis(120)));
        // never more than the burst, however long it rests
        let later = t0 + Duration::from_secs(60);
        assert!(limiter.take_at(later) && limiter.take_at(later) && limiter.take_at(later));
        assert!(!limiter.take_at(later));
    }
}
