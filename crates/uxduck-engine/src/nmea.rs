//! NMEA 0183 from a GPS receiver (ADR-0035 S179): positions from `GGA` and
//! `RMC`, the heading from `HDT`, `THS` or `HDG` with its variation, and
//! the course over ground from `RMC` or `VTG` while moving. Any talker
//! (`GP`, `GN`, `II`, `HE`…). A sentence with a checksum that does not
//! match is dropped. No IO here: the daemon feeds it lines.

/// A heading from the receiver is used this long after it was said.
const HEADING_FRESH_MS: u64 = 3_000;
/// Slower than this, a course says nothing about where it faces.
const MOVING_MPS: f64 = 0.5;
const KNOT_MPS: f64 = 0.514_444;

/// What one stream has said so far.
#[derive(Debug, Default)]
pub struct Nmea {
    heading: Option<(f64, u64)>,
    course: Option<f64>,
    satellites: Option<u32>,
    hdop: Option<f64>,
    /// Whether any sentence with a good checksum has come, to tell NMEA
    /// from noise at the wrong speed.
    pub heard: bool,
}

/// A position read from the stream.
#[derive(Clone, Debug, PartialEq)]
pub struct Fix {
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    pub heading_deg: Option<f64>,
    pub satellites: Option<u32>,
    /// Horizontal dilution of precision times a typical 5 m error; a rough
    /// guide, as receivers give no better.
    pub accuracy_m: Option<f64>,
}

impl Nmea {
    /// One line, at `now_ms`: a fix when it carried a good position.
    pub fn line(&mut self, line: &str, now_ms: u64) -> Option<Fix> {
        let body = checked(line.trim())?;
        self.heard = true;
        let fields: Vec<&str> = body.split(',').collect();
        let kind = fields.first()?.get(2..)?;
        match kind {
            "GGA" => {
                // Quality 0 is no fix.
                if number(&fields, 6).is_none_or(|q| q < 1.0) {
                    return None;
                }
                self.satellites = fields.get(7).and_then(|n| n.parse().ok());
                self.hdop = number(&fields, 8);
                let (latitude_deg, longitude_deg) = position(&fields, 2)?;
                Some(self.fix(latitude_deg, longitude_deg, now_ms))
            }
            "RMC" => {
                if fields.get(2) != Some(&"A") {
                    return None;
                }
                self.moving(number(&fields, 7).map(|k| k * KNOT_MPS), number(&fields, 8));
                let (latitude_deg, longitude_deg) = position(&fields, 3)?;
                Some(self.fix(latitude_deg, longitude_deg, now_ms))
            }
            "VTG" => {
                self.moving(number(&fields, 7).map(|k| k / 3.6), number(&fields, 1));
                None
            }
            "HDT" | "THS" => {
                // THS's mode V is not valid.
                if kind == "THS" && fields.get(2) == Some(&"V") {
                    return None;
                }
                self.heading = number(&fields, 1).map(|h| (h.rem_euclid(360.0), now_ms));
                None
            }
            "HDG" => {
                let magnetic = number(&fields, 1)?;
                let deviation = signed(&fields, 2).unwrap_or(0.0);
                // Without the variation it is magnetic, not true: unused.
                let variation = signed(&fields, 4)?;
                self.heading = Some(((magnetic + deviation + variation).rem_euclid(360.0), now_ms));
                None
            }
            _ => None,
        }
    }

    /// Stopped, the last course stands, as the hull still points there.
    fn moving(&mut self, speed_mps: Option<f64>, course_deg: Option<f64>) {
        if let (Some(speed), Some(course)) = (speed_mps, course_deg)
            && speed >= MOVING_MPS
        {
            self.course = Some(course.rem_euclid(360.0));
        }
    }

    fn fix(&self, latitude_deg: f64, longitude_deg: f64, now_ms: u64) -> Fix {
        let heading = self
            .heading
            .filter(|(_, at)| now_ms.saturating_sub(*at) <= HEADING_FRESH_MS)
            .map(|(h, _)| h);
        Fix {
            latitude_deg,
            longitude_deg,
            heading_deg: heading.or(self.course),
            satellites: self.satellites,
            accuracy_m: self.hdop.map(|h| (h * 5.0 * 10.0).round() / 10.0),
        }
    }
}

/// The sentence between `$` (or `!`) and `*`, when its checksum matches or
/// it has none.
fn checked(line: &str) -> Option<&str> {
    let line = line.strip_prefix('$').or_else(|| line.strip_prefix('!'))?;
    let (body, sum) = match line.split_once('*') {
        Some((body, sum)) => (body, Some(sum)),
        None => (line, None),
    };
    if let Some(sum) = sum {
        let want = u8::from_str_radix(sum.get(..2)?, 16).ok()?;
        if body.bytes().fold(0, |a, b| a ^ b) != want {
            return None;
        }
    }
    (body.len() >= 5 && body.is_ascii()).then_some(body)
}

