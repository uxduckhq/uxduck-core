//! Product analytics (ADR-0013 P10, P11; ADR-0016): the app's events over
//! `analytics/track`, the daemon's own, and the usage analytics switch. The
//! switch is checked here, so nothing reaches the port while it is off.

use serde_json::{Value, json};
use uxduck_contracts::dto::analytics::{
    AnalyticsAliasParams, AnalyticsEvent, AnalyticsPerformanceParams, AppEvent, CommandName,
    CommandOutcome, DaemonEvent, EngagementTask, EngagementUsage, ErrorFrame, LinkType,
    TurnOutcome, is_error_type, is_token,
};
use uxduck_contracts::dto::trace::Transport;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::command::{Command, ReceiptStatus};
use uxduck_core::domain::link::LinkKind;
use uxduck_core::domain::settings::Settings;
use uxduck_core::domain::thread::ItemKind;
use uxduck_core::events::{Actor, Change};

use crate::runtime::AnalyticsPort;
use crate::{ConnectionId, Engine};

fn performance_valid(sample: &AnalyticsPerformanceParams) -> bool {
    (1..=60_000).contains(&sample.window_ms)
        && [
            sample.long_tasks,
            sample.blocking_ms,
            sample.max_task_ms,
            sample.max_input_delay_ms,
            sample.controller_gaps,
            sample.max_controller_gap_ms,
        ]
        .iter()
        .all(|&value| value <= 60_000)
        && sample.heap_mb.is_none_or(|mb| mb <= 1_048_576)
        && sample.cpu_percent.is_none_or(|cpu| cpu <= 10_000)
        && sample.process_memory_mb.is_none_or(|mb| mb <= 1_048_576)
        && sample.renderer_cpu_percent.is_none_or(|cpu| cpu <= 10_000)
        && sample.gpu_cpu_percent.is_none_or(|cpu| cpu <= 10_000)
        && sample.cpu_window_ms.is_none_or(|ms| ms <= 86_400_000)
        && sample.cpu_sample_age_ms.is_none_or(|ms| ms <= 86_400_000)
        && [
            sample.app_window_count,
            sample.map_2d_count,
            sample.map_3d_count,
            sample.globe_count,
            sample.map_vehicle_count,
        ]
        .iter()
        .all(|count| count.is_none_or(|count| count <= 10_000))
        && [sample.map_2d_fps, sample.map_3d_fps]
            .iter()
            .all(|fps| fps.is_none_or(|fps| fps <= 1000))
        && sample.ui_build.as_deref().is_none_or(is_token)
}

