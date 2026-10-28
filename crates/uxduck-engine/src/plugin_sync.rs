//! Plugins on every computer, and shared (S59, ADR-0006 X13).
//!
//! **Mine.** A plugin installed from a folder here goes into the account:
//! the sync task uploads its bundle and reports its digest through
//! [`Engine::plugin_published`], which journals a `PluginPublished` that
//! sync carries like any entity. Every other computer of the operator's
//! fetches the bundle ([`Engine::plugins_to_fetch`]) and installs it
//! ([`Engine::plugin_fetched`]) with `read` only: anything more waits for an
//! operator there (`plugin/allow`), so a stolen account cannot give a
//! plugin control everywhere. Each new version waits again. Removing it
//! where it came from removes it everywhere; removing a synced copy removes
//! it here only, until a new version comes.
//!
//! **Shared.** A link any signed-in account may install from, each with its
//! own prompt (the install page), through the account task.

use serde_json::Value;
use uxduck_contracts::dto::link::IdParams;
use uxduck_contracts::dto::plugin::{Plugin as PluginDto, PluginShareParams, PluginTokenParams};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::{Capability, ClientKind};
use uxduck_core::domain::notification::{NotificationKind, Severity, Subject};
use uxduck_core::domain::plugin::{Plugin, PluginOrigin, PluginStatus, PublishedPlugin};
use uxduck_core::events::{Actor, Change};

use crate::account::AccountAction;
use crate::approvals::record_id;
use crate::commands::Raise;
use crate::plugins::{daemon, install_as};
use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// An installed plugin whose bundle the sync task keeps in the account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToPublish {
    pub id: String,
    /// Its folder.
    pub path: String,
    /// Its module or script, relative to `path`.
    pub module: String,
    /// The digest the account has, if any.
    pub published: Option<String>,
}

/// A link's bundle unpacked by the account task, ready to install.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchedShare {
    pub path: String,
    pub shared_by: String,
    pub token: String,
}

fn waiting_condition(id: &str) -> String {
    format!("plugin-waiting:{id}")
}

fn words(capability: Capability) -> &'static str {
    match capability {
        Capability::Read => "read",
        Capability::Control => "command vehicles",
        Capability::Config => "change links and settings",
        Capability::Approve => "answer approvals",
        Capability::Location => "see where this computer is",
    }
}

/// As the manifest and the API name it.
fn name(capability: Capability) -> &'static str {
    match capability {
        Capability::Read => "read",
        Capability::Control => "control",
        Capability::Config => "config",
        Capability::Approve => "approve",
        Capability::Location => "location",
    }
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
    operator: bool,
) -> Result<Value, ApiError> {
    let reads = matches!(
        request,
        ClientRequest::PluginShares(_) | ClientRequest::PluginShared(_)
    );
    if !(operator || reads && granted.contains(&Capability::Read)) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "only an operator client may allow, share or install plugins",
        ));
    }
    let action = match request {
        ClientRequest::PluginAllow(IdParams { id }) => return allow(engine, connection, &id),
        ClientRequest::PluginShare(PluginShareParams { id }) => {
            let plugin = engine
                .state
                .plugins
                .get(&id)
                .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no plugin {id}")))?;
            AccountAction::SharePlugin {
                plugin_id: plugin.id.clone(),
                name: plugin.name.clone(),
                version: plugin.version.clone(),
                path: plugin.path.clone(),
                module: plugin.script.clone().unwrap_or_else(|| plugin.wasm.clone()),
                capabilities: plugin
                    .capabilities
                    .iter()
                    .chain(&plugin.waiting)
                    .map(|c| name(*c).to_owned())
                    .collect(),
            }
        }
        ClientRequest::PluginShares(_) => AccountAction::PluginShares,
        ClientRequest::PluginUnshare(PluginTokenParams { token }) => {
            AccountAction::Unshare { token }
        }
        ClientRequest::PluginShared(PluginTokenParams { token }) => {
            AccountAction::SharedPlugin { token }
        }
        ClientRequest::PluginInstallShared(PluginTokenParams { token }) => {
            AccountAction::InstallShared { token }
        }
        _ => return Err(ApiError::new(ErrorKind::Internal, "not a plugin method")),
    };
    if engine.account.is_none() {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "sharing plugins needs UXDuck Desktop, signed in",
        ));
    }
    let installs = matches!(action, AccountAction::InstallShared { .. });
    let id = engine.ask_account(action);
    if installs {
        engine.awaiting_shared_installs.insert(id);
    }
    Ok(Value::Null)
}

