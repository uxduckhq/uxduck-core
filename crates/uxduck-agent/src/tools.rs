//! Tools, generated from the contracts (ADR-0009: "no hand-written tool
//! list"). Each tool is a `ClientRequest` variant: its description is the
//! variant's doc comment and its parameters are the variant's params schema.
//! Only the methods in [`AGENT_METHODS`] are offered.

use schemars::generate::SchemaSettings;
use serde_json::{Value, json};
use uxduck_contracts::rpc::ClientRequest;

/// The API methods the agent may call as tools. Adding one here is the
/// whole change; what it may do is still decided by its capabilities.
pub const AGENT_METHODS: &[&str] = &[
    "vehicle/list",
    "vehicle/get",
    "telemetry/get",
    // Read-only diagnostics and recorded MAVLink evidence.
    "daemon/about",
    "mavlink/inspect",
    "mavlink/history",
    "mavlink/dialects/list",
    "firmware/list",
    "router/events",
    "link/diagnose",
    "socket/inspect",
    "daemon/logs",
    "telemetry/window",
    "link/list",
    "command/list",
    "command/send",
    "search/query",
    "place/search",
    "place/locate",
    // Markers on the map (ADR-0035): reading changes nothing; adding and
    // changing one asks first without `config`, as saved places do.
    "marker/list",
    "marker/add",
    "marker/update",
    "marker/remove",
    // Where this computer is, once a person has turned it on (S137,
    // ADR-0030 L4); the agent cannot turn it on.
    "location/status",
    // Other aircraft near the vehicles (S168, ADR-0034); only a person
    // turns a source on or off.
    "traffic/status",
    // Saved plans (ADR-0029): drafting and changing one touches no vehicle;
    // making or changing one asks first unless the agent has `config`.
    // Checking one is free; uploading one, or putting back a mission from a
    // vehicle's history, asks the operator first.
    "plan/list",
    "plan/get",
    "plan/create",
    "plan/update",
    "plan/check",
    "plan/examples",
    "plan/import",
    "plan/export",
    "plan/upload",
    // Flying plans in simulation (S293): checking is free; starting the
    // simulated vehicles asks first unless the agent has `config`.
    "plan/simulateCheck",
    "plan/simulate",
    "mission/refresh",
    "mission/history",
    // What a vehicle holds as UXDuck last read it, with where its mission,
    // fence and rally points stand (FB-256): asks the vehicle nothing.
    "mission/get",
    "mission/restore",
    // Simulated vehicles (S17): starting and stopping one asks the operator
    // first, unless the agent was given the configuration capability.
    "sim/binaries",
    "sim/frames",
    "sim/inspect",
    "sim/ownBuild",
    "sim/catalogue",
    "sim/download",
    "sim/start",
    "sim/stop",
    // Starting one again after a crash asks too, and so does pausing one.
    "sim/restart",
    "sim/pause",
    // Faults in a simulated vehicle's camera and gimbal (ADR-0063 Y16):
    // reading them is free, and switching one asks first, as pausing does.
    "sim/faults",
    "sim/fault",
    // Practice (S100, ADR-0024 X12): listing and reading attempts; starting
    // one asks first; the agent never flies an exercise for the person.
    "practice/list",
    "practice/start",
    "practice/status",
    // The Get started guide on a simulated vehicle (S117): starting it asks
    // first.
    "practice/guide",
    // Snapshots (S34): saving one and starting from one.
    "sim/snapshot",
    "snapshot/list",
    "snapshot/start",
    // Parameters (S68, ADR-0021 P8): reading them and their definitions;
    // setting one asks the operator first.
    "param/search",
    "param/refresh",
    "param/get",
    "param/list",
    "param/diff",
    "param/changes",
    "param/set",
    // Which descriptions a vehicle uses (ADR-0044 F10): asks the operator
    // first; adding and removing sets stay a person's.
    "param/definitions/use",
    // Which dialects a vehicle uses (F13): asks the operator first.
    "mavlink/dialects/use",
    // Saved parameter sets (S217, ADR-0037 V19): listing, comparing (with
    // `param/diff`'s `setId`) and exporting freely; applying one is a
    // parameter write and asks the operator first. Saving, renaming and
    // deleting them are a person's.
    "param/sets",
    "param/setExport",
    "param/setApply",
    // The Tuning tab's parts and their values (S329, ADR-0054 T13), with
    // how each axis flew (S333): read freely; gains change only through
    // `param/set`, which asks first.
    "tuning/state",
    // Prepare's starting values (S331): worked out, never written.
    "tuning/prepare",
    // A vehicle's history (S69, ADR-0022): reading it freely; offer useful
    // notes for completed work, write only with consent and config permission.
    "vehicle/history",
    "flight/list",
    // Replay (ADR-0039 R9): open a flight to show a moment, move its clock
    // and close it. `read` alone: nothing reaches a vehicle.
    "replay/open",
    "replay/seek",
    "replay/state",
    "replay/close",
    "flight/series",
    "replay/logFields",
    "replay/add",
    // Files the operator attached to a message (S246): text read freely; a
    // log replayed, or a video kept with a flight after asking, by its id.
    // A picture is shown to the model freely, as they attached it (FB-248).
    "attachment/read",
    "attachment/image",
    "flight/addVideo",
    // Operations (ADR-0039 R16): reading freely, gathering after asking.
    "operation/list",
    "operation/create",
    "operation/rename",
    "operation/addFlights",
    "operation/removeFlights",
    // A flight's name, after asking.
    "flight/rename",
    // The logs on a vehicle's SD card plugged in, and those copied (S212).
    "logs/cards",
    // Flights in other ground stations' tlogs (S215).
    "flights/found",
    "vehicle/addNote",
    // Labels (S106): putting them on and off asks the operator first.
    "vehicle/label",
    "vehicle/size",
    "vehicle/look",
    "vehicle/colour",
    // Which actuator is the gripper, asking first, and its wiring (S263).
    "vehicle/gripper",
    "gripper/status",
    // Parameter changes with who made each and why (S74).
    "param/history",
    // A vehicle's own messages (S26): the folded log, freely.
    "vehicle/messages",
    // The ground's height at a point (S23), freely; a vehicle's own terrain
    // checked against it (S119), and what it asked for and was sent (S310).
    "terrain/at",
    "terrain/check",
    "terrain/status",
    "terrain/vehicle",
    // Areas kept offline (S120), polygons (ADR-0051 A5): listing and sizing
    // freely; keeping, changing or disabling one asks first.
    "terrain/areas",
    "terrain/areaSize",
    "terrain/keepArea",
    // Writing an area to a vehicle's card, or removing it (ADR-0051 A11),
    // asks first, as a file upload does.
    "terrain/putArea",
    // A vehicle's files (S121, ADR-0027 F6, F7): listing and reading small
    // ones freely, such as ArduPilot's `@SYS/uarts.txt`; sending and
    // deleting one asks the operator first.
    "files/list",
    "files/read",
    "files/upload",
    "files/remove",
    // A vehicle's setup (S184, ADR-0037 V4): what is calibrated, and a
    // procedure's progress and result, freely. Starting or cancelling one
    // needs a person at the vehicle, so they are not here.
    "setup/state",
    // A vehicle's preflight checks (S192, ADR-0038 K2), freely: the same
    // list the window and the CLI read. Launching past a failing one set
    // to hold is put to an operator; changing the checks is a person's.
    "preflight/state",
    // A vehicle's checklist (S193, S353, ADR-0038 K11): writing steps and
    // rules, with their groups and whether a person ticks a rule too, or
    // copying them in from another vehicle, a file or a pasted list,
    // asks the operator first without `config`. Ticking a step and arming
    // past one are a person's, so `checklist/tick` and `checklist/reset`
    // are not here. Nor is finishing a preflight (S350, K19). Earlier
    // preflights are read freely.
    "checklist/update",
    "checklist/copy",
    "checklist/export",
    "checklist/runs",
    // Opening a tab when asked (S43), in the chat's operator's layout.
    "layout/open",
    // A suggestion card for what UXDuck cannot do (S73): the person reads
    // and sends it. Where UXDuck sends reports, the agent files them itself
    // instead, the title and text only, and adds to them (S294).
    "feedback/suggest",
    "feedback/file",
    "feedback/add",
    // Camera video (S38, ADR-0017 V15): what is arriving and why it plays
    // badly; assigning or naming a camera asks the operator first.
    "video/list",
    // A look through a camera, as images (FB-244): read only, refused
    // while Settings › Privacy keeps cameras from the assistant.
    "video/frame",
    // A vehicle's cameras and what each can do (S404, ADR-0063 Y5), freely.
    "camera/list",
    // Having one take a photo, record, zoom, focus or format its card
    // (S405) is a command to its vehicle: it needs control, as
    // command/send does.
    "camera/command",
    // A camera's own settings, from its definition file (S411), freely.
    // Setting one is a camera/command.
    "camera/settings",
    // A vehicle's gimbals, what each can do and who has it (S406,
    // ADR-0063 Y6), freely.
    "gimbal/list",
    // Pointing one, centring it, having it hold north or look at a place
    // is a command to its vehicle: it needs control, as command/send
    // does, and taking it from the autopilot, a mission or another asks
    // a person.
    "gimbal/command",
    "video/devices",
    "video/capture",
    "video/capture/enable",
    "video/record",
    "video/recordAll",
    "video/assign",
    "video/add",
    "video/rename",
    // Finding vehicles (S44, ADR-0018 F10): the network asks first.
    "find/vehicles",
    // Plugins (S57, ADR-0006 X11): their commands found by words, then run
    // as `plugin_call`; running one asks the operator first.
    "tool/search",
    "plugin/command",
    // Writing plugins (S58, ADR-0006 X12): drafts and tests change nothing;
    // installing always asks the operator.
    "plugin/draft",
    "plugin/test",
    "plugin/drafts",
    "plugin/install",
    // Settings (maintainer, 2026-10-26): this device's and each operator's
    // preferences, read freely and changed only with `config`, which asks
    // the operator first. Skipped confirmations stay the operator's.
    "settings/get",
    "settings/update",
    "operator/list",
    "operator/update",
    // Calculated values (ADR-0056 K5): trying a formula changes nothing.
    "telemetry/calculate",
];

