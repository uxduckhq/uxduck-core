//! Client records, live capability edits, and escalation instead of
//! rejection (ADR-0005). When a non-operator client asks for something
//! outside its capabilities, the request is held as an approval and put to
//! the operators; on allow it runs as if it had been granted, on deny or at
//! the deadline the client gets `capability-denied`. The core never waits:
//! the request is simply answered later.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uxduck_contracts::dto::analytics::{ApprovalDecision, DaemonEvent};
use uxduck_contracts::dto::client::{
    Approval as ApprovalDto, ApprovalListResult, ApprovalResolveParams, CapabilitiesChanged,
    ClientListResult, ClientRecord as ClientDto, ClientRow, ClientSetCapabilitiesParams,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::{ClientRequest, ServerNotification};
use uxduck_core::domain::approval::{Approval, ApprovalState, Remember};
use uxduck_core::domain::client::{Capability, ClientKind, ClientRecord};
use uxduck_core::domain::notification::{
    Action, NotificationAction, NotificationKind, Severity, Subject,
};
use uxduck_core::events::{Actor, Change};

use crate::commands::Raise;
use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine, rpc};

/// How long an operator has to answer before the safe default, deny.
pub const APPROVAL_DEADLINE_US: u64 = 60_000_000;

/// A request waiting for its approval.
#[derive(Debug)]
pub(crate) struct Held {
    connection: ConnectionId,
    request_id: Value,
    request: ClientRequest,
    capability: Capability,
}

fn daemon() -> Actor {
    Actor {
        client_id: "daemon".into(),
        operator_id: None,
    }
}

/// The record id for a client identity: stable, so the same agent finds its
/// record again.
#[must_use]
pub fn record_id(kind: ClientKind, name: &str, executable: Option<&str>) -> String {
    let digest = Sha256::digest(format!("{kind:?}\n{name}\n{}", executable.unwrap_or("")));
    let hex = digest.iter().take(8).fold(String::new(), |mut out, byte| {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
        out
    });
    format!("cl{hex}")
}

/// The capability a request needs that can be asked for, if any. Methods
/// only operators may call (pairing, the trace) are not escalated.
fn escalatable(request: &ClientRequest) -> Option<Capability> {
    match request {
        ClientRequest::CommandSend(_)
        | ClientRequest::MissionUpload(_)
        | ClientRequest::PluginCommand(_)
        | ClientRequest::PracticeGuide(_) => Some(Capability::Control),
        ClientRequest::ParamSet(_)
        | ClientRequest::PluginInstall(_)
        | ClientRequest::VehicleAddNote(_)
        | ClientRequest::VideoAssign(_)
        | ClientRequest::VideoAdd(_)
        | ClientRequest::VideoRemove(_)
        | ClientRequest::VideoRecord(_)
        | ClientRequest::VehicleEditNote(_)
        | ClientRequest::VehicleDeleteNote(_)
        | ClientRequest::MappingUpdate(_)
        | ClientRequest::SettingsUpdate(_)
        | ClientRequest::PlaceSave(_)
        | ClientRequest::PlaceUpdate(_)
        | ClientRequest::PlaceRemove(_)
        | ClientRequest::LinkAdd(_)
        | ClientRequest::LinkRemove(_)
        | ClientRequest::LinkEnable(_)
        | ClientRequest::LinkDisable(_)
        | ClientRequest::LinkUpdate(_)
        | ClientRequest::SerialList(_)
        | ClientRequest::VehicleSave(_)
        | ClientRequest::VehicleLabel(_)
        | ClientRequest::VehicleSize(_)
        | ClientRequest::VehicleDismiss(_)
        | ClientRequest::VehicleDelete(_)
        | ClientRequest::OperatorCreate(_)
        | ClientRequest::OperatorUpdate(_)
        | ClientRequest::OperatorRemove(_)
        | ClientRequest::SimStart(_)
        | ClientRequest::SimStop(_)
        | ClientRequest::SimRestart(_)
        | ClientRequest::SimSnapshot(_)
        | ClientRequest::SnapshotStart(_)
        | ClientRequest::SnapshotRename(_)
        | ClientRequest::SnapshotDelete(_)
        | ClientRequest::PracticeStart(_)
        | ClientRequest::FilesUpload(_)
        | ClientRequest::FilesRemove(_)
        | ClientRequest::FilesMkdir(_)
        | ClientRequest::FilesRename(_) => Some(Capability::Config),
        // Searching the network asks first; looking nearby does not (F10).
        request @ (ClientRequest::FindStart(_) | ClientRequest::FindVehicles(_)) => {
            crate::finds::needed(request).filter(|c| *c == Capability::Config)
        }
        _ => None,
    }
}

