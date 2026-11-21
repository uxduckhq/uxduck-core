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
    // A camera's command is its vehicle's (S405): from here on it is the
    // `command/send` it stands for, with that method's checks and approval.
    let request = match request {
        ClientRequest::CameraCommand(params) => crate::cameras::as_command(engine, &params)?,
        // And a gimbal's (S406).
        ClientRequest::GimbalCommand(params) => crate::gimbals::as_command(engine, &params)?,
        other => other,
    };
    engine.check_mission_read_request(connection, &request)?;
    // A person flies AutoTune (S332): no approval lets an agent start it.
    if admitted.kind == uxduck_core::domain::client::ClientKind::Agent
        && let Some(why) = crate::autotune::agent_refusal(engine, &request)
    {
        return Err(ApiError::new(ErrorKind::CapabilityDenied, why));
    }
    // What no approval lets through for a terminal (ADR-0065 T7): an agent
    // or a plugin, one command from a client that asks, and a terminal that
    // could not open anyway.
    if let Some(refused) = crate::terminals::refusal(engine, connection, &request) {
        return Err(refused);
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
        ClientRequest::ClientActivity(_) => {
            require_operator()?;
            to_value(&engine.activity_snapshot())
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
        ClientRequest::ComputerRename(params) => {
            require(Config)?;
            engine.rename_computer(connection, params)
        }
        ClientRequest::ComputerConnectivity(params) => {
            require(Config)?;
            engine.configure_connectivity(connection, &params)
        }
        ClientRequest::ComputerNetwork(params) => {
            require(Config)?;
            engine.network_computer(connection, &params)
        }
        ClientRequest::ComputerCameras(params) => {
            require(Config)?;
            engine.cameras_computer(connection, &params)
        }
        ClientRequest::ComputerCheck(params) => {
            require(Config)?;
            engine.check_computer(connection, params)
        }
        request @ (ClientRequest::TerminalOpen(_) | ClientRequest::ComputerRun(_)) => {
            require(Terminal)?;
            crate::terminals::handle(engine, connection, request)
        }
        // What agents ran is the operator's to read.
        request @ ClientRequest::ComputerRuns(_) => {
            require_operator()?;
            crate::terminals::handle(engine, connection, request)
        }
        // The engine lets only a terminal's holder or an operator through.
        request @ (ClientRequest::TerminalResize(_) | ClientRequest::TerminalClose(_)) => {
            require(Read)?;
            crate::terminals::handle(engine, connection, request)
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
        | ClientRequest::VehicleForgetConnection(_)
        | ClientRequest::VehicleUseConnection(_)
        | ClientRequest::VehicleConnectionHistory(_)
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
        request @ (ClientRequest::PlaceSearch(_)
        | ClientRequest::PlaceLocate(_)
        | ClientRequest::PlaceArea(_)
        | ClientRequest::PlaceAreas(_)
        | ClientRequest::PlaceAreaAnswer(_)) => {
            crate::places::handle(engine, connection, request, &granted, operator)
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
            crate::feedback::file(engine, connection, &params)
        }
        ClientRequest::FeedbackAdd(params) => {
            require(Read)?;
            crate::feedback::add(engine, connection, &params, operator)
        }
        ClientRequest::JournalImport(params) => {
            crate::import::handle(engine, connection, params, operator)
        }
        ClientRequest::AccountToolCredits(_) if !operator => Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "only an operator client may manage tool metadata credits",
        )),
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
        | ClientRequest::AccountToolCredits(_)
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
        | ClientRequest::ChecklistFinish(_)
        | ClientRequest::ChecklistReopen(_)
        | ClientRequest::ChecklistRuns(_)
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
        ClientRequest::FlightsFound(params) => {
            require(Read)?;
            let mut found = engine
                .system
                .found_flights(&params)
                .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
            // A disk plugged in shows its flights in the Logbook (ADR-0062).
            engine.adopt_found_flight_files(&found.flight_files);
            engine.count_folder_entries(&mut found);
            to_value(&found)
        }
        ClientRequest::FlightsFolder(params) => {
            require(Config)?;
            let mut found = engine
                .system
                .flight_folder(&params)
                .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
            engine.count_folder_entries(&mut found);
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
        | ClientRequest::AttachmentRead(_)
        | ClientRequest::AttachmentImage(_)) => {
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
        | ClientRequest::SimInspect(_)
        | ClientRequest::SimOwnBuild(_)
        | ClientRequest::SimCatalogue(_)
        | ClientRequest::SimDownload(_)
        | ClientRequest::SimDelete(_)
        | ClientRequest::SimFault(_)
        | ClientRequest::SimFaults(_)
        | ClientRequest::SimPictures(_)) => {
            crate::sim::handle(engine, connection, request, &granted)
        }
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
        | ClientRequest::VideoAddresses(_)
        | ClientRequest::VideoInternet(_)
        | ClientRequest::VideoTrack(_)
        | ClientRequest::VideoReveal(_)
        | ClientRequest::VideoPhoto(_)
        | ClientRequest::VideoPhotoShow(_)
        | ClientRequest::VideoFrame(_)
        | ClientRequest::VideoVehicleCamera(_)
        | ClientRequest::VideoServedBy(_)) => {
            crate::video::handle(engine, connection, request, &granted)
        }
        ClientRequest::CameraList(_) => crate::cameras::list(engine, &granted),
        ClientRequest::CameraCommand(_) => Err(ApiError::new(
            ErrorKind::Internal,
            "camera/command is handled as command/send",
        )),
        ClientRequest::CameraImages(params) => crate::cameras::photos(engine, &params, &granted),
        ClientRequest::CameraSettings(params) => {
            crate::camera_settings::settings(engine, &params, &granted)
        }
        // A person agrees to the address: no agent or plugin does for them.
        ClientRequest::CameraDefinitionFetch(params) => {
            require_operator()?;
            crate::camera_settings::fetch(engine, &params)
        }
        ClientRequest::GimbalList(_) => crate::gimbals::list(engine, &granted),
        ClientRequest::GimbalWatch(params) => {
            crate::gimbals::watch(engine, connection, &params, &granted)
        }
        ClientRequest::GimbalCommand(_) => Err(ApiError::new(
            ErrorKind::Internal,
            "gimbal/command is handled as command/send",
        )),
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
        ClientRequest::FlightOffload(params) => {
            if !granted.contains(&Config) {
                return Err(ApiError::new(
                    ErrorKind::CapabilityDenied,
                    Config.denied_message(),
                ));
            }
            to_value(&engine.offload_flights(&params)?)
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
        | ClientRequest::ControllerNavigate(_)
        | ClientRequest::ClientSelect(_)
        | ClientRequest::MappingUpdate(_)
        | ClientRequest::MappingRemove(_)
        | ClientRequest::MappingCreate(_)) => {
            crate::controllers::handle(engine, connection, request, &granted, operator)
        }
        request @ (ClientRequest::ParamRefresh(_)
        | ClientRequest::ParamSet(_)
        | ClientRequest::MissionRefresh(_)
        | ClientRequest::MissionHistory(_)
        | ClientRequest::MissionGet(_)
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
    // The outlines a window shows are its operator's (FB-177).
    engine.send_areas_to(connection);
    Ok(json!({}))
}

