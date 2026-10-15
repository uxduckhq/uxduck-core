//! One function per method. Thin: checks, then a call into state.

use serde_json::{Value, json};
use uxduck_contracts::dto::initialize::{Capability, InitializeParams, InitializeResult};
use uxduck_contracts::dto::operator::{
    ClientSetOperatorParams, LayoutOpenParams, LayoutOpenResult, Operator as OperatorDto,
    OperatorCreateParams, OperatorListResult, OperatorRemoveParams, OperatorUpdateParams,
};
use uxduck_contracts::dto::pairing::{BootstrapToken, PairingAnswerParams, PairingListResult};
use uxduck_contracts::dto::state::{
    EntityType, Snapshot, StateSubscribeParams, StateSubscribeResult,
};
use uxduck_contracts::dto::trace::Transport;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::{ClientNotification, ClientRequest};
use uxduck_contracts::version::{MIN_CLIENT_VERSION, PROTOCOL_VERSION};
use uxduck_core::domain::operator::{self, Operator};
use uxduck_core::events::Change;
use uxduck_core::policy::admission;

use crate::{Admitted, ConnectionId, Engine, version};

// A flat router: one arm per method reads better than splitting it.
#[allow(clippy::too_many_lines)]
pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
) -> Result<Value, ApiError> {
    if let ClientRequest::Initialize(params) = &request {
        return initialize(engine, connection, params);
    }
    let admitted = engine
        .connections
        .get(&connection)
        .and_then(|c| c.admitted.as_ref())
        .ok_or_else(|| ApiError::new(ErrorKind::NotInitialized, "call initialize first"))?;
    let operator = admitted.is_operator();
    // From away (S80): only commands and the lease, each once.
    if let Some(result) = engine.remote_gate(connection, &request) {
        return result;
    }
    let granted = engine.granted(connection);
    // Escalation instead of rejection (ADR-0005): held, answered later.
    if !operator && engine.escalate(connection, &request, &granted) {
        return Ok(Value::Null);
    }
    let require = |capability: uxduck_core::domain::client::Capability| {
        if granted.contains(&capability) {
            Ok(())
        } else {
            Err(ApiError::new(
                ErrorKind::CapabilityDenied,
                capability.denied_message(),
            ))
        }
    };
    let require_operator = || {
        if operator {
            Ok(())
        } else {
            Err(ApiError::new(
                ErrorKind::CapabilityDenied,
                "only an operator client may do this",
            ))
        }
    };

    engine.note_simulation_consumer(connection, &request);

    match request {
        ClientRequest::Initialize(_) => Err(ApiError::new(
            ErrorKind::Internal,
            "initialize is handled first",
        )),
        ClientRequest::LeaseTake(params) => {
            require(Read)?;
            engine.lease(connection, &params, true, operator)
        }
        ClientRequest::LeaseRelease(params) => {
            require(Read)?;
            engine.lease(connection, &params, false, operator)
        }
        ClientRequest::TraceSubscribe(_) => {
            require_operator()?;
            engine.tracers.insert(connection);
            Ok(json!({}))
        }
        ClientRequest::PairingList(_) => {
            require_operator()?;
            let requests = engine.sessions.pending_pairings(engine.clock.as_ref());
            to_value(&PairingListResult { requests })
        }
        ClientRequest::PairingApprove(PairingAnswerParams { code }) => {
            require_operator()?;
            answer(engine, &code, true)
        }
        ClientRequest::PairingDeny(PairingAnswerParams { code }) => {
            require_operator()?;
            answer(engine, &code, false)
        }
        ClientRequest::SessionCreateBootstrap(_) => {
            require_operator()?;
            create_bootstrap(engine, connection)
        }
        ClientRequest::StateSubscribe(StateSubscribeParams { entities }) => {
            require(Read)?;
            to_value(&subscribe(engine, connection, entities))
        }
        ClientRequest::OperatorList(_) => {
            require(Read)?;
            to_value(&OperatorListResult {
                operators: operators(engine),
            })
        }
        ClientRequest::OperatorCreate(params) => {
            require(Config)?;
            operator_create(engine, connection, &params)
        }
        ClientRequest::OperatorUpdate(params) => {
            require(Config)?;
            operator_update(engine, connection, &params)
        }
        ClientRequest::OperatorRemove(params) => {
            require(Config)?;
            operator_remove(engine, connection, params)
        }
        ClientRequest::ClientSetOperator(params) => set_operator(engine, connection, params),
        ClientRequest::LayoutOpen(params) => {
            require(Read)?;
            layout_open(engine, connection, &params)
        }
        request @ (ClientRequest::VehicleList(_)
        | ClientRequest::VehicleGet(_)
        | ClientRequest::VehicleSave(_)
        | ClientRequest::VehicleLabel(_)
        | ClientRequest::VehicleSize(_)
        | ClientRequest::VehicleDismiss(_)
        | ClientRequest::VehicleDelete(_)
        | ClientRequest::LinkAdd(_)
        | ClientRequest::LinkList(_)
        | ClientRequest::LinkRemove(_)
        | ClientRequest::TelemetrySubscribe(_)
        | ClientRequest::TelemetryGet(_)
        | ClientRequest::TelemetryWindow(_)
        | ClientRequest::LinkEnable(_)
        | ClientRequest::LinkDisable(_)
        | ClientRequest::LinkUpdate(_)
        | ClientRequest::SerialList(_)) => {
            super::vehicles::handle(engine, connection, request, &granted)
        }
        request @ (ClientRequest::ClientList(_)
        | ClientRequest::ClientSetCapabilities(_)
        | ClientRequest::ApprovalList(_)
        | ClientRequest::ApprovalResolve(_)) => {
            crate::approvals::handle(engine, connection, request, &granted, operator)
        }
        request @ (ClientRequest::SearchQuery(_)
        | ClientRequest::PlaceSave(_)
        | ClientRequest::PlaceUpdate(_)
        | ClientRequest::PlaceRemove(_)) => {
            crate::search::handle(engine, connection, request, &granted)
        }
        request @ (ClientRequest::PluginInstall(_)
        | ClientRequest::PluginRemove(_)
        | ClientRequest::PluginCommand(_)
        | ClientRequest::PluginFeatures(_)
        | ClientRequest::PluginItems(_)
        | ClientRequest::PluginSetEnabled(_)
        | ClientRequest::PluginUsage(_)
        | ClientRequest::ToolSearch(_)
        | ClientRequest::PluginDraft(_)
        | ClientRequest::PluginTest(_)
        | ClientRequest::PluginDrafts(_)) => {
            crate::plugins::handle(engine, connection, request, &granted, operator)
        }
        request @ ClientRequest::TerrainAt(_) => {
            crate::terrain::handle(engine, connection, request, &granted)
        }
        request @ (ClientRequest::TerrainCheck(_) | ClientRequest::TerrainStatus(_)) => {
            crate::terrain_check::handle(engine, connection, request, &granted)
        }
        request @ (ClientRequest::TerrainAreas(_)
        | ClientRequest::TerrainAreaSize(_)
        | ClientRequest::TerrainKeepArea(_)
        | ClientRequest::TerrainRemoveArea(_)) => {
            crate::terrain_keep::handle(engine, connection, request, &granted)
        }
        request @ ClientRequest::VehicleMessages(_) => {
            crate::messages::handle(engine, connection, request, &granted)
        }
        request @ (ClientRequest::TrayMenu(_)
        | ClientRequest::TrayActivate(_)
        | ClientRequest::FeedbackScreenshot(_)) => {
            crate::tray::handle(engine, connection, request, operator, &granted)
        }
        request @ (ClientRequest::PluginAllow(_)
        | ClientRequest::PluginShare(_)
        | ClientRequest::PluginShares(_)
        | ClientRequest::PluginUnshare(_)
        | ClientRequest::PluginShared(_)
        | ClientRequest::PluginInstallShared(_)) => {
            crate::plugin_sync::handle(engine, connection, request, &granted, operator)
        }
        request @ (ClientRequest::AnalyticsTrack(_) | ClientRequest::AnalyticsAlias(_)) => {
            crate::analytics::handle(engine, connection, request, operator)
        }
        ClientRequest::FeedbackSuggest(params) => {
            require(Read)?;
            crate::feedback::suggest(&params)
        }
        request @ ClientRequest::FeedbackSend(_) => {
            crate::feedback::handle(engine, connection, request, operator)
        }
        ClientRequest::JournalImport(params) => crate::import::handle(engine, params, operator),
        ClientRequest::AccountSetEnvironment(_) if !operator => Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "only an operator client may change the daemon's server",
        )),
        request @ (ClientRequest::AccountGet(_)
        | ClientRequest::AccountEnvironment(_)
        | ClientRequest::AccountSetEnvironment(_)
        | ClientRequest::AccountSignIn(_)
        | ClientRequest::AccountVerify(_)
        | ClientRequest::AccountSignOut(_)
        | ClientRequest::AccountSetUpdates(_)
        | ClientRequest::AccountSetUsername(_)
        | ClientRequest::AccountSessions(_)
        | ClientRequest::AccountSignOutSession(_)
        | ClientRequest::AccountSignOutOthers(_)
        | ClientRequest::AccountRequestDeletion(_)
        | ClientRequest::AccountShared(_)
        | ClientRequest::AccountComputers(_)
        | ClientRequest::AccountDeleteShared(_)) => {
            crate::account::handle(engine, connection, request, &granted)
        }
        request @ (ClientRequest::FilesList(_)
        | ClientRequest::FilesRead(_)
        | ClientRequest::FilesDownload(_)
        | ClientRequest::FilesUpload(_)
        | ClientRequest::FilesRemove(_)
        | ClientRequest::FilesMkdir(_)
        | ClientRequest::FilesRename(_)
        | ClientRequest::FilesPause(_)
        | ClientRequest::FilesResume(_)
        | ClientRequest::FilesCancel(_)
        | ClientRequest::FilesSave(_)) => {
            crate::vehicle_files::handle(engine, connection, request, &granted)
        }
        request @ (ClientRequest::PracticeList(_)
        | ClientRequest::PracticeStart(_)
        | ClientRequest::PracticeAnswer(_)
        | ClientRequest::PracticeStop(_)
        | ClientRequest::PracticeGuide(_)
        | ClientRequest::PracticeStatus(_)) => {
            crate::practice::handle(engine, connection, request, &granted)
        }
        request @ (ClientRequest::WatchLinkCreate(_)
        | ClientRequest::WatchLinkList(_)
        | ClientRequest::WatchLinkViewers(_)
        | ClientRequest::WatchLinkRevoke(_)) => {
            crate::watch_link::handle(engine, connection, request, &granted)
        }
        ClientRequest::ThreadFeedback(params) => {
            crate::learning::feedback(engine, connection, params, operator)
        }
        request @ (ClientRequest::SyncGet(_)
        | ClientRequest::SyncSet(_)
        | ClientRequest::SyncActivate(_)) => {
            crate::sync::handle(engine, connection, request, &granted)
        }
        request @ (ClientRequest::SimStart(_)
        | ClientRequest::SimStop(_)
        | ClientRequest::SimRestart(_)
        | ClientRequest::SimBinaries(_)
        | ClientRequest::SimCatalogue(_)
        | ClientRequest::SimDownload(_)
        | ClientRequest::SimDelete(_)) => crate::sim::handle(engine, connection, request, &granted),
        request @ (ClientRequest::SimSnapshot(_)
        | ClientRequest::SnapshotStart(_)
        | ClientRequest::SnapshotList(_)
        | ClientRequest::SnapshotRename(_)
        | ClientRequest::SnapshotDelete(_)) => {
            crate::snapshots::handle(engine, connection, request, &granted)
        }
        request @ (ClientRequest::VehicleHistory(_)
        | ClientRequest::ParamHistory(_)
        | ClientRequest::VehicleAddNote(_)
        | ClientRequest::VehicleEditNote(_)
        | ClientRequest::VehicleDeleteNote(_)) => {
            crate::history::handle(engine, connection, request, &granted)
        }
        request @ (ClientRequest::FindStart(_)
        | ClientRequest::FindStop(_)
        | ClientRequest::FindVehicles(_)) => {
            crate::finds::handle(engine, connection, request, &granted)
        }
        request @ ClientRequest::RemoteWatch(_) => {
            require(uxduck_core::domain::client::Capability::Read)?;
            crate::remote_view::handle(engine, connection, request)
        }
        request @ (ClientRequest::VideoList(_)
        | ClientRequest::VideoAssign(_)
        | ClientRequest::VideoWatch(_)
        | ClientRequest::VideoUnwatch(_)
        | ClientRequest::VideoAdd(_)
        | ClientRequest::VideoRemove(_)
        | ClientRequest::VideoRecord(_)
        | ClientRequest::VideoPlayback(_)
        | ClientRequest::VideoRename(_)
        | ClientRequest::VideoTrack(_)
        | ClientRequest::VideoReveal(_)) => {
            crate::video::handle(engine, connection, request, &granted)
        }
        request @ (ClientRequest::ParamSearch(_)
        | ClientRequest::ParamGet(_)
        | ClientRequest::ParamList(_)
        | ClientRequest::ParamDiff(_)) => {
            crate::param_definitions::queries::handle(engine, connection, request, &granted)
        }
        request @ (ClientRequest::ParamDefinitions(_)
        | ClientRequest::ParamDefinitionsList(_)
        | ClientRequest::ParamDefinitionsAdd(_)
        | ClientRequest::ParamDefinitionsRemove(_)) => {
            crate::param_definitions::handle(engine, connection, request, &granted)
        }
        request @ (ClientRequest::DaemonStopPreview(_)
        | ClientRequest::ClientVisibility(_)
        | ClientRequest::AppFocus(_)
        | ClientRequest::AppOpen(_)
        | ClientRequest::DesktopAppStatus(_)
        | ClientRequest::DesktopAppDownload(_)
        | ClientRequest::DesktopAppChoose(_)
        | ClientRequest::DesktopAppRemove(_)) => {
            crate::app::handle(engine, connection, &request, operator, &granted)
        }
        request @ (ClientRequest::InterfaceStatus(_) | ClientRequest::InterfaceCheck(_)) => {
            crate::interface::handle(engine, &request, &granted)
        }
        request @ (ClientRequest::RouterEvents(_)
        | ClientRequest::LinkDiagnose(_)
        | ClientRequest::SocketInspect(_)
        | ClientRequest::DaemonLogs(_)) => crate::diagnostics::handle(engine, &request, &granted),
        request @ (ClientRequest::DaemonRestart(_)
        | ClientRequest::DaemonStop(_)
        | ClientRequest::DebugPanic(_)
        | ClientRequest::SettingsUpdate(_)
        | ClientRequest::StorageUsage(_)
        | ClientRequest::DaemonWakeups(_)
        | ClientRequest::DaemonAbout(_)
        | ClientRequest::MavlinkInspect(_)
        | ClientRequest::MavlinkHistory(_)) => {
            crate::restart::handle(engine, connection, request, &granted, operator)
        }
        request @ (ClientRequest::ControllerClaim(_)
        | ClientRequest::ControllerRelease(_)
        | ClientRequest::ControllerForget(_)
        | ClientRequest::ControllerSetTarget(_)
        | ClientRequest::ControllerInputs(_)
        | ClientRequest::ClientSelect(_)
        | ClientRequest::MappingUpdate(_)
        | ClientRequest::MappingCreate(_)) => {
            crate::controllers::handle(engine, connection, request, &granted, operator)
        }
        request @ (ClientRequest::ParamRefresh(_)
        | ClientRequest::ParamSet(_)
        | ClientRequest::MissionRefresh(_)
        | ClientRequest::MissionUpload(_)) => {
            crate::transfers::handle(engine, connection, request, &granted)
        }
        request @ (ClientRequest::CommandSend(_)
        | ClientRequest::CommandList(_)
        | ClientRequest::CommandModes(_)
        | ClientRequest::NotificationAcknowledge(_)
        | ClientRequest::NotificationDismiss(_)) => {
            crate::commands::handle(engine, connection, request, &granted, operator)
        }
        request @ (ClientRequest::AgentConfig(_)
        | ClientRequest::ThreadCreate(_)
        | ClientRequest::ThreadSend(_)
        | ClientRequest::ThreadUpdate(_)
        | ClientRequest::ThreadCancel(_)
        | ClientRequest::CredentialsSet(_)
        | ClientRequest::ThreadDelete(_)
        | ClientRequest::ThreadBulk(_)
        | ClientRequest::FolderCreate(_)
        | ClientRequest::FolderUpdate(_)
        | ClientRequest::FolderDelete(_)) => {
            super::threads::handle(engine, connection, request, &granted, operator)
        }
    }
}

