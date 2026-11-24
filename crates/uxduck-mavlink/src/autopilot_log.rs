//! What an autopilot's own log covers (S426, the first step of S70): the
//! vehicle it names and the time it spans, read once so the Logbook can say
//! which flight a `.bin` or `.ulg` belongs to. It reads the log as replay does
//! (ADR-0039 R15), so the span is on the same clock the replay plays. Each
//! arming it records comes with what the Logbook shows of a flight (S459),
//! so a log no flight covers gives the Logbook its own entries.

use uxduck_core::domain::telemetry::AltitudeFrame;

use crate::session::{Config, Effect, Session};

/// The vehicle an autopilot log names and the time it covers.
#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    /// `SYSID_THISMAV` or PX4's `MAV_SYS_ID`, else 1.
    pub system_id: u8,
    pub firmware: Option<String>,
    /// Its times are GPS time, so Unix microseconds. Else they count from
    /// boot, and the log cannot be placed beside a flight.
    pub gps_time: bool,
    pub from_us: u64,
    pub to_us: u64,
    /// The vehicle's type as its heartbeat says it (`Quadrotor`), when it
    /// says one.
    pub vehicle_type: Option<String>,
    /// A simulator's: it logged `SIM_` parameters.
    pub simulated: bool,
    /// Each time it armed, in order (S459).
    pub armings: Vec<Arming>,
    /// The flight controller it was written on, when it says.
    pub board: Option<crate::dataflash::Board>,
}

/// One arming an autopilot log records (S459), with what the Logbook
/// shows of a flight.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Arming {
    pub from_us: u64,
    /// When it disarmed, else where the log ends.
    pub to_us: u64,
    /// Above home by more than a flying height, until below a metre again.
    pub air_us: u64,
    /// With its ground speed above 0.5 m/s, as a flight recorded here.
    pub moving_us: u64,
    pub max_height_m: Option<f64>,
    pub max_speed_mps: Option<f64>,
    pub modes: Vec<String>,
    pub battery_start_pct: Option<f64>,
    pub battery_end_pct: Option<f64>,
}

/// Below this above home it is on the ground again, as the Logbook's own
/// flights count it.
const GROUND_BELOW_M: f64 = 1.0;
/// Moving, as the Logbook's own flights count it (FB-228).
const MOVING_MPS: f64 = 0.5;
/// How often the session gathers telemetry, in the log's microseconds.
const TICK_US: u64 = 100_000;

/// What an ArduPilot `.bin` or a PX4 `.ulg` covers.
///
/// # Errors
/// When it is neither, or holds no records UXDuck can read.
pub fn span(bytes: &[u8]) -> Result<Span, String> {
    let (tlog, about) = if crate::ulog::is_ulog(bytes) {
        crate::ulog::to_tlog(bytes)?
    } else if crate::dataflash::is_dataflash(bytes) {
        crate::dataflash::to_tlog(bytes)?
    } else {
        return Err("This is not an ArduPilot .bin or a PX4 .ulg log.".into());
    };
    let (from_us, to_us) = crate::tlog::records(&tlog)
        .map(|(at, _)| at)
        .fold(None, |span: Option<(u64, u64)>, at| {
            Some(span.map_or((at, at), |(from, to)| (from.min(at), to.max(at))))
        })
        .ok_or("This log holds no records UXDuck can read.")?;
    let read = armings(&tlog, about.system_id);
    Ok(Span {
        system_id: about.system_id,
        firmware: about.firmware,
        gps_time: about.gps_time,
        from_us,
        to_us,
        vehicle_type: read.vehicle_type,
        simulated: read.simulated,
        armings: read.armings,
        board: about.board,
    })
}

#[derive(Default)]
struct Read {
    vehicle_type: Option<String>,
    simulated: bool,
    armings: Vec<Arming>,
}

/// An arming as the log is gone through.
#[derive(Default)]
struct Open {
    arming: Arming,
    up: bool,
    moving: bool,
    last_us: u64,
}

