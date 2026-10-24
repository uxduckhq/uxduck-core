//! Traffic from a receiver on the ground (S172, ADR-0034): the formats a
//! receiver puts on the network, read into the same reports a vehicle's
//! `ADSB_VEHICLE` gives: SBS-1 (`BaseStation`) text, as readsb and dump1090
//! serve on TCP 30003, and GDL90, as Stratux broadcasts on UDP 4000. Both
//! carry decoded positions, so no Mode S decoding is needed here.

use std::collections::HashMap;

use uxduck_core::domain::traffic::{AltitudeFrame, TrafficProtocol, TrafficReport};

const FEET: f64 = 0.3048;
const KNOTS: f64 = 0.514_444;
/// Lines longer than this are not SBS: dropped, so junk cannot grow memory.
const MOST_LINE: usize = 512;
/// The most aircraft a reader keeps: more is refused by the engine anyway.
const MOST_AIRCRAFT: usize = 2_000;

/// What an SBS feed has said of one aircraft, message by message.
#[derive(Clone, Debug, Default)]
struct Partial {
    callsign: Option<String>,
    squawk: Option<u16>,
    position: Option<(f64, f64)>,
    altitude_m: Option<f64>,
    track_deg: Option<f64>,
    ground_speed_mps: Option<f64>,
    climb_mps: Option<f64>,
}

/// Reads SBS-1 lines (`MSG,3,…`) from a byte stream. Each message carries
/// part of an aircraft (callsign, position, velocity, squawk), so the reader
/// keeps what it has heard of each and reports the whole.
#[derive(Debug, Default)]
pub struct SbsReader {
    line: Vec<u8>,
    aircraft: HashMap<u32, Partial>,
}

impl SbsReader {
    /// The aircraft that the complete lines in `bytes` told of, heard now.
    pub fn push(&mut self, bytes: &[u8], now_us: u64) -> Vec<TrafficReport> {
        let mut reports = Vec::new();
        for byte in bytes {
            if *byte == b'\n' {
                let line = std::mem::take(&mut self.line);
                if let Some(report) = self.message(&String::from_utf8_lossy(&line), now_us) {
                    reports.push(report);
                }
            } else if self.line.len() < MOST_LINE {
                self.line.push(*byte);
            }
        }
        reports
    }

    fn message(&mut self, line: &str, now_us: u64) -> Option<TrafficReport> {
        let fields: Vec<&str> = line.trim_end_matches('\r').split(',').collect();
        if fields.first() != Some(&"MSG") || fields.len() < 11 {
            return None;
        }
        let address = u32::from_str_radix(fields.get(4)?.trim(), 16).ok()?;
        if address > 0x00FF_FFFF {
            return None;
        }
        if !self.aircraft.contains_key(&address) && self.aircraft.len() >= MOST_AIRCRAFT {
            return None;
        }
        let field = |i: usize| fields.get(i).map(|f| f.trim()).filter(|f| !f.is_empty());
        let number = |i: usize| field(i).and_then(|f| f.parse::<f64>().ok());
        let partial = self.aircraft.entry(address).or_default();
        if let Some(callsign) = field(10) {
            partial.callsign = Some(callsign.to_owned());
        }
        if let Some(feet) = number(11) {
            partial.altitude_m = Some(feet * FEET);
        }
        if let Some(knots) = number(12) {
            partial.ground_speed_mps = Some(knots * KNOTS);
        }
        if let Some(track) = number(13) {
            partial.track_deg = Some(track.rem_euclid(360.0));
        }
        if let (Some(lat), Some(lon)) = (number(14), number(15)) {
            partial.position = Some((lat, lon));
        }
        if let Some(feet_per_min) = number(16) {
            partial.climb_mps = Some(feet_per_min * FEET / 60.0);
        }
        if let Some(squawk) = field(17).and_then(|s| s.parse().ok()) {
            partial.squawk = Some(squawk);
        }
        (partial.position.is_some() || partial.altitude_m.is_some()).then(|| TrafficReport {
            address,
            callsign: partial.callsign.clone(),
            squawk: partial.squawk,
            category: uxduck_core::domain::traffic::TrafficCategory::Unknown,
            protocol: TrafficProtocol::Adsb,
            position: partial.position,
            altitude_m: partial.altitude_m,
            // SBS gives the barometric altitude.
            altitude_frame: partial.altitude_m.map(|_| AltitudeFrame::Pressure),
            track_deg: partial.track_deg,
            ground_speed_mps: partial.ground_speed_mps,
            climb_mps: partial.climb_mps,
            simulated: false,
            heard_us: now_us,
        })
    }
}

/// Reads GDL90 frames (`0x7E … 0x7E`, byte-stuffed, CRC-checked) and
/// reports their traffic reports (message 20). Ownship and other messages
/// are skipped.
#[derive(Debug, Default)]
pub struct Gdl90Reader {
    frame: Vec<u8>,
    escaped: bool,
}

/// The frame flag and the escape that stuffs a flag or escape within one.
const FLAG: u8 = 0x7E;
const ESCAPE: u8 = 0x7D;
const TRAFFIC_REPORT: u8 = 20;
/// The longest GDL90 message UXDuck reads, unstuffed, with its CRC.
const MOST_FRAME: usize = 1_024;

