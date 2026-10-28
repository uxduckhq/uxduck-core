//! Product analytics events (ADR-0013 P11, ADR-0016): names and small
//! enumerated properties, never free text, coordinates, names or vehicle
//! identifiers. Adding an event is a reviewed change here, and the app's
//! TypeScript is generated from it.
//!
//! Property names are `snake_case`, as `PostHog` shows them, unlike the rest
//! of the wire.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Where the app runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    /// A browser tab: the PWA, or the page an installed daemon serves.
    Browser,
    /// UXDuck Desktop, the Electron shell.
    Desktop,
}

/// Whether an installed UXDuck serves the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DaemonPresence {
    None,
    Installed,
}

/// Provenance for aggregate usage, not the safety classification of a vehicle.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AnalyticsVehicleKind {
    Real,
    Simulated,
    Unknown,
}

/// Confirmed work, never a button click or an automatic demo action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EngagementTask {
    ParameterWritten,
    MissionUploaded,
    PlanVerified,
    PracticePassed,
    VehicleCommand,
}

/// Engine-confirmed outcomes, consumed once across browser windows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EngagementUsage {
    pub events: Vec<AppEvent>,
}

/// Observed vehicle-time deltas, with no vehicle identity or flight content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FlightTimeDelta {
    pub vehicle_kind: AnalyticsVehicleKind,
    #[schemars(schema_with = "crate::safe::integer")]
    pub armed_ms: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub airborne_ms: u64,
}

/// Atomically consumes the browser daemon's aggregate counters across tabs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FlightTimeUsage {
    pub reports: Vec<FlightTimeDelta>,
}

/// Active time has its own optional RPC so older installed daemons can
/// refuse it with method-not-found without changing analytics/track's wire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AnalyticsActiveTimeParams {
    pub active_ms: u32,
    pub surface: Surface,
    pub daemon: DaemonPresence,
}

/// Foreground input interval for a selected simulator. IDs and timestamps are
/// local RPC context only; only the deduplicated duration enters analytics.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AnalyticsEngagedSimTimeParams {
    pub vehicle_id: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub start_ms: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub end_ms: u64,
    pub surface: Surface,
}

/// Browser workers return the accepted delta to the originating page only.
/// Installed daemons deliver directly and return zero to avoid a second sender.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EngagedSimTimeResult {
    pub engaged_ms: u32,
}

/// A visible map's trail choice, or differing choices across visible 3D maps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PerformanceTrails {
    Off,
    Recent,
    Whole,
    Mixed,
}

/// One bounded renderer-performance window; counts and settings, never flight values or IDs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AnalyticsPerformanceParams {
    pub window_ms: u32,
    pub long_tasks: u32,
    pub blocking_ms: u32,
    pub max_task_ms: u32,
    pub max_input_delay_ms: u32,
    pub controller_gaps: u32,
    pub max_controller_gap_ms: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heap_mb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_percent: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_memory_mb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renderer_cpu_percent: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_cpu_percent: Option<u32>,
    /// Electron CPU averages cover this interval, which can differ from `window_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_window_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_sample_age_ms: Option<u32>,
    /// Process CPU is app-wide; map context belongs to the reporting window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_window_count: Option<u32>,
    /// Visible engine instances, including decorative globes, which are also counted separately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map_2d_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map_3d_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub globe_count: Option<u32>,
    /// Largest vehicle count in a visible map; the same fleet in two views is not added twice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map_vehicle_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map_3d_trails: Option<PerformanceTrails>,
    /// Rounded actual render events/s per visible engine instance; idle maps can report zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map_2d_fps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map_3d_fps: Option<u32>,
    /// The renderer's commit, or dev, independently of the connected daemon's release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_build: Option<String>,
    pub surface: Surface,
    pub daemon: DaemonPresence,
}

/// Observed reasons for losing browser input, never a guess about USB or Bluetooth.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ControllerInterruptionReason {
    ReportTimeout,
    PageHidden,
    #[default]
    DeviceMissing,
    ReadError,
    ReaderStopped,
    PageClosed,
}

