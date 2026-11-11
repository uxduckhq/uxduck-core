//! Local Codex app-server over stdio. Codex owns device login, refresh and storage.
//! No subscription token is read, copied, or sent through UXDuck's API.
use super::{
    EngineHandle,
    connections::Connections,
    runtime::{Bridge, JsonReader, line, send},
};
use serde_json::{Value, json};
use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use tokio::process::{Child, ChildStdin, Command};
use uxduck_agent::Host;
use uxduck_contracts::dto::thread::{
    AgentConnection, ConnectedModel, ConnectionAction, ConnectionStatus, DeviceLogin,
};

pub struct Codex {
    home: PathBuf,
    executable: Option<PathBuf>,
}
struct Session {
    _child: Child,
    stdin: ChildStdin,
    reader: JsonReader,
    next_id: u64,
    notifications: std::collections::VecDeque<Value>,
}
fn binary() -> Option<PathBuf> {
    let name = if cfg!(windows) { "codex.exe" } else { "codex" };
    let mut paths: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|p| p.join(name)).collect())
        .unwrap_or_default();
    if let Some(home) = directories::BaseDirs::new() {
        paths.push(home.home_dir().join(".local/bin").join(name));
    }
    #[cfg(target_os = "macos")]
    for app in ["/Applications/Codex.app", "/Applications/ChatGPT.app"] {
        paths.push(PathBuf::from(app).join("Contents/Resources/codex-cli/bin/codex"));
    }
    paths.into_iter().find(|p| p.is_file())
}
impl Session {
    async fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        send(
            &mut self.stdin,
            &json!({"id":id,"method":method,"params":params}),
        )
        .await?;
        tokio::time::timeout(Duration::from_secs(30),async {
            while let Some(msg)=line(&mut self.reader).await? {
                if msg["id"]==id {
                    if msg.get("error").is_some() { return Err(format!("Codex could not complete {method}. Update Codex or reconnect your account.")); }
                    return Ok(msg["result"].clone());
                }
                self.reject_unexpected(&msg).await?;
                if msg.get("method").is_some() && msg.get("id").is_none() {
                    if self.notifications.len() >= 512 { return Err("Codex sent too many startup events.".into()); }
                    self.notifications.push_back(msg);
                }
            }
            Err("Codex stopped unexpectedly.".into())
        }).await.map_err(|_|"Codex did not respond in time.".to_owned())?
    }
    async fn next(&mut self) -> Result<Option<Value>, String> {
        if let Some(msg) = self.notifications.pop_front() {
            return Ok(Some(msg));
        }
        line(&mut self.reader).await
    }
    async fn reject_unexpected(&mut self, msg: &Value) -> Result<(), String> {
        if msg.get("method").is_some()
            && let Some(id) = msg.get("id")
        {
            send(&mut self.stdin,&json!({"id":id,"error":{"code":-32601,"message":"Only UXDuck tools are available in this client."}})).await?;
        }
        Ok(())
    }
}
impl Codex {
    pub fn new(data_dir: &std::path::Path) -> Self {
        Self {
            home: data_dir.join("agent/codex"),
            executable: None,
        }
    }
    async fn start(&self) -> Result<Session, String> {
        let binary = self
            .executable
            .clone()
            .or_else(binary)
            .ok_or("Install Codex from OpenAI, then refresh this connection.")?;
        super::super::create_private_dir(&self.home)
            .map_err(|_| "Could not create Codex's private credentials directory.".to_owned())?;
        let mut command = Command::new(binary);
        command
            .args(["app-server", "--listen", "stdio://"])
            .env("CODEX_HOME", &self.home)
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEX_API_KEY")
            .current_dir(&self.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for setting in [
            "analytics.enabled=false",
            "web_search=\"disabled\"",
            "features.shell_tool=false",
            "features.unified_exec=false",
            "features.apps=false",
            "features.plugins=false",
            "features.hooks=false",
            "features.computer_use=false",
            "features.browser_use=false",
            "features.browser_use_external=false",
            "features.view_image=false",
            "features.code_mode=false",
            "features.code_mode_host=false",
            "features.multi_agent=false",
            "features.skip_host_skill_discovery=true",
        ] {
            command.args(["-c", setting]);
        }
        let mut child = command
            .spawn()
            .map_err(|_| "Codex could not start. Update or reinstall it.".to_owned())?;
        let stdin = child.stdin.take().ok_or("Codex has no input stream.")?;
        let reader = JsonReader::new(child.stdout.take().ok_or("Codex has no output stream.")?);
        let mut session = Session {
            _child: child,
            stdin,
            reader,
            next_id: 0,
            notifications: std::collections::VecDeque::new(),
        };
        session.call("initialize",json!({"clientInfo":{"name":"uxduck","title":"UXDuck","version":env!("CARGO_PKG_VERSION")}})).await?;
        send(
            &mut session.stdin,
            &json!({"method":"initialized","params":{}}),
        )
        .await?;
        Ok(session)
    }
    pub async fn connection(
        &self,
        owner: &Connections,
        engine: &EngineHandle,
        action: ConnectionAction,
    ) -> Result<AgentConnection, String> {
        let mut state = AgentConnection {
            id: "codex".into(),
            title: "Codex".into(),
            status: ConnectionStatus::Disconnected,
            detail: "Use Codex with your ChatGPT subscription on this computer.".into(),
            models: vec![],
            login: None,
        };
        if self.executable.clone().or_else(binary).is_none() {
            state.status = ConnectionStatus::Unavailable;
            state.detail = "Install Codex from OpenAI, then refresh this connection.".into();
            return Ok(state);
        }
        let mut session = self.start().await?;
        if action == ConnectionAction::Disconnect {
            session.call("account/logout", json!({})).await?;
            return Ok(state);
        }
        let mut account = session
            .call("account/read", json!({"refreshToken":false}))
            .await?["account"]
            .clone();
        if account.is_null() && action == ConnectionAction::Connect {
            let login = session
                .call("account/login/start", json!({"type":"chatgptDeviceCode"}))
                .await?;
            let url = login["verificationUrl"]
                .as_str()
                .filter(|s| s.starts_with("https://auth.openai.com/"))
                .ok_or(
                    "Codex did not return a valid device sign-in page. Update Codex and try again.",
                )?;
            let code = login["userCode"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() < 100)
                .ok_or("Codex did not return a device code.")?;
            {
                let mut states = owner.states.lock().await;
                if let Some(c) = states.iter_mut().find(|c| c.id == "codex") {
                    c.login = Some(DeviceLogin {
                        url: url.into(),
                        code: code.into(),
                    });
                    c.status = ConnectionStatus::Connecting;
                    c.detail="Open OpenAI's device sign-in page and enter this code. Enable device login in your ChatGPT settings if asked.".into();
                }
            }
            owner.publish(engine).await;
            tokio::time::timeout(Duration::from_secs(300), async {
                while let Some(msg) = session.next().await? {
                    if msg["method"] == "account/login/completed" {
                        return if msg["params"]["success"] == true {
                            Ok(())
                        } else {
                            Err("Codex device sign-in did not finish. Try connecting again."
                                .to_owned())
                        };
                    }
                    session.reject_unexpected(&msg).await?;
                }
                Err("Codex stopped during sign-in.".into())
            })
            .await
            .map_err(|_| "The device code expired. Connect again for a new code.".to_owned())??;
            account = session
                .call("account/read", json!({"refreshToken":false}))
                .await?["account"]
                .clone();
        }
        if account.is_null() {
            return Ok(state);
        }
        if account["type"] != "chatgpt" {
            return Err("This connection needs a ChatGPT plan. Disconnect and sign in with your ChatGPT account.".into());
        }
        let models = available_models(&mut session).await?;
        if models.is_empty() {
            return Err(
                "Codex returned no models for your account. Check your plan or update Codex."
                    .into(),
            );
        }
        state.status = ConnectionStatus::Connected;
        state.detail = format!(
            "ChatGPT {} plan · Uses your Codex allowance. No UXDuck credits.",
            account["planType"].as_str().unwrap_or("connected")
        );
        state.models = models;
        Ok(state)
    }
    pub async fn run(
        &self,
        host: Arc<dyn Host>,
        model: &str,
        effort: Option<&str>,
        prompt: String,
    ) -> Result<(), String> {
        let bridge = Bridge::start(host.clone()).await?;
        let mut session = self.start().await?;
        let account = session
            .call("account/read", json!({"refreshToken":false}))
            .await?;
        if account["account"]["type"] != "chatgpt" {
            return Err("Reconnect Codex in Settings > Connections > AI models.".into());
        }
        let thread = session
            .call(
                "thread/start",
                thread_params(model, effort, &self.home, &bridge.config),
            )
            .await?;
        let thread = thread["thread"]["id"]
            .as_str()
            .ok_or("Codex did not create a conversation.")?
            .to_owned();
        let turn=session.call("turn/start",json!({"threadId":thread,"input":[{"type":"text","text":prompt}],
            "sandboxPolicy":{"type":"readOnly","access":{"type":"restricted","includePlatformDefaults":true,"readableRoots":[]}}})).await?;
        let turn = turn["turn"]["id"]
            .as_str()
            .ok_or("Codex did not start a turn.")?
            .to_owned();
        let inputs = host.inputs();
        let mut messages = std::collections::HashMap::<String, String>::new();
        let mut pending = std::collections::HashMap::<u64, uxduck_agent::input::Input>::new();
        let mut rejected = Vec::new();
        let mut total = 0usize;
        let lifetime = tokio::time::sleep(Duration::from_secs(600));
        tokio::pin!(lifetime);
        loop {
            tokio::select! {
                ()=&mut lifetime => return Err("Codex's turn timed out. Your queued messages are paused.".into()),
                input=inputs.next()=>{
                    bridge.hold_tools(true);
                    session.next_id+=1;let id=session.next_id;
                    send(&mut session.stdin,&json!({"id":id,"method":"turn/steer","params":{"threadId":thread,"expectedTurnId":turn,
                        "input":[{"type":"text","text":input.text}]}})).await?;
                    pending.insert(id,input);
                }
                next=session.next()=>{
                    let msg=next?.ok_or("Codex stopped before completing the answer. Check your plan or reconnect.")?;
                    total+=msg.to_string().len();if total>32*1024*1024{return Err("Codex's response exceeded the turn limit.".into());}
                    if let Some(id)=msg["id"].as_u64() && let Some(input)=pending.remove(&id) {
                        if msg.get("error").is_some() || msg["result"]["turnId"] != turn {rejected.push(input);} else {host.input_applied(input.id).await;}
                        if pending.is_empty() { bridge.hold_tools(false); }
                    }
                    if msg["method"]=="item/agentMessage/delta" {
                        let provider_id=msg["params"]["itemId"].as_str().unwrap_or("message").to_owned();
                        let id=if let Some(id) = messages.get(&provider_id) { id.clone() } else {let id=host.message_start().await;messages.insert(provider_id,id.clone());id};
                        if let Some(delta)=msg["params"]["delta"].as_str(){host.text_delta(id,delta.into()).await;}
                    }
                    if msg["method"]=="item/completed" && msg["params"]["item"]["type"]=="agentMessage"
                        && let Some(key)=msg["params"]["item"]["id"].as_str() && let Some(id)=messages.remove(key){host.message_finish(id,None).await;}
                    if msg["method"]=="turn/completed" {
                        for input in rejected {inputs.push(input);}
                        for (_,input) in pending {inputs.push(input);}
                        if msg["params"]["turn"]["status"]!="completed" {return Err("Codex could not complete this turn. Check your plan's allowance or reconnect in Settings > Connections > AI models.".into());}
                        for (_,id) in messages{host.message_finish(id,None).await;}
                        return Ok(());
                    }
                    session.reject_unexpected(&msg).await?;
                }
            }
        }
    }
}
fn thread_params(
    model: &str,
    effort: Option<&str>,
    home: &std::path::Path,
    bridge: &Value,
) -> Value {
    let mut params = json!({"model":model,"cwd":home,"ephemeral":true,"approvalPolicy":"never","sandbox":"read-only",
        "baseInstructions":uxduck_agent::turn::INSTRUCTIONS,
        "config":{"mcp_servers":{"uxduck":{"url":bridge["url"],"http_headers":bridge["headers"],
            "required":true,"tool_timeout_sec":180}},"web_search":"disabled"}});
    if let Some(effort) = effort {
        params["config"]["model_reasoning_effort"] = json!(effort);
    }
    params
}
async fn available_models(session: &mut Session) -> Result<Vec<ConnectedModel>, String> {
    let mut models = vec![];
    let mut cursor = Value::Null;
    for _ in 0..10 {
        let page = session
            .call(
                "model/list",
                json!({"includeHidden":false,"limit":100,"cursor":cursor}),
            )
            .await?;
        models.extend(catalog_models(&page["data"]));
        cursor = page["nextCursor"].clone();
        if cursor.is_null() {
            break;
        }
    }
    Ok(models)
}
fn catalog_models(value: &Value) -> Vec<ConnectedModel> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["hidden"] != true)
        .filter_map(|m| {
            let id = m["model"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() < 200)?;
            let level = |v: &Value| {
                v.as_str()
                    .filter(|s| !s.is_empty() && s.len() < 20)
                    .map(str::to_owned)
            };
            Some(ConnectedModel {
                id: format!("codex:{id}"),
                title: m["displayName"]
                    .as_str()
                    .unwrap_or(id)
                    .chars()
                    .take(120)
                    .collect(),
                efforts: m["supportedReasoningEfforts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(10)
                    .filter_map(|e| level(&e["reasoningEffort"]))
                    .collect(),
                default_effort: level(&m["defaultReasoningEffort"]),
            })
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hidden_models_are_not_advertised_and_subscription_ids_are_distinct() {
        assert_eq!(
            catalog_models(
                &json!([{"model":"gpt","displayName":"GPT","defaultReasoningEffort":"medium",
                    "supportedReasoningEfforts":[{"reasoningEffort":"low"},{"reasoningEffort":"medium"},{"reasoningEffort":"xhigh"}]},
                    {"model":"hidden","hidden":true}])
            ),
            vec![ConnectedModel {
                id: "codex:gpt".into(),
                title: "GPT".into(),
                efforts: vec!["low".into(), "medium".into(), "xhigh".into()],
                default_effort: Some("medium".into()),
            }]
        );
    }
}

#[cfg(all(test, unix))]
mod protocol_tests {
    use super::*;
    use crate::daemon::agent::runtime_tests::{FixtureHost, program};
    #[tokio::test]
    async fn app_server_steers_the_expected_turn_without_losing_partial_frames() {
        let dir = tempfile::tempdir().unwrap();
        let executable = program(
            dir.path(),
            r#"
import sys,json,time,urllib.request,os
assert 'OPENAI_API_KEY' not in os.environ
assert 'CODEX_HOME' in os.environ
def out(v): print(json.dumps(v),flush=True)
for raw in sys.stdin:
 m=json.loads(raw);method=m.get('method');i=m.get('id')
 if method=='initialized': continue
 if method=='initialize': out({'id':i,'result':{}})
 elif method=='account/read': out({'id':i,'result':{'account':{'type':'chatgpt','planType':'pro'}}})
 elif method=='thread/start':
  p=m['params'];assert p['ephemeral'] and p['sandbox']=='read-only' and p['approvalPolicy']=='never'
  assert p['config'].get('model_reasoning_effort')==('high' if p['model']=='gpt' else None)
  config=p['config']['mcp_servers']['uxduck']
  req=urllib.request.Request(config['url'],data=json.dumps({'id':1,'method':'tools/call','params':{'name':'vehicle_list','arguments':{}}}).encode(),headers={'Content-Type':'application/json',**config['http_headers']})
  assert json.load(urllib.request.urlopen(req))['result']['isError']==False
  out({'id':i,'result':{'thread':{'id':'thread'}}})
 elif method=='turn/start':
  out({'id':i,'result':{'turn':{'id':'turn'}}})
  out({'method':'item/agentMessage/delta','params':{'itemId':'answer','delta':'Thinking'}})
  sys.stdout.write('{"method":');sys.stdout.flush();time.sleep(.05)
  sys.stdout.write('"item/started","params":{}}\n');sys.stdout.flush()
 elif method=='turn/steer':
  assert m['params']['expectedTurnId']=='turn'
  out({'id':i,'result':{'turnId':'wrong' if p['model']=='wrong-ack' else 'turn'}})
  out({'method':'item/agentMessage/delta','params':{'itemId':'answer','delta':'Steered'}})
  out({'method':'turn/completed','params':{'turn':{'status':'completed'}}})
 else: out({'id':i,'error':{'code':-32601}})
"#,
        );
        let codex = Codex {
            home: dir.path().join("private"),
            executable: Some(executable),
        };
        let host = Arc::new(FixtureHost {
            steer: true,
            ..FixtureHost::default()
        });
        tokio::time::timeout(
            Duration::from_secs(5),
            codex.run(host.clone(), "gpt", Some("high"), "hello".into()),
        )
        .await
        .unwrap()
        .unwrap();
        let events = host.events.lock().unwrap().clone();
        assert!(events.contains(&"call:vehicle/list".into()));
        assert!(events.contains(&"applied:context-item".into()));
        assert!(events.contains(&"finished".into()));
        drop(events);
        let host = Arc::new(FixtureHost {
            steer: true,
            ..FixtureHost::default()
        });
        tokio::time::timeout(
            Duration::from_secs(5),
            codex.run(host.clone(), "wrong-ack", None, "hello".into()),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            !host
                .events
                .lock()
                .unwrap()
                .contains(&"applied:context-item".into()),
            "an acknowledgement of another turn must not consume this turn's context"
        );
        assert!(host.inputs.pending(), "unaccepted context remains pending");
    }
}

#[cfg(test)]
mod installed_qa {
    use super::*;
    #[tokio::test]
    #[ignore = "host QA: requires installed Codex; isolated home, no login or inference"]
    async fn installed_codex_runtime_handshake() {
        let dir = tempfile::tempdir().unwrap();
        let codex = Codex::new(dir.path());
        let mut session = codex.start().await.unwrap();
        let account = session
            .call("account/read", json!({"refreshToken":false}))
            .await
            .unwrap();
        assert!(
            account["account"].is_null(),
            "UXDuck must not reuse another app's credentials"
        );
        let host = Arc::new(crate::daemon::agent::runtime_tests::FixtureHost::default());
        let bridge = Bridge::start(host).await.unwrap();
        let thread = session
            .call(
                "thread/start",
                thread_params("gpt-6-sol", None, &codex.home, &bridge.config),
            )
            .await
            .unwrap();
        assert!(
            thread["thread"]["id"].is_string(),
            "The installed runtime must accept UXDuck's MCP and sandbox configuration"
        );
    }
}

#[cfg(all(test, unix))]
mod account_tests {
    use super::*;
    use crate::daemon::agent::{
        FileCredentials,
        runtime_tests::{engine, program},
    };
    #[tokio::test]
    async fn device_login_model_discovery_restart_and_logout_are_provider_owned() {
        let dir = tempfile::tempdir().unwrap();
        let executable = program(
            dir.path(),
            r"
import sys,json,os,pathlib
state=pathlib.Path(os.environ['CODEX_HOME'])/'fixture-login'
def out(v): print(json.dumps(v),flush=True)
for raw in sys.stdin:
 m=json.loads(raw);method=m.get('method');i=m.get('id')
 if method=='initialized': continue
 if method=='initialize': out({'id':i,'result':{}})
 elif method=='account/read': out({'id':i,'result':{'account':{'type':'chatgpt','planType':'pro'} if state.exists() else None}})
 elif method=='account/login/start':
  assert m['params']['type']=='chatgptDeviceCode'
  state.write_text('provider-owned')
  out({'id':i,'result':{'type':'chatgptDeviceCode','loginId':'login','verificationUrl':'https://auth.openai.com/codex/device','userCode':'TEST-CODE'}})
  out({'method':'account/login/completed','params':{'loginId':'login','success':True}})
 elif method=='model/list': out({'id':i,'result':{'data':[{'model':'gpt','displayName':'GPT'},{'model':'hidden','hidden':True}],'nextCursor':None}})
 elif method=='account/logout':
  state.unlink(missing_ok=True)
  out({'id':i,'result':{}})
 else: out({'id':i,'error':{'code':-32601}})
",
        );
        let mut owner = Connections::new(&FileCredentials::new(dir.path()), dir.path());
        Arc::get_mut(&mut owner).unwrap().codex.executable = Some(executable);
        let engine = engine();
        let action = |action| uxduck_contracts::dto::thread::AgentConnectionParams {
            provider: "codex".into(),
            action,
        };
        owner
            .handle(&engine, action(ConnectionAction::Connect))
            .await;
        {
            let states = owner.states.lock().await;
            let c = &states[1];
            assert_eq!(c.status, ConnectionStatus::Connected);
            assert!(c.login.is_none());
            assert_eq!(c.models.len(), 1);
            assert_eq!(c.models[0].id, "codex:gpt");
        }
        owner
            .handle(&engine, action(ConnectionAction::Refresh))
            .await;
        assert_eq!(
            owner.states.lock().await[1].status,
            ConnectionStatus::Connected
        );
        owner
            .handle(&engine, action(ConnectionAction::Disconnect))
            .await;
        assert_eq!(
            owner.states.lock().await[1].status,
            ConnectionStatus::Disconnected
        );
        assert!(!dir.path().join("agent/codex/fixture-login").exists());
        assert!(
            !dir.path().join("credentials.json").exists(),
            "UXDuck never copies the provider's tokens"
        );
        owner
            .handle(&engine, action(ConnectionAction::Refresh))
            .await;
        assert!(owner.states.lock().await[1].models.is_empty());
    }
}