impl Gdl90Reader {
    /// The traffic in the whole frames among `bytes`, heard now.
    pub fn push(&mut self, bytes: &[u8], now_us: u64) -> Vec<TrafficReport> {
        let mut reports = Vec::new();
        for byte in bytes {
            match *byte {
                FLAG => {
                    let frame = std::mem::take(&mut self.frame);
                    self.escaped = false;
                    if let Some(report) = traffic_report(&frame, now_us) {
                        reports.push(report);
                    }
                }
                ESCAPE => self.escaped = true,
                byte if self.frame.len() < MOST_FRAME => {
                    self.frame
                        .push(if self.escaped { byte ^ 0x20 } else { byte });
                    self.escaped = false;
                }
                _ => {}
            }
        }
        reports
    }
}

/// GDL90's CRC-16 (the ICD's table form of CCITT, initial 0).
#[must_use]
pub fn gdl90_crc(data: &[u8]) -> u16 {
    data.iter().fold(0u16, |crc, byte| {
        let mut table = (crc >> 8) << 8;
        for _ in 0..8 {
            table = if table & 0x8000 == 0 {
                table << 1
            } else {
                (table << 1) ^ 0x1021
            };
        }
        table ^ (crc << 8) ^ u16::from(*byte)
    })
}

/// A traffic report, from one unstuffed frame: message id, 27 bytes, CRC.
fn traffic_report(frame: &[u8], now_us: u64) -> Option<TrafficReport> {
    let (body, crc) = frame.split_at_checked(frame.len().checked_sub(2)?)?;
    if gdl90_crc(body) != u16::from_le_bytes([crc[0], crc[1]]) {
        return None;
    }
    let (&id, m) = body.split_first()?;
    if id != TRAFFIC_REPORT || m.len() < 27 {
        return None;
    }
    let u24 = |i: usize| u32::from(m[i]) << 16 | u32::from(m[i + 1]) << 8 | u32::from(m[i + 2]);
    // Signed 24-bit fractions of a half turn.
    let angle = |i: usize| {
        let raw = u24(i);
        let signed = if raw & 0x0080_0000 == 0 {
            f64::from(raw)
        } else {
            f64::from(raw) - f64::from(0x0100_0000_u32)
        };
        signed * 180.0 / f64::from(0x0080_0000_u32)
    };
    let address = u24(1);
    let (lat, lon) = (angle(4), angle(7));
    let nic = m[12] >> 4;
    let altitude_raw = u16::from(m[10]) << 4 | u16::from(m[11]) >> 4;
    let misc = m[11] & 0x0F;
    let horizontal = u16::from(m[13]) << 4 | u16::from(m[14]) >> 4;
    let vertical = u16::from(m[14] & 0x0F) << 8 | u16::from(m[15]);
    let callsign = String::from_utf8_lossy(&m[18..26]).trim().to_owned();
    Some(TrafficReport {
        address,
        callsign: (!callsign.is_empty()).then_some(callsign),
        squawk: None,
        category: crate::traffic::category(m[17]),
        // GDL90 comes from UAT receivers and dual-band ones alike: said as
        // ADS-B, which it is either way.
        protocol: TrafficProtocol::Adsb,
        // No position when both are zero and its integrity is unknown.
        position: (lat != 0.0 || lon != 0.0 || nic != 0).then_some((lat, lon)),
        altitude_m: (altitude_raw != 0xFFF)
            .then(|| (f64::from(altitude_raw) * 25.0 - 1000.0) * FEET),
        altitude_frame: (altitude_raw != 0xFFF).then_some(AltitudeFrame::Pressure),
        // Its low two bits: 0 is no track or heading.
        track_deg: (misc & 0x03 != 0).then(|| f64::from(m[16]) * 360.0 / 256.0),
        ground_speed_mps: (horizontal != 0xFFF).then(|| f64::from(horizontal) * KNOTS),
        climb_mps: (vertical != 0x800).then(|| {
            // Signed 12 bits, 64 ft a minute each.
            let signed = if vertical & 0x800 == 0 {
                i32::from(vertical)
            } else {
                i32::from(vertical) - 0x1000
            };
            f64::from(signed) * 64.0 * FEET / 60.0
        }),
        simulated: false,
        heard_us: now_us,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sbs_messages_add_up_to_an_aircraft() {
        let mut reader = SbsReader::default();
        let lines = concat!(
            "MSG,1,1,1,4CA2D1,1,2026/10/21,18:00:00.000,2026/10/21,18:00:00.000,EIN154,,,,,,,,,,,0\r\n",
            "MSG,3,1,1,4CA2D1,1,2026/10/21,18:00:00.100,2026/10/21,18:00:00.100,,3000,,,51.47681,-0.52413,,,0,0,0,0\r\n",
            "MSG,4,1,1,4CA2D1,1,2026/10/21,18:00:00.200,2026/10/21,18:00:00.200,,,157.7,264.5,,,-640,,0,0,0,0\r\n",
            "MSG,6,1,1,4CA2D1,1,2026/10/21,18:00:00.300,2026/10/21,18:00:00.300,,,,,,,,2012,0,0,0,0\r\n",
        );
        // Split mid-line, as a stream arrives.
        let (a, b) = lines.as_bytes().split_at(120);
        let mut reports = reader.push(a, 1);
        reports.extend(reader.push(b, 2));
        // The callsign alone is no report; the rest each give the whole.
        assert_eq!(reports.len(), 3);
        let last = reports.last().unwrap();
        assert_eq!(last.address, 0x004C_A2D1);
        assert_eq!(last.callsign.as_deref(), Some("EIN154"));
        assert_eq!(last.position, Some((51.47681, -0.52413)));
        assert!((last.altitude_m.unwrap() - 914.4).abs() < 0.01);
        assert_eq!(last.altitude_frame, Some(AltitudeFrame::Pressure));
        assert!((last.ground_speed_mps.unwrap() - 81.13).abs() < 0.01);
        assert_eq!(last.track_deg, Some(264.5));
        assert!((last.climb_mps.unwrap() + 3.2512).abs() < 0.001);
        assert_eq!(last.squawk, Some(2012));
        assert_eq!(last.heard_us, 2);
    }

    #[test]
    fn junk_is_not_sbs() {
        let mut reader = SbsReader::default();
        let mut junk = vec![b'x'; 10_000];
        junk.extend_from_slice(
            b"\nSTA,,5,179,400AE7,10103,2008/11/28\nMSG,3,,,ZZZZZZ,,,,,,,1,,,1,1\n",
        );
        assert!(reader.push(&junk, 1).is_empty());
    }

    #[test]
    fn the_icd_heartbeat_checks_and_is_not_traffic() {
        // The GDL90 ICD's own example heartbeat.
        assert_eq!(
            gdl90_crc(&[0x00, 0x81, 0x41, 0xDB, 0xD0, 0x08, 0x02]),
            0x8BB3
        );
        let mut reader = Gdl90Reader::default();
        let frame = [
            0x7E, 0x00, 0x81, 0x41, 0xDB, 0xD0, 0x08, 0x02, 0xB3, 0x8B, 0x7E,
        ];
        assert!(reader.push(&frame, 1).is_empty());
    }

    /// A traffic report as Stratux sends one, stuffed and framed.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "packing the ICD's fields"
    )]
    fn traffic_frame() -> Vec<u8> {
        let mut m = vec![TRAFFIC_REPORT, 0x00, 0xAB, 0x45, 0x49];
        // 44.90708°, -122.99488° (the ICD's example).
        let angle = |deg: f64| {
            let raw = (deg * f64::from(0x0080_0000_u32) / 180.0).round() as i32;
            let raw = (raw as u32) & 0x00FF_FFFF;
            [(raw >> 16) as u8, (raw >> 8) as u8, raw as u8]
        };
        m.extend(angle(44.907_08));
        m.extend(angle(-122.994_88));
        // 5,000 ft, true track; NIC 10 NACp 9; 123 kt, 64 ft/min up; 45°.
        let altitude = (5000 + 1000) / 25;
        m.extend([
            (altitude >> 4) as u8,
            ((altitude & 0x0F) << 4 | 0x09) as u8,
            0xA9,
        ]);
        m.extend([0x07, 0xB0, 0x01, 0x20, 0x01]);
        m.extend(b"N825V   ");
        m.push(0x00);
        let crc = gdl90_crc(&m);
        m.extend(crc.to_le_bytes());
        let mut framed = vec![FLAG];
        for byte in m {
            if byte == FLAG || byte == ESCAPE {
                framed.extend([ESCAPE, byte ^ 0x20]);
            } else {
                framed.push(byte);
            }
        }
        framed.push(FLAG);
        framed
    }

    #[test]
    fn a_gdl90_traffic_report_is_read() {
        let mut reader = Gdl90Reader::default();
        let reports = reader.push(&traffic_frame(), 7);
        assert_eq!(reports.len(), 1);
        let r = &reports[0];
        assert_eq!(r.address, 0x00AB_4549);
        assert_eq!(r.callsign.as_deref(), Some("N825V"));
        let (lat, lon) = r.position.unwrap();
        assert!((lat - 44.907_08).abs() < 1e-4 && (lon + 122.994_88).abs() < 1e-4);
        assert!((r.altitude_m.unwrap() - 1524.0).abs() < 0.01);
        assert!((r.ground_speed_mps.unwrap() - 123.0 * KNOTS).abs() < 0.01);
        assert!((r.climb_mps.unwrap() - 64.0 * FEET / 60.0).abs() < 0.001);
        assert!((r.track_deg.unwrap() - 45.0).abs() < 1.5);
        assert_eq!(
            r.category,
            uxduck_core::domain::traffic::TrafficCategory::Light
        );
    }

    #[test]
    fn a_damaged_gdl90_frame_is_dropped() {
        let mut frame = traffic_frame();
        frame[10] ^= 0x01;
        assert!(Gdl90Reader::default().push(&frame, 1).is_empty());
    }
}
