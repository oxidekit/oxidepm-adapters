//! Where the apps come from: the daemon, asked through `host.request`.
//!
//! A scrape must not become a request to the daemon for every caller. The daemon rates an adapter's
//! requests, and an adapter that keeps asking over its rate is struck, so one scraper in a loop
//! (or a hostile one) could get the exporter shut down. [`Cached`] asks at most once per interval,
//! lets concurrent scrapes wait for that one answer, and remembers a failure for the same time.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use oxidepm_adapter_sdk::{Host, HostError};
use serde_json::json;

use crate::render::{parse_status, App};

/// The most a scrape waits for the daemon.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// How long an answer, or a failure, is reused.
pub const CACHE_FOR: Duration = Duration::from_secs(1);

pub type Apps = Result<Arc<Vec<App>>, String>;

#[async_trait]
pub trait StatusSource: Send + Sync {
    /// The apps now, or the reason they could not be read. Never an empty list standing in for an error.
    async fn apps(&self) -> Apps;
}

/// The daemon behind `host.request`.
pub struct DaemonSource {
    pub host: Host,
    pub timeout: Duration,
}

#[async_trait]
impl StatusSource for DaemonSource {
    async fn apps(&self) -> Apps {
        let answer = match tokio::time::timeout(
            self.timeout,
            self.host.request(json!({ "type": "status" })),
        )
        .await
        {
            Err(_) => {
                return Err(format!(
                    "the daemon did not answer within {} seconds",
                    self.timeout.as_secs()
                ))
            }
            Ok(Err(HostError::Gone)) => return Err("the daemon is gone".to_string()),
            Ok(Err(e @ HostError::Refused(_))) => {
                return Err(format!("the daemon refused the status request: {e}"))
            }
            Ok(Ok(answer)) => answer,
        };
        parse_status(&answer).map(Arc::new)
    }
}

/// `inner`, asked at most once per `ttl`.
pub struct Cached<S> {
    inner: S,
    ttl: Duration,
    last: tokio::sync::Mutex<Option<(Instant, Apps)>>,
}

impl<S: StatusSource> Cached<S> {
    pub fn new(inner: S, ttl: Duration) -> Self {
        Cached {
            inner,
            ttl,
            last: tokio::sync::Mutex::new(None),
        }
    }
}

#[async_trait]
impl<S: StatusSource> StatusSource for Cached<S> {
    async fn apps(&self) -> Apps {
        // the lock is held while the daemon is asked: scrapes that arrive meanwhile wait for the
        // same answer instead of asking again
        let mut last = self.last.lock().await;
        if let Some((at, apps)) = last.as_ref() {
            if at.elapsed() < self.ttl {
                return apps.clone();
            }
        }
        let fresh = self.inner.apps().await;
        *last = Some((Instant::now(), fresh.clone()));
        fresh
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A source that counts its questions and answers from a script.
    pub struct Fake {
        pub asked: AtomicUsize,
        pub answer: std::sync::Mutex<Apps>,
        pub delay: Duration,
    }

    impl Fake {
        pub fn new(answer: Apps) -> Fake {
            Fake {
                asked: AtomicUsize::new(0),
                answer: std::sync::Mutex::new(answer),
                delay: Duration::ZERO,
            }
        }
    }

    #[async_trait]
    impl StatusSource for Fake {
        async fn apps(&self) -> Apps {
            self.asked.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            self.answer.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl StatusSource for Arc<Fake> {
        async fn apps(&self) -> Apps {
            (**self).apps().await
        }
    }

    #[tokio::test]
    async fn concurrent_scrapes_share_one_question() {
        let mut fake = Fake::new(Ok(Arc::new(vec![])));
        fake.delay = Duration::from_millis(100);
        let fake = Arc::new(fake);
        let cached = Arc::new(Cached::new(Arc::clone(&fake), Duration::from_secs(60)));
        let tasks: Vec<_> = (0..40)
            .map(|_| {
                tokio::spawn({
                    let c = Arc::clone(&cached);
                    async move { c.apps().await }
                })
            })
            .collect();
        for t in tasks {
            assert!(t.await.unwrap().is_ok());
        }
        assert_eq!(fake.asked.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_answer_is_asked_for_again_after_the_interval() {
        let fake = Arc::new(Fake::new(Ok(Arc::new(vec![]))));
        let cached = Cached::new(Arc::clone(&fake), Duration::from_millis(60));
        cached.apps().await.unwrap();
        cached.apps().await.unwrap();
        assert_eq!(fake.asked.load(Ordering::SeqCst), 1);
        tokio::time::sleep(Duration::from_millis(90)).await;
        cached.apps().await.unwrap();
        assert_eq!(fake.asked.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_failure_is_reused_for_the_interval_and_then_recovers() {
        let fake = Arc::new(Fake::new(Err("down".into())));
        let cached = Cached::new(Arc::clone(&fake), Duration::from_millis(60));
        assert_eq!(cached.apps().await.unwrap_err(), "down");
        assert_eq!(cached.apps().await.unwrap_err(), "down");
        assert_eq!(
            fake.asked.load(Ordering::SeqCst),
            1,
            "a dead daemon is not asked again for every scrape"
        );
        *fake.answer.lock().unwrap() = Ok(Arc::new(vec![]));
        tokio::time::sleep(Duration::from_millis(90)).await;
        assert!(cached.apps().await.is_ok());
    }
}
