//! The unmodified Claude Code runtime, controlled over its documented JSON stream.
//! A turn-scoped MCP endpoint exposes only UXDuck tools through the ordinary agent host.
use super::runtime::{Bridge, JsonReader, line, send};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::process::{Child, Command};
use uxduck_agent::Host;
use uxduck_contracts::dto::thread::ConnectedModel;

pub fn binary() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    };
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|p| p.join(name)).collect())
        .unwrap_or_default();
    if let Some(home) = directories::BaseDirs::new() {
        candidates.push(home.home_dir().join(".local/bin").join(name));
    }
    candidates.into_iter().find(|p| p.is_file())
}
const API_AUTH_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_CUSTOM_HEADERS",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];
fn command(binary: &Path) -> Command {
    let mut command = Command::new(binary);
    // This connection is for the installed CLI's subscription, never inherited API billing.
    for key in API_AUTH_ENV {
        command.env_remove(key);
    }
    command.args(["--setting-sources", ""]);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}
pub async fn signed_in(binary: &Path) -> Result<bool, String> {
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        command(binary).args(["auth", "status", "--json"]).output(),
    )
    .await
    .map_err(|_| "Claude Code did not respond. Update it and try again.".to_owned())?
    .map_err(|_| "Claude Code could not start.".to_owned())?;
    let value: Value = serde_json::from_slice(&output.stdout).map_err(|_| {
        "Claude Code returned an invalid account status. Update it and try again.".to_owned()
    })?;
    if value["loggedIn"] != true {
        return Ok(false);
    }
    if value["authMethod"] != "claude.ai"
        || value["apiProvider"] != "firstParty"
        || !(value["apiKeySource"].is_null() || value["apiKeySource"] == "none")
    {
        return Err("Claude Code is using API or cloud billing. Sign in with your Claude subscription, then reconnect.".into());
    }
    Ok(true)
}
pub async fn login(binary: &Path) -> Result<(), String> {
    let result = tokio::time::timeout(
        Duration::from_secs(300),
        command(binary)
            .args(["auth", "login"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status(),
    )
    .await
    .map_err(|_| "Claude sign-in timed out. Try connecting again.".to_owned())?
    .map_err(|_| "Claude Code could not open sign-in.".to_owned())?;
    if result.success() {
        Ok(())
    } else {
        Err("Claude sign-in did not finish. Try again in Claude Code, then reconnect.".into())
    }
}
fn stream(binary: &Path) -> Command {
    let mut cmd = command(binary);
    // Keep provider auth intact. Ignore project settings, skills, hooks and unrelated MCP servers.
    cmd.args([
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--replay-user-messages",
        "--no-session-persistence",
        "--tools",
        "",
        "--strict-mcp-config",
        "--disable-slash-commands",
        "--no-chrome",
    ]);
    cmd.current_dir(std::env::temp_dir());
    cmd
}
pub async fn catalog(binary: &Path) -> Result<Vec<ConnectedModel>, String> {
    let mut child = stream(binary)
        .args(["--mcp-config", "{\"mcpServers\":{}}"])
        .spawn()
        .map_err(|_| "Claude Code could not start.".to_owned())?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or("Claude Code has no input stream.")?;
    let mut reader = JsonReader::new(
        child
            .stdout
            .take()
            .ok_or("Claude Code has no output stream.")?,
    );
    send(&mut stdin, &json!({"type":"control_request","request_id":"models","request":{"subtype":"initialize","hooks":{}}})).await?;
    let models = tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(msg) = line(&mut reader).await? {
            if msg["type"] == "control_response" && msg["response"]["request_id"] == "models" {
                return Ok::<_, String>(catalog_models(&msg["response"]["response"]["models"]));
            }
        }
        Err("Claude Code ended before returning models.".into())
    })
    .await
    .map_err(|_| "Claude Code's model list timed out.".to_owned())??;
    let _ = child.kill().await;
    Ok(models)
}
fn catalog_models(value: &Value) -> Vec<ConnectedModel> {
    let mut out = Vec::new();
    for model in value.as_array().into_iter().flatten().take(100) {
        let Some(id) = model["value"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() < 200)
        else {
            continue;
        };
        if id == "default" {
            continue;
        }
        let efforts: Vec<String> = model["supportedEffortLevels"]
            .as_array()
            .into_iter()
            .flatten()
            .take(10)
            .filter_map(|l| l.as_str().filter(|s| !s.is_empty() && s.len() < 20))
            .map(str::to_owned)
            .collect();
        // Claude Code does not say its default, so none is claimed: a chat
        // that chooses none sends no `--effort` and reads "Default".
        out.push(ConnectedModel {
            efforts,
            default_effort: None,
            id: format!("claude-code:{id}"),
            title: model["displayName"]
                .as_str()
                .unwrap_or(id)
                .chars()
                .take(120)
                .collect(),
        });
    }
    out
}
fn message_id() -> Result<String, String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|_| "Could not identify this message.".to_owned())?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = format!("{:x}", uxduck_core::encoding::Hex(&b));
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    ))
}
pub async fn title(model: &str, text: &str) -> uxduck_agent::turn::TitleOutcome {
    let Some(binary) = binary() else {
        return uxduck_agent::turn::TitleOutcome::default();
    };
    title_with_binary(model, text, &binary).await
}
fn title_command(binary: &Path) -> Command {
    let mut cmd = stream(binary);
    cmd.env_clear();
    for key in [
        "PATH",
        "HOME",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "SYSTEMROOT",
        "WINDIR",
        "TMP",
        "TEMP",
        "LANG",
        "LC_ALL",
        "CLAUDE_CONFIG_DIR",
    ] {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    cmd.env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "128")
        .env("CLAUDE_CODE_MAX_RETRIES", "0")
        .args([
            "--safe-mode",
            "--mcp-config",
            "{\"mcpServers\":{}}",
            "--disallowedTools",
            "*",
            "--max-turns",
            "1",
            "--permission-mode",
            "dontAsk",
            "--system-prompt",
            "Generate only the requested chat title. Never call tools or answer the message.",
        ]);
    cmd
}
async fn title_with_binary(
    model: &str,
    text: &str,
    binary: &Path,
) -> uxduck_agent::turn::TitleOutcome {
    let mut outcome = uxduck_agent::turn::TitleOutcome::default();
    let result: Result<(), String> = async {
        if !signed_in(binary).await? {
            return Err("Claude Code is not signed in.".into());
        }
        let directory = tempfile::tempdir().map_err(|_| "Could not isolate the title request.")?;
        let mut child = title_command(binary)
            .args(["--model", model])
            .current_dir(directory.path())
            .spawn()
            .map_err(|_| "Claude Code could not start the title request.")?;
        let mut stdin = child.stdin.take().ok_or("Claude Code has no input stream.")?;
        let mut reader = JsonReader::new(child.stdout.take().ok_or("Claude Code has no output stream.")?);
        send(&mut stdin, &json!({"type":"user","message":{"role":"user","content":uxduck_agent::turn::title_prompt(text)},"parent_tool_use_id":null})).await?;
        let mut response = String::new();
        let mut total = 0usize;
        while let Some(msg) = line(&mut reader).await? {
            total = total.saturating_add(msg.to_string().len());
            if total > 64 * 1024 {
                return Err("Claude's title response exceeded the limit.".into());
            }
            if msg["type"] == "control_request"
                || msg["event"]["content_block"]["type"] == "tool_use"
                || msg["message"]["content"].as_array().is_some_and(|items| items.iter().any(|item| item["type"] == "tool_use"))
            {
                return Err("Claude requested a tool during title generation.".into());
            }
            if msg["type"] == "stream_event"
                && let Some(delta) = msg["event"]["delta"]["text"].as_str()
            {
                if response.len().saturating_add(delta.len()) > 4096 {
                    return Err("Claude's title response exceeded the limit.".into());
                }
                response.push_str(delta);
            }
            if msg["type"] == "result" {
                outcome.usage = title_usage(&msg["usage"]);
                if msg["is_error"] != true && msg["subtype"] == "success" {
                    let text = msg["result"].as_str().unwrap_or(&response);
                    if text.len() <= 4096 {
                        outcome.title = uxduck_agent::turn::parse_title(text);
                    }
                }
                return Ok(());
            }
        }
        Err("Claude Code stopped before returning a title.".into())
    }.await;
    if result.is_err() {
        outcome.title = None;
    }
    outcome
}
fn title_usage(value: &Value) -> Option<uxduck_agent::Usage> {
    Some(uxduck_agent::Usage {
        input_tokens: value["input_tokens"].as_u64()?,
        output_tokens: value["output_tokens"].as_u64()?,
        cache_read_tokens: value["cache_read_input_tokens"].as_u64().unwrap_or(0),
        cache_write_tokens: value["cache_creation_input_tokens"].as_u64().unwrap_or(0),
        thinking_dropped: 0,
    })
}
pub async fn run(
    host: Arc<dyn Host>,
    model: &str,
    effort: Option<&str>,
    prompt: String,
) -> Result<(), String> {
    let binary = binary()
        .ok_or("Install Claude Code, then reconnect in Settings > Connections > AI models.")?;
    run_with_binary(host, model, effort, prompt, &binary).await
}
async fn run_with_binary(
    host: Arc<dyn Host>,
    model: &str,
    effort: Option<&str>,
    prompt: String,
    binary: &Path,
) -> Result<(), String> {
    if !signed_in(binary).await? {
        return Err("Sign in to Claude Code with your subscription, then reconnect in Settings > Connections > AI models.".into());
    }
    let bridge = Bridge::start(host.clone()).await?;
    let config = json!({"mcpServers":{"uxduck":bridge.config}});
    let mut command = stream(binary);
    if let Some(effort) = effort {
        command.args(["--effort", effort]);
    }
    let mut child: Child = command
        .args([
            "--model",
            model,
            "--mcp-config",
            &config.to_string(),
            "--allowedTools",
            "mcp__uxduck__*",
            "--permission-mode",
            "dontAsk",
            "--system-prompt",
            uxduck_agent::turn::INSTRUCTIONS,
        ])
        .spawn()
        .map_err(|_| "Claude Code could not start. Update it and reconnect.".to_owned())?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or("Claude Code has no input stream.")?;
    let mut reader = JsonReader::new(
        child
            .stdout
            .take()
            .ok_or("Claude Code has no output stream.")?,
    );
    send(&mut stdin,&json!({"type":"user","message":{"role":"user","content":prompt},"parent_tool_use_id":null})).await?;
    let inputs = host.inputs();
    let mut pending = std::collections::HashMap::<String, Vec<uxduck_agent::input::Input>>::new();
    let mut updates = Vec::new();
    let mut message: Option<String> = None;
    let mut frames = 0usize;
    let lifetime = tokio::time::sleep(Duration::from_secs(600));
    tokio::pin!(lifetime);
    loop {
        tokio::select! {
            () = &mut lifetime => return Err("Claude's turn timed out. Your queued messages are paused.".into()),
            input = inputs.next() => {
                bridge.hold_tools(true);
                if updates.is_empty() {
                    send(&mut stdin,&json!({"type":"control_request","request_id":message_id()?,"request":{"subtype":"interrupt"}})).await?;
                }
                updates.push(input);
            }
            next = line(&mut reader) => {
                let msg = next?.ok_or("Claude Code stopped before completing the answer. Check your plan or reconnect.")?;
                frames += msg.to_string().len();
                if frames > 32*1024*1024 { return Err("Claude's response exceeded the turn limit.".into()); }
                if msg["type"] == "user" && let Some(id) = msg["uuid"].as_str() && let Some(applied) = pending.remove(id) {
                    for input in applied { host.input_applied(input.id).await; }
                    if updates.is_empty() { bridge.hold_tools(false); }
                }
                if msg["type"] == "stream_event" && msg["event"]["type"] == "content_block_delta"
                    && let Some(text) = msg["event"]["delta"]["text"].as_str() {
                    let id = if let Some(id) = &message { id.clone() } else { let id = host.message_start().await; message = Some(id.clone()); id };
                    host.text_delta(id,text.into()).await;
                }
                if msg["type"] == "result" {
                    if !updates.is_empty() {
                        if let Some(id) = message.take() { host.message_finish(id,None).await; }
                        let uuid = message_id()?;
                        let text = updates.iter().map(|i: &uxduck_agent::input::Input| i.text.as_str()).collect::<Vec<_>>().join("\n\n");
                        send(&mut stdin,&json!({"type":"user","uuid":uuid,"message":{"role":"user","content":text},"parent_tool_use_id":null})).await?;
                        pending.insert(uuid, std::mem::take(&mut updates));
                        continue;
                    }
                    if msg["is_error"] == true || msg["subtype"] != "success" {
                        return Err("Claude could not complete this turn. Check your plan's allowance or reconnect in Settings > Connections > AI models.".into());
                    }
                    if let Some(id) = message.take() { host.message_finish(id,None).await; }
                    if !pending.is_empty() { return Err("Claude did not acknowledge the added context. Your follow-up is paused.".into()); }
                    let _ = child.kill().await; return Ok(());
                }
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn subscription_commands_remove_inherited_api_and_cloud_credentials() {
        let cmd = command(Path::new("claude"));
        let removed: Vec<_> = cmd
            .as_std()
            .get_envs()
            .filter_map(|(key, value)| value.is_none().then_some(key.to_str().unwrap()))
            .collect();
        for key in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_CUSTOM_HEADERS",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
        ] {
            assert!(
                removed.contains(&key),
                "{key} must not change subscription billing"
            );
        }
    }
    #[test]
    fn the_runtime_catalog_keeps_the_provider_and_billing_identity() {
        assert_eq!(
            catalog_models(
                &json!([{"value":"default"},{"value":"opus","displayName":"Opus",
                "supportedEffortLevels":["low","medium","high","max"]},{"value":"haiku","displayName":"Haiku"}])
            ),
            vec![
                ConnectedModel {
                    id: "claude-code:opus".into(),
                    title: "Opus".into(),
                    efforts: vec!["low".into(), "medium".into(), "high".into(), "max".into()],
                    default_effort: None,
                },
                ConnectedModel {
                    id: "claude-code:haiku".into(),
                    title: "Haiku".into(),
                    efforts: vec![],
                    default_effort: None,
                }
            ]
        );
    }
}

#[cfg(all(test, unix))]
mod protocol_tests {
    use super::*;
    use crate::daemon::agent::runtime_tests::{FixtureHost, program};
    #[test]
    fn title_command_does_not_forward_the_platform_runtime_variable() {
        assert!(
            title_command(Path::new("claude"))
                .as_std()
                .get_envs()
                .all(|(key, _)| key != "__CF_USER_TEXT_ENCODING")
        );
    }
    #[tokio::test]
    async fn title_is_one_shot_same_model_tool_free_and_usage_is_optional() {
        let dir = tempfile::tempdir().unwrap();
        let binary = program(
            dir.path(),
            r#"
import sys,json,os,pathlib
args=sys.argv
if 'auth' in args:
 print(json.dumps({'loggedIn':True,'authMethod':'claude.ai','apiProvider':'firstParty'}));sys.exit(0)
allowed={'PATH','HOME','USERPROFILE','APPDATA','LOCALAPPDATA','SYSTEMROOT','WINDIR','TMP','TEMP','LANG','LC_ALL','LC_CTYPE','CLAUDE_CONFIG_DIR','CLAUDE_CODE_MAX_OUTPUT_TOKENS','CLAUDE_CODE_MAX_RETRIES'}
if sys.platform=='darwin': allowed.add('__CF_USER_TEXT_ENCODING')
assert set(os.environ)<=allowed
assert os.environ['CLAUDE_CODE_MAX_OUTPUT_TOKENS']=='128'
assert os.environ['CLAUDE_CODE_MAX_RETRIES']=='0'
assert '--safe-mode' in args and '--no-session-persistence' in args and '--no-chrome' in args
assert '--strict-mcp-config' in args and '--disable-slash-commands' in args
assert args[args.index('--tools')+1]==''
assert args[args.index('--disallowedTools')+1]=='*'
assert json.loads(args[args.index('--mcp-config')+1])=={'mcpServers':{}}
assert '--allowedTools' not in args and '--resume' not in args
assert args[args.index('--max-turns')+1]=='1'
model=args[args.index('--model')+1]
assert model in ('selected-opus','missing-usage','oversized','tool','failure','invalid')
assert pathlib.Path.cwd()!=pathlib.Path(__file__).parent
m=json.loads(sys.stdin.readline())
assert m['type']=='user' and m['message']['role']=='user'
assert m['message']['content'].endswith('x'*4000) and not m.get('uuid')
def out(v): print(json.dumps(v),flush=True)
if model=='tool':
 out({'type':'stream_event','event':{'type':'content_block_start','content_block':{'type':'tool_use','name':'Read'}}})
elif model=='oversized':
 out({'type':'stream_event','event':{'type':'content_block_delta','delta':{'text':'x'*4097}}})
else:
 usage={} if model=='missing-usage' else {'usage':{'input_tokens':41,'output_tokens':9,'cache_read_input_tokens':7,'cache_creation_input_tokens':3}}
 text='{"title":"Review vehicle flight logs"}' if model!='invalid' else '{"title":"bad\\ntitle"}'
 out({'type':'result','subtype':'success','is_error':model=='failure','result':text,**usage})
"#,
        );
        for model in [
            "selected-opus",
            "missing-usage",
            "oversized",
            "tool",
            "failure",
            "invalid",
        ] {
            let outcome = tokio::time::timeout(
                Duration::from_secs(5),
                title_with_binary(model, &"x".repeat(9000), &binary),
            )
            .await
            .unwrap();
            if matches!(model, "selected-opus" | "missing-usage") {
                assert_eq!(outcome.title.as_deref(), Some("Review vehicle flight logs"));
            } else {
                assert!(outcome.title.is_none(), "{model} must not produce a title");
            }
            if matches!(model, "selected-opus" | "failure" | "invalid") {
                let usage = outcome.usage.unwrap();
                assert_eq!(
                    (
                        usage.input_tokens,
                        usage.output_tokens,
                        usage.cache_read_tokens,
                        usage.cache_write_tokens
                    ),
                    (41, 9, 7, 3)
                );
            } else {
                assert!(outcome.usage.is_none());
            }
        }
    }
    #[tokio::test]
    async fn title_does_not_infer_with_api_authentication() {
        let dir = tempfile::tempdir().unwrap();
        let binary = program(
            dir.path(),
            r"
import sys,json,pathlib
if 'auth' in sys.argv:
 print(json.dumps({'loggedIn':True,'authMethod':'api_key','apiProvider':'firstParty'}))
else:
 pathlib.Path(__file__+'.inference').touch()
",
        );
        assert!(
            title_with_binary("opus", "hello", &binary)
                .await
                .title
                .is_none()
        );
        assert!(!binary.with_extension("inference").exists());
    }
    #[tokio::test]
    async fn api_profiles_cannot_start_a_subscription_turn() {
        let dir = tempfile::tempdir().unwrap();
        let binary = program(
            dir.path(),
            r"
import sys,json,pathlib
if 'auth' in sys.argv:
 print(json.dumps({'loggedIn':True,'authMethod':'api_key','apiProvider':'firstParty'}))
else:
 pathlib.Path(__file__+'.inference').touch()
",
        );
        let host = Arc::new(FixtureHost::default());
        let error = run_with_binary(host.clone(), "opus", None, "hello".into(), &binary)
            .await
            .unwrap_err();
        assert!(error.contains("API or cloud billing"));
        assert!(!binary.with_extension("inference").exists());
        assert!(host.events.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn official_stream_shape_supports_text_tools_and_acknowledged_steering() {
        let dir = tempfile::tempdir().unwrap();
        let binary = program(
            dir.path(),
            r#"
import sys,json,urllib.request,time
args=sys.argv
if 'auth' in args and 'status' in args:
 print(json.dumps({'loggedIn':True,'authMethod':'claude.ai','apiProvider':'firstParty'}));sys.exit(0)
assert args[args.index('--effort')+1]=='max'
config=json.loads(args[args.index('--mcp-config')+1])['mcpServers']['uxduck']
def out(v): print(json.dumps(v),flush=True)
for raw in sys.stdin:
 m=json.loads(raw)
 if m['type']=='control_request':
  out({'type':'result','subtype':'error_during_execution','is_error':True})
 elif m['type']=='user':
  if m.get('uuid'):
   out(m)
   out({'type':'stream_event','event':{'type':'content_block_delta','delta':{'text':'Steered'}}})
   out({'type':'result','subtype':'success','is_error':False})
  else:
   req=urllib.request.Request(config['url'],data=json.dumps({'id':1,'method':'tools/call','params':{'name':'vehicle_list','arguments':{}}}).encode(),headers={'Content-Type':'application/json',**config['headers']})
   assert json.load(urllib.request.urlopen(req))['result']['isError']==False
   out({'type':'stream_event','event':{'type':'content_block_delta','delta':{'text':'Thinking'}}})
   sys.stdout.write('{"type":');sys.stdout.flush();time.sleep(.05)
   sys.stdout.write('"system","subtype":"status"}\n');sys.stdout.flush()
"#,
        );
        let host = Arc::new(FixtureHost {
            steer: true,
            ..FixtureHost::default()
        });
        tokio::time::timeout(
            Duration::from_secs(5),
            run_with_binary(host.clone(), "opus", Some("max"), "hello".into(), &binary),
        )
        .await
        .unwrap()
        .unwrap();
        let events = host.events.lock().unwrap();
        assert!(events.contains(&"call:vehicle/list".into()));
        assert!(events.contains(&"applied:context-item".into()));
        assert!(events.contains(&"text:Steered".into()));
        assert!(events.contains(&"finished".into()));
    }
}

#[cfg(test)]
mod installed_qa {
    use super::*;
    #[tokio::test]
    #[ignore = "host QA: requires installed Claude Code; reads catalog only, no inference"]
    async fn installed_claude_runtime_catalog() {
        let models = catalog(&binary().expect("Install Claude Code for this host QA."))
            .await
            .unwrap();
        assert!(!models.is_empty());
    }
}