impl Engine {
    /// Finds or creates the record for a connection being admitted, and
    /// returns what it is granted.
    pub(crate) fn admit_record(
        &mut self,
        kind: ClientKind,
        name: &str,
        executable: Option<&str>,
        operator: bool,
        default: Vec<Capability>,
    ) -> (String, Vec<Capability>) {
        let id = record_id(kind, name, executable);
        // Operators always hold everything: nothing to remember, and no
        // journal entry for every CLI call.
        if operator {
            return (id, Capability::ALL.to_vec());
        }
        if let Some(record) = self.state.clients.get(&id) {
            return (id, record.capabilities.clone());
        }
        let capabilities = default;
        let record = ClientRecord {
            id: id.clone(),
            name: name.to_owned(),
            kind,
            executable: executable.map(str::to_owned),
            capabilities: capabilities.clone(),
            created_at_us: self.clock.now_us(),
            version: 1,
        };
        self.commit_as(daemon(), Change::ClientUpserted { client: record });
        (id, capabilities)
    }

    /// What a connection may do right now: its record's set, what was
    /// allowed for the connection, and a one-off allowance being replayed.
    pub(crate) fn granted(&mut self, connection: ConnectionId) -> Vec<Capability> {
        let one_shot = self.one_shot.take();
        let Some(admitted) = self
            .connections
            .get(&connection)
            .and_then(|c| c.admitted.as_ref())
        else {
            return Vec::new();
        };
        let mut granted = admitted.granted.clone();
        granted.extend(admitted.extra.iter().copied());
        granted.extend(one_shot);
        granted.sort();
        granted.dedup();
        granted
    }

    /// Holds a request that needs a capability its non-operator client lacks
    /// and asks the operators. True when it was held (answered later).
    pub(crate) fn escalate(
        &mut self,
        connection: ConnectionId,
        request: &ClientRequest,
        granted: &[Capability],
    ) -> bool {
        let Some(capability) = escalatable(request) else {
            return false;
        };
        // An agent's install always asks (X12): no capability skips it, and
        // only the operator's "allow once", now running it, lets it through.
        // So does a Critical command from away that remote control does not
        // allow (S80): the people at the computer say yes each time. And a
        // plugin's command it declares dangerous: "always allow" for the
        // `control` it needs must not let every later one through unasked.
        let install = matches!(request, ClientRequest::PluginInstall(_));
        let always = install
            || self.remote_critical_asks(connection, request)
            || self.dangerous_plugin_command(request);
        if always {
            if self.running_approved {
                return false;
            }
        } else if granted.contains(&capability) {
            return false;
        }
        let Some((record, name)) = self
            .connections
            .get(&connection)
            .and_then(|c| c.admitted.as_ref())
            .filter(|a| !a.operator)
            .and_then(|a| {
                self.state
                    .clients
                    .get(&a.record)
                    .map(|r| (r.id.clone(), r.name.clone()))
            })
        else {
            return false;
        };
        let Some((_, request_id)) = self.replying_to.take() else {
            return false;
        };
        let now = self.clock.now_us();
        let approval = Approval {
            id: format!("ap{}", &self.random.token()[..16]),
            client_id: record.clone(),
            client_name: name.clone(),
            capability,
            method: method_of(request),
            summary: self.summary(request),
            state: ApprovalState::Pending,
            remember: None,
            deadline_us: now + APPROVAL_DEADLINE_US,
            created_at_us: now,
            version: 1,
        };
        let id = approval.id.clone();
        self.held.insert(
            id.clone(),
            Held {
                connection,
                request_id,
                request: request.clone(),
                capability,
            },
        );
        let dto = ApprovalDto::from(&approval);
        let title = format!("{name} wants to {}", approval.summary);
        self.commit_as(daemon(), Change::ApprovalUpserted { approval });
        self.broadcast_approval(&ServerNotification::ApprovalRequested(dto));
        let action = |label: &str, allow: bool, remember: Remember| NotificationAction {
            label: label.into(),
            action: Action::ResolveApproval {
                approval_id: id.clone(),
                allow,
                remember,
            },
        };
        self.raise(Raise {
            severity: Severity::Warning,
            kind: NotificationKind::Persistent,
            subject: Subject::Client { id: record },
            title,
            body: Some(format!(
                "It needs the {} capability. Denied in {} s if nobody answers.",
                format!("{capability:?}").to_lowercase(),
                APPROVAL_DEADLINE_US / 1_000_000
            )),
            actions: if always {
                vec![
                    action(
                        if install { "Install" } else { "Allow once" },
                        true,
                        Remember::Once,
                    ),
                    action("Deny", false, Remember::Once),
                ]
            } else {
                vec![
                    action("Allow once", true, Remember::Once),
                    action("Allow for this connection", true, Remember::Connection),
                    action("Always allow", true, Remember::Always),
                    action("Deny", false, Remember::Once),
                ]
            },
            condition: Some(format!("approval:{id}")),
            report: None,
        });
        true
    }