/// The log's armings, from the heartbeats its records become (`ARM`,
/// `EV` or PX4's armed state), read by the live session as the replay
/// reads them, so the modes are named as the replay names them.
fn armings(tlog: &[u8], system_id: u8) -> Read {
    let mut read = Read::default();
    let mut session = Session::for_replay(Config::default());
    let mut open: Option<Open> = None;
    let mut mode: Option<String> = None;
    let (mut last_tick, mut last_at) = (0u64, 0u64);
    for (at, frame) in crate::tlog::records(tlog) {
        last_at = at;
        let mut effects = session.ingest("log", "log", frame, at);
        if at.saturating_sub(last_tick) >= TICK_US {
            effects.extend(session.tick(at));
            last_tick = at;
        }
        take(&mut read, &mut open, &mut mode, effects, at, system_id);
    }
    let effects = session.tick(last_at);
    take(&mut read, &mut open, &mut mode, effects, last_at, system_id);
    if let Some(open) = open {
        read.armings.push(close(open, last_at));
    }
    read
}

fn close(mut open: Open, at: u64) -> Arming {
    count(&mut open, at);
    open.arming.to_us = at;
    open.arming
}

/// Adds the time since the last sample to the air and moving times.
fn count(open: &mut Open, at: u64) {
    let since = at.saturating_sub(open.last_us);
    if open.up {
        open.arming.air_us = open.arming.air_us.saturating_add(since);
    }
    if open.moving {
        open.arming.moving_us = open.arming.moving_us.saturating_add(since);
    }
    open.last_us = at;
}

fn higher(was: Option<f64>, now: f64) -> f64 {
    was.map_or(now, |was| was.max(now))
}

