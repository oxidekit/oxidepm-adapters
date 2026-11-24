//! The HTTP side: `GET /metrics`, and nothing else.
//!
//! * `/metrics` is the only path; everything else is 404, `/` included.
//! * With a key configured, every request needs `Authorization: Bearer <key>` (401 otherwise).
//!   The configuration insists on a key for any address that is not loopback.
//! * Without a key (loopback only) the `Host` header must name a loopback address or `localhost`
//!   with the port that was bound: a web page whose name was re-pointed at 127.0.0.1 (DNS
//!   rebinding) sends its own name and is refused.
//! * If the daemon cannot be asked the answer is 503 with `oxidepm_up 0`.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hyper::header::{
    HeaderValue, ALLOW, AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, HOST, WWW_AUTHENTICATE,
};
use hyper::server::conn::Http;
use hyper::{Body, Method, Request, Response, StatusCode};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

use crate::render::{render, render_down};
use crate::source::StatusSource;

/// Connections served at once; the next one is closed at once.
pub const MAX_CONNECTIONS: usize = 32;
/// The most a connection lives (reading a request, waiting for the daemon, writing).
pub const CONNECTION_LIMIT: Duration = Duration::from_secs(15);
/// The most a request line and its headers may take.
const MAX_HEAD: usize = 16 * 1024;

pub struct State {
    pub source: Box<dyn StatusSource>,
    pub key: Option<String>,
    pub bound: SocketAddr,
}

/// Equal strings, without stopping at the first difference.
fn same(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}

