//! The four RPC enums, each tagged on `method` (ADR-0001 D2).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::dto::Empty;
use crate::dto::account::{
    AccountEnvironmentParams, AccountSessionParams, AccountSignInParams, AccountUpdatesParams,
    AccountUsernameParams, AccountVerifyParams,
};
use crate::dto::analytics::{
    AnalyticsActiveTimeParams, AnalyticsAliasParams, AnalyticsPerformanceParams, AppEvent,
};
use crate::dto::app::{
    AppOpenParams, ClientVisibilityParams, DesktopAppChooseParams, DesktopAppStatusParams,
};
use crate::dto::client::{
    Approval, ApprovalResolveParams, CapabilitiesChanged, ClientForgetParams,
    ClientSetCapabilitiesParams, DisplayPullParams, DisplayRegisterParams, DisplayReplyParams,
    DisplayRequested,
};
use crate::dto::command::{CommandSendParams, ModeListParams};
use crate::dto::controller::{
    ClientSelectParams, ControllerClaimParams, ControllerSetTargetParams, MappingCreateParams,
    MappingUpdateParams,
};
use crate::dto::diagnostics::{
    DaemonLogsParams, LinkDiagnoseParams, RouterEventsParams, SocketInspectParams,
};
use crate::dto::feedback::{FeedbackSendParams, FeedbackSuggestParams};
use crate::dto::find::{FindResult, FindStartParams};
use crate::dto::history::{
    AddNoteParams, EditNoteParams, ParamHistoryParams, VehicleHistoryParams,
};
use crate::dto::initialize::InitializeParams;
use crate::dto::inspect::{MavlinkHistoryParams, MavlinkInspectParams};
use crate::dto::journal::JournalImportParams;
use crate::dto::link::{IdParams, LinkAddParams, LinkUpdateParams};
use crate::dto::location::TrayLocationParams;
use crate::dto::marker::{MarkerAddParams, MarkerShareParams, MarkerUpdateParams};
use crate::dto::messages::{VehicleMessage, VehicleMessagesParams};
use crate::dto::mission::MissionUploadParams;
use crate::dto::operator::{
    ClientSetOperatorParams, LayoutOpenParams, OperatorCreateParams, OperatorProfileParams,
    OperatorRemoveParams, OperatorUpdateParams,
};
use crate::dto::pairing::PairingAnswerParams;
use crate::dto::param_definitions::{
    DefinitionsAddParams, DefinitionsListParams, ParamDefinitionsParams, ParamDiffParams,
    ParamGetParams, ParamListParams, ParamSearchParams,
};
use crate::dto::parameter::ParamSetParams;
use crate::dto::plan::{
    PlanCreateParams, PlanExportParams, PlanImportParams, PlanListParams, PlanPlaceParams,
    PlanStepParams, PlanUpdateParams, PlanUploadParams,
};
use crate::dto::plugin::{
    PluginCommandParams, PluginDraftParams, PluginInstallParams, PluginLayer, PluginShareParams,
    PluginTestParams, PluginTokenParams, ToolSearchParams,
};
use crate::dto::search::{
    PlaceLocateParams, PlaceSaveParams, PlaceSearchParams, PlaceUpdateParams, SearchQueryParams,
};
use crate::dto::settings::{DaemonRestartParams, SettingsUpdateParams};
use crate::dto::sim::{
    SimBuildParams, SimCatalogueParams, SimPauseParams, SimSnapshotParams, SimStartParams,
    SnapshotRenameParams, SnapshotStartParams,
};
use crate::dto::state::{StateDelta, StateSubscribeParams};
use crate::dto::sync::{SyncActivateParams, SyncSetParams};
use crate::dto::telemetry::VehicleTelemetry;
use crate::dto::telemetry_window::TelemetryWindowParams;
use crate::dto::thread::ThreadFeedbackParams;
use crate::dto::thread::{
    AgentItemDelta, CredentialsSetParams, DeleteParams, FolderCreateParams, FolderUpdateParams,
    ThreadBulkParams, ThreadCancelParams, ThreadCreateParams, ThreadSendParams, ThreadUpdateParams,
};
use crate::dto::trace::{RequestId, TraceFrame};
use crate::dto::tray::{FeedbackScreenshotParams, TrayActivateParams, TrayMenu};
use crate::dto::update::AppUpdateInstallParams;
use crate::dto::vehicle::{
    VehicleBookmarksParams, VehicleLabelParams, VehicleLookParams, VehicleSaveParams,
    VehicleSizeParams,
};
use crate::dto::video::{
    VideoAddParams, VideoAssignParams, VideoCaptureEnableParams, VideoCaptureParams,
    VideoCaptureSettingsParams, VideoDemosParams, VideoFileParams, VideoPhotoParams,
    VideoPhotoShowParams, VideoPlaybackParams, VideoRecordAllParams, VideoRecordParams,
    VideoRecordWithOthersParams, VideoRemoveParams, VideoRenameParams, VideoRevealParams,
    VideoTrackParams, VideoUnwatchParams, VideoVehicleCameraParams, VideoWatchParams,
};

