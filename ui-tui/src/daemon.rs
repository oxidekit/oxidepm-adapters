//! The way to the daemon: `host.request`, as a trait so the dashboard can be tested without one.

use std::time::Duration;

use async_trait::async_trait;
use oxidepm_adapter_sdk::{Host, HostError};
use serde_json::Value;

/// The most a request waits for the daemon.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a request got no response from the daemon. (A response of type `error` is an answer, not
/// this: it is the daemon refusing the request, and the caller reads it.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fail(pub String);

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
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
            Err(_) => Err(Fail("the daemon did not answer in time".to_string())),
            Ok(Ok(response)) => Ok(response),
            Ok(Err(HostError::Gone)) => Err(Fail("the daemon is gone".to_string())),
            Ok(Err(HostError::Refused(e))) => Err(Fail(match e.name() {
                Some(name) => format!("{name}: {}", e.message),
                None => e.message,
            })),
        }
    }
}
