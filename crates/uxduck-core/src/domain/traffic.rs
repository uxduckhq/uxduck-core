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
    /// GNSS altitude above mean sea level, as MAVLink `ADSB_VEHICLE` specifies.
    Geometric,
    /// GNSS height above the WGS84 ellipsoid, such as readsb `alt_geom`.
    Ellipsoid,
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

/// EGM96 geoid height above the WGS84 ellipsoid, with bilinear interpolation.
/// Returns none for invalid coordinates. Grid provenance is in `data/README.md`.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "finite grid coordinates are bounded to its dimensions"
)]
pub fn geoid_height(latitude_deg: f64, longitude_deg: f64) -> Option<f64> {
    const GRID: &[u8; 1440 * 721 * 2] = include_bytes!("../../data/egm96-15.bin");
    if !latitude_deg.is_finite() || !longitude_deg.is_finite() || latitude_deg.abs() > 90.0 {
        return None;
    }
    let x = longitude_deg.rem_euclid(360.0) * 4.0;
    let y = (90.0 - latitude_deg) * 4.0;
    let col = (x.floor() as usize).min(1439);
    let row = (y.floor() as usize).min(719);
    let (fx, fy) = (x - col as f64, y - row as f64);
    let at = |r: usize, c: usize| {
        let i = (r * 1440 + c % 1440) * 2;
        f64::from(u16::from_be_bytes([GRID[i], GRID[i + 1]])) * 0.003 - 108.0
    };
    Some(
        (1.0 - fy) * ((1.0 - fx) * at(row, col) + fx * at(row, col + 1))
            + fy * ((1.0 - fx) * at(row + 1, col) + fx * at(row + 1, col + 1)),
    )
}

/// A comparable sea-level height. Pressure or unspecified references stay unknown.
#[must_use]
pub fn sea_level_height(report: &TrafficReport) -> Option<f64> {
    let (lat, lon) = report.position?;
    match report.altitude_frame? {
        AltitudeFrame::Geometric => report.altitude_m,
        AltitudeFrame::Ellipsoid => Some(report.altitude_m? - geoid_height(lat, lon)?),
        AltitudeFrame::Pressure => None,
    }
}

/// WGS84 height for a 3D globe. Never use pressure height as a geometric height.
#[must_use]
pub fn ellipsoid_height(report: &TrafficReport) -> Option<f64> {
    let (lat, lon) = report.position?;
    match report.altitude_frame? {
        AltitudeFrame::Geometric => Some(report.altitude_m? + geoid_height(lat, lon)?),
        AltitudeFrame::Ellipsoid => report.altitude_m,
        AltitudeFrame::Pressure => None,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "test fixtures")]
mod tests {
    use super::geoid_height;
    #[test]
    fn geoid_wraps_longitude_and_handles_poles_and_bad_coordinates() {
        let london = geoid_height(51.5, -0.1).expect("valid coordinates");
        assert!((40.0..50.0).contains(&london));
        let san_francisco = geoid_height(37.78, -122.49).expect("valid coordinates");
        assert!((-40.0..-25.0).contains(&san_francisco));
        assert_eq!(geoid_height(51.5, -0.1), geoid_height(51.5, 359.9));
        for pole in [-90.0, 90.0] {
            assert_eq!(geoid_height(pole, 0.0), geoid_height(pole, 180.0));
        }
        assert_eq!(geoid_height(91.0, 0.0), None);
        assert_eq!(geoid_height(f64::NAN, 0.0), None);
        assert_eq!(geoid_height(0.0, f64::INFINITY), None);
    }
}
