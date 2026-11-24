//! The REST routes and the WebSocket. Every handler turns the call into the request the CLI would
//! send, hands it to the daemon (`host.request`, as the actor `adapter:ui-web` with the grant the
//! person gave) and turns the answer into JSON. The daemon decides what is allowed; this adapter
//! decides who may ask (the key, `Host`, `Origin`) and a few things the daemon cannot know.
//!
//! What it does not do: serve `/metrics` (that is the exporter adapter), create apps unless it was
//! given the `admin` level, or delete everything without `confirm`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::header::{
    HeaderValue, CACHE_CONTROL, CONTENT_TYPE, ORIGIN, VARY, WWW_AUTHENTICATE,
};
use axum::http::{Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Router};
use futures::{SinkExt, StreamExt};
use oxidepm_adapter_proto::messages::Grant;
use oxidepm_adapter_proto::HostLevel;
use oxidepm_core::{AppSpec, Runtime, Selector};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tokio::sync::{watch, Semaphore};

use crate::daemon::{Daemon, Fail, Limiter};
use crate::guard::{self, Refusal, Rules, WS_PROTOCOL};
use crate::hub::Hub;

/// The adapter's name, as an app lists it under `adapters`.
pub const ADAPTER: &str = "ui-web";
/// Largest request body.
pub const MAX_BODY: usize = 64 * 1024;
/// Requests being handled at once; the next one gets 503.
pub const MAX_IN_FLIGHT: usize = 64;
/// WebSocket clients at once.
pub const MAX_WS: usize = 16;
/// The most a request (its body, the daemon, the answer) takes.
const REQUEST_LIMIT: Duration = Duration::from_secs(30);
/// Longest selector in a path.
const MAX_SELECTOR: usize = 128;
/// Most lines one logs call returns.
pub const MAX_LOG_LINES: usize = 1000;
pub const DEFAULT_LOG_LINES: usize = 100;
/// Most apps one tag selector reads logs of in one call.
pub const MAX_LOG_APPS: usize = 16;
/// How long a status answer is reused.
const STATUS_TTL: Duration = Duration::from_secs(1);
/// A client that says nothing for this long, ping answers included, is dropped.
const WS_SILENCE: Duration = Duration::from_secs(90);
const WS_PING_EVERY: Duration = Duration::from_secs(30);
const WS_SEND_LIMIT: Duration = Duration::from_secs(10);

/// What a failed call looks like to the client.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: Option<String>,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> ApiError {
        ApiError {
            status,
            code: None,
            message: message.into(),
        }
    }

    pub fn bad(message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::BAD_REQUEST, message)
    }

    fn from_fail(fail: Fail) -> ApiError {
        match fail {
            Fail::Gone => ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "the daemon cannot be reached",
            ),
            Fail::Timeout => ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "the daemon did not answer in time",
            ),
            Fail::Refused { name, message } => {
                let status = name
                    .as_deref()
                    .map_or(StatusCode::SERVICE_UNAVAILABLE, status_of_code);
                ApiError {
                    status,
                    code: name,
                    message,
                }
            }
        }
    }
}

/// The HTTP status of a daemon error code.
pub fn status_of_code(code: &str) -> StatusCode {
    match code {
        "E_POLICY_DENIED" | "E_PROTECTED" => StatusCode::FORBIDDEN,
        "E_APP_NOT_FOUND" => StatusCode::NOT_FOUND,
        "E_APP_EXISTS" => StatusCode::CONFLICT,
        "E_RATE_LIMITED" => StatusCode::TOO_MANY_REQUESTS,
        "E_REQUEST_TOO_LARGE" => StatusCode::PAYLOAD_TOO_LARGE,
        "E_BUSY"
        | "E_SHUTTING_DOWN"
        | "E_AUDIT_UNAVAILABLE"
        | "E_ADAPTER_UNAVAILABLE"
        | "E_ADAPTER_BUSY" => StatusCode::SERVICE_UNAVAILABLE,
        "E_TIMEOUT" | "E_ADAPTER_TIMEOUT" => StatusCode::GATEWAY_TIMEOUT,
        "E_INTERNAL" | "E_DB" | "E_IO" | "E_STATE_INCOMPATIBLE" => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
        _ => StatusCode::BAD_REQUEST,
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body =
            json!({ "success": false, "data": null, "error": self.message, "code": self.code });
        let mut response = (self.status, axum::Json(body)).into_response();
        if self.status == StatusCode::TOO_MANY_REQUESTS {
            response
                .headers_mut()
                .insert("retry-after", HeaderValue::from_static("1"));
        }
        response
    }
}

fn success(data: Value) -> Response {
    axum::Json(json!({ "success": true, "data": data, "error": null })).into_response()
}

fn success_with_note(data: Value, note: &str) -> Response {
    axum::Json(json!({ "success": true, "data": data, "error": null, "note": note }))
        .into_response()
}

/// The last answer to a status request, and when it came.
type CachedApps = (Instant, Result<Arc<Value>, ApiError>);

/// What every handler shares.
pub struct Shared {
    pub daemon: Arc<dyn Daemon>,
    key: watch::Sender<String>,
    pub rules: Rules,
    pub grant: Grant,
    pub hub: Hub,
    limiter: Limiter,
    status: tokio::sync::Mutex<Option<CachedApps>>,
    ws_slots: Arc<Semaphore>,
    in_flight: Arc<Semaphore>,
}

