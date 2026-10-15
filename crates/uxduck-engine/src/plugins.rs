//! Plugins (ADR-0006). A plugin is a client: at install it gets a client
//! record with the capabilities its manifest asks for (installing is the
//! operator's confirmation), and while it runs it has a connection of its
//! own, `plugin:<id>`. Everything it does goes through the ordinary request
//! path on that connection, so capabilities, approvals, receipts and the
//! journal treat it like any other client. The host runs the plugins; the
//! engine never calls into one.

use serde_json::{Value, json};
use uxduck_contracts::dto::link::IdParams;
use uxduck_contracts::dto::plugin::{
    CommandCall, Description, Effects, FoundTool, MapFeature, PLUGIN_ABI, Plugin as PluginDto,
    PluginCommandParams, PluginDraftParams, PluginDraftsResult, PluginFeaturesResult,
    PluginInstallParams, PluginItems, PluginItemsResult, PluginLayer, PluginManifest,
    PluginSetEnabledParams, PluginTestParams, PluginTestResult, PluginUsage, PluginUsageResult,
    ProducedItem, SendMessage, ToolSearchParams, ToolSearchResult, WrittenBy,
};
use uxduck_contracts::dto::state::{EntityChange, EntityType};
use uxduck_contracts::dto::trace::Transport;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::{Capability, ClientKind, ClientRecord};
use uxduck_core::domain::notification::{NotificationKind, Severity, Subject};
use uxduck_core::domain::plugin::{Plugin, PluginOrigin, PluginStatus, valid_id};
use uxduck_core::domain::telemetry::{TelemetryItem, Unit, VehicleTelemetry, key_says_unit};
use uxduck_core::events::{Actor, Change};
use uxduck_core::policy::admission::Trust;

use crate::approvals::record_id;
use crate::commands::Raise;
use crate::rpc::handlers::to_value;
use crate::runtime::{DraftOrder, MessageOrder, PluginLoad, TestOrder};
use crate::{Admitted, ConnectionId, Engine, upsert};

pub(crate) fn daemon() -> Actor {
    Actor {
        client_id: "daemon".into(),
        operator_id: None,
    }
}

pub(crate) fn failed_condition(id: &str) -> String {
    format!("plugin-failed:{id}")
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
    operator: bool,
) -> Result<Value, ApiError> {
    let denied = |why: &str| Err(ApiError::new(ErrorKind::CapabilityDenied, why.to_owned()));
    match request {
        // An agent's install reaches here only once an operator allowed it
        // (`approvals::escalate` always asks for one).
        ClientRequest::PluginInstall(_) if !operator && !granted.contains(&Capability::Config) => {
            denied("an agent's install waits for an operator")
        }
        ClientRequest::PluginRemove(_) if !operator => {
            denied("only an operator client may remove plugins")
        }
        ClientRequest::PluginSetEnabled(_) if !operator => {
            denied("only an operator client may turn plugins off and on")
        }
        ClientRequest::PluginSetEnabled(PluginSetEnabledParams { id, enabled }) => {
            let plugin = engine.set_plugin_enabled(connection, &id, enabled)?;
            to_value(&PluginDto::from(&plugin))
        }
        ClientRequest::PluginUsage(_) => to_value(&PluginUsageResult {
            plugins: engine.plugin_usage.values().cloned().collect(),
        }),
        ClientRequest::PluginDraft(params) => draft(engine, connection, params),
        ClientRequest::PluginDrafts(_) => to_value(&PluginDraftsResult {
            drafts: engine.plugin_host.drafts(),
        }),
        ClientRequest::PluginTest(params) => test(engine, connection, params),
        ClientRequest::PluginInstall(PluginInstallParams { path }) => {
            install(engine, connection, &path)
        }
        ClientRequest::PluginRemove(IdParams { id }) => {
            if !engine.state.plugins.contains_key(&id) {
                return Err(ApiError::new(
                    ErrorKind::NotFound,
                    format!("no plugin {id}"),
                ));
            }
            if engine.state.plugins.get(&id).is_some_and(|p| p.included) {
                return Err(ApiError::new(
                    ErrorKind::InvalidRequest,
                    "this plugin comes with UXDuck: turn it off instead",
                ));
            }
            let removed = engine.state.plugins.get(&id).cloned();
            engine.stop_plugin(&id);
            engine.commit(connection, Change::PluginRemoved { id: id.clone() });
            engine.resolve(&failed_condition(&id));
            if let Some(removed) = removed {
                engine.plugin_removed_here(connection, &removed);
            }
            Ok(json!({}))
        }
        ClientRequest::PluginCommand(params) => command(engine, connection, params, granted),
        ClientRequest::ToolSearch(ToolSearchParams { query, limit }) => {
            to_value(&tool_search(engine, &query, limit.unwrap_or(10).min(50)))
        }
        ClientRequest::PluginFeatures(_) => {
            engine.feature_watchers.insert(connection);
            to_value(&PluginFeaturesResult {
                layers: engine
                    .plugin_layers
                    .keys()
                    .map(|id| engine.plugin_layer(id))
                    .collect(),
            })
        }
        ClientRequest::PluginItems(_) => {
            engine.item_watchers.insert(connection);
            to_value(&PluginItemsResult {
                plugins: engine
                    .plugin_own_items
                    .keys()
                    .map(|id| engine.own_items(id))
                    .collect(),
            })
        }
        _ => Err(ApiError::new(ErrorKind::Internal, "not a plugin method")),
    }
}

