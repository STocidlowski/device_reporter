//! HTTP and WebSocket surface.
//!
//! | Route                                      | Purpose                                              |
//! |--------------------------------------------|------------------------------------------------------|
//! | `GET /`                                    | The embedded status page (`static/index.html`).      |
//! | `GET /api/status`                          | Process health, every device, and forwarding state.  |
//! | `GET /api/devices`                         | Just the device list.                                |
//! | `GET /api/latest[?device=ID]`              | Most recent observation (from one device), or 404.   |
//! | `GET /api/observations[?device=ID&limit=N]`| Recent observations, newest first.                   |
//! | `GET /api/settings`                        | Settings, secrets redacted, plus lockout state.       |
//! | `PUT /api/settings`                        | Change settings (`{password?, settings}`).           |
//! | `PUT /api/settings/password`               | Set, change or clear the password (`{current?, new?}`).|
//! | `POST /api/settings/test`                  | Probe the configured EMR with the saved credentials. |
//! | `GET /ws`                                  | Live `server`, `device`, `reading`, `observation`.   |
//!
//! Reads are open to anyone who can reach the listener; the tailnet and the
//! firewall are the perimeter (see the README). Changes are gated by the
//! optional settings password, carried with each request rather than a
//! session cookie, so there is nothing for a cross-site request to ride.
//!
//! Hardening that costs nothing: conservative security headers on every
//! response, a 16 KB body limit, bounded WebSocket clients and frame sizes, a
//! send timeout so a stalled client cannot pin a task, and one Argon2 check
//! at a time so the settings password cannot be used to exhaust a Pi Zero.
//!
//! Settings errors map to `401` (password missing or wrong), `423` (locked
//! out, with `retry_after_secs`), `429` (another check in progress), `400`
//! (invalid value) and `500` (could not write the file).

use crate::model::Event;
use crate::settings::{SettingsError, SettingsPatch, SettingsStore};
use crate::state::AppState;
use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Query, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::sync::broadcast::error::RecvError;
use tower_http::cors::{Any, CorsLayer};

const INDEX_HTML: &str = include_str!("../static/index.html");
const PING_INTERVAL: Duration = Duration::from_secs(20);
const DEFAULT_HISTORY_LIMIT: usize = 20;
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const WS_SEND_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_WEBSOCKET_CLIENTS: usize = 16;

/// Where to listen and who may call from a browser on another origin.
#[derive(Debug, Clone)]
pub struct WebConfig {
    pub bind: SocketAddr,
    /// Allowed CORS origins. Empty disables CORS; `*` allows every origin.
    pub cors_origins: Vec<String>,
}

/// Concurrency caps. Argon2 is one at a time on purpose.
pub struct Limits {
    password_work: Semaphore,
    probe_work: Semaphore,
    websocket_slots: Semaphore,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            password_work: Semaphore::new(1),
            probe_work: Semaphore::new(1),
            websocket_slots: Semaphore::new(MAX_WEBSOCKET_CLIENTS),
        }
    }
}

/// Everything the handlers need.
#[derive(Clone)]
pub struct WebState {
    pub app: Arc<AppState>,
    pub settings: Arc<SettingsStore>,
    pub limits: Arc<Limits>,
}

/// Build the router. Separate from [`serve`] so tests can drive it in-process.
pub fn router(state: WebState, cors_origins: &[String]) -> Router {
    let app = Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/devices", get(devices))
        .route("/api/latest", get(latest))
        .route("/api/observations", get(observations))
        .route("/api/settings", get(get_settings).put(put_settings))
        .route("/api/settings/password", axum::routing::put(put_password))
        .route("/api/settings/test", post(test_forward))
        .route("/ws", get(ws_upgrade))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(middleware::from_fn(security_headers))
        .with_state(state);
    match cors_layer(cors_origins) {
        Some(cors) => app.layer(cors),
        None => app,
    }
}

/// Conservative headers on every response: no caching of clinical data, no
/// framing, no MIME sniffing, and a CSP that only allows the page's own
/// inline script and same-origin connections.
async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    );
    response
}