impl Shared {
    pub fn new(
        daemon: Arc<dyn Daemon>,
        key: String,
        rules: Rules,
        grant: Grant,
        hub: Hub,
    ) -> Arc<Shared> {
        Arc::new(Shared {
            daemon,
            key: watch::channel(key).0,
            rules,
            grant,
            hub,
            limiter: Limiter::standard(),
            status: tokio::sync::Mutex::new(None),
            ws_slots: Arc::new(Semaphore::new(MAX_WS)),
            in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
        })
    }

    /// Replace the key and invalidate event streams authenticated with the previous key.
    pub fn set_key(&self, key: String) {
        self.key.send_replace(key);
    }

    /// Ask the daemon, within this adapter's own rate.
    async fn ask(&self, request: Value) -> Result<Value, ApiError> {
        if !self.limiter.take() {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "too many requests; try again in a second",
            ));
        }
        let response = self
            .daemon
            .call(request)
            .await
            .map_err(ApiError::from_fail)?;
        if response["type"] == "error" {
            let code = response["code"].as_str().map(str::to_string);
            let status = code
                .as_deref()
                .map_or(StatusCode::BAD_REQUEST, status_of_code);
            let message = response["message"]
                .as_str()
                .unwrap_or("the daemon refused the request")
                .to_string();
            return Err(ApiError {
                status,
                code,
                message,
            });
        }
        Ok(response)
    }

    /// `ask`, and the response must be of type `expect`.
    async fn ask_for(&self, request: Value, expect: &str) -> Result<Value, ApiError> {
        let response = self.ask(request).await?;
        if response["type"] != expect {
            return Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "the daemon answered with something else than was asked for",
            ));
        }
        Ok(response)
    }

    /// The apps, at most one question a second however many clients ask; a failure is reused too.
    async fn apps(&self) -> Result<Arc<Value>, ApiError> {
        let mut last = self.status.lock().await;
        if let Some((at, answer)) = last.as_ref() {
            if at.elapsed() < STATUS_TTL {
                return answer.clone();
            }
        }
        let answer = self
            .ask_for(json!({ "type": "status" }), "status")
            .await
            .map(|r| Arc::new(r["apps"].clone()));
        *last = Some((Instant::now(), answer.clone()));
        answer
    }
}

// ---------------------------------------------------------------------------------------------
// The router and the guard
// ---------------------------------------------------------------------------------------------

pub fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/ping", get(ping))
        .route("/api/processes", get(list).post(start))
        .route("/api/processes/:selector", get(show).delete(delete))
        .route("/api/processes/:selector/stop", post(stop))
        .route("/api/processes/:selector/restart", post(restart))
        .route("/api/processes/:selector/logs", get(logs))
        .route("/api/save", post(save))
        .route("/api/resurrect", post(resurrect))
        .route("/ws", get(websocket))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(middleware::from_fn_with_state(shared.clone(), guard_layer))
        .with_state(shared)
}

fn refused(status: StatusCode, message: &str) -> Response {
    let mut response = ApiError::new(status, message).into_response();
    if status == StatusCode::UNAUTHORIZED {
        response
            .headers_mut()
            .insert(WWW_AUTHENTICATE, HeaderValue::from_static("X-API-Key"));
    }
    response
}

#[derive(Clone)]
struct StreamAuthorization(watch::Receiver<String>);

/// The checks that come before any handler: `Host`, `Origin`, the key, the in-flight cap, the
/// time limit; and the CORS answer for the one configured origin.
async fn guard_layer(State(sh): State<Arc<Shared>>, mut request: Request, next: Next) -> Response {
    let headers = request.headers();
    let origin = headers
        .get(ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let path = request.uri().path().to_string();

    // a preflight carries no key (the browser never sends credentials in it): answer it for the
    // configured origin and for nobody else
    if request.method() == Method::OPTIONS {
        let host_ok = sh.rules.host_ok(
            headers
                .get(axum::http::header::HOST)
                .and_then(|v| v.to_str().ok()),
        );
        return match origin {
            Some(o) if host_ok && sh.rules.is_configured_origin(&o) => {
                let mut response = StatusCode::NO_CONTENT.into_response();
                cors_headers(&mut response, &o);
                let h = response.headers_mut();
                h.insert(
                    "access-control-allow-methods",
                    HeaderValue::from_static("GET, POST, DELETE, OPTIONS"),
                );
                h.insert(
                    "access-control-allow-headers",
                    HeaderValue::from_static("x-api-key, content-type"),
                );
                h.insert("access-control-max-age", HeaderValue::from_static("600"));
                response
            }
            _ => refused(StatusCode::FORBIDDEN, "not allowed"),
        };
    }

    let mut key = sh.key.subscribe();
    let decision = guard::check(
        &sh.rules,
        request.method(),
        headers,
        &key.borrow_and_update(),
        path != "/api/health",
    );
    if let Err(refusal) = decision {
        return match refusal {
            Refusal::Host => refused(
                StatusCode::FORBIDDEN,
                "this address does not answer to that Host header",
            ),
            Refusal::Origin => refused(
                StatusCode::FORBIDDEN,
                "cross-origin requests are not allowed",
            ),
            Refusal::Key => refused(StatusCode::UNAUTHORIZED, "a valid X-API-Key is required"),
        };
    }

    if path == "/ws" {
        request.extensions_mut().insert(StreamAuthorization(key));
    }
    let Ok(_permit) = sh.in_flight.clone().try_acquire_owned() else {
        return refused(StatusCode::SERVICE_UNAVAILABLE, "too many requests at once");
    };
    let mut response = match tokio::time::timeout(REQUEST_LIMIT, next.run(request)).await {
        Ok(response) => response,
        Err(_) => refused(StatusCode::GATEWAY_TIMEOUT, "the request took too long"),
    };
    if let Some(o) = origin.filter(|o| sh.rules.is_configured_origin(o)) {
        cors_headers(&mut response, &o);
    }
    let h = response.headers_mut();
    h.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    if !h.contains_key(CONTENT_TYPE) {
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    }
    response
}

fn cors_headers(response: &mut Response, origin: &str) {
    if let Ok(value) = HeaderValue::from_str(origin) {
        let h = response.headers_mut();
        h.insert("access-control-allow-origin", value);
        h.append(VARY, HeaderValue::from_static("Origin"));
    }
}

async fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not found")
}