/// Running plugins' commands whose words hold the query's, best first:
/// the title counts most, then the name, the description and the plugin.
fn tool_search(engine: &Engine, query: &str, limit: u32) -> ToolSearchResult {
    let words: Vec<String> = query
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect();
    let mut found: Vec<(usize, FoundTool)> = engine
        .state
        .plugins
        .values()
        .filter(|p| p.status == PluginStatus::Running)
        .flat_map(|plugin| plugin.commands.iter().map(move |c| (plugin, c)))
        .filter_map(|(plugin, command)| {
            let fields = [
                (command.title.to_lowercase(), 4),
                (command.name.to_lowercase(), 3),
                (command.description.to_lowercase(), 2),
                (plugin.name.to_lowercase(), 1),
            ];
            let score: usize = words
                .iter()
                .map(|w| {
                    fields
                        .iter()
                        .filter(|(text, _)| text.contains(w.as_str()))
                        .map(|(_, weight)| *weight)
                        .max()
                        .unwrap_or(0)
                })
                .sum();
            (words.is_empty() || score > 0).then(|| {
                let dto: uxduck_contracts::dto::plugin::DeclaredCommand = command.into();
                (
                    score,
                    FoundTool {
                        plugin_id: plugin.id.clone(),
                        plugin_name: plugin.name.clone(),
                        command: dto.name,
                        title: dto.title,
                        description: dto.description,
                        parameters: dto.parameters,
                        risk: dto.risk,
                    },
                )
            })
        })
        .collect();
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    ToolSearchResult {
        tools: found
            .into_iter()
            .take(limit as usize)
            .map(|(_, tool)| tool)
            .collect(),
    }
}

/// Runs a declared command with parameters its schema allows (X10, X11),
/// journaled under the caller.
fn command(
    engine: &mut Engine,
    connection: ConnectionId,
    PluginCommandParams {
        plugin_id,
        command,
        vehicle_id,
        params,
    }: PluginCommandParams,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    let denied = |why: &str| Err(ApiError::new(ErrorKind::CapabilityDenied, why.to_owned()));
    if !granted.contains(&Capability::Control) {
        return denied(&Capability::Control.denied_message());
    }
    let Some(plugin) = engine
        .state
        .plugins
        .get(&plugin_id)
        .filter(|p| p.status == PluginStatus::Running)
    else {
        return Err(ApiError::new(
            ErrorKind::NotFound,
            format!("no running plugin {plugin_id}"),
        ));
    };
    // Only the commands it declares (X10), with parameters its
    // schema allows (X11).
    let Some(declared) = plugin.commands.iter().find(|c| c.name == command) else {
        return Err(ApiError::new(
            ErrorKind::NotFound,
            format!(
                "the {} plugin has no command {command}; tool/search lists them",
                plugin.name
            ),
        ));
    };
    let params = params.unwrap_or_else(|| json!({}));
    let checked = match declared.parameters.as_deref() {
        Some(schema) => serde_json::from_str::<Value>(schema)
            .map_err(|e| e.to_string())
            .and_then(|schema| crate::schema::check(&schema, &params)),
        None if params.as_object().is_none_or(serde_json::Map::is_empty) => Ok(()),
        None => Err("params: this command takes none".into()),
    };
    checked.map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
    // None from the plugin's own tab, which follows no vehicle (S101).
    let vehicle_id = match vehicle_id {
        Some(id) => Some(
            engine
                .vehicle(&id)
                .map(|v| v.id.clone())
                .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no vehicle {id}")))?,
        ),
        None => None,
    };
    engine.commit(
        connection,
        Change::PluginCalled {
            plugin_id: plugin_id.clone(),
            command: command.clone(),
            vehicle_id: vehicle_id.clone(),
            params: params.to_string(),
        },
    );
    engine.plugin_host.command(
        &plugin_id,
        CommandCall {
            command,
            vehicle_id,
            params,
        },
    );
    Ok(json!({}))
}