/// One worker-observed interruption, grouped by reporting page. No device or vehicle IDs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ControllerInputInterruption {
    /// The reporting engine's commit, distinct from the page displaying it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_commit")]
    pub engine_build: Option<String>,
    pub reason: ControllerInterruptionReason,
    /// Worker's wall clock at the cutoff, before a stalled renderer can receive this.
    #[schemars(schema_with = "crate::safe::integer")]
    pub interrupted_at_ms: u64,
    pub report_gap_ms: u32,
    pub worker_tick_gap_ms: u32,
    pub affected_controllers: u32,
    pub claimed_controllers: u32,
    pub driving_controllers: u32,
}

/// A download's platform, as the website's `/download/<platform>` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DownloadPlatform {
    Macos,
    Windows,
    Linux,
    LinuxArm64,
    /// "All downloads": the platform was not chosen here.
    Any,
}

/// Where a download button was pressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DownloadFrom {
    /// The home view with nothing installed.
    Home,
    /// The warning at the top of the page when UXDuck cannot be reached.
    Banner,
    Settings,
}

/// A part of the app, counted when it is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    Map,
    /// The 3D map tab (maintainer, 2026-10-04), opened from the map's "3D"
    /// button or from "+".
    #[serde(rename = "map_3d")]
    Map3d,
    /// The Globe tab (maintainer, 2026-10-04): the home view's pixel Earth
    /// as a tab of its own, opened from "+" or the Layouts menu.
    Globe,
    VehiclePanel,
    Vehicles,
    Parameters,
    Mission,
    Controllers,
    Search,
    Plugins,
    ChatManager,
    /// A vehicle's history (S69).
    History,
    /// The Camera tab (S75): a camera's video (S38) or a simulation's 3D
    /// world (S35). Counted as `view3d` and `video` before S75.
    Camera,
    /// The classic flight instruments (S43).
    Instruments,
    /// The modern flight display (S77).
    FlightDisplay,
    /// The Get started tab, reopened from the "+" (maintainer, 2026-10-03).
    GetStarted,
    /// The MAVLink inspector tab.
    MavlinkInspector,
    /// The Settings tab (S87), from "+", the Layouts menu or the page's
    /// "Open as a tab".
    Settings,
    /// The Practice tab (S100).
    Practice,
    /// The Places, Flights and Missions pages as tabs.
    Places,
    Flights,
    Missions,
    /// Ducky, the duck tab found by typing "duck" in the command palette
    /// (maintainer, 2026-10-12).
    Ducky,
    /// The Airspace tab: other aircraft and their sources (S168).
    Airspace,
}

/// A setting that was changed; never its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SettingName {
    Units,
    CoordinateFormat,
    /// The altitude the instruments show (maintainer, 2026-10-05).
    AltitudeFrame,
    Basemap,
    MapEngine,
    PlaceSearch,
    UsageAnalytics,
    Conversations,
    FlightData,
    ClosePolicy,
    AutoDelete,
    /// When cameras are recorded (S40).
    VideoRecording,
    /// A keyboard shortcut was changed, cleared or reset (S51).
    Shortcuts,
    /// A skipped confirmation was reset (S53).
    Confirmations,
    /// Google's photorealistic 3D tiles in the 3D map, on or off for this browser (ADR-0014 M10).
    Photorealistic,
}

/// One frame of an error's stack (ADR-0016 O13): a file within the app or
/// the workspace, never a path outside it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct ErrorFrame {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    /// A script's address, or a source file relative to the workspace.
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    /// UXDuck's own code, not a dependency's.
    pub in_app: bool,
}