    /// A plugin's command that the plugin itself declares dangerous.
    fn dangerous_plugin_command(&self, request: &ClientRequest) -> bool {
        let ClientRequest::PluginCommand(params) = request else {
            return false;
        };
        self.state
            .plugins
            .get(&params.plugin_id)
            .and_then(|p| p.commands.iter().find(|c| c.name == params.command))
            .is_some_and(|c| c.risk == uxduck_core::domain::command::Risk::Dangerous)
    }

    /// Words for a plugin's approval: what an install may do and where it
    /// was written; which command runs on which vehicle.
    fn plugin_summary(&self, request: &ClientRequest) -> String {
        match request {
            ClientRequest::PluginInstall(params) => match self.plugin_host.inspect(&params.path) {
                Ok(manifest) => {
                    let may: Vec<String> = manifest
                        .capabilities
                        .iter()
                        .map(|c| format!("{c:?}").to_lowercase())
                        .collect();
                    let chat = manifest
                        .written_by
                        .as_ref()
                        .and_then(|w| w.thread_id.as_ref())
                        .and_then(|t| self.state.threads.get(t))
                        .map(|t| format!(", written in the chat “{}”", t.title))
                        .unwrap_or_default();
                    format!(
                        "install the plugin “{}” {}, which may {}{chat}",
                        manifest.name,
                        manifest.version,
                        may.join(", ")
                    )
                }
                Err(_) => format!("install the plugin in {}", params.path),
            },
            ClientRequest::PluginCommand(params) => {
                let plugin = self.state.plugins.get(&params.plugin_id);
                let title = plugin
                    .and_then(|p| p.commands.iter().find(|c| c.name == params.command))
                    .map_or(params.command.as_str(), |c| c.title.as_str());
                let on = params.vehicle_id.as_deref().map_or_else(String::new, |id| {
                    let name = self.vehicle(id).map_or(id, |v| v.name.as_str());
                    format!(" on {name}")
                });
                format!(
                    "run the {} plugin's “{title}”{on}",
                    plugin.map_or(params.plugin_id.as_str(), |p| p.name.as_str())
                )
            }
            other => format!("call {}", method_of(other)),
        }
    }

    /// Words for an approval: "arm Copter 1".
    /// What a video request does, in a few words.
    fn video_summary(&self, request: &ClientRequest) -> String {
        match request {
            ClientRequest::VideoAssign(params) => match &params.vehicle_id {
                Some(id) => {
                    let name = self
                        .vehicle(id)
                        .map_or_else(|| id.clone(), |v| v.name.clone());
                    format!("say the camera {} belongs to {name}", params.source_id)
                }
                None => format!(
                    "match the camera {} to a vehicle by its address",
                    params.source_id
                ),
            },
            ClientRequest::VideoAdd(params) => format!(
                "add the camera {}",
                uxduck_core::domain::video::redact(&params.url)
            ),
            ClientRequest::VideoRemove(params) => match (&params.source_id, params.port) {
                (Some(id), _) => format!("remove the camera {id}"),
                (None, Some(port)) => format!("stop listening for video on UDP {port}"),
                (None, None) => "remove nothing".into(),
            },
            ClientRequest::VideoRecord(params) => format!(
                "{} recording the camera {}",
                if params.on { "start" } else { "stop" },
                params.source_id
            ),
            ClientRequest::FindStart(_) | ClientRequest::FindVehicles(_) => {
                let subnets = self.finding_subnets().join(", ");
                if subnets.is_empty() {
                    "search the local network for vehicles".into()
                } else {
                    format!("search the local network ({subnets}) for vehicles")
                }
            }
            _ => String::new(),
        }
    }