/// The longest script a draft may be.
const SCRIPT_BYTES: usize = 64 * 1024;

fn draft(
    engine: &mut Engine,
    connection: ConnectionId,
    params: PluginDraftParams,
) -> Result<Value, ApiError> {
    let invalid = |why: String| ApiError::new(ErrorKind::InvalidValue, why);
    if !valid_id(&params.id) {
        return Err(invalid(format!(
            "plugin id {:?} must be lower case letters, digits and dashes",
            params.id
        )));
    }
    if params.name.trim().is_empty() || params.script.len() > SCRIPT_BYTES {
        return Err(invalid(format!(
            "a draft needs a name and a script of at most {SCRIPT_BYTES} bytes"
        )));
    }
    if let Some(thread) = &params.thread_id
        && !engine.state.threads.contains_key(thread)
    {
        return Err(ApiError::new(
            ErrorKind::NotFound,
            format!("no chat {thread}"),
        ));
    }
    let agent = engine
        .connections
        .get(&connection)
        .map_or_else(|| "an agent".to_owned(), |c| c.name.clone());
    let written_by = WrittenBy {
        agent,
        thread_id: params.thread_id.clone(),
    };
    let drafted = engine
        .plugin_host
        .draft(DraftOrder { params, written_by })
        .map_err(invalid)?;
    to_value(&drafted)
}

fn test(
    engine: &mut Engine,
    connection: ConnectionId,
    params: PluginTestParams,
) -> Result<Value, ApiError> {
    let Some(draft) = engine
        .plugin_host
        .drafts()
        .into_iter()
        .find(|d| d.id == params.id)
    else {
        return Err(ApiError::new(
            ErrorKind::NotFound,
            format!("no draft {}; plugin/draft writes one", params.id),
        ));
    };
    let vehicle = match &params.vehicle_id {
        Some(id) => Some(
            engine
                .vehicle(id)
                .cloned()
                .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no vehicle {id}")))?,
        ),
        None => engine.state.vehicles.values().next().cloned(),
    };
    let flight = vehicle.map(|v| crate::feedback::FlightRef {
        link_id: v.link_id,
        system_id: v.system_id,
    });
    let Some((_, request_id)) = engine.replying_to.take() else {
        return Err(ApiError::new(ErrorKind::Internal, "a test needs a reply"));
    };
    let order = TestOrder {
        connection,
        request_id: request_id.clone(),
        path: draft.path,
        flight,
        command: params.command,
        params: params.params,
    };
    if engine.plugin_host.test(order) {
        // Answered when the run ends.
        return Ok(Value::Null);
    }
    engine.replying_to = Some((connection, request_id));
    Err(ApiError::new(
        ErrorKind::InvalidRequest,
        "plugins are not available here",
    ))
}

impl Engine {
    /// A draft's test run ended: answer the request that asked.
    pub fn plugin_tested(
        &mut self,
        connection: ConnectionId,
        request_id: &Value,
        result: Result<PluginTestResult, String>,
    ) {
        let frame = match result.and_then(|r| serde_json::to_value(r).map_err(|e| e.to_string())) {
            Ok(value) => json!({ "jsonrpc": "2.0", "id": request_id, "result": value }),
            Err(why) => {
                crate::rpc::router::error(request_id, &ApiError::new(ErrorKind::InvalidValue, why))
            }
        };
        self.send(connection, &frame);
    }
}

