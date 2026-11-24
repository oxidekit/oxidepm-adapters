//! Who may talk to the API: the `Host` and `Origin` rules and the key. All pure functions of the
//! request's headers, so each rule is a unit test.
//!
//! * The key is needed on every request except `/api/health`, whatever address is bound. Without
//!   it, any web page the person visits could send no-preflight POST and DELETE requests to
//!   `http://127.0.0.1:<port>`.
//! * `Host` must be the bound address (or `localhost` on a loopback bind, or a name in
//!   `allowed_hosts`). A page whose name was re-pointed at 127.0.0.1 (DNS rebinding) sends its own
//!   name there and is refused, and so is a request with no `Host` at all.
//! * `Origin` is checked on every request that changes something and on the WebSocket upgrade
//!   (CORS does not cover an upgrade): it must be absent or the one configured origin.
//! * A WebSocket from a browser cannot set a header, so it may carry the key as a subprotocol
//!   (`oxidepm-key.<key>`), never in the URL.

use std::net::{IpAddr, SocketAddr};

use axum::http::header::{HeaderMap, HOST, ORIGIN, SEC_WEBSOCKET_PROTOCOL};
use axum::http::Method;

/// The subprotocol the server selects for the event stream.
pub const WS_PROTOCOL: &str = "oxidepm.v1";
/// The prefix of the subprotocol that carries the key.
pub const WS_KEY_PREFIX: &str = "oxidepm-key.";

#[derive(Debug, Clone)]
pub struct Rules {
    bound: SocketAddr,
    origin: Option<String>,
    allowed_hosts: Vec<String>,
}

/// Why a request was turned away before any handler ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// 403: the `Host` header is not one this server answers to.
    Host,
    /// 403: the `Origin` header is not the configured one.
    Origin,
    /// 401: no key, or a wrong one.
    Key,
}

impl Rules {
    pub fn new(bound: SocketAddr, origin: Option<String>, allowed_hosts: Vec<String>) -> Rules {
        Rules {
            bound,
            origin,
            allowed_hosts,
        }
    }

    /// May a request with this `Host` header (`None` when it has none) be answered?
    pub fn host_ok(&self, host: Option<&str>) -> bool {
        let Some(host) = host else { return false };
        let host = host.to_ascii_lowercase();
        if self.allowed_hosts.contains(&host) {
            return true;
        }
        let (name, port) = match host.rsplit_once(':') {
            Some((name, port))
                if !host.ends_with(']')
                    && !port.is_empty()
                    && port.bytes().all(|b| b.is_ascii_digit()) =>
            {
                (name, port.parse::<u32>().ok())
            }
            _ => (host.as_str(), None),
        };
        // the port must be given and be the bound one: a browser leaves out only the default port of a scheme
        if port != Some(u32::from(self.bound.port())) {
            return false;
        }
        let name = name
            .strip_prefix('[')
            .and_then(|n| n.strip_suffix(']'))
            .unwrap_or(name);
        if name == "localhost" {
            return self.bound.ip().is_loopback();
        }
        match name.parse::<IpAddr>() {
            Ok(ip) => self.bound.ip().is_unspecified() || ip == self.bound.ip(),
            Err(_) => false,
        }
    }

    /// May a request with this `Origin` header go on? An absent header is fine.
    pub fn origin_ok(&self, origin: Option<&str>) -> bool {
        match (origin, &self.origin) {
            (None, _) => true,
            (Some(given), Some(allowed)) => given.eq_ignore_ascii_case(allowed),
            (Some(_), None) => false,
        }
    }

    /// Is this the configured origin? (For the CORS answer headers.)
    pub fn is_configured_origin(&self, origin: &str) -> bool {
        self.origin
            .as_deref()
            .is_some_and(|o| o.eq_ignore_ascii_case(origin))
    }
}

/// Does this request change something, or open a stream? Those need the `Origin` check.
pub fn needs_origin_check(method: &Method, headers: &HeaderMap) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) || is_upgrade(headers)
}

pub fn is_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .any(|t| t.trim().eq_ignore_ascii_case("websocket"))
        })
}

/// Equal strings, without stopping at the first difference.
pub fn same(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}

/// The key a request carries: the `X-API-Key` header, or, on a WebSocket upgrade only, a
/// `oxidepm-key.<key>` subprotocol. Never the URL.
pub fn key_of(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        return Some(value.to_string());
    }
    if is_upgrade(headers) {
        return protocols(headers)
            .into_iter()
            .find_map(|p| p.strip_prefix(WS_KEY_PREFIX).map(str::to_string));
    }
    None
}

