//! Shared local-runtime transport and authenticated, turn-scoped UXDuck tools.
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use base64::Engine as _;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::Semaphore,
};
use uxduck_agent::{Host, Tool};
pub(super) async fn send(
    stdin: &mut tokio::process::ChildStdin,
    value: &Value,
) -> Result<(), String> {
    let bytes = format!("{value}\n");
    stdin
        .write_all(bytes.as_bytes())
        .await
        .map_err(|_| "The agent runtime stopped accepting messages.".to_owned())
}
/// Buffer lives across select cancellation: steering cannot discard a partial JSON frame.
pub(super) struct JsonReader {
    reader: BufReader<tokio::process::ChildStdout>,
    buffer: Vec<u8>,
}
impl JsonReader {
    pub fn new(stdout: tokio::process::ChildStdout) -> Self {
        Self {
            reader: BufReader::new(stdout),
            buffer: Vec::new(),
        }
    }
}
pub(super) async fn line(reader: &mut JsonReader) -> Result<Option<Value>, String> {
    loop {
        let bytes = reader
            .reader
            .fill_buf()
            .await
            .map_err(|_| "Could not read the agent runtime output.".to_owned())?;
        if bytes.is_empty() {
            return if reader.buffer.is_empty() {
                Ok(None)
            } else {
                Err("The agent runtime ended mid-message.".into())
            };
        }
        let end = bytes.iter().position(|b| *b == b'\n');
        let n = end.map_or(bytes.len(), |n| n + 1);
        if reader.buffer.len() + n > 8 * 1024 * 1024 {
            return Err("The agent runtime sent an oversized message.".into());
        }
        reader.buffer.extend_from_slice(&bytes[..n]);
        reader.reader.consume(n);
        if end.is_some() {
            let result = serde_json::from_slice(&reader.buffer)
                .map(Some)
                .map_err(|_| {
                    "The agent runtime sent an invalid message. Update it and retry.".into()
                });
            reader.buffer.clear();
            return result;
        }
    }
}
struct Server {
    hold_tools: std::sync::atomic::AtomicBool,
    active: std::sync::atomic::AtomicBool,
    cancelled: tokio::sync::watch::Sender<bool>,
    host: Arc<dyn Host>,
    token: String,
    gate: Semaphore,
}
async fn mcp(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Json(msg): Json<Value>,
) -> Response {
    if !server.active.load(std::sync::atomic::Ordering::SeqCst)
        || headers.contains_key("origin")
        || headers.get("authorization").and_then(|v| v.to_str().ok())
            != Some(format!("Bearer {}", server.token).as_str())
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(id) = msg.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let result = match msg["method"].as_str().unwrap_or_default() {
        "initialize" => json!({"protocolVersion": "2025-03-26", "capabilities":{"tools":{}},"serverInfo":{"name":"uxduck","version":env!("CARGO_PKG_VERSION")}}),
        "ping" => json!({}),
        "tools/list" => json!({"tools":uxduck_agent::tools().into_iter().map(|t| json!({"name":t.name,"description":t.description,"inputSchema":t.parameters})).collect::<Vec<_>>()}),
        "tools/call" => {
            let Ok(_permit) = server.gate.try_acquire() else { return Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":"Another tool is running."}})).into_response(); };
            let name = msg["params"]["name"].as_str().unwrap_or_default();
            let Some(method) = Tool::method_for(&uxduck_agent::tools(), name) else { return Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":"Unknown tool."}})).into_response(); };
            let params = msg["params"].get("arguments").cloned().unwrap_or_else(|| json!({}));
            let mut cancelled = server.cancelled.subscribe();
            let item = server.host.tool_start(method.clone(),params.clone()).await;
            let result = if !server.active.load(std::sync::atomic::Ordering::SeqCst) || server.hold_tools.load(std::sync::atomic::Ordering::SeqCst) || server.host.inputs().pending() { Err(json!({"code":-32800,"message":"Superseded by an operator update."})) }
                else { tokio::select! {
                    biased;
                    _ = cancelled.wait_for(|stopped| *stopped) => Err(json!({"code":-32800,"message":"The turn was stopped."})),
                    result = server.host.call(method,params) => result,
                } };
            server.host.tool_finish(item,result.clone()).await;
            let (value, failed) = match result { Ok(v) => (v,false), Err(e) => (e,true) };
            json!({"content":[{"type":"text","text":value.to_string()}],"isError":failed})
        }
        _ => return Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Unsupported MCP method."}})).into_response(),
    };
    Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
}
struct RunningServer(tokio::task::JoinHandle<()>, Arc<Server>);
impl Drop for RunningServer {
    fn drop(&mut self) {
        self.1
            .active
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let _ = self.1.cancelled.send(true);
        self.0.abort();
    }
}

pub(super) struct Bridge {
    pub config: Value,
    server: RunningServer,
}
impl Bridge {
    pub fn hold_tools(&self, hold: bool) {
        self.server
            .1
            .hold_tools
            .store(hold, std::sync::atomic::Ordering::SeqCst);
    }
    pub async fn start(host: Arc<dyn Host>) -> Result<Self, String> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|_| "Could not start the local agent tool bridge.".to_owned())?;
        let address = listener
            .local_addr()
            .map_err(|_| "Could not find the agent tool bridge.".to_owned())?;
        let token = random()?;
        let state = Arc::new(Server {
            hold_tools: std::sync::atomic::AtomicBool::new(false),
            active: std::sync::atomic::AtomicBool::new(true),
            cancelled: tokio::sync::watch::channel(false).0,
            host: host.clone(),
            token: token.clone(),
            gate: Semaphore::new(1),
        });
        let router = Router::new()
            .route("/mcp", post(mcp))
            .layer(DefaultBodyLimit::max(256 * 1024))
            .with_state(state.clone());
        let server = RunningServer(
            tokio::spawn(async move {
                let _ = axum::serve(listener, router).await;
            }),
            state,
        );
        let config = json!({"type":"http","url":format!("http://{address}/mcp"),"headers":{"Authorization":format!("Bearer {token}")}});
        Ok(Self { config, server })
    }
}
pub fn random() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|_| "Could not generate a secure connection token.".to_owned())?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}