/// Bind and serve until Ctrl-C or SIGTERM.
pub async fn serve(cfg: WebConfig, state: WebState) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    tracing::info!(
        "listening on http://{}  (page /, status /api/status, latest /api/latest, stream /ws)",
        cfg.bind
    );
    axum::serve(listener, router(state, &cfg.cors_origins))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

fn cors_layer(origins: &[String]) -> Option<CorsLayer> {
    if origins.is_empty() {
        return None;
    }
    let layer = CorsLayer::new()
        .allow_methods([Method::GET, Method::PUT, Method::POST])
        .allow_headers(Any);
    if origins.iter().any(|o| o == "*") {
        return Some(layer.allow_origin(Any));
    }
    let parsed: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|o| {
            if let Ok(v) = o.parse::<HeaderValue>() {
                Some(v)
            } else {
                tracing::warn!(origin = %o, "ignoring invalid CORS origin");
                None
            }
        })
        .collect();
    Some(layer.allow_origin(parsed))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "failed to listen for ctrl-c");
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => tracing::error!(error = %e, "failed to listen for SIGTERM"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutting down");
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn status(State(state): State<WebState>) -> Response {
    Json(state.app.server_status()).into_response()
}

async fn devices(State(state): State<WebState>) -> Response {
    Json(state.app.devices()).into_response()
}

#[derive(Debug, Deserialize)]
struct ObservationQuery {
    device: Option<String>,
    limit: Option<usize>,
}

async fn latest(State(state): State<WebState>, Query(q): Query<ObservationQuery>) -> Response {
    match state.app.latest(q.device.as_deref()) {
        Some(o) => Json(o).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no observation yet" })),
        )
            .into_response(),
    }
}

async fn observations(
    State(state): State<WebState>,
    Query(q): Query<ObservationQuery>,
) -> Response {
    Json(state.app.history(
        q.device.as_deref(),
        q.limit.unwrap_or(DEFAULT_HISTORY_LIMIT),
    ))
    .into_response()
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

fn settings_error(e: &SettingsError) -> Response {
    let (status, body) = match e {
        SettingsError::Busy => (
            StatusCode::TOO_MANY_REQUESTS,
            json!({ "error": e.to_string() }),
        ),
        SettingsError::Locked(d) => (
            StatusCode::LOCKED,
            json!({ "error": e.to_string(), "retry_after_secs": d.as_secs().saturating_add(u64::from(d.subsec_nanos() > 0)) }),
        ),
        SettingsError::WrongPassword | SettingsError::PasswordRequired => {
            (StatusCode::UNAUTHORIZED, json!({ "error": e.to_string() }))
        }
        SettingsError::Invalid(_) => (StatusCode::BAD_REQUEST, json!({ "error": e.to_string() })),
        SettingsError::Io(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": e.to_string() }),
        ),
    };
    (status, Json(body)).into_response()
}

async fn get_settings(State(state): State<WebState>) -> Response {
    Json(state.settings.redacted()).into_response()
}

#[derive(Debug, Deserialize)]
struct SettingsUpdate {
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    settings: SettingsPatch,
}

async fn put_settings(State(state): State<WebState>, Json(body): Json<SettingsUpdate>) -> Response {
    let Ok(permit) = state.limits.password_work.try_acquire() else {
        return settings_error(&SettingsError::Busy);
    };
    let store = Arc::clone(&state.settings);
    let result =
        tokio::task::spawn_blocking(move || store.update(body.settings, body.password.as_deref()))
            .await
            .unwrap_or_else(|e| Err(SettingsError::Io(format!("task failed: {e}"))));
    drop(permit);
    match result {
        Ok(redacted) => Json(redacted).into_response(),
        Err(e) => settings_error(&e),
    }
}

#[derive(Debug, Deserialize)]
struct PasswordChange {
    #[serde(default)]
    current: Option<String>,
    #[serde(default)]
    new: Option<String>,
}

async fn put_password(State(state): State<WebState>, Json(body): Json<PasswordChange>) -> Response {
    let Ok(permit) = state.limits.password_work.try_acquire() else {
        return settings_error(&SettingsError::Busy);
    };
    let store = Arc::clone(&state.settings);
    let result = tokio::task::spawn_blocking(move || {
        store.set_password(body.current.as_deref(), body.new.as_deref())
    })
    .await
    .unwrap_or_else(|e| Err(SettingsError::Io(format!("task failed: {e}"))));
    drop(permit);
    match result {
        Ok(redacted) => Json(redacted).into_response(),
        Err(e) => settings_error(&e),
    }
}

async fn test_forward(State(state): State<WebState>) -> Response {
    let Ok(permit) = state.limits.probe_work.try_acquire() else {
        return settings_error(&SettingsError::Busy);
    };
    let store = Arc::clone(&state.settings);
    let result = tokio::task::spawn_blocking(move || crate::forward::probe(&store, PROBE_TIMEOUT))
        .await
        .unwrap_or_else(|e| Err(format!("task failed: {e}")));
    drop(permit);
    match result {
        Ok(code) => {
            let (ok, verdict) = match code {
                404 => (true, "ok: the EMR accepted the credentials"),
                200..=299 => (true, "ok: reached the EMR"),
                401 | 403 => (false, "the EMR rejected the credentials"),
                _ => (
                    false,
                    "reached a server, but it did not answer like a FHIR base",
                ),
            };
            Json(json!({ "ok": ok, "status": code, "message": verdict })).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "ok": false, "status": null, "message": e })),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------

async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<WebState>) -> Response {
    let Some(permit) = state.limits.websocket_slot() else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    ws.max_message_size(1024)
        .max_frame_size(1024)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            ws_session(socket, state.app).await;
        })
}