fn operator_remove(
    engine: &mut Engine,
    connection: ConnectionId,
    OperatorRemoveParams {
        id,
        expected_version,
    }: OperatorRemoveParams,
) -> Result<Value, ApiError> {
    current(engine, &id, expected_version)?;
    engine.commit(connection, Change::OperatorRemoved { id });
    Ok(json!({}))
}

fn set_operator(
    engine: &mut Engine,
    connection: ConnectionId,
    ClientSetOperatorParams { operator_id }: ClientSetOperatorParams,
) -> Result<Value, ApiError> {
    if let Some(id) = &operator_id
        && !engine.state.operators.contains_key(id)
    {
        return Err(ApiError::new(
            ErrorKind::NotFound,
            format!("no operator {id}"),
        ));
    }
    if let Some(state) = engine.connections.get_mut(&connection) {
        state.operator_id = operator_id;
    }
    Ok(json!({}))
}

use uxduck_core::domain::client::Capability::{Config, Read};

/// Registers the subscriber and copies the state in one step of the single
/// writer, so no event can fall between them: the first delta this
/// connection receives is `seq + 1` (ADR-0001 D7). Subscribing again replaces
/// the set, which is how a client recovers from a gap.
/// Every entity of a map, as the wire has it.
fn each<'a, T: 'a, U: From<&'a T>>(map: &'a std::collections::BTreeMap<String, T>) -> Vec<U> {
    map.values().map(Into::into).collect()
}