/// Methods agents are not offered, each on purpose. With [`AGENT_METHODS`]
/// they cover every method, so a new one (a new setting's, say) must be
/// placed in one or the other, and is not forgotten from MCP.
pub const KEPT_FROM_AGENTS: &[&str] = &[
    // The response graph's sample rate follows the Tuning tab being open (S330).
    "tuning/live",
    // The graph's samples, 25 a second, are more than a tool answer holds;
    // `tuning/state` has their summaries (S333).
    "tuning/response",
    // A connection's last 30 minutes a second at a time are more than a tool
    // answer holds. `vehicle/get` has how each connection is doing now.
    "vehicle/connectionHistory",
    // Only the renderer can observe scenery contact; agents can stop/restart simulations.
    "sim/crash",
    // Only a window draws a simulated camera's picture (S408): an agent
    // watches it as any camera's, with `video_frame`.
    "sim/pictures",
    // Companion computers are reached over SSH with the person's own keys
    // and agent (S296, ADR-0049 O8): adding one, signing in to it and
    // forgetting it are the person's.
    "computer/add",
    "computer/remove",
    // Its name is the person's, beside the computer's other settings.
    "computer/rename",
    "computer/check",
    "computer/cameras",
    "computer/network",
    "computer/connectivity",
    // Whether a vehicle is a simulation is the person's word (ADR-0039 R14).
    "vehicle/simulated",
    // Agreeing to the Terms of Service is the person's (ADR-0047).
    "terms/accept",
    // Saving a log to the account is the person's (ADR-0039 R11).
    "replay/save",
    // Naming a replay is the person's (ADR-0039 R21); an agent says what
    // it opened instead.
    "replay/rename",
    // Lining flights up is the bar's; the tools sent each turn are full.
    "replay/startTogether",
    // A flight's log as a file: bytes for the person to save, not to read.
    "flight/log",
    // Which folders are looked in is the person's (S215).
    "flights/folder",
    // Copying a card's logs is the person's, as plugging it in was.
    "logs/copyCard",
    // The replay bar's own controls: a person watches at their own pace.
    "replay/play",
    "replay/pause",
    "replay/restart",
    "replay/speed",
    "replay/nudge",
    // Where replay vehicles were, for the maps to draw them between
    // updates; an agent reads a flight with `flight/series`.
    "replay/motion",
    // An added video is aligned by the person's eye (ADR-0039 R8).
    "video/align",
    // Uploading a file is the person's: they attach it to their message.
    "attachment/begin",
    "attachment/append",
    "attachment/finish",
    // A vehicle's own 3D model's bytes, for the pages that draw it: its
    // name, size and warning are in the vehicle (ADR-0035 M5).
    "vehicle/modelRead",
    // Protocol, sessions, streams and window plumbing: not tools.
    "initialize",
    "trace/subscribe",
    "session/createBootstrap",
    "state/subscribe",
    "client/setOperator",
    "telemetry/subscribe",
    "client/list",
    "client/displayRegister",
    "client/displayList",
    "client/displayPull",
    "client/displayReply",
    "client/setCapabilities",
    "client/forget",
    "client/select",
    "client/visibility",
    "app/focus",
    "app/open",
    "video/watch",
    // Where a gimbal points ten times a second follows a Camera tab or
    // the Drone camera being open (S406): an agent reads it from the
    // vehicle's telemetry.
    "gimbal/watch",
    // Fetching a camera's definition file from an address is a person's
    // to agree to (S411): the address is the camera's to name.
    "camera/definition/fetch",
    "video/metadata/watch",
    "video/unwatch",
    "tray/menu",
    "tray/activate",
    "tray/location",
    "interface/status",
    "interface/check",
    // The operator's own: pairing, approvals, people, accounts, keys,
    // sharing, updates, analytics and stopping UXDuck.
    "pairing/list",
    "pairing/approve",
    "pairing/deny",
    "operator/profile",
    "operator/create",
    "operator/remove",
    "operator/switch",
    "agent/config",
    "credentials/set",
    "approval/list",
    "approval/resolve",
    "daemon/restart",
    "plugin/remove",
    "plugin/features",
    "plugin/items",
    "plugin/setEnabled",
    "plugin/usage",
    "daemon/stop",
    "daemon/stopPreview",
    "update/status",
    "update/check",
    "update/install",
    "update/later",
    "desktopApp/status",
    "network/status",
    "desktopApp/download",
    "desktopApp/choose",
    "desktopApp/remove",
    "daemon/wakeups",
    "analytics/track",
    "analytics/feedback",
    "analytics/activeTime",
    "analytics/engagedSimTime",
    "analytics/takeFlightTime",
    "analytics/takeEngagement",
    "analytics/performance",
    "analytics/alias",
    "feedback/send",
    "debug/panic",
    "journal/import",
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
    "account/shared",
    "watchLink/create",
    "watchLink/list",
    "watchLink/viewers",
    "watchLink/revoke",
    "account/deleteShared",
    "plugin/allow",
    "plugin/share",
    "plugin/shares",
    "plugin/unshare",
    "plugin/shared",
    "plugin/installShared",
    "feedback/screenshot",
    "account/setUsername",
    // The account's name is the person's own, like the username.
    "account/setName",
    // So is where they heard of UXDuck (S251).
    "account/setSource",
    // A vehicle's setup, preflight and checklist actions (S184 to S194):
    // starting, confirming or cancelling a procedure, spinning motors,
    // installing firmware, changing the checks and ticking a step need a
    // person at the vehicle, as the comments above on `setup/state`,
    // `preflight/state` and `checklist/update` say. So does a plugin's
    // check blocking arming.
    "setup/start",
    "setup/cancel",
    "setup/confirm",
    "setup/spin",
    "setup/firmware",
    "setup/firmwareReleases",
    "preflight/configure",
    "checklist/tick",
    "checklist/reset",
    "checklist/finish",
    "checklist/reopen",
    "plugin/setChecksBlock",
    "account/sessions",
    "account/signOutSession",
    "account/signOutOthers",
    "account/requestDeletion",
    "account/feedback",
    "account/claimFeedback",
    // The agent's own conversations, which the person manages.
    "thread/create",
    "thread/send",
    "thread/edit",
    "thread/queue",
    "agent/connection",
    "thread/update",
    "thread/cancel",
    "thread/delete",
    "thread/bulk",
    "folder/create",
    "folder/update",
    "folder/delete",
    "thread/feedback",
    // Not offered to agents yet: review before adding to AGENT_METHODS.
    "vehicle/save",
    "vehicle/traffic",
    "vehicle/bookmarks",
    "vehicle/dismiss",
    "vehicle/hide",
    "vehicle/forgetConnection",
    "vehicle/useConnection",
    "link/add",
    "link/remove",
    "link/enable",
    "link/disable",
    "link/update",
    "serial/list",
    "command/modes",
    "notification/acknowledge",
    "notification/dismiss",
    "param/definitions",
    "param/definitions/list",
    "param/definitions/add",
    "param/definitions/remove",
    "mavlink/dialects/add",
    "mavlink/dialects/remove",
    "mavlink/dialects/bind",
    "firmware/add",
    "firmware/remove",
    "firmware/rename",
    "param/setSave",
    "param/setRename",
    "param/setRemove",
    "flight/keep",
    "flight/remove",
    "flight/removal",
    // Writes and deletes files on a person's disks (ADR-0062).
    "flight/offload",
    "flight/end",
    "flight/uploadLog",
    "vehicle/editNote",
    "vehicle/deleteNote",
    "mission/upload",
    "controller/claim",
    // Holding the gripper is the operator's hand on the controls (S263).
    "gripper/hold",
    "controller/keyboardStart",
    "controller/keyboardInput",
    "controller/keyboardStop",
    "controller/release",
    "controller/takeOver",
    "controller/forget",
    "controller/setTarget",
    "controller/chooseMapping",
    "controller/inputs",
    // Navigate is the window's hand on its controller (S420).
    "controller/navigate",
    "mapping/update",
    "mapping/create",
    "mapping/remove",
    "storage/usage",
    "storage/clearSimulatorLogs",
    "storage/reveal",
    "place/save",
    "place/update",
    "place/remove",
    "marker/share",
    "marker/unshare",
    "plan/undo",
    "plan/redo",
    "plan/place",
    "plan/delete",
    "plan/restore",
    "transfer/cancel",
    "snapshot/rename",
    "snapshot/delete",
    "sim/delete",
    "practice/answer",
    "practice/stop",
    "files/download",
    "files/mkdir",
    "files/rename",
    "files/pause",
    "files/resume",
    "files/cancel",
    "files/save",
    "terrain/removeArea",
    "vehicle/delete",
    "video/capture/settings",
    "video/file",
    "video/demos",
    "video/remove",
    "video/recordWithOthers",
    "video/playback",
    "video/track",
    "video/addresses",
    "video/servedBy",
    "video/internet",
    "video/reveal",
    "video/photo",
    "video/photo/show",
    "video/vehicleCamera",
    "find/start",
    "find/stop",
    "location/start",
    "location/stop",
];

