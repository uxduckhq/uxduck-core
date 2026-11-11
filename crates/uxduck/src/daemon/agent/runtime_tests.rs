//! Runtime protocol fixtures: no paid requests, provider credentials, or vehicle commands.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    reason = "isolated protocol fixtures"
)]
use futures::{FutureExt, future::BoxFuture};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use uxduck_agent::{
    Host,
    input::{Input, Inputs},
};

#[derive(Default)]
pub(super) struct FixtureHost {
    pub events: Mutex<Vec<String>>,
    pub inputs: Inputs,
    pub steer: bool,
    pub block: bool,
    pub called: tokio::sync::Notify,
}
impl Host for FixtureHost {
    fn inputs(&self) -> Inputs {
        self.inputs.clone()
    }
    fn input_applied(&self, id: String) -> BoxFuture<'_, ()> {
        async move {
            self.events.lock().unwrap().push(format!("applied:{id}"));
        }
        .boxed()
    }
    fn message_start(&self) -> BoxFuture<'_, String> {
        async { "answer".into() }.boxed()
    }
    fn text_delta(&self, _: String, delta: String) -> BoxFuture<'_, ()> {
        async move {
            self.events.lock().unwrap().push(format!("text:{delta}"));
            if self.steer && delta == "Thinking" {
                self.inputs.push(Input {
                    id: "context-item".into(),
                    text: "Read only".into(),
                    share: false,
                });
            }
        }
        .boxed()
    }
    fn message_finish(&self, _: String, _: Option<String>) -> BoxFuture<'_, ()> {
        async {
            self.events.lock().unwrap().push("finished".into());
        }
        .boxed()
    }
    fn tool_start(&self, method: String, _: Value) -> BoxFuture<'_, String> {
        async move {
            self.events.lock().unwrap().push(format!("tool:{method}"));
            "tool".into()
        }
        .boxed()
    }
    fn call(&self, method: String, _: Value) -> BoxFuture<'_, Result<Value, Value>> {
        async move {
            self.events.lock().unwrap().push(format!("call:{method}"));
            self.called.notify_one();
            if self.block {
                std::future::pending::<()>().await;
            }
            Ok(json!({"vehicles":[]}))
        }
        .boxed()
    }
    fn tool_finish(&self, _: String, outcome: Result<Value, Value>) -> BoxFuture<'_, ()> {
        async move {
            self.events
                .lock()
                .unwrap()
                .push(format!("tool-finished:{}", outcome.is_ok()));
        }
        .boxed()
    }
}
#[cfg(unix)]
pub(super) fn program(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            "import os,sys; print(os.path.realpath(sys.executable))",
        ])
        .output()
        .expect("Python 3 is required for runtime protocol fixtures");
    assert!(
        output.status.success(),
        "Python 3 could not identify its interpreter"
    );
    let interpreter = String::from_utf8(output.stdout).unwrap();
    let interpreter = interpreter.trim();
    assert!(std::path::Path::new(interpreter).is_file());
    let path = dir.join("runtime");
    std::fs::write(
        &path,
        format!("#!/usr/bin/env -S {}\n{body}", json!(interpreter)),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}
#[tokio::test]
async fn tool_bridge_authenticates_rejects_origin_and_unknown_tools_and_expires() {
    let host = Arc::new(FixtureHost::default());
    let bridge = super::runtime::Bridge::start(host.clone()).await.unwrap();
    let url = bridge.config["url"].as_str().unwrap();
    let token = bridge.config["headers"]["Authorization"].as_str().unwrap();
    let client = reqwest::Client::new();
    let call = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"vehicle_list","arguments":{}}});
    assert_eq!(
        client.post(url).json(&call).send().await.unwrap().status(),
        403
    );
    assert_eq!(
        client
            .post(url)
            .header("Authorization", token)
            .header("Origin", "http://evil.test")
            .json(&call)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let unknown = client
        .post(url)
        .header("Authorization", token)
        .json(&json!({"id":1,"method":"tools/call","params":{"name":"shell"}}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(unknown["error"]["code"], -32602);
    let response = client
        .post(url)
        .header("Authorization", token)
        .json(&call)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(response["result"]["isError"], false);
    assert_eq!(
        host.events.lock().unwrap().as_slice(),
        [
            "tool:vehicle/list",
            "call:vehicle/list",
            "tool-finished:true"
        ]
    );
    host.inputs.push(Input {
        id: "steer".into(),
        text: "stop".into(),
        share: false,
    });
    let stopped = client
        .post(url)
        .header("Authorization", token)
        .json(&call)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(stopped["result"]["isError"], true);
    let url = url.to_owned();
    drop(bridge);
    assert!(client.post(url).json(&call).send().await.is_err());
}
#[tokio::test]
async fn stopping_bridge_cancels_a_waiting_tool_call() {
    let host = Arc::new(FixtureHost {
        block: true,
        ..FixtureHost::default()
    });
    let bridge = super::runtime::Bridge::start(host.clone()).await.unwrap();
    let config = bridge.config.clone();
    let request = tokio::spawn(async move {
        reqwest::Client::new().post(config["url"].as_str().unwrap()).header("Authorization",config["headers"]["Authorization"].as_str().unwrap())
    .json(&json!({"id":1,"method":"tools/call","params":{"name":"vehicle_list","arguments":{}}})).send().await
    });
    host.called.notified().await;
    drop(bridge);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), request)
        .await
        .unwrap();
    assert!(
        host.events
            .lock()
            .unwrap()
            .contains(&"tool-finished:false".into())
    );
}

// Only the Codex host's Unix tests use it.
#[cfg(unix)]
pub(super) fn engine() -> super::EngineHandle {
    use uxduck_engine::{
        Engine, EngineConfig, HostAbout, Loaded,
        runtime::{Clock, Forget, Ports, Random},
        state::State,
    };
    struct Tick;
    impl Clock for Tick {
        fn now_us(&self) -> u64 {
            1000
        }
    }
    struct Dice;
    impl Random for Dice {
        fn fill(&mut self, b: &mut [u8]) {
            getrandom::fill(b).unwrap();
        }
    }
    super::EngineHandle::spawn(Engine::new(
        EngineConfig {
            daemon_version: "0.1.0".into(),
            capabilities: uxduck_contracts::dto::initialize::DaemonCapabilities::default(),
            ui_url: "http://localhost".into(),
            checkpoint_every: 100,
            models: vec![],
            agent_provider: "hosted".into(),
            about: HostAbout::default(),
        },
        Box::new(Tick),
        Box::new(Dice),
        Ports::with_persistence(Box::new(Forget)),
        Loaded {
            state: State::default(),
            sessions: vec![],
        },
    ))
}