/// Grants what waits, and restarts the plugin with it.
fn allow(engine: &mut Engine, connection: ConnectionId, id: &str) -> Result<Value, ApiError> {
    let mut plugin = engine
        .state
        .plugins
        .get(id)
        .cloned()
        .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no plugin {id}")))?;
    if !plugin.waiting.is_empty() {
        let waiting = std::mem::take(&mut plugin.waiting);
        plugin.capabilities.extend(waiting);
        let record = record_id(ClientKind::Plugin, &plugin.id, None);
        if let Some(mut client) = engine.state.clients.get(&record).cloned() {
            client.capabilities.clone_from(&plugin.capabilities);
            client.version += 1;
            engine.commit(connection, Change::ClientUpserted { client });
        }
        engine.stop_plugin(&plugin.id);
        plugin.status = PluginStatus::Starting;
        plugin.detail = None;
        plugin.revision += 1;
        engine.commit(
            connection,
            Change::PluginUpserted {
                plugin: plugin.clone(),
            },
        );
        engine.start_plugin(&plugin);
        engine.resolve(&waiting_condition(id));
        engine.plugin_checks_changed(id, false);
    }
    to_value(&PluginDto::from(&plugin))
}

impl Engine {
    /// Plugins installed from a folder here, for the sync task to keep in
    /// the account. None while sync is off.
    #[must_use]
    pub fn plugins_to_publish(&self) -> Vec<ToPublish> {
        if !self.state.sync.enabled {
            return Vec::new();
        }
        self.state
            .plugins
            .values()
            .filter(|p| !matches!(p.origin, Some(PluginOrigin::Synced { .. })))
            // Every UXDuck has its own copy of these (S104).
            .filter(|p| !p.included)
            .map(|p| ToPublish {
                id: p.id.clone(),
                path: p.path.clone(),
                module: p.script.clone().unwrap_or_else(|| p.wasm.clone()),
                published: self
                    .state
                    .published
                    .get(&p.id)
                    .filter(|published| !published.removed)
                    .map(|published| published.digest.clone()),
            })
            .collect()
    }

    /// The account has the plugin's bundle under `digest`: journal it, for
    /// sync to carry to the operator's other computers.
    pub fn plugin_published(&mut self, id: &str, digest: &str) {
        let Some(plugin) = self.state.plugins.get(id) else {
            return;
        };
        let published = PublishedPlugin {
            id: plugin.id.clone(),
            name: plugin.name.clone(),
            version: plugin.version.clone(),
            digest: digest.to_owned(),
            capabilities: plugin
                .capabilities
                .iter()
                .chain(&plugin.waiting)
                .copied()
                .collect(),
            at_us: self.clock.now_us(),
            removed: false,
        };
        let same = self
            .state
            .published
            .get(id)
            .is_some_and(|p| !p.removed && p.digest == published.digest);
        if !same {
            self.commit_as(daemon(), Change::PluginPublished { plugin: published });
        }
    }

    /// Plugins in the account to install or update here: not installed
    /// from a folder here, and not declined at this version.
    #[must_use]
    pub fn plugins_to_fetch(&self) -> Vec<PublishedPlugin> {
        if !self.state.sync.enabled {
            return Vec::new();
        }
        self.state
            .published
            .values()
            .filter(|p| !p.removed)
            .filter(|p| self.state.declined_plugins.get(&p.id) != Some(&p.digest))
            .filter(|p| match self.state.plugins.get(&p.id) {
                None => true,
                Some(Plugin {
                    origin: Some(PluginOrigin::Synced { digest }),
                    ..
                }) => *digest != p.digest,
                Some(_) => false,
            })
            .cloned()
            .collect()
    }

