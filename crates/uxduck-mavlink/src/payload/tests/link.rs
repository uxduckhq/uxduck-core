//! The payload on the one link to a simulated vehicle, by what S400 found
//! the daemon's side must do ("On the daemon's side").

use mavio::dialects::ardupilotmega::enums::GimbalDeviceFlags;
use mavio::dialects::ardupilotmega::messages::{Attitude, GimbalDeviceSetAttitude};

use super::*;
use crate::payload::wire::quaternion;
use crate::payload::{Heard, OnLink};

fn both() -> OnLink {
    OnLink::new(VEHICLE, true, true).expect("a payload")
}

fn senders(frames: &[Vec<u8>]) -> Vec<(u8, u8)> {
    frames.iter().map(|frame| sender(frame)).collect()
}

/// The autopilot's target for its gimbal: an attitude, following the nose.
fn target(angles: [f32; 3]) -> Vec<u8> {
    let message = GimbalDeviceSetAttitude {
        target_system: GIMBAL.0,
        target_component: GIMBAL.1,
        flags: GimbalDeviceFlags::empty(),
        q: quaternion(angles),
        angular_velocity_x: f32::NAN,
        angular_velocity_y: f32::NAN,
        angular_velocity_z: f32::NAN,
    };
    crate::sim::frame(AUTOPILOT.0, AUTOPILOT.1, 0, &message)
}

fn attitude(time_boot_ms: u32) -> Vec<u8> {
    let message = Attitude {
        time_boot_ms,
        ..Attitude::default()
    };
    crate::sim::frame(AUTOPILOT.0, AUTOPILOT.1, 0, &message)
}

// ADR-0063 Y11: a simulation carries a camera, a gimbal, both or neither.
// S400, "To be found": heartbeats as soon as the link is up.
#[test]
fn a_simulation_carries_what_it_was_started_with() {
    assert!(OnLink::new(VEHICLE, false, false).is_none());
    for (camera, gimbal, components) in [
        (true, true, vec![CAMERA, GIMBAL]),
        (true, false, vec![CAMERA]),
        (false, true, vec![GIMBAL]),
    ] {
        let mut link = OnLink::new(VEHICLE, camera, gimbal).expect("a payload");
        assert_eq!(senders(&link.announce()), components);
    }
}

// S400: "Do not send the autopilot any frame addressed to component 154 or
// 100". It answers such a frame itself, with the wrong result.
#[test]
fn what_a_ground_station_addresses_to_the_camera_or_the_gimbal_stays_from_the_autopilot() {
    let mut link = both();
    for (to, message, id) in [(GIMBAL, 283.0, 283), (CAMERA, 259.0, 259)] {
        let heard = link.from_ground(&long(GCS, to, 512, &[message], 0));
        assert!(heard.kept, "{to:?}");
        // The payload answers it.
        assert_eq!(result(&heard.said, to, 512), ACCEPTED);
        assert_eq!(acks(&heard.said)[0].3, GCS);
        assert_eq!(
            crate::framing::header(&heard.said[1]).map(|h| h.message_id),
            Some(id)
        );
    }
    // The autopilot's own, and one for every component, go on to it. The
    // payload hears the second too, as it would on the vehicle.
    let heard = link.from_ground(&long(GCS, AUTOPILOT, 512, &[283.0], 0));
    assert_eq!(heard, Heard::default());
    let heard = link.from_ground(&long(GCS, (VEHICLE, 0), 512, &[283.0], 0));
    assert!(!heard.kept);
    assert_eq!(of!(heard.said, GimbalDeviceInformation).len(), 1);
    // Another vehicle's gimbal is not this one's.
    let heard = link.from_ground(&long(GCS, (2, 154), 512, &[283.0], 0));
    assert_eq!(heard, Heard::default());
    // A ground station's own heartbeat is nothing to the payload.
    let beat = crate::sim::gcs_heartbeat_frame(GCS.0, GCS.1, 0);
    assert_eq!(link.from_ground(&beat), Heard::default());
    // A camera alone has no gimbal to take a frame for, and a gimbal alone
    // no camera: those go on to the autopilot, which says what it says.
    let mut camera = OnLink::new(VEHICLE, true, false).expect("a camera");
    assert!(
        !camera
            .from_ground(&long(GCS, GIMBAL, 512, &[283.0], 0))
            .kept
    );
    assert!(
        camera
            .from_ground(&long(GCS, CAMERA, 512, &[259.0], 0))
            .kept
    );
    let mut gimbal = OnLink::new(VEHICLE, false, true).expect("a gimbal");
    assert!(
        !gimbal
            .from_ground(&long(GCS, CAMERA, 512, &[259.0], 0))
            .kept
    );
    assert!(
        gimbal
            .from_ground(&long(GCS, GIMBAL, 512, &[283.0], 0))
            .kept
    );
}

