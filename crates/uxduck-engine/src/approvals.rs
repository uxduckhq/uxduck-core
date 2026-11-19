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
use uxduck_core::domain::settings::AgentCameras;
use uxduck_core::domain::thread::{ItemKind, ItemStatus};
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
        identity: None,
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
#[expect(clippy::too_many_lines, reason = "one arm per method that can ask")]
fn escalatable(request: &ClientRequest) -> Option<Capability> {
    match request {
        ClientRequest::SimFrames(params) if !params.refresh => None,
        ClientRequest::CommandSend(_)
        | ClientRequest::MissionUpload(_)
        | ClientRequest::PlanUpload(_)
        | ClientRequest::MissionRestore(_)
        | ClientRequest::TransferCancel(_)
        | ClientRequest::PluginCommand(_)
        | ClientRequest::PracticeGuide(_)
        | ClientRequest::SimPause(_)
        | ClientRequest::SimFault(_)
        | ClientRequest::SimCrash(_) => Some(Capability::Control),
        ClientRequest::ParamSet(_)
        // Applying a saved parameter set is a parameter write (S217).
        | ClientRequest::ParamSetApply(_)
        // A vehicle's checklist steps (S193, ADR-0038 K11): an agent asks
        // first; ticking is a person's and never asked for.
        | ClientRequest::ChecklistUpdate(_)
        | ClientRequest::ChecklistCopy(_)
        | ClientRequest::PluginInstall(_)
        | ClientRequest::VehicleAddNote(_)
        | ClientRequest::VideoAssign(_)
        | ClientRequest::VideoDevices(_)
        | ClientRequest::VideoFile(_)
        | ClientRequest::VideoDemos(_)
        | ClientRequest::VideoCapture(_)
        | ClientRequest::VideoCaptureSettings(_)
        | ClientRequest::VideoCaptureEnable(_)
        | ClientRequest::VideoAdd(_)
        | ClientRequest::VideoRemove(_)
        | ClientRequest::VideoRecord(_)
        | ClientRequest::VideoRecordAll(_)
        | ClientRequest::VideoRecordWithOthers(_)
        | ClientRequest::VideoPhoto(_)
        | ClientRequest::VideoVehicleCamera(_)
        | ClientRequest::VideoServedBy(_)
        | ClientRequest::VehicleEditNote(_)
        | ClientRequest::VehicleDeleteNote(_)
        | ClientRequest::MappingUpdate(_)
        | ClientRequest::SettingsUpdate(_)
        | ClientRequest::PlaceSave(_)
        | ClientRequest::PlaceUpdate(_)
        | ClientRequest::PlaceRemove(_)
        | ClientRequest::MarkerAdd(_)
        | ClientRequest::MarkerUpdate(_)
        | ClientRequest::MarkerRemove(_)
        | ClientRequest::PlanCreate(_)
        | ClientRequest::PlanUpdate(_)
        | ClientRequest::PlanUndo(_)
        | ClientRequest::PlanRedo(_)
        | ClientRequest::PlanPlace(_)
        | ClientRequest::PlanDelete(_)
        | ClientRequest::PlanRestore(_)
        | ClientRequest::PlanImport(_)
        | ClientRequest::LinkAdd(_)
        | ClientRequest::LinkRemove(_)
        | ClientRequest::LinkEnable(_)
        | ClientRequest::LinkDisable(_)
        | ClientRequest::LinkUpdate(_)
        | ClientRequest::SerialList(_)
        | ClientRequest::VehicleSave(_)
        | ClientRequest::VehicleLabel(_)
        | ClientRequest::VehicleSize(_)
        | ClientRequest::VehicleLook(_)
        | ClientRequest::VehicleColour(_)
        | ClientRequest::VehicleGripper(_)
        | ClientRequest::VehicleBookmarks(_)
        | ClientRequest::VehicleDismiss(_)
        | ClientRequest::VehicleHide(_)
        | ClientRequest::VehicleForgetConnection(_)
        | ClientRequest::VehicleDelete(_)
        | ClientRequest::OperatorCreate(_)
        | ClientRequest::OperatorUpdate(_)
        | ClientRequest::OperatorRemove(_)
        | ClientRequest::SimStart(_)
        // Plans flown in simulation (S293): new simulated vehicles, so an
        // agent asks first, as for one.
        | ClientRequest::PlanSimulate(_)
        | ClientRequest::SimStop(_)
        | ClientRequest::SimRestart(_)
        | ClientRequest::SimDownload(_)
        | ClientRequest::SimFrames(_)
        | ClientRequest::SimSnapshot(_)
        | ClientRequest::SnapshotStart(_)
        | ClientRequest::SnapshotRename(_)
        | ClientRequest::SnapshotDelete(_)
        | ClientRequest::PracticeStart(_)
        | ClientRequest::FilesUpload(_)
        | ClientRequest::FilesRemove(_)
        | ClientRequest::FilesMkdir(_)
        | ClientRequest::FilesRename(_)
        // Terrain areas written to a vehicle's card (ADR-0051 A11).
        | ClientRequest::TerrainPutArea(_)
        // Operations (ADR-0039 R16): an agent gathers flights after asking.
        | ClientRequest::OperationCreate(_)
        | ClientRequest::OperationRename(_)
        | ClientRequest::OperationAddFlights(_)
        | ClientRequest::OperationRemoveFlights(_)
        | ClientRequest::FlightRename(_)
        // A video kept with a flight (S246): an agent asks first, even for
        // one the operator attached.
        | ClientRequest::FlightAddVideo(_)
        // Which descriptions a vehicle uses (ADR-0044 F10).
        | ClientRequest::ParamDefinitionsUse(_)
        // Which dialects a vehicle uses (F13).
        | ClientRequest::MavlinkDialectsUse(_) => Some(Capability::Config),
        // A camera look (FB-244), asked under "Ask" by `camera_look_asks`.
        ClientRequest::VideoFrame(_) => Some(Capability::Read),
        // Where this computer is (ADR-0030 L4): an agent asks first, and
        // "Always allow" remembers it for that agent.
        ClientRequest::LocationStatus(_) => Some(Capability::Location),
        // A terminal on a vehicle's computer (ADR-0065 T7): every client
        // but the person's own on this computer asks, in
        // `terminals::asked`'s words. Resizing and ending one are never
        // asked for. One command an agent runs there (T14) is behind the
        // same capability and asked in the same place's words, with the
        // command itself shown whole.
        ClientRequest::TerminalOpen(_) | ClientRequest::ComputerRun(_) => {
            Some(Capability::Terminal)
        }
        // Searching the network asks first; looking nearby does not (F10).
        request @ (ClientRequest::FindStart(_) | ClientRequest::FindVehicles(_)) => {
            crate::finds::needed(request).filter(|c| *c == Capability::Config)
        }
        // Everything else, setup procedures among them (`setup/start`,
        // `setup/cancel`): one needs a person at the vehicle (ADR-0037 V4),
        // so it is never asked for on a client's behalf.
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
    #[allow(
        clippy::too_many_lines,
        reason = "one approval's words, actions and notice, read top to bottom"
    )]
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
        // So does a plugin's command it declares dangerous: "always allow" for the
        // `control` it needs must not let every later one through unasked.
        let install = matches!(request, ClientRequest::PluginInstall(_));
        // Launching past failing checks set to hold always asks (ADR-0038
        // K24), listing them: a client never overrides them on its own.
        let preflight = self.preflight_asks(request);
        let camera = self.camera_look_asks(connection, request);
        // Whether the assistant sees cameras is a person's choice: an
        // agent's change to it always asks, whatever Config it was given.
        let setting = self.camera_setting_asks(request);
        // Taking a gimbal from whoever has it always asks (S406), as a
        // person's own is confirmed.
        let always = install
            || camera.is_some()
            || setting.is_some()
            || self.dangerous_plugin_command(request)
            || self.spoken_turn_asks(connection, request)
            || self.gimbal_asks(request)
            || !preflight.is_empty();
        if (always && self.running_approved) || (!always && granted.contains(&capability)) {
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
        let terminal = crate::terminals::asked(self, request, &name);
        let words = camera.as_ref().or(setting.as_ref()).or(terminal.as_ref());
        let (summary, body) = match words {
            Some(words) => (words.summary.clone(), words.body.clone()),
            None => self.approval_words(request, capability, &preflight),
        };
        let item_id = self.waiting_tool_call(connection, &method_of(request));
        let approval = Approval {
            id: format!("ap{}", &self.random.token()[..16]),
            client_id: record.clone(),
            client_name: name.clone(),
            capability,
            method: method_of(request),
            summary,
            question: words.map(|words| words.question.clone()),
            preflight,
            item_id,
            shell_command: crate::terminals::asked_command(request),
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
        let title = approval
            .question
            .clone()
            .unwrap_or_else(|| format!("{name} wants to {}", approval.summary));
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
            body: Some(body),
            actions: if camera.is_some() {
                // "Always allow" chooses it in Settings › Privacy for every
                // chat and connection, so later looks run unasked.
                vec![
                    action("Allow once", true, Remember::Once),
                    action("Always allow", true, Remember::Always),
                    action("Don't allow", false, Remember::Once),
                ]
            } else if always {
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

    /// A look through a camera that a person allows first (FB-244): under
    /// "Ask", from a chat's tool call, through a camera there is. The words
    /// it is asked in. Any other look is refused or run by `video/frame`.
    fn camera_look_asks(&self, connection: ConnectionId, request: &ClientRequest) -> Option<Asked> {
        let ClientRequest::VideoFrame(params) = request else {
            return None;
        };
        if self.state.settings.agent_cameras != AgentCameras::Ask {
            return None;
        }
        self.waiting_tool_call(connection, &method_of(request))?;
        let asked = crate::looks::asked(params).ok()?;
        let source = self.look_source(params).ok()?;
        let seen = source
            .vehicle_id
            .as_ref()
            .and_then(|id| self.vehicle(id))
            .map_or_else(
                || source.name.clone(),
                |v| format!("{} on {}", source.name, v.name),
            );
        let what = crate::looks::asked_words(&asked);
        let deadline = APPROVAL_DEADLINE_US / 1_000_000;
        Some(Asked {
            summary: format!("see {seen}, {what}"),
            question: format!("Can the assistant see {seen}? It asked for {what}."),
            body: format!(
                "The pictures go to this chat's AI provider and are not saved. Not allowed in \
                 {deadline} s if nobody answers."
            ),
        })
    }

    /// A client's change to whether the assistant sees cameras (FB-244),
    /// in the words it is asked in. An operator changes it unasked.
    fn camera_setting_asks(&self, request: &ClientRequest) -> Option<Asked> {
        let ClientRequest::SettingsUpdate(params) = request else {
            return None;
        };
        let choice: AgentCameras = params.agent_cameras?.into();
        if choice == self.state.settings.agent_cameras {
            return None;
        }
        let (summary, question) = match choice {
            AgentCameras::Always => (
                "always see your cameras without asking",
                "The assistant wants to always see your cameras without asking.",
            ),
            AgentCameras::Ask => (
                "ask before each look through your cameras",
                "The assistant wants to ask before each look through your cameras.",
            ),
            AgentCameras::Never => (
                "never see your cameras",
                "The assistant wants to stop seeing your cameras.",
            ),
        };
        let deadline = APPROVAL_DEADLINE_US / 1_000_000;
        Some(Asked {
            summary: summary.into(),
            question: question.into(),
            body: format!(
                "This changes {} for every chat and connection. Not allowed in {deadline} s if \
                 nobody answers.",
                crate::looks::CAMERAS_SETTING
            ),
        })
    }

    /// Whether the request comes from a turn said aloud (ADR-0012 V6): its
    /// commands are approved by hand each time, whatever was allowed before,
    /// since words heard are not always the operator's.
    fn spoken_turn_asks(&self, connection: ConnectionId, request: &ClientRequest) -> bool {
        self.waiting_tool_call(connection, &method_of(request))
            .and_then(|id| self.state.items.get(&id))
            .and_then(|item| self.turns.get(&item.thread_id))
            .is_some_and(|turn| self.spoken_turns.contains_key(turn))
    }

    /// The chat tool call an in-app agent on `connection` is waiting on for
    /// `method`: recorded just before the call, still in progress. The
    /// newest wins, in case an earlier call to the same method was left open.
    fn waiting_tool_call(&self, connection: ConnectionId, method: &str) -> Option<String> {
        let client_id = &self.connections.get(&connection)?.client_id;
        self.state
            .items
            .values()
            .filter(|item| {
                item.kind == ItemKind::ToolCall
                    && item.status == ItemStatus::InProgress
                    && self.turns.contains_key(&item.thread_id)
                    && item
                        .tool_call
                        .as_ref()
                        .is_some_and(|call| &call.client_id == client_id && call.method == method)
            })
            .max_by_key(|item| (item.created_at_us, item.index))
            .map(|item| item.id.clone())
    }

    /// An approval's summary and its notice's words: with failing preflight
    /// checks, which, and that allowing it launches past them (ADR-0038 K24).
    fn approval_words(
        &self,
        request: &ClientRequest,
        capability: Capability,
        preflight: &[uxduck_core::domain::preflight::CheckRef],
    ) -> (String, String) {
        let deadline = APPROVAL_DEADLINE_US / 1_000_000;
        if preflight.is_empty() {
            return (
                self.summary(request),
                format!(
                    "It needs the {} capability. Denied in {deadline} s if nobody answers.",
                    format!("{capability:?}").to_lowercase()
                ),
            );
        }
        let titles: Vec<&str> = preflight.iter().map(|c| c.title.as_str()).collect();
        let summary = format!(
            "{} past {} failing preflight {}: {}",
            self.summary(request),
            preflight.len(),
            if preflight.len() == 1 {
                "check"
            } else {
                "checks"
            },
            titles.join(", ")
        );
        let list: Vec<String> = preflight
            .iter()
            .map(|c| match &c.detail {
                Some(detail) => format!("{}: {detail}", c.title),
                None => c.title.clone(),
            })
            .collect();
        let body = format!(
            "{}\nAllowing it launches past them. Denied in {deadline} s if nobody answers.",
            list.join("\n")
        );
        (summary, body)
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
            ClientRequest::VideoFile(_) => "play prerecorded footage as a camera".into(),
            ClientRequest::VideoDemos(_) => "manage optional demo footage downloads".into(),
            ClientRequest::VideoDevices(_) => "list cameras attached to this computer".into(),
            ClientRequest::VideoCaptureSettings(_) => {
                "open camera privacy settings on this computer".into()
            }
            ClientRequest::VideoCapture(params) => {
                format!("capture video from {}", params.input.name)
            }
            ClientRequest::VideoCaptureEnable(params) => format!(
                "{} camera capture {}",
                if params.enabled { "enable" } else { "pause" },
                params.source_id
            ),
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
            ClientRequest::VideoPhoto(params) => {
                format!("keep a photo from the camera {}", params.source_id)
            }
            ClientRequest::VideoRecordAll(params) => format!(
                "{}{}",
                if params.on {
                    "start recording every camera that records with the others"
                } else {
                    "stop recording every camera"
                },
                params
                    .vehicle_id
                    .as_deref()
                    .map(|id| format!(" of the vehicle {id}"))
                    .unwrap_or_default()
            ),
            ClientRequest::VideoRecordWithOthers(params) => format!(
                "{} the camera {} when the others record",
                if params.on { "include" } else { "leave out" },
                params.source_id
            ),
            ClientRequest::VideoVehicleCamera(params) => format!(
                "{} the vehicle's own camera along with the camera {}",
                if params.on { "use" } else { "stop using" },
                params.source_id
            ),
            ClientRequest::VideoServedBy(params) => match &params.computer_id {
                Some(id) => {
                    let name = self
                        .state
                        .vehicles
                        .values()
                        .flat_map(|v| &v.companions)
                        .find(|c| &c.id == id)
                        .map_or_else(|| id.clone(), |c| c.name.clone());
                    format!(
                        "say the companion computer {name} serves the camera {}",
                        params.source_id
                    )
                }
                None => format!(
                    "find the companion computer serving the camera {} by its address",
                    params.source_id
                ),
            },
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
                // "take a photo on Copter 1" (S405), "point the gimbal on
                // Copter 1" (S406).
                let on = if matches!(
                    command,
                    uxduck_core::domain::command::Command::Camera { .. }
                        | uxduck_core::domain::command::Command::Gimbal { .. }
                ) {
                    " on"
                } else {
                    ""
                };
                let command = self.gimbal_addressed(&params.vehicle_id, command);
                let taking = self
                    .gimbal_held_elsewhere(&params.vehicle_id, &command)
                    .map_or_else(String::new, |holder| format!(", taking it from {holder}"));
                // A camera's own setting keeps its name as the camera
                // spells it: "set the camera's CAM_ISO to 800" (S411).
                let label = command.label();
                let said = match &command {
                    uxduck_core::domain::command::Command::Camera { action, .. }
                        if action.setting().is_some() =>
                    {
                        let mut letters = label.chars();
                        letters.next().map_or_else(String::new, |first| {
                            first.to_lowercase().chain(letters).collect()
                        })
                    }
                    _ => label.to_lowercase(),
                };
                format!("{said}{on} {name}{taking}")
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
                    .map(|p| match p.kind {
                        // A float the vehicle keeps in 32 bits, without the
                        // digits widening it adds (0.6, not 0.6000000238418579).
                        #[allow(clippy::cast_possible_truncation)]
                        uxduck_core::domain::parameter::ParamKind::Real32 => {
                            (p.value as f32).to_string()
                        }
                        _ => p.value.to_string(),
                    });
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
            ClientRequest::ParamSetApply(params) => {
                let name = self
                    .vehicle(&params.vehicle_id)
                    .map_or_else(|| params.vehicle_id.clone(), |v| v.name.clone());
                let set = self
                    .state
                    .saved_params
                    .get(&params.id)
                    .map_or_else(|| params.id.clone(), |s| s.name.clone());
                format!("apply the saved parameters “{set}” to {name}")
            }
            ClientRequest::VehicleAddNote(params) => {
                let name = self
                    .vehicle(&params.vehicle_id)
                    .map_or_else(|| params.vehicle_id.clone(), |v| v.name.clone());
                format!("note “{}” in {name}'s history", params.text.trim())
            }
            ClientRequest::VehicleEditNote(_) => "edit a note in a vehicle's history".into(),
            request @ (ClientRequest::VideoAssign(_)
            | ClientRequest::VideoDevices(_)
            | ClientRequest::VideoFile(_)
            | ClientRequest::VideoDemos(_)
            | ClientRequest::VideoCapture(_)
            | ClientRequest::VideoCaptureSettings(_)
            | ClientRequest::VideoCaptureEnable(_)
            | ClientRequest::VideoAdd(_)
            | ClientRequest::VideoRemove(_)
            | ClientRequest::VideoRecord(_)
            | ClientRequest::VideoRecordAll(_)
            | ClientRequest::VideoRecordWithOthers(_)
            | ClientRequest::VideoPhoto(_)
            | ClientRequest::VideoVehicleCamera(_)
            | ClientRequest::VideoServedBy(_)
            | ClientRequest::FindStart(_)
            | ClientRequest::FindVehicles(_)) => self.video_summary(request),
            ClientRequest::VehicleDeleteNote(_) => "delete a note from a vehicle's history".into(),
            ClientRequest::LocationStatus(_) => "see where this computer is".into(),
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
            ClientRequest::TerrainPutArea(params) => {
                let vehicle = self.vehicle_name(&params.vehicle_id);
                let area = params.area_id.as_ref().map(|id| {
                    self.state
                        .settings
                        .terrain_areas
                        .iter()
                        .find(|a| &a.id == id)
                        .map_or_else(|| id.clone(), |a| a.name.clone())
                });
                match (area, params.remove) {
                    (Some(area), true) => {
                        format!("remove the terrain area “{area}” from {vehicle}'s card")
                    }
                    (Some(area), false) => {
                        format!("write the terrain area “{area}” to {vehicle}'s card")
                    }
                    (None, _) => format!("write the enabled terrain areas to {vehicle}'s card"),
                }
            }
            ClientRequest::MissionUpload(params) => {
                let name = self
                    .vehicle(&params.vehicle_id)
                    .map_or_else(|| params.vehicle_id.clone(), |v| v.name.clone());
                format!("upload a mission of {} items to {name}", params.items.len())
            }
            ClientRequest::MissionRestore(params) => {
                let held = self
                    .state
                    .mission_records
                    .get(&params.record_id)
                    .map(|r| r.vehicle_id.clone());
                let id = params.vehicle_id.clone().or(held).unwrap_or_default();
                let name = self.vehicle(&id).map_or(id, |v| v.name.clone());
                format!("put an earlier mission back on {name}")
            }
            ClientRequest::LinkAdd(_) => "add a link".into(),
            ClientRequest::LinkRemove(_) => "remove a link".into(),
            ClientRequest::SimStart(params) => format!(
                "start a simulated {}{}",
                match params.vehicle {
                    uxduck_contracts::dto::link::SimVehicle::Plane => "plane",
                    uxduck_contracts::dto::link::SimVehicle::Multicopter => "multicopter",
                    uxduck_contracts::dto::link::SimVehicle::Vtol => "VTOL",
                    uxduck_contracts::dto::link::SimVehicle::Rover => "rover",
                    uxduck_contracts::dto::link::SimVehicle::Boat => "boat",
                    uxduck_contracts::dto::link::SimVehicle::Submarine => "submarine",
                    uxduck_contracts::dto::link::SimVehicle::Airship => "blimp",
                    uxduck_contracts::dto::link::SimVehicle::Jet => "jet",
                },
                if params.allow_network {
                    " and allow other computers on the network to connect and control it"
                } else {
                    ""
                }
            ),
            ClientRequest::SimStop(_) => "stop a simulated vehicle".into(),
            ClientRequest::ParamDefinitionsUse(params) => {
                let name = self
                    .vehicle(&params.vehicle_id)
                    .map_or_else(|| params.vehicle_id.clone(), |v| v.name.clone());
                format!("change which parameter descriptions {name} uses")
            }
            ClientRequest::MavlinkDialectsUse(params) => {
                let name = self
                    .vehicle(&params.vehicle_id)
                    .map_or_else(|| params.vehicle_id.clone(), |v| v.name.clone());
                format!("change which MAVLink dialects {name} uses")
            }
            ClientRequest::SimDownload(params) => {
                format!("download and install simulator build {}", params.id)
            }
            ClientRequest::SimFrames(params) => {
                format!("run {} to read its simulator frame list", params.binary)
            }
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
            ClientRequest::SimPause(params) => if params.paused {
                "pause a simulated vehicle"
            } else {
                "resume a paused simulated vehicle"
            }
            .into(),
            ClientRequest::SimFault(params) => format!(
                "switch {} a fault in a simulated vehicle's camera or gimbal",
                if params.on { "on" } else { "off" }
            ),
            ClientRequest::SimSnapshot(_) => "save a snapshot of a simulated vehicle".into(),
            ClientRequest::SimCrash(_) => "end a simulated flight after a scenery collision".into(),
            ClientRequest::SnapshotStart(params) => format!(
                "start a simulation from the snapshot {}{}",
                self.state
                    .snapshots
                    .get(&params.id)
                    .map_or(params.id.as_str(), |s| s.name.as_str()),
                if params.allow_network {
                    " and allow other computers on the network to connect and control it"
                } else {
                    ""
                }
            ),
            ClientRequest::MarkerAdd(params) if params.shape.is_some() => {
                use uxduck_contracts::dto::marker::MarkerShape;
                let (article, kind) = match params.shape {
                    Some(MarkerShape::Line { .. }) => ("a", "line"),
                    Some(MarkerShape::Area { .. }) => ("an", "area"),
                    _ => ("a", "circle"),
                };
                params.name.as_ref().map_or_else(
                    || format!("draw {article} {kind} on the map"),
                    |name| format!("draw the {kind} {name} on the map"),
                )
            }
            ClientRequest::MarkerAdd(params) => {
                let kind = if params.outline.is_some()
                    || params.height_m.is_some()
                    || params.look == Some(uxduck_contracts::dto::marker::MarkerLook::Building)
                {
                    "building"
                } else {
                    "marker"
                };
                params.name.as_ref().map_or_else(
                    || format!("add a {kind} to the map"),
                    |name| format!("add the {kind} {name} to the map"),
                )
            }
            ClientRequest::MarkerUpdate(params) => {
                format!("change the marker {}", self.marker_name(&params.id))
            }
            ClientRequest::MarkerRemove(params) => {
                format!("remove the marker {}", self.marker_name(&params.id))
            }
            ClientRequest::PluginInstall(_) | ClientRequest::PluginCommand(_) => {
                self.plugin_summary(request)
            }
            other => format!("call {}", method_of(other)),
        }
    }

    fn marker_name<'a>(&'a self, id: &'a str) -> &'a str {
        self.state.markers.get(id).map_or(id, |m| m.name.as_str())
    }

    /// Whether a connection must not be shown an approval: its own, for a
    /// terminal (ADR-0065 T7). The client asking for a shell on a vehicle
    /// never sees its request, and so cannot be led to answer it. And one
    /// that carries a command an agent asks to run (T14) is shown only to
    /// those who answer it, the operators on the computer UXDuck runs on:
    /// the command is no other client's to read. Of those, only a client
    /// that says it shows the command gets it: a page or a command from
    /// before S425 would offer the answers without what they allow.
    pub(crate) fn hides_approval(
        &self,
        connection: ConnectionId,
        client_id: &str,
        capability: Capability,
        command: bool,
    ) -> bool {
        capability == Capability::Terminal
            && self
                .connections
                .get(&connection)
                .and_then(|c| c.admitted.as_ref())
                .is_some_and(|a| {
                    (command && !a.reads.shell_commands)
                        || (!a.operator && (command || a.record == client_id))
                })
    }

    /// The same for the notice of a pending approval, by its condition.
    pub(crate) fn hides_notice(&self, connection: ConnectionId, condition: Option<&str>) -> bool {
        condition
            .and_then(|condition| condition.strip_prefix("approval:"))
            .and_then(|id| self.state.approvals.get(id))
            .is_some_and(|a| {
                self.hides_approval(
                    connection,
                    &a.client_id,
                    a.capability,
                    a.shell_command.is_some(),
                )
            })
    }

    /// Tells a connection what it may do now: its record's capabilities and
    /// what was allowed for the connection.
    fn tell_capabilities(&mut self, connection: ConnectionId) {
        let Some(admitted) = self
            .connections
            .get(&connection)
            .and_then(|c| c.admitted.as_ref())
        else {
            return;
        };
        let mut granted = admitted.granted.clone();
        granted.extend(admitted.extra.iter().copied());
        granted.sort();
        granted.dedup();
        let frame = rpc::router::notification(&ServerNotification::ClientCapabilitiesChanged(
            CapabilitiesChanged {
                granted: granted.into_iter().map(Into::into).collect(),
            },
        ));
        self.send(connection, &frame);
    }

    fn broadcast_approval(&mut self, notification: &ServerNotification) {
        let frame = rpc::router::notification(notification);
        let about = match notification {
            ServerNotification::ApprovalRequested(approval)
            | ServerNotification::ApprovalResolved(approval) => Some((
                approval.client_id.as_str(),
                Capability::from(approval.capability),
                approval.shell_command_shown.is_some(),
            )),
            _ => None,
        };
        let targets: Vec<ConnectionId> = self
            .connections
            .iter()
            .filter(|(_, c)| c.admitted.as_ref().is_some_and(|a| a.approvals))
            .filter(|(id, _)| {
                !about.is_some_and(|(client, capability, command)| {
                    self.hides_approval(**id, client, capability, command)
                })
            })
            .map(|(id, _)| *id)
            .collect();
        for id in targets {
            self.send(id, &frame);
        }
    }

    /// Ends an approval: allowed ones run now with the allowance, the rest
    /// answer `capability-denied`.
    #[allow(
        clippy::too_many_lines,
        reason = "each decision and each held request, in turn"
    )]
    fn finish(&mut self, id: &str, state: ApprovalState, remember: Option<Remember>, why: &str) {
        let Some(mut approval) = self.state.approvals.get(id).cloned() else {
            return;
        };
        approval.state = state;
        approval.remember = remember;
        approval.version += 1;
        let dto = ApprovalDto::from(&approval);
        let listed: Vec<String> = approval.preflight.iter().map(|c| c.id.clone()).collect();
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
            let message = match &held.request {
                ClientRequest::VideoFrame(params) if state == ApprovalState::Denied => {
                    self.look_source(params).map_or_else(
                        |_| "The person didn't let the assistant see the camera.".to_owned(),
                        |source| {
                            format!("The person didn't let the assistant see {}.", source.name)
                        },
                    )
                }
                // A terminal's asker is a person at a page: plain words.
                request => crate::terminals::not_allowed(request, state == ApprovalState::Expired)
                    .map_or_else(|| format!("{why} (approval {id})"), str::to_owned),
            };
            let error = ApiError::new(ErrorKind::CapabilityDenied, message);
            self.send(
                held.connection,
                &rpc::router::error(&held.request_id, &error),
            );
            return;
        }
        if let Err(error) = self.check_mission_read_request(held.connection, &held.request) {
            self.send(
                held.connection,
                &rpc::router::error(&held.request_id, &error),
            );
            return;
        }
        // "Always allow" for a camera look is Settings › Privacy's choice,
        // for every chat and connection, not this client's (FB-244).
        if matches!(held.request, ClientRequest::VideoFrame(_))
            && remember == Some(Remember::Always)
            && self.state.settings.agent_cameras != AgentCameras::Always
        {
            let mut settings = self.state.settings.clone();
            settings.agent_cameras = AgentCameras::Always;
            settings.version += 1;
            self.commit_as(daemon(), Change::SettingsChanged { settings });
        }
        // An install is never remembered: the next one asks again (X12).
        // Nor is a change to whether the assistant sees cameras, or a look.
        let remember = if matches!(
            &held.request,
            ClientRequest::PluginInstall(_) | ClientRequest::VideoFrame(_)
        ) || matches!(&held.request, ClientRequest::SettingsUpdate(p) if p.agent_cameras.is_some())
        {
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
                self.tell_capabilities(held.connection);
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
        // An approval that listed failing preflight checks launches past
        // them (ADR-0038 K24), recorded on the receipt as the operator's.
        self.approving_preflight = (!listed.is_empty()).then(|| (id.to_owned(), listed));
        let result = rpc::handlers::handle(self, held.connection, held.request);
        self.approving_preflight = None;
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

    pub(crate) fn cancel_approval(&mut self, connection: ConnectionId, request_id: &Value) {
        let cancelled: Vec<String> = self
            .held
            .iter()
            .filter(|(_, held)| held.connection == connection && held.request_id == *request_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in cancelled {
            self.held.remove(&id);
            self.finish(
                &id,
                ApprovalState::Expired,
                None,
                "the client cancelled the request",
            );
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
            set_capabilities(engine, connection, params)
        }
        ClientRequest::ClientForget(params) if operator => {
            if engine.forget_network_client(&params.id) {
                Ok(json!({}))
            } else {
                Err(ApiError::new(
                    ErrorKind::NotFound,
                    format!("no phone or tablet {}", params.id),
                ))
            }
        }
        ClientRequest::ClientList(_)
        | ClientRequest::ClientSetCapabilities(_)
        | ClientRequest::ClientForget(_) => Err(denied("only an operator client may do this")),
        ClientRequest::ApprovalList(_) | ClientRequest::ApprovalResolve(_)
            if !granted.contains(&Capability::Approve) =>
        {
            Err(denied(&Capability::Approve.denied_message()))
        }
        ClientRequest::ApprovalList(_) => {
            let mut approvals: Vec<ApprovalDto> = engine
                .state
                .approvals
                .values()
                .filter(|a| {
                    !engine.hides_approval(
                        connection,
                        &a.client_id,
                        a.capability,
                        a.shell_command.is_some(),
                    )
                })
                .map(Into::into)
                .collect();
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
    connection: ConnectionId,
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
    // Before the edit, which also replaces what a connection was allowed.
    let held_terminal = engine.holds_terminal(&id);
    client.capabilities.clone_from(&capabilities);
    client.version += 1;
    let result = ClientDto::from(&client);
    engine.store_capabilities(client);
    // Taking `terminal` away ends that client's terminals (ADR-0065 T7).
    let actor = engine.actor(connection);
    engine.terminal_capability_set(&id, held_terminal, &capabilities, &actor);
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
            format!("no pending approval {id}. It may have been answered already"),
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
    // A shell on a vehicle is more than control of it (ADR-0065 T7): an
    // operator answers, never another client that was given `approve`.
    let operator = engine
        .connections
        .get(&connection)
        .and_then(|c| c.admitted.as_ref())
        .is_some_and(|a| a.operator);
    if done.capability == Capability::Terminal && !operator {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "Only an operator on the computer UXDuck runs on can answer a request for a terminal.",
        ));
    }
    // The command is read before it is allowed (T14): a client that does
    // not show it cannot answer for it, either way.
    let shows = engine
        .connections
        .get(&connection)
        .and_then(|c| c.admitted.as_ref())
        .is_some_and(|a| a.reads.shell_commands);
    if done.shell_command.is_some() && !shows {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "This request is to run a command, and this client cannot show the command. Answer it in an up-to-date UXDuck window, or with `uxduck approvals`.",
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

/// The words a camera look, or a change to whether the assistant sees
/// cameras, is asked for in (FB-244).
pub(crate) struct Asked {
    pub(crate) summary: String,
    pub(crate) question: String,
    pub(crate) body: String,
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::too_many_lines,
    reason = "in-memory held approval fixture"
)]
mod mission_read_tests {
    use super::*;
    use crate::mission_reads::fixtures;

    #[test]
    fn legacy_held_mission_requests_cannot_gain_remembered_authority_or_dispatch() {
        for remember in ["once", "connection", "always"] {
            let (mut engine, mut ui, orders, _plan) = fixtures::setup();
            let mut agent = fixtures::connect(&mut engine, "agent", Some(true));
            let items = serde_json::to_value(&engine.state.missions["v"].items).expect("items");
            fixtures::send(
                &mut engine,
                &agent,
                "mission/upload",
                json!({ "vehicleId": "v", "expectedVersion": 1, "items": items }),
            );
            assert!(
                fixtures::frames(&mut agent)
                    .iter()
                    .all(|frame| frame["id"] != "mission/upload")
            );
            let approval = engine
                .state
                .approvals
                .values()
                .find(|approval| approval.method == "mission/upload")
                .expect("held approval")
                .id
                .clone();
            let admitted = engine
                .connections
                .get_mut(&agent.id)
                .expect("connection")
                .admitted
                .as_mut()
                .expect("admitted");
            admitted.reads.mission_read_float_bits = false;
            let record = admitted.record.clone();
            let authority = engine.granted(agent.id);
            let stored_capabilities = engine.state.clients[&record].capabilities.clone();
            let missions = engine.state.missions.clone();
            let plans = engine.state.plans.clone();
            assert!(orders.take().is_empty());
            let resolution = fixtures::call(
                &mut engine,
                &mut ui,
                "approval/resolve",
                json!({ "id": approval, "allow": true, "remember": remember }),
            );
            assert!(resolution.get("result").is_some(), "{resolution}");
            let replies = fixtures::frames(&mut agent);
            let refused = replies
                .iter()
                .find(|frame| frame["id"] == "mission/upload")
                .expect("held reply");
            assert_eq!(refused["error"]["data"]["kind"], "update-required");
            assert!(refused.get("result").is_none());
            assert!(!engine.held.contains_key(&approval));
            assert_eq!(engine.granted(agent.id), authority);
            assert_eq!(
                engine.state.clients[&record].capabilities,
                stored_capabilities
            );
            assert_eq!(engine.state.missions, missions);
            assert_eq!(engine.state.plans, plans);
            assert!(orders.take().is_empty());
        }
    }
}