/// What the app sends: over `analytics/track` to an installed daemon, or
/// itself with `posthog-js` when there is none (ADR-0016 O1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "event",
    rename_all = "snake_case",
    rename_all_fields = "snake_case"
)]
pub enum AppEvent {
    /// Starts an observable engagement cohort under the sender's current identity.
    EngagementStarted {
        surface: Surface,
        daemon: DaemonPresence,
    },
    /// A confirmed outcome of a person/client's request, not internal setup.
    TaskCompleted {
        task_type: EngagementTask,
        vehicle_kind: AnalyticsVehicleKind,
    },
    /// The corresponding request ended unsuccessfully; no error text or values.
    TaskFailed {
        task_type: EngagementTask,
        vehicle_kind: AnalyticsVehicleKind,
    },
    /// Browser-daemon durations, drained once across all of its pages.
    VehicleFlightTime {
        #[serde(flatten)]
        delta: FlightTimeDelta,
    },
    /// A page load, once the app knows whether UXDuck is installed.
    AppOpened {
        surface: Surface,
        daemon: DaemonPresence,
    },
    /// Foreground time since the last report, with input in the last minute.
    /// A delta, never a cumulative counter or a visit identifier.
    AppActiveTime {
        active_ms: u32,
        surface: Surface,
        daemon: DaemonPresence,
    },
    /// Foreground input with a selected online simulator, deduplicated across
    /// windows by the engine. Includes ground setup; not manual flight proof.
    EngagedSimTime {
        engaged_ms: u32,
        surface: Surface,
        daemon: DaemonPresence,
    },
    /// Aggregate responsiveness and memory, only when usage analytics is allowed.
    AppPerformance {
        #[serde(flatten)]
        sample: AnalyticsPerformanceParams,
    },
    /// Actual browser input loss, captured before claims are released, with renderer context.
    ControllerInputInterrupted {
        #[serde(flatten)]
        interruption: ControllerInputInterruption,
        visible: bool,
        delivery_delay_ms: u32,
        #[serde(flatten)]
        sample: AnalyticsPerformanceParams,
    },
    /// Live telemetry was on screen, once per page load.
    TelemetryShown,
    /// The once-only feedback invitation actually appeared on screen.
    FeedbackPromptShown {
        surface: Surface,
    },
    /// A feedback form became available, including opening its address directly.
    FeedbackOpened {
        surface: Surface,
    },
    /// A report was accepted by the API or durably queued by the installed daemon.
    /// No report identifier, text or attachments enter analytics.
    FeedbackSubmitted {
        surface: Surface,
        kind: crate::dto::feedback::FeedbackKind,
        status: crate::dto::feedback::FeedbackStatus,
    },
    FeatureUsed {
        feature: Feature,
    },
    /// An error the person saw, by its code, such as `map-unavailable`.
    ErrorShown {
        code: String,
    },
    SettingChanged {
        setting: SettingName,
    },
    DownloadClicked {
        platform: DownloadPlatform,
        from: DownloadFrom,
    },
    /// A simulated vehicle started in the browser (S20, the funnel's step 3).
    SimulationStarted {
        vehicle: crate::dto::link::SimVehicle,
    },
    /// A tile added to or removed from a type's Telemetry layout (S73): what
    /// people want each type to show. Only with analytics allowed; a page
    /// sends it only after "Yes".
    TelemetryLayoutChanged {
        vehicle_type: crate::dto::operator::VehicleClass,
        /// The telemetry item's key, such as `speed.air_mps`.
        item: String,
        change: LayoutChange,
    },
    /// An error (ADR-0016 O13), sent as `PostHog`'s `$exception`: its type,
    /// a known code instead of its message, and its stack.
    Exception {
        /// Such as `TypeError`.
        error_type: String,
        /// A known code, such as `uncaught` or `map-unavailable`.
        code: String,
        handled: bool,
        frames: Vec<ErrorFrame>,
    },
}

/// What happened to a tile (S73).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LayoutChange {
    Added,
    Removed,
}