/// At most this many keys, names, ids or commands in each list.
const MOST: usize = 64;

/// What the engine checks of a manifest; names of messages the host checks,
/// since only it knows the dialect.
fn check_manifest(manifest: &PluginManifest) -> Result<(), String> {
    if !valid_id(&manifest.id) {
        return Err(format!(
            "plugin id {:?} must be lower case letters, digits and dashes",
            manifest.id
        ));
    }
    if manifest.abi != PLUGIN_ABI {
        return Err(format!(
            "{} is built for plugin ABI v{}; this UXDuck runs v{PLUGIN_ABI}. Rebuild it with the current SDK.",
            manifest.id, manifest.abi
        ));
    }
    let cadence = |ms: u32| (20..=60_000).contains(&ms);
    let subscribe = &manifest.subscribe;
    let lists = [
        subscribe
            .items
            .as_ref()
            .map(|s| (s.keys.len(), s.cadence_ms)),
        subscribe
            .messages
            .as_ref()
            .map(|s| (s.names.len(), s.cadence_ms)),
        subscribe
            .frames
            .as_ref()
            .map(|s| (s.ids.len(), s.cadence_ms)),
    ];
    if lists
        .iter()
        .flatten()
        .any(|&(len, ms)| len > MOST || !cadence(ms))
        || manifest.sends.len() > MOST
        || manifest.commands.len() > MOST
    {
        return Err(format!(
            "a plugin subscribes to at most {MOST} keys, messages and frames, each at a cadence of 20 ms to 60 s, and sends and declares at most {MOST}"
        ));
    }
    if let Some(bad) = manifest.commands.iter().find(|c| !valid_id(&c.name)) {
        return Err(format!(
            "command {:?} must be lower case letters, digits and dashes",
            bad.name
        ));
    }
    Ok(())
}

fn install(engine: &mut Engine, connection: ConnectionId, path: &str) -> Result<Value, ApiError> {
    let actor = engine.actor(connection);
    if engine.plugin_host.included().iter().any(|p| p == path) {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "that is UXDuck's own copy of a plugin it comes with",
        ));
    }
    let plugin = install_as(engine, actor, path, None, false)?;
    to_value(&PluginDto::from(&plugin))
}

/// Installs the plugin in `path` and starts it. With `hold_back`, it gets
/// only `read`, and the rest of what its manifest asks for waits for an
/// operator here (X13).
pub(crate) fn install_as(
    engine: &mut Engine,
    actor: Actor,
    path: &str,
    origin: Option<PluginOrigin>,
    hold_back: bool,
) -> Result<Plugin, ApiError> {
    let manifest = engine
        .plugin_host
        .inspect(path)
        .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
    check_manifest(&manifest).map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
    // Installing again keeps it off if it was (S104).
    let enabled = engine
        .state
        .plugins
        .get(&manifest.id)
        .is_none_or(|p| p.enabled);
    // Installing again replaces it.
    if engine.state.plugins.contains_key(&manifest.id) {
        engine.stop_plugin(&manifest.id);
    }
    let asked: Vec<Capability> = manifest
        .capabilities
        .iter()
        .copied()
        .map(Into::into)
        .collect();
    let (capabilities, waiting): (Vec<Capability>, Vec<Capability>) = asked
        .into_iter()
        .partition(|c| !hold_back || *c == Capability::Read);
    let record = ClientRecord {
        id: record_id(ClientKind::Plugin, &manifest.id, None),
        name: manifest.name.clone(),
        kind: ClientKind::Plugin,
        executable: Some(path.to_owned()),
        capabilities: capabilities.clone(),
        created_at_us: engine.clock.now_us(),
        version: engine
            .state
            .clients
            .get(&record_id(ClientKind::Plugin, &manifest.id, None))
            .map_or(1, |r| r.version + 1),
    };
    engine.commit_as(actor.clone(), Change::ClientUpserted { client: record });
    let plugin = Plugin {
        id: manifest.id.clone(),
        name: manifest.name,
        version: manifest.version,
        description: manifest.description,
        path: path.to_owned(),
        wasm: manifest.wasm,
        script: manifest.script,
        dialect: manifest.dialect,
        capabilities,
        abi: manifest.abi,
        subscribe: (&manifest.subscribe).into(),
        sends: manifest.sends,
        commands: manifest.commands.iter().map(Into::into).collect(),
        written_by: manifest
            .written_by
            .as_ref()
            .map(|w| uxduck_core::domain::plugin::WrittenBy {
                agent: w.agent.clone(),
                thread_id: w.thread_id.clone(),
            }),
        waiting,
        origin,
        status: if enabled {
            PluginStatus::Starting
        } else {
            PluginStatus::Off
        },
        detail: None,
        panels: Vec::new(),
        revision: engine
            .state
            .plugins
            .get(&manifest.id)
            .map_or(1, |p| p.revision + 1),
        enabled,
        included: engine.plugin_host.included().iter().any(|p| p == path),
    };
    engine.commit_as(
        actor,
        Change::PluginUpserted {
            plugin: plugin.clone(),
        },
    );
    if plugin.enabled {
        engine.start_plugin(&plugin);
    }
    Ok(plugin)
}

