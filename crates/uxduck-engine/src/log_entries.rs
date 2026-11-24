//! Logbook entries from an autopilot log alone (S459, ADR-0039 R8, R15 and
//! "Autopilot logs from anywhere"): a vehicle flown without UXDuck still
//! has its flights in the Logbook, from the autopilot's own log.
//!
//! Each log in this computer's logs (`<data>/logs-from-cards`, copied off a
//! card or imported) that is on GPS time gives one entry for each arming it
//! records that no Logbook flight covers: the same system id and some of
//! the same armed time, as S426 matches a log to a flight. A log that
//! records no arming gives one entry for all of it. The entries are made
//! where the copied logs are listed (`flight/list`, `logs/cards`,
//! `logs/copyCard`), so a log read since gives its entries the next time
//! the Logbook looks. Each has an id from the log's path and when it armed,
//! so reading the log again never adds a second set, and one deleted stays
//! deleted as every deleted entry does.

use uxduck_core::domain::flight::{Flight, FlightEnd, FlightLogFile};
use uxduck_core::events::{Actor, Change};
use uxduck_mavlink::autopilot_log::Arming;

use crate::Engine;
use crate::runtime::AutopilotLogRead;

fn actor() -> Actor {
    Actor {
        client_id: "uxduck-flight".into(),
        operator_id: None,
        identity: None,
    }
}

