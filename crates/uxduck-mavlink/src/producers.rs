//! Producers turn frames into the generic outputs, `PositionEstimate` and
//! `TelemetryItem` (ADR-0006). The core's own reducers (the autopilot's fused
//! estimate, raw GPS, attitude, power, the HUD) are producers too, behind the
//! same interface a plugin's estimator uses, so the map and panels never know
//! which producer was native.

use std::f64::consts::PI;

use uxduck_core::domain::telemetry::{
    Altitude, AltitudeFrame, PositionEstimate, TelemetryItem, Unit,
};

use crate::decode::Message;

/// What producers wrote while handling one frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Produced {
    pub positions: Vec<PositionEstimate>,
    pub items: Vec<TelemetryItem>,
}

impl Produced {
    fn item(&mut self, key: &str, label: &str, unit: Unit, value: f64, from: (&str, u8, u64)) {
        let (source, component_id, time_us) = from;
        self.items.push(TelemetryItem {
            key: key.into(),
            label: label.into(),
            unit,
            value: Some(value),
            text: None,
            source: source.into(),
            component_id,
            time_us,
        });
    }

    fn text(&mut self, key: &str, label: &str, text: &str, from: (&str, u8, u64)) {
        let (source, component_id, time_us) = from;
        self.items.push(TelemetryItem {
            key: key.into(),
            label: label.into(),
            unit: Unit::None,
            value: None,
            text: Some(text.into()),
            source: source.into(),
            component_id,
            time_us,
        });
    }
}

/// Turns decoded frames from one vehicle into positions and items.
pub trait Producer: std::fmt::Debug + Send + Sync {
    /// Stable; the `source` of everything it produces.
    fn id(&self) -> &'static str;
    fn consume(&self, component_id: u8, message: &Message, now_us: u64, out: &mut Produced);
}

/// The core producers, in the order they run.
#[must_use]
pub fn core_producers() -> Vec<Box<dyn Producer>> {
    vec![
        Box::new(Fused),
        Box::new(Gps),
        Box::new(AttitudeProducer),
        Box::new(Power),
        Box::new(Hud),
        Box::new(MissionProgress),
        Box::new(Navigation),
        Box::new(Home),
        Box::new(Servos),
        Box::new(Health),
        Box::new(Terrain),
        Box::new(Gimbal),
    ]
}

fn degrees_e7(value: i32) -> f64 {
    f64::from(value) / 1e7
}

fn known_u16(value: u16, scale: f64) -> Option<f64> {
    (value != u16::MAX).then(|| f64::from(value) * scale)
}

/// Below this ground speed, in m/s, the direction of travel is noise.
const MIN_COURSE_MPS: f64 = 0.5;

/// The direction of travel from a velocity north and east, in degrees from
/// true north, or none when too slow to have one.
fn course(north_mps: f64, east_mps: f64) -> Option<f64> {
    (north_mps.hypot(east_mps) >= MIN_COURSE_MPS)
        .then(|| east_mps.atan2(north_mps).to_degrees().rem_euclid(360.0))
}

/// `GLOBAL_POSITION_INT`: the autopilot's own estimate, source `fused`.
#[derive(Debug)]
pub struct Fused;

impl Producer for Fused {
    fn id(&self) -> &'static str {
        "fused"
    }

    fn consume(&self, component_id: u8, message: &Message, now_us: u64, out: &mut Produced) {
        let &Message::GlobalPositionInt {
            lat,
            lon,
            alt,
            relative_alt,
            vx,
            vy,
            vz,
            hdg,
            ..
        } = message
        else {
            return;
        };
        if lat == 0 && lon == 0 {
            return; // No estimate yet.
        }
        let (north, east) = (f64::from(vx) / 100.0, f64::from(vy) / 100.0);
        out.positions.push(PositionEstimate {
            source: self.id().into(),
            component_id,
            latitude_deg: degrees_e7(lat),
            longitude_deg: degrees_e7(lon),
            altitudes: vec![
                Altitude {
                    meters: f64::from(alt) / 1000.0,
                    frame: AltitudeFrame::Amsl,
                },
                Altitude {
                    meters: f64::from(relative_alt) / 1000.0,
                    frame: AltitudeFrame::AboveHome,
                },
            ],
            heading_deg: known_u16(hdg, 0.01),
            course_deg: course(north, east),
            ground_speed_mps: Some(north.hypot(east)),
            climb_mps: Some(-f64::from(vz) / 100.0),
            horizontal_accuracy_m: None,
            vertical_accuracy_m: None,
            time_us: now_us,
        });
    }
}

/// The words for `GPS_FIX_TYPE`.
#[must_use]
pub fn fix_text(fix_type: u8) -> &'static str {
    match fix_type {
        0 => "No GPS",
        1 => "No fix",
        2 => "2D",
        3 => "3D",
        4 => "DGPS",
        5 => "RTK float",
        6 => "RTK fixed",
        7 => "Static",
        8 => "PPP",
        _ => "Unknown",
    }
}

/// `GPS_RAW_INT`: the receiver's own position, source `gps`, plus fix and
/// satellites.
#[derive(Debug)]
pub struct Gps;