fn number(fields: &[&str], at: usize) -> Option<f64> {
    fields
        .get(at)
        .filter(|f| !f.is_empty())
        .and_then(|f| f.parse::<f64>().ok())
        .filter(|n| n.is_finite())
}

/// A value followed by its E or W, as east positive.
fn signed(fields: &[&str], at: usize) -> Option<f64> {
    let value = number(fields, at)?;
    match fields.get(at + 1).copied() {
        Some("W") => Some(-value),
        _ => Some(value),
    }
}

/// `ddmm.mmmm,N,dddmm.mmmm,E` from `at`.
fn position(fields: &[&str], at: usize) -> Option<(f64, f64)> {
    let degrees = |text: &str, width: usize| -> Option<f64> {
        let whole: f64 = text.get(..width)?.parse().ok()?;
        let minutes: f64 = text.get(width..)?.parse().ok()?;
        (minutes < 60.0).then_some(whole + minutes / 60.0)
    };
    let lat = degrees(fields.get(at)?, 2)?;
    let lon = degrees(fields.get(at + 2)?, 3)?;
    let lat = match *fields.get(at + 1)? {
        "N" => lat,
        "S" => -lat,
        _ => return None,
    };
    let lon = match *fields.get(at + 3)? {
        "E" => lon,
        "W" => -lon,
        _ => return None,
    };
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sentence with its checksum, as a receiver sends it.
    fn sentence(body: &str) -> String {
        format!("${body}*{:02X}", body.bytes().fold(0u8, |a, b| a ^ b))
    }

    #[test]
    fn a_gga_gives_a_position_with_satellites_and_a_rough_accuracy() {
        let mut nmea = Nmea::default();
        let fix = nmea
            .line(
                &sentence("GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,"),
                0,
            )
            .unwrap();
        assert!((fix.latitude_deg - 48.1173).abs() < 1e-4);
        assert!((fix.longitude_deg - 11.516_667).abs() < 1e-4);
        assert_eq!(fix.satellites, Some(8));
        assert_eq!(fix.accuracy_m, Some(4.5));
        assert!(nmea.heard);
    }

    #[test]
    fn south_and_west_are_negative_and_a_bad_checksum_or_no_fix_is_dropped() {
        let mut nmea = Nmea::default();
        let rmc = sentence("GNRMC,081836,A,3751.65,S,14507.36,W,000.0,360.0,130998,011.3,E");
        let fix = nmea.line(&rmc, 0).unwrap();
        assert!(fix.latitude_deg < -37.0 && fix.longitude_deg < -145.0);
        assert_eq!(
            nmea.line(
                "$GNRMC,081836,A,3751.65,S,14507.36,W,000.0,360.0,130998,011.3,E*00",
                0
            ),
            None
        );
        assert_eq!(
            nmea.line(&sentence("GPRMC,081836,V,,,,,,,130998,,"), 0),
            None
        );
        assert_eq!(
            nmea.line(&sentence("GPGGA,123519,,,,,0,00,,,M,,M,,"), 0),
            None
        );
        assert_eq!(nmea.line("hello", 0), None);
    }

    #[test]
    fn a_true_heading_wins_while_fresh_then_the_course_while_moving() {
        let mut nmea = Nmea::default();
        nmea.line(&sentence("HEHDT,271.5,T"), 1_000);
        let rmc = sentence("GPRMC,1,A,5030.000,N,00100.000,W,5.0,90.0,1,,");
        assert_eq!(nmea.line(&rmc, 2_000).unwrap().heading_deg, Some(271.5));
        // The heading went quiet: the course over ground, moving at 5 knots.
        assert_eq!(nmea.line(&rmc, 9_000).unwrap().heading_deg, Some(90.0));
        // Stopped: the last course stands.
        let stopped = sentence("GPRMC,1,A,5030.000,N,00100.000,W,0.1,200.0,1,,");
        assert_eq!(nmea.line(&stopped, 10_000).unwrap().heading_deg, Some(90.0));
    }

    #[test]
    fn a_magnetic_heading_is_used_only_with_its_variation() {
        let mut nmea = Nmea::default();
        nmea.line(&sentence("HCHDG,100.0,,,2.0,W"), 0);
        let gga = sentence("GPGGA,1,5030.000,N,00100.000,W,1,05,1.0,0,M,0,M,,");
        assert_eq!(nmea.line(&gga, 0).unwrap().heading_deg, Some(98.0));
        let mut bare = Nmea::default();
        bare.line(&sentence("HCHDG,100.0,,,,"), 0);
        assert_eq!(bare.line(&gga, 0).unwrap().heading_deg, None);
    }
}