fn validate_event(event: &AppEvent) -> Result<(), ApiError> {
    if let AppEvent::ControllerInputInterrupted {
        interruption,
        sample,
        delivery_delay_ms,
        ..
    } = event
        && (!performance_valid(sample)
            || interruption.engine_build.as_deref().is_some_and(|build| {
                !(7..=40).contains(&build.len())
                    || !build
                        .bytes()
                        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            })
            || interruption.report_gap_ms > 60_000
            || interruption.worker_tick_gap_ms > 60_000
            || interruption.interrupted_at_ms > 9_007_199_254_740_991
            || *delivery_delay_ms > 60_000
            || !(1..=64).contains(&interruption.affected_controllers)
            || interruption.claimed_controllers > interruption.affected_controllers
            || interruption.driving_controllers > interruption.claimed_controllers)
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "controller interruptions must contain bounded timings and controller counts",
        ));
    }
    if let AppEvent::AppPerformance { sample } = event
        && !performance_valid(sample)
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "performance samples must be bounded aggregates over at most one minute",
        ));
    }

    if let AppEvent::AppActiveTime { active_ms, .. } = event
        && !(1..=60_000).contains(active_ms)
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "active time is a delta of 1 to 60000 milliseconds",
        ));
    }
    if let AppEvent::ErrorShown { code } = event
        && !is_token(code)
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "an error code is lower case letters, digits and dashes",
        ));
    }
    if let AppEvent::Exception {
        error_type,
        code,
        frames,
        ..
    } = event
        && !(is_error_type(error_type) && is_token(code) && frames_ok(frames))
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "an exception is a type, a known code and up to 50 frames, with no text",
        ));
    }
    if let AppEvent::TelemetryLayoutChanged { item, .. } = event
        && !is_item_key(item)
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "an item is a telemetry key, such as speed.air_mps",
        ));
    }
    Ok(())
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    operator: bool,
) -> Result<Value, ApiError> {
    if !operator {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "only an operator client may send analytics",
        ));
    }
    let request = match request {
        ClientRequest::AnalyticsFeedback(event) => ClientRequest::AnalyticsTrack(event),
        ClientRequest::AnalyticsPerformance(sample) => {
            ClientRequest::AnalyticsTrack(AppEvent::AppPerformance { sample })
        }
        ClientRequest::AnalyticsActiveTime(params) => {
            ClientRequest::AnalyticsTrack(AppEvent::AppActiveTime {
                active_ms: params.active_ms,
                surface: params.surface,
                daemon: params.daemon,
            })
        }
        other => other,
    };
    match request {
        ClientRequest::AnalyticsTakeFlightTime(_) => Ok(json!(engine.take_flight_usage())),
        ClientRequest::AnalyticsTakeEngagement(_) => {
            let events = std::mem::take(&mut engine.engagement);
            Ok(json!(EngagementUsage {
                events: if engine.state.settings.usage_analytics {
                    events
                } else {
                    Vec::new()
                }
            }))
        }
        ClientRequest::AnalyticsTrack(event) => {
            validate_event(&event)?;
            if engine.state.settings.usage_analytics {
                // A page over the WebSocket is a browser tab; the desktop
                // app's page comes over the local socket.
                let surface = match engine.connections.get(&connection).map(|c| c.transport) {
                    Some(Transport::WebSocket) => "browser",
                    _ => "desktop",
                };
                if let AppEvent::AppOpened {
                    surface: app_surface,
                    daemon,
                } = &event
                {
                    engine.analytics.track(
                        AnalyticsEvent::App(AppEvent::EngagementStarted {
                            surface: *app_surface,
                            daemon: *daemon,
                        }),
                        surface,
                    );
                }
                engine.analytics.track(AnalyticsEvent::App(event), surface);
            }
            Ok(json!({}))
        }
        ClientRequest::AnalyticsAlias(AnalyticsAliasParams { browser_id }) => {
            let valid = !browser_id.is_empty()
                && browser_id.len() <= 128
                && browser_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
            if !valid {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "not a browser's analytics id",
                ));
            }
            if engine.state.settings.usage_analytics {
                engine.analytics.alias(&browser_id);
            }
            Ok(json!({}))
        }
        _ => Err(ApiError::new(
            ErrorKind::Internal,
            "not an analytics method",
        )),
    }
}

impl Engine {
    /// Only called after an authoritative acknowledgement or judged result.
    /// Browser workers have no outbox: the existing page sender drains once.
    pub(crate) fn track_task(
        &mut self,
        vehicle_id: &str,
        task_type: EngagementTask,
        completed: bool,
    ) {
        if !self.state.settings.usage_analytics {
            return;
        }
        let vehicle_kind = self.analytics_vehicle_kind(vehicle_id);
        let event = if completed {
            AppEvent::TaskCompleted {
                task_type,
                vehicle_kind,
            }
        } else {
            AppEvent::TaskFailed {
                task_type,
                vehicle_kind,
            }
        };
        if self.config.capabilities.runs_in == uxduck_contracts::dto::initialize::RunsIn::Browser {
            if self.engagement.len() == 100 {
                self.engagement.remove(0);
            }
            self.engagement.push(event);
        } else {
            self.analytics.track(AnalyticsEvent::App(event), "daemon");
        }
    }