impl Engine {
    /// Opens the plugin's own connection and asks the host to run it.
    pub(crate) fn start_plugin(&mut self, plugin: &Plugin) {
        let (outbox, inbox) = futures::channel::mpsc::unbounded();
        // Replies to its requests have nowhere to go: its outcomes are the
        // journal's receipts.
        drop(inbox);
        let id = self.connect(Transport::Socket, Trust::LocalUser, outbox);
        let record = record_id(ClientKind::Plugin, &plugin.id, None);
        if let Some(connection) = self.connections.get_mut(&id) {
            connection.client_id = plugin.client_id();
            connection.name.clone_from(&plugin.name);
            connection.admitted = Some(Admitted {
                kind: ClientKind::Plugin,
                operator: false,
                record,
                granted: plugin.capabilities.clone(),
                extra: Vec::new(),
                approvals: false,
            });
        }
        self.plugin_connections.insert(plugin.id.clone(), id);
        let module = std::path::Path::new(&plugin.path)
            .join(plugin.script.as_deref().unwrap_or(&plugin.wasm))
            .display()
            .to_string();
        self.plugin_host.load(PluginLoad {
            id: plugin.id.clone(),
            module,
            script: plugin.script.is_some(),
            subscribe: (&plugin.subscribe).into(),
            dialect: plugin.dialect.as_ref().map(|file| {
                std::path::Path::new(&plugin.path)
                    .join(file)
                    .display()
                    .to_string()
            }),
        });
    }

    pub(crate) fn stop_plugin(&mut self, id: &str) {
        self.clear_features(id);
        self.clear_own_items(id);
        self.plugin_host.unload(id);
        if let Some(connection) = self.plugin_connections.remove(id) {
            self.disconnect(connection);
        }
    }

    /// Every installed plugin starts with the daemon, unless an operator
    /// turned it off; the ones UXDuck comes with are installed first, or
    /// again when UXDuck brings a new version of one (S104).
    pub(crate) fn start_plugins(&mut self) {
        // Installing starts them: they are not started twice.
        let just_installed = self.install_included();
        let installed: Vec<Plugin> = self.state.plugins.values().cloned().collect();
        for mut plugin in installed {
            if !plugin.enabled || just_installed.contains(&plugin.id) {
                continue;
            }
            if plugin.status != PluginStatus::Starting {
                plugin.status = PluginStatus::Starting;
                plugin.detail = None;
                plugin.revision += 1;
                self.commit_as(
                    daemon(),
                    Change::PluginUpserted {
                        plugin: plugin.clone(),
                    },
                );
            }
            // Built before v2: it would not understand what it is sent.
            if plugin.abi != PLUGIN_ABI {
                let id = plugin.id.clone();
                self.plugin_failed(
                    &id,
                    &format!(
                        "It is built for plugin ABI v{}; this UXDuck runs v{PLUGIN_ABI}. Rebuild it with the current SDK and install it again.",
                        plugin.abi
                    ),
                );
                continue;
            }
            self.start_plugin(&plugin);
        }
    }

