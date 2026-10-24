//! `ADSB_VEHICLE` as a traffic report (S168, ADR-0034): MAVLink's units to
//! SI, and only the fields its flags say hold something.

use uxduck_core::domain::traffic::{
    AltitudeFrame, TrafficCategory, TrafficProtocol, TrafficReport,
};

use crate::decode::Message;

const VALID_COORDS: u16 = 1;
const VALID_ALTITUDE: u16 = 2;
const VALID_HEADING: u16 = 4;
const VALID_VELOCITY: u16 = 8;
const VALID_CALLSIGN: u16 = 16;
const VALID_SQUAWK: u16 = 32;
const SIMULATED: u16 = 64;
const VERTICAL_VELOCITY_VALID: u16 = 128;
const SOURCE_UAT: u16 = 0x8000;

/// The report in an `ADSB_VEHICLE`, heard `tslc` seconds before
/// `received_us`; `None` for any other message.
#[must_use]
pub fn report(message: &Message, received_us: u64) -> Option<TrafficReport> {
    let Message::AdsbVehicle {
        icao_address,
        lat,
        lon,
        altitude_type,
        altitude,
        heading,
        hor_velocity,
        ver_velocity,
        callsign,
        emitter_type,
        tslc,
        flags,
        squawk,
    } = message
    else {
        return None;
    };
    let has = |flag: u16| flags & flag != 0;
    let callsign = callsign.trim();
    Some(TrafficReport {
        address: *icao_address,
        callsign: (has(VALID_CALLSIGN) && !callsign.is_empty()).then(|| callsign.to_owned()),
        squawk: has(VALID_SQUAWK).then_some(*squawk),
        category: category(*emitter_type),
        protocol: if has(SOURCE_UAT) {
            TrafficProtocol::Uat
        } else {
            TrafficProtocol::Adsb
        },
        position: has(VALID_COORDS).then(|| (f64::from(*lat) / 1e7, f64::from(*lon) / 1e7)),
        altitude_m: has(VALID_ALTITUDE).then(|| f64::from(*altitude) / 1000.0),
        altitude_frame: has(VALID_ALTITUDE).then_some(if *altitude_type == 1 {
            AltitudeFrame::Geometric
        } else {
            AltitudeFrame::Pressure
        }),
        track_deg: has(VALID_HEADING).then(|| f64::from(*heading) / 100.0 % 360.0),
        ground_speed_mps: has(VALID_VELOCITY).then(|| f64::from(*hor_velocity) / 100.0),
        climb_mps: has(VERTICAL_VELOCITY_VALID).then(|| f64::from(*ver_velocity) / 100.0),
        simulated: has(SIMULATED),
        heard_us: received_us.saturating_sub(u64::from(*tslc) * 1_000_000),
    })
}

/// `ADSB_EMITTER_TYPE` as a category.
pub(crate) fn category(emitter_type: u8) -> TrafficCategory {
    match emitter_type {
        1 => TrafficCategory::Light,
        2 => TrafficCategory::Small,
        3 | 4 => TrafficCategory::Large,
        5 => TrafficCategory::Heavy,
        6 => TrafficCategory::Fast,
        7 => TrafficCategory::Rotorcraft,
        9 => TrafficCategory::Glider,
        10 => TrafficCategory::Balloon,
        11 => TrafficCategory::Parachute,
        12 => TrafficCategory::Ultralight,
        14 => TrafficCategory::Uav,
        15 => TrafficCategory::Space,
        17 | 18 => TrafficCategory::Surface,
        19 => TrafficCategory::Obstacle,
        _ => TrafficCategory::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adsb(flags: u16) -> Message {
        Message::AdsbVehicle {
            icao_address: 0x004C_A2D1,
            lat: 514_700_000,
            lon: -4_500_000,
            altitude_type: 1,
            altitude: 914_400,
            heading: 26_454,
            hor_velocity: 8_100,
            ver_velocity: -520,
            callsign: "AFR58PM ".into(),
            emitter_type: 3,
            tslc: 2,
            flags,
            squawk: 0o2012,
        }
    }

    #[test]
    fn a_full_report_is_in_si_units() {
        let all = VALID_COORDS
            | VALID_ALTITUDE
            | VALID_HEADING
            | VALID_VELOCITY
            | VALID_CALLSIGN
            | VALID_SQUAWK
            | VERTICAL_VELOCITY_VALID;
        let r = report(&adsb(all), 10_000_000).unwrap_or_else(|| panic!("report"));
        assert_eq!(r.address, 0x004C_A2D1);
        assert_eq!(r.callsign.as_deref(), Some("AFR58PM"));
        assert_eq!(r.position, Some((51.47, -0.45)));
        assert_eq!(r.altitude_m, Some(914.4));
        assert_eq!(r.altitude_frame, Some(AltitudeFrame::Geometric));
        assert_eq!(r.track_deg, Some(264.54));
        assert_eq!(r.ground_speed_mps, Some(81.0));
        assert_eq!(r.climb_mps, Some(-5.2));
        assert_eq!(r.category, TrafficCategory::Large);
        assert_eq!(r.protocol, TrafficProtocol::Adsb);
        assert!(!r.simulated);
        assert_eq!(r.heard_us, 8_000_000);
    }

    #[test]
    fn fields_without_their_flag_are_left_out() {
        let r = report(&adsb(VALID_ALTITUDE | SIMULATED | SOURCE_UAT), 1_000_000)
            .unwrap_or_else(|| panic!("report"));
        assert_eq!(r.position, None);
        assert_eq!(r.callsign, None);
        assert_eq!(r.track_deg, None);
        assert_eq!(r.squawk, None);
        assert_eq!(r.altitude_m, Some(914.4));
        assert!(r.simulated);
        assert_eq!(r.protocol, TrafficProtocol::Uat);
        assert_eq!(r.heard_us, 0);
    }

    #[test]
    fn other_messages_are_not_traffic() {
        assert_eq!(report(&Message::Other { message_id: 0 }, 0), None);
    }
}