fn subscribe(
    engine: &mut Engine,
    connection: ConnectionId,
    entities: Vec<EntityType>,
) -> StateSubscribeResult {
    let types: std::collections::BTreeSet<EntityType> = entities.into_iter().collect();
    let snapshot = Snapshot {
        operators: types
            .contains(&EntityType::Operator)
            .then(|| operators(engine)),
        threads: types
            .contains(&EntityType::Thread)
            .then(|| super::threads::threads(engine)),
        items: types
            .contains(&EntityType::Item)
            .then(|| super::threads::items(engine)),
        folders: types
            .contains(&EntityType::Folder)
            .then(|| super::folders::folders(engine)),
        links: types
            .contains(&EntityType::Link)
            .then(|| super::vehicles::links(engine)),
        clients: types
            .contains(&EntityType::Client)
            .then(|| engine.state.clients.values().map(Into::into).collect()),
        approvals: types
            .contains(&EntityType::Approval)
            .then(|| engine.state.approvals.values().map(Into::into).collect()),
        receipts: types
            .contains(&EntityType::Receipt)
            .then(|| engine.state.receipts.values().map(Into::into).collect()),
        notifications: types
            .contains(&EntityType::Notification)
            .then(|| each(&engine.state.notifications)),
        vehicles: types
            .contains(&EntityType::Vehicle)
            .then(|| super::vehicles::vehicles(engine)),
        parameter_sets: types
            .contains(&EntityType::ParameterSet)
            .then(|| each(&engine.state.parameter_sets)),
        parameters: types
            .contains(&EntityType::Parameter)
            .then(|| engine.state.parameters.values().map(Into::into).collect()),
        missions: types
            .contains(&EntityType::Mission)
            .then(|| engine.state.missions.values().map(Into::into).collect()),
        controllers: types
            .contains(&EntityType::Controller)
            .then(|| engine.state.controllers.values().map(Into::into).collect()),
        places: types
            .contains(&EntityType::Place)
            .then(|| engine.state.places.values().map(Into::into).collect()),
        snapshots: types
            .contains(&EntityType::Snapshot)
            .then(|| engine.state.snapshots.values().map(Into::into).collect()),
        history: types
            .contains(&EntityType::HistoryEntry)
            .then(|| engine.state.history.values().map(Into::into).collect()),
        video_sources: types
            .contains(&EntityType::VideoSource)
            .then(|| each(&engine.state.video_sources)),
        video_recordings: types
            .contains(&EntityType::VideoRecording)
            .then(|| each(&engine.state.video_recordings)),
        practice_attempts: types
            .contains(&EntityType::PracticeAttempt)
            .then(|| engine.state.practice.values().map(Into::into).collect()),
        file_transfers: types.contains(&EntityType::FileTransfer).then(|| {
            engine
                .state
                .file_transfers
                .values()
                .map(Into::into)
                .collect()
        }),
        param_definitions: types
            .contains(&EntityType::ParamDefinitions)
            .then(|| each(&engine.state.param_definitions)),
        plugins: types
            .contains(&EntityType::Plugin)
            .then(|| engine.state.plugins.values().map(Into::into).collect()),
        settings: types
            .contains(&EntityType::Settings)
            .then(|| engine.settings_dto(&engine.state.settings)),
        input_mappings: types.contains(&EntityType::InputMapping).then(|| {
            crate::controllers::mappings(engine)
                .iter()
                .map(Into::into)
                .collect()
        }),
    };
    engine.subscribers.insert(connection, types);
    StateSubscribeResult {
        seq: engine.state.seq,
        snapshot,
    }
}