    /// The plugins UXDuck comes with (S104): installed where they are not,
    /// and installed again where UXDuck brings a new version. Their own
    /// copy is the host's, written out at each start.
    fn install_included(&mut self) -> Vec<String> {
        let mut installed = Vec::new();
        for path in self.plugin_host.included() {
            let Ok(manifest) = self.plugin_host.inspect(&path) else {
                continue;
            };
            let current = self
                .state
                .plugins
                .get(&manifest.id)
                .is_some_and(|p| p.version == manifest.version && p.path == path);
            if current {
                continue;
            }
            match install_as(self, daemon(), &path, None, false) {
                Ok(plugin) => installed.push(plugin.id),
                Err(error) => tracing_line(&manifest.id, &error.message),
            }
        }
        installed
    }

    /// Turns a plugin off or on (S104): off stops it and keeps it; on
    /// starts it again. Journaled under whoever asked.
    pub(crate) fn set_plugin_enabled(
        &mut self,
        connection: ConnectionId,
        id: &str,
        enabled: bool,
    ) -> Result<Plugin, ApiError> {
        let Some(mut plugin) = self.state.plugins.get(id).cloned() else {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                format!("no plugin {id}"),
            ));
        };
        if plugin.enabled == enabled {
            return Ok(plugin);
        }
        plugin.enabled = enabled;
        plugin.detail = None;
        plugin.revision += 1;
        if enabled {
            plugin.status = PluginStatus::Starting;
        } else {
            self.stop_plugin(id);
            self.plugin_usage.remove(id);
            self.resolve(&failed_condition(id));
            plugin.status = PluginStatus::Off;
            plugin.panels.clear();
        }
        self.commit(
            connection,
            Change::PluginUpserted {
                plugin: plugin.clone(),
            },
        );
        if enabled {
            self.start_plugin(&plugin);
        }
        Ok(plugin)
    }

    /// The host's account of what running plugins used (S104).
    pub fn plugins_used(&mut self, usage: Vec<PluginUsage>) {
        self.plugin_usage = usage
            .into_iter()
            .filter(|u| {
                self.state
                    .plugins
                    .get(&u.plugin_id)
                    .is_some_and(|p| p.status == PluginStatus::Running)
            })
            .map(|u| (u.plugin_id.clone(), u))
            .collect();
    }

    /// The host loaded a plugin and it described itself.
    pub fn plugin_described(&mut self, id: &str, description: Description) {
        let Some(mut plugin) = self.state.plugins.get(id).cloned() else {
            return;
        };
        // Turned off while it was loading.
        if !plugin.enabled {
            return;
        }
        plugin.status = PluginStatus::Running;
        plugin.detail = None;
        plugin.panels = description.panels.into_iter().map(Into::into).collect();
        plugin.revision += 1;
        self.commit_as(daemon(), Change::PluginUpserted { plugin });
        self.resolve(&failed_condition(id));
    }

    /// The host stopped a plugin: it would not load, trapped, or ran out of
    /// time or memory.
    pub fn plugin_failed(&mut self, id: &str, reason: &str) {
        let Some(mut plugin) = self.state.plugins.get(id).cloned() else {
            return;
        };
        if let Some(connection) = self.plugin_connections.remove(id) {
            self.disconnect(connection);
        }
        self.clear_features(id);
        self.clear_own_items(id);
        plugin.status = PluginStatus::Failed;
        plugin.detail = Some(reason.to_owned());
        plugin.revision += 1;
        let name = plugin.name.clone();
        self.commit_as(daemon(), Change::PluginUpserted { plugin });
        self.raise(Raise {
            severity: Severity::Warning,
            kind: NotificationKind::Persistent,
            subject: Subject::System,
            title: format!("The {name} plugin stopped"),
            body: Some(reason.to_owned()),
            actions: Vec::new(),
            condition: Some(failed_condition(id)),
            report: Some("plugin-stopped".into()),
        });
    }

    /// What a plugin function returned: its commands go through the
    /// ordinary path on its own connection; its messages go out only if its
    /// manifest lists them; its items join the vehicle's live telemetry when
    /// their keys say their units; its notifications are raised under its
    /// name.
    pub fn plugin_effects(&mut self, id: &str, effects: Effects) {
        let Some(connection) = self.plugin_connections.get(id).copied() else {
            return;
        };
        let Some(plugin) = self.state.plugins.get(id).cloned() else {
            return;
        };
        for line in &effects.log {
            tracing_line(id, line);
        }
        for (n, send) in effects.commands.into_iter().enumerate() {
            let request = json!({
                "jsonrpc": "2.0",
                "id": format!("{id}-{n}"),
                "method": "command/send",
                "params": { "vehicleId": send.vehicle_id, "command": send.command }
            });
            self.receive(connection, &request.to_string());
        }
        self.plugin_messages(&plugin, effects.messages);
        self.plugin_features(&plugin, effects.features, &effects.removed_features);
        self.plugin_items(&plugin, effects.items);
        for notification in effects
            .notifications
            .into_iter()
            .take(NOTIFICATIONS_PER_CALL)
        {
            let subject = notification
                .vehicle_id
                .as_deref()
                .and_then(|v| self.vehicle(v))
                .map_or(Subject::System, |v| Subject::Vehicle { id: v.id.clone() });
            self.raise(Raise {
                severity: notification.severity.into(),
                kind: NotificationKind::Transient,
                subject,
                title: format!("{}: {}", plugin.name, notification.title),
                body: notification.body,
                actions: Vec::new(),
                condition: None,
                report: None,
            });
        }
    }

    /// Messages by name, to vehicles, only those the manifest allows (X4).
    fn plugin_messages(&mut self, plugin: &Plugin, messages: Vec<SendMessage>) {
        let may_control = plugin.capabilities.contains(&Capability::Control);
        for message in messages {
            if !plugin.sends.contains(&message.name) || !may_control {
                tracing_line(
                    &plugin.id,
                    &format!(
                        "{} is not in its manifest's sends, or it may not control",
                        message.name
                    ),
                );
                continue;
            }
            let Some(vehicle_id) = self.vehicle(&message.vehicle_id).map(|v| v.id.clone()) else {
                continue;
            };
            self.links.message(MessageOrder {
                vehicle_id,
                name: message.name,
                fields: message.fields,
                plugin_id: plugin.id.clone(),
            });
        }
    }

    /// Items with their units; a key must end in its unit's suffix (X2).
    /// A vehicle's join its live telemetry; the plugin's own, with no
    /// vehicle, go to the connections that asked for them (S101).
    fn plugin_items(&mut self, plugin: &Plugin, items: Vec<ProducedItem>) {
        let now = self.clock.now_us();
        let mut by_vehicle: std::collections::BTreeMap<String, Vec<TelemetryItem>> =
            std::collections::BTreeMap::new();
        let mut own = Vec::new();
        for item in items {
            let unit: Unit = item.unit.into();
            if !key_says_unit(&item.key, unit) {
                tracing_line(
                    &plugin.id,
                    &format!("{}: the key does not say its unit", item.key),
                );
                continue;
            }
            let rate_per_s = item.rate_per_s.filter(|r| r.is_finite());
            let telemetry = TelemetryItem {
                key: item.key,
                label: item.label,
                unit,
                value: item.value,
                text: item.text,
                source: format!("plugin:{}", plugin.id),
                component_id: 0,
                time_us: now,
            };
            match item.vehicle_id {
                Some(vehicle_id) => by_vehicle.entry(vehicle_id).or_default().push(telemetry),
                None => own.push(uxduck_contracts::dto::telemetry::TelemetryItem {
                    rate_per_s,
                    ..(&telemetry).into()
                }),
            }
        }
        self.plugin_own(&plugin.id, own);
        for (vehicle_id, items) in by_vehicle {
            if self.vehicle(&vehicle_id).is_some() {
                self.telemetry_observed(VehicleTelemetry {
                    vehicle_id,
                    positions: Vec::new(),
                    items,
                });
            }
        }
    }
}