    /// Where analytics go. The port is told at once whether the switch is on.
    pub fn set_analytics(&mut self, mut port: Box<dyn AnalyticsPort>) {
        port.set_enabled(self.state.settings.usage_analytics);
        self.analytics = port;
    }

    /// Queues one of the daemon's own events, if usage analytics are on.
    pub fn track(&mut self, event: DaemonEvent) {
        if self.state.settings.usage_analytics {
            self.analytics
                .track(AnalyticsEvent::Daemon(event), "daemon");
        }
    }

    /// Switches usage analytics off, as `--no-analytics` does at start, so
    /// an install can begin with nothing sent (ADR-0016 O15).
    pub fn turn_analytics_off(&mut self) {
        self.engagement.clear();
        self.flight_usage = crate::flight_usage::FlightUsage::default();
        if !self.state.settings.usage_analytics {
            return;
        }
        let mut settings = self.state.settings.clone();
        settings.usage_analytics = false;
        settings.version += 1;
        self.commit_as(
            Actor {
                client_id: "daemon".into(),
                operator_id: None,
            },
            Change::SettingsChanged { settings },
        );
        self.analytics.set_enabled(false);
    }

    /// After a settings change: the outbox forgets the install id when the
    /// switch goes off, and makes one when it comes on.
    pub(crate) fn analytics_settings_changed(&mut self, before: &Settings) {
        let on = self.state.settings.usage_analytics;
        if before.usage_analytics != on {
            self.engagement.clear();
            self.flight_usage = crate::flight_usage::FlightUsage::default();
            self.analytics.set_enabled(on);
        }
    }

    /// A vehicle heard for the first time.
    pub(crate) fn track_vehicle(&mut self, autopilot: &str, vehicle_type: &str, link_id: &str) {
        let link = match self.state.links.get(link_id).map(|l| &l.kind) {
            Some(LinkKind::Serial { .. }) => LinkType::Serial,
            Some(LinkKind::TcpClient { .. } | LinkKind::TcpServer { .. }) => LinkType::Tcp,
            _ => LinkType::Udp,
        };
        self.track(DaemonEvent::VehicleConnected {
            autopilot: token_or_other(autopilot),
            vehicle_type: token_or_other(vehicle_type),
            link,
        });
    }

    /// What a turn used, from the agent host; sent with the turn's event.
    /// Call before [`Engine::agent_turn_finish`].
    pub fn agent_turn_stats(&mut self, turn: u64, stats: crate::runtime::TurnStats) {
        if self.turn_started.contains_key(&turn) {
            self.turn_stats.insert(turn, stats);
        }
    }

    pub(crate) fn turn_began(&mut self, turn: u64) {
        let now = self.clock.now_us();
        self.turn_started.insert(turn, now);
    }

    /// A turn ended: its model, tool calls, outcome and length, never its
    /// words.
    pub(crate) fn turn_ended(&mut self, thread_id: &str, turn: u64, cancelled: bool) {
        let Some(started) = self.turn_started.remove(&turn) else {
            return;
        };
        let now = self.clock.now_us();
        let items = self.state.items_of(thread_id);
        let during = items.iter().filter(|item| item.created_at_us >= started);
        let (mut tools, mut failed) = (0u32, false);
        for item in during {
            match item.kind {
                ItemKind::ToolCall => tools += 1,
                ItemKind::Error => failed = true,
                _ => {}
            }
        }
        let model = self
            .state
            .threads
            .get(thread_id)
            .map_or_else(|| "other".into(), |t| token_or_other(&t.model));
        let outcome = if cancelled {
            TurnOutcome::Cancelled
        } else if failed {
            TurnOutcome::Failed
        } else {
            TurnOutcome::Answered
        };
        let stats = self.turn_stats.remove(&turn).unwrap_or_default();
        self.track(DaemonEvent::AgentTurn {
            model,
            tools,
            outcome,
            ms: now.saturating_sub(started) / 1000,
            rounds: stats.rounds,
            tool_errors: stats.tool_errors,
            input_tokens: stats.input_tokens,
            output_tokens: stats.output_tokens,
            cache_read_tokens: stats.cache_read_tokens,
            cache_write_tokens: stats.cache_write_tokens,
            first_token_ms: stats.first_token_ms,
            retries: stats.retries,
            fell_back: stats.fell_back,
            compacted: stats.compacted,
            thinking_dropped: stats.thinking_dropped,
        });
    }