impl Producer for Gps {
    fn id(&self) -> &'static str {
        "gps"
    }

    fn consume(&self, component_id: u8, message: &Message, now_us: u64, out: &mut Produced) {
        let &Message::GpsRawInt {
            fix_type,
            lat,
            lon,
            alt,
            eph,
            epv,
            vel,
            cog,
            satellites_visible,
            alt_ellipsoid,
            h_acc,
            v_acc,
        } = message
        else {
            return;
        };
        let from = (self.id(), component_id, now_us);
        out.text("gps.fix", "GPS fix", fix_text(fix_type), from);
        // The same as a number, for colour rules (S65): 0 none, 3 a 3D fix.
        out.item(
            "gps.fix_type",
            "GPS fix type",
            Unit::Count,
            f64::from(fix_type),
            from,
        );
        if satellites_visible != u8::MAX {
            out.item(
                "gps.satellites",
                "Satellites",
                Unit::Count,
                f64::from(satellites_visible),
                from,
            );
        }
        if let Some(hdop) = known_u16(eph, 0.01) {
            out.item("gps.hdop", "HDOP", Unit::None, hdop, from);
        }
        if let Some(vdop) = known_u16(epv, 0.01) {
            out.item("gps.vdop", "VDOP", Unit::None, vdop, from);
        }
        if fix_type < 2 {
            return;
        }
        let mut altitudes = vec![Altitude {
            meters: f64::from(alt) / 1000.0,
            frame: AltitudeFrame::Amsl,
        }];
        if alt_ellipsoid != 0 {
            altitudes.push(Altitude {
                meters: f64::from(alt_ellipsoid) / 1000.0,
                frame: AltitudeFrame::Ellipsoid,
            });
        }
        out.positions.push(PositionEstimate {
            source: self.id().into(),
            component_id,
            latitude_deg: degrees_e7(lat),
            longitude_deg: degrees_e7(lon),
            altitudes,
            heading_deg: known_u16(cog, 0.01),
            course_deg: known_u16(cog, 0.01),
            ground_speed_mps: known_u16(vel, 0.01),
            climb_mps: None,
            horizontal_accuracy_m: (h_acc != 0).then(|| f64::from(h_acc) / 1000.0),
            vertical_accuracy_m: (v_acc != 0).then(|| f64::from(v_acc) / 1000.0),
            time_us: now_us,
        });
    }
}

/// `ATTITUDE`: roll, pitch and yaw in degrees.
#[derive(Debug)]
pub struct AttitudeProducer;

impl Producer for AttitudeProducer {
    fn id(&self) -> &'static str {
        "attitude"
    }

    fn consume(&self, component_id: u8, message: &Message, now_us: u64, out: &mut Produced) {
        let &Message::Attitude {
            roll, pitch, yaw, ..
        } = message
        else {
            return;
        };
        let from = (self.id(), component_id, now_us);
        let degrees = |radians: f32| f64::from(radians) * 180.0 / PI;
        out.item(
            "attitude.roll_deg",
            "Roll",
            Unit::Degree,
            degrees(roll),
            from,
        );
        out.item(
            "attitude.pitch_deg",
            "Pitch",
            Unit::Degree,
            degrees(pitch),
            from,
        );
        // Yaw as a compass heading, 0 to 360.
        out.item(
            "attitude.yaw_deg",
            "Yaw",
            Unit::Degree,
            degrees(yaw).rem_euclid(360.0),
            from,
        );
    }
}

/// `SYS_STATUS`: battery voltage, current and remaining, and whether the
/// pre-arm checks pass.
#[derive(Debug)]
pub struct Power;

impl Producer for Power {
    fn id(&self) -> &'static str {
        "power"
    }

    fn consume(&self, component_id: u8, message: &Message, now_us: u64, out: &mut Produced) {
        let &Message::SysStatus {
            voltage_battery,
            current_battery,
            battery_remaining,
            prearm,
        } = message
        else {
            return;
        };
        let from = (self.id(), component_id, now_us);
        // 1 when the pre-arm checks pass: what the simulation start waits
        // for before it arms (S33).
        if let Some(ready) = prearm {
            let value = if ready { 1.0 } else { 0.0 };
            out.item("arming.ready", "Ready to arm", Unit::None, value, from);
        }
        if let Some(volts) = known_u16(voltage_battery, 0.001) {
            out.item("battery.voltage_v", "Battery", Unit::Volt, volts, from);
        }
        if current_battery >= 0 {
            out.item(
                "battery.current_a",
                "Current",
                Unit::Ampere,
                f64::from(current_battery) / 100.0,
                from,
            );
        }
        if battery_remaining >= 0 {
            out.item(
                "battery.remaining_pct",
                "Remaining",
                Unit::Percent,
                f64::from(battery_remaining),
                from,
            );
        }
    }
}

/// `VFR_HUD`: speeds, heading, throttle and climb.
#[derive(Debug)]
pub struct Hud;