impl Engine {
    /// One plugin's layer as the map draws it.
    fn plugin_layer(&self, id: &str) -> PluginLayer {
        PluginLayer {
            plugin_id: id.to_owned(),
            name: self
                .state
                .plugins
                .get(id)
                .map_or_else(|| id.to_owned(), |p| p.name.clone()),
            features: self
                .plugin_layers
                .get(id)
                .map(|f| f.values().cloned().collect())
                .unwrap_or_default(),
        }
    }

    /// Features drawn, replaced by id, or taken off (X4); at most
    /// `FEATURES` per plugin, each small.
    fn plugin_features(&mut self, plugin: &Plugin, features: Vec<MapFeature>, removed: &[String]) {
        if features.is_empty() && removed.is_empty() {
            return;
        }
        let layer = self.plugin_layers.entry(plugin.id.clone()).or_default();
        for id in removed {
            layer.remove(id);
        }
        for feature in features {
            let size = feature.geometry.to_string().len();
            if size > FEATURE_BYTES || (layer.len() >= FEATURES && !layer.contains_key(&feature.id))
            {
                tracing_line(
                    &plugin.id,
                    &format!("feature {} is too large or too many", feature.id),
                );
                continue;
            }
            layer.insert(feature.id.clone(), feature);
        }
        self.push_layer(&plugin.id);
    }