    /// A command ended, sent or not.
    pub(crate) fn track_command(&mut self, command: &Command, status: ReceiptStatus) {
        let outcome = match status {
            ReceiptStatus::Sent => return,
            ReceiptStatus::Accepted => CommandOutcome::Accepted,
            ReceiptStatus::Rejected => CommandOutcome::Rejected,
            ReceiptStatus::Timeout => CommandOutcome::Timeout,
            ReceiptStatus::NotSent => CommandOutcome::NotSent,
            ReceiptStatus::Interrupted => CommandOutcome::Interrupted,
        };
        let command = match command {
            Command::Arm => CommandName::Arm,
            Command::Disarm => CommandName::Disarm,
            Command::SetMode { .. } => CommandName::SetMode,
            Command::Rtl => CommandName::Rtl,
            Command::Land => CommandName::Land,
            Command::Reboot => CommandName::Reboot,
            Command::GoTo { .. } => CommandName::GoTo,
            Command::Long { .. } => CommandName::Long,
        };
        self.track(DaemonEvent::CommandSent { command, outcome });
    }
}

/// Frames an app may send: at most 50, short, and no local file paths,
/// which could hold a user name.
fn frames_ok(frames: &[ErrorFrame]) -> bool {
    frames.len() <= 50
        && frames.iter().all(|f| {
            let local =
                f.file.starts_with('/') || f.file.contains(":\\") || f.file.starts_with("file:");
            f.file.len() <= 300
                && !local
                && f.function.as_ref().is_none_or(|name| name.len() <= 200)
        })
}

/// A name from a fixed set (an autopilot, a model) as a short token, or
/// `other` when it does not look like one.
fn token_or_other(value: &str) -> String {
    let token = value.trim().to_lowercase().replace([' ', '_'], "-");
    if is_token(&token) {
        token
    } else {
        "other".into()
    }
}

/// A telemetry key such as `speed.air_mps`: never free text.
fn is_item_key(key: &str) -> bool {
    key.len() <= 64
        && key.contains('.')
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_'))
}

#[cfg(test)]
mod tests {
    use super::token_or_other;

    #[test]
    fn names_become_tokens() {
        assert_eq!(token_or_other("ArduPilot"), "ardupilot");
        assert_eq!(token_or_other("Fixed wing"), "fixed-wing");
        assert_eq!(token_or_other("gpt-6-sol"), "gpt-6-sol");
        assert_eq!(token_or_other("what is this?"), "other");
    }

    #[test]
    fn a_layout_change_names_a_telemetry_key_and_nothing_else() {
        assert!(super::is_item_key("speed.air_mps"));
        assert!(super::is_item_key("battery_2.cell_1_v"));
        assert!(!super::is_item_key("my rover's airspeed"));
        assert!(!super::is_item_key("speed"));
        assert!(!super::is_item_key(&"a.".repeat(40)));
    }

