//! People away, as the relay brings their requests (S80, ADR-0003 C9):
//! each request's grant is checked here, against the API's keys, and each
//! person gets one `remote-operator` connection to the engine with
//! `Trust::Relay`, so their commands take the computer's own command path.
//! The engine's answers go back to the relay with the relay's own ids.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use serde_json::{Value, json};
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::policy::admission::Trust;
use uxduck_engine::ConnectionId;

use crate::daemon::engine_task::EngineHandle;
use crate::grants::{Keys, kid_of, verify};

/// Keys are fetched again for an unknown key id at most this often.
const KEYS_EVERY: Duration = Duration::from_secs(60);

pub struct RemoteUsers {
    engine: EngineHandle,
    data_dir: PathBuf,
    api: String,
    device: String,
    keys: Keys,
    fetched: Option<tokio::time::Instant>,
    /// Each person's connection, by account id.
    users: HashMap<String, ConnectionId>,
    to_relay: UnboundedSender<String>,
}

fn refusal(id: &Value, why: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32001, "message": why, "data": { "kind": "capability-denied" } }
    })
    .to_string()
}

impl RemoteUsers {
    /// And where the engine's answers for the relay come out.
    pub fn new(
        engine: EngineHandle,
        data_dir: PathBuf,
        api: String,
        device: String,
    ) -> (Self, UnboundedReceiver<String>) {
        let (to_relay, answers) = unbounded();
        let keys = Keys::load(&data_dir);
        (
            Self {
                engine,
                data_dir,
                api,
                device,
                keys,
                fetched: None,
                users: HashMap::new(),
                to_relay,
            },
            answers,
        )
    }

    /// The API's keys, fetched and kept on disk.
    pub async fn refresh_keys(&mut self) {
        self.fetched = Some(tokio::time::Instant::now());
        let Some(client) = crate::online::client(Duration::from_secs(20)) else {
            return;
        };
        let Ok(answer) = client
            .get(format!("{}/v1/computers/keys", self.api))
            .send()
            .await
        else {
            return;
        };
        if let Ok(keys) = answer.json::<Keys>().await
            && !keys.keys.is_empty()
        {
            keys.save(&self.data_dir);
            self.keys = keys;
        }
    }

    /// A request from the relay. The answer, when there is one at once;
    /// otherwise it comes out of the engine later.
    pub async fn request(&mut self, message: &Value) -> Option<String> {
        let id = &message["id"];
        let params = &message["params"];
        let method = message["method"].as_str()?;
        if method == "edge/left" {
            if let Some(user) = params["userId"].as_str()
                && let Some(connection) = self.users.remove(user)
            {
                self.engine.disconnect(connection);
            }
            return None;
        }
        if !matches!(method, "edge/command" | "edge/lease" | "edge/receipt") || id.is_null() {
            return None;
        }
        let token = params["grant"].as_str().unwrap_or_default();
        if let Some(kid) = kid_of(token)
            && !self.keys.has(&kid)
            && self.fetched.is_none_or(|at| at.elapsed() > KEYS_EVERY)
        {
            self.refresh_keys().await;
        }
        let grant = match verify(token, &self.keys, &self.device, crate::edge::now_ms()) {
            Ok(grant) => grant,
            Err(why) => return Some(refusal(id, &why)),
        };
        if method == "edge/receipt" {
            let (user, command) = (
                grant.sub.clone(),
                params["commandId"].as_str().unwrap_or_default().to_owned(),
            );
            let receipt = self
                .engine
                .run(move |e| e.remote_receipt(&user, &command))
                .await
                .ok()
                .flatten();
            return Some(json!({ "jsonrpc": "2.0", "id": id, "result": receipt }).to_string());
        }
        let Some(connection) = self.connection(&grant.sub, &grant.name).await else {
            return Some(refusal(id, "the daemon is stopping"));
        };
        let (engine_method, engine_params) = if method == "edge/command" {
            (
                "command/send",
                json!({
                    "vehicleId": params["vehicleId"],
                    "command": params["command"],
                    "confirm": params["confirm"].as_bool().unwrap_or(false),
                    "commandId": params["commandId"],
                    "sentAtMs": params["sentAtMs"],
                }),
            )
        } else if params["take"] == true {
            ("lease/take", json!({ "vehicleId": params["vehicleId"] }))
        } else {
            ("lease/release", json!({ "vehicleId": params["vehicleId"] }))
        };
        // The relay's id goes through the engine and back, unchanged.
        let frame =
            json!({ "jsonrpc": "2.0", "id": id, "method": engine_method, "params": engine_params });
        self.engine.receive(connection, frame.to_string());
        None
    }

    /// The person's connection, opened and introduced the first time.
    async fn connection(&mut self, user_id: &str, name: &str) -> Option<ConnectionId> {
        if let Some(connection) = self.users.get(user_id) {
            return Some(*connection);
        }
        let (outbox, mut frames) = unbounded::<String>();
        let connection = self
            .engine
            .connect(
                Transport::Internal,
                Trust::Relay {
                    user_id: user_id.to_owned(),
                    name: name.to_owned(),
                },
                outbox,
            )
            .await
            .ok()?;
        let to_relay = self.to_relay.clone();
        tokio::spawn(async move {
            while let Some(text) = frames.next().await {
                let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                // Answers to the relay's requests; not the introduction, not
                // notifications.
                if frame.get("id").is_some_and(|id| id != "init") && frame.get("method").is_none() {
                    let _ = to_relay.unbounded_send(text);
                }
            }
        });
        let introduce = json!({
            "jsonrpc": "2.0",
            "id": "init",
            "method": "initialize",
            "params": {
                "clientInfo": { "name": format!("{name} (from away)"), "version": env!("CARGO_PKG_VERSION"), "kind": "remote-operator" },
                "capabilities": { "protocolVersion": uxduck_contracts::version::PROTOCOL_VERSION }
            }
        });
        self.engine.receive(connection, introduce.to_string());
        self.users.insert(user_id.to_owned(), connection);
        Some(connection)
    }

    /// The relay connection ended: everyone away is gone with it.
    pub fn close(&mut self) {
        for (_, connection) in self.users.drain() {
            self.engine.disconnect(connection);
        }
    }
}
