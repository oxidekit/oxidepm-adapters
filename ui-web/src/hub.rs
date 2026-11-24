//! The daemon's lifecycle events, fanned out to the WebSocket clients.
//!
//! The adapter subscribes to the `lifecycle` family, so what a client sees is what the daemon
//! emitted (a stop from the CLI, a crash, a restart by the supervisor), not only what a handler of
//! this adapter did. Each event is turned into its JSON text once. A client that reads too slowly
//! loses the oldest events and is told how many (`lagged`); it never holds the others back.

use std::sync::Arc;

use oxidepm_adapter_proto::messages::EventParams;
use serde_json::{json, Value};
use tokio::sync::broadcast;

/// Events kept for a slow client before it starts to lose them.
const CAPACITY: usize = 256;

#[derive(Clone)]
pub struct Hub {
    tx: broadcast::Sender<Arc<str>>,
}

impl Hub {
    pub fn new() -> Hub {
        Hub {
            tx: broadcast::channel(CAPACITY).0,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<str>> {
        self.tx.subscribe()
    }

    pub fn clients(&self) -> usize {
        self.tx.receiver_count()
    }

    /// Hand an event to every client. Never waits; with no client it does almost nothing.
    pub fn publish(&self, event: &EventParams) {
        if self.tx.receiver_count() == 0 {
            return;
        }
        let _ = self.tx.send(Arc::from(event_json(event).to_string()));
    }
}

impl Default for Hub {
    fn default() -> Self {
        Hub::new()
    }
}

/// The message a client reads. `options` (an app's own settings for this adapter) are not passed on.
pub fn event_json(event: &EventParams) -> Value {
    let mut message = json!({
        "type": "event",
        "seq": event.seq,
        "family": event.family,
        "kind": event.kind,
        "at": event.at,
        "data": event.data,
    });
    if let Some(app) = &event.app {
        message["app"] = json!({ "id": app.id, "name": app.name, "tags": app.tags });
    }
    if let Some(dropped) = event.dropped_before {
        message["dropped_before"] = json!(dropped);
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_adapter_proto::messages::EventApp;

    fn event(kind: &str) -> EventParams {
        EventParams {
            seq: 7,
            kind: kind.to_string(),
            family: "lifecycle".to_string(),
            at: "2026-10-30T10:00:00Z".to_string(),
            app: Some(EventApp {
                id: 3,
                name: "api".to_string(),
                tags: vec!["web".to_string()],
            }),
            data: serde_json::from_value(json!({ "pid": 4242, "reason": "x" })).unwrap(),
            options: Some(serde_json::from_value(json!({ "secret": "s" })).unwrap()),
            dropped_before: Some(2),
        }
    }

    #[test]
    fn an_event_becomes_one_message_without_the_apps_options() {
        let message = event_json(&event("stopped"));
        assert_eq!(message["type"], "event");
        assert_eq!(message["kind"], "stopped");
        assert_eq!(message["seq"], 7);
        assert_eq!(message["app"]["name"], "api");
        assert_eq!(message["app"]["id"], 3);
        assert_eq!(message["data"]["pid"], 4242);
        assert_eq!(message["dropped_before"], 2);
        assert!(!message.to_string().contains("secret"));
    }

    #[tokio::test]
    async fn clients_get_the_events_and_a_slow_one_lags_instead_of_blocking() {
        let hub = Hub::new();
        hub.publish(&event("started")); // nobody listens: nothing is kept
        let mut fast = hub.subscribe();
        let mut slow = hub.subscribe();
        assert_eq!(hub.clients(), 2);
        for _ in 0..(CAPACITY + 10) {
            hub.publish(&event("restarted"));
            let m = fast.recv().await.unwrap();
            assert!(m.contains("restarted"));
        }
        assert!(
            matches!(slow.recv().await, Err(broadcast::error::RecvError::Lagged(n)) if n >= 10)
        );
        assert!(
            slow.recv().await.is_ok(),
            "after the lag the newest events are still there"
        );
    }
}
