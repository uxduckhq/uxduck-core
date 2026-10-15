//! Looking at a serial device for a vehicle (ADR-0018 F5, F6, S44): listen
//! before writing, at the speeds radios use, accept only frames whose
//! checksum holds, write a ground-station heartbeat only to a known flight
//! controller that stays silent, and close the device when done. DTR is
//! left as it is, so boards that reset on it do not.

use std::io::Write;
use std::time::{Duration, Instant};

use crate::decode::{Message, decode};
use crate::framing::split;

/// A vehicle's heartbeat, heard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Heard {
    pub system_id: u8,
    pub component_id: u8,
    pub vehicle_type: String,
    pub autopilot: String,
    pub armed: bool,
    /// From `AUTOPILOT_VERSION`, when it answered: its UID, for the
    /// vehicle UXDuck knows it as.
    pub uid: Option<(u64, [u8; 18])>,
}

/// The first vehicle heartbeat in some bytes: not a ground station, and a
/// whole frame whose checksum holds.
#[must_use]
pub fn heartbeat_in(bytes: &[u8]) -> Option<Heard> {
    split(bytes).frames.into_iter().find_map(|frame| {
        let decoded = decode(frame)?;
        match decoded.message {
            Message::Heartbeat {
                mav_type,
                autopilot,
                base_mode,
                ..
            } if autopilot != "Invalid" && mav_type != "Gcs" => Some(Heard {
                system_id: decoded.system_id,
                component_id: decoded.component_id,
                vehicle_type: mav_type,
                autopilot,
                armed: base_mode & 0x80 != 0,
                uid: None,
            }),
            _ => None,
        }
    })
}

/// A UID in some bytes, from `AUTOPILOT_VERSION`.
#[must_use]
pub fn uid_in(bytes: &[u8]) -> Option<(u64, [u8; 18])> {
    split(bytes)
        .frames
        .into_iter()
        .find_map(|frame| match decode(frame)?.message {
            Message::AutopilotVersion { uid, uid2, .. } => Some((uid, uid2)),
            _ => None,
        })
}

/// How to look at a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    /// A flight controller on USB: the speed does not matter; one try, and
    /// one ground-station heartbeat if it stays silent.
    KnownBoard,
    /// A USB-serial adapter: maybe a radio, at one of the usual speeds.
    Adapter,
}

/// The speeds radios use, most common first.
pub const ADAPTER_BAUDS: [u32; 4] = [57_600, 115_200, 921_600, 460_800];

/// What looking found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Heard {
        baud: u32,
        heard: Heard,
    },
    /// Opened at every speed; no valid MAVLink.
    NoMavlink,
    /// A known board that did not answer its heartbeat.
    SilentKnownBoard,
    /// Another program has it.
    Busy,
    /// Not allowed: the platform's fix.
    Permission(String),
    /// Anything else, as the system said it.
    Failed(String),
}

/// How long each listen lasts.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    pub listen: Duration,
    /// A known board's first silence before it gets a heartbeat.
    pub known_silence: Duration,
    pub after_heartbeat: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            listen: Duration::from_millis(1500),
            known_silence: Duration::from_secs(2),
            after_heartbeat: Duration::from_secs(1),
        }
    }
}

fn permission_fix() -> String {
    if cfg!(target_os = "linux") {
        "Add yourself to the dialout group (sudo usermod -aG dialout $USER), then log out and back in"
            .into()
    } else if cfg!(windows) {
        "Close any program using it, or install the board's driver".into()
    } else {
        "Allow UXDuck to use it in the system's privacy settings".into()
    }
}