// ---------------------------------------------------------------------------------------------
// Selectors
// ---------------------------------------------------------------------------------------------

/// The selector in a path: an id, a name, `@tag` or `all`.
pub fn parse_selector(raw: &str) -> Result<Selector, ApiError> {
    if raw.is_empty() || raw.chars().count() > MAX_SELECTOR || raw.chars().any(char::is_control) {
        return Err(ApiError::bad(
            "the selector must be an app id, name, @tag or all (at most 128 characters)",
        ));
    }
    Ok(Selector::parse(raw))
}

fn selector_json(selector: &Selector) -> Value {
    serde_json::to_value(selector).unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

async fn health() -> Response {
    axum::Json(json!({ "ok": true })).into_response()
}

async fn ping(State(sh): State<Arc<Shared>>) -> Result<Response, ApiError> {
    sh.ask_for(json!({ "type": "ping" }), "pong").await?;
    Ok(success(json!("pong")))
}

async fn list(State(sh): State<Arc<Shared>>) -> Result<Response, ApiError> {
    let apps = sh.apps().await?;
    Ok(success((*apps).clone()))
}

async fn show(
    State(sh): State<Arc<Shared>>,
    Path(raw): Path<String>,
) -> Result<Response, ApiError> {
    let selector = parse_selector(&raw)?;
    let response = sh
        .ask_for(
            json!({ "type": "show", "selector": selector_json(&selector) }),
            "show",
        )
        .await?;
    Ok(success(response["app"].clone()))
}

async fn stop(
    State(sh): State<Arc<Shared>>,
    Path(raw): Path<String>,
) -> Result<Response, ApiError> {
    let selector = parse_selector(&raw)?;
    let r = sh
        .ask_for(
            json!({ "type": "stop", "selector": selector_json(&selector) }),
            "stopped",
        )
        .await?;
    Ok(success(
        json!({ "stopped": r["count"], "items": r["items"] }),
    ))
}

async fn restart(
    State(sh): State<Arc<Shared>>,
    Path(raw): Path<String>,
) -> Result<Response, ApiError> {
    let selector = parse_selector(&raw)?;
    let r = sh
        .ask_for(
            json!({ "type": "restart", "selector": selector_json(&selector) }),
            "restarted",
        )
        .await?;
    Ok(success(
        json!({ "restarted": r["count"], "items": r["items"] }),
    ))
}

/// `DELETE /api/processes/all` removes every app, so it needs `{"confirm":"all"}` in the body or
/// `?confirm=all`. (At the default level the daemon refuses every delete; this check is for the
/// adapter that was given more.)
async fn delete(
    State(sh): State<Arc<Shared>>,
    Path(raw): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let selector = parse_selector(&raw)?;
    if selector == Selector::All {
        let in_body = serde_json::from_slice::<Value>(&body)
            .ok()
            .is_some_and(|v| v["confirm"] == "all");
        if !in_body && query.get("confirm").map(String::as_str) != Some("all") {
            return Err(ApiError::bad("deleting all apps needs confirmation: send {\"confirm\":\"all\"} or add ?confirm=all"));
        }
    }
    let r = sh
        .ask_for(
            json!({ "type": "delete", "selector": selector_json(&selector) }),
            "deleted",
        )
        .await?;
    Ok(success(
        json!({ "deleted": r["count"], "items": r["items"] }),
    ))
}

async fn save(State(sh): State<Arc<Shared>>) -> Result<Response, ApiError> {
    let r = sh
        .ask_for(json!({ "type": "save", "force": false }), "saved")
        .await?;
    Ok(success(json!({ "saved": r["count"], "path": r["path"] })))
}

async fn resurrect(State(sh): State<Arc<Shared>>) -> Result<Response, ApiError> {
    let r = sh
        .ask_for(json!({ "type": "resurrect" }), "resurrected")
        .await?;
    let failed: Vec<Value> = r["failed"]
        .as_array()
        .map(|a| a.iter().map(|i| i["name"].clone()).collect())
        .unwrap_or_default();
    Ok(success(
        json!({ "resurrected": r["started"], "imported": r["imported"], "failed": failed }),
    ))
}

/// The apps a `@tag` selector means, from the status the daemon gave (ids and names).
fn apps_with_tag(apps: &Value, tag: &str) -> Vec<(u64, String)> {
    apps.as_array()
        .map(|list| {
            list.iter()
                .filter(|app| {
                    app["spec"]["tags"]
                        .as_array()
                        .is_some_and(|tags| tags.iter().any(|t| t == tag))
                })
                .filter_map(|app| {
                    Some((
                        app["spec"]["id"].as_u64()?,
                        app["spec"]["name"].as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `GET /api/processes/:selector/logs?lines=N`. A tag selector returns the lines of every app
/// that has the tag (the daemon alone would read only the first), each line marked with its
/// app, newest last.
async fn logs(
    State(sh): State<Arc<Shared>>,
    Path(raw): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let selector = parse_selector(&raw)?;
    let lines = match query.get("lines") {
        None => DEFAULT_LOG_LINES,
        Some(text) => text
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=MAX_LOG_LINES).contains(n))
            .ok_or_else(|| {
                ApiError::bad(format!("lines must be a number from 1 to {MAX_LOG_LINES}"))
            })?,
    };
    let ask = |selector: Value| json!({ "type": "logs", "selector": selector, "lines": lines, "follow": false, "stdout": true, "stderr": true });
    let lines_of = |r: &Value| -> Vec<String> {
        r["lines"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|l| l.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };

    let Selector::ByTag(tag) = &selector else {
        let r = sh
            .ask_for(ask(selector_json(&selector)), "log_lines")
            .await?;
        return Ok(success(json!(lines_of(&r))));
    };
    let apps = sh.apps().await?;
    let matches = apps_with_tag(&apps, tag);
    if matches.is_empty() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            format!(
                "no app has the tag '{}'",
                tag.chars().take(64).collect::<String>()
            ),
        ));
    }
    let truncated = matches.len() > MAX_LOG_APPS;
    let mut merged: Vec<(String, String)> = Vec::new();
    for (id, name) in matches.into_iter().take(MAX_LOG_APPS) {
        let r = sh
            .ask_for(ask(json!({ "by": "id", "value": id })), "log_lines")
            .await?;
        merged.extend(lines_of(&r).into_iter().map(|line| (line, name.clone())));
    }
    // the lines start with their time, so sorting them interleaves the apps
    merged.sort_by(|a, b| a.0.cmp(&b.0));
    if merged.len() > lines {
        merged.drain(..merged.len() - lines);
    }
    let out: Vec<String> = merged
        .into_iter()
        .map(|(line, name)| format!("[{name}] {line}"))
        .collect();
    if truncated {
        Ok(success_with_note(
            json!(out),
            &format!("only the first {MAX_LOG_APPS} apps with this tag are shown"),
        ))
    } else {
        Ok(success(json!(out)))
    }
}

// ---------------------------------------------------------------------------------------------
// Creating an app
// ---------------------------------------------------------------------------------------------

/// The body of `POST /api/processes`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartBody {
    pub name: String,
    pub command: String,
    /// Must be absolute. A relative one would mean a different place to this adapter than to the daemon.
    pub cwd: String,
    /// The runtime; `cmd` (run the command as it is) when left out. Nothing is guessed from the directory.
    #[serde(default)]
    pub runtime: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
    #[serde(default)]
    pub instances: Option<u32>,
    #[serde(default)]
    pub port: Option<u16>,
}

/// The spec a body asks for, or the reason it cannot be one (never a panic).
pub fn spec_of(body: StartBody) -> Result<AppSpec, ApiError> {
    let runtime = body.runtime.unwrap_or_else(|| "cmd".to_string());
    if !Runtime::is_valid_name(&runtime) {
        return Err(ApiError::bad(format!(
            "runtime '{}' is not a runtime name (2 to 16 lower-case letters and digits)",
            runtime.chars().take(32).collect::<String>()
        )));
    }
    if body.command.trim().is_empty() {
        return Err(ApiError::bad("command must not be empty"));
    }
    if !body.cwd.starts_with('/') {
        return Err(ApiError::bad("cwd must be an absolute path"));
    }
    let runtime: Runtime =
        serde_json::from_value(Value::String(runtime)).map_err(|e| ApiError::bad(e.to_string()))?;
    let mut spec = AppSpec::try_new(body.name, runtime, body.command, body.cwd.into())
        .map_err(|e| ApiError::bad(e.to_string()))?;
    if let Some(args) = body.args {
        spec.args = args;
    }
    if let Some(env) = body.env {
        spec.env = env;
    }
    if let Some(instances) = body.instances {
        spec.instances = instances;
    }
    if let Some(port) = body.port {
        spec.port = Some(port);
    }
    // with the scope `opted-in` the daemon lets an adapter create only apps that list it
    spec.adapters
        .insert(ADAPTER.to_string(), Value::Object(Map::new()));
    Ok(spec)
}

/// `POST /api/processes`: creating an app starts a program of the caller's choosing, so it is
/// refused unless the adapter was given the `admin` level when it was enabled.
async fn start(State(sh): State<Arc<Shared>>, body: Bytes) -> Result<Response, ApiError> {
    if sh.grant.level < HostLevel::Admin {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "creating an app starts a program of your choosing, so this adapter may not do it at its level (lifecycle). \
             Start the app with the oxidepm command line, or enable the adapter with the admin level",
        ));
    }
    let parsed: StartBody = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad(format!("the body is not a valid start request: {e}")))?;
    let spec = spec_of(parsed)?;
    let r = sh
        .ask_for(json!({ "type": "start", "spec": spec }), "started")
        .await?;
    Ok(success(json!({ "id": r["id"], "name": r["name"] })))
}

