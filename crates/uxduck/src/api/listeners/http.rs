//! The loopback HTTP and WebSocket listener (ADR-0001 D4, D5): `Host` and
//! `Origin` allowlists against DNS rebinding and cross-site use, browser
//! pairing, the session cookie, and JSON-RPC over WebSocket at `/rpc`.
//!
//! UXDuck on the web (`app.uxduck.com`) is another site, so its cookies do
//! not reach this listener (ADR-0011, open decision 1, closed 2026-10-02).
//! It pairs like any browser, is given the session token in the answer, and
//! sends it back as a WebSocket subprotocol, `uxduck-token.<token>`, or as
//! `Authorization: Bearer`. Such a token only works from the origin that
//! paired. Chrome asks the person before the page may reach this computer
//! (Local Network Access); the preflight says this listener agrees.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tower_http::services::ServeFile;
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::policy::admission::Trust;
use uxduck_engine::PairingState;

use crate::daemon::engine_task::EngineHandle;
use crate::interface::Interfaces;

pub const SESSION_COOKIE: &str = "uxduck_session";

/// UXDuck on the web, production's and staging's: other sites that may pair
/// and connect with a token.
pub const HOSTED_ORIGINS: [&str; 2] = ["https://app.uxduck.com", "https://app-staging.uxduck.com"];

/// The subprotocol every token connection speaks, echoed back.
const PROTOCOL: &str = "uxduck";
/// The prefix of the subprotocol that carries the session token.
const TOKEN_PROTOCOL: &str = "uxduck-token.";

/// Who may talk to the listener.
#[derive(Clone, Debug)]
pub struct Allowlist {
    /// `Host` header values, such as `127.0.0.1:47800`.
    pub hosts: Vec<String>,
    /// `Origin` header values, such as `http://localhost:8793`.
    pub origins: Vec<String>,
    /// Origins of other sites, such as `https://app.uxduck.com`: allowed
    /// with CORS, and with a token instead of the cookie. Their hosts are not.
    pub hosted: Vec<String>,
}

impl Allowlist {
    /// The daemon's own origin on every loopback name, plus `extra_origins`
    /// (the dev server) whose hosts are allowed too, since a dev proxy
    /// forwards its own `Host`.
    #[must_use]
    pub fn new(port: u16, extra_origins: &[String]) -> Self {
        let mut hosts: Vec<String> = ["127.0.0.1", "localhost", "[::1]"]
            .iter()
            .map(|name| format!("{name}:{port}"))
            .collect();
        let mut origins: Vec<String> = hosts.iter().map(|host| format!("http://{host}")).collect();
        for origin in extra_origins {
            let origin = origin.trim_end_matches('/').to_owned();
            if let Some(host) = origin.split_once("://").map(|(_, host)| host.to_owned()) {
                hosts.push(host);
            }
            origins.push(origin);
        }
        Self {
            hosts,
            origins,
            hosted: Vec::new(),
        }
    }

    /// Also allows these other sites, such as `HOSTED_ORIGINS`.
    #[must_use]
    pub fn with_hosted(mut self, hosted: &[String]) -> Self {
        self.hosted = hosted
            .iter()
            .map(|origin| origin.trim_end_matches('/').to_owned())
            .collect();
        self
    }

    fn is_hosted(&self, origin: &str) -> bool {
        self.hosted.iter().any(|allowed| allowed == origin)
    }
}

/// The request's `Origin`, when it is text.
fn origin_of(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
}

#[derive(Clone, Debug)]
struct AppState {
    engine: EngineHandle,
    allow: Arc<Allowlist>,
    daemon_version: &'static str,
    video: Arc<crate::video::hub::Hub>,
    interfaces: Arc<Interfaces>,
    files: crate::vehicle_files::Store,
}