/// A request from a client to the daemon. Results are listed in `Result`
/// types named after the method (`InitializeResult`, ...).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "method", content = "params")]
pub enum ClientRequest {
    /// The first call on every connection. Result: `InitializeResult`.
    #[serde(rename = "initialize")]
    Initialize(InitializeParams),
    /// Mirror every frame in and out as `trace/frame` notifications. Operators only. Result: `Empty`.
    #[serde(rename = "trace/subscribe")]
    TraceSubscribe(Empty),
    /// Browsers waiting to pair. Operators only. Result: `PairingListResult`.
    #[serde(rename = "pairing/list")]
    PairingList(Empty),
    /// Pair the browser showing this code. Operators only. Result: `Empty`.
    #[serde(rename = "pairing/approve")]
    PairingApprove(PairingAnswerParams),
    /// Refuse the browser showing this code. Operators only. Result: `Empty`.
    #[serde(rename = "pairing/deny")]
    PairingDeny(PairingAnswerParams),
    /// A one-time token for opening the UI already paired (`uxduck open`).
    /// Operators on the local socket only. Result: `BootstrapToken`.
    #[serde(rename = "session/createBootstrap")]
    SessionCreateBootstrap(Empty),
    /// A snapshot of the given entity types, then `state/delta` for every
    /// change. Needs `read`. Result: `StateSubscribeResult`.
    #[serde(rename = "state/subscribe")]
    StateSubscribe(StateSubscribeParams),
    /// Every operator. Needs `read`. Result: `OperatorListResult`.
    #[serde(rename = "operator/list")]
    OperatorList(Empty),
    /// Activity attributed to a local operator. Needs read. Result: `OperatorProfile`.
    #[serde(rename = "operator/profile")]
    OperatorProfile(OperatorProfileParams),
    /// Needs `config`. Result: `Operator`.
    #[serde(rename = "operator/create")]
    OperatorCreate(OperatorCreateParams),
    /// Needs `config`. Result: `Operator`.
    #[serde(rename = "operator/update")]
    OperatorUpdate(OperatorUpdateParams),
    /// Needs `config`. Result: `Empty`.
    #[serde(rename = "operator/remove")]
    OperatorRemove(OperatorRemoveParams),
    /// Act as this operator from now on. Result: `Empty`.
    #[serde(rename = "client/setOperator")]
    ClientSetOperator(ClientSetOperatorParams),
    /// Switch people with account sign-out and control handover guards.
    /// New clients use this method so older daemons fail closed. Result: `Empty`.
    #[serde(rename = "operator/switch")]
    OperatorSwitch(ClientSetOperatorParams),
    /// The vehicles the daemon hears or has saved, without dismissed ones.
    /// Needs `read`. Result: `VehicleListResult`.
    #[serde(rename = "vehicle/list")]
    VehicleList(Empty),
    /// A new, empty thread. Needs `control`. Result: `Thread`.
    #[serde(rename = "thread/create")]
    ThreadCreate(ThreadCreateParams),
    /// Adds a user message and starts the agent's turn. Needs `control`.
    /// Result: `Item` (the user message).
    #[serde(rename = "thread/send")]
    ThreadSend(ThreadSendParams),
    /// Needs `control`. Result: `Thread`.
    #[serde(rename = "thread/update")]
    ThreadUpdate(ThreadUpdateParams),
    /// Stops the turn in progress. Needs `control`. Result: `Empty`.
    #[serde(rename = "thread/cancel")]
    ThreadCancel(ThreadCancelParams),
    /// Models and whether the provider is ready. Needs `read`. Result: `AgentConfig`.
    #[serde(rename = "agent/config")]
    AgentConfig(Empty),
    /// Stores a secret such as the OpenAI key. Operators over the local
    /// socket only. Result: `Empty`.
    #[serde(rename = "credentials/set")]
    CredentialsSet(CredentialsSetParams),
    /// Deletes a thread as a tombstone; its items stay. Needs `control`.
    /// Result: `Empty`.
    #[serde(rename = "thread/delete")]
    ThreadDelete(DeleteParams),
    /// One action on many threads. Needs `control`. Result: `ThreadBulkResult`.
    #[serde(rename = "thread/bulk")]
    ThreadBulk(ThreadBulkParams),
    /// Needs `control`. Result: `Folder`.
    #[serde(rename = "folder/create")]
    FolderCreate(FolderCreateParams),
    /// Renames, moves or reorders a folder. Needs `control`. Result: `Folder`.
    #[serde(rename = "folder/update")]
    FolderUpdate(FolderUpdateParams),
    /// Deletes a folder as a tombstone; its threads and folders move to its
    /// parent. Needs `control`. Result: `Empty`.
    #[serde(rename = "folder/delete")]
    FolderDelete(DeleteParams),
    /// One vehicle by id (or an earlier alias). Needs `read`. Result: `Vehicle`.
    #[serde(rename = "vehicle/get")]
    VehicleGet(IdParams),
    /// Keeps a vehicle, optionally naming it. Needs `config`. Result: `Vehicle`.
    #[serde(rename = "vehicle/save")]
    VehicleSave(VehicleSaveParams),
    /// Puts labels on and takes them off several vehicles at once (S106);
    /// renames a label as both, and removes one everywhere as `remove` on
    /// every vehicle carrying it. Labels mean nothing to UXDuck. Needs
    /// `config`. Result: `VehicleListResult`, the vehicles changed.
    #[serde(rename = "vehicle/label")]
    VehicleLabel(VehicleLabelParams),
    /// A vehicle's length, or wingspan, for the 3D map, or back to the
    /// size of the one its autopilot's simulator flies. Doesn't save it.
    /// Needs `config`. Result: `Vehicle`.
    #[serde(rename = "vehicle/size")]
    VehicleSize(VehicleSizeParams),
    /// Whether the traffic a vehicle's ADS-B receiver reports is used
    /// (S168, ADR-0034): off until a person turns it on. Operators only.
    /// Result: `Vehicle`.
    #[serde(rename = "vehicle/traffic")]
    VehicleTraffic(crate::dto::traffic::VehicleTrafficParams),
    /// Draws a vehicle as another of UXDuck's models on both maps (plane,
    /// vtol, multicopter, rover, boat, submarine, airship, jet), such as a
    /// copter drawn as a boat; without `look`, as its type gives. Kept on
    /// this computer. Needs `config`. Result: `Vehicle`.
    #[serde(rename = "vehicle/look")]
    VehicleLook(VehicleLookParams),
    /// A vehicle's web pages, such as its radio's settings page, all of
    /// them in order; an empty list removes them. Doesn't save it. Needs
    /// `config`. Result: `Vehicle`.
    #[serde(rename = "vehicle/bookmarks")]
    VehicleBookmarks(VehicleBookmarksParams),
    /// Hides a vehicle until it is saved or the daemon restarts. Needs
    /// `config`. Result: `Vehicle`.
    #[serde(rename = "vehicle/dismiss")]
    VehicleDismiss(IdParams),
    /// Adds a link and opens it. Needs `config`. Result: `Link`.
    #[serde(rename = "link/add")]
    LinkAdd(LinkAddParams),
    /// Closes and removes a link; its vehicles' live records go. Needs `config`.
    /// Result: `Empty`.
    #[serde(rename = "link/remove")]
    LinkRemove(IdParams),
    /// The latest telemetry of every vehicle; `telemetry/update` follows at
    /// up to 10 Hz per vehicle. Needs `read`. Result: `TelemetrySubscribeResult`.
    #[serde(rename = "telemetry/subscribe")]
    TelemetrySubscribe(Empty),
    /// One vehicle's latest telemetry: its position estimates (with the
    /// altitude frame of each altitude) and values such as battery, attitude
    /// and GPS, in SI units. Needs `read`. Result: `VehicleTelemetry`.
    #[serde(rename = "telemetry/get")]
    TelemetryGet(IdParams),
    /// Turns a link on again after `link/disable` or loop detection turned
    /// it off. Needs `config`. Result: `Link`.
    #[serde(rename = "link/enable")]
    LinkEnable(IdParams),
    /// Turns a link off without removing it: closed, its vehicles offline,
    /// kept with its settings until `link/enable`. Not a simulated
    /// vehicle's, which `sim/stop` ends. Needs `config`. Result: `Link`.
    #[serde(rename = "link/disable")]
    LinkDisable(IdParams),
    /// Changes a link in place, keeping its id and its vehicles: its name,
    /// where it listens or connects, its policy, topology or vehicle
    /// filter, or whether it keeps UXDuck running. A changed kind, policy,
    /// topology or filter reopens it. Needs `config`. Result: `Link`.
    #[serde(rename = "link/update")]
    LinkUpdate(LinkUpdateParams),
    /// The serial ports on this machine, with USB identities where known.
    /// Needs `config`. Result: `SerialListResult`.
    #[serde(rename = "serial/list")]
    SerialList(Empty),
    /// Sends a command to one vehicle and answers when the vehicle has
    /// acknowledged it or every retry has timed out, with the receipt. While
    /// the vehicle is not online, `offlineBehaviour` decides (see
    /// `command/list`). Needs `control`. Result: `Receipt`.
    #[serde(rename = "command/send")]
    CommandSend(CommandSendParams),
    /// The commands a vehicle offers, with their offline behaviour, for
    /// buttons. Needs `read`. Result: `CommandListResult`.
    #[serde(rename = "command/list")]
    CommandList(IdParams),
    /// The modes every family of vehicle can be put in, with their words,
    /// for a controller's mapping, which may drive any vehicle. Needs
    /// `read`. Result: `ModeListResult`.
    #[serde(rename = "command/modes")]
    CommandModes(ModeListParams),
    /// Marks a notification seen: off the screen, read in the tray. Needs
    /// `read`. Result: `Empty`.
    #[serde(rename = "notification/acknowledge")]
    NotificationAcknowledge(IdParams),
    /// Removes a notification from the tray everywhere. Needs `read`.
    /// Result: `Empty`.
    #[serde(rename = "notification/dismiss")]
    NotificationDismiss(IdParams),
    /// Every client identity the daemon has admitted, with its open
    /// connections. Operators only. Result: `ClientListResult`.
    #[serde(rename = "client/list")]
    ClientList(Empty),
    /// Announces a UI that can answer a display-settings pull. Needs `read`. Result: `Empty`.
    #[serde(rename = "client/displayRegister")]
    ClientDisplayRegister(DisplayRegisterParams),
    /// Other connected UIs offering display settings. Needs `read`. Result: `DisplayClientsResult`.
    #[serde(rename = "client/displayList")]
    ClientDisplayList(Empty),
    /// Pulls a live snapshot from one UI; never changes that UI. Needs `read`. Result: `DisplaySnapshot`.
    #[serde(rename = "client/displayPull")]
    ClientDisplayPull(DisplayPullParams),
    /// Answers a pending pull, only from its selected source. Needs `read`. Result: `Empty`.
    #[serde(rename = "client/displayReply")]
    ClientDisplayReply(DisplayReplyParams),
    /// Replaces a non-operator client's capabilities, live. Operators only.
    /// Result: `ClientRecord`.
    #[serde(rename = "client/setCapabilities")]
    ClientSetCapabilities(ClientSetCapabilitiesParams),
    /// Unpairs a phone or tablet paired on the network (S97); its open page
    /// is disconnected and must pair again. Operators only. Result: `Empty`.
    #[serde(rename = "client/forget")]
    ClientForget(ClientForgetParams),
    /// Approvals still pending. Needs `approve`. Result: `ApprovalListResult`.
    #[serde(rename = "approval/list")]
    ApprovalList(Empty),
    /// Allows or denies a pending approval; the first answer wins. Needs
    /// `approve`; a client cannot answer its own. Result: `Approval`.
    #[serde(rename = "approval/resolve")]
    ApprovalResolve(ApprovalResolveParams),
    /// Downloads a vehicle's parameters again. They also download on their
    /// own when a vehicle is first seen and after it reboots. Needs `read`.
    /// Result: `ParameterSet`, downloading.
    #[serde(rename = "param/refresh")]
    ParamRefresh(IdParams),
    /// Sets a parameter and answers when the vehicle reports it back, or
    /// every retry has timed out. A compare-and-swap on `expectedValue`.
    /// Include the reason for a change: UXDuck records the value change,
    /// author and reason automatically, so do not add a duplicate history
    /// note. Needs `config`. Result: `ParamSetResult`.
    #[serde(rename = "param/set")]
    ParamSet(ParamSetParams),
    /// What a vehicle's parameters mean, from the definition set chosen for
    /// its firmware and version (ADR-0021): only those it reports, in the
    /// variant for its frame. Needs `read`. Result: `ParamDefinitionsResult`.
    #[serde(rename = "param/definitions")]
    ParamDefinitions(ParamDefinitionsParams),
    /// The definition sets people added, and the catalogue's state. Needs
    /// `read`. Result: `DefinitionsListResult`.
    #[serde(rename = "param/definitions/list")]
    ParamDefinitionsList(DefinitionsListParams),
    /// Adds a definition set from a file's text, for a fork's own
    /// parameters; it wins over the catalogue's for what it is bound to.
    /// Needs `config`. Result: `OwnSet`.
    #[serde(rename = "param/definitions/add")]
    ParamDefinitionsAdd(DefinitionsAddParams),
    /// Removes a definition set a person added. Needs `config`. Result:
    /// empty.
    #[serde(rename = "param/definitions/remove")]
    ParamDefinitionsRemove(IdParams),
    /// Finds a vehicle's parameters by what they mean ("battery failsafe",
    /// "return altitude") or by name (`RTL_`), ranked: an exact name, a name
    /// prefix, then words in the display name, then in the description;
    /// ArduPilot's abbreviations (RTL, FS, BATT) match their words. Brief
    /// results with current values, paged. `describedBy` names the set
    /// the descriptions are from: say its version, and say when it is
    /// approximate or missing. Use `param/get` for a parameter's full
    /// definition before advising on it. Needs `read`. Result:
    /// `ParamSearchResult`.
    #[serde(rename = "param/search")]
    ParamSearch(ParamSearchParams),
    /// Parameters by name, each with its value and its whole definition:
    /// description, units, range, values, bits, whether it needs a reboot.
    /// A parameter without a definition is not described by the vehicle's
    /// set: say so and do not guess what it does. Needs `read`. Result:
    /// `ParamGetResult`.
    #[serde(rename = "param/get")]
    ParamGet(ParamGetParams),
    /// A vehicle's parameters in a group (`BATT_`), paged. Without a group,
    /// when there are many, how many each group has instead: ask again for
    /// the group you need. Needs `read`. Result: `ParamListResult`.
    #[serde(rename = "param/list")]
    ParamList(ParamListParams),
    /// How a vehicle's parameters differ from a `.param` file's, another
    /// vehicle's or a snapshot's, with units. Nothing is written. Needs
    /// `read`. Result: `ParamDiffResult`.
    #[serde(rename = "param/diff")]
    ParamDiff(ParamDiffParams),
    /// Pins the vehicle's parameters now as a named baseline (C7), with who
    /// set it and a note; it syncs to the person's other computers. Needs
    /// `config`. Result: `ParamBaseline`.
    #[serde(rename = "param/baselineSet")]
    ParamBaselineSet(crate::dto::param_definitions::ParamBaselineSetParams),
    /// A vehicle's baselines, newest first, each with how many parameters
    /// differ from it now. Needs `read`. Result: `ParamBaselinesResult`.
    #[serde(rename = "param/baselines")]
    ParamBaselines(crate::dto::param_definitions::ParamBaselinesParams),
    /// Removes a baseline. Needs `config`. Result: `{}`.
    #[serde(rename = "param/baselineRemove")]
    ParamBaselineRemove(IdParams),
    /// This computer's flights (C8), newest first: arm to disarm, with time
    /// in the air, highest point, top speed, modes, battery and who acted
    /// on the vehicle. Needs `read`. Result: `FlightListResult`.
    #[serde(rename = "flight/list")]
    FlightList(crate::dto::flight::FlightListParams),
    /// Saves a flight to the account, so it is in the logbook on every
    /// computer, or keeps it on this computer only (C9). Flight data is off
    /// by default. Needs `config`. Result: `Flight`.
    #[serde(rename = "flight/keep")]
    FlightKeep(crate::dto::flight::FlightKeepParams),
    /// A vehicle's history, newest first: notes people wrote about what
    /// they did to it (repairs, changes, tests) and what UXDuck recorded
    /// (first seen, renamed, firmware changed, snapshots). Filter by kind,
    /// category, text or time. Read it before diagnosing a vehicle or
    /// advising a change, and cite the entry and its date. Read it before
    /// proposing a note too, so notes do not duplicate existing notes or
    /// automatic events. Needs `read`.
    /// Result: `VehicleHistoryResult`.
    #[serde(rename = "vehicle/history")]
    VehicleHistory(VehicleHistoryParams),
    /// A vehicle's parameter changes, newest first, with who or what made
    /// each and why (S74): a person, the agent, another ground station, the
    /// vehicle, or a download. Needs `read`. Result: `ParamHistoryResult`.
    #[serde(rename = "param/history")]
    ParamHistory(ParamHistoryParams),
    /// Writes a note in a vehicle's history, dated now or earlier, for
    /// completed physical maintenance or useful context from completed agent
    /// changes. Read `vehicle_history` first; do not duplicate existing notes
    /// or automatic events. Parameter writes already record their reason.
    /// Batch related work into one concise proposed note and write only
    /// after the person agrees; an explicit request to record it already
    /// counts. Configuration permission alone is not note consent. Do not
    /// interrupt active flying or an unfinished task with an offer, or repeat
    /// a declined offer. Attribute user reports and preserve supplied dates
    /// and details; never invent facts or record proposed work as completed.
    /// Needs `config`; the daemon's capability approval still applies.
    /// Result: `HistoryEntry`.
    #[serde(rename = "vehicle/addNote")]
    VehicleAddNote(AddNoteParams),
    /// Edits a note, keeping the earlier version. Events cannot be edited.
    /// Needs `config`. Result: `HistoryEntry`.
    #[serde(rename = "vehicle/editNote")]
    VehicleEditNote(EditNoteParams),
    /// Deletes a note; it stays in the history marked as deleted, by whom
    /// and when. Needs `config`. Result: `HistoryEntry`.
    #[serde(rename = "vehicle/deleteNote")]
    VehicleDeleteNote(IdParams),
    /// Downloads a vehicle's mission again. Needs `read`. Result: `Mission`,
    /// downloading.
    #[serde(rename = "mission/refresh")]
    MissionRefresh(IdParams),
    /// A vehicle's missions over time (S134): each different mission it
    /// has held, newest first, with how it came and who. `id` is the
    /// vehicle. Needs `read`. Result: `MissionHistoryResult`.
    #[serde(rename = "mission/history")]
    MissionHistory(IdParams),
    /// Puts a mission from a vehicle's history back (S134), with its fence
    /// and rally points, and answers once it is read back. Replaces what
    /// the vehicle holds. Needs `control`. Result: `MissionRestoreResult`.
    #[serde(rename = "mission/restore")]
    MissionRestore(crate::dto::mission::MissionRestoreParams),
    /// Uploads a mission and answers when the vehicle has accepted or
    /// refused it. Refused with `version-conflict` if the mission changed
    /// since `expectedVersion`. Needs `control`. Result:
    /// `MissionUploadResult`.
    #[serde(rename = "mission/upload")]
    MissionUpload(MissionUploadParams),
    /// Claims a controller for this client (operators only, needs
    /// `control`). The sticks must be centred, and no other controller may
    /// drive the vehicle. Result: `Controller`.
    #[serde(rename = "controller/claim")]
    ControllerClaim(ControllerClaimParams),
    /// Starts a pinned, simulation-only keyboard controller. Operator with control;
    /// existing controller claim and neutral rules apply. Result: `Controller`.
    #[serde(rename = "controller/keyboardStart")]
    KeyboardStart(crate::dto::controller::KeyboardStartParams),
    /// Refreshes this connection's keyboard input. Expires after 500 ms without
    /// frames. Cannot start or resume a released controller. Result: `Empty`.
    #[serde(rename = "controller/keyboardInput")]
    KeyboardInput(crate::dto::controller::KeyboardInputParams),
    /// Releases this connection's keyboard controller immediately. Result: `Empty`.
    #[serde(rename = "controller/keyboardStop")]
    KeyboardStop(Empty),
    /// Releases a controller: it stops driving at once, and the vehicle's
    /// own failsafe decides what happens. Operators only. Result: `Empty`.
    #[serde(rename = "controller/release")]
    ControllerRelease(IdParams),
    /// Moves a held controller to this window, still flying what it flew
    /// with the same mapping. Only for the operator holding it, from a
    /// window on the same computer; the holding window may be open or have
    /// closed moments ago. Result: `Controller`.
    #[serde(rename = "controller/takeOver")]
    ControllerTakeOver(IdParams),
    /// Forgets a known controller: it leaves the list until it is plugged
    /// in again. Refused while it is plugged in or held. Operators only,
    /// needs `config`. Result: `Empty`.
    #[serde(rename = "controller/forget")]
    ControllerForget(IdParams),
    /// Changes what a claimed controller drives, or confirms a pending
    /// switch. Only the claiming client; moving to another vehicle needs
    /// the sticks centred. Result: `Controller`.
    #[serde(rename = "controller/setTarget")]
    ControllerSetTarget(ControllerSetTargetParams),
    /// Every plugged-in controller's raw axes and buttons right now, before
    /// any mapping, for testing a gamepad. Changes nothing; poll it while
    /// it is shown. Needs `read`. Result: `ControllerInputsResult`.
    #[serde(rename = "controller/inputs")]
    ControllerInputs(Empty),
    /// Says which vehicle this client has selected, for its controllers
    /// that follow the selection. Needs `read`. Result: `Empty`.
    #[serde(rename = "client/select")]
    ClientSelect(ClientSelectParams),
    /// Replaces an input mapping edited from `expectedVersion`. Needs
    /// `config`. Result: `InputMapping`.
    #[serde(rename = "mapping/update")]
    MappingUpdate(MappingUpdateParams),
    /// Makes a new input mapping, a copy of `from` (Mode 2 when absent)
    /// named `name`, so a controller such as a radio has one of its own.
    /// Its id comes from the name. Needs `config`. Result: `InputMapping`.
    #[serde(rename = "mapping/create")]
    MappingCreate(MappingCreateParams),
    /// Restarts the daemon after answering: a final checkpoint, then the
    /// same program in its place. Refused with `confirmation-required`
    /// while a vehicle is armed, unless `confirm`. Operators only. Result:
    /// `Empty`.
    #[serde(rename = "daemon/restart")]
    DaemonRestart(DaemonRestartParams),
    /// Stops the daemon after answering, with a final checkpoint, whatever
    /// the close policy: the desktop shell's "quit and release ports".
    /// Operators only. Result: `Empty`.
    #[serde(rename = "daemon/stop")]
    DaemonStop(Empty),
    /// Full product update status and current restart blockers. Needs read.
    /// Also subscribes this connection to changed `update/status` notifications.
    #[serde(rename = "update/status")]
    UpdateStatus(Empty),
    /// Check and download a signed full app update. Local operators only.
    #[serde(rename = "update/check")]
    UpdateCheck(Empty),
    /// Approve this version now or when idle, optionally stopping simulations.
    /// Real-vehicle checks cannot be overridden. Local operators only.
    #[serde(rename = "update/install")]
    UpdateInstall(AppUpdateInstallParams),
    /// Cancel queued consent and silence this version's prompt. Local operators only.
    #[serde(rename = "update/later")]
    UpdateLater(Empty),
    /// What would stop if UXDuck quit now: simulations, forward links,
    /// connected vehicles and recordings, with the words for the question
    /// (S107, ADR-0025 D1). Changes nothing. Operators only. Result:
    /// `StopPreview`.
    #[serde(rename = "daemon/stopPreview")]
    DaemonStopPreview(Empty),
    /// Whether this page is on screen, sent on connecting and on each
    /// `visibilitychange` (S107): `app/focus` prefers a page that is seen.
    /// Any client. Result: `Empty`.
    #[serde(rename = "client/visibility")]
    ClientVisibility(ClientVisibilityParams),
    /// Asks one connected page, a seen one first, else the one seen last,
    /// to come forward (`app/focus` to it), so that opening UXDuck again
    /// opens nothing new (S107, ADR-0025 D1). Operators only. Result:
    /// `AppFocusResult`.
    #[serde(rename = "app/focus")]
    AppFocus(Empty),
    /// Opens UXDuck as chosen (S110): the desktop app, started by UXDuck,
    /// or brought forward; else a page already open comes forward; else
    /// the answer has the address to open, already paired. Operators only.
    /// Result: `AppOpenResult`.
    #[serde(rename = "app/open")]
    AppOpen(AppOpenParams),
    /// Where the desktop app stands: offered, its sizes, downloading,
    /// installed, chosen (S110, ADR-0025 D6). Needs `read`. Result:
    /// `DesktopAppStatus`.
    #[serde(rename = "desktopApp/status")]
    DesktopAppStatus(DesktopAppStatusParams),
    /// This computer's addresses on its networks, and where phones and
    /// tablets open UXDuck while "Phones and tablets on this network" is on
    /// (S97). Operators only. Result: `NetworkStatus`.
    #[serde(rename = "network/status")]
    NetworkStatus(Empty),
    /// Downloads the desktop app the release names for this computer,
    /// checks it and unpacks it; `desktopApp/status` shows how it goes.
    /// Nothing is downloaded before this is asked. Needs `config`. Result:
    /// `Empty`.
    #[serde(rename = "desktopApp/download")]
    DesktopAppDownload(Empty),
    /// Opens UXDuck in the desktop app from now on, or in a browser. The
    /// desktop app must be installed to choose it. Needs `config`. Result:
    /// `DesktopAppStatus`.
    #[serde(rename = "desktopApp/choose")]
    DesktopAppChoose(DesktopAppChooseParams),
    /// Deletes the desktop app; UXDuck opens in a browser again. Needs
    /// `config`. Result: `DesktopAppStatus`.
    #[serde(rename = "desktopApp/remove")]
    DesktopAppRemove(Empty),
    /// Searches threads, folders, vehicles, parameters, missions, live
    /// telemetry, settings and saved places, best match first. Needs
    /// `read`. Result: `SearchResult`.
    #[serde(rename = "search/query")]
    SearchQuery(SearchQueryParams),
    /// Searches global towns, landmarks and addresses for a simulation's
    /// starting point or other map use. Returns names, region details and
    /// keys; choose the requested place, then use `place_locate` to obtain
    /// coordinates. Ask when the place is ambiguous. Uses the hosted API,
    /// respects the operator's place-search setting and needs `read`.
    /// Result: `PlaceSearchResult`.
    #[serde(rename = "place/search")]
    PlaceSearch(PlaceSearchParams),
    /// Locates the chosen `place_search` result using its key, text and
    /// original near hint. Returns latitudeDeg and longitudeDeg; read
    /// `terrain_at` for altitudeM before passing them to `sim_start`.
    /// Starts nothing and saves nothing. Respects the operator's place-search
    /// setting and needs `read`. Result: `PlaceLocateResult`.
    #[serde(rename = "place/locate")]
    PlaceLocate(PlaceLocateParams),
    /// Saves a place on the connection's operator's list. Needs `config`.
    /// Result: `SavedPlace`.
    #[serde(rename = "place/save")]
    PlaceSave(PlaceSaveParams),
    /// Renames a saved place from `expectedVersion`. Needs `config`.
    /// Result: `SavedPlace`.
    #[serde(rename = "place/update")]
    PlaceUpdate(PlaceUpdateParams),
    /// Removes a saved place. Needs `config`. Result: `Empty`.
    #[serde(rename = "place/remove")]
    PlaceRemove(IdParams),
    /// Every marker on this computer (ADR-0035): named things on the map
    /// besides vehicles, such as a launch site, a ground station or a boat,
    /// with their position and heading. Changes nothing. Needs `read`.
    /// Result: `MarkerListResult`.
    #[serde(rename = "marker/list")]
    MarkerList(Empty),
    /// Adds a marker at a latitude and longitude, seen in every window.
    /// Needs `config`. Result: `Marker`.
    #[serde(rename = "marker/add")]
    MarkerAdd(MarkerAddParams),
    /// Renames, moves, turns or restyles a marker from `expectedVersion`.
    /// Needs `config`. Result: `Marker`.
    #[serde(rename = "marker/update")]
    MarkerUpdate(MarkerUpdateParams),
    /// Removes a marker. Needs `config`. Result: `Empty`.
    #[serde(rename = "marker/remove")]
    MarkerRemove(IdParams),
    /// This window's device position for a marker whose position comes
    /// from a device, about once a second while it shares; refused while
    /// another device shares it. Needs `config`. Result: `Empty`.
    #[serde(rename = "marker/share")]
    MarkerShare(MarkerShareParams),
    /// Stops this window sharing a marker's position. Needs `config`.
    /// Result: `Empty`.
    #[serde(rename = "marker/unshare")]
    MarkerUnshare(IdParams),
    /// Lists saved plans (ADR-0029): missions kept in UXDuck for a vehicle
    /// type, with a fence and rally points, that reach a vehicle only when
    /// uploaded. Needs `read`. Result: `PlanListResult`.
    #[serde(rename = "plan/list")]
    PlanList(PlanListParams),
    /// One plan with its items, fence and rally points. Needs `read`.
    /// Result: `Plan`.
    #[serde(rename = "plan/get")]
    PlanGet(IdParams),
    /// Makes a plan at once ("Untitled plan" unless named), for the given
    /// vehicle type. It is not uploaded anywhere. Needs `config`. Result:
    /// `Plan`.
    #[serde(rename = "plan/create")]
    PlanCreate(PlanCreateParams),
    /// Replaces a plan's body as edited from `expectedVersion`; refused
    /// with `version-conflict` if it changed meanwhile. Each update can be
    /// undone. Needs `config`. Result: `Plan`.
    #[serde(rename = "plan/update")]
    PlanUpdate(PlanUpdateParams),
    /// Undoes a plan's last change, or every change back to `toVersion`.
    /// Needs `config`. Result: `Plan`.
    #[serde(rename = "plan/undo")]
    PlanUndo(PlanStepParams),
    /// Redoes a plan's last undone change. Needs `config`. Result: `Plan`.
    #[serde(rename = "plan/redo")]
    PlanRedo(PlanStepParams),
    /// Records the nearest place's name for a plan. Needs `config`.
    /// Result: `Plan`.
    #[serde(rename = "plan/place")]
    PlanPlace(PlanPlaceParams),
    /// Moves a plan to Deleted, kept 30 days. Needs `config`. Result:
    /// `Plan`.
    #[serde(rename = "plan/delete")]
    PlanDelete(IdParams),
    /// Brings a plan back from Deleted. Needs `config`. Result: `Plan`.
    #[serde(rename = "plan/restore")]
    PlanRestore(IdParams),
    /// Reads files into one new plan: QGroundControl `.plan`, Mission
    /// Planner's waypoint, fence and rally files, ArduPilot's fence text,
    /// KML. What could not be read is listed. Needs `config`. Result:
    /// `PlanImportResult`.
    #[serde(rename = "plan/import")]
    PlanImport(PlanImportParams),
    /// A plan as files: one QGroundControl `.plan`, or Mission Planner's
    /// waypoint, fence and rally files. Needs `read`. Result:
    /// `PlanExportResult`.
    #[serde(rename = "plan/export")]
    PlanExport(PlanExportParams),
    /// Uploads a plan to a vehicle as its mission (replacing what it
    /// holds, which is kept in its history), reads it back and compares.
    /// Answers when verified, refused, cancelled or failed. Needs
    /// `control`. Result: `PlanUploadResult`.
    #[serde(rename = "plan/upload")]
    PlanUpload(PlanUploadParams),
    /// A plan's problems: errors stop an upload, warnings do not. Heights
    /// over the ground use the terrain the named vehicle would use; ground
    /// not fetched yet is fetched and said to be missing. Needs `read`.
    /// Result: `PlanCheckResult`.
    #[serde(rename = "plan/check")]
    PlanCheck(crate::dto::plan::PlanCheckParams),
    /// The example plans, one per vehicle type, placed at `at` and turned
    /// to `headingDeg` (ADR-0029 P12). Not in the library: `plan/create`
    /// with one's body makes a plan of it. Needs `read`. Result:
    /// `PlanExamplesResult`.
    #[serde(rename = "plan/examples")]
    PlanExamples(crate::dto::plan::PlanExamplesParams),
    /// Stops a parameter download or a mission transfer on a vehicle.
    /// Needs `control`. Result: `Empty`.
    #[serde(rename = "transfer/cancel")]
    TransferCancel(crate::dto::mission::TransferCancelParams),
    /// Changes the daemon's settings, edited from `expectedVersion`. Needs
    /// `config`. Result: `Settings`.
    #[serde(rename = "settings/update")]
    SettingsUpdate(SettingsUpdateParams),
    /// Disk space by category, and what is free. Needs `read`. Result:
    /// `StorageUsage`.
    #[serde(rename = "storage/usage")]
    StorageUsage(Empty),
    /// Clears logs of stopped native simulations, preserving builds and state.
    /// Needs `config`. Result: `StorageUsage`.
    #[serde(rename = "storage/clearSimulatorLogs")]
    StorageClearSimulatorLogs(Empty),
    /// Shows UXDuck's app folder (`~/.uxduck` on a Mac) in the computer's
    /// file manager (S150). Only on the computer UXDuck runs on. Needs
    /// `read`. Result: `VideoRevealResult`.
    #[serde(rename = "storage/reveal")]
    StorageReveal(Empty),
    /// How often each of the daemon's tasks has woken since it started.
    /// Needs `read`. Result: `Wakeups`.
    #[serde(rename = "daemon/wakeups")]
    DaemonWakeups(Empty),
    /// Which UXDuck this is and how it was built: version, commit, build
    /// time, target, data directory, port and uptime (S87). Changes
    /// nothing. Needs `read`. Result: `DaemonAbout`.
    #[serde(rename = "daemon/about")]
    DaemonAbout(Empty),
    /// Which interface UXDuck serves to a page loaded now, and how its
    /// updates are going (S109). Changes nothing. Needs `read`. Result:
    /// `InterfaceStatus`.
    #[serde(rename = "interface/status")]
    InterfaceStatus(Empty),
    /// Checks for a newer interface now, as UXDuck does every hour; a page
    /// sends it when it regains focus after more than an hour. What the
    /// check finds arrives as `interface/updated`. Needs `read`. Result:
    /// `InterfaceStatus`.
    #[serde(rename = "interface/check")]
    InterfaceCheck(Empty),
    /// The latest message of each MAVLink type heard from a vehicle and
    /// sent to it, with its rate and fields, for the inspector. Changes
    /// nothing. Needs `read`. Result: `MavlinkInspectResult`.
    #[serde(rename = "mavlink/inspect")]
    MavlinkInspect(MavlinkInspectParams),
    /// Read-only recorded packet evidence. Use linkId (including a forward),
    /// fromUs/toUs, allMessages and includeRaw to inspect sequence, exact-wire
    /// fingerprints, peer provenance and coverage. Historical tlogs have no
    /// peer field. Detailed queries default to the last minute, at most ten
    /// minutes and 200 records; id defaults to 0 (HEARTBEAT). Existing vehicle
    /// message pages remain available. Needs `read`. Result:
    /// `MavlinkHistoryResult`.
    #[serde(rename = "mavlink/history")]
    MavlinkHistory(MavlinkHistoryParams),
    /// Read-only router decisions and preserved loop incidents. Query by link
    /// and time; includes triggering/matched packet provenance, threshold and
    /// coverage. Exact bytes are opt-in. Result: `RouterEventsResult`.
    #[serde(rename = "router/events")]
    RouterEvents(RouterEventsParams),
    /// Resolve an exact link/vehicle id or name into its endpoints, vehicles,
    /// forwarding relationships and recent failures. Read only. Result: `LinkDiagnoseResult`.
    #[serde(rename = "link/diagnose")]
    LinkDiagnose(LinkDiagnoseParams),
    /// Read-only local socket ownership by protocol and port; never probes a
    /// remote host or stops a process. Reports unavailable/incomplete platform
    /// inspection explicitly. Result: `SocketInspectResult`.
    #[serde(rename = "socket/inspect")]
    SocketInspect(SocketInspectParams),
    /// Read-only daemon logs by time, severity, component or literal text.
    /// Bounded pages with coverage; output is redacted. Result: `DaemonLogsResult`.
    #[serde(rename = "daemon/logs")]
    DaemonLogs(DaemonLogsParams),
    /// Installs a plugin from a directory with `plugin.json`, granting the
    /// capabilities its manifest asks for: installing is the operator's
    /// confirmation. An agent's install, such as a draft's from
    /// `plugin/draft`, always waits for an operator, who sees what it may do
    /// and the chat it was written in. Result: `Plugin`, starting.
    #[serde(rename = "plugin/install")]
    PluginInstall(PluginInstallParams),
    /// Stops and removes a plugin. Operators only. Result: `Empty`.
    #[serde(rename = "plugin/remove")]
    PluginRemove(IdParams),
    /// Runs one of an installed plugin's commands on a vehicle (as
    /// `plugin_call` for agents): find it first with `tool/search`, which
    /// gives its `pluginId`, `command` and the schema `params` must meet.
    /// Needs `control`; an agent without it waits for an operator. The call
    /// is journaled under the caller; what the plugin does next, under the
    /// plugin. Result: `Empty`.
    #[serde(rename = "plugin/command")]
    PluginCommand(PluginCommandParams),
    /// Writes a TypeScript plugin into UXDuck's drafts, under the chat it
    /// is written in (S58, ADR-0006 X12). Writing the same id again makes
    /// a new version. Test it with `plugin/test`; install it with
    /// `plugin/install` and the path this returns, which for an agent always
    /// asks an operator. Result: `PluginDraft`.
    #[serde(rename = "plugin/draft")]
    PluginDraft(PluginDraftParams),
    /// Runs a draft as the daemon would, against a vehicle's last five
    /// minutes, and says what it did: calls, items with their last values,
    /// notifications, log lines and any error. Changes nothing. Result:
    /// `PluginTestResult`.
    #[serde(rename = "plugin/test")]
    PluginTest(PluginTestParams),
    /// Drafts, newest first, each with the chat it came from. Result:
    /// `PluginDraftsResult`.
    #[serde(rename = "plugin/drafts")]
    PluginDrafts(Empty),
    /// Grants a synced plugin what waits for an operator on this computer
    /// (S59, ADR-0006 X13) and restarts it. Operators only. Result: `Plugin`.
    #[serde(rename = "plugin/allow")]
    PluginAllow(IdParams),
    /// Makes a link to an installed plugin that any signed-in account may
    /// install from, each with its own prompt. Operators only, signed in.
    /// Result: `PluginShare`.
    #[serde(rename = "plugin/share")]
    PluginShare(PluginShareParams),
    /// The account's live links. Result: `PluginSharesResult`.
    #[serde(rename = "plugin/shares")]
    PluginShares(Empty),
    /// Stops a link; computers that installed from it keep their copy.
    /// Operators only. Result: `Empty`.
    #[serde(rename = "plugin/unshare")]
    PluginUnshare(PluginTokenParams),
    /// What a link offers and who shared it, before installing. Result:
    /// `PluginShare`.
    #[serde(rename = "plugin/shared")]
    PluginShared(PluginTokenParams),
    /// Installs from a link, with everything its manifest asks for: the
    /// operator's confirmation is the request. Operators only. Result:
    /// `Plugin`.
    #[serde(rename = "plugin/installShared")]
    PluginInstallShared(PluginTokenParams),
    /// The tray's menu (S64, ADR-0008): the status line, the icon's tone
    /// and the items, built from UXDuck's state. Changes follow as
    /// `tray/menuChanged` to the connection that asked. Operators only.
    /// Result: `TrayMenu`.
    #[serde(rename = "tray/menu")]
    TrayMenu(Empty),
    /// An item of the tray's menu was chosen. Operators only. Result:
    /// `TrayAction`, what the tray does next.
    #[serde(rename = "tray/activate")]
    TrayActivate(TrayActivateParams),
    /// The screen the tray captured for "Send feedback…", once. Operators
    /// only. Result: `FeedbackScreenshot`.
    #[serde(rename = "feedback/screenshot")]
    FeedbackScreenshot(FeedbackScreenshotParams),
    /// What the system said about this computer's location (S137,
    /// ADR-0030 L2), from the tray, which asks it as UXDuck. Operators
    /// only. Result: `Empty`.
    #[serde(rename = "tray/location")]
    TrayLocation(TrayLocationParams),
    /// Where this computer is, as the system told UXDuck (S137, ADR-0030):
    /// off until a person turns it on, then the latest fix. Changes follow
    /// as `location/changed` to the connection that asked. Changes
    /// nothing. Needs `location`: an agent without it asks the person
    /// first, who may allow it once or always. Result: `LocationStatus`.
    #[serde(rename = "location/status")]
    LocationStatus(Empty),
    /// Turns this computer's location on: the system asks the person the
    /// first time, then fixes follow as `location/changed`. Operators only;
    /// an agent cannot turn it on. Result: `LocationStatus`.
    #[serde(rename = "location/start")]
    LocationStart(Empty),
    /// Turns this computer's location off and forgets the fix. Operators
    /// only. Result: `LocationStatus`.
    #[serde(rename = "location/stop")]
    LocationStop(Empty),
    /// Other aircraft near the vehicles (S168, ADR-0034), merged from the
    /// sources that are on, and every source, on or off. Changes follow as
    /// `traffic/changed` to the connection that asked. Needs `read`.
    /// Result: `TrafficStatus`.
    #[serde(rename = "traffic/status")]
    TrafficStatus(Empty),
    /// A vehicle's own messages (S26): every one, oldest first, a run of
    /// the same message with nothing between folded into one entry with a
    /// count; or with `raw` each as it came. With `subscribe`,
    /// `vehicle/message` follows. Needs `read`. Result:
    /// `VehicleMessagesResult`.
    #[serde(rename = "vehicle/messages")]
    VehicleMessages(VehicleMessagesParams),
    /// Shows a tab in the chat's operator's layout, as "+" does (S43): how
    /// the agent opens one when asked. Needs `read`: it changes only what
    /// shows. Result: `LayoutOpenResult`.
    #[serde(rename = "layout/open")]
    LayoutOpen(LayoutOpenParams),
    /// Finds installed plugins' commands by words, each with its parameter
    /// schema, for `plugin/command` (S57, ADR-0006 X11). Result:
    /// `ToolSearchResult`.
    #[serde(rename = "tool/search")]
    ToolSearch(ToolSearchParams),
    /// Every plugin's map features, one layer per plugin (S55). Changes
    /// follow as `plugin/featuresChanged` to the connections that asked.
    /// Result: `PluginFeaturesResult`.
    #[serde(rename = "plugin/features")]
    PluginFeatures(Empty),
    /// Every running plugin's own items, those with no vehicle, such as a
    /// stopwatch's time (S101). Changes follow as `plugin/itemsChanged` to
    /// the connections that asked. Result: `PluginItemsResult`.
    #[serde(rename = "plugin/items")]
    PluginItems(Empty),
    /// Turns a plugin off or on without removing it (S104); it stays as
    /// left. Operators only. Result: `Plugin`.
    #[serde(rename = "plugin/setEnabled")]
    PluginSetEnabled(crate::dto::plugin::PluginSetEnabledParams),
    /// What each running plugin has used in the last minute (S104), for
    /// the Plugins page; asked again to refresh. Result: `PluginUsageResult`.
    #[serde(rename = "plugin/usage")]
    PluginUsage(Empty),
    /// One product analytics event from the app, sent from the daemon's
    /// outbox when usage analytics are on and dropped when they are off
    /// (ADR-0016 O1). Operators only. Result: `Empty`.
    #[serde(rename = "analytics/track")]
    AnalyticsTrack(AppEvent),
    /// Feedback funnel events, through an optional method so older daemons
    /// can answer method-not-found. Same usage switch. Result: `Empty`.
    #[serde(rename = "analytics/feedback")]
    AnalyticsFeedback(AppEvent),
    /// Foreground active-time delta. Optional for older daemons; probe by
    /// method-not-found. Same analytics switch as analytics/track. Result: `Empty`.
    #[serde(rename = "analytics/activeTime")]
    AnalyticsActiveTime(AnalyticsActiveTimeParams),
    /// Consumes coarse browser-daemon flight-time totals once across tabs.
    /// Installed daemons send them directly. Operators only. Result: `FlightTimeUsage`.
    #[serde(rename = "analytics/takeFlightTime")]
    AnalyticsTakeFlightTime(Empty),
    /// Atomically consumes confirmed browser-daemon task outcomes. Optional
    /// for older daemons. Operators only. Result: `EngagementUsage`.
    #[serde(rename = "analytics/takeEngagement")]
    AnalyticsTakeEngagement(Empty),
    /// Optional aggregate performance report. Result: `Empty`.
    #[serde(rename = "analytics/performance")]
    AnalyticsPerformance(AnalyticsPerformanceParams),
    /// Joins a browser's earlier events to this install (ADR-0016 O4), when
    /// usage analytics are on. Operators only. Result: `Empty`.
    #[serde(rename = "analytics/alias")]
    AnalyticsAlias(AnalyticsAliasParams),
    /// Sends a feedback report with UXDuck's part of the diagnostics bundle,
    /// scrubbed, to the team's Linear; kept while offline and sent later
    /// (ADR-0016 O8 to O11). Operators only. Result: `FeedbackSendResult`.
    #[serde(rename = "feedback/send")]
    FeedbackSend(Box<FeedbackSendParams>),
    /// A suggestion card for the chat (S73): what someone asked for that
    /// UXDuck cannot do, for them to read, change and send themselves with
    /// `feedback/send`. Sends nothing. Needs `read`. Result: `SuggestionCard`.
    #[serde(rename = "feedback/suggest")]
    FeedbackSuggest(FeedbackSuggestParams),
    /// Development builds only: the daemon panics once it has answered, to
    /// test error reporting (`uxduck debug panic`, S32). Operators only.
    /// Result: `Empty`.
    #[serde(rename = "debug/panic")]
    DebugPanic(Empty),
    /// Takes in a browser's work (ADR-0011, S20): its operators, chats,
    /// folders and saved places, each only if it is new here or newer.
    /// Installed daemons only; operators only. Result: `JournalImportResult`.
    #[serde(rename = "journal/import")]
    JournalImport(JournalImportParams),
    /// Starts a simulated vehicle at a place: a multicopter, plane or VTOL,
    /// with an ArduPilot frame from `sim/binaries`. It appears as a vehicle
    /// on a link of its own. Configuration capability; an agent asks first.
    /// Result: `Link`.
    #[serde(rename = "sim/start")]
    SimStart(SimStartParams),
    /// Stops a simulated vehicle and removes its link. Configuration
    /// capability; an agent asks first. Result: `Empty`.
    #[serde(rename = "sim/stop")]
    SimStop(IdParams),
    /// Starts a simulated vehicle again from where it started, after a
    /// crash or to begin afresh: the same link, place, height and speed, so
    /// it keeps its name, history and tabs. Configuration capability; an
    /// agent asks first. Result: `Link`.
    #[serde(rename = "sim/restart")]
    SimRestart(IdParams),
    /// Pauses a simulated vehicle where it is, or resumes it: no time
    /// passes for it while paused, so it neither moves nor counts down a
    /// failsafe, and commands to it are refused until it resumes. Real
    /// vehicles cannot be paused. Control capability. Result: `Link`.
    #[serde(rename = "sim/pause")]
    SimPause(SimPauseParams),
    /// Saves a snapshot of a simulated vehicle: where it is, its height,
    /// heading, mode, parameters and mission (S34). Configuration
    /// capability. Result: `SimSnapshot`.
    #[serde(rename = "sim/snapshot")]
    SimSnapshot(SimSnapshotParams),
    /// Starts a new simulated vehicle from a snapshot: at its place, with
    /// its parameters and mission, in the air at its height if it was
    /// flying. Configuration capability; an agent asks first. Result:
    /// `Link`.
    #[serde(rename = "snapshot/start")]
    SnapshotStart(SnapshotStartParams),
    /// The saved simulation snapshots, newest first. Result:
    /// `SnapshotListResult`.
    #[serde(rename = "snapshot/list")]
    SnapshotList(Empty),
    /// Renames a snapshot. Configuration capability. Result: `SimSnapshot`.
    #[serde(rename = "snapshot/rename")]
    SnapshotRename(SnapshotRenameParams),
    /// Deletes a snapshot. Configuration capability. Result: `Empty`.
    #[serde(rename = "snapshot/delete")]
    SnapshotDelete(IdParams),
    /// The SITL builds the daemon can run, with their frames. Result:
    /// `SimBinariesResult`.
    #[serde(rename = "sim/binaries")]
    SimBinaries(Empty),
    /// The simulator catalogue's builds for this platform (S18), and which
    /// are installed or downloading. Result: `SimCatalogueResult`.
    #[serde(rename = "sim/catalogue")]
    SimCatalogue(SimCatalogueParams),
    /// Downloads and installs a catalogue build, checked against the signed
    /// catalogue's checksums; it shows in `sim/catalogue` as it goes.
    /// Configuration capability. Result: `Empty`.
    #[serde(rename = "sim/download")]
    SimDownload(SimBuildParams),
    /// Deletes an installed catalogue build. Configuration capability.
    /// Result: `Empty`.
    #[serde(rename = "sim/delete")]
    SimDelete(SimBuildParams),
    /// Whether this UXDuck is signed in to a UXDuck account, and as whom
    /// (ADR-0013 P9). Result: `AccountState`.
    #[serde(rename = "account/get")]
    AccountGet(Empty),
    /// Recorded telemetry logs in this computer's account. Needs `read`.
    /// Result: `AccountFlights`. Tokens stay with the daemon.
    #[serde(rename = "account/flights")]
    AccountFlights(Empty),
    /// Every copy of one synced entity the account was sent (C5), newest
    /// first, the ones that lost too. Needs `read`. Result: `AccountVersions`.
    #[serde(rename = "account/versions")]
    AccountVersions(crate::dto::sync::AccountVersionsParams),
    /// Development daemon's server choice. Result: `AccountEnvironmentState`.
    #[serde(rename = "account/environment")]
    AccountEnvironment(Empty),
    /// Saves the next-start server choice in a development daemon. Needs config.
    /// Result: `AccountEnvironmentState`; apply with `daemon/restart`.
    #[serde(rename = "account/setEnvironment")]
    AccountSetEnvironment(AccountEnvironmentParams),
    /// Emails a one-time code to sign in with. Configuration capability.
    /// Result: `Empty`.
    #[serde(rename = "account/signIn")]
    AccountSignIn(AccountSignInParams),
    /// Signs in with the emailed code; the token stays in the daemon's
    /// credential store. Configuration capability. Result: `AccountState`.
    #[serde(rename = "account/verify")]
    AccountVerify(AccountVerifyParams),
    /// Signs out and forgets the token; everything local keeps working.
    /// Configuration capability. Result: `Empty`.
    #[serde(rename = "account/signOut")]
    AccountSignOut(Empty),
    /// Joins or leaves the product updates list. Configuration capability.
    /// Result: `AccountState`.
    #[serde(rename = "account/setUpdates")]
    AccountSetUpdates(AccountUpdatesParams),
    /// Chooses or changes the account's username, which credited
    /// suggestions show (S73). Needs `config`. Result: `AccountState`.
    #[serde(rename = "account/setUsername")]
    AccountSetUsername(AccountUsernameParams),
    /// Where the account is signed in (S114): each computer and browser,
    /// this one marked. Needs `read`. Result: `AccountSessions`.
    #[serde(rename = "account/sessions")]
    AccountSessions(Empty),
    /// Signs out one of the account's sessions; this computer's own forgets
    /// its token. Needs `config`. Result: `AccountState`.
    #[serde(rename = "account/signOutSession")]
    AccountSignOutSession(AccountSessionParams),
    /// Signs out every session of the account but this computer's. Needs
    /// `config`. Result: `AccountState`.
    #[serde(rename = "account/signOutOthers")]
    AccountSignOutOthers(Empty),
    /// Emails a link that deletes the account (ADR-0013 P23); nothing is
    /// deleted until the link's page is confirmed. Needs `config`. Result:
    /// `Empty`.
    #[serde(rename = "account/requestDeletion")]
    AccountRequestDeletion(Empty),
    /// The account's feedback reports from any computer, with what was
    /// added since and a key to add more. Needs `read`. Result:
    /// `AccountFeedbackList`.
    #[serde(rename = "account/feedback")]
    AccountFeedback(Empty),
    /// Keeps reports this browser sent signed out in the account, by their
    /// keys. Needs `config`. Result: `ClaimFeedbackResult`.
    #[serde(rename = "account/claimFeedback")]
    AccountClaimFeedback(crate::dto::feedback::ClaimFeedbackParams),
    /// Whether this computer mirrors its work to the account, how far it
    /// has got, and the vehicles other computers published (S16).
    /// Result: `SyncStatus`.
    #[serde(rename = "sync/get")]
    SyncGet(Empty),
    /// Turns sync on or off, or changes which vehicles from the account are
    /// used here. Configuration capability. Result: `SyncStatus`.
    #[serde(rename = "sync/set")]
    SyncSet(SyncSetParams),
    /// Uses a vehicle from the account on this computer. Configuration
    /// capability. Result: `SyncStatus`.
    #[serde(rename = "sync/activate")]
    SyncActivate(SyncActivateParams),
    /// Puts back an earlier version of a plan from `account/versions` (C6),
    /// as a new edit that goes to the other computers; the body it replaces
    /// can be undone back to. Configuration capability. Result: `Plan`.
    #[serde(rename = "sync/restore")]
    SyncRestore(crate::dto::sync::SyncRestoreParams),
    /// Thumbs up or down on an agent message, with an optional comment,
    /// sent with the kept turn or, with `send`, with the conversation
    /// (ADR-0013 P13). Operator clients only. Result: `ThreadFeedbackResult`.
    #[serde(rename = "thread/feedback")]
    ThreadFeedback(ThreadFeedbackParams),
    /// What this account shared to improve the agent. Result: `SharedData`.
    #[serde(rename = "account/shared")]
    AccountShared(Empty),
    /// The account's computers and this one's connection to it (S78).
    #[serde(rename = "account/computers")]
    AccountComputers(Empty),
    /// Makes a watch link for this computer or one vehicle on it (S95,
    /// ADR-0023): anyone with its address watches, read only, until it ends.
    /// Needs `config`. Result: `WatchLinkCreateResult`.
    /// The practice exercises (S100, ADR-0024), with whether the vehicle
    /// named can run each and the operator's best result. Needs `read`.
    /// Result: `PracticeListResult`.
    #[serde(rename = "practice/list")]
    PracticeList(crate::dto::practice::PracticeListParams),
    /// Starts an exercise on a simulated vehicle UXDuck started: the vehicle
    /// is started again at the exercise's start, set up, then judged. Needs
    /// `config`; the agent asks first. Result: `PracticeAttempt`.
    #[serde(rename = "practice/start")]
    PracticeStart(crate::dto::practice::PracticeStartParams),
    /// Answers a question of the running attempt ("Read the panel"),
    /// checked against the vehicle now. Needs `control`. Result:
    /// `PracticeAnswerResult`.
    #[serde(rename = "practice/answer")]
    PracticeAnswer(crate::dto::practice::PracticeAnswerParams),
    /// A folder on a vehicle, over MAVLink FTP (S121, ADR-0027): `/` is its
    /// storage's root (ArduPilot's SD card: logs in `/APM/LOGS`, scripts in
    /// `/APM/scripts`; PX4's in `/fs/microsd/log`), `@SYS` ArduPilot's own
    /// files (`uarts.txt`, `threads.txt`, `tasks.txt`, `memory.txt`). Needs
    /// `read`. Result: `FilesListResult`, once the vehicle answers.
    #[serde(rename = "files/list")]
    FilesList(crate::dto::files::FilesPathParams),
    /// A small file's text, up to 64 KB, such as `@SYS/uarts.txt` for
    /// what each serial port carries. Needs `read`. Result:
    /// `FilesReadResult`.
    #[serde(rename = "files/read")]
    FilesRead(crate::dto::files::FilesPathParams),
    /// Copies a file from the vehicle to this computer, as a transfer that
    /// goes on whoever watches it. Needs `read`. Result: `FileTransfer`.
    #[serde(rename = "files/download")]
    FilesDownload(crate::dto::files::FilesDownloadParams),
    /// Sends a file to the vehicle, replacing one of the same name. Needs
    /// `config`; the agent asks first. Result: `FileTransfer`.
    #[serde(rename = "files/upload")]
    FilesUpload(crate::dto::files::FilesUploadParams),
    /// Deletes a file, or an empty folder, from the vehicle. Needs
    /// `config`; the agent asks first. Result: `Empty`.
    #[serde(rename = "files/remove")]
    FilesRemove(crate::dto::files::FilesRemoveParams),
    /// Makes a folder on the vehicle. Needs `config`. Result: `Empty`.
    #[serde(rename = "files/mkdir")]
    FilesMkdir(crate::dto::files::FilesPathParams),
    /// Renames or moves a file on the vehicle. Needs `config`. Result: `Empty`.
    #[serde(rename = "files/rename")]
    FilesRename(crate::dto::files::FilesRenameParams),
    /// Pauses a transfer; its bytes so far are kept. Needs `read`. Result:
    /// `FileTransfer`.
    #[serde(rename = "files/pause")]
    FilesPause(crate::dto::files::FileTransferParams),
    /// Resumes a paused, waiting or failed transfer from where it stopped.
    /// Needs `read`. Result: `FileTransfer`.
    #[serde(rename = "files/resume")]
    FilesResume(crate::dto::files::FileTransferParams),
    /// Cancels a transfer, or puts a finished one away with its copy.
    /// Needs `read`. Result: `Empty`.
    #[serde(rename = "files/cancel")]
    FilesCancel(crate::dto::files::FileTransferParams),
    /// A one-time address the browser saves a finished download from.
    /// Needs `read`. Result: `FileSaveResult`.
    #[serde(rename = "files/save")]
    FilesSave(crate::dto::files::FileTransferParams),
    /// Gives up the vehicle's running attempt; the vehicle stays where it
    /// is. Needs `control`. Result: `PracticeAttempt`.
    #[serde(rename = "practice/stop")]
    PracticeStop(crate::dto::practice::PracticeVehicleParams),
    /// The vehicle's latest attempt, running or ended. Needs `read`.
    /// Result: `PracticeStatusResult`.
    #[serde(rename = "practice/status")]
    PracticeStatus(crate::dto::practice::PracticeVehicleParams),
    /// The "Get started" guide on a simulated vehicle UXDuck started (S117):
    /// its running guide, or the guide for its kind of vehicle started on it
    /// as it is, with no restart, judged from now. Refused while another
    /// exercise runs on it. Needs `control`. Result: `PracticeAttempt`.
    #[serde(rename = "practice/guide")]
    PracticeGuide(crate::dto::practice::PracticeVehicleParams),
    /// The ground's height at a point (S23, ADR-0026), from UXDuck's own
    /// copy of a terrain dataset: Settings › Terrain's default unless one
    /// is given. Answers once the data is fetched, or from what this
    /// computer keeps when offline; says why when there is no height.
    /// Needs `read`. Result: `TerrainAtResult`.
    #[serde(rename = "terrain/at")]
    TerrainAt(crate::dto::terrain::TerrainAtParams),
    /// Asks a vehicle for its own terrain height (`TERRAIN_CHECK`, S119) at
    /// a point, or at home and under it, and compares it with what its
    /// dataset gives on its own grid: 2 m apart is a difference. Needs
    /// `read`. Result: `TerrainCheckResult`, once the vehicle answers or
    /// five seconds pass.
    #[serde(rename = "terrain/check")]
    TerrainCheck(crate::dto::terrain::TerrainCheckParams),
    /// A vehicle's dataset, whether it was chosen for it, and its last
    /// check. Needs `read`. Result: `TerrainStatusResult`.
    #[serde(rename = "terrain/status")]
    TerrainStatus(crate::dto::terrain::TerrainStatusParams),
    /// The areas whose terrain is kept offline (S120), how each is going,
    /// and when the manifest was last checked. Needs `read`. Result:
    /// `TerrainAreasResult`.
    #[serde(rename = "terrain/areas")]
    TerrainAreas(Empty),
    /// What keeping an area would take, from the default dataset: chunks
    /// and bytes. Needs `read`. Result: `TerrainAreaSize`.
    #[serde(rename = "terrain/areaSize")]
    TerrainAreaSize(crate::dto::terrain::TerrainBounds),
    /// Keeps an area's terrain on this computer for use offline, and
    /// starts fetching it. Needs `config`. Result: `TerrainArea`.
    #[serde(rename = "terrain/keepArea")]
    TerrainKeepArea(crate::dto::terrain::TerrainKeepAreaParams),
    /// Stops keeping an area; what was fetched stays until it is needed
    /// no more. Needs `config`. Result: `Empty`.
    #[serde(rename = "terrain/removeArea")]
    TerrainRemoveArea(crate::dto::link::IdParams),
    #[serde(rename = "watchLink/create")]
    WatchLinkCreate(crate::dto::edge::WatchLinkCreateParams),
    /// This computer's watch links that have not ended, with how many watch
    /// through each. Needs `read`. Result: `WatchLinkListResult`.
    #[serde(rename = "watchLink/list")]
    WatchLinkList(Empty),
    /// How many guests watch each vehicle now, through any watch link; then
    /// `watchLink/viewersChanged` when it changes. Needs `read`. Result:
    /// `WatchViewersResult`.
    #[serde(rename = "watchLink/viewers")]
    WatchLinkViewers(Empty),
    /// Ends a watch link now; its guests are closed within a second. Needs
    /// `config`. Result: `Empty`.
    #[serde(rename = "watchLink/revoke")]
    WatchLinkRevoke(crate::dto::edge::WatchLinkRevokeParams),
    /// Someone away takes a vehicle's control lease (S80): only through
    /// the relay.
    #[serde(rename = "lease/take")]
    LeaseTake(crate::dto::edge::LeaseParams),
    /// Gives a lease back: its holder, or anyone at the computer taking it
    /// back.
    #[serde(rename = "lease/release")]
    LeaseRelease(crate::dto::edge::LeaseParams),
    /// Watches another of the account's computers from here (S86), through
    /// the relay as this computer's account: its vehicles arrive as
    /// `remote/changed` until no computer is given or the connection ends.
    /// Read only. Needs `read`. Result: `Empty`.
    #[serde(rename = "remote/watch")]
    RemoteWatch(crate::dto::edge::RemoteWatchParams),
    /// Deletes everything this account shared. Configuration capability.
    /// Result: `SharedDeleted`.
    #[serde(rename = "account/deleteShared")]
    AccountDeleteShared(Empty),
    /// Forgets an offline vehicle: its record goes, with its name. Heard
    /// again, it comes back as new. A vehicle still heard is refused with
    /// `busy`. Needs `config`. Result: `Empty`.
    #[serde(rename = "vehicle/delete")]
    VehicleDelete(IdParams),
    /// Enumerates local camera identities asynchronously on the daemon computer. Needs `config`. Result: `VideoDevicesResult`.
    #[serde(rename = "video/devices")]
    VideoDevices(Empty),
    /// Adds or configures a camera attached to the daemon's computer. Needs config.
    #[serde(rename = "video/capture")]
    VideoCapture(VideoCaptureParams),
    /// Opens macOS camera/microphone privacy settings on the daemon computer. Needs config. Result: Empty.
    #[serde(rename = "video/capture/settings")]
    VideoCaptureSettings(VideoCaptureSettingsParams),
    /// Add/replace a prerecorded camera; result `VideoSource`.
    #[serde(rename = "video/file")]
    VideoFile(VideoFileParams),
    /// Optional demo catalogue and downloads; result `VideoDemosResult`.
    #[serde(rename = "video/demos")]
    VideoDemos(VideoDemosParams),
    /// Enables or pauses capture independently of UI sessions. Needs config.
    #[serde(rename = "video/capture/enable")]
    VideoCaptureEnable(VideoCaptureEnableParams),
    /// The camera streams arriving at UXDuck, with their codec, picture
    /// size, the vehicle each belongs to, and counters that say why a
    /// stream plays badly (lost packets, damaged pictures, malformed
    /// lengths). Also the UDP port UXDuck listens on for video, and why not
    /// if it cannot. Needs `read`. Result: `VideoListResult`.
    #[serde(rename = "video/list")]
    VideoList(Empty),
    /// Says which vehicle a camera stream belongs to, or with no vehicle
    /// goes back to matching it by the sender's address. Needs `config`.
    /// Result: `VideoSource`.
    #[serde(rename = "video/assign")]
    VideoAssign(VideoAssignParams),
    /// Starts receiving a camera stream's pictures: open a WebSocket on the
    /// path returned. Needs `read`. Result: `VideoWatchResult`.
    #[serde(rename = "video/watch")]
    VideoWatch(VideoWatchParams),
    /// Keeps a camera's live metadata flowing for a visible map, without
    /// granting a video socket. Needs `read`. Result: `VideoMetadataWatchResult`.
    #[serde(rename = "video/metadata/watch")]
    VideoMetadataWatch(VideoWatchParams),
    /// Stops a watch and closes its socket. Needs `read`. Result: `Empty`.
    #[serde(rename = "video/unwatch")]
    VideoUnwatch(VideoUnwatchParams),
    /// Adds a camera by hand: an RTSP address UXDuck connects to and keeps
    /// reconnecting, or another UDP port to listen on for video. Needs
    /// `config`, so an agent asks the operator first. Result:
    /// `VideoAddResult`.
    #[serde(rename = "video/add")]
    VideoAdd(VideoAddParams),
    /// Removes a camera or a UDP port added by hand. Needs `config`. Result:
    /// `Empty`.
    #[serde(rename = "video/remove")]
    VideoRemove(VideoRemoveParams),
    /// Starts or stops recording a camera by hand, whatever the recording
    /// setting says, until explicitly stopped or the daemon restarts. Needs `config`. Result:
    /// `Empty`.
    #[serde(rename = "video/record")]
    VideoRecord(VideoRecordParams),
    /// Record all and Stop all (S153): `on` asks every camera that records
    /// with the others and isn't paused or unplayable to record, as
    /// `video/record` would, and stops none; off stops every camera recording or asked to, as
    /// `video/record` off would, until it is recorded again or its vehicle
    /// next arms. With `vehicleId`, only that vehicle's cameras (S154).
    /// Needs `config`. Result: `Empty`.
    #[serde(rename = "video/recordAll")]
    VideoRecordAll(VideoRecordAllParams),
    /// Whether a camera records with the others (S153): off, neither the
    /// recording setting nor `video/recordAll` starts it, only
    /// `video/record`. Kept with the camera; on by default. Needs `config`.
    /// Result: `VideoSource`.
    #[serde(rename = "video/recordWithOthers")]
    VideoRecordWithOthers(VideoRecordWithOthersParams),
    /// A recording to play: an MP4 path on the daemon's origin, good for an
    /// hour. Needs `read`. Result: `VideoPlaybackResult`.
    #[serde(rename = "video/playback")]
    VideoPlayback(VideoPlaybackParams),
    /// Names a camera, such as "Front"; an empty name goes back to its
    /// address. Needs `config`. Result: `VideoSource`.
    #[serde(rename = "video/rename")]
    VideoRename(VideoRenameParams),
    /// Chooses which of an RTSP camera's video tracks plays (S90), kept with
    /// the camera as `#track=N` on its address; it reconnects to play it.
    /// Only cameras added by hand. Needs `config`. Result: `VideoSource`.
    #[serde(rename = "video/track")]
    VideoTrack(VideoTrackParams),
    /// Shows where recordings are kept in the computer's file manager
    /// (Finder, Explorer), with a recording selected if one is named. Only
    /// on the computer UXDuck runs on. Needs `read`. Result:
    /// `VideoRevealResult`.
    #[serde(rename = "video/reveal")]
    VideoReveal(VideoRevealParams),
    /// Keeps a photo from the video (S144): a JPEG of the picture a Camera
    /// tab shows, written to the platform's pictures folder with EXIF (the
    /// time, the camera, and where its vehicle was). Needs `config`.
    /// Result: `VideoPhoto`.
    #[serde(rename = "video/photo")]
    VideoPhoto(VideoPhotoParams),
    /// A photo to show: a JPEG path on the daemon's origin, good for an
    /// hour. Needs `read`. Result: `VideoPlaybackResult`.
    #[serde(rename = "video/photo/show")]
    VideoPhotoShow(VideoPhotoShowParams),
    /// Whether a camera's vehicle takes its own camera's photo and records
    /// along with UXDuck's (S145, ADR-0017 V20): `IMAGE_START_CAPTURE` with
    /// each photo, `VIDEO_START_CAPTURE` and `VIDEO_STOP_CAPTURE` with each
    /// recording. Kept with the camera. Needs `config`. Result:
    /// `VideoSource`.
    #[serde(rename = "video/vehicleCamera")]
    VideoVehicleCamera(VideoVehicleCameraParams),
    /// Starts looking for vehicles for this client: plugged in, on this
    /// computer and its gateway, or, with `network`, on the local network
    /// too. Finds arrive as `find/changed` until `find/stop` or the
    /// connection ends; only one search per client. Never connects
    /// anything. Needs `read`; `network` needs `config`. Result:
    /// `FindResult`.
    #[serde(rename = "find/start")]
    FindStart(FindStartParams),
    /// Stops this client's search. Result: `Empty`.
    #[serde(rename = "find/stop")]
    FindStop(Empty),
    /// Looks for vehicles once and answers with what was found and, for each
    /// thing that is not a vehicle, why (no MAVLink at any speed, in its
    /// bootloader, used by another program, not permitted, silent, or not
    /// answering). `detect` sends nothing; `nearby` looks at this computer
    /// and its gateway; `network` searches the local /24 and asks the
    /// operator first. Explain an empty or negative result with the
    /// reasons. Needs `read`; `network` needs `config`. Result:
    /// `FindResult`.
    #[serde(rename = "find/vehicles")]
    FindVehicles(FindStartParams),
    /// Recent selected telemetry with timestamps, coverage and full-window
    /// extrema; captured in memory before callers poll. Up to five minutes,
    /// bounded by memory, coalesced at the telemetry producer's rate.
    /// Needs `read`. Result: `TelemetryWindowResult`.
    #[serde(rename = "telemetry/window")]
    TelemetryWindow(TelemetryWindowParams),
    /// Configured links and their current status. Changes nothing. Needs
    /// `read`. Result: `LinkListResult`.
    #[serde(rename = "link/list")]
    LinkList(Empty),
}

