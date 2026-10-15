//! Product analytics (ADR-0013 P10, P11; ADR-0016): the app's events over
//! `analytics/track`, the daemon's own, and the usage analytics switch. The
//! switch is checked here, so nothing reaches the port while it is off.

use serde_json::{Value, json};
use uxduck_contracts::dto::analytics::{
    AnalyticsAliasParams, AnalyticsEvent, AppEvent, CommandName, CommandOutcome, DaemonEvent,
    ErrorFrame, LinkType, TurnOutcome, is_error_type, is_token,
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
    match request {
        ClientRequest::AnalyticsTrack(event) => {
            if let AppEvent::ErrorShown { code } = &event
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
            } = &event
                && !(is_error_type(error_type) && is_token(code) && frames_ok(frames))
            {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "an exception is a type, a known code and up to 50 frames, with no text",
                ));
            }
            if let AppEvent::TelemetryLayoutChanged { item, .. } = &event
                && !is_item_key(item)
            {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "an item is a telemetry key, such as speed.air_mps",
                ));
            }
            if engine.state.settings.usage_analytics {
                // A page over the WebSocket is a browser tab; the desktop
                // app's page comes over the local socket.
                let surface = match engine.connections.get(&connection).map(|c| c.transport) {
                    Some(Transport::WebSocket) => "browser",
                    _ => "desktop",
                };
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
}