impl Limits {
    /// Reserve a WebSocket slot for the lifetime of the session, or `None` when full.
    fn websocket_slot(self: &Arc<Self>) -> Option<OwnedSlot> {
        let permit = self.websocket_slots.try_acquire().ok()?;
        permit.forget();
        Some(OwnedSlot(Arc::clone(self)))
    }
}

/// A WebSocket slot, returned when the session ends.
struct OwnedSlot(Arc<Limits>);

impl Drop for OwnedSlot {
    fn drop(&mut self) {
        self.0.websocket_slots.add_permits(1);
    }
}

async fn ws_session(socket: WebSocket, state: Arc<AppState>) {
    // Subscribe first, then snapshot, so an event landing in between is not lost.
    let mut events = state.subscribe();
    let (mut sink, mut stream) = socket.split();

    let snapshot = std::iter::once(Event::Server(state.server_status())).chain(
        state
            .latest_per_device()
            .into_iter()
            .map(Event::Observation),
    );
    for event in snapshot {
        if !send_ws(&mut sink, Message::Text(event.to_json().into())).await {
            return;
        }
    }
    tracing::debug!("websocket client connected");

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await; // the first tick fires immediately; skip it

    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(event) => {
                    if !send_ws(&mut sink, Message::Text(event.to_json().into())).await {
                        break;
                    }
                }
                Err(RecvError::Lagged(n)) => tracing::warn!(missed = n, "slow websocket client skipped events"),
                Err(RecvError::Closed) => break,
            },
            _ = ping.tick() => {
                if !send_ws(&mut sink, Message::Ping(Bytes::new())).await {
                    break;
                }
            }
            incoming = stream.next() => match incoming {
                None | Some(Err(_) | Ok(Message::Close(_))) => break,
                Some(Ok(_)) => {} // clients have nothing to say; pongs are handled by axum
            },
        }
    }
    tracing::debug!("websocket client disconnected");
}