/// FNV-1a: the same on every computer and every run, unlike the standard
/// library's hasher, so an entry's id stays its own.
fn stable_hash(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// An entry's id: its log's path and when it armed.
#[must_use]
pub fn entry_id(path: &str, armed_at_us: u64) -> String {
    format!("fl{:016x}", stable_hash(&format!("{path}@{armed_at_us}")))
}

/// The vehicle of a log's entries when none known here is it: the log's own.
fn log_vehicle_id(path: &str) -> String {
    format!("log{:016x}", stable_hash(path))
}

/// The id a vehicle with this flight controller takes when it connects
/// here: from its `uid2`, as the log's board id gives it in hex.
fn board_vehicle_id(uid: &str) -> Option<String> {
    let mut uid2 = [0u8; 18];
    if uid.len() != uid2.len() * 2 {
        return None;
    }
    for (byte, hex) in uid2.iter_mut().zip(uid.as_bytes().chunks(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
    }
    uxduck_mavlink::identity::vid_from_uid(0, uid2)
}

impl Engine {
    /// Adds the Logbook entries the copied logs read so far give (S459).
    pub(crate) fn add_log_entries(&mut self) {
        for read in self.system.autopilot_log_reads() {
            if !read.span.gps_time {
                continue;
            }
            let armings = if read.armings.is_empty() {
                vec![Arming {
                    from_us: read.span.from_us,
                    to_us: read.span.to_us,
                    ..Arming::default()
                }]
            } else {
                read.armings.clone()
            };
            for arming in armings {
                let id = entry_id(&read.path, arming.from_us);
                if self.state.flights.contains_key(&id)
                    || self.state.forgotten_flights.contains(&id)
                    || self.arming_covered(read.span.system_id, arming.from_us, arming.to_us)
                {
                    continue;
                }
                let flight = self.log_entry(&read, id, arming);
                self.commit_as(actor(), Change::FlightUpserted { flight });
            }
        }
    }

    /// Whether a Logbook flight of this system id was armed some of this
    /// time (S426's rule).
    fn arming_covered(&self, system_id: u8, from_us: u64, to_us: u64) -> bool {
        let now = self.clock.now_us();
        self.state.flights.values().any(|flight| {
            let system = flight
                .system_id
                .or_else(|| self.vehicle(&flight.vehicle_id).map(|v| v.system_id));
            system == Some(system_id)
                && flight.armed_at_us <= to_us
                && from_us <= flight.ended_at_us.unwrap_or(now)
        })
    }

    /// The vehicle of a log's entries: the one known here whose uid is the
    /// log's flight controller's. Else the one known here with its system
    /// id, real or simulated as the log is, when there is exactly one. Else
    /// the log's own, named as a replay names a vehicle it does not know
    /// (R3): by its flight controller when the log names it, so every log
    /// from one flight controller is one vehicle, with the id that vehicle
    /// takes when it connects here (ADR-0004). Else by the log's path, the
    /// same for every entry of the log.
    fn entry_vehicle(&self, read: &AutopilotLogRead) -> (String, String, String) {
        let board = read.span.board.as_ref();
        let by_board = board.and_then(|board| {
            self.state
                .vehicles
                .values()
                .find(|v| !v.replay && v.uid.as_deref() == Some(board.uid.as_str()))
        });
        let mut known = self.state.vehicles.values().filter(|v| {
            !v.replay
                && v.system_id == read.span.system_id
                && self.is_simulated(v) == read.simulated
        });
        let by_system = match (known.next(), known.next()) {
            (Some(vehicle), None) => Some(vehicle),
            _ => None,
        };
        if let Some(vehicle) = by_board.or(by_system) {
            return (
                vehicle.id.clone(),
                vehicle.name.clone(),
                vehicle.vehicle_type.clone(),
            );
        }
        let id = board
            .and_then(|board| board_vehicle_id(&board.uid))
            .unwrap_or_else(|| log_vehicle_id(&read.path));
        let vehicle_type = read.vehicle_type.clone().unwrap_or_default();
        let entries = || {
            self.state
                .flights
                .values()
                .filter(|f| f.autopilot_log.is_some())
        };
        if let Some(named) = entries().find(|f| f.vehicle_id == id) {
            return (id, named.vehicle_name.clone(), vehicle_type);
        }
        // Nor a name a replay vehicle goes by, but its own replay's.
        let own_replay = uxduck_mavlink::identity::replay_vid(&id);
        let replaying: Vec<String> = self
            .state
            .vehicles
            .values()
            .filter(|v| v.replay && v.id != own_replay)
            .map(|v| v.name.clone())
            .collect();
        let name = self.replay_name(read.simulated, &replaying);
        (id, name, vehicle_type)
    }

    /// Who a flight controller is here: the known vehicle with its uid,
    /// else the vehicle its logs' entries are, by the id it takes when it
    /// connects. Its id and name.
    pub(crate) fn board_vehicle(&self, uid: &str) -> Option<(String, String)> {
        let known = self
            .state
            .vehicles
            .values()
            .find(|v| !v.replay && v.uid.as_deref() == Some(uid))
            .map(|v| (v.id.clone(), v.name.clone()));
        known.or_else(|| {
            let id = board_vehicle_id(uid)?;
            let entry = self
                .state
                .flights
                .values()
                .find(|f| f.autopilot_log.is_some() && f.vehicle_id == id)?;
            Some((id, entry.vehicle_name.clone()))
        })
    }

    /// The names of the vehicles known here only by their logs' entries
    /// (S459), which no other vehicle is given.
    pub(crate) fn log_vehicle_names(&self) -> impl Iterator<Item = &str> {
        self.state
            .flights
            .values()
            .filter(|f| f.autopilot_log.is_some() && self.vehicle(&f.vehicle_id).is_none())
            .map(|f| f.vehicle_name.as_str())
    }

    fn log_entry(&self, read: &AutopilotLogRead, id: String, arming: Arming) -> Flight {
        let (vehicle_id, vehicle_name, vehicle_type) = self.entry_vehicle(read);
        Flight {
            id,
            vehicle_id,
            system_id: Some(read.span.system_id),
            vehicle_name,
            vehicle_type,
            name: None,
            simulated: read.simulated,
            seen_armed: false,
            armed_at_us: arming.from_us,
            ended_at_us: Some(arming.to_us),
            end: FlightEnd::Disarmed,
            air_us: arming.air_us,
            moving_us: Some(arming.moving_us),
            max_height_m: arming.max_height_m,
            max_speed_mps: arming.max_speed_mps,
            modes: arming.modes,
            battery_start_pct: arming.battery_start_pct,
            battery_end_pct: arming.battery_end_pct,
            operators: Vec::new(),
            keep: false,
            recorded_here: false,
            left_account: false,
            contact_lost_at_us: None,
            link_id: None,
            tlog: None,
            tlog_end: None,
            other_links: Vec::new(),
            telemetry_gaps: None,
            log: uxduck_core::domain::flight::FlightLog::None,
            sync_video: false,
            mission: None,
            params: None,
            checklist: None,
            copies: Vec::new(),
            autopilot_log: Some(FlightLogFile {
                path: read.path.clone(),
            }),
            version: 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_id_is_its_log_and_its_arming_every_time() {
        let id = entry_id("/data/logs-from-cards/NO NAME-2026-11-22/00000047.BIN", 1);
        assert_eq!(
            id,
            entry_id("/data/logs-from-cards/NO NAME-2026-11-22/00000047.BIN", 1)
        );
        assert_eq!(id.len(), 18);
        assert_ne!(
            id,
            entry_id("/data/logs-from-cards/NO NAME-2026-11-22/00000047.BIN", 2)
        );
        assert_ne!(
            id,
            entry_id("/data/logs-from-cards/NO NAME-2026-11-22/00000048.BIN", 1)
        );
    }
}
