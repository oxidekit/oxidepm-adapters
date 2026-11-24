use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use tokio::time::{sleep_until, Instant};

pub struct Debounce {
    delay: Duration,
    deadline: Option<Instant>,
    paths: HashSet<PathBuf>,
}

impl Debounce {
    pub fn new(delay: Duration) -> Self {
        Self {
            delay,
            deadline: None,
            paths: HashSet::new(),
        }
    }

    pub fn record(&mut self, paths: Vec<PathBuf>) {
        if !paths.is_empty() {
            self.paths.extend(paths);
            self.deadline = Some(Instant::now() + self.delay);
        }
    }

    pub async fn ready(&mut self) -> HashSet<PathBuf> {
        let Some(deadline) = self.deadline else {
            return std::future::pending().await;
        };
        sleep_until(deadline).await;
        self.deadline = None;
        std::mem::take(&mut self.paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{advance, timeout};

    #[tokio::test(start_paused = true)]
    async fn trailing_edge_debounce_emits_once_after_burst() {
        let delay = Duration::from_millis(100);
        let mut debounce = Debounce::new(delay);
        for path in ["first", "second", "third"] {
            debounce.record(vec![path.into()]);
            advance(delay / 2).await;
        }
        assert!(timeout(delay / 4, debounce.ready()).await.is_err());
        assert_eq!(
            timeout(delay / 2, debounce.ready()).await.unwrap(),
            ["first", "second", "third"]
                .into_iter()
                .map(PathBuf::from)
                .collect()
        );
        assert!(timeout(delay, debounce.ready()).await.is_err());
    }
}