async fn send_ws(
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    message: Message,
) -> bool {
    matches!(
        tokio::time::timeout(WS_SEND_TIMEOUT, sink.send(message)).await,
        Ok(Ok(()))
    )
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::assert_is_empty,
    clippy::similar_names
)]
mod tests {
    use super::*;
    use crate::settings::Settings;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn app() -> (WebState, Router) {
        let dir = std::env::temp_dir().join(format!("dr-web-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = WebState {
            app: Arc::new(AppState::new("test".to_owned(), 5)),
            settings: Arc::new(
                SettingsStore::open(
                    dir.join("settings.json"),
                    &Settings::default(),
                    vec!["healthometer_scale".to_owned()],
                )
                .unwrap(),
            ),
            limits: Arc::new(Limits::default()),
        };
        let router = router(state.clone(), &[]);
        (state, router)
    }

    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[allow(clippy::needless_pass_by_value)]
    fn json_req(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    #[tokio::test]
    async fn index_is_public_and_carries_security_headers() {
        let (_, app) = app();
        let resp = app
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["cache-control"], "no-store");
        assert_eq!(resp.headers()["x-frame-options"], "DENY");
        assert!(
            resp.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("frame-ancestors 'none'")
        );
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        assert!(std::str::from_utf8(&bytes).unwrap().contains("<title>"));
    }

    #[tokio::test]
    async fn reads_are_open_and_status_includes_forwarding() {
        let (_, app) = app();
        let resp = app
            .clone()
            .oneshot(Request::get("/api/latest").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let resp = app
            .clone()
            .oneshot(Request::get("/api/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let json = body_json(resp).await;
        assert_eq!(json["host"], "test");
        assert_eq!(json["devices"], json!([]));
        assert!(json["forwarding"].is_object());
        let resp = app
            .oneshot(
                Request::get("/api/observations?limit=3&device=x")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(body_json(resp).await, json!([]));
    }

    #[tokio::test]
    async fn oversized_bodies_are_refused() {
        let (_, app) = app();
        let big = "x".repeat(20 * 1024);
        let resp = app
            .oneshot(json_req(
                "PUT",
                "/api/settings",
                json!({ "settings": { "host": big } }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn settings_round_trip_redacts_and_gates_on_password() {
        let (_, app) = app();

        // Open: no password, a change goes through and the key comes back redacted.
        let resp = app
            .clone()
            .oneshot(json_req(
                "PUT",
                "/api/settings",
                json!({ "settings": { "forward_url": "http://emr:8000/fhir/v5", "forward_api_key": "secretkey99" } }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json["forward_url"], "http://emr:8000/fhir/v5");
        assert_eq!(json["forward_api_key_hint"], "…ey99");
        assert_eq!(json["password_set"], false);

        // Invalid value is a 400 and changes nothing.
        let resp = app
            .clone()
            .oneshot(json_req(
                "PUT",
                "/api/settings",
                json!({ "settings": { "forward_url": "emr:8000" } }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Set a password from the page; now a change without it is 401, wrong is 401, then 423.
        let resp = app
            .clone()
            .oneshot(json_req(
                "PUT",
                "/api/settings/password",
                json!({ "new": "correct horse" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["password_set"], true);

        let resp = app
            .clone()
            .oneshot(json_req(
                "PUT",
                "/api/settings",
                json!({ "settings": { "host": "x" } }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let resp = app
            .clone()
            .oneshot(json_req(
                "PUT",
                "/api/settings",
                json!({ "password": "wrong", "settings": { "host": "x" } }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let resp = app
            .clone()
            .oneshot(json_req(
                "PUT",
                "/api/settings",
                json!({ "password": "correct horse", "settings": { "host": "x" } }),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::LOCKED,
            "locked right after a wrong password"
        );
        assert!(body_json(resp).await["retry_after_secs"].as_u64().unwrap() >= 1);

        // GET never leaks the key.
        let resp = app
            .clone()
            .oneshot(Request::get("/api/settings").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let text = String::from_utf8(
            axum::body::to_bytes(resp.into_body(), 1 << 20)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(!text.contains("secretkey99"));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["password_set"], true);
    }

    #[tokio::test]
    async fn test_forward_without_a_url_is_a_502_with_a_message() {
        let (_, app) = app();
        let resp = app
            .oneshot(json_req("POST", "/api/settings/test", json!({})))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(body_json(resp).await["ok"], false);
    }

    #[test]
    fn cors_is_off_unless_configured() {
        assert!(cors_layer(&[]).is_none());
        assert!(cors_layer(&["*".to_owned()]).is_some());
        assert!(cors_layer(&["http://localhost:8080".to_owned()]).is_some());
    }

    #[test]
    fn websocket_slots_are_returned_on_drop() {
        let limits = Arc::new(Limits::default());
        let slots: Vec<OwnedSlot> = (0..MAX_WEBSOCKET_CLIENTS)
            .map(|_| limits.websocket_slot().unwrap())
            .collect();
        assert!(limits.websocket_slot().is_none());
        drop(slots);
        assert!(limits.websocket_slot().is_some());
    }
}