// ---------------------------------------------------------------------------------------------
// The WebSocket
// ---------------------------------------------------------------------------------------------

fn ignored_text_note(text: &str) -> String {
    format!(
        "ui-web: a WebSocket client sent text ({} bytes) that is ignored",
        text.len()
    )
}

async fn websocket(
    State(sh): State<Arc<Shared>>,
    Extension(StreamAuthorization(mut key)): Extension<StreamAuthorization>,
    ws: WebSocketUpgrade,
) -> Response {
    let Ok(permit) = sh.ws_slots.clone().try_acquire_owned() else {
        return refused(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many event streams are open",
        );
    };
    ws.protocols([WS_PROTOCOL])
        .max_message_size(4096)
        .max_frame_size(4096)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            // Cancelling the stream drops the socket, including any writes waiting to flush.
            tokio::select! {
                biased;
                _ = key.changed() => {}
                _ = session(socket, sh.hub.clone()) => {}
            }
        })
}

async fn send(sender: &mut futures::stream::SplitSink<WebSocket, Message>, text: String) -> bool {
    matches!(
        tokio::time::timeout(WS_SEND_LIMIT, sender.send(Message::Text(text))).await,
        Ok(Ok(()))
    )
}

async fn session(socket: WebSocket, hub: Hub) {
    let (mut sender, mut receiver) = socket.split();
    // subscribe before saying hello: nothing that happens after the hello is missed
    let mut events = hub.subscribe();
    if !send(
        &mut sender,
        json!({ "type": "hello", "protocol": WS_PROTOCOL }).to_string(),
    )
    .await
    {
        return;
    }
    let mut ping = tokio::time::interval(WS_PING_EVERY);
    ping.tick().await;
    let mut last_heard = Instant::now();
    let mut logged = 0;
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(text) => {
                    if !send(&mut sender, text.to_string()).await {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                    if !send(&mut sender, json!({ "type": "lagged", "missed": missed }).to_string()).await {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            incoming = receiver.next() => match incoming {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(Message::Text(text))) => {
                    last_heard = Instant::now();
                    // the stream is one way; what a client sends is noted a few times and ignored
                    if logged < 3 {
                        logged += 1;
                        eprintln!("{}", ignored_text_note(&text));
                    }
                }
                Some(Ok(_)) => last_heard = Instant::now(),
            },
            _ = ping.tick() => {
                if last_heard.elapsed() > WS_SILENCE {
                    break;
                }
                if tokio::time::timeout(WS_SEND_LIMIT, sender.send(Message::Ping(Vec::new()))).await.map_or(true, |r| r.is_err()) {
                    break;
                }
            }
        }
    }
    let _ = tokio::time::timeout(WS_SEND_LIMIT, sender.close()).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use std::sync::Mutex;
    use tower::ServiceExt;

    const KEY: &str = "0123456789abcdef-0123456789abcdef";

    /// A daemon that records what it is asked and answers from a list.
    struct Fake {
        seen: Mutex<Vec<Value>>,
        answers: Mutex<Vec<Result<Value, Fail>>>,
    }

    impl Fake {
        fn new(answers: Vec<Result<Value, Fail>>) -> Arc<Fake> {
            Arc::new(Fake {
                seen: Mutex::new(Vec::new()),
                answers: Mutex::new(answers),
            })
        }

        fn seen(&self) -> Vec<Value> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Daemon for Fake {
        async fn call(&self, request: Value) -> Result<Value, Fail> {
            self.seen.lock().unwrap().push(request);
            let mut answers = self.answers.lock().unwrap();
            if answers.is_empty() {
                Err(Fail::Gone)
            } else {
                answers.remove(0)
            }
        }
    }

    fn app(id: u64, name: &str, tags: &[&str]) -> Value {
        json!({ "spec": { "id": id, "name": name, "tags": tags }, "state": { "status": "running" } })
    }

    fn grant(level: HostLevel) -> Grant {
        Grant {
            level,
            scope: oxidepm_adapter_proto::HostScope::OptedIn,
        }
    }

    fn app_router(daemon: Arc<Fake>, level: HostLevel) -> Router {
        let rules = Rules::new(
            "127.0.0.1:9615".parse().unwrap(),
            Some("http://localhost:3000".to_string()),
            vec![],
        );
        router(Shared::new(
            daemon,
            KEY.to_string(),
            rules,
            grant(level),
            Hub::new(),
        ))
    }

    async fn call(
        router: &Router,
        method: &str,
        path: &str,
        extra: &[(&str, &str)],
        body: &str,
    ) -> (u16, Value, axum::http::HeaderMap) {
        let mut b = HttpRequest::builder().method(method).uri(path);
        // the defaults, unless the test names its own
        for (k, v) in [("host", "127.0.0.1:9615"), ("x-api-key", KEY)] {
            if !extra.iter().any(|(name, _)| *name == k) {
                b = b.header(k, v);
            }
        }
        for (k, v) in extra {
            b = b.header(*k, *v);
        }
        let response = router
            .clone()
            .oneshot(b.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            headers,
        )
    }

    #[tokio::test]
    async fn health_says_only_ok_and_needs_no_key() {
        let router = app_router(Fake::new(vec![]), HostLevel::Lifecycle);
        let request = HttpRequest::builder()
            .uri("/api/health")
            .header("host", "localhost:9615")
            .body(Body::empty())
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);
        let bytes = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&bytes[..], br#"{"ok":true}"#);
    }

    #[tokio::test]
    async fn everything_else_needs_the_key_even_on_loopback() {
        let fake = Fake::new(vec![]);
        let router = app_router(fake.clone(), HostLevel::Lifecycle);
        for (method, path) in [
            ("GET", "/api/processes"),
            ("POST", "/api/processes"),
            ("DELETE", "/api/processes/all"),
            ("GET", "/ws"),
            ("GET", "/metrics"),
            ("GET", "/"),
        ] {
            let request = HttpRequest::builder()
                .method(method)
                .uri(path)
                .header("host", "127.0.0.1:9615")
                .body(Body::empty())
                .unwrap();
            assert_eq!(
                router.clone().oneshot(request).await.unwrap().status(),
                401,
                "{method} {path}"
            );
        }
        let (code, ..) = call(
            &router,
            "GET",
            "/api/processes",
            &[("x-api-key", "wrong")],
            "",
        )
        .await;
        assert_eq!(code, 401);
        assert!(fake.seen().is_empty(), "nothing reached the daemon");
    }

    #[tokio::test]
    async fn a_rebound_host_and_a_foreign_origin_are_refused_before_any_handler() {
        let fake = Fake::new(vec![]);
        let router = app_router(fake.clone(), HostLevel::Lifecycle);
        let (code, ..) = call(
            &router,
            "GET",
            "/api/processes",
            &[("host", "evil.example:9615")],
            "",
        )
        .await;
        assert_eq!(code, 403);
        for method in ["POST", "DELETE"] {
            let (code, ..) = call(
                &router,
                method,
                "/api/processes/all",
                &[("origin", "http://evil.example")],
                "",
            )
            .await;
            assert_eq!(code, 403, "{method}");
        }
        let (code, ..) = call(
            &router,
            "GET",
            "/ws",
            &[
                ("origin", "http://evil.example"),
                ("upgrade", "websocket"),
                ("connection", "upgrade"),
            ],
            "",
        )
        .await;
        assert_eq!(code, 403);
        assert!(fake.seen().is_empty());
    }

    #[tokio::test]
    async fn the_configured_origin_gets_cors_answers_and_nobody_else() {
        let fake = Fake::new(vec![Ok(json!({ "type": "status", "apps": [] }))]);
        let router = app_router(fake, HostLevel::Lifecycle);
        let (code, _, headers) = call(
            &router,
            "GET",
            "/api/processes",
            &[("origin", "http://localhost:3000")],
            "",
        )
        .await;
        assert_eq!(code, 200);
        assert_eq!(
            headers["access-control-allow-origin"],
            "http://localhost:3000"
        );
        // preflight: no key, answered for the configured origin only
        let preflight = |origin: &'static str| {
            HttpRequest::builder()
                .method("OPTIONS")
                .uri("/api/processes/1/restart")
                .header("host", "127.0.0.1:9615")
                .header("origin", origin)
                .header("access-control-request-method", "POST")
                .body(Body::empty())
                .unwrap()
        };
        let ok = router
            .clone()
            .oneshot(preflight("http://localhost:3000"))
            .await
            .unwrap();
        assert_eq!(ok.status(), 204);
        assert!(ok.headers()["access-control-allow-headers"]
            .to_str()
            .unwrap()
            .contains("x-api-key"));
        assert_eq!(
            router
                .clone()
                .oneshot(preflight("http://evil.example"))
                .await
                .unwrap()
                .status(),
            403
        );
    }

    #[tokio::test]
    async fn the_list_is_asked_for_once_a_second_however_many_clients() {
        let fake = Fake::new(vec![Ok(
            json!({ "type": "status", "apps": [app(1, "a", &[])] }),
        )]);
        let router = app_router(fake.clone(), HostLevel::Lifecycle);
        for _ in 0..5 {
            let (code, body, _) = call(&router, "GET", "/api/processes", &[], "").await;
            assert_eq!(code, 200);
            assert_eq!(body["data"][0]["spec"]["name"], "a");
            assert_eq!(body["success"], true);
        }
        assert_eq!(fake.seen().len(), 1);
    }

    #[tokio::test]
    async fn a_daemon_that_is_gone_is_503_not_an_empty_list() {
        let router = app_router(Fake::new(vec![Err(Fail::Gone)]), HostLevel::Lifecycle);
        let (code, body, _) = call(&router, "GET", "/api/processes", &[], "").await;
        assert_eq!(code, 503);
        assert_eq!(body["success"], false);
        assert!(body["data"].is_null());
    }

    #[tokio::test]
    async fn daemon_errors_keep_their_code_and_get_a_matching_status() {
        for (code_name, expected) in [
            ("E_POLICY_DENIED", 403),
            ("E_APP_NOT_FOUND", 404),
            ("E_RATE_LIMITED", 429),
            ("E_APP_EXISTS", 409),
            ("E_INVALID_SELECTOR", 400),
            ("E_INTERNAL", 500),
        ] {
            let fake = Fake::new(vec![Ok(
                json!({ "type": "error", "code": code_name, "message": "nope" }),
            )]);
            let router = app_router(fake, HostLevel::Lifecycle);
            let (code, body, headers) =
                call(&router, "POST", "/api/processes/web/restart", &[], "").await;
            assert_eq!(code, expected, "{code_name}");
            assert_eq!(body["code"], code_name);
            assert_eq!(body["error"], "nope");
            if expected == 429 {
                assert_eq!(headers["retry-after"], "1");
            }
        }
    }

    #[tokio::test]
    async fn stop_and_restart_send_the_selector_the_cli_would() {
        let fake = Fake::new(vec![
            Ok(json!({ "type": "stopped", "count": 2, "items": [] })),
            Ok(json!({ "type": "restarted", "count": 1, "items": [] })),
        ]);
        let router = app_router(fake.clone(), HostLevel::Lifecycle);
        let (code, body, _) = call(&router, "POST", "/api/processes/@web/stop", &[], "").await;
        assert_eq!((code, &body["data"]["stopped"]), (200, &json!(2)));
        let (code, body, _) = call(&router, "POST", "/api/processes/7/restart", &[], "").await;
        assert_eq!((code, &body["data"]["restarted"]), (200, &json!(1)));
        let seen = fake.seen();
        assert_eq!(
            seen[0],
            json!({ "type": "stop", "selector": { "by": "tag", "value": "web" } })
        );
        assert_eq!(
            seen[1],
            json!({ "type": "restart", "selector": { "by": "id", "value": 7 } })
        );
    }

    #[tokio::test]
    async fn a_selector_with_control_characters_is_a_400() {
        let fake = Fake::new(vec![]);
        let router = app_router(fake.clone(), HostLevel::Lifecycle);
        let (code, ..) = call(&router, "POST", "/api/processes/a%0Ab/stop", &[], "").await;
        assert_eq!(code, 400);
        let long = "x".repeat(200);
        let (code, ..) = call(&router, "GET", &format!("/api/processes/{long}"), &[], "").await;
        assert_eq!(code, 400);
        assert!(fake.seen().is_empty());
    }

    #[tokio::test]
    async fn delete_all_needs_the_confirmation_field() {
        let deleted = || Ok(json!({ "type": "deleted", "count": 0, "items": [] }));
        let fake = Fake::new(vec![deleted(), deleted(), deleted()]);
        let router = app_router(fake.clone(), HostLevel::Admin);
        for (path, body) in [
            ("/api/processes/all", ""),
            ("/api/processes/all", "{}"),
            ("/api/processes/all", r#"{"confirm":true}"#),
            ("/api/processes/ALL?confirm=yes", ""),
        ] {
            let (code, ..) = call(&router, "DELETE", path, &[], body).await;
            assert_eq!(code, 400, "{path} {body}");
        }
        assert!(
            fake.seen().is_empty(),
            "an unconfirmed delete never reaches the daemon"
        );
        for (path, body) in [
            ("/api/processes/all", r#"{"confirm":"all"}"#),
            ("/api/processes/all?confirm=all", ""),
            ("/api/processes/one", ""),
        ] {
            let (code, ..) = call(&router, "DELETE", path, &[], body).await;
            assert_eq!(code, 200, "{path} {body}");
        }
        assert_eq!(fake.seen().len(), 3);
    }

    #[tokio::test]
    async fn logs_of_a_tag_come_from_every_app_with_it() {
        let fake = Fake::new(vec![
            Ok(
                json!({ "type": "status", "apps": [app(1, "a", &["web"]), app(2, "b", &["db"]), app(3, "c", &["web", "x"])] }),
            ),
            Ok(
                json!({ "type": "log_lines", "lines": ["2026-01-01T00:00:02 from a", "2026-01-01T00:00:04 from a"] }),
            ),
            Ok(json!({ "type": "log_lines", "lines": ["2026-01-01T00:00:03 from c"] })),
        ]);
        let router = app_router(fake.clone(), HostLevel::Lifecycle);
        let (code, body, _) =
            call(&router, "GET", "/api/processes/@web/logs?lines=10", &[], "").await;
        assert_eq!(code, 200);
        assert_eq!(
            body["data"],
            json!([
                "[a] 2026-01-01T00:00:02 from a",
                "[c] 2026-01-01T00:00:03 from c",
                "[a] 2026-01-01T00:00:04 from a"
            ])
        );
        let seen = fake.seen();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[1]["selector"], json!({ "by": "id", "value": 1 }));
        assert_eq!(seen[2]["selector"], json!({ "by": "id", "value": 3 }));
        assert_eq!(seen[1]["lines"], 10);
    }

    #[tokio::test]
    async fn logs_of_a_tag_nobody_has_is_404_and_bad_line_counts_are_400() {
        let fake = Fake::new(vec![Ok(
            json!({ "type": "status", "apps": [app(1, "a", &["web"])] }),
        )]);
        let router = app_router(fake, HostLevel::Lifecycle);
        let (code, ..) = call(&router, "GET", "/api/processes/@nope/logs", &[], "").await;
        assert_eq!(code, 404);
        for lines in ["0", "1001", "x", "-1"] {
            let (code, ..) = call(
                &router,
                "GET",
                &format!("/api/processes/1/logs?lines={lines}"),
                &[],
                "",
            )
            .await;
            assert_eq!(code, 400, "{lines}");
        }
    }

    #[tokio::test]
    async fn creating_an_app_is_refused_below_admin_whatever_the_body() {
        let fake = Fake::new(vec![]);
        for level in [HostLevel::Read, HostLevel::Lifecycle] {
            let router = app_router(fake.clone(), level);
            for body in [
                r#"{"name":"x","command":"true","cwd":"/tmp"}"#,
                "not json",
                "",
            ] {
                let (code, json, _) = call(&router, "POST", "/api/processes", &[], body).await;
                assert_eq!(code, 403, "{level:?} {body}");
                assert!(json["error"].as_str().unwrap().contains("admin"));
            }
        }
        assert!(fake.seen().is_empty());
    }

    #[tokio::test]
    async fn creating_an_app_with_admin_sends_a_cmd_spec_that_lists_the_adapter() {
        let fake = Fake::new(vec![Ok(
            json!({ "type": "started", "id": 4, "name": "x", "status": "running", "child_ids": [] }),
        )]);
        let router = app_router(fake.clone(), HostLevel::Admin);
        let (code, body, _) = call(
            &router,
            "POST",
            "/api/processes",
            &[],
            r#"{"name":"x","command":"sleep","args":["5"],"cwd":"/tmp"}"#,
        )
        .await;
        assert_eq!(code, 200, "{body}");
        assert_eq!(body["data"], json!({ "id": 4, "name": "x" }));
        let spec = &fake.seen()[0]["spec"];
        assert_eq!(spec["mode"], "cmd");
        assert_eq!(spec["cwd"], "/tmp");
        assert_eq!(spec["args"], json!(["5"]));
        assert!(spec["adapters"].get("ui-web").is_some());
    }

    #[test]
    fn a_start_body_is_checked_here_and_never_panics() {
        let body = |json: &str| serde_json::from_str::<StartBody>(json).unwrap();
        let err = |json: &str| spec_of(body(json)).unwrap_err();
        assert!(err(r#"{"name":"a b","command":"true","cwd":"/tmp"}"#)
            .message
            .contains("invalid app name"));
        assert!(err(r#"{"name":"","command":"true","cwd":"/tmp"}"#)
            .message
            .contains("empty"));
        assert!(err(r#"{"name":"all","command":"true","cwd":"/tmp"}"#)
            .message
            .contains("reserved"));
        assert!(err(r#"{"name":"12","command":"true","cwd":"/tmp"}"#)
            .message
            .contains("id"));
        assert!(err(r#"{"name":"x","command":"true","cwd":"."}"#)
            .message
            .contains("absolute"));
        assert!(err(r#"{"name":"x","command":"true","cwd":""}"#)
            .message
            .contains("absolute"));
        assert!(err(r#"{"name":"x","command":" ","cwd":"/tmp"}"#)
            .message
            .contains("command"));
        assert!(
            err(r#"{"name":"x","command":"true","cwd":"/tmp","runtime":"Node;rm"}"#)
                .message
                .contains("runtime")
        );
        assert!(
            serde_json::from_str::<StartBody>(
                r#"{"name":"x","command":"true","cwd":"/tmp","watch":true}"#
            )
            .is_err(),
            "unknown fields are refused"
        );
        // the runtime is what was asked for, not what the directory looks like
        let spec = spec_of(body(r#"{"name":"x","command":"true","cwd":"/tmp","runtime":"node","instances":2,"port":8080}"#)).unwrap();
        assert_eq!(spec.mode.as_str(), "node");
        assert_eq!((spec.instances, spec.port), (2, Some(8080)));
        let spec = spec_of(body(r#"{"name":"x","command":"true","cwd":"/tmp"}"#)).unwrap();
        assert_eq!(spec.mode.as_str(), "cmd");
    }

    #[tokio::test]
    async fn a_body_over_the_limit_is_413() {
        let router = app_router(Fake::new(vec![]), HostLevel::Admin);
        let huge = format!(
            r#"{{"name":"x","command":"{}","cwd":"/tmp"}}"#,
            "a".repeat(MAX_BODY + 10)
        );
        let (code, ..) = call(&router, "POST", "/api/processes", &[], &huge).await;
        assert_eq!(code, 413);
    }

    #[test]
    fn logged_input_only_reports_the_frame_type_and_byte_count() {
        let text = "ok\nui-web: forged line\r\u{1b}[31m☃";
        let note = ignored_text_note(text);
        assert_eq!(
            note,
            format!(
                "ui-web: a WebSocket client sent text ({} bytes) that is ignored",
                text.len()
            )
        );
        assert!(
            !note.contains("forged line") && !note.chars().any(char::is_control),
            "{note:?}"
        );
    }

    #[tokio::test]
    async fn a_new_key_replaces_the_old_one_at_once() {
        let fake = Fake::new(vec![Ok(json!({ "type": "pong" }))]);
        let rules = Rules::new("127.0.0.1:9615".parse().unwrap(), None, vec![]);
        let shared = Shared::new(
            fake,
            KEY.to_string(),
            rules,
            grant(HostLevel::Lifecycle),
            Hub::new(),
        );
        let router = router(shared.clone());
        shared.set_key("another-key-another-key-another".to_string());
        let (code, ..) = call(&router, "GET", "/api/ping", &[], "").await;
        assert_eq!(code, 401, "the old key is dead");
        let (code, ..) = call(
            &router,
            "GET",
            "/api/ping",
            &[("x-api-key", "another-key-another-key-another")],
            "",
        )
        .await;
        assert_eq!(code, 200);
    }
}
