//! A vehicle's own messages (`STATUSTEXT`, S26, ADR-0004, ADR-0005).
//!
//! Every message goes to the vehicle's log, which people read in the
//! Messages tab and agents read with `vehicle/messages`. A repeat (the same
//! text at the same severity from the same component within the quiet
//! window) folds into the entry before it, counted, when nothing else came
//! between: the log stays in the order things happened, and an entry's
//! first and last times bound a run with nothing else in it. Only what a
//! person should act on becomes a notification:
//!
//! - a message at the operator's threshold or more severe (warning by
//!   default), when it is new or comes back after the window, whatever
//!   came between; an unresolved notice is updated, including after a gap;
//! - but warnings in the first seconds after a (re)boot are one
//!   notification, "Copter 1 started with 3 warnings";
//! - and pre-arm failures are state: one notification per vehicle listing
//!   the reasons now, updated in place, cleared when the vehicle arms, its
//!   pre-arm checks pass, or the reasons stop repeating;
//! - a simulated vehicle's messages notify no one, but its pre-arm reasons
//!   once its simulation start is over.
//!
//! The log is not journaled: the tlog keeps every frame, and the log starts
//! again with the daemon. Its raw view keeps every message as it came.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde_json::Value;
use sha2::{Digest, Sha256};
use uxduck_contracts::dto::messages::{
    VehicleMessage, VehicleMessagesParams, VehicleMessagesResult,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::{ClientRequest, ServerNotification};
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::command::Command;
use uxduck_core::domain::link::LinkKind;
use uxduck_core::domain::notification::{
    Action, NotificationAction, NotificationKind, NotificationState, Severity, Subject,
};
use uxduck_core::domain::setup::{Fix, SetupRow, fix_for};
use uxduck_core::domain::telemetry::{TelemetryItem, Unit};
use uxduck_core::domain::vehicle::Vehicle;
use uxduck_core::events::Change;

use crate::commands::Raise;
use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// Entries kept per vehicle, folded and raw.
const KEEP: usize = 2000;
/// A repeat within this long folds into the entry before it, and does not
/// notify again.
pub const QUIET_US: u64 = 60_000_000;
/// Warnings this soon after a boot are the start-up's: ArduPilot sends its
/// start-up messages within a few seconds.
pub const STARTUP_US: u64 = 10_000_000;
/// A pre-arm reason not repeated for this long has cleared. ArduPilot
/// repeats them about every 30 s while disarmed.
pub const PREARM_US: u64 = 75_000_000;
/// For this long after someone asks a vehicle to arm, or to take off,
/// which arms it first, its pre-arm reasons are a warning. Otherwise they
/// are said quietly beside Arm (`arming.checks`): after a landing,
/// `ArduPilot` repeats "`QLand` mode not armable" every 30 s to nobody who
/// asked (QA 2026-10-24, maintainer).
pub const ARM_ASKED_US: u64 = 30_000_000;

/// One vehicle's log and what its notifications are made of.
#[derive(Default)]
pub(crate) struct Log {
    folded: VecDeque<VehicleMessage>,
    raw: VecDeque<VehicleMessage>,
    /// When this daemon first heard a message from it.
    first_heard_us: Option<u64>,
    /// Start-up warnings since the last boot, and that boot.
    startup: Vec<String>,
    startup_boot_us: Option<u64>,
    /// Each pre-arm reason now, with when it was last said.
    prearm: BTreeMap<String, u64>,
}

/// Everything about vehicles' messages the engine keeps.
#[derive(Default)]
pub(crate) struct Messages {
    logs: BTreeMap<String, Log>,
    next_id: u64,
    /// Connections that asked for a vehicle's entries as they come.
    watchers: BTreeMap<ConnectionId, BTreeSet<String>>,
}

impl Messages {
    pub(crate) fn forget(&mut self, connection: ConnectionId) {
        self.watchers.remove(&connection);
    }
}

/// A pre-arm failure's reason, from ArduPilot's `PreArm: …` or PX4's
/// `Preflight Fail: …`.
fn prearm_reason(text: &str) -> Option<&str> {
    ["PreArm: ", "Preflight Fail: ", "Arm: "]
        .iter()
        .find_map(|prefix| text.strip_prefix(prefix))
        .map(str::trim)
}

fn severity_of(level: u8) -> Severity {
    match level {
        0..=2 => Severity::Alarm,
        3 | 4 => Severity::Warning,
        _ => Severity::Info,
    }
}

fn severity_rank(severity: Severity) -> u8 {
    match severity {
        Severity::Info => 0,
        Severity::Warning => 1,
        Severity::Alarm => 2,
    }
}

fn plural(n: usize, one: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {one}s")
    }
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    let ClientRequest::VehicleMessages(params) = request else {
        return Err(ApiError::new(ErrorKind::Internal, "not a messages method"));
    };
    if !granted.contains(&Capability::Read) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "reading a vehicle's messages needs read",
        ));
    }
    let vehicle = engine
        .vehicle(&params.vehicle_id)
        .map(|v| v.id.clone())
        .ok_or_else(|| {
            ApiError::new(
                ErrorKind::NotFound,
                format!("no vehicle {}", params.vehicle_id),
            )
        })?;
    if params.subscribe {
        engine
            .messages
            .watchers
            .entry(connection)
            .or_default()
            .insert(vehicle.clone());
    }
    to_value(&VehicleMessagesResult {
        messages: engine.vehicle_messages(&vehicle, &params),
    })
}

