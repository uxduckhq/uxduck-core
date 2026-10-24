//! Other aircraft, as a source reported them (ADR-0034): one report of one
//! aircraft, in SI units, whichever way it came in. The engine merges
//! reports from every source a person turned on into one picture.

use serde::{Deserialize, Serialize};

/// What kind of aircraft (ADS-B's emitter category, widened for FLARM,
/// Remote ID and ships later).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrafficCategory {
    #[default]
    Unknown,
    /// Under 7,000 kg: most light aircraft.
    Light,
    /// 7,000 to 34,000 kg.
    Small,
    /// 34,000 to 136,000 kg, airliners among them.
    Large,
    /// Over 136,000 kg.
    Heavy,
    /// Aerobatic, fast or highly manoeuvrable.
    Fast,
    Rotorcraft,
    Glider,
    /// Balloons and airships.
    Balloon,
    Parachute,
    Ultralight,
    /// A drone.
    Uav,
    Space,
    /// A vehicle on the ground at an airport.
    Surface,
    /// A tall fixed thing, such as a tethered balloon or a mast.
    Obstacle,
}

/// How the aircraft's position reached the receiver.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrafficProtocol {
    /// ADS-B on 1090 MHz.
    #[default]
    Adsb,
    /// ADS-B on 978 MHz (UAT), in the USA.
    Uat,
}

/// Which altitude an aircraft reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AltitudeFrame {
    /// Barometric, from its altimeter.
    Pressure,
    /// From its GNSS, above mean sea level.
    Geometric,
}

/// One aircraft as one source last heard it.
#[derive(Clone, Debug, PartialEq)]
pub struct TrafficReport {
    /// ICAO 24-bit address.
    pub address: u32,
    pub callsign: Option<String>,
    pub squawk: Option<u16>,
    pub category: TrafficCategory,
    pub protocol: TrafficProtocol,
    /// Absent when the aircraft gave no position (an altitude-only target).
    pub position: Option<(f64, f64)>,
    pub altitude_m: Option<f64>,
    pub altitude_frame: Option<AltitudeFrame>,
    /// Degrees from true north, 0 to 360.
    pub track_deg: Option<f64>,
    pub ground_speed_mps: Option<f64>,
    /// Up is positive.
    pub climb_mps: Option<f64>,
    /// The receiver marked it as made up, such as a simulator's.
    pub simulated: bool,
    /// When the aircraft was last heard, in the engine's clock (µs): the
    /// time it arrived less how long ago the source says it heard it.
    pub heard_us: u64,
}