/// Methods the model is not sent each turn (maintainer, 2026-10-16: the
/// tools were 65 KB with every request): it finds them with `tool_search`
/// and runs them with `tool_call`. Every tool the instructions name stays
/// loaded. MCP discovery searches the full generated catalogue separately.
pub const DEFERRED_METHODS: &[&str] = &[
    "tuning/state",
    "tuning/prepare",
    "place/search",
    "place/locate",
    "marker/add",
    "marker/update",
    "marker/remove",
    "camera/list",
    "camera/command",
    "camera/settings",
    "gimbal/list",
    "gimbal/command",
    "video/devices",
    "video/capture",
    "video/capture/enable",
    "video/record",
    "video/recordAll",
    "location/status",
    "traffic/status",
    "daemon/about",
    "mavlink/inspect",
    "mavlink/history",
    "mavlink/dialects/list",
    "firmware/list",
    "router/events",
    "link/diagnose",
    "socket/inspect",
    "daemon/logs",
    "telemetry/window",
    "link/list",
    "plan/list",
    "plan/get",
    "plan/create",
    "plan/update",
    "plan/check",
    "plan/examples",
    "plan/import",
    "plan/export",
    "plan/upload",
    "plan/simulateCheck",
    "plan/simulate",
    "mission/refresh",
    "mission/history",
    "mission/get",
    "mission/restore",
    "sim/binaries",
    "sim/frames",
    "sim/inspect",
    "sim/ownBuild",
    "sim/catalogue",
    "sim/download",
    "sim/start",
    "sim/stop",
    "sim/restart",
    "sim/pause",
    "sim/faults",
    "sim/fault",
    "sim/snapshot",
    "snapshot/list",
    "snapshot/start",
    "practice/guide",
    "param/list",
    "param/refresh",
    "flight/list",
    "replay/open",
    "replay/seek",
    "replay/state",
    "replay/close",
    "flight/series",
    "replay/logFields",
    "replay/add",
    "attachment/read",
    "attachment/image",
    "flight/addVideo",
    "operation/list",
    "operation/create",
    "operation/rename",
    "operation/addFlights",
    "operation/removeFlights",
    "flight/rename",
    "logs/cards",
    "flights/found",
    "param/diff",
    "param/changes",
    "param/sets",
    "param/setExport",
    "param/setApply",
    "param/history",
    "param/definitions/use",
    "mavlink/dialects/use",
    "vehicle/label",
    "vehicle/size",
    "vehicle/look",
    "vehicle/colour",
    "vehicle/gripper",
    "gripper/status",
    "terrain/at",
    "terrain/check",
    "terrain/status",
    "terrain/vehicle",
    "terrain/areas",
    "terrain/areaSize",
    "terrain/keepArea",
    "terrain/putArea",
    "files/list",
    "files/read",
    "files/upload",
    "files/remove",
    "video/rename",
    // Writing plugins is rare, and its tools with their guide were 8 KB of
    // every request. Found with tool_search, an install still waits for an
    // operator (X12).
    "plugin/draft",
    "plugin/test",
    "plugin/drafts",
    "plugin/install",
    "settings/get",
    "settings/update",
    "operator/list",
    "operator/update",
    // Only where UXDuck Web sends reports itself (S294).
    "feedback/suggest",
    // Rare, and kept the tools sent each turn under budget for set home (S308).
    "checklist/copy",
    // Rare too, like the other exports, and kept them under budget for a
    // step's group, its tick on a rule and where it counts (S353, S352).
    "checklist/export",
    // Rare: the preflight under way is in `preflight/state`, and each
    // one that ended is in the vehicle's history too (S350).
    "checklist/runs",
    // Found with operator/update's calculated values (ADR-0056 K5).
    "telemetry/calculate",
    // A job a person asks for by name, and the largest tool left after
    // plugin writing (maintainer, 2026-11-15: "consider if more should be
    // behind search"). Found beside its copy, export and runs. The checks
    // are still read each turn (`preflight/state`).
    "checklist/update",
    // Adding or assigning a camera and the practice exercises are asked
    // for by name too (maintainer, 2026-11-15: "Move them too"). What is
    // arriving is still read each turn (`video/list`).
    "video/add",
    "video/assign",
    "practice/list",
    "practice/start",
    "practice/status",
];