    #[test]
    fn performance_context_is_optional_bounded_and_survives_event_serialization() {
        use serde_json::json;
        use uxduck_contracts::dto::analytics::{AnalyticsPerformanceParams, AppEvent};

        let base = json!({
            "window_ms": 30000, "long_tasks": 0, "blocking_ms": 0, "max_task_ms": 0,
            "max_input_delay_ms": 0, "controller_gaps": 0, "max_controller_gap_ms": 0,
            "surface": "desktop", "daemon": "installed"
        });
        let old: AnalyticsPerformanceParams = serde_json::from_value(base.clone()).unwrap();
        assert!(super::performance_valid(&old));
        assert!(old.map_3d_count.is_none());

        let mut full = base;
        let fields = json!({
            "map_2d_count": 1, "map_3d_count": 1, "globe_count": 0, "map_vehicle_count": 10,
            "map_2d_fps": 60, "map_3d_fps": 30, "map_3d_trails": "off",
            "renderer_cpu_percent": 210, "gpu_cpu_percent": 35, "ui_build": "abcdef1",
            "cpu_window_ms": 30000, "cpu_sample_age_ms": 0, "app_window_count": 1
        });
        full.as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        let sample: AnalyticsPerformanceParams = serde_json::from_value(full.clone()).unwrap();
        assert!(super::performance_valid(&sample));
        let event = AppEvent::AppPerformance { sample };
        let encoded = serde_json::to_value(event).unwrap();
        assert_eq!(encoded["map_vehicle_count"], 10);
        assert_eq!(encoded["map_3d_fps"], 30);
        assert_eq!(encoded["renderer_cpu_percent"], 210);
        assert_eq!(encoded["map_3d_trails"], "off");

        for (field, value) in [
            ("map_2d_count", json!(10001)),
            ("map_3d_count", json!(10001)),
            ("globe_count", json!(10001)),
            ("map_vehicle_count", json!(10001)),
            ("map_2d_fps", json!(1001)),
            ("map_3d_fps", json!(1001)),
            ("renderer_cpu_percent", json!(10001)),
            ("gpu_cpu_percent", json!(10001)),
            ("cpu_window_ms", json!(86_400_001)),
            ("cpu_sample_age_ms", json!(86_400_001)),
            ("app_window_count", json!(10001)),
            ("ui_build", json!("a private path/with text")),
        ] {
            let mut invalid = full.clone();
            invalid[field] = value;
            let sample = serde_json::from_value(invalid).unwrap();
            assert!(
                !super::performance_valid(&sample),
                "accepted invalid {field}"
            );
        }
        full["map_3d_trails"] = json!("unrecognised");
        assert!(serde_json::from_value::<AnalyticsPerformanceParams>(full).is_err());
    }

    #[test]
    fn interruption_diagnostics_validate_counts_timings_and_the_reason_vocabulary() {
        use serde_json::json;
        use uxduck_contracts::dto::analytics::AppEvent;
        let request: serde_json::Value = serde_json::from_str(include_str!(
            "../../uxduck-contracts/fixtures/analytics_track.request.controller-interruption.json"
        ))
        .unwrap();
        let valid = request["params"].clone();
        let event = serde_json::from_value::<AppEvent>(valid.clone()).unwrap();
        assert!(super::validate_event(&event).is_ok());
        // Reports from older engines omit their build. New engines send only a commit.
        let mut with_build = valid.clone();
        with_build["engine_build"] = json!("abcdef1");
        let event = serde_json::from_value::<AppEvent>(with_build).unwrap();
        assert!(super::validate_event(&event).is_ok());
        for (field, value) in [
            ("engine_build", json!("private/device-name")),
            ("engine_build", json!("ABCDEF1")),
            ("engine_build", json!("abcdef")),
            ("engine_build", json!("a".repeat(41))),
            ("report_gap_ms", json!(60_001)),
            ("worker_tick_gap_ms", json!(60_001)),
            ("delivery_delay_ms", json!(60_001)),
            ("interrupted_at_ms", json!(9_007_199_254_740_992_u64)),
            ("affected_controllers", json!(0)),
            ("affected_controllers", json!(65)),
            ("claimed_controllers", json!(3)),
            ("driving_controllers", json!(2)),
            ("window_ms", json!(60_001)),
            ("ui_build", json!("private/path")),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = value;
            let event = serde_json::from_value::<AppEvent>(invalid).unwrap();
            assert!(
                super::validate_event(&event).is_err(),
                "accepted invalid {field}"
            );
        }
        let mut invalid = valid;
        invalid["reason"] = json!("untrusted free text");
        assert!(serde_json::from_value::<AppEvent>(invalid).is_err());
    }
}