// S400, "To be found", "To stay healthy" and "To move": the autopilot asks
// the gimbal and the camera who they are, sends the gimbal its target, and
// its ATTITUDE is the clock the gimbal's status keeps.
#[test]
fn the_autopilot_finds_its_payload_aims_the_gimbal_and_gives_it_the_time() {
    let mut link = both();
    for (to, message, id) in [(GIMBAL, 283.0, 283), (CAMERA, 259.0, 259)] {
        let said = link.from_autopilot(&long(AUTOPILOT, to, 512, &[message], 0));
        assert_eq!(result(&said, to, 512), ACCEPTED);
        assert_eq!(acks(&said)[0].3, AUTOPILOT);
        assert_eq!(
            crate::framing::header(&said[1]).map(|h| h.message_id),
            Some(id)
        );
    }
    let down_30 = [0.0, -30_f32.to_radians(), 0.0];
    assert!(link.from_autopilot(&target(down_30)).is_empty());
    let mut statuses = Vec::new();
    for tick in 1..=30_u32 {
        let said = link.from_autopilot(&attitude(20_000 + tick * 100));
        statuses.extend(of!(said, GimbalDeviceAttitudeStatus));
    }
    // Ten a second on the autopilot's clock, and it has turned to the target.
    assert_eq!(statuses.len(), 30);
    let pitch = wire::angles(statuses[29].q).expect("a quaternion")[1];
    assert!(near(pitch, down_30[1]), "{pitch}");
}

// A simulated vehicle is started as system "instance + 1", and a person may
// give it another (MAV_SYSID) and start it again: the payload is that
// vehicle's whatever its number.
#[test]
fn the_payload_takes_the_system_id_its_autopilot_says() {
    let mut link = both();
    assert!(
        link.from_autopilot(&crate::sim::heartbeat_frame(VEHICLE, 1, 0))
            .is_empty()
    );
    assert_eq!(senders(&link.announce()), [CAMERA, GIMBAL]);
    // Another component of another system is not its autopilot.
    link.from_autopilot(&crate::sim::heartbeat_frame(9, 190, 0));
    assert_eq!(senders(&link.announce()), [CAMERA, GIMBAL]);
    link.from_autopilot(&crate::sim::heartbeat_frame(7, 1, 0));
    assert_eq!(senders(&link.announce()), [(7, 100), (7, 154)]);
    assert!(
        link.from_ground(&long(GCS, (7, 154), 512, &[283.0], 0))
            .kept
    );
    assert!(!link.from_ground(&long(GCS, GIMBAL, 512, &[283.0], 0)).kept);
}

// S408, ADR-0063 Y7: where a simulation's picture is sent, its camera
// names as its video stream, as RTP over UDP.
#[test]
fn a_camera_whose_picture_is_sent_names_where_as_rtp_over_udp() {
    use mavio::dialects::ardupilotmega::enums::{CameraCapFlags, VideoStreamType};
    const URI: &str = "udp://127.0.0.1:15600";
    let mut link = both().streaming(URI);
    let heard = link.from_ground(&long(GCS, CAMERA, 512, &[269.0, 1.0], 0));
    assert_eq!(result(&heard.said, CAMERA, 512), ACCEPTED);
    let streams = of!(heard.said, VideoStreamInformation);
    assert_eq!(streams.len(), 1);
    assert_eq!(streams[0].type_, VideoStreamType::Rtpudp);
    let uri: Vec<u8> = streams[0]
        .uri
        .iter()
        .copied()
        .take_while(|byte| *byte != 0)
        .collect();
    assert_eq!(String::from_utf8(uri).as_deref(), Ok(URI));
    assert_eq!(
        (streams[0].resolution_h, streams[0].resolution_v),
        (1280, 720)
    );
    let heard = link.from_ground(&long(GCS, CAMERA, 512, &[259.0], 0));
    assert!(
        of!(heard.said, CameraInformation)[0]
            .flags
            .contains(CameraCapFlags::HAS_VIDEO_STREAM)
    );
    // Given another system id, it is the same camera with the same stream.
    link.from_autopilot(&crate::sim::heartbeat_frame(7, 1, 0));
    let heard = link.from_ground(&long(GCS, (7, 100), 512, &[269.0, 1.0], 0));
    let streams = of!(heard.said, VideoStreamInformation);
    assert_eq!(streams[0].type_, VideoStreamType::Rtpudp);
    // With no picture sent it names no stream, as before.
    let mut silent = both();
    let heard = silent.from_ground(&long(GCS, CAMERA, 512, &[269.0, 1.0], 0));
    assert_eq!(result(&heard.said, CAMERA, 512), DENIED);
    // A gimbal alone has no camera to name one.
    let mut gimbal = OnLink::new(VEHICLE, false, true)
        .expect("a gimbal")
        .streaming(URI);
    assert!(
        !gimbal
            .from_ground(&long(GCS, CAMERA, 512, &[269.0, 1.0], 0))
            .kept
    );
}