fn operators(engine: &Engine) -> Vec<OperatorDto> {
    engine
        .state
        .operators
        .values()
        .map(OperatorDto::from)
        .collect()
}

/// The operator `id` if `expected_version` is still its version.
fn current(engine: &Engine, id: &str, expected_version: u64) -> Result<Operator, ApiError> {
    let found = engine
        .state
        .operators
        .get(id)
        .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no operator {id}")))?;
    if found.version != expected_version {
        let mut error = ApiError::new(
            ErrorKind::VersionConflict,
            format!(
                "operator {id} is at version {}, not {expected_version}",
                found.version
            ),
        );
        error.data.current_version = Some(found.version);
        return Err(error);
    }
    Ok(found.clone())
}

fn clean_name(name: &str) -> Result<String, ApiError> {
    operator::clean_name(name).ok_or_else(|| {
        ApiError::new(
            ErrorKind::InvalidValue,
            format!("a name has 1 to {} characters", operator::MAX_NAME_CHARS),
        )
    })
}

fn operator_create(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &OperatorCreateParams,
) -> Result<Value, ApiError> {
    let operator = Operator {
        id: format!("op{}", &engine.random.token()[..16]),
        name: clean_name(&params.name)?,
        units: params.units.map(Into::into).unwrap_or_default(),
        coordinate_format: params.coordinate_format.map(Into::into).unwrap_or_default(),
        altitude_frame: params.altitude_frame.map(Into::into).unwrap_or_default(),
        layout: operator::Layout::default(),
        place_search: true,
        basemap: operator::Basemap::default(),
        shortcuts: std::collections::BTreeMap::new(),
        skipped_confirmations: Vec::new(),
        telemetry_layouts: Vec::new(),
        vehicle_telemetry: std::collections::BTreeMap::new(),
        parameter_groups: Vec::new(),
        inspector_pinned: Vec::new(),
        control_actions: Vec::new(),
        version: 1,
    };
    let result = OperatorDto::from(&operator);
    engine.commit(connection, Change::OperatorUpserted { operator });
    to_value(&result)
}

