//! One function per method. Thin: checks, then a call into state.

use serde_json::{Value, json};
use uxduck_contracts::dto::initialize::{
    Capability, ClientKind, DaemonCapabilities, InitializeParams, InitializeResult,
};
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

use crate::{Admitted, ConnectionId, Engine, ReadCapabilities, version};

// A flat router: one arm per method reads better than splitting it.
#[allow(clippy::too_many_lines)]
pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
) -> Result<Value, ApiError> {
    if engine.app_update.installing {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "UXDuck is installing an update. Reconnect after it restarts.",
        ));
    }
    if let ClientRequest::Initialize(params) = &request {
        return initialize(engine, connection, params);
    }
    let admitted = engine
        .connections
        .get(&connection)
        .and_then(|c| c.admitted.as_ref())
        .ok_or_else(|| ApiError::new(ErrorKind::NotInitialized, "call initialize first"))?;
    let operator = admitted.is_operator();
    engine.check_mission_read_request(connection, &request)?;
    // A person flies AutoTune (S332): no approval lets an agent start it.
    if admitted.kind == uxduck_core::domain::client::ClientKind::Agent
        && let Some(why) = crate::autotune::agent_refusal(engine, &request)
    {
        return Err(ApiError::new(ErrorKind::CapabilityDenied, why));
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
        // Retired (ADR-0048): not in `METHODS`, so the router answers first.
        ClientRequest::AccountComputers(_)
        | ClientRequest::LeaseTake(_)
        | ClientRequest::LeaseRelease(_)
        | ClientRequest::RemoteWatch(_) => Err(ApiError::new(
            ErrorKind::MethodNotFound,
            "removed with the account's computers (ADR-0048)",
        )),
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
        ClientRequest::PairingApprove(PairingAnswerParams { code, access }) => {
            require_operator()?;
            answer(engine, &code, true, access.map(Into::into))
        }
        ClientRequest::PairingDeny(PairingAnswerParams { code, .. }) => {
            require_operator()?;
            answer(engine, &code, false, None)
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
        ClientRequest::OperatorProfile(params) => {
            require(Read)?;
            operator_profile(engine, &params.id)
        }
        ClientRequest::OperatorCreate(params) => {
            require(Config)?;
            operator_create(engine, connection, &params)
        }
        ClientRequest::OperatorUpdate(params) => {
            require(Config)?;
            // Agents may change an operator's preferences when asked, but
            // never skip the confirmations that guard commands (ADR-0005).
            if !operator && params.skipped_confirmations.is_some() {
                return Err(ApiError::new(
                    ErrorKind::CapabilityDenied,
                    "only the operator may choose which confirmations to skip",
                ));
            }
            operator_update(engine, connection, &params)
        }
        ClientRequest::TermsAccept(params) => {
            require_operator()?;
            crate::terms::accept(engine, connection, &params)
        }
        ClientRequest::OperatorRemove(params) => {
            require(Config)?;
            operator_remove(engine, connection, params)
        }
        ClientRequest::ClientSetOperator(params) | ClientRequest::OperatorSwitch(params) => {
            set_operator(engine, connection, params)
        }
        ClientRequest::LayoutOpen(params) => {
            require(Read)?;
            layout_open(engine, connection, &params)
        }
        ClientRequest::ComputerAdd(params) => {
            require(Config)?;
            engine.add_computer(connection, params)
        }
        ClientRequest::ComputerRemove(params) => {
            require(Config)?;
            engine.remove_computer(connection, params)
        }
        ClientRequest::ComputerCheck(params) => {
            require(Config)?;
            engine.check_computer(connection, params)
        }
        request @ (ClientRequest::VehicleList(_)
        | ClientRequest::VehicleGet(_)
        | ClientRequest::VehicleSave(_)
        | ClientRequest::VehicleLabel(_)
        | ClientRequest::VehicleSize(_)
        | ClientRequest::VehicleLook(_)
        | ClientRequest::VehicleModelRead(_)
        | ClientRequest::VehicleColour(_)
        | ClientRequest::VehicleGripper(_)
        | ClientRequest::GripperStatus(_)
        | ClientRequest::VehicleSimulated(_)
        | ClientRequest::VehicleBookmarks(_)
        | ClientRequest::VehicleDismiss(_)
        | ClientRequest::VehicleHide(_)
        | ClientRequest::VehicleDelete(_)
        | ClientRequest::LinkAdd(_)
        | ClientRequest::LinkList(_)
        | ClientRequest::LinkRemove(_)
        | ClientRequest::TelemetrySubscribe(_)
        | ClientRequest::TelemetryGet(_)
        | ClientRequest::TelemetryWindow(_)
        | ClientRequest::TelemetryCalculate(_)
        | ClientRequest::LinkEnable(_)
        | ClientRequest::LinkDisable(_)
        | ClientRequest::LinkUpdate(_)
        | ClientRequest::SerialList(_)) => {
            super::vehicles::handle(engine, connection, request, &granted)
        }
        request @ (ClientRequest::ClientDisplayRegister(_)
        | ClientRequest::ClientDisplayList(_)
        | ClientRequest::ClientDisplayPull(_)
        | ClientRequest::ClientDisplayReply(_)) => {
            require(Read)?;
            crate::display::handle(engine, connection, request)
        }
        request @ (ClientRequest::ClientList(_)
        | ClientRequest::ClientSetCapabilities(_)
        | ClientRequest::ClientForget(_)
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
        request @ (ClientRequest::MarkerList(_)
        | ClientRequest::MarkerAdd(_)
        | ClientRequest::MarkerUpdate(_)
        | ClientRequest::MarkerRemove(_)
        | ClientRequest::MarkerShare(_)
        | ClientRequest::MarkerUnshare(_)) => {
            crate::markers::handle(engine, connection, request, &granted)
        }
        request @ (ClientRequest::PlaceSearch(_) | ClientRequest::PlaceLocate(_)) => {
            crate::places::handle(engine, connection, request, &granted)
        }
        request @ (ClientRequest::PlanList(_)
        | ClientRequest::PlanGet(_)
        | ClientRequest::PlanCreate(_)
        | ClientRequest::PlanUpdate(_)
        | ClientRequest::PlanUndo(_)
        | ClientRequest::PlanRedo(_)
        | ClientRequest::PlanPlace(_)
        | ClientRequest::PlanDelete(_)
        | ClientRequest::PlanRestore(_)
        | ClientRequest::PlanImport(_)
        | ClientRequest::PlanExport(_)
        | ClientRequest::PlanUpload(_)
        | ClientRequest::PlanCheck(_)
        | ClientRequest::PlanExamples(_)) => {
            crate::plans::handle(engine, connection, request, &granted)
        }
        request @ (ClientRequest::PlanSimulate(_) | ClientRequest::PlanSimulateCheck(_)) => {
            crate::plan_runs::handle(engine, connection, request, &granted)
        }
        request @ (ClientRequest::PluginInstall(_)
        | ClientRequest::PluginRemove(_)
        | ClientRequest::PluginCommand(_)
        | ClientRequest::PluginFeatures(_)
        | ClientRequest::PluginItems(_)
        | ClientRequest::PluginSetEnabled(_)
        | ClientRequest::PluginSetChecksBlock(_)
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
        ClientRequest::TerrainVehicle(params) => {
            crate::terrain_vehicle::handle(engine, connection, &params, &granted)
        }
        ClientRequest::TerrainPutArea(params) => {
            crate::terrain_put::handle(engine, connection, &params, &granted)
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
        request @ (ClientRequest::AnalyticsTrack(_)
        | ClientRequest::AnalyticsFeedback(_)
        | ClientRequest::AnalyticsActiveTime(_)
        | ClientRequest::AnalyticsEngagedSimTime(_)
        | ClientRequest::AnalyticsTakeFlightTime(_)
        | ClientRequest::AnalyticsTakeEngagement(_)
        | ClientRequest::AnalyticsPerformance(_)
        | ClientRequest::AnalyticsAlias(_)) => {
            crate::analytics::handle(engine, connection, request, operator)
        }
        ClientRequest::FeedbackSuggest(params) => {
            require(Read)?;
            crate::feedback::suggest(&params)
        }
        request @ ClientRequest::FeedbackSend(_) => {
            crate::feedback::handle(engine, connection, request, operator)
        }
        ClientRequest::FeedbackFile(params) => {
            require(Read)?;
            crate::feedback::file(engine, &params)
        }
        ClientRequest::FeedbackAdd(params) => {
            require(Read)?;
            crate::feedback::add(engine, &params, operator)
        }
        ClientRequest::JournalImport(params) => {
            crate::import::handle(engine, connection, params, operator)
        }
        ClientRequest::AccountSetEnvironment(_) if !operator => Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "only an operator client may change the daemon's server",
        )),
        request @ (ClientRequest::AccountGet(_)
        | ClientRequest::AccountFlights(_)
        | ClientRequest::AccountVersions(_)
        | ClientRequest::AccountEnvironment(_)
        | ClientRequest::AccountSetEnvironment(_)
        | ClientRequest::AccountSignIn(_)
        | ClientRequest::AccountVerify(_)
        | ClientRequest::AccountSignOut(_)
        | ClientRequest::AccountSetUpdates(_)
        | ClientRequest::AccountSetUsername(_)
        | ClientRequest::AccountSetName(_)
        | ClientRequest::AccountSetSource(_)
        | ClientRequest::AccountSessions(_)
        | ClientRequest::AccountSignOutSession(_)
        | ClientRequest::AccountSignOutOthers(_)
        | ClientRequest::AccountRequestDeletion(_)
        | ClientRequest::AccountShared(_)
        | ClientRequest::AccountFeedback(_)
        | ClientRequest::AccountClaimFeedback(_)
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
        request @ (ClientRequest::PreflightState(_) | ClientRequest::PreflightConfigure(_)) => {
            crate::preflight::handle(engine, connection, request, &granted, operator)
        }
        request @ (ClientRequest::ChecklistUpdate(_)
        | ClientRequest::ChecklistCopy(_)
        | ClientRequest::ChecklistTick(_)
        | ClientRequest::ChecklistReset(_)
        | ClientRequest::ChecklistExport(_)) => {
            crate::checklist::handle(engine, connection, request, &granted, operator)
        }
        request @ (ClientRequest::SetupState(_)
        | ClientRequest::SetupStart(_)
        | ClientRequest::SetupCancel(_)
        | ClientRequest::SetupConfirm(_)
        | ClientRequest::SetupSpin(_)
        | ClientRequest::SetupFirmware(_)
        | ClientRequest::SetupFirmwareReleases(_)) => {
            crate::setup::handle(engine, connection, request, &granted, operator)
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
        | ClientRequest::SyncActivate(_)
        | ClientRequest::SyncRestore(_)) => {
            crate::sync::handle(engine, connection, request, &granted)
        }
        ClientRequest::FlightsFound(_) => {
            require(Read)?;
            let found = engine
                .system
                .found_flights()
                .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
            to_value(&found)
        }
        ClientRequest::FlightsFolder(params) => {
            require(Config)?;
            let found = engine
                .system
                .flight_folder(&params)
                .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
            to_value(&found)
        }
        ClientRequest::LogsCards(_) => {
            require(Read)?;
            let cards = engine
                .system
                .log_cards()
                .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
            to_value(&cards)
        }
        ClientRequest::LogsCopyCard(params) => {
            require(Config)?;
            engine
                .system
                .copy_card_logs(&params.path)
                .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
            let cards = engine
                .system
                .log_cards()
                .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
            to_value(&cards)
        }
        request @ (ClientRequest::OperationList(_)
        | ClientRequest::OperationCreate(_)
        | ClientRequest::OperationRename(_)
        | ClientRequest::OperationAddFlights(_)
        | ClientRequest::OperationRemoveFlights(_)) => {
            crate::operations::handle(engine, connection, request, &granted)
        }
        request @ (ClientRequest::AttachmentBegin(_)
        | ClientRequest::AttachmentAppend(_)
        | ClientRequest::AttachmentFinish(_)
        | ClientRequest::AttachmentRead(_)) => {
            crate::attachments::handle(engine, connection, request, &granted)
        }
        request @ (ClientRequest::ReplayOpen(_)
        | ClientRequest::FlightSeries(_)
        | ClientRequest::ReplayLogFields(_)
        | ClientRequest::ReplayMotion(_)
        | ClientRequest::ReplaySave(_)
        | ClientRequest::ReplayAdd(_)
        | ClientRequest::ReplayNudge(_)
        | ClientRequest::FlightAddVideo(_)
        | ClientRequest::VideoAlign(_)
        | ClientRequest::ReplayPlay(_)
        | ClientRequest::ReplayPause(_)
        | ClientRequest::ReplayRestart(_)
        | ClientRequest::ReplaySpeed(_)
        | ClientRequest::ReplaySeek(_)
        | ClientRequest::ReplayClose(_)
        | ClientRequest::ReplayState(_)
        | ClientRequest::ReplayRename(_)
        | ClientRequest::ReplayStartTogether(_)) => {
            crate::replay::handle(engine, connection, request, &granted)
        }
        request @ (ClientRequest::SimStart(_)
        | ClientRequest::SimStop(_)
        | ClientRequest::SimRestart(_)
        | ClientRequest::SimPause(_)
        | ClientRequest::SimCrash(_)
        | ClientRequest::SimBinaries(_)
        | ClientRequest::SimFrames(_)
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
        request @ (ClientRequest::VideoDevices(_)
        | ClientRequest::VideoFile(_)
        | ClientRequest::VideoDemos(_)
        | ClientRequest::VideoCapture(_)
        | ClientRequest::VideoCaptureSettings(_)
        | ClientRequest::VideoCaptureEnable(_)
        | ClientRequest::VideoList(_)
        | ClientRequest::VideoAssign(_)
        | ClientRequest::VideoWatch(_)
        | ClientRequest::VideoMetadataWatch(_)
        | ClientRequest::VideoUnwatch(_)
        | ClientRequest::VideoAdd(_)
        | ClientRequest::VideoRemove(_)
        | ClientRequest::VideoRecord(_)
        | ClientRequest::VideoRecordAll(_)
        | ClientRequest::VideoRecordWithOthers(_)
        | ClientRequest::VideoPlayback(_)
        | ClientRequest::VideoRename(_)
        | ClientRequest::VideoTrack(_)
        | ClientRequest::VideoReveal(_)
        | ClientRequest::VideoPhoto(_)
        | ClientRequest::VideoPhotoShow(_)
        | ClientRequest::VideoVehicleCamera(_)) => {
            crate::video::handle(engine, connection, request, &granted)
        }
        request @ (ClientRequest::ParamSearch(_)
        | ClientRequest::ParamGet(_)
        | ClientRequest::ParamList(_)
        | ClientRequest::ParamDiff(_)
        | ClientRequest::ParamChanges(_)) => {
            crate::param_definitions::queries::handle(engine, connection, request, &granted)
        }
        ClientRequest::FlightList(params) => {
            if !granted.contains(&Read) {
                return Err(ApiError::new(
                    ErrorKind::CapabilityDenied,
                    Read.denied_message(),
                ));
            }
            to_value(&engine.list_flights(&params)?)
        }
        ClientRequest::FlightLog(params) => {
            if !granted.contains(&Read) {
                return Err(ApiError::new(
                    ErrorKind::CapabilityDenied,
                    Read.denied_message(),
                ));
            }
            to_value(&engine.flight_log(&params.id)?)
        }
        ClientRequest::FlightRemove(params) => {
            if !granted.contains(&Config) {
                return Err(ApiError::new(
                    ErrorKind::CapabilityDenied,
                    Config.denied_message(),
                ));
            }
            engine.remove_flight(connection, &params.id, params.video)?;
            to_value(&uxduck_contracts::dto::Empty {})
        }
        ClientRequest::FlightRemoval(params) => {
            if !granted.contains(&Read) {
                return Err(ApiError::new(
                    ErrorKind::CapabilityDenied,
                    Read.denied_message(),
                ));
            }
            to_value(&engine.flight_removal(&params.ids)?)
        }
        ClientRequest::FlightKeep(params) => {
            if !granted.contains(&Config) {
                return Err(ApiError::new(
                    ErrorKind::CapabilityDenied,
                    Config.denied_message(),
                ));
            }
            to_value(&engine.keep_flight(
                connection,
                &params.id,
                params.keep,
                params.always,
                params.content.map(Into::into),
            )?)
        }
        ClientRequest::FlightEnd(params) => {
            if !granted.contains(&Config) {
                return Err(ApiError::new(
                    ErrorKind::CapabilityDenied,
                    Config.denied_message(),
                ));
            }
            to_value(&engine.end_lost_flight(&params.id)?)
        }
        ClientRequest::FlightRename(params) => {
            if !granted.contains(&Config) {
                return Err(ApiError::new(
                    ErrorKind::CapabilityDenied,
                    Config.denied_message(),
                ));
            }
            to_value(&engine.rename_flight(connection, &params.id, &params.name)?)
        }
        ClientRequest::FlightUploadLog(params) => {
            if !granted.contains(&Config) {
                return Err(ApiError::new(
                    ErrorKind::CapabilityDenied,
                    Config.denied_message(),
                ));
            }
            to_value(&engine.ask_flight_log(connection, &params.id)?)
        }
        request @ (ClientRequest::ParamSets(_)
        | ClientRequest::ParamSetSave(_)
        | ClientRequest::ParamSetApply(_)
        | ClientRequest::ParamSetRename(_)
        | ClientRequest::ParamSetRemove(_)
        | ClientRequest::ParamSetExport(_)) => {
            crate::param_definitions::saved::handle(engine, connection, request, &granted)
        }
        ClientRequest::TuningState(params) => crate::tuning::handle(engine, &params, &granted),
        ClientRequest::TuningResponse(params) => {
            crate::tuning::handle_response(engine, &params, &granted)
        }
        ClientRequest::TuningLive(params) => crate::tuning::handle_live(engine, &params, &granted),
        ClientRequest::TuningPrepare(params) => {
            crate::tuning::handle_prepare(engine, &params, &granted)
        }
        request @ (ClientRequest::MavlinkDialectsList(_)
        | ClientRequest::MavlinkDialectsAdd(_)
        | ClientRequest::MavlinkDialectsRemove(_)
        | ClientRequest::MavlinkDialectsBind(_)
        | ClientRequest::MavlinkDialectsUse(_)) => {
            crate::dialects::handle(engine, connection, request, &granted)
        }
        request @ (ClientRequest::FirmwareList(_)
        | ClientRequest::FirmwareAdd(_)
        | ClientRequest::FirmwareRemove(_)
        | ClientRequest::FirmwareRename(_)) => {
            crate::firmware_profiles::handle(engine, request, &granted)
        }
        request @ (ClientRequest::ParamDefinitions(_)
        | ClientRequest::ParamDefinitionsList(_)
        | ClientRequest::ParamDefinitionsAdd(_)
        | ClientRequest::ParamDefinitionsRemove(_)
        | ClientRequest::ParamDefinitionsUse(_)) => {
            crate::param_definitions::handle(engine, connection, request, &granted)
        }
        request @ (ClientRequest::DaemonStopPreview(_)
        | ClientRequest::ClientVisibility(_)
        | ClientRequest::AppFocus(_)
        | ClientRequest::AppOpen(_)
        | ClientRequest::DesktopAppStatus(_)
        | ClientRequest::NetworkStatus(_)
        | ClientRequest::DesktopAppDownload(_)
        | ClientRequest::DesktopAppChoose(_)
        | ClientRequest::DesktopAppRemove(_)) => {
            crate::app::handle(engine, connection, &request, operator, &granted)
        }
        request @ (ClientRequest::TrafficStatus(_) | ClientRequest::VehicleTraffic(_)) => {
            crate::traffic::handle(engine, connection, request, operator, &granted)
        }
        request @ (ClientRequest::LocationStatus(_)
        | ClientRequest::LocationStart(_)
        | ClientRequest::LocationStop(_)
        | ClientRequest::TrayLocation(_)) => {
            crate::location::handle(engine, connection, request, operator, &granted)
        }
        request @ (ClientRequest::UpdateStatus(_)
        | ClientRequest::UpdateCheck(_)
        | ClientRequest::UpdateInstall(_)
        | ClientRequest::UpdateLater(_)) => {
            crate::app_update::handle(engine, connection, &request, operator, &granted)
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
        | ClientRequest::SettingsGet(_)
        | ClientRequest::StorageUsage(_)
        | ClientRequest::StorageClearSimulatorLogs(_)
        | ClientRequest::StorageReveal(_)
        | ClientRequest::DaemonWakeups(_)
        | ClientRequest::DaemonAbout(_)
        | ClientRequest::MavlinkInspect(_)
        | ClientRequest::MavlinkHistory(_)) => {
            crate::restart::handle(engine, connection, request, &granted, operator)
        }
        request @ (ClientRequest::KeyboardStart(_)
        | ClientRequest::KeyboardInput(_)
        | ClientRequest::KeyboardStop(_)
        | ClientRequest::GripperHold(_)
        | ClientRequest::ControllerClaim(_)
        | ClientRequest::ControllerTakeOver(_)
        | ClientRequest::ControllerRelease(_)
        | ClientRequest::ControllerForget(_)
        | ClientRequest::ControllerSetTarget(_)
        | ClientRequest::ControllerChooseMapping(_)
        | ClientRequest::ControllerInputs(_)
        | ClientRequest::ClientSelect(_)
        | ClientRequest::MappingUpdate(_)
        | ClientRequest::MappingCreate(_)) => {
            crate::controllers::handle(engine, connection, request, &granted, operator)
        }
        request @ (ClientRequest::ParamRefresh(_)
        | ClientRequest::ParamSet(_)
        | ClientRequest::MissionRefresh(_)
        | ClientRequest::MissionHistory(_)
        | ClientRequest::MissionRestore(_)
        | ClientRequest::TransferCancel(_)
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
        | ClientRequest::AgentConnection(_)
        | ClientRequest::ThreadCreate(_)
        | ClientRequest::ThreadSend(_)
        | ClientRequest::ThreadEdit(_)
        | ClientRequest::ThreadQueue(_)
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

fn operator_profile(engine: &Engine, id: &str) -> Result<Value, ApiError> {
    let operator = engine
        .state
        .operators
        .get(id)
        .ok_or_else(|| ApiError::new(ErrorKind::NotFound, "That operator was removed."))?;
    let receipts: Vec<_> = engine
        .state
        .receipts
        .values()
        .filter(|r| r.operator_id.as_deref() == Some(id))
        .collect();
    let notes: Vec<_> = engine
        .state
        .history
        .values()
        .filter(|entry| {
            entry.author.operator_id.as_deref() == Some(id)
                && matches!(
                    entry.kind,
                    uxduck_core::domain::history::EntryKind::Note { .. }
                )
        })
        .collect();
    to_value(&uxduck_contracts::dto::operator::OperatorProfile {
        operator: operator.into(),
        commands_requested: receipts.len() as u64,
        notes_written: notes.len() as u64,
        last_active_us: receipts
            .iter()
            .map(|r| r.created_at_us)
            .chain(notes.iter().map(|n| n.written_at_us))
            .max(),
    })
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
    if engine
        .connections
        .values()
        .any(|c| c.operator_id.as_deref() == Some(&id))
        || engine.state.controllers.values().any(|c| {
            c.claim
                .as_ref()
                .is_some_and(|claim| claim.operator_id.as_deref() == Some(&id))
        })
    {
        return Err(ApiError::new(
            ErrorKind::ConfirmationRequired,
            "Switch away from this operator and release their controllers before removing them.",
        ));
    }
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
    engine.ensure_operator_handover(connection, operator_id.as_deref())?;
    let previous = engine
        .connections
        .get(&connection)
        .and_then(|c| c.operator_id.as_ref());
    if previous == operator_id.as_ref() {
        return Ok(json!({}));
    }
    // The computer's sign-in stays: every operator here works under it
    // (ADR-0033, one account per computer).
    if let Some(state) = engine.connections.get_mut(&connection) {
        if state.is_local_operator() {
            engine.chosen_operator.clone_from(&operator_id);
        }
        state.operator_id = operator_id;
    }
    engine.operator_changed(connection);
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

#[allow(clippy::too_many_lines, reason = "one field per entity type")]
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
            .then(|| super::threads::threads(engine, connection)),
        items: types
            .contains(&EntityType::Item)
            .then(|| super::threads::items(engine, connection)),
        folders: types
            .contains(&EntityType::Folder)
            .then(|| super::folders::folders(engine, connection)),
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
        markers: types
            .contains(&EntityType::Marker)
            .then(|| crate::markers::sorted(engine, connection)),
        plans: types
            .contains(&EntityType::Plan)
            .then(|| engine.state.plans.values().map(Into::into).collect()),
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
        video_photos: types
            .contains(&EntityType::VideoPhoto)
            .then(|| each(&engine.state.video_photos)),
        practice_attempts: types
            .contains(&EntityType::PracticeAttempt)
            .then(|| engine.state.practice.values().map(Into::into).collect()),
        preflights: types
            .contains(&EntityType::Preflight)
            .then(|| engine.state.preflights.values().map(Into::into).collect()),
        setups: types
            .contains(&EntityType::Setup)
            .then(|| engine.state.setups.values().map(Into::into).collect()),
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
    let mut operator = Operator {
        id: format!("op{}", &engine.random.token()[..16]),
        name: clean_name(&params.name)?,
        units: params.units.map(Into::into).unwrap_or_default(),
        quantity_units: None,
        coordinate_format: params.coordinate_format.map(Into::into).unwrap_or_default(),
        altitude_frame: params.altitude_frame.map(Into::into).unwrap_or_default(),
        layout: operator::Layout::default(),
        place_search: true,
        basemap: operator::Basemap::default(),
        shortcuts: std::collections::BTreeMap::new(),
        skipped_confirmations: Vec::new(),
        telemetry_layouts: Vec::new(),
        vehicle_telemetry: std::collections::BTreeMap::new(),
        calculated_items: Vec::new(),
        parameter_groups: Vec::new(),
        inspector_pinned: Vec::new(),
        control_actions: Vec::new(),
        me_marker_id: None,
        display: None,
        account_id: None,
        controller_mappings: Vec::new(),
        terms: None,
        version: 1,
    };
    if let Some(units) = params.quantity_units {
        operator.set_quantity_units(units.into());
    } else if let Some(units) = params.units {
        operator.set_quantity_units(operator::UnitSystem::from(units).quantity_units());
    }
    let result = OperatorDto::from(&operator);
    engine.commit(connection, Change::OperatorUpserted { operator });
    to_value(&result)
}

/// The Cockpit tab's buttons for every type, each type once and each
/// action once, by action id.
fn control_actions(
    all: &[uxduck_contracts::dto::operator::ControlActions],
) -> Result<Vec<operator::ControlActions>, ApiError> {
    let all: Vec<operator::ControlActions> = all.iter().map(Into::into).collect();
    operator::clean_control_actions(&all).ok_or_else(|| {
        ApiError::new(
            ErrorKind::InvalidValue,
            format!(
                "each vehicle type once, with at most {} actions of up to {} characters, each vehicle: and a-z, 0-9, -, _, : or spaces",
                operator::MAX_CONTROL_ACTIONS,
                operator::MAX_CONTROL_ACTION_CHARS,
            ),
        )
    })
}

/// An operator's calculated values (ADR-0056 K1, K2), each as ADR-0056
/// says, and none another operator here works out differently under the
/// same key: every operator's are worked out for every vehicle.
fn calculated_items(
    engine: &Engine,
    operator_id: &str,
    items: &[uxduck_contracts::dto::operator::CalculatedItem],
) -> Result<Vec<uxduck_core::domain::calculated::CalculatedItem>, ApiError> {
    let items: Vec<uxduck_core::domain::calculated::CalculatedItem> =
        items.iter().map(Into::into).collect();
    let items = uxduck_core::domain::calculated::clean_calculated(&items)
        .map_err(|message| ApiError::new(ErrorKind::InvalidValue, message))?;
    for item in &items {
        let other = engine
            .state
            .operators
            .values()
            .filter(|o| o.id != operator_id)
            .find_map(|o| {
                o.calculated_items
                    .iter()
                    .find(|c| {
                        c.key == item.key && (c.formula != item.formula || c.unit != item.unit)
                    })
                    .map(|_| o)
            });
        if let Some(other) = other {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                format!(
                    "{} already works out {} another way on this computer. Name {} something else.",
                    other.name, item.key, item.label
                ),
            ));
        }
    }
    Ok(items)
}

/// An operator's display document (C1), kept as sent: the app owns its
/// shape; here only its size and that it is a JSON object.
fn display_document(document: &str) -> Result<String, ApiError> {
    let object = document.len() <= operator::MAX_DISPLAY_BYTES
        && serde_json::from_str::<Value>(document).is_ok_and(|v| v.is_object());
    if object {
        Ok(document.to_owned())
    } else {
        Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!(
                "send the display settings as a JSON object of at most {} KiB",
                operator::MAX_DISPLAY_BYTES / 1024
            ),
        ))
    }
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
    if let Some(units) = params.quantity_units {
        operator.set_quantity_units(units.into());
    } else if let Some(units) = params.units {
        operator.set_quantity_units(operator::UnitSystem::from(units).quantity_units());
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
    if let Some(items) = &params.calculated_items {
        operator.calculated_items = calculated_items(engine, &operator.id, items)?;
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
    if let Some(me) = &params.me_marker_id {
        if let Some(id) = me
            && !engine.state.markers.contains_key(id)
        {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                format!("no marker {id}"),
            ));
        }
        operator.me_marker_id.clone_from(me);
    }
    if let Some(all) = &params.control_actions {
        operator.control_actions = control_actions(all)?;
    }
    if let Some(document) = &params.display {
        operator.display = Some(display_document(document)?);
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
    // Controllers are in the Cockpit (ADR-0005, 2026-11-04): asked for, the
    // Cockpit opens, and the window that sees the call shows its controllers.
    let kind = match params.kind.into() {
        operator::TabKind::Controllers => operator::TabKind::Control,
        kind => kind,
    };
    if kind == operator::TabKind::Chat {
        return Err(ApiError::new(
            ErrorKind::InvalidParams,
            "the chat is always open",
        ));
    }
    let tab = given_tab(engine, kind, params)?;
    let result = |operator_id| LayoutOpenResult {
        operator_id,
        kind: params.kind,
        vehicle_id: tab.vehicle_id.clone(),
        chart: tab.chart.as_ref().map(Into::into),
        item: tab.item.clone(),
        section: tab.section.map(Into::into),
    };
    let from_thread = params
        .thread_id
        .as_ref()
        .and_then(|id| engine.state.threads.get(id))
        .and_then(|thread| thread.operator_id.clone());
    let Some(id) = from_thread.or_else(|| params.operator_id.clone()) else {
        return to_value(&result(None));
    };
    let mut operator = engine
        .state
        .operators
        .get(&id)
        .cloned()
        .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no operator {id}")))?;
    let shape = operator.layout.dock.shape();
    let opened = result(Some(id));
    operator.layout.open_tab(tab);
    // A tab added is arranging the dock, as "+" is (S65).
    operator.layout.arranged |= operator.layout.dock.shape() != shape;
    operator.version += 1;
    engine.commit(connection, Change::OperatorUpserted { operator });
    to_value(&opened)
}

/// The tab `layout/open` asks for, with what it shows checked, so a tab
/// never opens empty or on a vehicle that is not there.
fn given_tab(
    engine: &Engine,
    kind: operator::TabKind,
    params: &LayoutOpenParams,
) -> Result<operator::Tab, ApiError> {
    let invalid = |message: &str| ApiError::new(ErrorKind::InvalidParams, message);
    let items: Vec<String> = params.items.iter().map(|i| i.trim().to_owned()).collect();
    if items.iter().any(|i| i.is_empty() || i.len() > 64) {
        return Err(invalid("each item is a telemetry key from telemetry_get"));
    }
    match kind {
        _ if items.is_empty() => {}
        operator::TabKind::Chart if items.len() <= operator::MAX_CHART_SERIES => {}
        operator::TabKind::Readout if items.len() == 1 => {}
        operator::TabKind::Chart => return Err(invalid("a chart draws at most 8 items")),
        operator::TabKind::Readout => return Err(invalid("a readout shows one item")),
        _ => return Err(invalid("items are only for a chart or a readout")),
    }
    if params.section.is_some() && kind != operator::TabKind::Tuning {
        return Err(invalid("a section is only for a tuning tab"));
    }
    let vehicle = params
        .vehicle_id
        .as_deref()
        .map(|id| {
            engine
                .vehicle(id)
                .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no vehicle {id}")))
        })
        .transpose()?;
    // An item the vehicle does not report draws nothing: say which, so the
    // agent reads the vehicle's keys instead of claiming a chart is there.
    // A tab given no vehicle follows the chat's, so that one's keys count.
    let shown = vehicle.or_else(|| {
        params
            .thread_id
            .as_ref()
            .and_then(|id| engine.state.threads.get(id))
            .and_then(|thread| thread.vehicle_id.as_deref())
            .and_then(|id| engine.vehicle(id))
    });
    if let Some(vehicle) = shown
        && let Some(telemetry) = engine.telemetry(&vehicle.id)
    {
        let unknown: Vec<&str> = items
            .iter()
            .map(String::as_str)
            .filter(|key| {
                !key.starts_with("mavlink.") && !telemetry.items.iter().any(|i| i.key == *key)
            })
            .collect();
        if !unknown.is_empty() {
            return Err(invalid(&format!(
                "{} does not report {}. Read its keys with telemetry_get",
                vehicle.name,
                unknown.join(", ")
            )));
        }
    }
    let chart = (kind == operator::TabKind::Chart && !items.is_empty()).then(|| {
        let mut chart = operator::ChartSpec {
            series: items
                .iter()
                .map(|item| operator::ChartSeries {
                    item: item.clone(),
                    colour: 0,
                    hidden: false,
                })
                .collect(),
            span_s: None,
        };
        chart.tidy();
        chart
    });
    Ok(operator::Tab {
        vehicle_id: vehicle.map(|v| v.id.clone()),
        chart,
        item: (kind == operator::TabKind::Readout)
            .then(|| items.into_iter().next())
            .flatten(),
        section: params.section.map(Into::into),
        ..operator::Tab::of(kind)
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
    engine: &mut Engine,
    connection: ConnectionId,
    notification: &ClientNotification,
) {
    match notification {
        ClientNotification::Cancel(params) => {
            engine.cancel_approval(connection, &json!(params.id));
            if let Some(client) = engine.connections.get_mut(&connection) {
                client.pending.remove(&params.id);
                client.private_requests.remove(&params.id);
            }
        }
    }
}

/// A CLI command follows the operator chosen last on this computer without
/// switching or restoring one.
fn current_cli_operator(
    engine: &Engine,
    connection: ConnectionId,
    kind: ClientKind,
) -> Option<String> {
    let state = engine.connections.get(&connection)?;
    if kind != ClientKind::OperatorCli
        || state.transport != Transport::Socket
        || state.trust != admission::Trust::LocalUser
    {
        return None;
    }
    engine
        .chosen_operator
        .as_ref()
        .filter(|id| engine.state.operators.contains_key(*id))
        .cloned()
}

#[allow(clippy::too_many_lines, reason = "each admission rule in order")]
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
    // UXDuck's page from before lines, areas and buildings (ADR-0052,
    // ADR-0053) rejects a building and shows a shape as a point.
    if params.client_info.kind == uxduck_contracts::dto::initialize::ClientKind::OperatorUi
        && params.client_info.name == "uxduck-web"
        && !capabilities.marker_shapes
    {
        return Err(update_required(format!(
            "This page ({}) is too old for UXDuck on this computer. Reload to update it.",
            params.client_info.version
        )));
    }

    if params.client_info.kind == uxduck_contracts::dto::initialize::ClientKind::OperatorUi
        && params.client_info.name == "uxduck-web"
        && !capabilities.mission_read_float_bits
    {
        return Err(update_required(
            "This page cannot read exact mission values. Reload a matching interface before connecting to this Core.".into(),
        ));
    }

    let kind = params.client_info.kind;
    let cli_operator = current_cli_operator(engine, connection, kind);
    if kind == uxduck_contracts::dto::initialize::ClientKind::Plugin {
        return Err(ApiError::new(
            ErrorKind::InvalidParams,
            "plugins are run by the daemon. A connection cannot be one",
        ));
    }
    let default = admission::granted(kind.into(), &state.trust);
    let operator = default.len() == uxduck_core::domain::client::Capability::ALL.len();
    let client_id = state.client_id.clone();
    let executable = state.executable.clone();
    // A phone or tablet on the network has the record it paired with (S97).
    let network = match &state.trust {
        admission::Trust::NetworkBrowser { client, .. } => Some(client.clone()),
        _ => None,
    };
    let (record, core_granted) = match network {
        Some(client) => {
            let Some(granted) = engine.admit_network(&client) else {
                return Err(ApiError::new(
                    ErrorKind::CapabilityDenied,
                    "This device was forgotten on the computer running UXDuck. Reload to pair it again.",
                ));
            };
            (client, granted)
        }
        None => engine.admit_record(
            kind.into(),
            &params.client_info.name,
            executable.as_deref(),
            operator,
            default,
        ),
    };
    let granted: Vec<Capability> = core_granted.iter().copied().map(Into::into).collect();
    engine.opened.seen_order += 1;
    if let Some(state) = engine.connections.get_mut(&connection) {
        state.operator_id = cli_operator;
        state.name.clone_from(&params.client_info.name);
        state.seen = engine.opened.seen_order;
        state.admitted = Some(Admitted {
            kind: kind.into(),
            reads: ReadCapabilities {
                mission_read_float_bits: capabilities.mission_read_float_bits,
                replay_step_marks: capabilities.replay_step_marks,
            },
            operator,
            record,
            granted: core_granted,
            extra: Vec::new(),
            approvals: params.capabilities.approvals,
        });
    }
    engine.window_admitted(kind.into());
    // A reloaded window takes back the controllers it held.
    engine.adopt_claims(connection);
    to_value(&InitializeResult {
        daemon_version: engine.config.daemon_version.clone(),
        protocol_version: PROTOCOL_VERSION,
        min_client_version: MIN_CLIENT_VERSION.into(),
        capabilities: DaemonCapabilities {
            mission_read_float_bits: true,
            unused_controls: true,
            terms: true,
            survey_planning: true,
            terrain_polygons: true,
            marker_shapes: true,
            raw_mission_numbers: true,
            ..engine.config.capabilities.clone()
        },
        client_id,
        granted,
        release_stage: uxduck_contracts::version::ReleaseStage::Flight,
    })
}

pub(crate) fn answer(
    engine: &mut Engine,
    code: &str,
    approve: bool,
    access: Option<uxduck_core::domain::client::NetworkAccess>,
) -> Result<Value, ApiError> {
    let answered = engine
        .sessions
        .answer_pairing(code, approve, access, engine.clock.as_ref());
    // A question for a browser that has gone is cleared, answered or not.
    engine.sync_pairing_questions();
    if answered {
        Ok(json!({}))
    } else {
        Err(ApiError::new(
            ErrorKind::NotFound,
            "that browser stopped waiting. Reload it to ask again",
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