    #[allow(clippy::too_many_lines, reason = "one line per request asked about")]
    fn summary(&self, request: &ClientRequest) -> String {
        match request {
            ClientRequest::CommandSend(params) => {
                let command: uxduck_core::domain::command::Command = params.command.clone().into();
                let name = self
                    .vehicle(&params.vehicle_id)
                    .map_or_else(|| params.vehicle_id.clone(), |v| v.name.clone());
                format!("{} {name}", command.label().to_lowercase())
            }
            ClientRequest::ParamSet(params) => {
                let name = self
                    .vehicle(&params.vehicle_id)
                    .map_or_else(|| params.vehicle_id.clone(), |v| v.name.clone());
                // "set RTL_ALT from 1500 to 3000 cm on Copter 1" (S68).
                let now = self
                    .state
                    .parameters
                    .get(&uxduck_core::domain::parameter::parameter_id(
                        &params.vehicle_id,
                        &params.name,
                    ))
                    .map(|p| p.value);
                let helicopter = self
                    .vehicle(&params.vehicle_id)
                    .is_some_and(|v| v.vehicle_type == "Helicopter");
                let units = self
                    .vehicle_definitions(&params.vehicle_id)
                    .and_then(|set| {
                        set.find(&params.name, helicopter)
                            .and_then(|d| d.units.clone())
                    })
                    .map_or_else(String::new, |u| format!(" {u}"));
                match now {
                    Some(now) => format!(
                        "set {} from {now} to {}{units} on {name}",
                        params.name, params.value
                    ),
                    None => format!("set {} to {}{units} on {name}", params.name, params.value),
                }
            }
            ClientRequest::VehicleAddNote(params) => {
                let name = self
                    .vehicle(&params.vehicle_id)
                    .map_or_else(|| params.vehicle_id.clone(), |v| v.name.clone());
                format!("note “{}” in {name}'s history", params.text.trim())
            }
            ClientRequest::VehicleEditNote(_) => "edit a note in a vehicle's history".into(),
            request @ (ClientRequest::VideoAssign(_)
            | ClientRequest::VideoAdd(_)
            | ClientRequest::VideoRemove(_)
            | ClientRequest::VideoRecord(_)
            | ClientRequest::FindStart(_)
            | ClientRequest::FindVehicles(_)) => self.video_summary(request),
            ClientRequest::VehicleDeleteNote(_) => "delete a note from a vehicle's history".into(),
            // The file and the vehicle, named (ADR-0027 F6).
            ClientRequest::FilesUpload(params) => format!(
                "send {} to {}",
                params.path,
                self.vehicle_name(&params.vehicle_id)
            ),
            ClientRequest::FilesRemove(params) => format!(
                "delete {} from {}",
                params.path,
                self.vehicle_name(&params.vehicle_id)
            ),
            ClientRequest::FilesMkdir(params) => format!(
                "make the folder {} on {}",
                params.path,
                self.vehicle_name(&params.vehicle_id)
            ),
            ClientRequest::FilesRename(params) => format!(
                "rename {} to {} on {}",
                params.from,
                params.to,
                self.vehicle_name(&params.vehicle_id)
            ),
            ClientRequest::MissionUpload(params) => {
                let name = self
                    .vehicle(&params.vehicle_id)
                    .map_or_else(|| params.vehicle_id.clone(), |v| v.name.clone());
                format!("upload a mission of {} items to {name}", params.items.len())
            }
            ClientRequest::LinkAdd(_) => "add a link".into(),
            ClientRequest::LinkRemove(_) => "remove a link".into(),
            ClientRequest::SimStart(params) => format!(
                "start a simulated {}",
                match params.vehicle {
                    uxduck_contracts::dto::link::SimVehicle::Plane => "plane",
                    uxduck_contracts::dto::link::SimVehicle::Multicopter => "multicopter",
                    uxduck_contracts::dto::link::SimVehicle::Vtol => "VTOL",
                    uxduck_contracts::dto::link::SimVehicle::Rover => "rover",
                    uxduck_contracts::dto::link::SimVehicle::Boat => "boat",
                    uxduck_contracts::dto::link::SimVehicle::Submarine => "submarine",
                    uxduck_contracts::dto::link::SimVehicle::Airship => "blimp",
                }
            ),
            ClientRequest::SimStop(_) => "stop a simulated vehicle".into(),
            ClientRequest::PracticeStart(params) => format!(
                "start the practice exercise \"{}\" on {}",
                crate::practice::exercise(&params.exercise_id)
                    .map_or(params.exercise_id.as_str(), |e| e.title.as_str()),
                self.vehicle(&params.vehicle_id)
                    .map_or_else(|| params.vehicle_id.clone(), |v| v.name.clone())
            ),
            ClientRequest::PracticeGuide(params) => format!(
                "start the Get started guide on {}",
                self.vehicle(&params.vehicle_id)
                    .map_or_else(|| params.vehicle_id.clone(), |v| v.name.clone())
            ),
            ClientRequest::SimRestart(_) => {
                "start a simulated vehicle again from where it started".into()
            }
            ClientRequest::SimSnapshot(_) => "save a snapshot of a simulated vehicle".into(),
            ClientRequest::SnapshotStart(params) => format!(
                "start a simulation from the snapshot {}",
                self.state
                    .snapshots
                    .get(&params.id)
                    .map_or(params.id.as_str(), |s| s.name.as_str())
            ),
            ClientRequest::PluginInstall(_) | ClientRequest::PluginCommand(_) => {
                self.plugin_summary(request)
            }
            other => format!("call {}", method_of(other)),
        }
    }