use uxduck_core::domain::client::Capability::{Config, Read, Terminal};

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
        // Without a client's own request for a terminal (ADR-0065 T7), and
        // without a command only an operator is to read (T14).
        approvals: types.contains(&EntityType::Approval).then(|| {
            engine
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
                .collect()
        }),
        receipts: types
            .contains(&EntityType::Receipt)
            .then(|| engine.state.receipts.values().map(Into::into).collect()),
        notifications: types.contains(&EntityType::Notification).then(|| {
            engine
                .state
                .notifications
                .values()
                .filter(|n| !engine.hides_notice(connection, n.condition.as_deref()))
                .map(Into::into)
                .collect()
        }),
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
        cameras: types
            .contains(&EntityType::Camera)
            .then(|| crate::cameras::listed(engine)),
        gimbals: types
            .contains(&EntityType::Gimbal)
            .then(|| crate::gimbals::listed(engine)),
        terminals: types
            .contains(&EntityType::Terminal)
            .then(|| crate::terminals::listed(engine)),
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
/// action once, by action id, however many a type has.
fn control_actions(
    all: &[uxduck_contracts::dto::operator::ControlActions],
) -> Result<Vec<operator::ControlActions>, ApiError> {
    let all: Vec<operator::ControlActions> = all.iter().map(Into::into).collect();
    operator::clean_control_actions(&all).ok_or_else(|| {
        ApiError::new(
            ErrorKind::InvalidValue,
            format!(
                "each vehicle type once, and each action up to {} characters, vehicle: and a-z, 0-9, -, _, : or spaces",
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
    // Every other operator's values by key, read once, so that checking
    // takes time in step with how many values there are, not its square.
    let mut others: std::collections::HashMap<&str, Vec<(&Operator, &_)>> =
        std::collections::HashMap::new();
    for other in engine.state.operators.values() {
        if other.id != operator_id {
            for theirs in &other.calculated_items {
                others
                    .entry(theirs.key.as_str())
                    .or_default()
                    .push((other, theirs));
            }
        }
    }
    for item in &items {
        let other = others.get(item.key.as_str()).and_then(|same| {
            same.iter()
                .find(|(_, c)| c.formula != item.formula || c.unit != item.unit)
                .map(|(o, _)| *o)
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
/// shape; here only that it is a JSON object. It has no size of its own to
/// keep to: it holds what the person saved, and what bounds it is what one
/// request may carry and what the account takes of one synced change, which
/// is said where a change is too large to sync.
fn display_document(document: &str) -> Result<String, ApiError> {
    if serde_json::from_str::<Value>(document).is_ok_and(|v| v.is_object()) {
        Ok(document.to_owned())
    } else {
        Err(ApiError::new(
            ErrorKind::InvalidValue,
            "send the display settings as a JSON object",
        ))
    }
}

/// A layout's saved layouts, each with a name a person can read in a menu.
/// A name is checked here, where it is given, so that one too long is
/// refused with words and its layout is never dropped.
fn saved_layout_names(layout: &uxduck_contracts::dto::operator::Layout) -> Result<(), ApiError> {
    for saved in &layout.saved {
        if operator::clean_layout_name(&saved.name).is_none() {
            let length = saved.name.trim().chars().count();
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                if length == 0 {
                    "Name each saved layout.".to_owned()
                } else {
                    format!(
                        "A saved layout's name is at most {} characters, and “{}” is {length}.",
                        operator::MAX_LAYOUT_NAME_CHARS,
                        saved.name.trim(),
                    )
                },
            ));
        }
    }
    Ok(())
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
        saved_layout_names(&layout)?;
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
            uxduck_core::domain::telemetry_layout::cleaned(layouts, chosen)
                .map_err(|message| ApiError::new(ErrorKind::InvalidValue, message))?;
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
                    "each parameter group a unique name of 1 to {} characters, and each parameter 1 to {} characters A-Z, 0-9 or _",
                    operator::MAX_GROUP_NAME_CHARS,
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
                    "each pinned message received: or sent: and 1 to {} characters A-Z, 0-9, _ or #",
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
    // A Terminal tab is opened by the person, from its computer's card or
    // the palette, with the computer it is for (ADR-0065 T6).
    if kind == operator::TabKind::Terminal {
        return Err(ApiError::new(
            ErrorKind::InvalidParams,
            "a terminal is opened from its computer's card",
        ));
    }
    let tab = given_tab(engine, kind, params)?;
    let plan_id = given_plan(engine, kind, params)?;
    let result = |operator_id| LayoutOpenResult {
        operator_id,
        kind: params.kind,
        vehicle_id: tab.vehicle_id.clone(),
        chart: tab.chart.as_ref().map(Into::into),
        item: tab.item.clone(),
        section: tab.section.map(Into::into),
        plan_id: plan_id.clone(),
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

/// The saved plan `layout/open` shows on a map (FB-178): one that is there,
/// on a map, so the agent never says it showed a plan the map cannot draw.
fn given_plan(
    engine: &Engine,
    kind: operator::TabKind,
    params: &LayoutOpenParams,
) -> Result<Option<String>, ApiError> {
    let Some(id) = params.plan_id.as_deref() else {
        return Ok(None);
    };
    if !matches!(kind, operator::TabKind::Map | operator::TabKind::Map3d) {
        return Err(ApiError::new(
            ErrorKind::InvalidParams,
            "a plan is shown on a map or a map-3d tab",
        ));
    }
    match engine.state.plans.get(id) {
        Some(plan) if plan.deleted_at_us.is_none() => Ok(Some(plan.id.clone())),
        _ => Err(ApiError::new(
            ErrorKind::NotFound,
            format!("no plan {id}. Read the saved plans with plan_list"),
        )),
    }
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
        operator::TabKind::Chart => {}
        operator::TabKind::Readout if items.len() == 1 => {}
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

/// The longest action id and key of a changed shortcut, in bytes.
const MAX_SHORTCUT_TEXT: usize = 64;

/// Shortcuts are the app's to read; the daemon checks only that each names
/// an action and a key. However many a person changed, each is kept.
fn clean_shortcuts(
    shortcuts: &[uxduck_contracts::dto::operator::ShortcutChange],
) -> Result<std::collections::BTreeMap<String, Option<String>>, ApiError> {
    let too_long = |text: &str| text.is_empty() || text.len() > MAX_SHORTCUT_TEXT;
    if shortcuts
        .iter()
        .any(|c| too_long(&c.action) || c.shortcut.as_deref().is_some_and(too_long))
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!("each shortcut an action id and a key of 1 to {MAX_SHORTCUT_TEXT} characters"),
        ));
    }
    Ok(shortcuts
        .iter()
        .map(|c| (c.action.clone(), c.shortcut.clone()))
        .collect())
}

/// Without repeats, however many there are; the app knows which actions may
/// be skipped.
fn clean_skipped(skipped: &[String]) -> Result<Vec<String>, ApiError> {
    if skipped
        .iter()
        .any(|id| id.is_empty() || id.len() > MAX_SHORTCUT_TEXT)
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!(
                "each skipped confirmation an action id of 1 to {MAX_SHORTCUT_TEXT} characters"
            ),
        ));
    }
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    Ok(skipped
        .iter()
        .filter(|id| seen.insert(id.as_str()))
        .cloned()
        .collect())
}

pub(crate) fn notify(
    engine: &mut Engine,
    connection: ConnectionId,
    notification: &ClientNotification,
) {
    match notification {
        ClientNotification::Cancel(params) => {
            engine.tool_interrupted(
                connection,
                &params.id,
                uxduck_contracts::dto::analytics::ToolInterruption::Cancelled,
            );
            engine.cancel_approval(connection, &json!(params.id));
            engine.cancel_place_area(connection, &json!(params.id));
            engine.stop_looks(connection, Some(&json!(params.id)));
            engine.stop_runs(connection, &json!(params.id));
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
    // The agent host inside the daemon is the only internal one.
    let outside = kind == ClientKind::Agent && state.transport != Transport::Internal;
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
                shell_commands: capabilities.shell_commands,
            },
            operator,
            record,
            granted: core_granted,
            extra: Vec::new(),
            approvals: params.capabilities.approvals,
        });
    }
    if outside {
        engine.mcp_connected(connection, &params.client_info.name);
    }
    if kind == ClientKind::Agent {
        engine.tool_session_connected(connection);
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