/// The tool that runs a deferred one by name.
pub const TOOL_CALL: &str = "tool_call";

/// A method's provider-facing name: `/` as `_` and camelCase as snake
/// case, as providers want lowercase (`vehicle/addNote` is
/// `vehicle_add_note`).
#[must_use]
pub fn tool_name(method: &str) -> String {
    // ADR-0006 X11 names it for what the agent does with it.
    if method == "plugin/command" {
        return "plugin_call".into();
    }
    let mut name = String::with_capacity(method.len() + 4);
    for c in method.chars() {
        if c == '/' {
            name.push('_');
        } else if c.is_ascii_uppercase() {
            name.push('_');
            name.push(c.to_ascii_lowercase());
        } else {
            name.push(c);
        }
    }
    name
}

#[derive(Clone, Debug, PartialEq)]
pub struct Tool {
    /// The provider-facing name: the method with `/` as `_`, such as `vehicle_list`.
    pub name: String,
    /// The API method it calls, such as `vehicle/list`.
    pub method: String,
    pub description: String,
    /// JSON Schema of the params.
    pub parameters: Value,
}

impl Tool {
    /// The method for a provider-facing tool name.
    #[must_use]
    pub fn method_for(tools: &[Tool], name: &str) -> Option<String> {
        tools
            .iter()
            .find(|tool| tool.name == name)
            .map(|tool| tool.method.clone())
    }
}

/// A tool as sent each turn: its description is its method's doc, so its
/// params' own doc, which mostly says the same again, is left out.
fn as_sent(mut tool: Tool) -> Tool {
    if let Some(parameters) = tool.parameters.as_object_mut() {
        parameters.remove("description");
    }
    tool
}

/// What the model is sent each turn: the tools not deferred, and
/// `tool_call` to run a deferred one that `tool_search` found. Their words
/// go without the plan's own references ([`without_references`]).
#[must_use]
pub fn model_tools() -> Vec<Tool> {
    let mut sent: Vec<Tool> = tools()
        .into_iter()
        .filter(|tool| !DEFERRED_METHODS.contains(&tool.method.as_str()))
        .map(as_sent)
        .map(for_model)
        .collect();
    if let Some(search) = sent.iter_mut().find(|tool| tool.method == "tool/search") {
        search.description.push_str(
            " Also finds deferred UXDuck tools in uxduckTools: plans, missions, simulations, \
             terrain, files, parameters, history, link diagnostics, practice, cameras, \
             checklists and plugin writing. Run them with tool_call.",
        );
    }
    sent.push(Tool {
        name: TOOL_CALL.into(),
        method: String::new(),
        description: "Runs a UXDuck tool found by tool_search, using its name and params schema. \
            The tool's normal operator approvals apply."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The tool's name, as tool_search gave it." },
                "params": { "type": "object", "description": "Its params, as its schema says." }
            },
            "required": ["name"]
        }),
    });
    for tool in &mut sent {
        tool.description = without_references(&tool.description);
        describe_without_references(&mut tool.parameters);
    }
    sent
}

/// The limit the in-app agent asks for when the model gives none, below
/// the daemon's own default, which the app and MCP keep: every result
/// stays in the model's chat. A paged list also has the most it asks for,
/// so that a page fits whole within `MAX_TOOL_OUTPUT_CHARS` and the model
/// asks again from `nextOffset` for the rest (FB-104). Each with the
/// model's description of it.
const MODEL_LIMITS: &[(&str, u64, Option<u64>, &str)] = &[
    (
        "vehicle/messages",
        30,
        None,
        "The newest this many, 30 if left out, at most 2000.",
    ),
    (
        "param/list",
        50,
        Some(50),
        "At most this many, up to 50 (the default).",
    ),
    (
        "param/search",
        20,
        Some(50),
        "At most this many hits (default 20, at most 50).",
    ),
];

/// The limit the agent asks for in a call of `method` that asked for
/// `asked`, when it is not that.
pub(crate) fn model_limit(method: &str, asked: Option<&Value>) -> Option<u64> {
    let &(_, limit, most, _) = MODEL_LIMITS
        .iter()
        .find(|(limited, ..)| *limited == method)?;
    match asked {
        None | Some(Value::Null) => Some(limit),
        Some(asked) => {
            let most = most?;
            asked
                .as_u64()
                .is_some_and(|asked| asked > most)
                .then_some(most)
        }
    }
}

/// A tool as the in-app model is told of it: with the limit the agent asks
/// for, and what `output` leaves out of its results that the model would
/// otherwise miss. Kept short: the tools are sent with every request.
fn for_model(mut tool: Tool) -> Tool {
    if let Some((.., about)) = MODEL_LIMITS
        .iter()
        .find(|(limited, ..)| *limited == tool.method)
        && let Some(limit) = tool
            .parameters
            .pointer_mut("/properties/limit")
            .and_then(Value::as_object_mut)
    {
        limit.insert("description".into(), json!(about));
    }
    if tool.method == "command/list" {
        tool.description.push_str(
            " Without riskInFlight, it is as risky in flight. Without offlineBehaviour, a safety command is allowed offline and any other warns. A set-mode command without a label reads Set mode and its mode.",
        );
    }
    tool
}