impl ClientRequest {
    /// Every method name, for probing and for tests.
    pub const METHODS: [&'static str; 283] = [
        "initialize",
        "trace/subscribe",
        "pairing/list",
        "pairing/approve",
        "pairing/deny",
        "session/createBootstrap",
        "state/subscribe",
        "operator/list",
        "operator/profile",
        "operator/create",
        "operator/update",
        "operator/remove",
        "client/setOperator",
        "operator/switch",
        "vehicle/list",
        "thread/create",
        "thread/send",
        "thread/update",
        "thread/cancel",
        "agent/config",
        "credentials/set",
        "thread/delete",
        "thread/bulk",
        "folder/create",
        "folder/update",
        "folder/delete",
        "vehicle/get",
        "vehicle/save",
        "vehicle/label",
        "vehicle/size",
        "vehicle/traffic",
        "vehicle/look",
        "vehicle/bookmarks",
        "vehicle/dismiss",
        "link/add",
        "link/remove",
        "telemetry/subscribe",
        "telemetry/get",
        "link/enable",
        "link/disable",
        "link/update",
        "serial/list",
        "command/send",
        "command/list",
        "command/modes",
        "notification/acknowledge",
        "notification/dismiss",
        "client/list",
        "client/displayRegister",
        "client/displayList",
        "client/displayPull",
        "client/displayReply",
        "client/setCapabilities",
        "client/forget",
        "approval/list",
        "approval/resolve",
        "param/refresh",
        "param/set",
        "param/definitions",
        "param/definitions/list",
        "param/definitions/add",
        "param/definitions/remove",
        "param/search",
        "param/get",
        "param/list",
        "param/baselineSet",
        "param/baselines",
        "param/baselineRemove",
        "flight/list",
        "flight/keep",
        "param/diff",
        "vehicle/history",
        "vehicle/addNote",
        "vehicle/editNote",
        "vehicle/deleteNote",
        "mission/refresh",
        "mission/history",
        "mission/restore",
        "mission/upload",
        "controller/claim",
        "controller/keyboardStart",
        "controller/keyboardInput",
        "controller/keyboardStop",
        "controller/release",
        "controller/takeOver",
        "controller/forget",
        "controller/setTarget",
        "controller/inputs",
        "client/select",
        "mapping/update",
        "mapping/create",
        "daemon/restart",
        "settings/update",
        "storage/usage",
        "storage/clearSimulatorLogs",
        "storage/reveal",
        "plugin/install",
        "plugin/remove",
        "plugin/command",
        "plugin/features",
        "plugin/items",
        "plugin/setEnabled",
        "plugin/usage",
        "tool/search",
        "plugin/draft",
        "plugin/test",
        "plugin/drafts",
        "daemon/stop",
        "daemon/stopPreview",
        "update/status",
        "update/check",
        "update/install",
        "update/later",
        "client/visibility",
        "app/focus",
        "app/open",
        "desktopApp/status",
        "network/status",
        "desktopApp/download",
        "desktopApp/choose",
        "desktopApp/remove",
        "search/query",
        "place/search",
        "place/locate",
        "place/save",
        "place/update",
        "place/remove",
        "marker/list",
        "marker/add",
        "marker/update",
        "marker/remove",
        "marker/share",
        "marker/unshare",
        "plan/list",
        "plan/get",
        "plan/create",
        "plan/update",
        "plan/undo",
        "plan/redo",
        "plan/place",
        "plan/delete",
        "plan/restore",
        "plan/import",
        "plan/export",
        "plan/upload",
        "plan/check",
        "plan/examples",
        "transfer/cancel",
        "daemon/wakeups",
        "daemon/about",
        "analytics/track",
        "analytics/feedback",
        "analytics/activeTime",
        "analytics/takeFlightTime",
        "analytics/takeEngagement",
        "analytics/performance",
        "analytics/alias",
        "feedback/send",
        "debug/panic",
        "journal/import",
        "sim/start",
        "sim/stop",
        "sim/restart",
        "sim/pause",
        "sim/snapshot",
        "snapshot/start",
        "snapshot/list",
        "snapshot/rename",
        "snapshot/delete",
        "sim/binaries",
        "sim/catalogue",
        "sim/download",
        "sim/delete",
        "account/get",
        "account/flights",
        "account/versions",
        "account/environment",
        "account/setEnvironment",
        "account/signIn",
        "account/verify",
        "account/signOut",
        "account/setUpdates",
        "sync/get",
        "sync/set",
        "sync/activate",
        "sync/restore",
        "thread/feedback",
        "account/shared",
        "account/computers",
        "practice/list",
        "practice/start",
        "practice/answer",
        "practice/stop",
        "practice/status",
        "practice/guide",
        "files/list",
        "files/read",
        "files/download",
        "files/upload",
        "files/remove",
        "files/mkdir",
        "files/rename",
        "files/pause",
        "files/resume",
        "files/cancel",
        "files/save",
        "terrain/at",
        "terrain/check",
        "terrain/status",
        "terrain/areas",
        "terrain/areaSize",
        "terrain/keepArea",
        "terrain/removeArea",
        "watchLink/create",
        "watchLink/list",
        "watchLink/viewers",
        "watchLink/revoke",
        "lease/take",
        "lease/release",
        "remote/watch",
        "account/deleteShared",
        "vehicle/delete",
        "video/devices",
        "video/capture",
        "video/capture/settings",
        "video/file",
        "video/demos",
        "video/capture/enable",
        "video/list",
        "video/assign",
        "video/watch",
        "video/metadata/watch",
        "video/unwatch",
        "video/add",
        "video/remove",
        "video/record",
        "video/recordAll",
        "video/recordWithOthers",
        "video/playback",
        "video/rename",
        "video/track",
        "video/reveal",
        "video/photo",
        "video/photo/show",
        "video/vehicleCamera",
        "find/start",
        "find/stop",
        "find/vehicles",
        "plugin/allow",
        "plugin/share",
        "plugin/shares",
        "plugin/unshare",
        "plugin/shared",
        "plugin/installShared",
        "tray/menu",
        "tray/activate",
        "feedback/screenshot",
        "tray/location",
        "location/status",
        "location/start",
        "location/stop",
        "traffic/status",
        "vehicle/messages",
        "layout/open",
        "feedback/suggest",
        "account/setUsername",
        "account/sessions",
        "account/signOutSession",
        "account/signOutOthers",
        "account/requestDeletion",
        "account/feedback",
        "account/claimFeedback",
        "param/history",
        "mavlink/inspect",
        "mavlink/history",
        "router/events",
        "link/diagnose",
        "socket/inspect",
        "daemon/logs",
        "telemetry/window",
        "link/list",
        "interface/status",
        "interface/check",
    ];
}