fn take(
    read: &mut Read,
    open: &mut Option<Open>,
    mode: &mut Option<String>,
    effects: Vec<Effect>,
    at: u64,
    system_id: u8,
) {
    for effect in effects {
        match effect {
            Effect::Vehicle(seen) if seen.system_id == system_id => {
                if read.vehicle_type.is_none() && !seen.vehicle_type.is_empty() {
                    read.vehicle_type = Some(seen.vehicle_type.clone());
                }
                mode.clone_from(&seen.mode);
                match (open.take(), seen.armed) {
                    (None, true) => {
                        *open = Some(Open {
                            arming: Arming {
                                from_us: at,
                                ..Arming::default()
                            },
                            last_us: at,
                            ..Open::default()
                        });
                    }
                    (Some(was), false) => read.armings.push(close(was, at)),
                    (still, _) => *open = still,
                }
                if let (Some(open), Some(mode)) = (open.as_mut(), mode.as_ref())
                    && !open.arming.modes.contains(mode)
                {
                    open.arming.modes.push(mode.clone());
                }
            }
            Effect::Telemetry(update) => {
                let Some(open) = open.as_mut() else {
                    continue;
                };
                count(open, at);
                let height = update
                    .positions
                    .iter()
                    .filter(|p| p.source == "fused")
                    .flat_map(|p| &p.altitudes)
                    .find(|a| a.frame == AltitudeFrame::AboveHome)
                    .map(|a| a.meters);
                // The ground speed the live Logbook counts. A converted
                // DataFlash `POS` carries no velocity, so its position's
                // speed reads 0 and is not used.
                let speed = update
                    .items
                    .iter()
                    .find(|i| i.key == "speed.ground_mps")
                    .and_then(|i| i.value)
                    .filter(|v| v.is_finite());
                if let Some(speed) = speed {
                    open.arming.max_speed_mps = Some(higher(open.arming.max_speed_mps, speed));
                    open.moving = speed > MOVING_MPS;
                }
                if let Some(height) = height {
                    open.arming.max_height_m = Some(higher(open.arming.max_height_m, height));
                    if height > uxduck_core::domain::command::FLYING_ABOVE_HOME_M {
                        open.up = true;
                    } else if height < GROUND_BELOW_M {
                        open.up = false;
                    }
                }
                if let Some(battery) = update
                    .items
                    .iter()
                    .find(|i| i.key == "battery.remaining_pct")
                    .and_then(|i| i.value)
                    .filter(|v| v.is_finite())
                {
                    open.arming.battery_start_pct.get_or_insert(battery);
                    open.arming.battery_end_pct = Some(battery);
                }
            }
            Effect::ParamValue { param, .. } if param.name.starts_with("SIM_") => {
                read.simulated = true;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bin_and_a_ulg_say_their_vehicle_and_their_gps_time() {
        let bin = span(&crate::dataflash::fixture::flight()).unwrap();
        assert!(bin.gps_time, "{bin:?}");
        assert!(
            crate::tlog::sane(bin.from_us) && bin.from_us < bin.to_us,
            "{bin:?}"
        );
        assert!(
            bin.firmware
                .as_deref()
                .is_some_and(|f| f.starts_with("ArduCopter")),
            "{bin:?}"
        );
        assert_eq!(
            bin.board.as_ref().map(|b| b.uid.as_str()),
            Some("1f002d00044e564b32422020000000000000")
        );
        let ulg = span(&crate::ulog::fixture::flight()).unwrap();
        assert!(
            ulg.gps_time && crate::tlog::sane(ulg.from_us) && ulg.from_us < ulg.to_us,
            "{ulg:?}"
        );
        assert!(ulg.board.is_some(), "{ulg:?}");
    }

    /// S459: the fixture's copter arms at 10 s and disarms at 50 s,
    /// GUIDED then RTL, up to 20 m and back.
    #[test]
    fn a_bin_says_each_arming_with_what_the_logbook_shows() {
        let bin = span(&crate::dataflash::fixture::flight()).unwrap();
        assert_eq!(bin.vehicle_type.as_deref(), Some("Quadrotor"));
        assert!(!bin.simulated);
        let [arming] = bin.armings.as_slice() else {
            panic!("one arming: {:?}", bin.armings);
        };
        let armed_us = arming.to_us - arming.from_us;
        assert!((38_500_000..=41_500_000).contains(&armed_us), "{arming:?}");
        assert!(arming.from_us > bin.from_us && arming.to_us <= bin.to_us);
        assert_eq!(arming.modes, ["GUIDED", "RTL"]);
        let height = arming.max_height_m.unwrap();
        assert!((15.0..=25.0).contains(&height), "{arming:?}");
        assert!(arming.air_us > 0 && arming.air_us < arming.to_us - arming.from_us);
        assert!(arming.max_speed_mps.is_some(), "{arming:?}");
        let (start, end) = (
            arming.battery_start_pct.unwrap(),
            arming.battery_end_pct.unwrap(),
        );
        assert!(start >= end, "{arming:?}");
    }

    /// A converted DataFlash `POS` has no velocity, so its position's
    /// ground speed reads 0 between `GPS` records. A plane flying at
    /// 20 m/s is moving all the while, as the live Logbook counts it.
    #[test]
    fn a_position_without_velocity_does_not_stop_the_moving_time() {
        use uxduck_core::domain::telemetry::{
            PositionEstimate, TelemetryItem, Unit, VehicleTelemetry,
        };
        let speed = |v: f64| VehicleTelemetry {
            items: vec![TelemetryItem {
                key: "speed.ground_mps".into(),
                label: "Ground speed".into(),
                unit: Unit::MeterPerSecond,
                value: Some(v),
                text: None,
                source: "log".into(),
                component_id: 1,
                time_us: 0,
            }],
            ..VehicleTelemetry::default()
        };
        let position = VehicleTelemetry {
            positions: vec![PositionEstimate {
                source: "fused".into(),
                component_id: 1,
                latitude_deg: 51.5,
                longitude_deg: -0.1,
                altitudes: Vec::new(),
                heading_deg: None,
                course_deg: None,
                ground_speed_mps: Some(0.0),
                climb_mps: None,
                horizontal_accuracy_m: None,
                vertical_accuracy_m: None,
                time_us: 0,
            }],
            ..VehicleTelemetry::default()
        };
        let (mut read, mut mode) = (Read::default(), None);
        let mut open = Some(Open::default());
        for (at, update) in [(1, speed(20.0)), (2, position.clone()), (3, position)] {
            take(
                &mut read,
                &mut open,
                &mut mode,
                vec![Effect::Telemetry(update)],
                at * 1_000_000,
                1,
            );
        }
        let arming = close(open.unwrap(), 4_000_000);
        assert_eq!(arming.moving_us, 3_000_000, "{arming:?}");
    }

    #[test]
    fn a_ulg_says_its_arming() {
        let ulg = span(&crate::ulog::fixture::flight()).unwrap();
        assert!(!ulg.armings.is_empty(), "{ulg:?}");
        assert!(ulg.armings.iter().all(|a| a.from_us < a.to_us));
    }

    #[test]
    fn anything_else_says_what_it_is_not() {
        assert_eq!(
            span(b"not a log at all").unwrap_err(),
            "This is not an ArduPilot .bin or a PX4 .ulg log."
        );
    }
}