fn reply(status: StatusCode, content_type: &'static str, body: String) -> Response<Body> {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}

fn plain(status: StatusCode, text: &str) -> Response<Body> {
    reply(status, "text/plain; charset=utf-8", format!("{text}\n"))
}

/// Is `host` (a `Host` header value) one a local scraper would use for the port `port`?
fn is_local_host(host: &str, port: u16) -> bool {
    let (name, given) = match host.rsplit_once(':') {
        Some((name, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
            (name, p.parse::<u32>().ok())
        }
        _ => (host, None),
    };
    if given.is_some_and(|p| p != u32::from(port)) {
        return false;
    }
    let name = name.trim_start_matches('[').trim_end_matches(']');
    name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

pub async fn handle(state: &State, request: Request<Body>) -> Response<Body> {
    match &state.key {
        Some(key) => {
            let given = request
                .headers()
                .get(AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "));
            if !given.is_some_and(|given| same(given.as_bytes(), key.as_bytes())) {
                let mut response = plain(StatusCode::UNAUTHORIZED, "a bearer key is required");
                response
                    .headers_mut()
                    .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
                return response;
            }
        }
        None => {
            let host = request.headers().get(HOST);
            // an HTTP/1.0 client may send no Host; a browser always does
            if let Some(host) = host {
                if !host
                    .to_str()
                    .is_ok_and(|h| is_local_host(h, state.bound.port()))
                {
                    return plain(
                        StatusCode::FORBIDDEN,
                        "this address answers to localhost only",
                    );
                }
            }
        }
    }
    if request.uri().path() != "/metrics" {
        return plain(StatusCode::NOT_FOUND, "not found");
    }
    if request.method() != Method::GET && request.method() != Method::HEAD {
        let mut response = plain(StatusCode::METHOD_NOT_ALLOWED, "GET only");
        response
            .headers_mut()
            .insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
        return response;
    }
    const TEXT: &str = "text/plain; version=0.0.4; charset=utf-8";
    match state.source.apps().await {
        Ok(apps) => reply(StatusCode::OK, TEXT, render(&apps)),
        Err(why) => {
            eprintln!("exporter-prometheus: a scrape got 503: {why}");
            reply(StatusCode::SERVICE_UNAVAILABLE, TEXT, render_down())
        }
    }
}

/// Serve `listener` until the task is dropped.
pub async fn serve(listener: TcpListener, state: Arc<State>) {
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(_) => {
                // out of descriptors or a reset while accepting: do not spin
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
            drop(stream);
            continue;
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |request| {
                let state = Arc::clone(&state);
                async move { Ok::<_, Infallible>(handle(&state, request).await) }
            });
            let mut http = Http::new();
            http.http1_only(true)
                .http1_keep_alive(false)
                .max_buf_size(MAX_HEAD);
            let _ = tokio::time::timeout(CONNECTION_LIMIT, http.serve_connection(stream, service))
                .await;
            drop(slot);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::App;
    use crate::source::tests::Fake;
    use std::sync::atomic::Ordering;

    const KEY: &str = "0123456789abcdef-key";

    fn app() -> App {
        App {
            name: "web".into(),
            id: 1,
            mode: "cmd".into(),
            up: true,
            uptime_secs: 5,
            restarts: 0,
            memory_bytes: 10,
            cpu_percent: 0.0,
        }
    }

    fn state(answer: crate::source::Apps, key: Option<&str>) -> State {
        State {
            source: Box::new(Fake::new(answer)),
            key: key.map(str::to_string),
            bound: "127.0.0.1:9616".parse().unwrap(),
        }
    }

    fn get(path: &str, headers: &[(&str, &str)]) -> Request<Body> {
        let mut b = Request::builder().method("GET").uri(path);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(Body::empty()).unwrap()
    }

    async fn text(response: Response<Body>) -> (u16, String) {
        let status = response.status().as_u16();
        let bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn metrics_are_served_in_the_text_format() {
        let s = state(Ok(Arc::new(vec![app()])), None);
        let response = handle(&s, get("/metrics", &[("host", "127.0.0.1:9616")])).await;
        assert_eq!(
            response.headers()[CONTENT_TYPE],
            "text/plain; version=0.0.4; charset=utf-8"
        );
        let (code, body) = text(response).await;
        assert_eq!(code, 200);
        assert!(
            body.contains("oxidepm_up 1\n") && body.contains("oxidepm_process_up{name=\"web\""),
            "{body}"
        );
    }

    #[tokio::test]
    async fn a_daemon_that_cannot_be_asked_is_503_with_up_0() {
        let s = state(Err("the daemon is gone".into()), None);
        let (code, body) =
            text(handle(&s, get("/metrics", &[("host", "localhost:9616")])).await).await;
        assert_eq!(code, 503);
        assert!(body.contains("oxidepm_up 0\n"), "{body}");
        assert!(
            !body.contains("oxidepm_process"),
            "no list that looks healthy: {body}"
        );
    }

    #[tokio::test]
    async fn a_key_is_required_when_one_is_configured() {
        let s = state(Ok(Arc::new(vec![app()])), Some(KEY));
        for headers in [
            vec![],
            vec![("authorization", "Bearer wrong-key-wrong-key")],
            vec![("authorization", KEY)],
            vec![("authorization", "Basic abc")],
            vec![("authorization", "Bearer ")],
        ] {
            let response = handle(&s, get("/metrics", &headers)).await;
            assert_eq!(response.status(), 401, "{headers:?}");
            assert_eq!(response.headers()[WWW_AUTHENTICATE], "Bearer");
        }
        let ok = handle(
            &s,
            get("/metrics", &[("authorization", &format!("Bearer {KEY}"))]),
        )
        .await;
        assert_eq!(ok.status(), 200);
        // a key is checked before the path: nobody learns what exists without it
        assert_eq!(handle(&s, get("/other", &[])).await.status(), 401);
        assert_eq!(
            handle(
                &s,
                get("/other", &[("authorization", &format!("Bearer {KEY}"))])
            )
            .await
            .status(),
            404
        );
    }

    #[tokio::test]
    async fn the_key_comparison_looks_at_every_byte() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"abcd"));
        assert!(!same(b"", b"a"));
        assert!(same(b"", b""));
    }

    #[tokio::test]
    async fn without_a_key_a_rebound_host_name_is_refused() {
        let s = state(Ok(Arc::new(vec![app()])), None);
        for host in [
            "evil.example",
            "evil.example:9616",
            "127.0.0.1.evil.example:9616",
            "127.0.0.1:1234",
            "localhost:80",
            "10.0.0.5:9616",
            "localhost.evil.example",
        ] {
            assert_eq!(
                handle(&s, get("/metrics", &[("host", host)]))
                    .await
                    .status(),
                403,
                "{host}"
            );
        }
        for host in [
            "127.0.0.1:9616",
            "127.0.0.1",
            "localhost:9616",
            "LOCALHOST:9616",
            "[::1]:9616",
            "127.0.0.9:9616",
        ] {
            assert_eq!(
                handle(&s, get("/metrics", &[("host", host)]))
                    .await
                    .status(),
                200,
                "{host}"
            );
        }
    }

    #[tokio::test]
    async fn only_metrics_exists_and_only_get_and_head() {
        let s = state(Ok(Arc::new(vec![])), None);
        for path in [
            "/",
            "/api/health",
            "/metrics/",
            "/metricsx",
            "/api/processes",
        ] {
            assert_eq!(
                handle(&s, get(path, &[("host", "localhost")]))
                    .await
                    .status(),
                404,
                "{path}"
            );
        }
        let post = Request::builder()
            .method("POST")
            .uri("/metrics")
            .header("host", "localhost:9616")
            .body(Body::empty())
            .unwrap();
        let response = handle(&s, post).await;
        assert_eq!(response.status(), 405);
        assert_eq!(response.headers()[ALLOW], "GET, HEAD");
        // the query string is not part of the path
        assert_eq!(
            handle(&s, get("/metrics?x=1", &[("host", "localhost")]))
                .await
                .status(),
            200
        );
    }

    #[tokio::test]
    async fn a_wrong_request_does_not_ask_the_daemon() {
        let fake = Arc::new(Fake::new(Ok(Arc::new(vec![]))));
        let s = State {
            source: Box::new(Arc::clone(&fake)),
            key: Some(KEY.into()),
            bound: "0.0.0.0:9616".parse().unwrap(),
        };
        handle(&s, get("/metrics", &[])).await;
        handle(
            &s,
            get("/nope", &[("authorization", &format!("Bearer {KEY}"))]),
        )
        .await;
        assert_eq!(fake.asked.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_listener_serves_real_connections_and_closes_after_one_answer() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let s = Arc::new(State {
            source: Box::new(Fake::new(Ok(Arc::new(vec![app()])))),
            key: None,
            bound: listener.local_addr().unwrap(),
        });
        let task = tokio::spawn(serve(listener, s));
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        stream
            .write_all(
                format!("GET /metrics HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes(),
            )
            .await
            .unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).await.unwrap();
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        assert!(answer.contains("oxidepm_up 1"));
        task.abort();
    }

    #[tokio::test]
    async fn a_head_that_is_too_large_is_refused_not_buffered() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let s = Arc::new(State {
            source: Box::new(Fake::new(Ok(Arc::new(vec![])))),
            key: None,
            bound: listener.local_addr().unwrap(),
        });
        let task = tokio::spawn(serve(listener, s));
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let junk = "a".repeat(64 * 1024);
        let _ = stream
            .write_all(
                format!("GET /metrics HTTP/1.1\r\nHost: localhost\r\nX-Junk: {junk}\r\n\r\n")
                    .as_bytes(),
            )
            .await;
        let mut answer = String::new();
        let _ = stream.read_to_string(&mut answer).await;
        assert!(!answer.starts_with("HTTP/1.1 200"), "{answer}");
        task.abort();
    }
}