/// A request from the daemon that one specific client must answer. None yet.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "method", content = "params")]
pub enum ServerRequest {}

/// A notification from a client. The daemon never replies.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "method", content = "params")]
pub enum ClientNotification {
    /// Give up on a pending request.
    #[serde(rename = "cancel")]
    Cancel(CancelParams),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CancelParams {
    pub id: RequestId,
}

/// A notification from the daemon.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "method", content = "params")]
pub enum ServerNotification {
    /// Full product update status changed. Sent to connections that requested `update/status`.
    #[serde(rename = "update/status")]
    UpdateStatus(crate::dto::update::AppUpdateStatus),
    /// Invalidate private account views; contains no identity or credentials.
    #[serde(rename = "account/changed")]
    AccountChanged(Empty),
    /// One requester explicitly wants this connection's current display settings.
    #[serde(rename = "client/displayRequested")]
    ClientDisplayRequested(DisplayRequested),
    /// One frame from the inspector tap, after `trace/subscribe`.
    #[serde(rename = "trace/frame")]
    TraceFrame(TraceFrame),
    /// One journal entry, after `state/subscribe`.
    #[serde(rename = "state/delta")]
    StateDelta(StateDelta),
    /// Subscribe again: this connection's operator changed, so what it may
    /// see did (each operator's chats are their own). After `state/subscribe`.
    #[serde(rename = "state/resync")]
    StateResync(Empty),
    /// Text streamed into an agent message in progress, to subscribers of
    /// `item`.
    #[serde(rename = "agent/itemDelta")]
    AgentItemDelta(AgentItemDelta),
    /// What changed in a vehicle's telemetry, after `telemetry/subscribe`.
    /// Not journaled.
    #[serde(rename = "telemetry/update")]
    TelemetryUpdate(VehicleTelemetry),
    /// This connection's capabilities were edited; the next request uses them.
    #[serde(rename = "client/capabilitiesChanged")]
    ClientCapabilitiesChanged(CapabilitiesChanged),
    /// An approval to answer, to clients that declared `approvals: true`.
    #[serde(rename = "approval/requested")]
    ApprovalRequested(Approval),
    /// An approval was answered or expired.
    #[serde(rename = "approval/resolved")]
    ApprovalResolved(Approval),
    /// Every current find, whenever one changes, to a client that called
    /// `find/start` (S44). Not journaled.
    #[serde(rename = "find/changed")]
    FindChanged(FindResult),
    /// How many guests watch each vehicle changed (S95): to every operator
    /// client, as `watchLink/viewers` answers.
    #[serde(rename = "watchLink/viewersChanged")]
    WatchLinkViewersChanged(crate::dto::edge::WatchViewersResult),
    /// The computer watched from away, whenever it changes, to a client that
    /// called `remote/watch` (S86). Not journaled.
    #[serde(rename = "remote/changed")]
    RemoteChanged(crate::dto::edge::RemoteView),
    /// One plugin's whole layer, whenever it changes, to a client that
    /// called `plugin/features` (S55). Not journaled.
    #[serde(rename = "plugin/featuresChanged")]
    PluginFeaturesChanged(PluginLayer),
    /// One plugin's own items, whenever they change, to a client that
    /// called `plugin/items` (S101). Not journaled.
    #[serde(rename = "plugin/itemsChanged")]
    PluginItemsChanged(crate::dto::plugin::PluginItems),
    /// The tray's menu, whenever it changes, to the connection that called
    /// `tray/menu` (S64). Not journaled.
    #[serde(rename = "tray/menuChanged")]
    TrayMenuChanged(TrayMenu),
    /// UXDuck was opened again while it ran (`app/open` with `reopened`):
    /// the tray says so under its icon, as reopening UXDuck.app does on
    /// macOS. To the connection that called `tray/menu`. Not journaled.
    #[serde(rename = "tray/reopened")]
    TrayReopened(Empty),
    /// Whether the tray should ask the system where this computer is
    /// (S137, ADR-0030 L2): on while a person has location on. To the
    /// connection that called `tray/menu`. Not journaled.
    #[serde(rename = "tray/locationWanted")]
    TrayLocationWanted(crate::dto::location::TrayLocationWanted),
    /// This computer's location changed (S137): to the connections that
    /// called `location/status` or `location/start`, as they answer. Not
    /// journaled.
    #[serde(rename = "location/changed")]
    LocationChanged(crate::dto::location::LocationStatus),
    /// The traffic picture or a source changed (S168): to the connections
    /// that called `traffic/status`, at most four times a second. Not
    /// journaled.
    #[serde(rename = "traffic/changed")]
    TrafficChanged(crate::dto::traffic::TrafficStatus),
    /// A live marker's latest position or how its source stands (ADR-0035),
    /// at most twice a second, to connections subscribed to markers that
    /// hold `location`. Not journaled.
    #[serde(rename = "marker/live")]
    MarkerLive(crate::dto::marker::MarkerLiveChanged),
    /// Opening UXDuck again found this page open: it comes forward, or
    /// flashes its title where the browser will not let it (S107). Sent to
    /// one page. Not journaled.
    #[serde(rename = "app/focus")]
    AppFocus(Empty),
    /// The interface a page loaded now gets changed, or how its updates
    /// are going did (S109): to every client with `read`, as
    /// `interface/status` answers. A page running another version offers
    /// a reload; it never reloads by itself. Not journaled.
    #[serde(rename = "interface/updated")]
    InterfaceUpdated(crate::dto::interface::InterfaceStatus),
    /// An entry of a vehicle's log, new or changed (a repeat counted), to a
    /// connection that called `vehicle/messages` with `subscribe` (S26).
    /// Not journaled.
    #[serde(rename = "vehicle/message")]
    VehicleMessage(VehicleMessage),
}
