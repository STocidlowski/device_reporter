//! HTTP and WebSocket surface.
//!
//! | Route                                      | Purpose                                              |
//! |--------------------------------------------|------------------------------------------------------|
//! | `GET /`                                    | The embedded status page (`static/index.html`).      |
//! | `GET /api/status`                          | Process health plus every device's status.           |
//! | `GET /api/devices`                         | Just the device list.                                |
//! | `GET /api/latest[?device=ID]`              | Most recent observation (from one device), or 404.   |
//! | `GET /api/observations[?device=ID&limit=N]`| Recent observations, newest first.                   |
//! | `GET /ws`                                  | Live `server`, `device`, `reading`, `observation`.   |
//!
//! A WebSocket client first receives a `server` snapshot and the latest
//! observation from each device, then live events. The server pings every
//! 20 s so idle connections survive nginx's default 60 s `proxy_read_timeout`.

use crate::model::Event;
use crate::state::AppState;
use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;
use tower_http::cors::{Any, CorsLayer};

const INDEX_HTML: &str = include_str!("../static/index.html");
const PING_INTERVAL: Duration = Duration::from_secs(20);
const DEFAULT_HISTORY_LIMIT: usize = 20;

/// Where to listen and who may call from a browser on another origin.
#[derive(Debug, Clone)]
pub struct WebConfig {
    pub bind: SocketAddr,
    /// Allowed CORS origins. Empty disables CORS; `*` allows every origin.
    pub cors_origins: Vec<String>,
}

/// Build the router. Separate from [`serve`] so tests can drive it in-process.
pub fn router(state: Arc<AppState>, cors_origins: &[String]) -> Router {
    let app = Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/devices", get(devices))
        .route("/api/latest", get(latest))
        .route("/api/observations", get(observations))
        .route("/ws", get(ws_upgrade))
        .with_state(state);
    match cors_layer(cors_origins) {
        Some(cors) => app.layer(cors),
        None => app,
    }
}

/// Bind and serve until Ctrl-C or SIGTERM.
pub async fn serve(cfg: WebConfig, state: Arc<AppState>) -> anyhow::Result<()> {
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
        .allow_methods([Method::GET])
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

async fn status(State(state): State<Arc<AppState>>) -> Response {
    Json(state.server_status()).into_response()
}

async fn devices(State(state): State<Arc<AppState>>) -> Response {
    Json(state.devices()).into_response()
}

#[derive(Debug, Deserialize)]
struct ObservationQuery {
    device: Option<String>,
    limit: Option<usize>,
}

async fn latest(State(state): State<Arc<AppState>>, Query(q): Query<ObservationQuery>) -> Response {
    match state.latest(q.device.as_deref()) {
        Some(o) => Json(o).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no observation yet" })),
        )
            .into_response(),
    }
}

async fn observations(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ObservationQuery>,
) -> Response {
    Json(state.history(
        q.device.as_deref(),
        q.limit.unwrap_or(DEFAULT_HISTORY_LIMIT),
    ))
    .into_response()
}

async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(move |socket| ws_session(socket, state))
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
        if sink
            .send(Message::Text(event.to_json().into()))
            .await
            .is_err()
        {
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
                    if sink.send(Message::Text(event.to_json().into())).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(n)) => tracing::warn!(missed = n, "slow websocket client skipped events"),
                Err(RecvError::Closed) => break,
            },
            _ = ping.tick() => {
                if sink.send(Message::Ping(Bytes::new())).await.is_err() {
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
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn app() -> (Arc<AppState>, Router) {
        let state = Arc::new(AppState::new("test".to_owned(), 5));
        let router = router(Arc::clone(&state), &[]);
        (state, router)
    }

    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn index_serves_the_embedded_page() {
        let (_, app) = app();
        let resp = app
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        assert!(std::str::from_utf8(&bytes).unwrap().contains("<title>"));
    }

    #[tokio::test]
    async fn latest_is_404_until_an_observation_exists() {
        let (_, app) = app();
        let resp = app
            .oneshot(Request::get("/api/latest").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(body_json(resp).await["error"], "no observation yet");
    }

    #[tokio::test]
    async fn status_lists_no_devices_initially() {
        let (_, app) = app();
        let resp = app
            .oneshot(Request::get("/api/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json["host"], "test");
        assert_eq!(json["devices"], json!([]));
    }

    #[tokio::test]
    async fn observations_is_an_empty_list_initially() {
        let (_, app) = app();
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

    #[test]
    fn cors_is_off_unless_configured() {
        assert!(cors_layer(&[]).is_none());
        assert!(cors_layer(&["*".to_owned()]).is_some());
        assert!(cors_layer(&["http://localhost:8080".to_owned()]).is_some());
    }
}