fn open(path: &str, baud: u32) -> Result<Box<dyn serialport::SerialPort>, Outcome> {
    let try_open = |baud| {
        serialport::new(path, baud)
            .timeout(Duration::from_millis(100))
            .preserve_dtr_on_open()
            .open()
    };
    let result = match try_open(baud) {
        // A pseudo-terminal has no line speed (macOS: "Not a typewriter").
        Err(error) if error.description.contains("typewriter") => try_open(0),
        other => other,
    };
    result.map_err(|error| {
        let text = error.description.to_lowercase();
        if text.contains("busy") || text.contains("in use") || text.contains("exclusive") {
            Outcome::Busy
        } else if text.contains("permission") || text.contains("denied") {
            if cfg!(windows) && text.contains("denied") {
                Outcome::Busy
            } else {
                Outcome::Permission(permission_fix())
            }
        } else {
            Outcome::Failed(error.description)
        }
    })
}

/// Reads for `long`, until a heartbeat arrives.
fn listen(port: &mut dyn serialport::SerialPort, long: Duration) -> (Option<Heard>, Vec<u8>) {
    let until = Instant::now() + long;
    let mut heard = Vec::new();
    let mut chunk = [0u8; 1024];
    while Instant::now() < until {
        match port.read(&mut chunk) {
            Ok(0) => std::thread::sleep(Duration::from_millis(10)),
            Ok(n) => {
                heard.extend_from_slice(&chunk[..n]);
                if let Some(found) = heartbeat_in(&heard) {
                    return (Some(found), heard);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
    (None, heard)
}

/// Asks a vehicle that has spoken for its UID, briefly.
fn ask_uid(
    port: &mut dyn serialport::SerialPort,
    heard: &Heard,
    timing: &Timing,
    gcs: u8,
) -> Option<(u64, [u8; 18])> {
    let ask =
        crate::sim::request_autopilot_version_frame(gcs, heard.system_id, heard.component_id, 1);
    port.write_all(&ask).ok()?;
    let until = Instant::now() + timing.after_heartbeat;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 1024];
    while Instant::now() < until {
        if let Ok(n) = port.read(&mut chunk) {
            bytes.extend_from_slice(&chunk[..n]);
            if let Some(uid) = uid_in(&bytes) {
                return Some(uid);
            }
        }
    }
    None
}

/// Looks at one device as its plan says, and closes it, speaking as ground
/// station `gcs` when it has to speak.
#[must_use]
pub fn probe(path: &str, plan: Plan, timing: &Timing, gcs: u8) -> Outcome {
    let bauds: &[u32] = match plan {
        Plan::KnownBoard => &[115_200],
        Plan::Adapter => &ADAPTER_BAUDS,
    };
    for &baud in bauds {
        let mut port = match open(path, baud) {
            Ok(port) => port,
            Err(outcome) => return outcome,
        };
        let first = if plan == Plan::KnownBoard {
            timing.known_silence
        } else {
            timing.listen
        };
        let (mut heard, _) = listen(port.as_mut(), first);
        if heard.is_none() && plan == Plan::KnownBoard {
            // Some autopilots wait for a ground station to speak first.
            let hello = crate::sim::gcs_heartbeat_frame(gcs, crate::GCS_COMPONENT_ID, 0);
            if port.write_all(&hello).is_ok() {
                heard = listen(port.as_mut(), timing.after_heartbeat).0;
            }
            if heard.is_none() {
                return Outcome::SilentKnownBoard;
            }
        }
        if let Some(mut heard) = heard {
            heard.uid = ask_uid(port.as_mut(), &heard, timing, gcs);
            return Outcome::Heard { baud, heard };
        }
    }
    Outcome::NoMavlink
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::{FakeVehicle, gcs_heartbeat_frame};

    #[test]
    fn a_vehicle_heartbeat_is_heard_and_a_ground_stations_is_not() {
        let mut fake = FakeVehicle::new(3, 0xBEEF);
        let heard = heartbeat_in(&fake.heartbeat()).unwrap();
        assert_eq!(
            (heard.system_id, heard.vehicle_type.as_str()),
            (3, "Quadrotor")
        );
        assert_eq!(heartbeat_in(&gcs_heartbeat_frame(255, 190, 0)), None);
        assert_eq!(
            heartbeat_in(b"$GPGGA,123519,4807.038,N,01131.000,E*47\r\n"),
            None
        );
    }
}