fn operator_update(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &OperatorUpdateParams,
) -> Result<Value, ApiError> {
    let mut operator = current(engine, &params.id, params.expected_version)?;
    if let Some(name) = &params.name {
        operator.name = clean_name(name)?;
    }
    if let Some(units) = params.units {
        operator.units = units.into();
    }
    if let Some(format) = params.coordinate_format {
        operator.coordinate_format = format.into();
    }
    if let Some(frame) = params.altitude_frame {
        operator.altitude_frame = frame.into();
    }
    if let Some(layout) = params.layout.clone() {
        let mut layout: operator::Layout = layout.into();
        // A tab moved, closed, added or hidden is the person arranging the
        // dock, which UXDuck then leaves to them (S65); a size or the
        // showing tab is not.
        layout.arranged =
            operator.layout.arranged || layout.dock.shape() != operator.layout.dock.shape();
        layout.flight_opened = operator.layout.flight_opened;
        operator.layout = layout;
    }
    if params.telemetry_layouts.is_some() || params.vehicle_telemetry.is_some() {
        let layouts = params.telemetry_layouts.clone().map_or_else(
            || operator.telemetry_layouts.clone(),
            |l| l.into_iter().map(Into::into).collect(),
        );
        let chosen = params.vehicle_telemetry.clone().map_or_else(
            || operator.vehicle_telemetry.clone(),
            |c| c.into_iter().map(|v| (v.vehicle_id, v.layout_id)).collect(),
        );
        (operator.telemetry_layouts, operator.vehicle_telemetry) =
            uxduck_core::domain::telemetry_layout::cleaned(layouts, chosen);
    }
    if let Some(on) = params.place_search {
        operator.place_search = on;
    }
    if let Some(basemap) = params.basemap {
        operator.basemap = basemap.into();
    }
    if let Some(shortcuts) = &params.shortcuts {
        operator.shortcuts = clean_shortcuts(shortcuts)?;
    }
    if let Some(skipped) = &params.skipped_confirmations {
        operator.skipped_confirmations = clean_skipped(skipped)?;
    }
    if let Some(groups) = &params.parameter_groups {
        let groups: Vec<operator::ParameterGroup> = groups.iter().map(Into::into).collect();
        operator.parameter_groups = operator::clean_parameter_groups(&groups).ok_or_else(|| {
            ApiError::new(
                ErrorKind::InvalidValue,
                format!(
                    "at most {} parameter groups, each a unique name of 1 to {} characters and at most {} parameters of 1 to {} characters A-Z, 0-9 or _",
                    operator::MAX_PARAMETER_GROUPS,
                    operator::MAX_GROUP_NAME_CHARS,
                    operator::MAX_GROUP_PARAMETERS,
                    operator::MAX_PARAMETER_NAME_CHARS,
                ),
            )
        })?;
    }
    if let Some(pinned) = &params.inspector_pinned {
        operator.inspector_pinned = operator::clean_inspector_pinned(pinned).ok_or_else(|| {
            ApiError::new(
                ErrorKind::InvalidValue,
                format!(
                    "at most {} pinned messages, each received: or sent: and 1 to {} characters A-Z, 0-9, _ or #",
                    operator::MAX_INSPECTOR_PINNED,
                    operator::MAX_PINNED_MESSAGE_CHARS,
                ),
            )
        })?;
    }
    if let Some(all) = &params.control_actions {
        let all: Vec<operator::ControlActions> = all.iter().map(Into::into).collect();
        operator.control_actions = operator::clean_control_actions(&all).ok_or_else(|| {
            ApiError::new(
                ErrorKind::InvalidValue,
                format!(
                    "each vehicle type once, with at most {} actions of up to {} characters, each vehicle: and a-z, 0-9, -, _, : or spaces",
                    operator::MAX_CONTROL_ACTIONS,
                    operator::MAX_CONTROL_ACTION_CHARS,
                ),
            )
        })?;
    }
    operator.version += 1;
    let result = OperatorDto::from(&operator);
    engine.commit(connection, Change::OperatorUpserted { operator });
    to_value(&result)
}