impl Producer for Hud {
    fn id(&self) -> &'static str {
        "hud"
    }

    fn consume(&self, component_id: u8, message: &Message, now_us: u64, out: &mut Produced) {
        let &Message::VfrHud {
            airspeed,
            groundspeed,
            heading,
            throttle,
            climb,
        } = message
        else {
            return;
        };
        let from = (self.id(), component_id, now_us);
        out.item(
            "speed.air_mps",
            "Airspeed",
            Unit::MeterPerSecond,
            f64::from(airspeed),
            from,
        );
        out.item(
            "speed.ground_mps",
            "Ground speed",
            Unit::MeterPerSecond,
            f64::from(groundspeed),
            from,
        );
        out.item(
            "heading.true_deg",
            "Heading",
            Unit::Degree,
            f64::from(heading),
            from,
        );
        out.item(
            "throttle.output_pct",
            "Throttle",
            Unit::Percent,
            f64::from(throttle),
            from,
        );
        out.item(
            "speed.climb_mps",
            "Climb",
            Unit::MeterPerSecond,
            f64::from(climb),
            from,
        );
    }
}

/// `MISSION_CURRENT`: the mission item it is flying to, which a simulation
/// snapshot keeps (S34).
#[derive(Debug)]
pub struct MissionProgress;

impl Producer for MissionProgress {
    fn id(&self) -> &'static str {
        "mission"
    }

    fn consume(&self, component_id: u8, message: &Message, now_us: u64, out: &mut Produced) {
        let &Message::MissionCurrent { seq, state, mode } = message else {
            return;
        };
        let from = (self.id(), component_id, now_us);
        out.item(
            "mission.current",
            "Mission item",
            Unit::None,
            f64::from(seq),
            from,
        );
        out.item(
            "mission.state",
            "Mission state",
            Unit::None,
            f64::from(state),
            from,
        );
        out.item(
            "mission.mode",
            "Mission execution mode",
            Unit::None,
            f64::from(mode),
            from,
        );
    }
}

/// `NAV_CONTROLLER_OUTPUT`: the distance to the waypoint and how far off the
/// line to it the vehicle is (S65); `POSITION_TARGET_GLOBAL_INT`: the place
/// it is going to, which the map joins to it in GUIDED.
#[derive(Debug)]
pub struct Navigation;

impl Producer for Navigation {
    fn id(&self) -> &'static str {
        "navigation"
    }

    fn consume(&self, component_id: u8, message: &Message, now_us: u64, out: &mut Produced) {
        let from = (self.id(), component_id, now_us);
        match *message {
            Message::NavControllerOutput {
                wp_dist,
                xtrack_error,
            } => {
                out.item(
                    "navigation.waypoint_distance_m",
                    "To waypoint",
                    Unit::Meter,
                    f64::from(wp_dist),
                    from,
                );
                if xtrack_error.is_finite() {
                    out.item(
                        "navigation.crosstrack_error_m",
                        "Off route",
                        Unit::Meter,
                        f64::from(xtrack_error),
                        from,
                    );
                }
            }
            // Bits 0 and 1 must be clear: latitude and longitude are used.
            Message::PositionTarget {
                latitude,
                longitude,
                altitude,
                coordinate_frame,
                type_mask,
            } if type_mask.trailing_zeros() >= 2 && (latitude, longitude) != (0, 0) => {
                out.item(
                    "navigation.target_latitude_deg",
                    "Target latitude",
                    Unit::Degree,
                    degrees_e7(latitude),
                    from,
                );
                out.item(
                    "navigation.target_longitude_deg",
                    "Target longitude",
                    Unit::Degree,
                    degrees_e7(longitude),
                    from,
                );
                let frame = match coordinate_frame {
                    0 | 5 => "amsl",
                    3 | 6 => "above-home",
                    10 | 11 => "above-ground",
                    _ => "",
                };
                out.item(
                    "navigation.target_altitude_m",
                    "Target altitude",
                    Unit::Meter,
                    f64::from(altitude),
                    from,
                );
                // Clear a previous altitude when this target ignores it or
                // uses an unsupported frame; its horizontal position still stands.
                if (type_mask & 4 != 0 || !altitude.is_finite() || frame.is_empty())
                    && let Some(item) = out.items.last_mut()
                {
                    item.value = None;
                }
                out.text(
                    "navigation.target_altitude_frame",
                    "Target altitude frame",
                    frame,
                    from,
                );
            }
            _ => {}
        }
    }
}

/// `HOME_POSITION`: where home is, from which the app works out the
/// distance home (S65).
#[derive(Debug)]
pub struct Home;

impl Producer for Home {
    fn id(&self) -> &'static str {
        "home"
    }

    fn consume(&self, component_id: u8, message: &Message, now_us: u64, out: &mut Produced) {
        let &Message::HomePosition {
            latitude,
            longitude,
            altitude,
        } = message
        else {
            return;
        };
        let from = (self.id(), component_id, now_us);
        out.item(
            "home.latitude_deg",
            "Home latitude",
            Unit::Degree,
            degrees_e7(latitude),
            from,
        );
        out.item(
            "home.longitude_deg",
            "Home longitude",
            Unit::Degree,
            degrees_e7(longitude),
            from,
        );
        out.item(
            "home.altitude_msl_m",
            "Home altitude",
            Unit::Meter,
            f64::from(altitude) / 1000.0,
            from,
        );
    }
}

/// `SERVO_OUTPUT_RAW`: the outputs in use, as `servo.1` to `servo.8` in
/// microseconds, which the 3D view turns into control surfaces and
/// propellers (S35).
#[derive(Debug)]
pub struct Servos;

