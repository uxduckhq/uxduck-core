//! Vehicles: a daemon-minted id grouping component addresses by evidence
//! (ADR-0004). The live record (address, presence) and the user's profile
//! (name, saved) are one entity for now; they split when profiles gain
//! parameters and missions.

use serde::{Deserialize, Serialize};

/// A model the person chose to draw a vehicle with (ADR-0035 M5), in place
/// of the one its type gives: UXDuck's own models only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VehicleLook {
    Plane,
    Vtol,
    Multicopter,
    Rover,
    Boat,
    Submarine,
    Airship,
    Jet,
}

/// A 3D model of the person's own that a vehicle is drawn with (ADR-0035
/// M5, 2026-11-12), in place of UXDuck's: a glTF file UXDuck keeps.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnModel {
    /// Where UXDuck keeps the file: its SHA-256, in hex.
    pub file: String,
    /// The file's name as the person chose it, such as `skywalker.glb`.
    pub name: String,
    pub bytes: u64,
    /// How far it is turned clockwise, seen from above, from the front
    /// glTF gives it (+Z): 0, 90, 180 or 270 degrees.
    pub turn_deg: u16,
    /// Why it may slow the maps down, in words, when it is more detailed
    /// than a vehicle needs: written when the file was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// A vehicle's colour on the maps, in the sidebar and on its mission
/// (ADR-0029 P19). None is green, red, amber, orange or grey, which mean
/// inside and outside a fence, connected, lost and offline, so a colour
/// never reads as a vehicle's state (maintainer, 2026-10-29).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VehicleColour {
    Blue,
    Pink,
    Teal,
    Purple,
    Lilac,
    Orchid,
    Violet,
    Sky,
    Magenta,
}

impl VehicleColour {
    /// In the order they are given: each as far as it can be from those
    /// before it, so the first few vehicles are told apart at a glance.
    pub const ALL: [Self; 9] = [
        Self::Blue,
        Self::Pink,
        Self::Teal,
        Self::Purple,
        Self::Lilac,
        Self::Orchid,
        Self::Violet,
        Self::Sky,
        Self::Magenta,
    ];