/// Shows a tab in the layout of the chat's operator, else the one named
/// (S43). With neither, the chat's window keeps its own layout and opens
/// the tab itself when it sees the call succeed.
fn layout_open(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &LayoutOpenParams,
) -> Result<Value, ApiError> {
    let kind = params.kind.into();
    if kind == operator::TabKind::Chat {
        return Err(ApiError::new(
            ErrorKind::InvalidParams,
            "the chat is always open",
        ));
    }
    let from_thread = params
        .thread_id
        .as_ref()
        .and_then(|id| engine.state.threads.get(id))
        .and_then(|thread| thread.operator_id.clone());
    let Some(id) = from_thread.or_else(|| params.operator_id.clone()) else {
        return to_value(&LayoutOpenResult {
            operator_id: None,
            kind: params.kind,
        });
    };
    let mut operator = engine
        .state
        .operators
        .get(&id)
        .cloned()
        .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no operator {id}")))?;
    let shape = operator.layout.dock.shape();
    operator.layout.open(kind);
    // A tab added is arranging the dock, as "+" is (S65).
    operator.layout.arranged |= operator.layout.dock.shape() != shape;
    operator.version += 1;
    engine.commit(connection, Change::OperatorUpserted { operator });
    to_value(&LayoutOpenResult {
        operator_id: Some(id),
        kind: params.kind,
    })
}