    /// A plugin's own items as `plugin/items` lists them.
    fn own_items(&self, id: &str) -> PluginItems {
        PluginItems {
            plugin_id: id.to_owned(),
            items: self
                .plugin_own_items
                .get(id)
                .map(|items| items.values().cloned().collect())
                .unwrap_or_default(),
        }
    }

    /// The plugin's own items, the latest of each key, at most
    /// `OWN_ITEMS`; sent on when any changed.
    fn plugin_own(
        &mut self,
        id: &str,
        items: Vec<uxduck_contracts::dto::telemetry::TelemetryItem>,
    ) {
        if items.is_empty() {
            return;
        }
        let held = self.plugin_own_items.entry(id.to_owned()).or_default();
        let mut changed = false;
        for item in items {
            if held.len() >= OWN_ITEMS && !held.contains_key(&item.key) {
                tracing_line(id, &format!("{}: too many items of its own", item.key));
                continue;
            }
            let same = held.get(&item.key).is_some_and(|h| {
                h.value == item.value
                    && h.text == item.text
                    && h.label == item.label
                    && h.rate_per_s == item.rate_per_s
            });
            if !same {
                changed = true;
            }
            held.insert(item.key.clone(), item);
        }
        if changed {
            self.push_own_items(id);
        }
    }

    fn clear_own_items(&mut self, id: &str) {
        if self.plugin_own_items.remove(id).is_some() {
            self.push_own_items(id);
        }
    }

    fn push_own_items(&mut self, id: &str) {
        let frame = json!({
            "jsonrpc": "2.0",
            "method": "plugin/itemsChanged",
            "params": self.own_items(id),
        });
        let watchers: Vec<ConnectionId> = self.item_watchers.iter().copied().collect();
        for connection in watchers {
            self.send(connection, &frame);
        }
    }

    fn clear_features(&mut self, id: &str) {
        if self.plugin_layers.remove(id).is_some() {
            self.push_layer(id);
        }
    }

    fn push_layer(&mut self, id: &str) {
        let frame = json!({
            "jsonrpc": "2.0",
            "method": "plugin/featuresChanged",
            "params": self.plugin_layer(id),
        });
        let watchers: Vec<ConnectionId> = self.feature_watchers.iter().copied().collect();
        for connection in watchers {
            self.send(connection, &frame);
        }
    }
}

/// At most this many features per plugin, each geometry at most this many
/// bytes of JSON.
const FEATURES: usize = 200;
const FEATURE_BYTES: usize = 64 * 1024;

/// A plugin keeps at most this many items of its own.
const OWN_ITEMS: usize = 64;

/// A plugin raises at most this many notifications in one call.
const NOTIFICATIONS_PER_CALL: usize = 4;

fn tracing_line(id: &str, line: &str) {
    // The engine has no logger of its own; plugin lines are rare and short.
    let _ = (id, line);
}

/// The wire form of a plugin change.
pub(crate) fn entity_changes(change: &Change) -> Vec<EntityChange> {
    match change {
        Change::PluginUpserted { plugin } => vec![EntityChange {
            plugin: Some(plugin.into()),
            ..upsert(EntityType::Plugin, &plugin.id)
        }],
        Change::PluginRemoved { id } => vec![EntityChange {
            op: uxduck_contracts::dto::state::ChangeOp::Remove,
            ..upsert(EntityType::Plugin, id)
        }],
        _ => Vec::new(),
    }
}
