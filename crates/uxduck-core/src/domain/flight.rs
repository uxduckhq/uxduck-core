//! A flight (C8): arm to disarm, as UXDuck saw it, kept so a logbook can be
//! made from it. Landed but still armed does not end one; disarming does.

use serde::{Deserialize, Serialize};

/// How a flight ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FlightEnd {
    /// Still armed.
    #[default]
    Open,
    /// It disarmed.
    Disarmed,
    /// UXDuck stopped while it was armed; ended when UXDuck last heard it.
    Interrupted,
    /// Someone ended it while UXDuck could not hear the vehicle (`flight/end`),
    /// at when it was last heard. Never ended so by itself: a link can drop
    /// and come back (maintainer, 2026-10-24).
    ContactLost,
}

/// A flight's log sent to the account by itself, rather than with every
/// flight log while "Store flight logs and videos" is on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FlightLog {
    #[default]
    None,
    /// Asked for: on its way, cut from the connection's log here.
    Asked,
    /// In the account, as a recording named after the flight.
    Stored,
}

impl FlightLog {
    #[must_use]
    pub fn is_none(&self) -> bool {
        *self == Self::None
    }
}

/// A mission a flight flew (C8). One mission can take several flights, a
/// battery swap apart: they share a run, and UXDuck asks about saving once
/// the run's mission is finished, not at each landing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlightMission {
    /// The mission on the vehicle, by its content (`<vehicle>:<hash>`,
    /// S134), when UXDuck knew it: flights share a run only with the same.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission_id: Option<String>,
    /// The plan it was uploaded from, when UXDuck uploaded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_name: Option<String>,
    /// The vehicle said the mission was complete during this flight.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub completed: bool,
    /// The first flight of the run.
    pub run_id: String,
}

/// The parameters a flight armed with (C7): enough to tell later whether
/// two flights flew the same configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlightParams {
    /// SHA-256 of every parameter, one `NAME=value` line each, by name.
    pub digest: String,
    pub count: u32,
}

/// Someone who acted on the vehicle during a flight, from their first
/// command or claim: each a part of the flight (handovers, maintainer
/// 2026-10-23).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlightOperator {
    pub operator_id: String,
    pub name: String,
    pub from_us: u64,
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "each is a fact about the flight"
)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Flight {
    pub id: String,
    pub vehicle_id: String,
    /// The vehicle's name and type when it flew.
    pub vehicle_name: String,
    pub vehicle_type: String,
    /// A simulation's, by its link.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub simulated: bool,
    /// UXDuck first heard it already armed, so it began earlier.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub seen_armed: bool,
    pub armed_at_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_us: Option<u64>,
    #[serde(default)]
    pub end: FlightEnd,
    /// Time off the ground, for one that flies.
    #[serde(default)]
    pub air_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_height_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_speed_mps: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_start_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_end_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operators: Vec<FlightOperator>,
    /// Saved to the account (C9): flight data is off by default, so a
    /// flight goes to the account, and the logbook on every computer, only
    /// when someone keeps it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keep: bool,
    /// Recorded on this computer, rather than had from the account. Never
    /// sent: the account's copy says false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recorded_here: bool,
    /// Deleted from the account after it was saved; kept here, and not
    /// saved again (the account keeps its removal).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub left_account: bool,
    /// While open: when UXDuck last heard the vehicle, if it has gone
    /// offline since. Cleared when it is heard again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact_lost_at_us: Option<u64>,
    /// The connection it was heard on, for its flight log here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_id: Option<String>,
    /// Its flight log in the account on its own (2026-10-24): asked for
    /// while "Store flight logs and videos" is off, then stored.
    #[serde(default, skip_serializing_if = "FlightLog::is_none")]
    pub log: FlightLog,
    /// The mission it flew, if it flew one (C8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission: Option<FlightMission>,
    /// The parameters it armed with (C7), when UXDuck had them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<FlightParams>,
    /// Its checklist as it stood when it armed, and every override
    /// (S193, ADR-0038 K14).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checklist: Option<crate::domain::checklist::ChecklistRecord>,
    pub version: u64,
}
