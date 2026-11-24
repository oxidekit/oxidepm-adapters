//! The four channels. Every error that leaves this module is a short sentence made here: it never
//! contains a URL, a token, a response body or the text of the HTTP library's error, so it can be
//! logged, shown to the person and kept in `adapter info` without leaking a secret.

use std::time::Duration;

use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;

use crate::config::{Config, Telegram};
use crate::event::Alert;

/// The whole request, unless `timeout_secs` says otherwise. A channel that takes longer is
/// reported as timed out.
pub const DEFAULT_TIMEOUT_SECS: u64 = 10;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The most that one channel may take, the plain-text retry of Telegram included.
pub fn budget(timeout: Duration) -> Duration {
    timeout * 2 + Duration::from_secs(1)
}

const DEFAULT_TELEGRAM_BASE: &str = "https://api.telegram.org";

pub fn client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT.min(timeout))
        .timeout(timeout)
        // a redirect would send the secret path to a host the person did not name
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("oxidepm-notifier/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("an HTTP client with these settings")
}

/// One configured channel.
#[derive(Debug, Clone)]
pub enum Channel {
    Telegram {
        token: Option<String>,
        chat_id: String,
        base: String,
    },
    Discord {
        url: String,
    },
    Slack {
        url: String,
    },
    Webhook {
        url: String,
        secret: Option<String>,
    },
}

/// The channels a configuration names, in a fixed order.
pub fn channels(config: &Config) -> Vec<Channel> {
    let mut out = Vec::new();
    if let Some(t) = &config.telegram {
        out.push(telegram(t));
    }
    if let Some(d) = &config.discord {
        out.push(Channel::Discord {
            url: d.webhook_url.clone(),
        });
    }
    if let Some(s) = &config.slack {
        out.push(Channel::Slack {
            url: s.webhook_url.clone(),
        });
    }
    if let Some(w) = &config.webhook {
        out.push(Channel::Webhook {
            url: w.url.clone(),
            secret: w.secret.clone(),
        });
    }
    out
}

fn telegram(t: &Telegram) -> Channel {
    Channel::Telegram {
        token: t.resolve_token(),
        chat_id: t.chat_id.clone(),
        base: t
            .api_base
            .clone()
            .unwrap_or_else(|| DEFAULT_TELEGRAM_BASE.to_string())
            .trim_end_matches('/')
            .to_string(),
    }
}

/// `hooks.slack.com` becomes `*.slack.com`: enough to tell channels apart in a log, not enough to
/// name a private server.
pub fn redact_host(url: &str) -> String {
    let Some(host) = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
    else {
        return "?".to_string();
    };
    let labels: Vec<&str> = host.split('.').collect();
    let is_ip = host.parse::<std::net::IpAddr>().is_ok() || host.contains(':');
    if is_ip || labels.len() < 3 {
        return host;
    }
    format!("*.{}", labels[labels.len() - 2..].join("."))
}

impl Channel {
    pub fn name(&self) -> &'static str {
        match self {
            Channel::Telegram { .. } => "telegram",
            Channel::Discord { .. } => "discord",
            Channel::Slack { .. } => "slack",
            Channel::Webhook { .. } => "webhook",
        }
    }

    pub fn host(&self) -> String {
        match self {
            Channel::Telegram { base, .. } => redact_host(base),
            Channel::Discord { url } | Channel::Slack { url } | Channel::Webhook { url, .. } => {
                redact_host(url)
            }
        }
    }

    /// The secrets of this channel, to be removed from any text before it leaves.
    fn secrets(&self) -> Vec<&str> {
        match self {
            Channel::Telegram { token, .. } => token.iter().map(String::as_str).collect(),
            Channel::Discord { url } | Channel::Slack { url } => vec![url.as_str()],
            Channel::Webhook { url, secret } => std::iter::once(url.as_str())
                .chain(secret.iter().map(String::as_str))
                .collect(),
        }
    }

    /// Send `alert`. The text of an error is already safe to show.
    pub async fn send(&self, http: &reqwest::Client, alert: &Alert) -> Result<(), String> {
        let result = match self {
            Channel::Telegram {
                token,
                chat_id,
                base,
            } => send_telegram(http, token.as_deref(), chat_id, base, alert).await,
            Channel::Discord { url } => {
                post_json(http, url, &json!({ "content": alert.plain }), "Discord").await
            }
            Channel::Slack { url } => {
                post_json(http, url, &json!({ "text": alert.plain }), "Slack").await
            }
            Channel::Webhook { url, secret } => {
                send_webhook(http, url, secret.as_deref(), alert).await
            }
        };
        result.map_err(|e| remove_secrets(&e, &self.secrets()))
    }
}