impl Producer for Servos {
    fn id(&self) -> &'static str {
        "servos"
    }

    fn consume(&self, component_id: u8, message: &Message, now_us: u64, out: &mut Produced) {
        let Message::ServoOutputs { servos } = message else {
            return;
        };
        let from = (self.id(), component_id, now_us);
        for (index, pulse) in servos.iter().enumerate().filter(|(_, p)| **p > 0) {
            let n = index + 1;
            out.item(
                &format!("servo.{n}"),
                &format!("Servo {n}"),
                Unit::None,
                f64::from(*pulse),
                from,
            );
        }
    }
}

/// `TERRAIN_REPORT`: the ground's height above mean sea level under the
/// vehicle, from the autopilot's terrain data (maintainer, 2026-10-05).
/// The vehicle's own height above it joins the fused position as its
/// above-ground altitude in the session, which remembers the terrain
/// between reports; nothing is produced while the autopilot has no data
/// for where it is.
#[derive(Debug)]
pub struct Terrain;

impl Producer for Terrain {
    fn id(&self) -> &'static str {
        "terrain"
    }

    fn consume(&self, component_id: u8, message: &Message, now_us: u64, out: &mut Produced) {
        let &Message::TerrainReport {
            spacing,
            terrain_height,
            pending,
            loaded,
            ..
        } = message
        else {
            return;
        };
        // How far the autopilot's terrain is from complete around it
        // (S118): 4 × 4 groups still to come, and held.
        out.item(
            "terrain.pending",
            "Terrain pending",
            Unit::Count,
            f64::from(pending),
            (self.id(), component_id, now_us),
        );
        out.item(
            "terrain.loaded",
            "Terrain loaded",
            Unit::Count,
            f64::from(loaded),
            (self.id(), component_id, now_us),
        );
        if spacing == 0 || !terrain_height.is_finite() {
            return;
        }
        out.item(
            "terrain.altitude_msl_m",
            "Terrain altitude",
            Unit::Meter,
            f64::from(terrain_height),
            (self.id(), component_id, now_us),
        );
    }
}

/// The values people check before and during a flight (S72): link quality
/// from the radio, the EKF, vibration, battery used, the rangefinder, wind,
/// a submarine's water temperature, and leaks it reports.
#[derive(Debug)]
pub struct Health;

/// `MAV_SENSOR_ROTATION_PITCH_270`: pointing down.
const DOWNWARD: u8 = 25;

impl Producer for Health {
    fn id(&self) -> &'static str {
        "health"
    }

    fn consume(&self, component_id: u8, message: &Message, now_us: u64, out: &mut Produced) {
        let from = (self.id(), component_id, now_us);
        match message {
            &Message::RadioStatus {
                rssi,
                remrssi,
                noise,
                remnoise,
                rxerrors,
                fixed,
            } => radio([rssi, remrssi, noise, remnoise], rxerrors, fixed, from, out),
            &Message::EkfStatus { flags, variances } => ekf(flags, variances, from, out),
            // The fence (S132): outside it now, and breaches since boot.
            &Message::FenceStatus { breached, count } => {
                out.item(
                    "fence.breached",
                    "Fence breached",
                    Unit::None,
                    if breached { 1.0 } else { 0.0 },
                    from,
                );
                out.item(
                    "fence.breaches",
                    "Fence breaches",
                    Unit::Count,
                    f64::from(count),
                    from,
                );
            }
            &Message::Vibration { axes, clipping } => vibration(axes, clipping, from, out),
            &Message::BatteryStatus {
                id,
                consumed_mah,
                cells_mv,
                temperature_cdeg,
            } => battery(id, consumed_mah, &cells_mv, temperature_cdeg, from, out),
            &Message::DistanceSensor {
                distance_cm,
                orientation,
            } if orientation == DOWNWARD => {
                rangefinder(f64::from(distance_cm) / 100.0, from, out);
            }
            &Message::Rangefinder { distance } if distance.is_finite() => {
                rangefinder(f64::from(distance), from, out);
            }
            &Message::Wind {
                direction,
                speed,
                speed_z,
            } => {
                out.item(
                    "wind.speed_mps",
                    "Wind",
                    Unit::MeterPerSecond,
                    f64::from(speed),
                    from,
                );
                out.item(
                    "wind.direction_deg",
                    "Wind from",
                    Unit::Degree,
                    f64::from(direction).rem_euclid(360.0),
                    from,
                );
                out.item(
                    "wind.vertical_mps",
                    "Vertical wind",
                    Unit::MeterPerSecond,
                    f64::from(speed_z),
                    from,
                );
            }
            &Message::ExtendedSysState { landed_state } => {
                // 0 is "undefined": the autopilot does not know, so say nothing.
                if let Some(state) = uxduck_core::domain::telemetry::LANDED_STATES
                    .get(usize::from(landed_state).wrapping_sub(1))
                {
                    out.text("flight.state", "Flight state", state, from);
                }
            }
            &Message::ScaledPressure2 { temperature_cdeg } => {
                out.item(
                    "pressure_2.temperature_degc",
                    "Second barometer temperature",
                    Unit::DegreeCelsius,
                    f64::from(temperature_cdeg) / 100.0,
                    from,
                );
            }
            // `ArduSub` says "Leak Detected!" (S72).
            Message::StatusText { text, .. } if text.to_lowercase().contains("leak detected") => {
                out.item("leak.detected", "Leak", Unit::None, 1.0, from);
            }
            _ => {}
        }
    }
}

