//! A vehicle's history (ADR-0022): notes people write about what they did
//! to it, and events UXDuck records by itself. Called history, never log.
//! Every entry has two times: when it happened, which a note may set
//! earlier, and when it was written, which nobody can change (H1).

use serde::{Deserialize, Serialize};

use crate::domain::parameter::show_value;

/// What a note is about (H2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NoteCategory {
    Maintenance,
    Repair,
    Damage,
    Configuration,
    Calibration,
    Firmware,
    Test,
    Other,
}

/// What UXDuck recorded by itself (H3): things a person would want to read
/// later, never every heartbeat or arm.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    rename_all = "kebab-case",
    tag = "type",
    rename_all_fields = "camelCase"
)]
pub enum HistoryEvent {
    FirstSeen {
        /// Such as `Quadrotor`.
        vehicle_type: String,
    },
    Renamed {
        from: String,
        to: String,
    },
    FirmwareChanged {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        to: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        commit: Option<String>,
    },
    TypeChanged {
        from: String,
        to: String,
    },
    SnapshotSaved {
        snapshot_id: String,
        name: String,
    },
    /// A flight ended (C8, ADR-0022 H3): its record in `flight/list`, and
    /// what happened in words ("Flew 12 min, highest 45 m").
    FlightEnded {
        flight_id: String,
        text: String,
    },
    /// Its mission changed (S134): the record in its missions over time,
    /// and what happened in words ("Mission uploaded from Ridge survey: 38
    /// items, a fence").
    MissionChanged {
        record_id: String,
        text: String,
    },
    /// A parameter's value changed (S74), with who or what changed it and,
    /// when they gave one, why. Changes of one parameter in one sitting
    /// fold into one entry (H3): `from` is the value before the first,
    /// `to` the value after the last.
    ParameterChanged {
        name: String,
        /// Unknown when it was not known before.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<f64>,
        to: f64,
        source: ParamSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// How many changes the entry stands for, when more than one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        changes: Option<u32>,
        /// From the first of them to the last, when more than one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        span_us: Option<u64>,
    },
    /// Nobody supervised it, so UXDuck stopped its ground station heartbeat
    /// and its own failsafe took over (S81).
    SupervisionLost {
        /// What its failsafe was set to do.
        failsafe: String,
    },
    /// Someone supervises it again, and the heartbeat is back (S81).
    SupervisionResumed,
}

/// Who or what changed a parameter (S74).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ParamSource {
    /// Someone in UXDuck: the entry's author says who.
    Person,
    /// UXDuck's agent.
    Agent,
    /// Another ground station: one was heard when the vehicle reported it.
    OtherStation,
    /// The vehicle, unasked: by itself, as ArduPilot turns its airspeed
    /// sensor off and on, or by a ground station UXDuck does not hear.
    Vehicle,
    /// Found on a download, such as after the vehicle restarted.
    Download,
}

/// Parameters left out of the history whatever the vehicle's descriptions
/// say, or when it has none: the simulator's own (`SIM_`), counters, ids,
/// totals, and what the vehicle sets itself at each start (ground
/// pressure, airspeed and gyro offsets).
#[must_use]
pub fn unrecorded(name: &str) -> bool {
    const PREFIXES: [&str; 2] = ["SIM_", "STAT_"];
    const PARTS: [&str; 6] = [
        "GND_PRESS",
        "GND_ABS_PRESS",
        "GND_TEMP",
        "DEVID",
        "DEV_ID",
        "CAN_NODEID",
    ];
    const NAMES: [&str; 7] = [
        "MIS_TOTAL",
        "FENCE_TOTAL",
        "RALLY_TOTAL",
        "FORMAT_VERSION",
        "ALT_OFFSET",
        "BARO_FIELD_ELV",
        "VTX_FREQ",
    ];
    let gyro_offsets =
        name.starts_with("INS_GYR") && (name.contains("OFFS") || name.ends_with("_CALTEMP"));
    let airspeed_offset = name.starts_with("ARSPD") && name.ends_with("_OFFSET");
    let sensor_id = name.starts_with("INS") && name.ends_with("_ID");
    PREFIXES.iter().any(|p| name.starts_with(p))
        || PARTS.iter().any(|p| name.contains(p))
        || NAMES.contains(&name)
        || gyro_offsets
        || airspeed_offset
        || sensor_id
}

impl HistoryEvent {
    /// The event in a few words, as the History tab and the agent read it.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::FirstSeen { vehicle_type } => format!("First seen, as a {vehicle_type}"),
            Self::Renamed { from, to } => format!("Renamed from “{from}” to “{to}”"),
            Self::FirmwareChanged { from, to, .. } => match from {
                Some(from) => format!("Firmware {from} → {to}"),
                None => format!("Firmware {to}"),
            },
            Self::TypeChanged { from, to } => format!("Type changed from {from} to {to}"),
            Self::SnapshotSaved { name, .. } => format!("Snapshot “{name}” saved"),
            Self::MissionChanged { text, .. } | Self::FlightEnded { text, .. } => text.clone(),
            Self::ParameterChanged {
                name,
                from,
                to,
                reason,
                changes,
                span_us,
                ..
            } => {
                let (from, to) = (from.map(show_value), show_value(*to));
                let change = match (&from, changes.unwrap_or(1)) {
                    (Some(from), 1) => format!("{name} {from} → {to}"),
                    (None, 1) => format!("{name} set to {to}"),
                    (Some(from), n) if from == &to => {
                        let over = span(span_us.unwrap_or(0));
                        format!("{name} changed {n} times over {over} and is back at {to}")
                    }
                    (Some(from), n) => {
                        let over = span(span_us.unwrap_or(0));
                        format!("{name} {from} → {to} in {n} changes over {over}")
                    }
                    (None, n) => {
                        let over = span(span_us.unwrap_or(0));
                        format!("{name} set to {to} in {n} changes over {over}")
                    }
                };
                match reason {
                    Some(reason) => format!("{change}: {reason}"),
                    None => change,
                }
            }
            Self::SupervisionLost { failsafe } => format!(
                "Nobody was supervising it, so UXDuck stopped its ground station heartbeat: {failsafe}"
            ),
            Self::SupervisionResumed => {
                "Supervised again: UXDuck's ground station heartbeat resumed".into()
            }
        }
    }
}

