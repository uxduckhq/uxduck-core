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
//!
//! "Phones and tablets on this network" (S97) adds a second listener on one
//! of the computer's addresses, built by `network_router`: the same page,
//! pairing and `/rpc`, without what only this computer may ask for (opening
//! a browser or the desktop app, bootstrap tokens). Its sessions count only
//! there, and its pages are never operators.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
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
    /// `Host` header values, such as `127.0.0.1:45000`.
    pub hosts: Vec<String>,
    /// `Origin` header values, such as `http://localhost:45101`.
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

    /// The network listener's: its own address and port, nothing else.
    #[must_use]
    pub fn network(origin: &Network) -> Self {
        Self {
            hosts: vec![origin.host.clone()],
            origins: vec![origin.origin.clone()],
            hosted: Vec::new(),
        }
    }
}

/// The network listener (S97): where it is, and a signal that ends its
/// connections when it stops.
#[derive(Clone, Debug)]
pub struct Network {
    /// `192.168.1.20:45000`.
    pub host: String,
    /// `http://192.168.1.20:45000`.
    pub origin: String,
    /// Turns true when the listener stops: open sockets close with it.
    pub stopped: tokio::sync::watch::Receiver<bool>,
    /// This computer's own page, such as `http://127.0.0.1:45000/`: the
    /// referrer the map keys allow, sent with relayed map requests.
    pub referrer: String,
}