    /// The colour for a vehicle heard for the first time: the first that no
    /// vehicle heard now has, else the one fewest vehicles have.
    /// `in_use` is every other vehicle's colour, with whether it is heard.
    #[must_use]
    pub fn given<'a>(in_use: impl IntoIterator<Item = (&'a Self, bool)>) -> Self {
        let mut heard = [false; Self::ALL.len()];
        let mut count = [0usize; Self::ALL.len()];
        for (colour, now) in in_use {
            let index = Self::ALL.iter().position(|c| c == colour).unwrap_or(0);
            heard[index] |= now;
            count[index] += 1;
        }
        (0..Self::ALL.len())
            .min_by_key(|&i| (heard[i], count[i], i))
            .map_or(Self::Blue, |i| Self::ALL[i])
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Presence {
    Online,
    Stale,
    #[default]
    Offline,
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "what the heartbeat says, and the user's choices about it"
)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Vehicle {
    /// The VID: a UUID v5 of the autopilot UID once known.
    pub id: String,
    pub name: String,
    /// Earlier ids of this vehicle, such as its provisional id.
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connections: Option<VehicleConnections>,
    pub link_id: String,
    pub peer: String,
    pub system_id: u8,
    pub component_ids: Vec<u8>,
    pub autopilot: String,
    pub vehicle_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    /// The first 8 characters of the git hash the firmware was built from,
    /// from `AUTOPILOT_VERSION`'s `flight_custom_version` (ADR-0021 P3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware_commit: Option<String>,
    pub presence: Presence,
    /// When the last heartbeat arrived; updated on presence changes, not
    /// on every heartbeat.
    pub last_heartbeat_us: u64,
    /// From the heartbeat's base mode.
    #[serde(default)]
    pub armed: bool,
    /// The flight mode by ArduPilot's name, such as `LOITER`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// The user kept it: it survives the session and shows when offline.
    pub saved: bool,
    /// Hidden for the rest of the session (a neighbour's vehicle).
    pub dismissed: bool,
    /// Hidden by the person, once offline, until it is seen again or they
    /// unhide it (S247):
    /// out of every list, kept with its name, flights and history. Kept on
    /// this computer only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    /// Met only in a log someone opened, never heard live (S248): a real
    /// board's UID, kept so its name sticks. Cleared when it is heard.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub seen_in_log: bool,
    /// Kept out of the account's library although sync is on (ADR-0001 D9,
    /// "publication").
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub local_only: bool,
    /// The person's labels on it, such as "Survey" or "Site B" (S106): any
    /// number, meaning nothing to UXDuck, sorted without regard to case.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Web pages the person keeps with it, such as its radio's settings
    /// page, opened from its menu: in their order, meaning nothing to
    /// UXDuck.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bookmarks: Vec<Bookmark>,
    /// Its length, or wingspan, in centimetres, as the person gave it, so
    /// the 3D map draws it that size; absent, the map uses the size of the
    /// one its autopilot's simulator flies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_cm: Option<u32>,
    /// Its gripper, as the operator identified it (S263): which actuator
    /// and which end opens it. Kept on this computer with the vehicle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gripper: Option<crate::domain::gripper::GripperSetup>,
    /// Its companion computers, as the person added them (S296, ADR-0049
    /// O7). Kept on this computer with the vehicle.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub companions: Vec<crate::domain::companion::CompanionComputer>,
    /// Whether the traffic its ADS-B receiver reports is used (S168,
    /// ADR-0034): off until a person turns it on, so one vehicle's faulty
    /// receiver never puts aircraft in front of the whole fleet.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub traffic: bool,
    /// The model it is drawn as, as the person chose; absent, its type's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub look: Option<VehicleLook>,
    /// Its own 3D model, as the person gave it, drawn in place of `look`
    /// and its type's. Never set with `look`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own_model: Option<OwnModel>,
    /// Its colour: given when it is first heard and kept with its id, so it
    /// is the same whichever order vehicles connect in, or chosen in its
    /// menu. Absent only on a record from before colours were kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colour: Option<VehicleColour>,
    /// How its preflight checks are set and the plan chosen for its next
    /// flight (ADR-0038 K6): only what a person changed.
    #[serde(
        default,
        skip_serializing_if = "crate::domain::preflight::PreflightSettings::is_default"
    )]
    pub preflight: crate::domain::preflight::PreflightSettings,
    /// Which parameter descriptions it uses (ADR-0044 F10): only what a
    /// person chose.
    #[serde(
        default,
        skip_serializing_if = "crate::domain::param_definitions::DefinitionsChoice::is_automatic"
    )]
    pub definitions: crate::domain::param_definitions::DefinitionsChoice,
    /// Which MAVLink dialects it uses (ADR-0044 F13): only what a person
    /// chose.
    #[serde(
        default,
        skip_serializing_if = "crate::domain::dialect::DialectChoice::is_automatic"
    )]
    pub dialects: crate::domain::dialect::DialectChoice,
    /// Heard in a replay (ADR-0039 R3), not live: nothing can be sent to
    /// it and nothing is recorded of it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replay: bool,
    /// A replay vehicle's original, when it is here: the vehicle it shows,
    /// whose name, look and size it follows and which a rename of it
    /// renames (ADR-0039 R3, S238).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_of: Option<String>,
    /// When its source begins, for a replay vehicle shown twice at once (two
    /// sources with the same vehicle, ADR-0039 R20): each copy says when.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_at_us: Option<u64>,
    /// It said it is simulated (ADR-0039 R14): ArduPilot's and PX4's SITL
    /// have `SIM_` parameters. A simulation of UXDuck's own is simulated by
    /// its link.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sim_evidence: bool,
    /// What the person said, over the evidence: simulated or real.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simulated: Option<bool>,
    pub version: u64,
}

/// A web page kept with a vehicle, such as the settings page its radio
/// serves on the local network.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bookmark {
    pub name: String,
    /// An `http` or `https` address.
    pub url: String,
}

/// What the data plane saw of a vehicle; the engine merges it into the
/// entity and keeps the user's fields (name, saved, dismissed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    pub id: String,
    pub aliases: Vec<String>,
    pub connections: Option<VehicleConnections>,
    pub link_id: String,
    pub peer: String,
    pub system_id: u8,
    pub component_ids: Vec<u8>,
    pub autopilot: String,
    pub vehicle_type: String,
    pub uid: Option<String>,
    pub firmware: Option<String>,
    pub firmware_commit: Option<String>,
    pub presence: Presence,
    pub last_heartbeat_us: u64,
    pub armed: bool,
    pub mode: Option<String>,
}