/// Signal and noise here and at the far end, as 0 to 254 (255 unknown).
fn radio(levels: [u8; 4], rxerrors: u16, fixed: u16, from: (&str, u8, u64), out: &mut Produced) {
    let [rssi, remrssi, noise, remnoise] = levels;
    // 0 to 254 in the radio's own scale, as a percentage.
    let pct = |v: u8| f64::from(v) * 100.0 / 254.0;
    if rssi != u8::MAX {
        out.item(
            "link.rssi_pct",
            "Radio signal",
            Unit::Percent,
            pct(rssi),
            from,
        );
    }
    if remrssi != u8::MAX {
        out.item(
            "link.remote_rssi_pct",
            "Remote radio signal",
            Unit::Percent,
            pct(remrssi),
            from,
        );
    }
    if noise != u8::MAX {
        out.item(
            "link.noise",
            "Radio noise",
            Unit::None,
            f64::from(noise),
            from,
        );
    }
    if remnoise != u8::MAX {
        out.item(
            "link.remote_noise",
            "Remote radio noise",
            Unit::None,
            f64::from(remnoise),
            from,
        );
    }
    out.item(
        "link.rx_errors",
        "Radio receive errors",
        Unit::Count,
        f64::from(rxerrors),
        from,
    );
    out.item(
        "link.fixed",
        "Radio errors corrected",
        Unit::Count,
        f64::from(fixed),
        from,
    );
}

fn ekf(flags: u16, variances: [f32; 5], from: (&str, u8, u64), out: &mut Produced) {
    let names = [
        ("ekf.velocity_variance", "EKF velocity variance"),
        ("ekf.position_variance", "EKF position variance"),
        ("ekf.height_variance", "EKF height variance"),
        ("ekf.compass_variance", "EKF compass variance"),
        ("ekf.terrain_variance", "EKF terrain variance"),
    ];
    for ((key, label), value) in names.iter().zip(variances) {
        out.item(key, label, Unit::None, f64::from(value), from);
    }
    // The worst, as Mission Planner's EKF indicator shows it.
    let worst = variances.iter().copied().fold(0.0_f32, f32::max);
    out.item("ekf.variance", "EKF", Unit::None, f64::from(worst), from);
    out.text("ekf.status", "EKF state", ekf_text(flags), from);
}

fn vibration(axes: [f32; 3], clipping: [u32; 3], from: (&str, u8, u64), out: &mut Produced) {
    for (axis, value) in ["x", "y", "z"].iter().zip(axes) {
        out.item(
            &format!("vibration.{axis}_mps2"),
            &format!("Vibration {}", axis.to_uppercase()),
            Unit::MeterPerSecondSquared,
            f64::from(value),
            from,
        );
    }
    let worst = axes.iter().copied().fold(0.0_f32, f32::max);
    out.item(
        "vibration.max_mps2",
        "Vibration",
        Unit::MeterPerSecondSquared,
        f64::from(worst),
        from,
    );
    out.item(
        "vibration.clipping",
        "Accelerometer clipping",
        Unit::Count,
        clipping.iter().map(|c| f64::from(*c)).sum(),
        from,
    );
}

/// Where the vehicle's gimbal points (maintainer, 2026-10-18), so the 3D
/// map's Drone camera can look that way without KLV: its pitch, up
/// positive, and its yaw, from the nose and right positive, or from north
/// when that is what the gimbal says.
#[derive(Debug)]
pub struct Gimbal;

/// `GIMBAL_DEVICE_FLAGS`: yaw held to north, and which frame the yaw is in.
const GIMBAL_YAW_LOCK: u16 = 16;
const GIMBAL_YAW_IN_VEHICLE_FRAME: u16 = 128;
const GIMBAL_YAW_IN_EARTH_FRAME: u16 = 256;

impl Producer for Gimbal {
    fn id(&self) -> &'static str {
        "gimbal"
    }

    fn consume(&self, component_id: u8, message: &Message, now_us: u64, out: &mut Produced) {
        let from = (self.id(), component_id, now_us);
        match *message {
            Message::GimbalAttitude { flags, q } => {
                if q.iter().any(|c| !c.is_finite()) {
                    return;
                }
                let [w, x, y, z] = q.map(f64::from);
                let pitch = (2.0 * (w * y - z * x)).clamp(-1.0, 1.0).asin().to_degrees();
                let yaw = (2.0 * (w * z + x * y))
                    .atan2(1.0 - 2.0 * (y * y + z * z))
                    .to_degrees();
                out.item(
                    "gimbal.pitch_deg",
                    "Gimbal pitch",
                    Unit::Degree,
                    pitch,
                    from,
                );
                // Neither frame said (before MAVLink had the flags): north
                // only while it holds its yaw there.
                let north = flags & GIMBAL_YAW_IN_EARTH_FRAME != 0
                    || (flags & GIMBAL_YAW_IN_VEHICLE_FRAME == 0 && flags & GIMBAL_YAW_LOCK != 0);
                if north {
                    out.item(
                        "gimbal.heading_deg",
                        "Gimbal heading",
                        Unit::Degree,
                        yaw.rem_euclid(360.0),
                        from,
                    );
                } else {
                    out.item("gimbal.yaw_deg", "Gimbal yaw", Unit::Degree, yaw, from);
                }
            }
            Message::MountStatus {
                pitch_cdeg,
                yaw_cdeg,
            } => {
                let degrees = |cdeg: i32| f64::from(cdeg) / 100.0;
                out.item(
                    "gimbal.pitch_deg",
                    "Gimbal pitch",
                    Unit::Degree,
                    degrees(pitch_cdeg),
                    from,
                );
                out.item(
                    "gimbal.yaw_deg",
                    "Gimbal yaw",
                    Unit::Degree,
                    degrees(yaw_cdeg),
                    from,
                );
            }
            _ => {}
        }
    }
}