/// The subprotocols offered, in order.
pub fn protocols(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// The whole decision, in the order Host, Origin, key. `key_needed` is false for the health route.
pub fn check(
    rules: &Rules,
    method: &Method,
    headers: &HeaderMap,
    key: &str,
    key_needed: bool,
) -> Result<(), Refusal> {
    let host = headers.get(HOST).and_then(|v| v.to_str().ok());
    if !rules.host_ok(host) {
        return Err(Refusal::Host);
    }
    if needs_origin_check(method, headers) {
        let origin = headers
            .get(ORIGIN)
            .map(|v| v.to_str().unwrap_or("\u{0}invalid"));
        if !rules.origin_ok(origin) {
            return Err(Refusal::Origin);
        }
    }
    if key_needed {
        match key_of(headers) {
            Some(given) if same(given.as_bytes(), key.as_bytes()) => {}
            _ => return Err(Refusal::Key),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    const KEY: &str = "0123456789abcdef-0123456789abcdef";

    fn rules(bound: &str, origin: Option<&str>, hosts: &[&str]) -> Rules {
        Rules::new(
            bound.parse().unwrap(),
            origin.map(str::to_string),
            hosts.iter().map(|h| h.to_string()).collect(),
        )
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.append(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        map
    }

    #[test]
    fn loopback_bind_answers_to_its_address_and_localhost_with_the_port() {
        let r = rules("127.0.0.1:9615", None, &[]);
        for ok in ["127.0.0.1:9615", "localhost:9615", "LOCALHOST:9615"] {
            assert!(r.host_ok(Some(ok)), "{ok}");
        }
        for bad in [
            "127.0.0.1",
            "localhost",
            "127.0.0.1:9616",
            "127.0.0.2:9615",
            "evil.example:9615",
            "evil.example",
            "[::1]:9615",
            "localhost.evil.example:9615",
            "127.0.0.1:9615.evil.example",
            "",
        ] {
            assert!(!r.host_ok(Some(bad)), "{bad}");
        }
        assert!(!r.host_ok(None), "no Host header");
    }

    #[test]
    fn ipv6_loopback_bind() {
        let r = rules("[::1]:9615", None, &[]);
        assert!(r.host_ok(Some("[::1]:9615")));
        assert!(r.host_ok(Some("localhost:9615")));
        assert!(!r.host_ok(Some("[::2]:9615")));
        assert!(!r.host_ok(Some("::1")));
    }

    #[test]
    fn a_public_bind_does_not_answer_to_localhost_and_needs_allowed_names() {
        let r = rules(
            "192.168.1.5:9615",
            None,
            &["pm.example.org", "pm.example.org:8443"],
        );
        assert!(r.host_ok(Some("192.168.1.5:9615")));
        assert!(r.host_ok(Some("pm.example.org")));
        assert!(r.host_ok(Some("PM.example.org:8443")));
        assert!(!r.host_ok(Some("localhost:9615")));
        assert!(!r.host_ok(Some("other.example.org")));
        assert!(!r.host_ok(Some("192.168.1.6:9615")));
    }

    #[test]
    fn an_unspecified_bind_answers_to_any_address_literal_with_its_port_only() {
        let r = rules("0.0.0.0:9615", None, &[]);
        assert!(r.host_ok(Some("10.1.2.3:9615")));
        assert!(r.host_ok(Some("[fe80::1]:9615")));
        assert!(!r.host_ok(Some("10.1.2.3:80")));
        assert!(
            !r.host_ok(Some("rebound.example:9615")),
            "a name is never an address literal"
        );
        assert!(!r.host_ok(Some("localhost:9615")), "not loopback-bound");
    }

    #[test]
    fn origin_absent_or_the_configured_one() {
        let none = rules("127.0.0.1:9615", None, &[]);
        assert!(none.origin_ok(None));
        assert!(!none.origin_ok(Some("http://localhost:3000")));
        assert!(!none.origin_ok(Some("null")));
        let some = rules("127.0.0.1:9615", Some("http://localhost:3000"), &[]);
        assert!(some.origin_ok(None));
        assert!(some.origin_ok(Some("http://localhost:3000")));
        assert!(!some.origin_ok(Some("http://localhost:3001")));
        assert!(!some.origin_ok(Some("http://evil.example")));
        assert!(!some.origin_ok(Some("null")));
    }

    #[test]
    fn the_origin_check_covers_everything_that_changes_state_and_the_upgrade() {
        let plain = headers(&[]);
        for m in [Method::POST, Method::DELETE, Method::PUT, Method::PATCH] {
            assert!(needs_origin_check(&m, &plain), "{m}");
        }
        for m in [Method::GET, Method::HEAD, Method::OPTIONS] {
            assert!(!needs_origin_check(&m, &plain), "{m}");
        }
        assert!(needs_origin_check(
            &Method::GET,
            &headers(&[("upgrade", "websocket")])
        ));
        assert!(needs_origin_check(
            &Method::GET,
            &headers(&[("upgrade", "h2c, WebSocket")])
        ));
    }

    #[test]
    fn the_key_comes_from_the_header_or_a_subprotocol_never_the_url() {
        assert_eq!(
            key_of(&headers(&[("x-api-key", KEY)])).as_deref(),
            Some(KEY)
        );
        assert_eq!(key_of(&headers(&[])), None);
        // a subprotocol counts only on an upgrade
        let offered = [("sec-websocket-protocol", "oxidepm.v1, oxidepm-key.abc")];
        assert_eq!(key_of(&headers(&offered)), None);
        let upgrade = [
            ("upgrade", "websocket"),
            ("sec-websocket-protocol", "oxidepm.v1, oxidepm-key.abc"),
        ];
        assert_eq!(key_of(&headers(&upgrade)).as_deref(), Some("abc"));
        // offered in two header lines
        let two = [
            ("upgrade", "websocket"),
            ("sec-websocket-protocol", "oxidepm.v1"),
            ("sec-websocket-protocol", "oxidepm-key.def"),
        ];
        assert_eq!(key_of(&headers(&two)).as_deref(), Some("def"));
    }

    #[test]
    fn same_compares_whole_strings() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"abcd"));
        assert!(!same(b"", b"a"));
        assert!(same(b"", b""));
    }

    #[test]
    fn check_runs_host_then_origin_then_key() {
        let r = rules("127.0.0.1:9615", None, &[]);
        let good = [("host", "127.0.0.1:9615"), ("x-api-key", KEY)];
        assert_eq!(check(&r, &Method::GET, &headers(&good), KEY, true), Ok(()));
        // no key, loopback or not
        assert_eq!(
            check(
                &r,
                &Method::GET,
                &headers(&[("host", "127.0.0.1:9615")]),
                KEY,
                true
            ),
            Err(Refusal::Key)
        );
        assert_eq!(
            check(
                &r,
                &Method::GET,
                &headers(&[("host", "127.0.0.1:9615"), ("x-api-key", "wrong")]),
                KEY,
                true
            ),
            Err(Refusal::Key)
        );
        // the health route needs none
        assert_eq!(
            check(
                &r,
                &Method::GET,
                &headers(&[("host", "127.0.0.1:9615")]),
                KEY,
                false
            ),
            Ok(())
        );
        // a rebound name is refused even with the right key, and before anything else
        let rebound = [
            ("host", "evil.example:9615"),
            ("x-api-key", KEY),
            ("origin", "http://evil.example:9615"),
        ];
        assert_eq!(
            check(&r, &Method::POST, &headers(&rebound), KEY, true),
            Err(Refusal::Host)
        );
        // a foreign origin on a POST, even with the right key
        let foreign = [
            ("host", "127.0.0.1:9615"),
            ("x-api-key", KEY),
            ("origin", "http://evil.example"),
        ];
        assert_eq!(
            check(&r, &Method::POST, &headers(&foreign), KEY, true),
            Err(Refusal::Origin)
        );
        assert_eq!(
            check(&r, &Method::DELETE, &headers(&foreign), KEY, true),
            Err(Refusal::Origin)
        );
        // the same on a GET is not an origin question (the reader gets no CORS header)
        assert_eq!(
            check(&r, &Method::GET, &headers(&foreign), KEY, true),
            Ok(())
        );
        // but the upgrade is
        let ws = [
            ("host", "127.0.0.1:9615"),
            ("x-api-key", KEY),
            ("origin", "http://evil.example"),
            ("upgrade", "websocket"),
        ];
        assert_eq!(
            check(&r, &Method::GET, &headers(&ws), KEY, true),
            Err(Refusal::Origin)
        );
    }
}