/// At most this many changed shortcuts, each id and key this long.
const MAX_SHORTCUTS: usize = 500;
const MAX_SHORTCUT_TEXT: usize = 64;

/// Shortcuts are the app's to read; the daemon only keeps them small.
fn clean_shortcuts(
    shortcuts: &[uxduck_contracts::dto::operator::ShortcutChange],
) -> Result<std::collections::BTreeMap<String, Option<String>>, ApiError> {
    let too_long = |text: &str| text.is_empty() || text.len() > MAX_SHORTCUT_TEXT;
    if shortcuts.len() > MAX_SHORTCUTS
        || shortcuts
            .iter()
            .any(|c| too_long(&c.action) || c.shortcut.as_deref().is_some_and(too_long))
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!(
                "at most {MAX_SHORTCUTS} shortcuts, each id and key 1 to {MAX_SHORTCUT_TEXT} characters"
            ),
        ));
    }
    Ok(shortcuts
        .iter()
        .map(|c| (c.action.clone(), c.shortcut.clone()))
        .collect())
}

/// Kept small and without repeats; the app knows which actions may be skipped.
fn clean_skipped(skipped: &[String]) -> Result<Vec<String>, ApiError> {
    if skipped.len() > MAX_SHORTCUTS
        || skipped
            .iter()
            .any(|id| id.is_empty() || id.len() > MAX_SHORTCUT_TEXT)
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!(
                "at most {MAX_SHORTCUTS} skipped confirmations, each id 1 to {MAX_SHORTCUT_TEXT} characters"
            ),
        ));
    }
    let mut kept: Vec<String> = Vec::new();
    for id in skipped {
        if !kept.contains(id) {
            kept.push(id.clone());
        }
    }
    Ok(kept)
}

pub(crate) fn notify(
    _engine: &mut Engine,
    _connection: ConnectionId,
    notification: &ClientNotification,
) {
    match notification {
        // Every S1 handler answers immediately, so there is nothing to cancel.
        ClientNotification::Cancel(_) => {}
    }
}