fn rangefinder(metres: f64, from: (&str, u8, u64), out: &mut Produced) {
    out.item(
        "rangefinder.distance_m",
        "Rangefinder",
        Unit::Meter,
        metres,
        from,
    );
}

/// Battery used, cells and temperature; a second battery's keys say which.
fn battery(
    id: u8,
    consumed_mah: i32,
    cells_mv: &[u16; 10],
    temperature_cdeg: i16,
    from: (&str, u8, u64),
    out: &mut Produced,
) {
    let (group, name) = if id == 0 {
        ("battery".to_owned(), "Battery".to_owned())
    } else {
        (format!("battery_{}", id + 1), format!("Battery {}", id + 1))
    };
    if consumed_mah >= 0 {
        out.item(
            &format!("{group}.consumed_mah"),
            &format!("{name} used"),
            Unit::MilliampHour,
            f64::from(consumed_mah),
            from,
        );
    }
    // Only the total in the first is not per cell.
    let cells: Vec<u16> = cells_mv
        .iter()
        .copied()
        .take_while(|mv| *mv != u16::MAX)
        .collect();
    if cells.len() > 1 {
        for (i, mv) in cells.iter().enumerate() {
            let n = i + 1;
            out.item(
                &format!("{group}.cell_{n}_v"),
                &format!("{name} cell {n}"),
                Unit::Volt,
                f64::from(*mv) / 1000.0,
                from,
            );
        }
    }
    if temperature_cdeg != i16::MAX {
        out.item(
            &format!("{group}.temperature_degc"),
            &format!("{name} temperature"),
            Unit::DegreeCelsius,
            f64::from(temperature_cdeg) / 100.0,
            from,
        );
    }
}