/// A doc without its references to UXDuck's own plan, such as "(S65)" or
/// "(S168, ADR-0034 A5)". They name a slice or a decision for the people
/// who build UXDuck. A model cannot look one up, and it is sent these words
/// on every turn. Only a pair of brackets holding nothing else goes, with
/// the space before it.
fn without_references(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('(') {
        match rest[open..].find(')').map(|at| open + at) {
            Some(close) if only_references(&rest[open + 1..close]) => {
                out.push_str(rest[..open].trim_end());
                rest = &rest[close + 1..];
            }
            _ => {
                out.push_str(&rest[..=open]);
                rest = &rest[open + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Whether the words inside a pair of brackets are references and nothing
/// else: slices ("S65"), decisions ("ADR-0034") and their clauses ("A5"),
/// with at least one slice or decision among them.
fn only_references(inside: &str) -> bool {
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    let named = |word: &str| {
        word.strip_prefix("ADR-").is_some_and(digits) || word.strip_prefix('S').is_some_and(digits)
    };
    let clause = |word: &str| {
        let number = word.trim_start_matches(|c: char| c.is_ascii_uppercase());
        number.len() < word.len() && digits(number)
    };
    let words: Vec<&str> = inside
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|word| !word.is_empty())
        .collect();
    words.iter().any(|word| named(word)) && words.iter().all(|word| named(word) || clause(word))
}

/// The same for every description in a params schema.
fn describe_without_references(schema: &mut Value) {
    match schema {
        Value::Object(object) => {
            for (key, value) in object.iter_mut() {
                match value {
                    Value::String(text) if key == "description" => {
                        *text = without_references(text);
                    }
                    _ => describe_without_references(value),
                }
            }
        }
        Value::Array(values) => values.iter_mut().for_each(describe_without_references),
        _ => {}
    }
}

pub(crate) fn chat_tools(can_file_feedback: bool) -> (Vec<Tool>, Vec<Tool>) {
    let mut all = tools();
    let mut sent = model_tools();
    if !can_file_feedback {
        for catalog in [&mut all, &mut sent] {
            catalog
                .retain(|tool| !matches!(tool.method.as_str(), "feedback/file" | "feedback/add"));
        }
        sent.extend(
            all.iter()
                .filter(|tool| tool.method == "feedback/suggest")
                .cloned()
                .map(as_sent),
        );
    }
    (all, sent)
}

/// The deferred tools matching words, best first: each word in a tool's
/// name counts three times one in its description.
#[must_use]
pub fn find_deferred(query: &str, limit: usize) -> Vec<Tool> {
    let words: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 1)
        .map(str::to_lowercase)
        .collect();
    let mut scored: Vec<(usize, Tool)> = tools()
        .into_iter()
        .filter(|tool| DEFERRED_METHODS.contains(&tool.method.as_str()))
        .filter_map(|tool| {
            let name = tool.name.to_lowercase();
            let about = tool.description.to_lowercase();
            let score: usize = words
                .iter()
                .map(|w| {
                    3 * usize::from(name.contains(w.as_str()))
                        + usize::from(about.contains(w.as_str()))
                })
                .sum();
            (score > 0).then_some((score, tool))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, tool)| for_model(tool))
        .collect()
}

/// Every agent tool, from `ClientRequest`'s schema.
#[must_use]
pub fn tools() -> Vec<Tool> {
    let settings =
        SchemaSettings::draft2020_12().with(|settings| settings.inline_subschemas = true);
    let schema = settings
        .into_generator()
        .into_root_schema_for::<ClientRequest>();
    let variants = schema
        .as_value()
        .get("oneOf")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    AGENT_METHODS
        .iter()
        .filter_map(|method| {
            let variant = variants
                .iter()
                .find(|v| v.pointer("/properties/method/const") == Some(&json!(method)))?;
            let mut parameters = variant
                .pointer("/properties/params")
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object" }));
            if let Some(object) = parameters.as_object_mut() {
                object.remove("$schema");
                object.entry("properties").or_insert_with(|| json!({}));
            }
            // Agent calls return a bounded snapshot. Neither model providers
            // nor MCP forward the daemon's subscription notifications.
            if *method == "vehicle/messages"
                && let Some(properties) = parameters["properties"].as_object_mut()
            {
                properties.remove("subscribe");
            }
            // A camera's commands have a tool of their own (S405,
            // `camera/command`), found with `tool_search`, and so have a
            // gimbal's (S406, `gimbal/command`). Their schemas are left
            // out here, where they would be sent with every request.
            if *method == "command/send"
                && let Some(commands) = parameters
                    .pointer_mut("/properties/command/oneOf")
                    .and_then(Value::as_array_mut)
            {
                commands.retain(|command| {
                    let kind = command.pointer("/properties/type/const");
                    kind != Some(&json!("camera")) && kind != Some(&json!("gimbal"))
                });
            }
            // The turn names the chat itself; the chat is always open, and
            // the duck and Get started are no one's request. The method's
            // doc says the rest.
            if *method == "layout/open" {
                if let Some(object) = parameters.as_object_mut() {
                    object.remove("description");
                }
                if let Some(properties) = parameters["properties"].as_object_mut() {
                    properties.remove("threadId");
                }
                if let Some(kinds) = parameters
                    .pointer_mut("/properties/kind/oneOf")
                    .and_then(Value::as_array_mut)
                {
                    kinds.retain(|kind| {
                        !matches!(kind["const"].as_str(), Some("chat" | "get-started" | "duck"))
                    });
                }
            }
            // The host names the chat, only the person attaches files, and a
            // report the agent files is one of three kinds.
            if matches!(*method, "feedback/file" | "feedback/add")
                && let Some(properties) = parameters["properties"].as_object_mut()
            {
                properties.remove("threadId");
                properties.remove("attachments");
                if let Some(kind) = properties.get_mut("kind") {
                    let description = kind["description"].clone();
                    *kind = json!({
                        "enum": ["problem", "suggestion", "idea"],
                        "description": description,
                    });
                }
            }
            let mut description = variant
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or(method)
                .to_owned();
            if *method == "plugin/draft" {
                description.push_str("\n\n");
                // A Windows checkout gives the guide CRLF line ends: the same text everywhere.
                description.push_str(&include_str!("plugin-guide.md").replace('\r', ""));
            }
            if *method == "vehicle/messages" {
                // The doc comment wraps anywhere; match it as one line.
                description = description.split_whitespace().collect::<Vec<_>>().join(" ");
                description = description.replace(
                    "With `subscribe`, `vehicle/message` follows.",
                    "A bounded snapshot; call again for newer messages. Subscriptions are not supported by agent tools.",
                );
            }
            if *method == "tool/search" {
                // Only the in-app model_tools catalogue adds the turn loop's
                // uxduckTools/tool_call instructions; MCP has its own discovery.
                description = description.split_whitespace().collect::<Vec<_>>().join(" ");
            }
            Some(Tool {
                name: tool_name(method),
                method: (*method).to_owned(),
                description,
                parameters,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    #[test]
    fn recovery_tools_are_discovered_without_growing_the_initial_context() {
        for (query, expected) in [
            ("flight logs", "flight_list"),
            ("download simulator", "sim_download"),
            ("param_refresh", "param_refresh"),
            ("sim_catalogue", "sim_catalogue"),
        ] {
            let found = super::find_deferred(query, 5);
            assert!(
                found.iter().any(|tool| tool.name == expected),
                "{query}: {found:?}"
            );
            assert!(
                !super::model_tools()
                    .iter()
                    .any(|tool| tool.name == expected)
            );
        }
    }

    /// Writing plugins is rare: its tools are found with `tool_search`, not
    /// sent each turn, and an install still waits for an operator.
    #[test]
    fn plugin_writing_tools_are_found_with_tool_search_and_installs_still_asked() {
        for (query, expected) in [
            ("write a plugin", "plugin_draft"),
            ("test a plugin draft", "plugin_test"),
            ("install the plugin", "plugin_install"),
        ] {
            let found = super::find_deferred(query, 5);
            assert!(
                found.iter().any(|tool| tool.name == expected),
                "{query}: {:?}",
                found.iter().map(|t| &t.name).collect::<Vec<_>>()
            );
            assert!(
                !super::model_tools()
                    .iter()
                    .any(|tool| tool.name == expected)
            );
        }
        let instructions = crate::turn::INSTRUCTIONS;
        assert!(instructions.contains("find the plugin writing tools with tool_search"));
        assert!(
            instructions
                .contains("install it only when asked. An operator always approves an install.")
        );
    }

    #[test]
    fn plugin_authors_get_the_runtime_contract_when_they_discover_drafting() {
        let draft = super::tools()
            .into_iter()
            .find(|tool| tool.method == "plugin/draft")
            .unwrap();
        for term in [
            "@uxduck/sdk",
            "onStart",
            "onTick",
            "vehicleId",
            "battery.remaining_pct",
            "wakeInMs",
            "flightS",
        ] {
            assert!(draft.description.contains(term), "missing {term}");
        }
    }

    use super::*;

    /// An agent may stop a simulation, which keeps it (S264), but hiding or
    /// deleting it stays a person's: Stop and hide is `sim/stop` then
    /// `vehicle/hide`, and Delete is `link/remove`.
    #[test]
    fn agents_stop_simulations_but_never_hide_or_delete_them() {
        assert!(AGENT_METHODS.contains(&"sim/stop"));
        for method in ["vehicle/hide", "link/remove", "vehicle/delete"] {
            assert!(!AGENT_METHODS.contains(&method), "{method}");
            assert!(KEPT_FROM_AGENTS.contains(&method), "{method}");
        }
    }

    #[test]
    fn every_method_is_offered_to_agents_or_kept_from_them_on_purpose() {
        let offered: std::collections::BTreeSet<_> = AGENT_METHODS.iter().collect();
        let kept: std::collections::BTreeSet<_> = KEPT_FROM_AGENTS.iter().collect();
        assert!(
            offered.is_disjoint(&kept),
            "{:?}",
            offered.intersection(&kept)
        );
        let unplaced: Vec<_> = ClientRequest::METHODS
            .iter()
            .filter(|method| !offered.contains(method) && !kept.contains(method))
            .collect();
        assert!(
            unplaced.is_empty(),
            "add each to AGENT_METHODS or KEPT_FROM_AGENTS: {unplaced:?}"
        );
        let unknown: Vec<_> = kept
            .iter()
            .filter(|method| !ClientRequest::METHODS.contains(method))
            .collect();
        assert!(unknown.is_empty(), "no such methods: {unknown:?}");
    }

    /// Tuning (S333, ADR-0054 T13): the agent reads it, with how each axis
    /// flew, and opens the tab at a part; the graph's samples and their rate
    /// are the tab's, and gains change only through `param/set`.
    #[test]
    fn the_agent_reads_tuning_and_opens_it_at_a_part() {
        assert!(AGENT_METHODS.contains(&"tuning/state"));
        assert!(AGENT_METHODS.contains(&"param/set"));
        for method in ["tuning/response", "tuning/live"] {
            assert!(KEPT_FROM_AGENTS.contains(&method), "{method}");
        }
        let found = find_deferred("tuning gains oscillating", 3);
        assert_eq!(
            found[0].name,
            "tuning_state",
            "{:?}",
            found.iter().map(|t| &t.name).collect::<Vec<_>>()
        );
        assert!(
            found[0].description.contains("how each axis flew"),
            "{}",
            found[0].description
        );
        let open = tools()
            .into_iter()
            .find(|t| t.name == "layout_open")
            .unwrap();
        assert!(
            open.parameters["properties"]["section"].is_object(),
            "{}",
            open.parameters
        );
        let instructions = crate::turn::INSTRUCTIONS;
        for words in [
            "read its tuning state first",
            "param_set",
            "Never start AutoTune",
        ] {
            assert!(instructions.contains(words), "{words}");
        }
    }

    /// FB-217: a parameter's value is the vehicle's and its default the
    /// firmware's as the vehicle reported it. The definition set only
    /// describes, so the agent never credits it with a value or a default.
    #[test]
    fn parameter_tools_say_whose_value_and_default_they_give() {
        let tools = tools();
        let says = |name: &str, words: &[&str]| {
            let tool = tools.iter().find(|t| t.name == name).unwrap();
            let description = tool
                .description
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            for words in words {
                assert!(
                    description.contains(words),
                    "{name}: {words}: {description}"
                );
            }
        };
        says(
            "param_get",
            &[
                "`value` is what the vehicle holds now",
                "`default` its firmware's default as the vehicle reported it",
                "never sets or overrides either",
            ],
        );
        for name in ["param_search", "param_list"] {
            says(
                name,
                &["Values and defaults are the vehicle's, never the definition set's"],
            );
        }
    }

    /// FB-141, FB-168: an agent read a replay result's `atUs`, this
    /// computer's clock, as where the replay was, and so reported a seek
    /// as landing near the end, or past it, when the replay was where it
    /// was asked to be.
    #[test]
    fn replay_tools_say_where_the_replay_is_and_what_at_us_is() {
        let tools = tools();
        for name in ["replay_open", "replay_seek"] {
            let tool = tools.iter().find(|t| t.name == name).unwrap();
            let description = tool
                .description
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            for words in [
                "the result's `timeUs`",
                "Its `atUs` is this computer's clock when that was read, never a time in the replay",
            ] {
                assert!(
                    description.contains(words),
                    "{name}: {words}: {description}"
                );
            }
        }
    }

    #[test]
    fn names_are_lowercase_with_underscores() {
        assert_eq!(tool_name("vehicle/list"), "vehicle_list");
        assert_eq!(tool_name("vehicle/addNote"), "vehicle_add_note");
    }

    /// The hosted model takes at most 64 tools, named in lowercase with
    /// underscores, in 128 KB, as UXDuck's API checks.
    #[test]
    fn the_hosted_model_takes_every_tool() {
        let tools = model_tools();
        assert!(tools.len() <= 64, "{} tools", tools.len());
        for tool in &tools {
            let mut chars = tool.name.chars();
            let fits = tool.name.len() <= 64
                && chars.next().is_some_and(|c| c.is_ascii_lowercase())
                && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
            assert!(fits, "{} would be refused", tool.name);
        }
        let size = serde_json::to_string(
            &tools
                .iter()
                .map(|t| (&t.name, &t.description, &t.parameters))
                .collect::<Vec<_>>(),
        )
        .map_or(usize::MAX, |json| json.len());
        assert!(size < 128 * 1024, "{size} bytes of tools");
        // Sent with every request: 65 KB once, 28 KB since plugin writing
        // was deferred and the biggest schemas tightened, with 2 KB to spare.
        assert!(size < 30 * 1024, "{size} bytes of tools sent each turn");
    }

    #[test]
    fn the_model_is_told_the_limits_the_agent_asks_for() {
        let limit = |tool: &Tool| {
            tool.parameters["properties"]["limit"]["description"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        };
        let sent = model_tools();
        let messages = sent
            .iter()
            .find(|t| t.method == "vehicle/messages")
            .unwrap();
        assert!(
            limit(messages).contains("30 if left out"),
            "{}",
            limit(messages)
        );
        let list = find_deferred("param_list", 5)
            .into_iter()
            .find(|t| t.method == "param/list")
            .unwrap();
        assert!(limit(&list).contains("up to 50"), "{}", limit(&list));
        let search = sent.iter().find(|t| t.method == "param/search").unwrap();
        assert!(limit(search).contains("at most 50"), "{}", limit(search));
        let commands = sent.iter().find(|t| t.method == "command/list").unwrap();
        assert!(commands.description.contains("Without riskInFlight"));
        assert!(commands.description.contains("Without offlineBehaviour"));
        assert!(
            commands
                .description
                .contains("without a label reads Set mode")
        );
        // MCP clients keep the daemon's own defaults.
        let all = tools();
        let messages = all.iter().find(|t| t.method == "vehicle/messages").unwrap();
        assert!(
            limit(messages).contains("200 if left out"),
            "{}",
            limit(messages)
        );
        assert_eq!(model_limit("vehicle/history", None), None);
    }

    #[test]
    fn the_agent_asks_for_fewer_and_a_page_that_fits() {
        assert_eq!(model_limit("vehicle/messages", None), Some(30));
        assert_eq!(
            model_limit("vehicle/messages", Some(&json!(null))),
            Some(30)
        );
        assert_eq!(model_limit("vehicle/messages", Some(&json!(500))), None);
        assert_eq!(model_limit("param/list", None), Some(50));
        assert_eq!(model_limit("param/list", Some(&json!(20))), None);
        assert_eq!(model_limit("param/list", Some(&json!(500))), Some(50));
        assert_eq!(model_limit("param/search", None), Some(20));
        assert_eq!(model_limit("param/search", Some(&json!(100))), Some(50));
        // A limit the daemon would refuse is left for it to refuse.
        assert_eq!(model_limit("param/list", Some(&json!("all"))), None);
        assert_eq!(model_limit("telemetry/get", None), None);
    }

    /// The plan's own references stay in the docs people read and are taken
    /// out of what the model is sent each turn.
    #[test]
    fn the_model_is_sent_no_references_to_the_plan() {
        assert_eq!(
            without_references("The focused vehicle's readings, as tiles (S65)."),
            "The focused vehicle's readings, as tiles."
        );
        assert_eq!(
            without_references(
                "Practice exercises\n(S100, ADR-0024 X8). Follows a vehicle (S105)."
            ),
            "Practice exercises. Follows a vehicle."
        );
        assert_eq!(
            without_references("its checks (S193, S353, ADR-0038 K8, K11, K16, K17), as a tab"),
            "its checks, as a tab"
        );
        // Brackets that hold anything else stay as they are.
        for kept in [
            "a `chart` (any number) or a `readout` (one)",
            "the terrain check (P9) of a plan",
            "(S65 and its tiles)",
            "up to 2,000 characters (about 300 words)",
            "an open bracket (S65",
        ] {
            assert_eq!(without_references(kept), kept);
        }
        let words = |tools: &[Tool]| {
            serde_json::to_string(
                &tools
                    .iter()
                    .map(|t| (&t.description, &t.parameters))
                    .collect::<Vec<_>>(),
            )
            .unwrap_or_default()
        };
        let sent = words(&model_tools());
        assert!(!sent.contains("ADR-"), "a decision is still named");
        assert!(
            !sent
                .match_indices("(S")
                .any(|(at, _)| sent[at + 2..].starts_with(|c: char| c.is_ascii_digit())),
            "a slice is still named"
        );
        // MCP and the tools found by search keep the docs as they are written.
        assert!(words(&tools()).contains("(S65)"));
    }

    /// A camera's commands are a tool of their own, found by words, and
    /// their schema is not sent each turn inside `command_send` (S405).
    #[test]
    fn a_cameras_commands_are_their_own_deferred_tool() {
        let all = tools();
        let camera = all
            .iter()
            .find(|tool| tool.name == "camera_command")
            .unwrap_or_else(|| panic!("camera/command is an agent tool"));
        assert_eq!(
            camera.parameters["required"],
            serde_json::json!(["cameraId", "action"])
        );
        let actions: Vec<&str> = camera.parameters["properties"]["action"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|action| action.pointer("/properties/type/const")?.as_str())
            .collect();
        for action in ["take-photo", "start-recording", "zoom-step", "format"] {
            assert!(actions.contains(&action), "{action} of {actions:?}");
        }
        assert!(camera.description.contains("Needs `control`"));
        assert!(DEFERRED_METHODS.contains(&"camera/command"));
        assert!(
            model_tools()
                .iter()
                .all(|tool| tool.name != "camera_command")
        );
        let found: Vec<String> = find_deferred("have the camera take a photo", 3)
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        assert_eq!(
            found.first().map(String::as_str),
            Some("camera_command"),
            "{found:?}"
        );

        let send = all.iter().find(|tool| tool.name == "command_send").unwrap();
        let commands: Vec<&str> = send.parameters["properties"]["command"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|command| command.pointer("/properties/type/const")?.as_str())
            .collect();
        assert!(commands.contains(&"long") && commands.contains(&"arm"));
        assert!(!commands.contains(&"camera"), "{commands:?}");
    }

    /// A camera's own settings are read with a tool found by words and
    /// set with the camera's commands, and its definition file is fetched
    /// from an address only by a person (S411).
    #[test]
    fn a_cameras_own_settings_are_read_by_a_deferred_tool_and_set_as_a_command() {
        let all = tools();
        let settings = all
            .iter()
            .find(|tool| tool.name == "camera_settings")
            .unwrap_or_else(|| panic!("camera/settings is an agent tool"));
        assert_eq!(
            settings.parameters["required"],
            serde_json::json!(["cameraId"])
        );
        assert!(settings.description.contains("Needs `read`"));
        assert!(DEFERRED_METHODS.contains(&"camera/settings"));
        assert!(
            model_tools()
                .iter()
                .all(|tool| tool.name != "camera_settings")
        );
        let found: Vec<String> = find_deferred("what ISO is the camera set to", 3)
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        assert!(
            found.iter().any(|name| name == "camera_settings"),
            "{found:?}"
        );
        let camera = all
            .iter()
            .find(|tool| tool.name == "camera_command")
            .unwrap();
        let setting = camera.parameters["properties"]["action"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|action| action.pointer("/properties/type/const") == Some(&"set-setting".into()))
            .unwrap_or_else(|| panic!("set-setting is an action"));
        assert_eq!(
            setting["required"],
            serde_json::json!(["type", "name", "value"])
        );
        assert!(KEPT_FROM_AGENTS.contains(&"camera/definition/fetch"));
        assert!(
            all.iter()
                .all(|tool| tool.name != "camera_definition_fetch")
        );
    }

    /// A gimbal's commands are a tool of their own too, found by words,
    /// and their schema is not sent each turn inside `command_send`
    /// (S406).
    #[test]
    fn a_gimbals_commands_are_their_own_deferred_tool() {
        let all = tools();
        let gimbal = all
            .iter()
            .find(|tool| tool.name == "gimbal_command")
            .unwrap_or_else(|| panic!("gimbal/command is an agent tool"));
        assert_eq!(
            gimbal.parameters["required"],
            serde_json::json!(["gimbalId", "action"])
        );
        let actions: Vec<&str> = gimbal.parameters["properties"]["action"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|action| action.pointer("/properties/type/const")?.as_str())
            .collect();
        for action in [
            "take-control",
            "release-control",
            "point",
            "centre",
            "follow",
            "hold-north",
            "look-at",
        ] {
            assert!(actions.contains(&action), "{action} of {actions:?}");
        }
        assert!(gimbal.description.contains("Needs `control`"));
        for method in ["gimbal/list", "gimbal/command"] {
            assert!(DEFERRED_METHODS.contains(&method), "{method}");
        }
        assert!(KEPT_FROM_AGENTS.contains(&"gimbal/watch"));
        assert!(
            model_tools()
                .iter()
                .all(|tool| !tool.name.starts_with("gimbal_"))
        );
        for (words, tool) in [
            ("point the gimbal down", "gimbal_command"),
            ("who is in control of the gimbal", "gimbal_list"),
        ] {
            let found: Vec<String> = find_deferred(words, 3)
                .into_iter()
                .map(|tool| tool.name)
                .collect();
            assert_eq!(found.first().map(String::as_str), Some(tool), "{found:?}");
        }

        let send = all.iter().find(|tool| tool.name == "command_send").unwrap();
        let commands: Vec<&str> = send.parameters["properties"]["command"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|command| command.pointer("/properties/type/const")?.as_str())
            .collect();
        assert!(commands.contains(&"long") && commands.contains(&"arm"));
        assert!(!commands.contains(&"gimbal"), "{commands:?}");
    }

    #[test]
    fn every_agent_method_is_a_tool_with_its_doc_and_params() {
        let tools = tools();
        assert_eq!(tools.len(), AGENT_METHODS.len());
        let list = &tools[0];
        assert_eq!(list.name, "vehicle_list");
        assert_eq!(list.method, "vehicle/list");
        assert!(
            list.description
                .starts_with("The vehicles the daemon hears"),
            "{}",
            list.description
        );
        assert_eq!(tools[1].name, "vehicle_get");
        assert_eq!(tools[1].parameters["required"], serde_json::json!(["id"]));
        assert_eq!(list.parameters["type"], "object");
        assert_eq!(
            Tool::method_for(&tools, "vehicle_list").as_deref(),
            Some("vehicle/list")
        );
    }

    #[test]
    fn diagnosis_tools_offer_history_and_bounded_message_snapshots() {
        let tools = tools();
        for (name, method) in [
            ("daemon_about", "daemon/about"),
            ("mavlink_inspect", "mavlink/inspect"),
            ("mavlink_history", "mavlink/history"),
            ("router_events", "router/events"),
            ("link_diagnose", "link/diagnose"),
            ("socket_inspect", "socket/inspect"),
            ("daemon_logs", "daemon/logs"),
            ("telemetry_window", "telemetry/window"),
            ("link_list", "link/list"),
        ] {
            assert_eq!(Tool::method_for(&tools, name).as_deref(), Some(method));
        }
        let messages = tools
            .iter()
            .find(|tool| tool.name == "vehicle_messages")
            .unwrap();
        assert!(messages.parameters["properties"].get("subscribe").is_none());
        assert!(messages.description.contains("bounded snapshot"));
        assert!(!messages.description.contains("`vehicle/message` follows"));
        let history = tools
            .iter()
            .find(|tool| tool.name == "mavlink_history")
            .unwrap();
        for property in [
            "vehicleId",
            "direction",
            "id",
            "before",
            "limit",
            "linkId",
            "fromUs",
            "toUs",
            "allMessages",
            "includeRaw",
        ] {
            assert!(history.parameters["properties"].get(property).is_some());
        }
    }
}

#[cfg(test)]
mod deferred_tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    /// ADR-0035 M5: asked to change a vehicle's model, the tool that does is found.
    #[test]
    fn a_vehicles_own_model_is_found_by_words() {
        let found: Vec<String> = find_deferred("change my plane's 3D model to a glb file", 3)
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert!(found.contains(&"vehicle_look".to_owned()), "{found:?}");
    }

    #[test]
    fn deferred_tools_are_agent_tools_found_by_words_and_never_named_in_the_instructions() {
        let all: Vec<String> = tools().into_iter().map(|t| t.method).collect();
        for method in DEFERRED_METHODS {
            assert!(
                all.contains(&(*method).to_owned()),
                "{method} is not an agent tool"
            );
            assert!(
                !crate::turn::INSTRUCTIONS.contains(&tool_name(method)),
                "{method} is named in the instructions but not sent"
            );
        }
        let sent = model_tools();
        assert!(
            sent.iter()
                .all(|t| !DEFERRED_METHODS.contains(&t.method.as_str()))
        );
        assert!(sent.iter().any(|t| t.name == TOOL_CALL));
        let found: Vec<String> = find_deferred("upload a plan", 5)
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(
            found.first().map(String::as_str),
            Some("plan_upload"),
            "{found:?}"
        );
        let history = find_deferred("mission history", 3);
        assert_eq!(history[0].name, "mission_history");
    }

    /// Jobs a person asks for by name are found when asked for, and their
    /// words are not sent on every turn (maintainer, 2026-11-15): writing a
    /// checklist, adding or assigning a camera, and the practice exercises.
    #[test]
    fn checklist_camera_and_practice_tools_are_found_when_asked_for() {
        let sent = model_tools();
        for (query, expected) in [
            ("add a step to the checklist", "checklist_update"),
            ("change the preflight checklist", "checklist_update"),
            ("add a camera", "video_add"),
            ("assign the camera to a vehicle", "video_assign"),
            ("practice exercises", "practice_list"),
            ("start a practice exercise", "practice_start"),
            ("practice attempt", "practice_status"),
        ] {
            let found: Vec<String> = find_deferred(query, 5)
                .into_iter()
                .map(|t| t.name)
                .collect();
            assert!(found.contains(&expected.to_owned()), "{query}: {found:?}");
            assert!(
                sent.iter().all(|t| t.name != expected),
                "{expected} is sent"
            );
        }
        // The instructions and the search tool say they are there to find.
        let instructions = crate::turn::INSTRUCTIONS;
        assert!(instructions.contains(
            "practice exercises, adding and assigning cameras, writing a vehicle's checklist and writing plugins"
        ));
        assert!(instructions.contains("find the practice tools with tool_search"));
        assert!(instructions.contains("with the camera tools tool_search finds"));
        let search = sent.iter().find(|t| t.name == "tool_search").unwrap();
        assert!(
            search.description.contains("practice, cameras"),
            "{}",
            search.description
        );
        assert!(
            search.description.contains("checklists and plugin writing"),
            "{}",
            search.description
        );
        // What stays: reading the checks and the cameras, and running an
        // installed plugin.
        for kept in [
            "preflight_state",
            "video_list",
            "plugin_call",
            "tool_search",
            TOOL_CALL,
        ] {
            assert!(sent.iter().any(|t| t.name == kept), "{kept}");
        }
    }

    /// FB-214: asked to move a simulated vehicle home, put it back or reset
    /// it, the agent finds Start again.
    #[test]
    fn start_again_is_found_to_move_a_simulated_vehicle_home() {
        for query in [
            "teleport the simulated vehicle to its home position",
            "move my simulated plane home",
            "put the simulated plane back",
            "reset the simulation",
        ] {
            let found: Vec<String> = find_deferred(query, 3)
                .into_iter()
                .map(|t| t.name)
                .collect();
            assert!(
                found.contains(&"sim_restart".to_owned()),
                "{query}: {found:?}"
            );
        }
    }
}