    fn broadcast_approval(&mut self, notification: &ServerNotification) {
        let frame = rpc::router::notification(notification);
        let targets: Vec<ConnectionId> = self
            .connections
            .iter()
            .filter(|(_, c)| c.admitted.as_ref().is_some_and(|a| a.approvals))
            .map(|(id, _)| *id)
            .collect();
        for id in targets {
            self.send(id, &frame);
        }
    }

    /// Ends an approval: allowed ones run now with the allowance, the rest
    /// answer `capability-denied`.
    fn finish(&mut self, id: &str, state: ApprovalState, remember: Option<Remember>, why: &str) {
        let Some(mut approval) = self.state.approvals.get(id).cloned() else {
            return;
        };
        approval.state = state;
        approval.remember = remember;
        approval.version += 1;
        let dto = ApprovalDto::from(&approval);
        let record = approval.client_id.clone();
        self.commit_as(daemon(), Change::ApprovalUpserted { approval });
        self.track(DaemonEvent::ApprovalResolved {
            decision: match (state, remember) {
                (ApprovalState::Allowed, Some(Remember::Connection)) => {
                    ApprovalDecision::AllowConnection
                }
                (ApprovalState::Allowed, Some(Remember::Always)) => ApprovalDecision::AllowAlways,
                (ApprovalState::Allowed, _) => ApprovalDecision::AllowOnce,
                (ApprovalState::Denied, _) => ApprovalDecision::Deny,
                (ApprovalState::Expired | ApprovalState::Pending, _) => ApprovalDecision::Expired,
            },
        });
        self.resolve(&format!("approval:{id}"));
        self.broadcast_approval(&ServerNotification::ApprovalResolved(dto));
        let Some(held) = self.held.remove(id) else {
            return;
        };
        if state != ApprovalState::Allowed {
            let error = ApiError::new(
                ErrorKind::CapabilityDenied,
                format!("{why} (approval {id})"),
            );
            self.send(
                held.connection,
                &rpc::router::error(&held.request_id, &error),
            );
            return;
        }
        // An install is never remembered: the next one asks again (X12).
        let remember = if matches!(held.request, ClientRequest::PluginInstall(_)) {
            Remember::Once
        } else {
            remember.unwrap_or(Remember::Once)
        };
        match remember {
            Remember::Once => self.one_shot = Some(held.capability),
            Remember::Connection => {
                if let Some(admitted) = self
                    .connections
                    .get_mut(&held.connection)
                    .and_then(|c| c.admitted.as_mut())
                {
                    admitted.extra.push(held.capability);
                }
            }
            Remember::Always => {
                if let Some(mut client) = self.state.clients.get(&record).cloned() {
                    if !client.capabilities.contains(&held.capability) {
                        client.capabilities.push(held.capability);
                        client.capabilities.sort();
                    }
                    client.version += 1;
                    self.store_capabilities(client);
                }
            }
        }
        // Run it as the client, answering its original request. The
        // operator's own `approval/resolve` keeps its reply slot.
        let outer = self.replying_to.replace((held.connection, held.request_id));
        self.running_approved = true;
        let result = rpc::handlers::handle(self, held.connection, held.request);
        self.running_approved = false;
        self.one_shot = None;
        if let Some((connection, request_id)) = self.replying_to.take() {
            let frame = match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": request_id, "result": value }),
                Err(error) => rpc::router::error(&request_id, &error),
            };
            self.send(connection, &frame);
        }
        self.replying_to = outer;
    }

    /// Journals a record's capabilities and applies them to its open
    /// connections at once.
    fn store_capabilities(&mut self, client: ClientRecord) {
        let capabilities = client.capabilities.clone();
        let id = client.id.clone();
        self.commit_as(daemon(), Change::ClientUpserted { client });
        let live: Vec<ConnectionId> = self
            .connections
            .iter()
            .filter(|(_, c)| {
                c.admitted
                    .as_ref()
                    .is_some_and(|a| a.record == id && !a.operator)
            })
            .map(|(cid, _)| *cid)
            .collect();
        let frame = rpc::router::notification(&ServerNotification::ClientCapabilitiesChanged(
            CapabilitiesChanged {
                granted: capabilities.iter().copied().map(Into::into).collect(),
            },
        ));
        for connection in live {
            if let Some(admitted) = self
                .connections
                .get_mut(&connection)
                .and_then(|c| c.admitted.as_mut())
            {
                admitted.granted.clone_from(&capabilities);
                // An edit replaces what was allowed for the connection too.
                admitted.extra.clear();
            }
            self.send(connection, &frame);
        }
    }

    /// Applies the safe default to approvals past their deadline.
    pub fn expire_approvals(&mut self, now_us: u64) {
        let due: Vec<String> = self
            .state
            .approvals
            .values()
            .filter(|a| a.deadline_us <= now_us)
            .map(|a| a.id.clone())
            .collect();
        for id in due {
            self.finish(&id, ApprovalState::Expired, None, "nobody answered in time");
        }
    }

    /// A client went away: its held requests cannot be answered.
    pub(crate) fn abandon_approvals(&mut self, connection: ConnectionId) {
        let gone: Vec<String> = self
            .held
            .iter()
            .filter(|(_, h)| h.connection == connection)
            .map(|(id, _)| id.clone())
            .collect();
        for id in gone {
            self.held.remove(&id);
            self.finish(&id, ApprovalState::Expired, None, "the client disconnected");
        }
    }

    /// Pending approvals left by a previous run cannot be answered.
    pub(crate) fn expire_stale_approvals(&mut self) {
        let stale: Vec<String> = self.state.approvals.keys().cloned().collect();
        for id in stale {
            self.finish(&id, ApprovalState::Expired, None, "the daemon restarted");
        }
    }
}