/// `EKF_STATUS_FLAGS` as one state, the worst first.
#[must_use]
pub fn ekf_text(flags: u16) -> &'static str {
    const ATTITUDE: u16 = 1;
    const VELOCITY_HORIZ: u16 = 2;
    const POS_HORIZ_ABS: u16 = 16;
    const CONST_POS_MODE: u16 = 128;
    const UNINITIALIZED: u16 = 1024;
    const GPS_GLITCHING: u16 = 32768;
    if flags & UNINITIALIZED != 0 {
        "Not started"
    } else if flags & ATTITUDE == 0 {
        "No attitude"
    } else if flags & GPS_GLITCHING != 0 {
        "GPS glitching"
    } else if flags & CONST_POS_MODE != 0 {
        "No position"
    } else if flags & (VELOCITY_HORIZ | POS_HORIZ_ABS) != VELOCITY_HORIZ | POS_HORIZ_ABS {
        "Partial"
    } else {
        "Good"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn produced(message: &Message) -> Vec<(String, f64)> {
        let mut out = Produced::default();
        for producer in core_producers() {
            producer.consume(1, message, 5, &mut out);
        }
        out.items
            .into_iter()
            .map(|i| (i.key, i.value.unwrap_or(f64::NAN)))
            .collect()
    }

    #[test]
    fn mission_progress_keeps_the_reported_execution_state() {
        for state in [0, 2, 3, 4, 5] {
            let items = produced(&Message::MissionCurrent {
                seq: 4,
                state,
                mode: 1,
            });
            assert!(items.contains(&("mission.current".into(), 4.0)));
            assert!(items.contains(&("mission.state".into(), f64::from(state))));
            assert!(items.contains(&("mission.mode".into(), 1.0)));
        }
    }

    #[test]
    fn a_gimbal_says_where_it_points_from_the_nose_or_from_north() {
        // Tilted 30° down and turned 45° right: yaw 45°, then pitch -30°.
        let (half_yaw, half_pitch) = (45f64.to_radians() / 2.0, -30f64.to_radians() / 2.0);
        #[allow(clippy::cast_possible_truncation, reason = "a test quaternion")]
        let q = [
            (half_yaw.cos() * half_pitch.cos()) as f32,
            (-half_yaw.sin() * half_pitch.sin()) as f32,
            (half_yaw.cos() * half_pitch.sin()) as f32,
            (half_yaw.sin() * half_pitch.cos()) as f32,
        ];
        let near = |items: &[(String, f64)], key: &str, want: f64| {
            let got = items.iter().find(|(k, _)| k == key).map(|(_, v)| *v);
            assert!(
                got.is_some_and(|v| (v - want).abs() < 0.01),
                "{key}: {got:?}"
            );
        };
        let nose = produced(&Message::GimbalAttitude { flags: 128, q });
        near(&nose, "gimbal.pitch_deg", -30.0);
        near(&nose, "gimbal.yaw_deg", 45.0);
        assert!(!nose.iter().any(|(k, _)| k == "gimbal.heading_deg"));
        let north = produced(&Message::GimbalAttitude { flags: 256, q });
        near(&north, "gimbal.heading_deg", 45.0);
        assert!(!north.iter().any(|(k, _)| k == "gimbal.yaw_deg"));
        // Older flags: north only while the yaw is locked there.
        near(
            &produced(&Message::GimbalAttitude { flags: 16, q }),
            "gimbal.heading_deg",
            45.0,
        );
        near(
            &produced(&Message::GimbalAttitude { flags: 0, q }),
            "gimbal.yaw_deg",
            45.0,
        );
        // No attitude yet: nothing.
        let unknown = [f32::NAN; 4];
        assert!(
            produced(&Message::GimbalAttitude {
                flags: 128,
                q: unknown
            })
            .is_empty()
        );
        let mount = produced(&Message::MountStatus {
            pitch_cdeg: -9000,
            yaw_cdeg: -1250,
        });
        near(&mount, "gimbal.pitch_deg", -90.0);
        near(&mount, "gimbal.yaw_deg", -12.5);
    }

    #[test]
    fn the_landed_state_is_said_in_words_and_not_when_unknown() {
        let state = |landed_state| {
            let mut out = Produced::default();
            Health.consume(1, &Message::ExtendedSysState { landed_state }, 5, &mut out);
            out.items.into_iter().find_map(|i| i.text)
        };
        assert_eq!(state(0), None);
        assert_eq!(state(1).as_deref(), Some("On the ground"));
        assert_eq!(state(2).as_deref(), Some("In the air"));
        assert_eq!(state(4).as_deref(), Some("Landing"));
        assert_eq!(state(9), None);
    }

    #[test]
    fn the_direction_of_travel_comes_from_the_velocity() {
        let at = |north, east| {
            let mut out = Produced::default();
            Fused.consume(
                1,
                &Message::GlobalPositionInt {
                    time_boot_ms: 0,
                    lat: -353_632_610,
                    lon: 1_491_652_300,
                    alt: 584_000,
                    relative_alt: 10_000,
                    vx: north,
                    vy: east,
                    vz: 0,
                    hdg: 0,
                },
                5,
                &mut out,
            );
            out.positions[0].course_deg
        };
        // Pointing north and drifting east in the wind.
        let drift = at(1000, 100).unwrap();
        assert!((drift - 5.71).abs() < 0.01, "{drift}");
        assert!((at(-300, -300).unwrap() - 225.0).abs() < 1e-9);
        assert_eq!(at(20, 20), None, "too slow to have one");
    }

    #[test]
    fn the_waypoint_and_home_come_as_items() {
        let navigation = produced(&Message::NavControllerOutput {
            wp_dist: 120,
            xtrack_error: -3.5,
        });
        assert_eq!(
            navigation,
            [
                ("navigation.waypoint_distance_m".to_owned(), 120.0),
                ("navigation.crosstrack_error_m".to_owned(), -3.5),
            ]
        );
        let home = produced(&Message::HomePosition {
            latitude: -353_632_610,
            longitude: 1_491_652_300,
            altitude: 584_090,
        });
        assert_eq!(home[0].0, "home.latitude_deg");
        assert!((home[0].1 + 35.363_261).abs() < 1e-9);
        assert!((home[2].1 - 584.09).abs() < 1e-9);
    }

    #[test]
    fn the_terrain_under_the_vehicle_comes_as_an_item_only_with_data_and_counts_always() {
        let report = |spacing| {
            produced(&Message::TerrainReport {
                lat: 0,
                lon: 0,
                spacing,
                terrain_height: 580.5,
                current_height: 33.5,
                pending: 0,
                loaded: 56,
            })
        };
        let counts = [
            ("terrain.pending".to_owned(), 0.0),
            ("terrain.loaded".to_owned(), 56.0),
        ];
        let with = report(100);
        assert_eq!(with[..2], counts);
        assert_eq!(with[2], ("terrain.altitude_msl_m".to_owned(), 580.5));
        assert_eq!(
            report(0),
            counts,
            "no terrain data for where it is: counts only"
        );
    }

    #[test]
    fn the_guided_target_comes_as_items_when_it_is_a_place() {
        let target = |type_mask| {
            produced(&Message::PositionTarget {
                latitude: -353_620_000,
                longitude: 1_491_660_000,
                altitude: 80.0,
                coordinate_frame: 6,
                type_mask,
            })
            .into_iter()
            .filter(|(_, value)| value.is_finite())
            .collect::<Vec<_>>()
        };
        assert_eq!(
            target(0b1111_1111_1000),
            [
                ("navigation.target_latitude_deg".to_owned(), -35.362),
                ("navigation.target_longitude_deg".to_owned(), 149.166),
                ("navigation.target_altitude_m".to_owned(), 80.0),
            ]
        );
        assert!(
            target(0b1111_1100_0111).is_empty(),
            "a velocity, not a place"
        );
    }

    #[test]
    fn the_fence_says_a_breach_and_counts_them() {
        assert_eq!(
            produced(&Message::FenceStatus {
                breached: true,
                count: 2
            }),
            [
                ("fence.breached".to_owned(), 1.0),
                ("fence.breaches".to_owned(), 2.0)
            ]
        );
    }

    #[test]
    fn health_values_are_named_with_their_units() {
        let link = produced(&Message::RadioStatus {
            rssi: 127,
            remrssi: u8::MAX,
            noise: 40,
            remnoise: u8::MAX,
            rxerrors: 3,
            fixed: 1,
        });
        assert_eq!(link[0], ("link.rssi_pct".to_owned(), 50.0));
        assert_eq!(
            link.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            [
                "link.rssi_pct",
                "link.noise",
                "link.rx_errors",
                "link.fixed"
            ],
            "unknown values left out"
        );

        let ekf = produced(&Message::EkfStatus {
            flags: 0b1_1111_1111,
            variances: [0.1, 0.62, 0.2, 0.3, 0.0],
        });
        assert!(ekf.contains(&("ekf.variance".to_owned(), f64::from(0.62_f32))));
        assert_eq!(ekf_text(0b1_1111_1111 & !128), "Good");
        assert_eq!(ekf_text(1024), "Not started");
        assert_eq!(ekf_text(1 | 32768 | 2 | 16), "GPS glitching");

        let vibration = produced(&Message::Vibration {
            axes: [12.0, 64.5, 30.0],
            clipping: [0, 2, 5],
        });
        assert!(vibration.contains(&("vibration.max_mps2".to_owned(), 64.5)));
        assert!(vibration.contains(&("vibration.clipping".to_owned(), 7.0)));

        let mut cells = [u16::MAX; 10];
        cells[..3].copy_from_slice(&[4100, 4090, 4110]);
        let battery = produced(&Message::BatteryStatus {
            id: 0,
            consumed_mah: 1234,
            cells_mv: cells,
            temperature_cdeg: i16::MAX,
        });
        assert_eq!(battery[0], ("battery.consumed_mah".to_owned(), 1234.0));
        assert_eq!(battery[1], ("battery.cell_1_v".to_owned(), 4.1));
        assert_eq!(battery.len(), 4, "three cells, no temperature");
        let mut total = [u16::MAX; 10];
        total[0] = 12_600;
        let second = produced(&Message::BatteryStatus {
            id: 1,
            consumed_mah: -1,
            cells_mv: total,
            temperature_cdeg: 2550,
        });
        assert_eq!(
            second,
            [("battery_2.temperature_degc".to_owned(), 25.5)],
            "a total is not a cell; unknown use left out"
        );

        let down = produced(&Message::DistanceSensor {
            distance_cm: 1250,
            orientation: 25,
        });
        assert_eq!(down, [("rangefinder.distance_m".to_owned(), 12.5)]);
        let forward = produced(&Message::DistanceSensor {
            distance_cm: 500,
            orientation: 0,
        });
        assert!(forward.is_empty(), "only one pointing down is the height");

        let wind = produced(&Message::Wind {
            direction: -90.0,
            speed: 4.0,
            speed_z: 0.5,
        });
        assert_eq!(wind[1], ("wind.direction_deg".to_owned(), 270.0));
        let water = produced(&Message::ScaledPressure2 {
            temperature_cdeg: 1475,
        });
        assert_eq!(water, [("pressure_2.temperature_degc".to_owned(), 14.75)]);
        let leak = produced(&Message::StatusText {
            severity: 2,
            text: "Leak Detected!".into(),
            id: 0,
            chunk_seq: 0,
            full: false,
        });
        assert_eq!(leak, [("leak.detected".to_owned(), 1.0)]);
    }

    #[test]
    fn every_health_key_says_its_unit() {
        let mut out = Produced::default();
        let mut cells = [u16::MAX; 10];
        cells[..2].copy_from_slice(&[4100, 4100]);
        for message in [
            Message::RadioStatus {
                rssi: 1,
                remrssi: 1,
                noise: 1,
                remnoise: 1,
                rxerrors: 1,
                fixed: 1,
            },
            Message::EkfStatus {
                flags: 0,
                variances: [0.0; 5],
            },
            Message::Vibration {
                axes: [0.0; 3],
                clipping: [0; 3],
            },
            Message::BatteryStatus {
                id: 2,
                consumed_mah: 1,
                cells_mv: cells,
                temperature_cdeg: 1,
            },
            Message::Rangefinder { distance: 1.0 },
            Message::Wind {
                direction: 0.0,
                speed: 0.0,
                speed_z: 0.0,
            },
            Message::ScaledPressure2 {
                temperature_cdeg: 0,
            },
        ] {
            Health.consume(1, &message, 0, &mut out);
        }
        for item in &out.items {
            assert!(
                uxduck_core::domain::telemetry::key_says_unit(&item.key, item.unit),
                "{} as {:?}",
                item.key,
                item.unit
            );
        }
    }
}