fn initialize(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &InitializeParams,
) -> Result<Value, ApiError> {
    let Some(state) = engine.connections.get(&connection) else {
        return Err(ApiError::new(ErrorKind::Internal, "unknown connection"));
    };
    if state.admitted.is_some() {
        return Err(ApiError::new(
            ErrorKind::AlreadyInitialized,
            "initialize was already called on this connection",
        ));
    }
    let update_required = |message: String| {
        let mut error = ApiError::new(ErrorKind::UpdateRequired, message);
        error.data.min_client_version = Some(MIN_CLIENT_VERSION.into());
        error
    };
    // An interface talks the current protocol and the two before it
    // (ADR-0025 D4), and lists them.
    let capabilities = &params.capabilities;
    if capabilities.protocol_version != PROTOCOL_VERSION
        && !capabilities.supported_protocols.contains(&PROTOCOL_VERSION)
    {
        return Err(update_required(
            "This page and UXDuck Desktop don't match. Update UXDuck Desktop, then reload the page."
                .to_owned(),
        ));
    }
    let Some(client_version) = version::parse(&params.client_info.version) else {
        return Err(ApiError::new(
            ErrorKind::InvalidParams,
            "clientInfo.version must be major.minor.patch",
        ));
    };
    if Some(client_version) < version::parse(MIN_CLIENT_VERSION) {
        return Err(update_required(format!(
            "This page ({}) is too old for UXDuck on this computer. Reload to update it.",
            params.client_info.version
        )));
    }

    let kind = params.client_info.kind;
    if kind == uxduck_contracts::dto::initialize::ClientKind::Plugin {
        return Err(ApiError::new(
            ErrorKind::InvalidParams,
            "plugins are run by the daemon; a connection cannot be one",
        ));
    }
    let default = admission::granted(kind.into(), &state.trust);
    let operator = default.len() == uxduck_core::domain::client::Capability::ALL.len();
    let client_id = state.client_id.clone();
    let executable = state.executable.clone();
    let (record, core_granted) = engine.admit_record(
        kind.into(),
        &params.client_info.name,
        executable.as_deref(),
        operator,
        default,
    );
    let granted: Vec<Capability> = core_granted.iter().copied().map(Into::into).collect();
    let window = matches!(
        kind,
        uxduck_contracts::dto::initialize::ClientKind::OperatorUi
            | uxduck_contracts::dto::initialize::ClientKind::Shell
    );
    engine.opened.window_seen |= window;
    engine.opened.seen_order += 1;
    let order = engine.opened.seen_order;
    if let Some(state) = engine.connections.get_mut(&connection) {
        state.name.clone_from(&params.client_info.name);
        state.seen = order;
        state.admitted = Some(Admitted {
            kind: kind.into(),
            operator,
            record,
            granted: core_granted,
            extra: Vec::new(),
            approvals: params.capabilities.approvals,
        });
    }
    to_value(&InitializeResult {
        daemon_version: engine.config.daemon_version.clone(),
        protocol_version: PROTOCOL_VERSION,
        min_client_version: MIN_CLIENT_VERSION.into(),
        capabilities: engine.config.capabilities.clone(),
        client_id,
        granted,
        release_stage: uxduck_contracts::version::ReleaseStage::Flight,
    })
}

fn answer(engine: &mut Engine, code: &str, approve: bool) -> Result<Value, ApiError> {
    let answered = engine
        .sessions
        .answer_pairing(code, approve, engine.clock.as_ref());
    // A question for a browser that has gone is cleared, answered or not.
    engine.sync_pairing_questions();
    if answered {
        Ok(json!({}))
    } else {
        Err(ApiError::new(
            ErrorKind::NotFound,
            "that browser stopped waiting; reload it to ask again",
        ))
    }
}

pub(crate) fn to_value<T: serde::Serialize>(value: &T) -> Result<Value, ApiError> {
    serde_json::to_value(value).map_err(|e| ApiError::new(ErrorKind::Internal, e.to_string()))
}

fn create_bootstrap(engine: &mut Engine, connection: ConnectionId) -> Result<Value, ApiError> {
    let local = engine
        .connections
        .get(&connection)
        .is_some_and(|c| c.transport == Transport::Socket);
    if !local {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "bootstrap tokens are only issued over the local socket",
        ));
    }
    let token = engine.create_bootstrap();
    to_value(&BootstrapToken {
        token,
        ui_url: engine.config.ui_url.clone(),
    })
}