// S408, ADR-0063 Y7: the picture is drawn from where the camera truly is,
// the way its gimbal truly points, as wide as its zoom truly sees.
#[test]
fn the_camera_tells_what_it_sees_for_whoever_draws_its_picture() {
    use mavio::dialects::ardupilotmega::messages::GlobalPositionInt;
    let mut link = both().streaming("udp://127.0.0.1:15600");
    // Nowhere yet: the autopilot has not said where the vehicle is.
    assert_eq!(link.view(), None);
    let place = over_home();
    let position = GlobalPositionInt {
        lat: place.lat,
        lon: place.lon,
        alt: place.alt_mm,
        relative_alt: place.relative_alt_mm,
        ..GlobalPositionInt::default()
    };
    link.from_autopilot(&crate::sim::frame(AUTOPILOT.0, AUTOPILOT.1, 0, &position));
    link.from_autopilot(&attitude(20_000));
    let view = link.view().expect("a view");
    assert_eq!(view.place, place);
    // The lens at its widest: 13.2 mm of sensor behind 16 mm.
    assert!((view.hfov_deg - 44.83).abs() < 0.05, "{}", view.hfov_deg);
    // Level, looking north along the nose: the lens is the x axis.
    let down = |[w, x, y, z]: [f32; 4]| 2.0 * (x * z - w * y);
    assert!(down(view.attitude).abs() < 0.02);
    // The autopilot points the gimbal straight down, and the view follows
    // as the gimbal turns.
    link.from_autopilot(&target([0.0, -90_f32.to_radians(), 0.0]));
    for tick in 1..=60_u32 {
        link.from_autopilot(&attitude(20_000 + tick * 100));
    }
    let view = link.view().expect("a view");
    assert!(down(view.attitude) > 0.99, "{:?}", view.attitude);
    // Zoomed all the way in: 80 mm.
    let heard = link.from_ground(&long(GCS, CAMERA, 531, &[2.0, 100.0], 0));
    assert_eq!(result(&heard.said, CAMERA, 531), ACCEPTED);
    let view = link.view().expect("a view");
    assert!((view.hfov_deg - 9.43).abs() < 0.05, "{}", view.hfov_deg);
    // A gimbal alone has no camera, and so no view.
    let mut gimbal = OnLink::new(VEHICLE, false, true).expect("a gimbal");
    gimbal.from_autopilot(&crate::sim::frame(AUTOPILOT.0, AUTOPILOT.1, 0, &position));
    assert_eq!(gimbal.view(), None);
}

// S405: a development seam leaves a card with little or no room, to see a
// full card refuse a recording without recording for hours. It holds when
// the payload starts over as another system's.
#[test]
fn a_card_left_full_refuses_a_recording_and_is_still_full_when_the_payload_starts_over() {
    let mut link = both();
    link.leave_room(0.0);
    let heard = link.from_ground(&long(GCS, CAMERA, 2500, &[0.0, 1.0], 0));
    assert!(heard.kept);
    assert_eq!(result(&heard.said, CAMERA, 2500), FAILED);
    link.from_autopilot(&crate::sim::heartbeat_frame(7, 1, 0));
    let heard = link.from_ground(&long(GCS, (7, 100), 2500, &[0.0, 1.0], 0));
    assert_eq!(result(&heard.said, (7, 100), 2500), FAILED);
    // With room it records.
    link.leave_room(100.0);
    let heard = link.from_ground(&long(GCS, (7, 100), 2500, &[0.0, 1.0], 0));
    assert_eq!(result(&heard.said, (7, 100), 2500), ACCEPTED);
}