/// Builds the router. The interface (S109) is served from what
/// `interfaces` chose: a downloaded one, `--ui-dir`, or the copy built in.
pub fn router(
    engine: EngineHandle,
    allow: Allowlist,
    interfaces: Arc<Interfaces>,
    video: Arc<crate::video::hub::Hub>,
    files: crate::vehicle_files::Store,
) -> Router {
    let state = AppState {
        engine,
        allow: Arc::new(allow),
        daemon_version: env!("CARGO_PKG_VERSION"),
        video,
        interfaces,
        files,
    };
    let api = Router::new()
        .route("/api/health", get(health))
        .route("/api/browser", get(browser_found))
        .route("/api/open-in-chromium", post(open_in_chromium))
        .route("/api/desktop-app", get(desktop_app))
        .route("/api/desktop-app/open", post(open_in_desktop_app))
        .route("/api/session", get(session_status).post(session_bootstrap))
        .route("/api/pairing", post(pairing_start))
        .route("/api/pairing/{id}", get(pairing_poll))
        .route("/rpc", get(rpc))
        .route("/video", get(video_socket))
        .route("/recordings/{file}", get(recording))
        .route("/api/vehicle-files/{file}", get(vehicle_file))
        // A page's start (S109): never with a session, since the pairing
        // screen is a start too; the guard keeps other sites out.
        .route("/interface/ready", post(interface_ready))
        .route("/interface/boot-failed", post(interface_boot_failed))
        .fallback(interface);
    api.layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

/// Refuses foreign `Host` headers (DNS rebinding) and foreign `Origin`s
/// (other sites). Requests without an `Origin` are not from a page script.
async fn guard(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let host_ok = headers
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        .is_some_and(|host| {
            state
                .allow
                .hosts
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(host))
        });
    if !host_ok {
        return (StatusCode::FORBIDDEN, "host not allowed").into_response();
    }
    let hosted = match headers.get(header::ORIGIN) {
        Some(origin) => {
            let origin = origin.to_str().unwrap_or_default().to_owned();
            if state.allow.is_hosted(&origin) {
                Some(origin)
            } else if state.allow.origins.contains(&origin) {
                None
            } else {
                return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
            }
        }
        None if request.method() != Method::GET && request.method() != Method::HEAD => {
            return (StatusCode::FORBIDDEN, "origin required").into_response();
        }
        None => None,
    };
    let Some(origin) = hosted else {
        return next.run(request).await;
    };
    // Another site: CORS for it alone, and the preflight Chrome sends before
    // a public page may reach this computer.
    let mut response = if request.method() == Method::OPTIONS {
        let mut preflight = StatusCode::NO_CONTENT.into_response();
        let headers = preflight.headers_mut();
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, POST"),
        );
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("authorization, content-type"),
        );
        headers.insert(
            header::ACCESS_CONTROL_MAX_AGE,
            HeaderValue::from_static("600"),
        );
        headers.insert(
            "access-control-allow-private-network",
            HeaderValue::from_static("true"),
        );
        preflight
    } else {
        next.run(request).await
    };
    if let Ok(value) = HeaderValue::from_str(&origin) {
        let headers = response.headers_mut();
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
        headers.append(header::VARY, HeaderValue::from_static("origin"));
    }
    response
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    Json(json!({ "daemonVersion": state.daemon_version }))
}

/// The Chromium browser UXDuck's pages open in, for the page Safari and
/// Firefox show (S107, ADR-0025 D2): `{"chromium": "Chrome"}`, or null
/// when there is none. Not for other sites.
async fn browser_found(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if origin_of(&headers).is_some_and(|o| state.allow.is_hosted(o)) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let found = tokio::task::spawn_blocking(crate::adapters::app_browser::chromium)
        .await
        .ok()
        .flatten();
    Json(json!({ "chromium": found.map(|browser| browser.name) })).into_response()
}

/// "Open in Chrome" on that page: the app, already paired, in the Chromium
/// browser found. Only from UXDuck's own pages, which the guard has
/// checked by their `Origin`.
async fn open_in_chromium(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if origin_of(&headers).is_none_or(|o| state.allow.is_hosted(o)) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(url) = state.engine.run(uxduck_engine::Engine::bootstrap_url).await else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let opened = tokio::task::spawn_blocking(move || {
        let browser = crate::adapters::app_browser::chromium()
            .ok_or_else(|| "No Chrome, Edge or Brave was found on this computer.".to_owned())?;
        match browser.open(&url) {
            Ok(()) => Ok(browser.name),
            Err(error) => Err(format!("{} did not open: {error}", browser.name)),
        }
    })
    .await
    .unwrap_or_else(|error| Err(error.to_string()));
    match opened {
        Ok(name) => Json(json!({ "opened": name })).into_response(),
        Err(error) => (StatusCode::CONFLICT, Json(json!({ "error": error }))).into_response(),
    }
}