/// How long folded changes spread over, in round words: "a few seconds",
/// "3 minutes", "2 hours", "a day". The History tab says the same.
#[must_use]
pub fn span(us: u64) -> String {
    let seconds = us / 1_000_000;
    let (count, unit) = if seconds < 60 {
        return "a few seconds".into();
    } else if seconds < 3600 {
        (seconds / 60, "minute")
    } else if seconds < 86_400 {
        (seconds / 3600, "hour")
    } else {
        (seconds / 86_400, "day")
    };
    match (count, unit) {
        (1, "hour") => "an hour".into(),
        (1, _) => format!("a {unit}"),
        (n, _) => format!("{n} {unit}s"),
    }
}

/// A note as it was before an edit (H2): kept, so history cannot be quietly
/// rewritten.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteVersion {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<NoteCategory>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub happened_at_us: u64,
    /// When it was replaced.
    pub replaced_at_us: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    rename_all = "kebab-case",
    tag = "type",
    rename_all_fields = "camelCase"
)]
pub enum EntryKind {
    Note {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        category: Option<NoteCategory>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tags: Vec<String>,
        /// Earlier versions, oldest first.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        earlier: Vec<NoteVersion>,
    },
    Event {
        event: HistoryEvent,
    },
}

/// Who wrote an entry: a person, or a client acting for one (H1, H6).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Author {
    /// The client's name, such as `uxduck-web`, `Claude Code` or `uxduck`
    /// for UXDuck itself.
    pub client_name: String,
    /// The client's kind, such as `agent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_id: Option<String>,
    /// The operator's name at the time, so it reads after a rename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_name: Option<String>,
}

/// Who removed a note, and when (H2): it stays, marked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Deletion {
    pub by: Author,
    pub at_us: u64,
}

/// One entry of a vehicle's history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub id: String,
    /// The VID, so it follows the vehicle (ADR-0004).
    pub vehicle_id: String,
    pub kind: EntryKind,
    /// When it happened.
    pub happened_at_us: u64,
    /// When it was written; never changes.
    pub written_at_us: u64,
    pub author: Author,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_at_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted: Option<Deletion>,
    /// Kept on this computer: the vehicle is local or not saved (S16).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub local: bool,
    pub version: u64,
}

impl HistoryEntry {
    /// Its text: a note's words, or what an event was.
    #[must_use]
    pub fn text(&self) -> String {
        match &self.kind {
            EntryKind::Note { text, .. } => text.clone(),
            EntryKind::Event { event } => event.describe(),
        }
    }

    #[must_use]
    pub const fn is_note(&self) -> bool {
        matches!(self.kind, EntryKind::Note { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_read_as_a_person_would_say_them() {
        let firmware = HistoryEvent::FirmwareChanged {
            from: Some("4.5.7".into()),
            to: "4.6.0".into(),
            commit: None,
        };
        assert_eq!(firmware.describe(), "Firmware 4.5.7 → 4.6.0");
        assert_eq!(
            HistoryEvent::Renamed {
                from: "Copter 1".into(),
                to: "Survey".into()
            }
            .describe(),
            "Renamed from “Copter 1” to “Survey”"
        );
    }

    #[test]
    fn what_the_vehicle_sets_itself_is_not_recorded() {
        for name in [
            "BARO1_GND_PRESS",
            "BARO_GND_TEMP",
            "ARSPD_OFFSET",
            "ARSPD2_OFFSET",
            "INS_GYROFFS_X",
            "INS_GYR2OFFS_Z",
            "INS_GYR1_CALTEMP",
            "INS_ACC2_ID",
            "COMPASS_DEV_ID",
            "MIS_TOTAL",
            "SIM_SPEEDUP",
            "STAT_RUNTIME",
        ] {
            assert!(unrecorded(name), "{name}");
        }
        for name in [
            "ARSPD_USE",
            "INS_ACCOFFS_X",
            "INS_ACC1_CALTEMP",
            "WPNAV_SPEED",
            "RTL_ALT",
        ] {
            assert!(!unrecorded(name), "{name}");
        }
    }

    #[test]
    fn folded_parameter_changes_say_how_many_over_how_long() {
        let changed = |from: Option<f64>, to: f64, changes: u32, span_us: u64| {
            HistoryEvent::ParameterChanged {
                name: "ARSPD_USE".into(),
                from,
                to,
                source: ParamSource::OtherStation,
                reason: None,
                changes: Some(changes),
                span_us: Some(span_us),
            }
            .describe()
        };
        assert_eq!(
            changed(Some(1.0), 1.0, 40, 12 * 60_000_000),
            "ARSPD_USE changed 40 times over 12 minutes and is back at 1"
        );
        assert_eq!(
            changed(Some(1.0), 0.0, 3, 20_000_000),
            "ARSPD_USE 1 → 0 in 3 changes over a few seconds"
        );
        assert_eq!(
            changed(None, 0.0, 2, 3_600_000_000),
            "ARSPD_USE set to 0 in 2 changes over an hour"
        );
        assert_eq!(span(2 * 86_400_000_000), "2 days");
        assert_eq!(span(60_000_000), "a minute");
    }
}