/// How a daemon was installed, as far as it can tell from where it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PackageKind {
    Pkg,
    Msi,
    Deb,
    /// Inside UXDuck Desktop.
    Desktop,
    /// Unpacked from an archive, or anywhere else.
    Archive,
    /// A development build.
    Dev,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LinkType {
    Udp,
    Serial,
    Tcp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Answered,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    AllowOnce,
    AllowConnection,
    AllowAlways,
    Deny,
    /// Nobody answered in time, or the asker went away.
    Expired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CommandName {
    Arm,
    Disarm,
    SetMode,
    Rtl,
    Land,
    Reboot,
    GoTo,
    Long,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CommandOutcome {
    Accepted,
    Rejected,
    Timeout,
    NotSent,
    Interrupted,
}

/// What the daemon sends about itself, from its outbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "event",
    rename_all = "snake_case",
    rename_all_fields = "snake_case"
)]
pub enum DaemonEvent {
    /// Observed vehicle time; no vehicle IDs, positions or flight logs.
    VehicleFlightTime {
        #[serde(flatten)]
        delta: FlightTimeDelta,
    },
    /// The first start with a new data directory (ADR-0016 O6).
    InstallCompleted {
        /// `macos`, `windows` or `linux`.
        platform: String,
        /// `aarch64` or `x86_64`.
        arch: String,
        version: String,
        package: PackageKind,
    },
    /// A browser paired with this install for the first time.
    DaemonPaired,
    /// A vehicle was heard for the first time.
    VehicleConnected {
        /// Lower case, such as `ardupilot`.
        autopilot: String,
        /// Lower case, such as `copter`.
        vehicle_type: String,
        link: LinkType,
    },
    /// An agent turn ended: what it used and how it went, never its words.
    AgentTurn {
        model: String,
        /// Tool calls in the turn.
        tools: u32,
        outcome: TurnOutcome,
        ms: u64,
        /// Model responses: the first and one per tool round.
        #[serde(default)]
        rounds: u32,
        #[serde(default)]
        tool_errors: u32,
        /// Tokens read at full price, written, and read from the cache.
        #[serde(default)]
        input_tokens: u64,
        #[serde(default)]
        output_tokens: u64,
        #[serde(default)]
        cache_read_tokens: u64,
        #[serde(default)]
        cache_write_tokens: u64,
        /// From sending to the first streamed text or tool call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        first_token_ms: Option<u64>,
        /// Requests sent again after a busy answer or a dropped connection.
        #[serde(default)]
        retries: u32,
        /// Another model answered because this one stayed busy.
        #[serde(default)]
        fell_back: bool,
        /// The chat was summarised before this turn.
        #[serde(default)]
        compacted: bool,
        /// Earlier reasoning the provider set aside; more than zero is a bug.
        #[serde(default)]
        thinking_dropped: u32,
    },
    ApprovalResolved {
        decision: ApprovalDecision,
    },
    /// A practice exercise started (S100, ADR-0024 X7): its id only.
    PracticeStarted {
        exercise: String,
    },
    /// A practice attempt ended: its exercise and `passed`, `failed` or
    /// `given-up`; nothing about the flight.
    PracticeEnded {
        exercise: String,
        result: String,
    },
    /// A command ended, whoever sent it.
    CommandSent {
        command: CommandName,
        outcome: CommandOutcome,
    },
    /// A panic, or an error logged with a code (ADR-0016 O13), sent as
    /// `$exception` with Rust frames.
    Exception {
        /// `panic` or `error`.
        error_type: String,
        code: String,
        frames: Vec<ErrorFrame>,
    },
}

/// Either kind, as the outbox keeps it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum AnalyticsEvent {
    App(AppEvent),
    Daemon(DaemonEvent),
}

/// Joins a browser's earlier events to this install, when that browser
/// agreed to a persistent id (ADR-0016 O4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AnalyticsAliasParams {
    /// The browser's `PostHog` distinct id.
    pub browser_id: String,
}

/// Whether an error code or a model name is safe to send: short, lower
/// case, digits, dots and dashes only, so it cannot carry free text.
#[must_use]
pub fn is_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 48
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
}

/// Whether an error type is safe to send: a short name such as `TypeError`.
#[must_use]
pub fn is_error_type(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 60
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':'))
}