/// The desktop app for the page Safari and Firefox show (S110): offered,
/// its sizes, downloading or installed. Only UXDuck's own pages.
async fn desktop_app(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if origin_of(&headers).is_some_and(|o| state.allow.is_hosted(o)) {
        return StatusCode::FORBIDDEN.into_response();
    }
    match state.engine.run(|e| e.desktop_app_status(false)).await {
        Ok(status) => Json(status).into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

/// "Open as a desktop app" there: downloads it, after the page showed its
/// size, and once it is here opens UXDuck in it.
async fn open_in_desktop_app(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if origin_of(&headers).is_none_or(|o| state.allow.is_hosted(o)) {
        return StatusCode::FORBIDDEN.into_response();
    }
    match state
        .engine
        .run(uxduck_engine::Engine::open_in_desktop_app)
        .await
    {
        Ok(Ok(status)) => Json(status).into_response(),
        Ok(Err(error)) => (StatusCode::CONFLICT, Json(json!({ "error": error }))).into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, token)| token.to_owned())
}

fn set_cookie(token: &str) -> (header::HeaderName, HeaderValue) {
    let value = format!("{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/");
    (
        header::SET_COOKIE,
        HeaderValue::from_str(&value).unwrap_or_else(|_| HeaderValue::from_static("")),
    )
}

/// The session token another site sent instead of a cookie: as a
/// subprotocol on `/rpc`, or as `Authorization: Bearer`.
fn sent_token(headers: &HeaderMap) -> Option<String> {
    let protocols = headers
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .find_map(|protocol| {
            protocol
                .trim()
                .strip_prefix(TOKEN_PROTOCOL)
                .map(str::to_owned)
        });
    protocols.or_else(|| {
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|token| token.trim().to_owned())
    })
}

/// The browser's session: its cookie on this site, or for another site the
/// token it sent, which counts only from the origin that paired.
async fn current_session(state: &AppState, headers: &HeaderMap) -> Option<uxduck_engine::Session> {
    if let Some(origin) = origin_of(headers).filter(|o| state.allow.is_hosted(o)) {
        let token = sent_token(headers)?;
        let session = state.engine.session(token).await.ok().flatten()?;
        return (session.origin == origin).then_some(session);
    }
    let token = cookie_token(headers)?;
    state.engine.session(token).await.ok().flatten()
}

async fn session_status(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    Json(
        json!({ "paired": current_session(&state, &headers).await.is_some(), "daemonVersion": state.daemon_version }),
    )
}

#[derive(Deserialize)]
struct Bootstrap {
    bootstrap: String,
}