impl Engine {
    /// A vehicle's log, filtered, oldest first.
    #[must_use]
    pub fn vehicle_messages(
        &self,
        vehicle_id: &str,
        params: &VehicleMessagesParams,
    ) -> Vec<VehicleMessage> {
        let Some(log) = self.messages.logs.get(vehicle_id) else {
            return Vec::new();
        };
        let words: Vec<String> = params
            .query
            .as_deref()
            .unwrap_or_default()
            .to_lowercase()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let limit = usize::try_from(params.limit.unwrap_or(200))
            .unwrap_or(KEEP)
            .min(KEEP);
        let entries = if params.raw { &log.raw } else { &log.folded };
        let mut kept: Vec<VehicleMessage> = entries
            .iter()
            .rev()
            .filter(|m| params.max_severity.is_none_or(|max| m.severity <= max))
            .filter(|m| {
                let text = m.text.to_lowercase();
                words.iter().all(|w| text.contains(w.as_str()))
            })
            .take(limit)
            .cloned()
            .collect();
        kept.reverse();
        kept
    }

    /// A message from a vehicle, whole.
    #[allow(
        clippy::too_many_lines,
        reason = "one message, folded, kept and told in order"
    )]
    pub fn vehicle_message(
        &mut self,
        vehicle_id: &str,
        component_id: u8,
        severity: u8,
        text: &str,
    ) {
        let Some(vehicle) = self.vehicle(vehicle_id).cloned() else {
            return;
        };
        let now = self.clock.now_us();
        let boot = self.transfers.boots.get(&vehicle.id).copied();
        self.messages.next_id += 1;
        let id = self.messages.next_id;
        let log = self.messages.logs.entry(vehicle.id.clone()).or_default();
        let first_heard = *log.first_heard_us.get_or_insert(now);
        let entry = VehicleMessage {
            id,
            vehicle_id: vehicle.id.clone(),
            component_id,
            severity,
            text: text.to_owned(),
            first_us: now,
            last_us: now,
            count: 1,
        };
        log.raw.push_back(entry.clone());
        if log.raw.len() > KEEP {
            log.raw.pop_front();
        }
        let same = |m: &VehicleMessage| {
            m.text == entry.text
                && m.severity == severity
                && m.component_id == component_id
                && now.saturating_sub(m.last_us) < QUIET_US
        };
        // New to the person unless said within the window, whatever came
        // between; folded only into the entry right before it.
        let new = !log.folded.iter().rev().any(same);
        let repeat = log.folded.back_mut().filter(|m| same(m));
        let shown = if let Some(first) = repeat {
            first.count = first.count.saturating_add(1);
            first.last_us = now;
            first.clone()
        } else {
            log.folded.push_back(entry.clone());
            if log.folded.len() > KEEP {
                log.folded.pop_front();
            }
            entry
        };
        let prearm = prearm_reason(text).map(str::to_owned);
        if let Some(reason) = &prearm {
            log.prearm.insert(reason.clone(), now);
        }
        // Start-up: soon after the boot, or, with the boot not known yet,
        // soon after it was first heard.
        let startup = match boot {
            Some(boot) => now.saturating_sub(boot) < STARTUP_US,
            None => now.saturating_sub(first_heard) < STARTUP_US / 2,
        };
        if boot.is_some() && log.startup_boot_us != boot {
            log.startup.clear();
            log.startup_boot_us = boot;
        }
        self.push_message(&shown);
        // Why a calibration from a heading just failed (S185); a setup
        // procedure's own prompts and results show on its row (S186).
        if self.setup_said(&vehicle.id, text) {
            return;
        }
        if self.kept_in_the_log(&vehicle, text, prearm.is_some()) {
            return;
        }
        let threshold = self.state.settings.message_notify_severity;
        if prearm.is_some() {
            self.prearm_notice(&vehicle.id, &vehicle.name, shown.id);
        } else if new && severity <= threshold {
            if startup {
                let log = self.messages.logs.entry(vehicle.id.clone()).or_default();
                log.startup.push(text.to_owned());
                let warnings = log.startup.clone();
                let mut actions =
                    self.setup_actions(&vehicle.id, warnings.iter().map(String::as_str));
                actions.push(show_in_messages(&vehicle.id, shown.id));
                self.raise_or_update(Raise {
                    severity: Severity::Warning,
                    kind: NotificationKind::Persistent,
                    subject: Subject::Vehicle {
                        id: vehicle.id.clone(),
                    },
                    title: format!(
                        "{} started with {}",
                        vehicle.name,
                        plural(warnings.len(), "warning")
                    ),
                    body: Some(warnings.join("\n")),
                    actions,
                    condition: Some(format!(
                        "startup:{}:{}",
                        vehicle.id,
                        boot.unwrap_or(first_heard)
                    )),
                    report: None,
                });
            } else {
                self.message_notice(&vehicle, component_id, severity, text, shown.id);
            }
        }
    }

    /// The same unresolved warning keeps its notice, its acknowledgment and
    /// a link to the latest occurrence. A higher severity alerts anew.
    fn message_notice(
        &mut self,
        vehicle: &Vehicle,
        component_id: u8,
        level: u8,
        text: &str,
        message_id: u64,
    ) {
        let condition = format!(
            "vehicle-message:{}:{component_id}:{:x}",
            vehicle.id,
            uxduck_core::encoding::Hex(&Sha256::digest(text.as_bytes()))
        );
        let title = format!("{}: {text}", vehicle.name);
        let subject = Subject::Vehicle {
            id: vehicle.id.clone(),
        };
        // Notices stored by older versions had no condition. Adopt one and
        // retire its duplicates rather than adding another on the next repeat.
        let mut previous: Vec<_> = self
            .state
            .notifications
            .values()
            .filter(|n| {
                n.condition.as_deref() == Some(&condition)
                    || (n.condition.is_none() && n.subject == subject && n.title == title)
            })
            .cloned()
            .collect();
        previous.sort_by_key(|n| (n.state != NotificationState::Acknowledged, n.created_at_us));
        let severity = previous
            .iter()
            .map(|n| n.severity)
            .chain([severity_of(level)])
            .max_by_key(|s| severity_rank(*s))
            .unwrap_or_else(|| severity_of(level));
        if let Some((kept, duplicates)) = previous.split_first_mut() {
            if kept.condition.as_deref() != Some(&condition) {
                kept.condition = Some(condition.clone());
                kept.version += 1;
                self.commit_as(
                    crate::plugins::daemon(),
                    Change::NotificationUpserted {
                        notification: kept.clone(),
                    },
                );
            }
            for duplicate in duplicates {
                duplicate.state = NotificationState::Resolved;
                duplicate.version += 1;
                self.commit_as(
                    crate::plugins::daemon(),
                    Change::NotificationUpserted {
                        notification: duplicate.clone(),
                    },
                );
            }
        }
        let mut actions = self.setup_actions(&vehicle.id, [text]);
        actions.push(show_in_messages(&vehicle.id, message_id));
        self.raise_or_update(Raise {
            severity,
            kind: NotificationKind::Persistent,
            subject,
            title,
            body: None,
            actions,
            condition: Some(condition),
            report: None,
        });
    }

    /// A simulated vehicle's messages stay in its log (maintainer,
    /// 2026-10-03): its start-up says a great deal that matters to no one,
    /// and the simulation start says itself when the take-off fails. Its
    /// pre-arm reasons are said once nobody is taking it off, and a crash
    /// is said with a way back (`grounded`). True when nothing more is to
    /// be said of this message.
    fn kept_in_the_log(&mut self, vehicle: &Vehicle, text: &str, prearm: bool) -> bool {
        if !self.is_simulated(vehicle) {
            return false;
        }
        if crate::grounded::is_crash(text) {
            self.crash_reported(vehicle);
        }
        !prearm || self.is_launching(&vehicle.link_id)
    }

    /// Whether a vehicle is one UXDuck simulates, on a simulator link.
    pub(crate) fn is_simulated(&self, vehicle: &Vehicle) -> bool {
        self.state
            .links
            .get(&vehicle.link_id)
            .is_some_and(|link| matches!(link.kind, LinkKind::Simulator { .. }))
    }

    /// The vehicle's pre-arm reasons now, without their prefix.
    pub(crate) fn prearm_reasons(&self, vehicle_id: &str) -> Vec<String> {
        self.messages
            .logs
            .get(vehicle_id)
            .map(|log| log.prearm.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Calibrate or Restart for messages a Setup row fixes (S184, ADR-0037
    /// V12), one per row, in the order said.
    fn setup_actions<'a>(
        &self,
        vehicle_id: &str,
        texts: impl IntoIterator<Item = &'a str>,
    ) -> Vec<NotificationAction> {
        let Some(vehicle) = self.vehicle(vehicle_id) else {
            return Vec::new();
        };
        let mut actions: Vec<NotificationAction> = Vec::new();
        for text in texts {
            let Some((row, fix)) = uxduck_core::domain::setup::fix_for(&vehicle.autopilot, text)
            else {
                continue;
            };
            let action = Action::OpenSetup {
                vehicle_id: vehicle.id.clone(),
                row,
            };
            if !uxduck_core::domain::setup::offered(&vehicle.autopilot, row)
                || actions.iter().any(|a| a.action == action)
            {
                continue;
            }
            actions.push(NotificationAction {
                label: match fix {
                    Fix::Calibrate => "Calibrate".into(),
                    Fix::Restart => "Restart".into(),
                },
                action,
            });
        }
        // Two Calibrates side by side say which (S187): "Calibrate radio".
        let calibrates = actions.iter().filter(|a| a.label == "Calibrate").count();
        if calibrates > 1 {
            for action in &mut actions {
                if let (Action::OpenSetup { row, .. }, "Calibrate") =
                    (&action.action, action.label.as_str())
                {
                    action.label = format!("Calibrate {}", row_word(*row));
                }
            }
        }
        actions
    }

    /// The reasons now, as the `arming.checks` item, and the pre-arm notice:
    /// one per vehicle, raised once someone asked it to arm (`ARM_ASKED_US`)
    /// and kept up to date until the reasons clear, with a way to each fix
    /// UXDuck has (S184).
    fn prearm_notice(&mut self, vehicle_id: &str, name: &str, message_id: u64) {
        let reasons = self.prearm_reasons(vehicle_id);
        let now = self.clock.now_us();
        self.telemetry_said(
            vehicle_id,
            TelemetryItem {
                key: "arming.checks".into(),
                label: "Pre-arm checks".into(),
                unit: Unit::None,
                value: None,
                text: (!reasons.is_empty()).then(|| reasons.join("; ")),
                source: "messages".into(),
                component_id: 1,
                time_us: now,
            },
        );
        let condition = format!("prearm:{vehicle_id}");
        if reasons.is_empty() {
            self.resolve(&condition);
            return;
        }
        let shown = self
            .state
            .notifications
            .values()
            .any(|n| n.condition.as_deref() == Some(condition.as_str()));
        if !shown && !self.asked_to_arm(vehicle_id, now) {
            return;
        }
        let mut actions = self.setup_actions(vehicle_id, reasons.iter().map(String::as_str));
        actions.push(show_in_messages(vehicle_id, message_id));
        self.raise_or_update(Raise {
            severity: Severity::Warning,
            kind: NotificationKind::Persistent,
            subject: Subject::Vehicle {
                id: vehicle_id.to_owned(),
            },
            title: format!("{name} cannot arm"),
            body: Some(reasons.join("\n")),
            actions,
            condition: Some(condition),
            report: None,
        });
    }

    /// A row was calibrated and the vehicle uses it at once (S185): its
    /// "not calibrated" reasons are forgotten now rather than when the
    /// vehicle stops repeating them. Said again, they come back.
    pub(crate) fn forget_prearm(&mut self, vehicle_id: &str, row: SetupRow) {
        let Some(vehicle) = self.state.vehicles.get(vehicle_id) else {
            return;
        };
        let (autopilot, name) = (vehicle.autopilot.clone(), vehicle.name.clone());
        let Some(log) = self.messages.logs.get_mut(vehicle_id) else {
            return;
        };
        let before = log.prearm.len();
        log.prearm
            .retain(|reason, _| fix_for(&autopilot, reason) != Some((row, Fix::Calibrate)));
        if log.prearm.len() == before {
            return;
        }
        let last = log.folded.back().map_or(0, |m| m.id);
        self.prearm_notice(vehicle_id, &name, last);
    }

    /// Whether someone asked the vehicle to arm, or to take off, lately:
    /// a person, the agent or a script, not a simulation's own start.
    fn asked_to_arm(&self, vehicle_id: &str, now: u64) -> bool {
        self.state.receipts.values().any(|r| {
            r.vehicle_id == vehicle_id
                && now.saturating_sub(r.created_at_us) < ARM_ASKED_US
                && ![crate::launch::ACTOR, crate::practice::ACTOR].contains(&r.client_id.as_str())
                && match &r.command {
                    Command::Arm => true,
                    // COMPONENT_ARM_DISARM, arming.
                    Command::Long {
                        command: 400,
                        params,
                    } => params.first().is_some_and(|p| *p >= 0.5),
                    command => command.is_take_off(),
                }
        })
    }

    /// Pre-arm reasons clear when the vehicle arms, reports its checks
    /// passing, or stops repeating them. Looked at with each telemetry
    /// update and each change to the vehicle.
    pub(crate) fn review_prearm(&mut self, vehicle_id: &str) {
        let Some(log) = self.messages.logs.get_mut(vehicle_id) else {
            return;
        };
        if log.prearm.is_empty() {
            return;
        }
        let now = self.clock.now_us();
        let armed = self.state.vehicles.get(vehicle_id).is_some_and(|v| v.armed);
        let ready = self
            .telemetry
            .get(vehicle_id)
            .and_then(|t| t.items.iter().find(|i| i.key == "arming.ready"))
            .is_some_and(|i| i.value == Some(1.0));
        let before = log.prearm.len();
        if armed || ready {
            log.prearm.clear();
        } else {
            log.prearm
                .retain(|_, at| now.saturating_sub(*at) < PREARM_US);
        }
        if log.prearm.len() == before {
            return;
        }
        let name = self
            .state
            .vehicles
            .get(vehicle_id)
            .map_or_else(|| vehicle_id.to_owned(), |v| v.name.clone());
        let last = log.folded.back().map_or(0, |m| m.id);
        self.prearm_notice(vehicle_id, &name, last);
    }

    /// Raises a notification, or changes the active one bound to the same
    /// condition in place.
    pub(crate) fn raise_or_update(&mut self, raise: Raise) {
        let existing = raise.condition.as_ref().and_then(|condition| {
            self.state
                .notifications
                .values()
                .find(|n| {
                    n.condition.as_ref() == Some(condition)
                        && n.state != NotificationState::Resolved
                })
                .cloned()
        });
        let Some(mut notification) = existing else {
            self.raise(raise);
            return;
        };
        if severity_rank(raise.severity) > severity_rank(notification.severity)
            || (notification.kind == NotificationKind::Transient
                && raise.kind == NotificationKind::Persistent)
        {
            if let Some(condition) = &raise.condition {
                self.resolve(condition);
            }
            self.raise(raise);
            return;
        }
        if severity_rank(raise.severity) < severity_rank(notification.severity) {
            return;
        }
        if notification.title == raise.title
            && notification.body == raise.body
            && notification.actions == raise.actions
            && notification.severity == raise.severity
            && notification.kind == raise.kind
            && notification.subject == raise.subject
            && notification.report == raise.report
        {
            return;
        }
        notification.title = raise.title;
        notification.body = raise.body;
        notification.actions = raise.actions;
        notification.severity = raise.severity;
        notification.kind = raise.kind;
        notification.subject = raise.subject;
        notification.report = raise.report;
        notification.version += 1;
        self.commit_as(
            crate::plugins::daemon(),
            Change::NotificationUpserted { notification },
        );
    }

    fn push_message(&mut self, entry: &VehicleMessage) {
        let watching: Vec<ConnectionId> = self
            .messages
            .watchers
            .iter()
            .filter(|(_, vehicles)| vehicles.contains(&entry.vehicle_id))
            .map(|(id, _)| *id)
            .collect();
        if watching.is_empty() {
            return;
        }
        let frame =
            crate::rpc::router::notification(&ServerNotification::VehicleMessage(entry.clone()));
        for connection in watching {
            self.send(connection, &frame);
        }
    }
}

fn show_in_messages(vehicle_id: &str, message_id: u64) -> NotificationAction {
    NotificationAction {
        // Short enough for an alert's button (QA 2026-10-24: "Show in me…").
        label: "Messages".into(),
        action: Action::OpenMessages {
            vehicle_id: vehicle_id.to_owned(),
            message_id,
        },
    }
}

/// A Setup row in a button: "radio".
const fn row_word(row: SetupRow) -> &'static str {
    match row {
        SetupRow::Firmware => "firmware",
        SetupRow::Accelerometer => "accelerometer",
        SetupRow::Compass => "compass",
        SetupRow::Radio => "radio",
        SetupRow::Airspeed => "airspeed",
        SetupRow::Motors => "motors",
    }
}