fn remove_secrets(text: &str, secrets: &[&str]) -> String {
    let mut out = text.to_string();
    for secret in secrets.iter().filter(|s| s.len() >= 4) {
        out = out.replace(secret, "<redacted>");
    }
    out
}

/// What went wrong with a request, without the library's text (which names the URL).
fn describe(e: reqwest::Error) -> String {
    let e = e.without_url();
    if e.is_timeout() {
        "timed out".to_string()
    } else if e.is_connect() {
        "could not connect".to_string()
    } else if e.is_redirect() {
        "was redirected (not followed)".to_string()
    } else if e.is_builder() {
        "the request could not be made".to_string()
    } else if e.is_decode() || e.is_body() {
        "the answer could not be read".to_string()
    } else {
        "the request failed".to_string()
    }
}

fn cut(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if flat.chars().count() <= max {
        flat
    } else {
        format!("{}...", flat.chars().take(max).collect::<String>())
    }
}

async fn post_json(
    http: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
    who: &str,
) -> Result<(), String> {
    let response = http.post(url).json(body).send().await.map_err(describe)?;
    let status = response.status();
    if status.is_success() {
        Ok(())
    } else {
        // the body is not shown: a webhook may echo what it was sent
        Err(format!("{who} answered HTTP {}", status.as_u16()))
    }
}

async fn send_telegram(
    http: &reqwest::Client,
    token: Option<&str>,
    chat_id: &str,
    base: &str,
    alert: &Alert,
) -> Result<(), String> {
    let Some(token) = token else {
        return Err(
            "no bot token: set bot_token_env (the daemon's OXIDEPM_TELEGRAM_TOKEN) or bot_token"
                .to_string(),
        );
    };
    let url = format!("{base}/bot{token}/sendMessage");
    // formatted first; a 400 is Telegram refusing the formatting, and the text goes again as it is
    let formatted = json!({ "chat_id": chat_id, "text": alert.html, "parse_mode": "HTML" });
    match telegram_once(http, &url, &formatted).await {
        Err(Tried::Http(400, _)) => {}
        other => return other.map_err(Tried::text),
    }
    let plain = json!({ "chat_id": chat_id, "text": alert.plain });
    telegram_once(http, &url, &plain).await.map_err(Tried::text)
}

enum Tried {
    Http(u16, String),
    Other(String),
}

impl Tried {
    fn text(self) -> String {
        match self {
            Tried::Http(code, why) if why.is_empty() => format!("Telegram answered HTTP {code}"),
            Tried::Http(code, why) => format!("Telegram answered HTTP {code}: {why}"),
            Tried::Other(why) => why,
        }
    }
}

