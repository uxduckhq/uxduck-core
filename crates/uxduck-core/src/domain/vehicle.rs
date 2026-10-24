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
    /// Whether the traffic its ADS-B receiver reports is used (S168,
    /// ADR-0034): off until a person turns it on, so one vehicle's faulty
    /// receiver never puts aircraft in front of the whole fleet.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub traffic: bool,
    /// The model it is drawn as, as the person chose; absent, its type's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub look: Option<VehicleLook>,
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
}