/// Exchanges a one-time bootstrap token (from `uxduck open`) for a session.
async fn session_bootstrap(State(state): State<AppState>, Json(body): Json<Bootstrap>) -> Response {
    match state.engine.exchange_bootstrap(body.bootstrap).await {
        Ok(Some(session)) => {
            ([set_cookie(&session)], Json(json!({ "paired": true }))).into_response()
        }
        Ok(None) => (StatusCode::FORBIDDEN, Json(json!({ "paired": false }))).into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

/// Starts pairing: the page shows the code, an operator approves it.
async fn pairing_start(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    match state.engine.request_pairing(origin, user_agent).await {
        Ok((id, code)) => Json(json!({ "id": id, "code": code })).into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

async fn pairing_poll(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    match state.engine.poll_pairing(id).await {
        Ok(PairingState::Pending { code }) => {
            Json(json!({ "state": "pending", "code": code })).into_response()
        }
        // Another site cannot use the cookie: it keeps the token itself.
        Ok(PairingState::Approved { session })
            if origin_of(&headers).is_some_and(|o| state.allow.is_hosted(o)) =>
        {
            Json(json!({ "state": "approved", "token": session })).into_response()
        }
        Ok(PairingState::Approved { session }) => {
            ([set_cookie(&session)], Json(json!({ "state": "approved" }))).into_response()
        }
        Ok(PairingState::Denied) => Json(json!({ "state": "denied" })).into_response(),
        Ok(PairingState::Unknown) => {
            (StatusCode::NOT_FOUND, Json(json!({ "state": "unknown" }))).into_response()
        }
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

/// JSON-RPC over WebSocket, one text frame per message. Needs a session.
async fn rpc(
    State(state): State<AppState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let Some(session) = current_session(&state, &headers).await else {
        return (StatusCode::UNAUTHORIZED, "pair this browser first").into_response();
    };
    let engine = state.engine.clone();
    // A browser that offered subprotocols closes unless one is echoed.
    upgrade.protocols([PROTOCOL]).on_upgrade(move |socket| {
        serve_websocket(
            socket,
            engine,
            Trust::PairedBrowser {
                origin: session.origin,
            },
        )
    })
}

#[derive(Deserialize)]
struct VideoQuery {
    token: String,
}

/// A watched camera's pictures (ADR-0017 V6). The token is the session: it
/// came from `video/watch` on a paired connection, opens one socket once,
/// and lapses after 30 s, so the shell's main process can use it too.
async fn video_socket(
    State(state): State<AppState>,
    Query(query): Query<VideoQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let Some((_, watcher)) = state.video.open(&query.token) else {
        return (StatusCode::UNAUTHORIZED, "watch the source first").into_response();
    };
    let (hub, engine) = (state.video.clone(), state.engine.clone());
    upgrade.on_upgrade(move |socket| {
        crate::video::socket::serve(socket, query.token, watcher, hub, engine)
    })
}

/// A recording to play (S40), with the token `video/playback` gave. Range
/// requests work, so a `<video>` can seek.
async fn recording(
    State(state): State<AppState>,
    Path(file): Path<String>,
    Query(query): Query<VideoQuery>,
    request: Request,
) -> Response {
    let Some(id) = state.video.playback(&query.token) else {
        return (StatusCode::UNAUTHORIZED, "ask for the recording first").into_response();
    };
    if file != format!("{id}.mp4") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = crate::video::recorder::mp4_path(state.video.dir(), &id);
    match ServeFile::new(path).try_call(request).await {
        Ok(response) => response.map(Body::new).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// A copy of a vehicle's file (S121), once, with the address `files/save`
/// gave: as an attachment, under the file's own name.
async fn vehicle_file(
    State(state): State<AppState>,
    Path(_file): Path<String>,
    Query(query): Query<VideoQuery>,
    request: Request,
) -> Response {
    let Some((path, name)) = state.files.take(&query.token) else {
        return (
            StatusCode::UNAUTHORIZED,
            "save the file from the Files tab again",
        )
            .into_response();
    };
    let quoted: String = name
        .chars()
        .filter(|c| !c.is_control() && *c != '"' && *c != '\\')
        .collect();
    match ServeFile::new(path).try_call(request).await {
        Ok(response) => {
            let mut response = response.map(Body::new).into_response();
            if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{quoted}\""))
            {
                response
                    .headers_mut()
                    .insert(header::CONTENT_DISPOSITION, value);
            }
            response
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn serve_websocket(socket: WebSocket, engine: EngineHandle, trust: Trust) {
    let (mut sink, mut stream) = socket.split();
    let (outbox, mut outgoing) = futures::channel::mpsc::unbounded::<String>();
    let Ok(connection) = engine.connect(Transport::WebSocket, trust, outbox).await else {
        return;
    };
    let writing = tokio::spawn(async move {
        while let Some(text) = outgoing.next().await {
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });
    while let Some(Ok(message)) = stream.next().await {
        match message {
            Message::Text(text) => engine.receive(connection, text.to_string()),
            Message::Close(_) => break,
            // Binary channels arrive with the inspector's raw frames (S5).
            _ => {}
        }
    }
    engine.disconnect(connection);
    writing.abort();
}

async fn interface(State(state): State<AppState>, request: Request) -> Response {
    crate::interface::serve(State(state.interfaces), request).await
}

async fn interface_ready(
    State(state): State<AppState>,
    report: Json<crate::interface::PageReport>,
) -> StatusCode {
    crate::interface::ready(State(state.interfaces), report)
}

async fn interface_boot_failed(
    State(state): State<AppState>,
    report: Json<crate::interface::PageReport>,
) -> StatusCode {
    crate::interface::boot_failed(State(state.interfaces), report)
}