async fn telegram_once(
    http: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
) -> Result<(), Tried> {
    let response = http
        .post(url)
        .json(body)
        .send()
        .await
        .map_err(|e| Tried::Other(describe(e)))?;
    let status = response.status();
    let reply: Option<serde_json::Value> = response.json().await.ok();
    let ok = reply
        .as_ref()
        .and_then(|r| r.get("ok"))
        .and_then(serde_json::Value::as_bool);
    if status.is_success() && ok != Some(false) {
        return Ok(());
    }
    let description = reply
        .as_ref()
        .and_then(|r| r.get("description"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    Err(Tried::Http(status.as_u16(), cut(description, 200)))
}

/// The one body every webhook gets.
pub fn webhook_body(alert: &Alert, timestamp: &str) -> serde_json::Value {
    json!({
        "version": 1,
        "event": alert.event,
        "severity": alert.severity.as_str(),
        "message": alert.plain,
        "process": alert.name,
        "id": alert.id,
        "timestamp": timestamp,
    })
}

/// `v1=<hex>`: HMAC-SHA256 of `<timestamp>.<body>` with the secret.
pub fn signature(secret: &str, timestamp: &str, body: &[u8]) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes a key of any size");
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    let hex: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("v1={hex}")
}

async fn send_webhook(
    http: &reqwest::Client,
    url: &str,
    secret: Option<&str>,
    alert: &Alert,
) -> Result<(), String> {
    let now = chrono::Utc::now();
    let body = serde_json::to_vec(&webhook_body(
        alert,
        &now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    ))
    .map_err(|_| "the body could not be made".to_string())?;
    let unix = now.timestamp().to_string();
    let mut request = http.post(url).header("Content-Type", "application/json");
    if let Some(secret) = secret {
        request = request
            .header("X-OxidePM-Timestamp", &unix)
            .header("X-OxidePM-Signature", signature(secret, &unix, &body));
    }
    let response = request.body(body).send().await.map_err(describe)?;
    let status = response.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("the webhook answered HTTP {}", status.as_u16()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_redacted_to_their_registrable_part() {
        assert_eq!(
            redact_host("https://hooks.slack.com/services/T0/B0/xxx"),
            "*.slack.com"
        );
        assert_eq!(
            redact_host("https://discord.com/api/webhooks/1/abc"),
            "discord.com"
        );
        assert_eq!(redact_host("http://127.0.0.1:9000/x"), "127.0.0.1");
        assert_eq!(
            redact_host("https://alerts.internal.example.org/hook?token=SECRET"),
            "*.example.org"
        );
        assert_eq!(redact_host("nonsense"), "?");
    }

    #[test]
    fn secrets_are_removed_from_error_text() {
        assert_eq!(
            remove_secrets(
                "bad https://x/hook/SECRETPATH here",
                &["https://x/hook/SECRETPATH"]
            ),
            "bad <redacted> here"
        );
        assert_eq!(
            remove_secrets("token abcd1234 seen", &["abcd1234"]),
            "token <redacted> seen"
        );
        assert_eq!(
            remove_secrets("ab stays", &["ab"]),
            "ab stays",
            "a very short secret is not worth mangling text for"
        );
    }

    #[test]
    fn the_signature_is_hmac_sha256_over_timestamp_dot_body() {
        // RFC 4231 test case 2 style check, computed with the same construction
        let a = signature("key", "1700000000", b"{\"a\":1}");
        let b = signature("key", "1700000000", b"{\"a\":1}");
        assert_eq!(a, b);
        assert!(a.starts_with("v1=") && a.len() == 3 + 64, "{a}");
        assert_ne!(a, signature("other", "1700000000", b"{\"a\":1}"));
        assert_ne!(a, signature("key", "1700000001", b"{\"a\":1}"));
        assert_ne!(a, signature("key", "1700000000", b"{\"a\":2}"));
        // a known value: HMAC-SHA256("key", "1.x") in hex
        assert_eq!(
            signature("key", "1", b"x"),
            format!("v1={}", known("key", "1.x"))
        );
    }

    /// An independent computation of HMAC-SHA256 (RFC 2104) from the hash function alone.
    fn known(key: &str, message: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut k = key.as_bytes().to_vec();
        k.resize(64, 0);
        let inner: Vec<u8> = k.iter().map(|b| b ^ 0x36).chain(message.bytes()).collect();
        let inner_hash = Sha256::digest(&inner);
        let outer: Vec<u8> = k
            .iter()
            .map(|b| b ^ 0x5c)
            .chain(inner_hash.iter().copied())
            .collect();
        Sha256::digest(&outer)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    #[test]
    fn the_webhook_has_one_schema() {
        let alert = Alert::own("test", "hello");
        let body = webhook_body(&alert, "2026-01-01T00:00:00Z");
        let keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            vec![
                "event",
                "id",
                "message",
                "process",
                "severity",
                "timestamp",
                "version"
            ]
        );
    }
}
