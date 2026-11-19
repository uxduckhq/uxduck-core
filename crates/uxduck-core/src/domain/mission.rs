//! A vehicle's mission (ADR-0004, ADR-0001 entity versions). The daemon
//! keeps the mission as last downloaded or uploaded; an upload names the
//! version it was edited from and is refused if another write came first.

use serde::{Deserialize, Serialize};

use crate::domain::parameter::{TransferProgress, TransferStatus};

/// One item, as `MISSION_ITEM_INT` carries it. In global frames the
/// position is in degrees and metres; other frames carry `param5` and
/// `param6` in the same fields, scaled the same way.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MissionItem {
    /// `MAV_CMD`, such as 16 for a waypoint or 22 for take-off.
    pub command: u16,
    /// `MAV_FRAME`: 0 above mean sea level, 3 above home, 10 above terrain.
    pub frame: u8,
    pub param1: f64,
    pub param2: f64,
    pub param3: f64,
    pub param4: f64,
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    pub altitude_m: f64,
    pub autocontinue: bool,
}

/// A vehicle's mission. Its id is the vehicle id; item 0 is home on
/// ArduPilot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mission {
    pub vehicle_id: String,
    pub items: Vec<MissionItem>,
    pub status: TransferStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_instant_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_at_us: Option<u64>,
    /// Bumped when the items change, not when only the status does, so an
    /// editor sees a conflict only when there is one.
    pub version: u64,
    /// How far a download or upload has come while it runs (S127).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<TransferProgress>,
    /// Its fence and rally points, as the mission protocol carries them
    /// (S128): fence vertices and circles, and `NAV_RALLY_POINT`s.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fence: Vec<MissionItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rally: Vec<MissionItem>,
    /// How the last read of each part from the vehicle ended (FB-256):
    /// the items, the fence and the rally points. Absent until one ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items_read: Option<PartRead>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fence_read: Option<PartRead>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rally_read: Option<PartRead>,
    /// The parts UXDuck sent, or tried to, since each was last read:
    /// `MAV_MISSION_TYPE` 0 the mission, 1 the fence, 2 rally points. What
    /// is kept of them is what was sent, not what the vehicle answered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sent: Vec<u8>,
}

impl Mission {
    /// The last read of one part: 0 the mission, 1 the fence, 2 rally points.
    #[must_use]
    pub fn part_read(&self, kind: u8) -> Option<&PartRead> {
        match kind {
            0 => self.items_read.as_ref(),
            1 => self.fence_read.as_ref(),
            _ => self.rally_read.as_ref(),
        }
    }

    /// Records how a read of one part ended. Once read, whole or not, it is
    /// no longer one UXDuck sent without reading back.
    pub fn read_part(&mut self, kind: u8, read: PartRead) {
        self.sent.retain(|sent| *sent != kind);
        match kind {
            0 => self.items_read = Some(read),
            1 => self.fence_read = Some(read),
            _ => self.rally_read = Some(read),
        }
    }

    /// UXDuck sent one part, or tried to: what the vehicle holds of it is
    /// not known again until it is read.
    pub fn sent_part(&mut self, kind: u8) {
        if !self.sent.contains(&kind) {
            self.sent.push(kind);
            self.sent.sort_unstable();
        }
    }
}

/// How a read of one part of a vehicle's mission ended (FB-256).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartRead {
    pub outcome: ReadOutcome,
    /// When it ended, in the daemon's clock.
    pub at_us: u64,
    /// Why it was not read, in the vehicle's or UXDuck's words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReadOutcome {
    /// Every item the vehicle said it holds came, none at all included.
    Read,
    /// It did not come whole: no answer, or a refusal.
    Failed,
    /// The autopilot does not keep this part through the mission protocol.
    Unsupported,
}

/// How a mission came to be on a vehicle (ADR-0029 P7, S134).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum MissionOrigin {
    /// On the vehicle when UXDuck first read it.
    Found,
    /// Uploaded from UXDuck, from a plan when one is named; `verified` once
    /// it was read back.
    Uploaded {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        verified: Option<bool>,
    },
    /// A simulated vehicle's example, sent as it started (P12).
    Example,
    /// Changed by something UXDuck did not send: another ground station, a
    /// companion computer, or the autopilot itself.
    ChangedElsewhere,
}

/// One mission a vehicle has held (S134), kept once by its content: the
/// same mission held again is the same record, seen again.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MissionRecord {
    /// `<vehicle>:<content hash>`.
    pub id: String,
    pub vehicle_id: String,
    /// Without ArduPilot's home at item 0, which moves with the vehicle.
    pub items: Vec<MissionItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fence: Vec<MissionItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rally: Vec<MissionItem>,
    /// How it last came to be on the vehicle.
    pub origin: MissionOrigin,
    /// Who uploaded it, for an upload: the client and operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_id: Option<String>,
    pub first_seen_us: u64,
    /// When it last became the vehicle's mission.
    pub last_seen_us: u64,
    /// How many times it became the vehicle's mission.
    pub times: u32,
    /// The journal's sequence when it last became the vehicle's mission:
    /// orders records whatever the clock did.
    #[serde(default)]
    pub seq: u64,
}

impl MissionRecord {
    /// How it came, in a few words: "Uploaded from Ridge survey".
    #[must_use]
    pub fn origin_words(&self) -> String {
        match &self.origin {
            MissionOrigin::Found => "Found when it connected".into(),
            MissionOrigin::Uploaded {
                plan_name: Some(name),
                ..
            } => format!("Uploaded from {name}"),
            MissionOrigin::Uploaded { .. } => "Uploaded".into(),
            MissionOrigin::Example => "Its example, as the simulation started".into(),
            MissionOrigin::ChangedElsewhere => "Changed by another ground station".into(),
        }
    }

    /// What it holds: "38 items, a fence, 2 rally points", or "No mission".
    #[must_use]
    pub fn counts(&self) -> String {
        let mut parts = Vec::new();
        match self.items.len() {
            0 => {}
            1 => parts.push("1 item".to_owned()),
            n => parts.push(format!("{n} items")),
        }
        if !self.fence.is_empty() {
            parts.push("a fence".into());
        }
        match self.rally.len() {
            0 => {}
            1 => parts.push("1 rally point".into()),
            n => parts.push(format!("{n} rally points")),
        }
        if parts.is_empty() {
            "No mission".into()
        } else {
            parts.join(", ")
        }
    }
}