/// `PostHog`'s `$exception` properties (ADR-0016 O13): the code stands in for
/// the message, which could hold paths, names or values.
fn exception(
    error_type: &str,
    code: &str,
    handled: bool,
    frames: &[ErrorFrame],
    rust: bool,
) -> serde_json::Map<String, serde_json::Value> {
    let frames: Vec<serde_json::Value> = frames
        .iter()
        .map(|f| {
            let mut frame = serde_json::json!({
                "platform": if rust { "custom" } else { "web:javascript" },
                "filename": f.file,
                "function": f.function,
                "lineno": f.line,
                "colno": f.column,
                "in_app": f.in_app,
            });
            if rust {
                frame["lang"] = "rust".into();
                frame["resolved"] = true.into();
            }
            frame
        })
        .collect();
    let mut properties = serde_json::Map::new();
    properties.insert(
        "$exception_list".into(),
        serde_json::json!([{
            "type": error_type,
            "value": code,
            "mechanism": { "handled": handled, "synthetic": false },
            "stacktrace": { "type": "raw", "frames": frames },
        }]),
    );
    properties.insert("$exception_level".into(), "error".into());
    properties.insert("code".into(), code.into());
    properties
}

impl AnalyticsEvent {
    /// The event's name and its properties, as `PostHog` takes them.
    #[must_use]
    pub fn split(&self) -> (String, serde_json::Map<String, serde_json::Value>) {
        match self {
            Self::App(AppEvent::Exception {
                error_type,
                code,
                handled,
                frames,
            }) => {
                return (
                    "$exception".into(),
                    exception(error_type, code, *handled, frames, false),
                );
            }
            Self::Daemon(DaemonEvent::Exception {
                error_type,
                code,
                frames,
            }) => {
                return (
                    "$exception".into(),
                    exception(error_type, code, false, frames, true),
                );
            }
            _ => {}
        }
        let mut properties = match serde_json::to_value(self) {
            Ok(serde_json::Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        };
        let name = match properties.remove("event") {
            Some(serde_json::Value::String(name)) => name,
            _ => "unknown".to_owned(),
        };
        (name, properties)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_split_into_a_name_and_snake_case_properties() {
        let (name, properties) = AnalyticsEvent::App(AppEvent::DownloadClicked {
            platform: DownloadPlatform::LinuxArm64,
            from: DownloadFrom::Home,
        })
        .split();
        assert_eq!(name, "download_clicked");
        assert_eq!(properties["platform"], "linux_arm64");
        assert_eq!(properties["from"], "home");

        let (name, properties) = AnalyticsEvent::Daemon(DaemonEvent::VehicleConnected {
            autopilot: "ardupilot".into(),
            vehicle_type: "copter".into(),
            link: LinkType::Udp,
        })
        .split();
        assert_eq!(name, "vehicle_connected");
        assert_eq!(properties["vehicle_type"], "copter");

        let (name, properties) = AnalyticsEvent::App(AppEvent::TelemetryShown).split();
        assert_eq!((name.as_str(), properties.len()), ("telemetry_shown", 0));
    }

    #[test]
    fn exceptions_go_as_posthogs_exception_with_the_code_for_a_message() {
        let (name, properties) = AnalyticsEvent::Daemon(DaemonEvent::Exception {
            error_type: "panic".into(),
            code: "panic".into(),
            frames: vec![ErrorFrame {
                function: Some("uxduck::daemon::run".into()),
                file: "crates/uxduck/src/daemon/mod.rs".into(),
                line: Some(12),
                column: Some(5),
                in_app: true,
            }],
        })
        .split();
        assert_eq!(name, "$exception");
        let first = &properties["$exception_list"][0];
        assert_eq!(first["type"], "panic");
        assert_eq!(first["value"], "panic");
        assert_eq!(first["mechanism"]["handled"], false);
        let frame = &first["stacktrace"]["frames"][0];
        assert_eq!(frame["filename"], "crates/uxduck/src/daemon/mod.rs");
        assert_eq!(frame["lang"], "rust");
        assert_eq!(frame["lineno"], 12);
        assert!(is_error_type("TypeError"));
        assert!(!is_error_type("Type Error: x"));
    }

    #[test]
    fn tokens_carry_no_free_text() {
        assert!(is_token("map-unavailable"));
        assert!(is_token("gpt-6-sol"));
        assert!(!is_token("Where is my drone?"));
        assert!(!is_token(""));
        assert!(!is_token(&"a".repeat(49)));
    }
}