impl Network {
    #[must_use]
    pub fn new(
        address: std::net::SocketAddr,
        stopped: tokio::sync::watch::Receiver<bool>,
        referrer: String,
    ) -> Self {
        let host = address.to_string();
        Self {
            origin: format!("http://{host}"),
            host,
            stopped,
            referrer,
        }
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
    /// In development, the pairing screen shows the command for this
    /// daemon rather than for the installed `uxduck`.
    dev_cli: Option<Arc<crate::paths::DevCli>>,
    /// Set on the network listener (S97).
    network: Option<Network>,
}

/// Builds the router. The interface (S109) is served from what
/// `interfaces` chose: a downloaded one, `--ui-dir`, or the copy built in.
pub fn router(
    engine: EngineHandle,
    allow: Allowlist,
    interfaces: Arc<Interfaces>,
    video: Arc<crate::video::hub::Hub>,
    files: crate::vehicle_files::Store,
    dev_cli: Option<crate::paths::DevCli>,
) -> Router {
    let state = AppState {
        engine,
        allow: Arc::new(allow),
        daemon_version: env!("CARGO_PKG_VERSION"),
        video,
        interfaces,
        files,
        dev_cli: dev_cli.map(Arc::new),
        network: None,
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
        .route("/photos/{file}", get(photo))
        .route("/api/vehicle-files/{file}", get(vehicle_file))
        // A page's start (S109): never with a session, since the pairing
        // screen is a start too; the guard keeps other sites out.
        .route("/interface/ready", post(interface_ready))
        .route("/interface/boot-failed", post(interface_boot_failed))
        .fallback(interface);
    api.layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

/// The network listener's router (S97): the page, pairing, `/rpc` and the
/// files a paired page asks for. Serve it with `ConnectInfo<SocketAddr>`,
/// so a pairing question names the device's address.
pub fn network_router(
    engine: EngineHandle,
    network: Network,
    interfaces: Arc<Interfaces>,
    video: Arc<crate::video::hub::Hub>,
    files: crate::vehicle_files::Store,
) -> Router {
    let state = AppState {
        engine,
        allow: Arc::new(Allowlist::network(&network)),
        daemon_version: env!("CARGO_PKG_VERSION"),
        video,
        interfaces,
        files,
        dev_cli: None,
        network: Some(network),
    };
    let api = Router::new()
        .route("/api/health", get(health))
        .route("/api/session", get(session_status))
        .route("/api/pairing", post(pairing_start))
        .route("/api/pairing/{id}", get(pairing_poll))
        .route("/rpc", get(rpc))
        .route("/api/maps/relay", get(map_relay))
        // Not live video: a page on plain HTTP cannot decode it (S97).
        .route("/recordings/{file}", get(recording))
        .route("/photos/{file}", get(photo))
        .route("/api/vehicle-files/{file}", get(vehicle_file))
        .route("/interface/ready", post(interface_ready))
        .route("/interface/boot-failed", post(interface_boot_failed))
        .fallback(network_interface);
    api.layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

/// Where a page on the network may have its map requests relayed (S97):
/// Esri's basemaps and elevation, and Google's 3D tiles.
fn relayed_map_host(host: &str) -> bool {
    host.ends_with(".arcgis.com")
        || host.ends_with(".arcgisonline.com")
        || host == "tile.googleapis.com"
}

#[derive(Deserialize)]
struct MapRelay {
    url: String,
}

/// A map request from a paired page on the network (S97). The map keys are
/// locked to the sites allowed to use them, and a phone's page is at this
/// computer's network address, which cannot be among them; so this computer
/// asks instead, as its own page, and passes the answer on.
async fn map_relay(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(MapRelay { url }): Query<MapRelay>,
) -> Response {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let Some(network) = &state.network else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if current_session(&state, &headers).await.is_none() {
        return (StatusCode::UNAUTHORIZED, "pair this browser first").into_response();
    }
    let Ok(target) = reqwest::Url::parse(&url) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if target.scheme() != "https" || !target.host_str().is_some_and(relayed_map_host) {
        return (StatusCode::FORBIDDEN, "only map requests are relayed").into_response();
    }
    let client = CLIENT.get_or_init(reqwest::Client::new);
    let mut asked = client
        .get(target)
        .header(header::REFERER, network.referrer.as_str());
    if let Some(accept) = headers.get(header::ACCEPT) {
        asked = asked.header(header::ACCEPT, accept);
    }
    let Ok(answer) = asked.send().await else {
        return (
            StatusCode::BAD_GATEWAY,
            "This computer couldn't reach the map service.",
        )
            .into_response();
    };
    let status = answer.status();
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    for name in [
        header::CONTENT_TYPE,
        header::CACHE_CONTROL,
        header::ETAG,
        header::LAST_MODIFIED,
        header::EXPIRES,
    ] {
        if let Some(value) = answer.headers().get(&name) {
            response.headers_mut().insert(name, value.clone());
        }
    }
    *response.body_mut() = Body::from_stream(answer.bytes_stream());
    response
}

/// The page on the network listener (S97). In development Vite serves it on
/// this computer's loopback only, so it is relayed from there with its
/// hot-reload socket; pairing and `/rpc` stay on this listener's own rules.
async fn network_interface(State(state): State<AppState>, request: Request) -> Response {
    let Some(dev) = state.interfaces.dev_origin().map(str::to_owned) else {
        return interface(State(state), request).await;
    };
    let upgrade = request
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"));
    if upgrade {
        dev_socket(state, &dev, request).await
    } else {
        dev_page(&dev, request).await
    }
}

/// A page or module from the dev server, as it answered.
async fn dev_page(dev: &str, request: Request) -> Response {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let path = request
        .uri()
        .path_and_query()
        .map_or("/", axum::http::uri::PathAndQuery::as_str);
    let client = CLIENT.get_or_init(reqwest::Client::new);
    let mut asked = client.request(request.method().clone(), format!("{dev}{path}"));
    for name in [
        header::ACCEPT,
        header::IF_NONE_MATCH,
        header::IF_MODIFIED_SINCE,
    ] {
        if let Some(value) = request.headers().get(&name) {
            asked = asked.header(name, value);
        }
    }
    let Ok(answer) = asked.send().await else {
        return (
            StatusCode::BAD_GATEWAY,
            "The development server isn't running on this computer.",
        )
            .into_response();
    };
    let status = answer.status();
    let mut headers = answer.headers().clone();
    for hop in [
        header::CONNECTION,
        header::TRANSFER_ENCODING,
        header::CONTENT_LENGTH,
        header::CONTENT_ENCODING,
    ] {
        headers.remove(hop);
    }
    let body = answer.bytes().await.unwrap_or_default();
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// Vite's hot-reload socket, relayed to the dev server until either side
/// closes or the network listener stops.
async fn dev_socket(state: AppState, dev: &str, request: Request) -> Response {
    use axum::extract::FromRequestParts;
    use tokio_tungstenite::tungstenite::Message as Relayed;

    let (mut parts, _) = request.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map_or("/", axum::http::uri::PathAndQuery::as_str)
        .to_owned();
    let Ok(upgrade) = WebSocketUpgrade::from_request_parts(&mut parts, &state).await else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let target = format!("{}{path}", dev.replacen("http", "ws", 1));
    let Ok(mut outgoing) = target.into_client_request() else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    outgoing.headers_mut().insert(
        header::SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_static("vite-hmr"),
    );
    let Ok((server, _)) = tokio_tungstenite::connect_async(outgoing).await else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let stopped = state.network.as_ref().map(|n| n.stopped.clone());
    upgrade
        .protocols(["vite-hmr"])
        .on_upgrade(move |page| async move {
            let (mut to_page, mut from_page) = page.split();
            let (mut to_server, mut from_server) = server.split();
            let down = async {
                while let Some(Ok(message)) = from_server.next().await {
                    let message = match message {
                        Relayed::Text(text) => Message::Text(text.as_str().into()),
                        Relayed::Binary(bytes) => Message::Binary(bytes),
                        Relayed::Close(_) => break,
                        _ => continue,
                    };
                    if to_page.send(message).await.is_err() {
                        break;
                    }
                }
            };
            let up = async {
                while let Some(Ok(message)) = from_page.next().await {
                    let message = match message {
                        Message::Text(text) => Relayed::Text(text.as_str().into()),
                        Message::Binary(bytes) => Relayed::Binary(bytes),
                        Message::Close(_) => break,
                        _ => continue,
                    };
                    if to_server.send(message).await.is_err() {
                        break;
                    }
                }
            };
            tokio::select! {
                () = down => {}
                () = up => {}
                () = listener_stopped(stopped) => {}
            }
        })
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

/// The browser's session and its token: its cookie on this site, or for
/// another site the token it sent, which counts only from the origin that
/// paired. A session paired on the network listener counts only there, and
/// no other session counts there (S97).
async fn current_session(
    state: &AppState,
    headers: &HeaderMap,
) -> Option<(String, uxduck_engine::Session)> {
    if let Some(origin) = origin_of(headers).filter(|o| state.allow.is_hosted(o)) {
        let token = sent_token(headers)?;
        let session = state.engine.session(token.clone()).await.ok().flatten()?;
        return (session.origin == origin).then_some((token, session));
    }
    let token = cookie_token(headers)?;
    let session = state.engine.session(token.clone()).await.ok().flatten()?;
    let here = match &state.network {
        // And not forgotten at the computer since.
        Some(network) if session.origin == network.origin => {
            let client = uxduck_engine::pairing::network_client(&token);
            state
                .engine
                .run(move |e| e.state().clients.contains_key(&client))
                .await
                .unwrap_or(false)
        }
        Some(_) => false,
        None => !uxduck_engine::pairing::network_origin(&session.origin),
    };
    here.then_some((token, session))
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

/// Starts pairing: the page shows the code, an operator approves it. On the
/// network listener the question names the device's address, and only a
/// few devices may wait at once (S97).
async fn pairing_start(State(state): State<AppState>, request: Request) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .map(|ConnectInfo(address)| address.ip().to_string());
    let headers = request.headers();
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let asked = if state.network.is_some() {
        let Some(peer) = peer else {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        state
            .engine
            .run(move |e| e.request_network_pairing(&origin, user_agent, peer))
            .await
    } else {
        state
            .engine
            .request_pairing(origin, user_agent)
            .await
            .map(Some)
    };
    match asked {
        Ok(None) => (
            StatusCode::TOO_MANY_REQUESTS,
            "Too many devices are asking to connect. Try again in a minute.",
        )
            .into_response(),
        Ok(Some((id, code))) => {
            let command = state
                .dev_cli
                .as_ref()
                .map(|cli| cli.rpc_call("pairing/approve", &json!({ "code": code })));
            // The page says to answer at the duck when there is one.
            let tray = state.engine.run(|e| e.has_tray()).await.unwrap_or(false);
            Json(json!({ "id": id, "code": code, "command": command, "tray": tray }))
                .into_response()
        }
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
        Ok(PairingState::Approved { session, .. })
            if origin_of(&headers).is_some_and(|o| state.allow.is_hosted(o)) =>
        {
            Json(json!({ "state": "approved", "token": session })).into_response()
        }
        Ok(PairingState::Approved { session, .. }) => {
            ([set_cookie(&session)], Json(json!({ "state": "approved" }))).into_response()
        }
        Ok(PairingState::Denied) => Json(json!({ "state": "denied" })).into_response(),
        Ok(PairingState::Unknown) => {
            (StatusCode::NOT_FOUND, Json(json!({ "state": "unknown" }))).into_response()
        }
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

/// The largest JSON-RPC message from a paired page.
const RPC_MESSAGE: usize = 48 * 1024 * 1024;

/// JSON-RPC over WebSocket, one text frame per message. Needs a session.
async fn rpc(
    State(state): State<AppState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let Some((token, session)) = current_session(&state, &headers).await else {
        return (StatusCode::UNAUTHORIZED, "pair this browser first").into_response();
    };
    let engine = state.engine.clone();
    let (trust, stopped) = match &state.network {
        Some(network) => (
            Trust::NetworkBrowser {
                origin: session.origin,
                client: uxduck_engine::pairing::network_client(&token),
            },
            Some(network.stopped.clone()),
        ),
        None => (
            Trust::PairedBrowser {
                origin: session.origin,
            },
            None,
        ),
    };
    // A browser that offered subprotocols closes unless one is echoed. A
    // browser sends a message as one frame, and a feedback report with its
    // screenshot and 25 MB of attached files is about 38 MB in base64.
    upgrade
        .protocols([PROTOCOL])
        .max_frame_size(RPC_MESSAGE)
        .max_message_size(RPC_MESSAGE)
        .on_upgrade(move |socket| serve_websocket(socket, engine, trust, stopped))
}

/// Resolves once the network listener a socket came through stops (S97);
/// never for one from this computer's own listener.
async fn listener_stopped(stopped: Option<tokio::sync::watch::Receiver<bool>>) {
    match stopped {
        Some(mut stopped) => {
            let _ = stopped.wait_for(|stopped| *stopped).await;
        }
        None => std::future::pending().await,
    }
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

/// Recordings converted for playback one at a time (S151).
static CONVERTING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A recording to play (S40), with the token `video/playback` gave. Range
/// requests work, so a `<video>` can seek; one kept as MPEG-TS plays as its
/// MP4 copy, and gives its original as `<id>.ts` (S151).
async fn recording(
    State(state): State<AppState>,
    Path(file): Path<String>,
    Query(query): Query<VideoQuery>,
    request: Request,
) -> Response {
    let Some((id, name)) = state.video.playback(&query.token) else {
        return (StatusCode::UNAUTHORIZED, "ask for the recording first").into_response();
    };
    let dir = state.video.dir().to_owned();
    let kept_as_ts = std::path::Path::new(&name)
        .extension()
        .is_some_and(|e| e == "ts");
    let path = if kept_as_ts && file == format!("{id}.ts") {
        // The original, for Save… (S151).
        dir.join(&name)
    } else if file != format!("{id}.mp4") {
        return StatusCode::NOT_FOUND.into_response();
    } else if kept_as_ts {
        // Played as MP4: the copy in the cache, made now if it isn't there.
        let mp4 = crate::video::playback::cached(&crate::video::playback::cache_dir(&dir), &id);
        let _one_at_a_time = CONVERTING.lock().await;
        let (ts, to) = (dir.join(&name), mp4.clone());
        let made = tokio::task::spawn_blocking(move || {
            crate::video::playback::convert(&ts, &to)?;
            crate::video::playback::mark_used(&to);
            Ok::<(), std::io::Error>(())
        })
        .await;
        if !matches!(made, Ok(Ok(()))) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "the recording could not be prepared to play",
            )
                .into_response();
        }
        mp4
    } else {
        dir.join(&name)
    };
    match ServeFile::new(path).try_call(request).await {
        Ok(response) => response.map(Body::new).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// A photo from the video (S144), with the address `video/photo/show` gave.
async fn photo(
    State(state): State<AppState>,
    Path(file): Path<String>,
    Query(query): Query<VideoQuery>,
    request: Request,
) -> Response {
    let Some((id, name)) = state.video.photo(&query.token) else {
        return (StatusCode::UNAUTHORIZED, "ask for the photo first").into_response();
    };
    if file != format!("{id}.jpg") {
        return StatusCode::NOT_FOUND.into_response();
    }
    match ServeFile::new(state.video.dir().join(name))
        .try_call(request)
        .await
    {
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

async fn serve_websocket(
    socket: WebSocket,
    engine: EngineHandle,
    trust: Trust,
    stopped: Option<tokio::sync::watch::Receiver<bool>>,
) {
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
    let stopped = listener_stopped(stopped);
    tokio::pin!(stopped);
    let mut writing = writing;
    loop {
        let message = tokio::select! {
            message = stream.next() => message,
            () = &mut stopped => break,
            // The engine let the connection go (a phone unpaired, S97).
            _ = &mut writing => break,
        };
        match message {
            Some(Ok(Message::Text(text))) => engine.receive(connection, text.to_string()),
            Some(Ok(Message::Close(_)) | Err(_)) | None => break,
            // Binary channels arrive with the inspector's raw frames (S5).
            Some(Ok(_)) => {}
        }
    }
    engine.disconnect(connection);
    // The sink goes with the writer: the socket closes.
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