/// Operator clients connected now, as records (they are not journaled).
fn live_operators(engine: &Engine) -> Vec<ClientRecord> {
    let mut seen = std::collections::BTreeMap::new();
    for connection in engine.connections.values() {
        let Some(admitted) = connection.admitted.as_ref().filter(|a| a.operator) else {
            continue;
        };
        seen.entry(admitted.record.clone())
            .or_insert_with(|| ClientRecord {
                id: admitted.record.clone(),
                name: connection.name.clone(),
                kind: admitted.kind,
                executable: connection.executable.clone(),
                capabilities: Capability::ALL.to_vec(),
                created_at_us: 0,
                version: 0,
            });
    }
    seen.into_values().collect()
}

fn method_of(request: &ClientRequest) -> String {
    serde_json::to_value(request)
        .ok()
        .and_then(|v| v["method"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
    operator: bool,
) -> Result<Value, ApiError> {
    let denied = |why: &str| ApiError::new(ErrorKind::CapabilityDenied, why.to_owned());
    match request {
        ClientRequest::ClientList(_) if operator => list_clients(engine),
        ClientRequest::ClientSetCapabilities(params) if operator => {
            set_capabilities(engine, params)
        }
        ClientRequest::ClientList(_) | ClientRequest::ClientSetCapabilities(_) => {
            Err(denied("only an operator client may do this"))
        }
        ClientRequest::ApprovalList(_) | ClientRequest::ApprovalResolve(_)
            if !granted.contains(&Capability::Approve) =>
        {
            Err(denied(&Capability::Approve.denied_message()))
        }
        ClientRequest::ApprovalList(_) => {
            let mut approvals: Vec<ApprovalDto> =
                engine.state.approvals.values().map(Into::into).collect();
            approvals.sort_by_key(|a| a.created_at_us);
            to_value(&ApprovalListResult { approvals })
        }
        ClientRequest::ApprovalResolve(params) => resolve_approval(engine, connection, params),
        _ => Err(ApiError::new(ErrorKind::Internal, "not a client method")),
    }
}

fn list_clients(engine: &Engine) -> Result<Value, ApiError> {
    let records: Vec<ClientRecord> = engine
        .state
        .clients
        .values()
        .cloned()
        .chain(live_operators(engine))
        .collect();
    let mut clients: Vec<ClientRow> = records
        .iter()
        .map(|record| ClientRow {
            client: ClientDto::from(record),
            connections: u32::try_from(
                engine
                    .connections
                    .values()
                    .filter(|c| c.admitted.as_ref().is_some_and(|a| a.record == record.id))
                    .count(),
            )
            .unwrap_or(u32::MAX),
            operator: record.kind.is_operator(),
        })
        .collect();
    clients.sort_by(|a, b| {
        (b.connections > 0, &a.client.name).cmp(&(a.connections > 0, &b.client.name))
    });
    to_value(&ClientListResult { clients })
}

fn set_capabilities(
    engine: &mut Engine,
    ClientSetCapabilitiesParams {
        id,
        expected_version,
        capabilities,
    }: ClientSetCapabilitiesParams,
) -> Result<Value, ApiError> {
    let Some(mut client) = engine.state.clients.get(&id).cloned() else {
        return Err(ApiError::new(
            ErrorKind::NotFound,
            format!("no client {id}"),
        ));
    };
    if client.kind.is_operator() {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "operators always hold every capability",
        ));
    }
    if client.version != expected_version {
        let mut error = ApiError::new(
            ErrorKind::VersionConflict,
            format!(
                "client {id} is at version {}, not {expected_version}",
                client.version
            ),
        );
        error.data.current_version = Some(client.version);
        return Err(error);
    }
    let mut capabilities: Vec<Capability> = capabilities.into_iter().map(Into::into).collect();
    capabilities.sort();
    capabilities.dedup();
    client.capabilities = capabilities;
    client.version += 1;
    let result = ClientDto::from(&client);
    engine.store_capabilities(client);
    to_value(&result)
}

fn resolve_approval(
    engine: &mut Engine,
    connection: ConnectionId,
    ApprovalResolveParams {
        id,
        allow,
        remember,
    }: ApprovalResolveParams,
) -> Result<Value, ApiError> {
    let Some(mut done) = engine.state.approvals.get(&id).cloned() else {
        return Err(ApiError::new(
            ErrorKind::NotFound,
            format!("no pending approval {id}; it may have been answered already"),
        ));
    };
    let own = engine
        .connections
        .get(&connection)
        .and_then(|c| c.admitted.as_ref())
        .is_some_and(|a| a.record == done.client_id);
    if own {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "a client cannot answer its own approval",
        ));
    }
    let remember = allow.then(|| remember.map_or(Remember::Once, Into::into));
    if allow {
        engine.finish(&id, ApprovalState::Allowed, remember, "");
    } else {
        engine.finish(&id, ApprovalState::Denied, None, "an operator denied it");
    }
    done.state = if allow {
        ApprovalState::Allowed
    } else {
        ApprovalState::Denied
    };
    done.remember = remember;
    done.version += 1;
    to_value(&ApprovalDto::from(&done))
}
