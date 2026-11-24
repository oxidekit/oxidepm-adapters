//! The rate limit against crash loops: five messages per app and event type in five minutes. A
//! critical alert is never held back (a crash, a crash loop, an unhealthy app is what the person
//! most wants to hear about), so the limit applies to the rest.

use std::collections::HashMap;
use std::time::{Duration, Instant};

pub const WINDOW: Duration = Duration::from_secs(300);
pub const MAX_PER_WINDOW: usize = 5;
/// More keys than this and the entries whose window is over are forgotten.
const MAX_KEYS: usize = 4096;

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Send,
    /// This message is over the limit and is the first: send this notice instead.
    Notice(String),
    /// Over the limit, and the notice was sent: say nothing.
    Drop,
}

#[derive(Default)]
pub struct Limiter {
    seen: HashMap<(String, &'static str), (usize, Instant)>,
}

impl Limiter {
    pub fn check(
        &mut self,
        name: &str,
        event: &'static str,
        critical: bool,
        now: Instant,
    ) -> Verdict {
        if critical {
            return Verdict::Send;
        }
        if self.seen.len() >= MAX_KEYS {
            self.seen
                .retain(|_, (_, start)| now.saturating_duration_since(*start) <= WINDOW);
        }
        let entry = self
            .seen
            .entry((name.to_string(), event))
            .or_insert((0, now));
        if now.saturating_duration_since(entry.1) > WINDOW {
            *entry = (0, now);
        }
        entry.0 += 1;
        match entry.0 {
            n if n <= MAX_PER_WINDOW => Verdict::Send,
            n if n == MAX_PER_WINDOW + 1 => Verdict::Notice(format!(
                "Notifications for '{name}' ({event}) are rate limited: {MAX_PER_WINDOW} in {} s. Further ones are held back for now.",
                WINDOW.as_secs()
            )),
            _ => Verdict::Drop,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sixth_message_in_a_window_becomes_one_notice_then_silence() {
        let mut limiter = Limiter::default();
        let t0 = Instant::now();
        for _ in 0..MAX_PER_WINDOW {
            assert_eq!(limiter.check("api", "restart", false, t0), Verdict::Send);
        }
        assert!(
            matches!(limiter.check("api", "restart", false, t0), Verdict::Notice(n) if n.contains("'api' (restart)"))
        );
        assert_eq!(limiter.check("api", "restart", false, t0), Verdict::Drop);
        // another app, another type, and a new window start afresh
        assert_eq!(limiter.check("web", "restart", false, t0), Verdict::Send);
        assert_eq!(limiter.check("api", "start", false, t0), Verdict::Send);
        assert_eq!(
            limiter.check(
                "api",
                "restart",
                false,
                t0 + WINDOW + Duration::from_secs(1)
            ),
            Verdict::Send
        );
    }

    #[test]
    fn critical_alerts_are_never_limited() {
        let mut limiter = Limiter::default();
        let t0 = Instant::now();
        for _ in 0..50 {
            assert_eq!(limiter.check("api", "crash", true, t0), Verdict::Send);
        }
        // and they do not use up the allowance of the others
        assert_eq!(limiter.check("api", "crash", false, t0), Verdict::Send);
    }

    #[test]
    fn the_table_does_not_grow_without_bound() {
        let mut limiter = Limiter::default();
        let t0 = Instant::now();
        for n in 0..(MAX_KEYS + 10) {
            limiter.check(&format!("app{n}"), "start", false, t0);
        }
        limiter.check("late", "start", false, t0 + WINDOW + Duration::from_secs(1));
        assert!(limiter.seen.len() < MAX_KEYS, "{}", limiter.seen.len());
    }
}