    /// The sync task unpacked a plugin from the account into `path`:
    /// installed with `read`, the rest waiting for an operator here. False
    /// when it did not install, which says why in a notification.
    pub fn plugin_fetched(&mut self, id: &str, digest: &str, path: &str) -> bool {
        let Some(published) = self.state.published.get(id).cloned() else {
            return false;
        };
        if published.digest != digest || published.removed {
            return false;
        }
        let origin = Some(PluginOrigin::Synced {
            digest: digest.to_owned(),
        });
        let actor = Actor {
            client_id: format!("{}plugins", crate::sync::FROM_ACCOUNT),
            operator_id: None,
            identity: None,
        };
        match install_as(self, actor, path, origin, true, None) {
            Ok(plugin) if !plugin.waiting.is_empty() => {
                let wants: Vec<&str> = plugin.waiting.iter().map(|c| words(*c)).collect();
                self.raise(Raise {
                    severity: Severity::Info,
                    kind: NotificationKind::Persistent,
                    subject: Subject::System,
                    title: format!("The {} plugin is waiting for you", plugin.name),
                    body: Some(format!(
                        "It came from another of your computers. For now it can only read. It also wants to {}. You can allow that on the Plugins page.",
                        wants.join(" and ")
                    )),
                    actions: Vec::new(),
                    condition: Some(waiting_condition(id)),
                    report: None,
                });
                true
            }
            Ok(_) => true,
            Err(why) => {
                self.raise(Raise {
                    severity: Severity::Warning,
                    kind: NotificationKind::Persistent,
                    subject: Subject::System,
                    title: format!(
                        "The {} plugin from your other computers did not install",
                        published.name
                    ),
                    body: Some(why.message),
                    actions: Vec::new(),
                    condition: Some(waiting_condition(id)),
                    report: None,
                });
                false
            }
        }
    }

    /// A plugin removed here: withdrawn from the account if it was
    /// installed from a folder here; remembered as declined if it came from
    /// the account.
    pub(crate) fn plugin_removed_here(&mut self, connection: ConnectionId, removed: &Plugin) {
        self.resolve(&waiting_condition(&removed.id));
        if let Some(PluginOrigin::Synced { digest }) = &removed.origin {
            self.commit(
                connection,
                Change::PluginDeclined {
                    id: removed.id.clone(),
                    digest: digest.clone(),
                },
            );
            return;
        }
        let Some(mut published) = self.state.published.get(&removed.id).cloned() else {
            return;
        };
        if !published.removed {
            published.removed = true;
            published.at_us = self.clock.now_us();
            self.commit(connection, Change::PluginPublished { plugin: published });
        }
    }

    /// A `PluginPublished` taken from the account: a removal removes the
    /// synced copy here.
    pub(crate) fn published_taken(&mut self, actor: &Actor, published: &PublishedPlugin) {
        if !published.removed {
            return;
        }
        let synced = self
            .state
            .plugins
            .get(&published.id)
            .is_some_and(|p| matches!(p.origin, Some(PluginOrigin::Synced { .. })));
        if synced {
            self.stop_plugin(&published.id);
            self.commit_as(
                actor.clone(),
                Change::PluginRemoved {
                    id: published.id.clone(),
                },
            );
            self.resolve(&waiting_condition(&published.id));
            self.resolve(&crate::plugins::failed_condition(&published.id));
        }
    }

    /// The account task unpacked a shared plugin: install it with what it
    /// asks for, and answer the request.
    pub(crate) fn shared_fetched(&mut self, id: &str, result: Result<Value, ApiError>) {
        let Some((connection, request_id)) = self.awaiting_account.remove(id) else {
            return;
        };
        let installed = result.and_then(|value| {
            let fetched: FetchedShare = serde_json::from_value(value).map_err(|_| {
                ApiError::new(ErrorKind::Internal, "the shared plugin was not understood")
            })?;
            let origin = Some(PluginOrigin::Shared {
                shared_by: fetched.shared_by,
                token: fetched.token,
            });
            let actor = self.actor(connection);
            let plugin = install_as(self, actor, &fetched.path, origin, false, None)?;
            to_value(&PluginDto::from(&plugin))
        });
        let frame = match installed {
            Ok(value) => serde_json::json!({ "jsonrpc": "2.0", "id": request_id, "result": value }),
            Err(error) => crate::rpc::router::error(&request_id, &error),
        };
        self.send(connection, &frame);
    }
}