/// The words UXDuck names vehicles with before anyone does: the NATO
/// alphabet, so a name never repeats a system id, which two real vehicles
/// can share (maintainer, 2026-10-10).
pub const CALLSIGNS: [&str; 26] = [
    "Alpha", "Bravo", "Charlie", "Delta", "Echo", "Foxtrot", "Golf", "Hotel", "India", "Juliett",
    "Kilo", "Lima", "Mike", "November", "Oscar", "Papa", "Quebec", "Romeo", "Sierra", "Tango",
    "Uniform", "Victor", "Whiskey", "X-ray", "Yankee", "Zulu",
];

/// A name for a vehicle nobody has named: the first callsign no other
/// vehicle goes by, then "Alpha 2" and on once all 26 are taken.
#[must_use]
pub fn callsign<'a>(taken: impl IntoIterator<Item = &'a str>) -> String {
    let taken: std::collections::HashSet<&str> = taken.into_iter().collect();
    // Enough rounds for more names than are taken, so one is free.
    let rounds = u32::try_from(taken.len() / CALLSIGNS.len() + 1).unwrap_or(u32::MAX);
    (1..=rounds)
        .flat_map(|round| {
            CALLSIGNS.iter().map(move |word| {
                if round == 1 {
                    (*word).to_owned()
                } else {
                    format!("{word} {round}")
                }
            })
        })
        .find(|name| !taken.contains(name.as_str()))
        .unwrap_or_else(|| CALLSIGNS[0].to_owned())
}

/// Whether UXDuck gave this name: a callsign, alone or with a round
/// ("Alpha 2").
#[must_use]
pub fn is_callsign(name: &str) -> bool {
    let (word, round) = name.split_once(' ').unwrap_or((name, ""));
    CALLSIGNS.contains(&word)
        && (round.is_empty()
            || round
                .parse::<u32>()
                .is_ok_and(|n| n >= 2 && !round.starts_with('0')))
}

/// A name for a simulation nobody has named: "Sim" and its instance
/// counted from 1, so simulations started together are named the same
/// each time, or the first free "Sim n" when that one is taken or the
/// instance is unknown. Callsigns are for real vehicles, so a simulation
/// never looks like one (maintainer, 2026-10-28).
#[must_use]
pub fn simulation_name<'a>(
    instance: Option<usize>,
    taken: impl IntoIterator<Item = &'a str>,
) -> String {
    let taken: std::collections::HashSet<&str> = taken.into_iter().collect();
    // One more number than names are taken, so one is free.
    instance
        .map(|i| i + 1)
        .into_iter()
        .chain(1..=taken.len() + 1)
        .map(|n| format!("Sim {n}"))
        .find(|name| !taken.contains(name.as_str()))
        .unwrap_or_else(|| "Sim 1".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callsigns_take_the_first_free_word() {
        assert_eq!(callsign([]), "Alpha");
        assert_eq!(callsign(["Alpha", "Charlie"]), "Bravo");
        assert_eq!(callsign(["Bravo", "My plane"]), "Alpha");
        let all: Vec<&str> = CALLSIGNS.to_vec();
        assert_eq!(callsign(all.iter().copied()), "Alpha 2");
        assert_eq!(callsign(all.iter().copied().chain(["Alpha 2"])), "Bravo 2");
    }

    #[test]
    fn callsigns_are_words_with_an_optional_round() {
        assert!(is_callsign("Alpha"));
        assert!(is_callsign("X-ray 3"));
        assert!(!is_callsign("Alpha 1"));
        assert!(!is_callsign("Alpha 02"));
        assert!(!is_callsign("Alpha quad"));
        assert!(!is_callsign("Sim 1"));
    }

    #[test]
    fn simulations_are_named_by_instance_then_the_first_free_number() {
        assert_eq!(simulation_name(Some(0), []), "Sim 1");
        assert_eq!(simulation_name(Some(2), ["Sim 1"]), "Sim 3");
        assert_eq!(simulation_name(Some(0), ["Sim 1", "Sim 2"]), "Sim 3");
        assert_eq!(simulation_name(None, ["Sim 1"]), "Sim 2");
    }
}

/// Discovered connections only. Configuring a link is never vehicle evidence.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VehicleConnections {
    pub routes: Vec<VehicleRoute>,
    pub command_available: bool,
    pub reason: String,
    pub epoch: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent transport capability and selection evidence"
)]
pub struct VehicleRoute {
    pub link_id: String,
    pub peer: String,
    pub system_id: u8,
    pub priority: u32,
    pub reachable: bool,
    pub writable: bool,
    pub selected: bool,
    pub telemetry: bool,
    pub last_heartbeat_us: u64,
    pub round_trip_us: Option<u64>,
    pub loss_per_mille: Option<u32>,
    pub last_receive_age_ms: Option<u64>,
    pub reason: String,
}
