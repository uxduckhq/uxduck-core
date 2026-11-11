//! Provider-owned authentication. Only metadata is sent to the engine.
use super::{EngineHandle, FileCredentials};
use std::sync::Arc;
use tokio::sync::Mutex;
use uxduck_contracts::dto::thread::{
    AgentConnection, AgentConnectionParams, ConnectionAction, ConnectionStatus,
};
use uxduck_engine::runtime::Credentials;

pub struct Connections {
    pub credentials: FileCredentials,
    pub states: Mutex<Vec<AgentConnection>>,
    pub codex: super::codex::Codex,
}
impl Connections {
    pub fn new(credentials: &FileCredentials, data_dir: &std::path::Path) -> Arc<Self> {
        Arc::new(Self {
            credentials: credentials.clone(),
            states: Mutex::new(vec![
                AgentConnection {
                    id: "claude-code".into(),
                    title: "Claude".into(),
                    status: ConnectionStatus::Disconnected,
                    detail: "Use your Claude subscription through Claude Code on this computer."
                        .into(),
                    models: vec![],
                    login: None,
                },
                AgentConnection {
                    id: "codex".into(),
                    title: "Codex".into(),
                    status: ConnectionStatus::Disconnected,
                    detail: "Use Codex with your ChatGPT subscription on this computer.".into(),
                    models: vec![],
                    login: None,
                },
            ]),
            codex: super::codex::Codex::new(data_dir),
        })
    }
    pub async fn publish(&self, engine: &EngineHandle) {
        let states = self.states.lock().await.clone();
        let _ = engine.run(move |e| e.agent_connections(states)).await;
    }
    pub async fn enabled(&self, provider: &str, model: &str) -> bool {
        self.states.lock().await.iter().any(|c| {
            c.id == provider
                && c.status == ConnectionStatus::Connected
                && c.models.iter().any(|m| m.id == model)
        })
    }
    pub async fn handle(&self, engine: &EngineHandle, params: AgentConnectionParams) {
        let provider = params.provider.as_str();
        if params.action == ConnectionAction::Connect {
            let mut states = self.states.lock().await;
            if let Some(c) = states.iter_mut().find(|c| c.id == provider) {
                c.status = ConnectionStatus::Connecting;
                c.detail = "Complete sign-in in your browser, then return here.".into();
                c.models.clear();
                c.login = None;
            }
            drop(states);
            self.publish(engine).await;
        }
        let result = if provider == "claude-code" {
            self.claude(params.action).await
        } else {
            self.codex.connection(self, engine, params.action).await
        };
        let mut states = self.states.lock().await;
        if let Some(c) = states.iter_mut().find(|c| c.id == provider) {
            match result {
                Ok(next) => *c = next,
                Err(why) => {
                    c.status = ConnectionStatus::Error;
                    c.detail = why;
                    c.models.clear();
                    c.login = None;
                }
            }
        }
        drop(states);
        self.publish(engine).await;
    }
    async fn claude(&self, action: ConnectionAction) -> Result<AgentConnection, String> {
        let mut state = AgentConnection {
            id: "claude-code".into(),
            title: "Claude".into(),
            status: ConnectionStatus::Disconnected,
            detail: "Use your Claude subscription on this computer.".into(),
            models: vec![],
            login: None,
        };
        let mut credentials = self.credentials.clone();
        if action == ConnectionAction::Disconnect {
            // Disconnect UXDuck only; do not sign the person out of other Claude clients.
            credentials
                .set("agent.claude.enabled", "")
                .map_err(|_| "Could not save the Claude connection.".to_owned())?;
            return Ok(state);
        }
        let Some(binary) = super::claude::binary() else {
            state.status = ConnectionStatus::Unavailable;
            state.detail =
                "Install Claude Code from claude.com, then refresh this connection.".into();
            return Ok(state);
        };
        if action == ConnectionAction::Connect {
            if !super::claude::signed_in(&binary).await? {
                super::claude::login(&binary).await?;
            }
            credentials
                .set("agent.claude.enabled", "true")
                .map_err(|_| "Could not save the Claude connection.".to_owned())?;
        }
        if credentials.get("agent.claude.enabled").is_none() {
            return Ok(state);
        }
        if !super::claude::signed_in(&binary).await? {
            state.detail = "Sign in to Claude Code to reconnect your plan.".into();
            return Ok(state);
        }
        state.models = super::claude::catalog(&binary).await?;
        if state.models.is_empty() {
            return Err("Claude Code returned no available models. Update it and refresh.".into());
        }
        state.status = ConnectionStatus::Connected;
        state.detail = "Uses your Claude Code account and its allowance. No UXDuck credits.".into();
        Ok(state)
    }
}
