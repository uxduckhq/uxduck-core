//! The fake vehicle with the payload on it: who answers what.

use super::*;
use crate::sim::FakeVehicle;

fn fake() -> FakeVehicle {
    let mut fake = FakeVehicle::new(VEHICLE, 0xAB);
    fake.payload = Some(
        Payload::new(VEHICLE)
            .with_camera(Some("udp://127.0.0.1:5600".into()))
            .with_gimbal(ManagedBy::Itself),
    );
    fake
}

/// The components that acknowledged among some frames.
fn acknowledged_by(frames: &[Vec<u8>]) -> Vec<(u8, u8)> {
    acks(frames).iter().map(|ack| ack.0).collect()
}

#[test]
fn the_fake_vehicle_carries_a_camera_and_a_gimbal_that_manages_itself() {
    let mut fake = fake();
    let heartbeats = fake.payload_heartbeats();
    let senders: Vec<(u8, u8)> = heartbeats.iter().map(|frame| sender(frame)).collect();
    assert_eq!(senders, [CAMERA, GIMBAL], "components 100 and 154");

    // The payload moves on the vehicle's clock and knows where it is.
    let _ = fake.telemetry(10.0);
    let frames = fake.payload_step(10.0);
    let attitude = of!(frames, GimbalDeviceAttitudeStatus).remove(0);
    assert_eq!(attitude.time_boot_ms, 10_000);
    assert!(!attitude.delta_yaw.is_nan(), "the vehicle's heading");
    assert_eq!(of!(frames, GimbalManagerStatus).len(), 1);
    assert!(frames.iter().all(|f| [CAMERA, GIMBAL].contains(&sender(f))));
    // Its heartbeats now come with the steps.
    assert!(fake.payload_heartbeats().is_empty());

    // The gimbal is its own manager: found at component 154, id 1.
    let frames = fake.answer(&long(GCS, GIMBAL, 512, &[280.0], 0));
    assert_eq!(acknowledged_by(&frames), [GIMBAL]);
    assert_eq!(of!(frames, GimbalManagerInformation)[0].gimbal_device_id, 1);
    // The camera names its stream (S39) and the gimbal it sits on.
    let frames = fake.answer(&long(GCS, CAMERA, 512, &[269.0, 0.0], 0));
    assert_eq!(acknowledged_by(&frames), [CAMERA]);
    assert!(
        of!(frames, VideoStreamInformation)[0]
            .uri
            .starts_with(b"udp://127.0.0.1:5600\0")
    );
    let frames = fake.answer(&long(GCS, (VEHICLE, 0), 512, &[259.0], 0));
    assert_eq!(acknowledged_by(&frames), [CAMERA]);
    assert_eq!(of!(frames, CameraInformation)[0].gimbal_device_id, 154);
}

// S400: an IMAGE_START_CAPTURE addressed to component 100 was acknowledged
// by the autopilot, which then sent the camera its own. The fake vehicle
// gives what names the payload to the payload alone, so an image is taken
// once however it is asked for.
#[test]
fn an_image_is_taken_once_whoever_it_is_asked_of() {
    let mut fake = fake();
    let _ = fake.telemetry(10.0);
    fake.payload_step(10.0);
    // Of the camera: the camera answers, and the autopilot says nothing.
    let frames = fake.answer(&long(GCS, CAMERA, 2000, &[0.0, 0.0, 1.0, 1.0], 0));
    assert_eq!(acknowledged_by(&frames), [CAMERA]);
    let images = of!(frames, CameraImageCaptured);
    assert_eq!((images.len(), images[0].image_index), (1, 0));
    // Within the circle it flies around home, 30 m up.
    assert!(images[0].lat.abs_diff(-353_632_610) < 10_000);
    assert_eq!(images[0].relative_alt, 30_000);

    // Of the autopilot, as UXDuck asks: the autopilot acknowledges to the
    // ground station and passes it on, as ArduPilot does. The camera's
    // acknowledgement to the autopilot stays inside the vehicle.
    let frames = fake.answer(&long(GCS, AUTOPILOT, 2000, &[0.0, 0.0, 1.0, 2.0], 0));
    assert_eq!(acks(&frames), [(AUTOPILOT, 2000, ACCEPTED, GCS)]);
    let images = of!(frames, CameraImageCaptured);
    assert_eq!((images.len(), images[0].image_index), (1, 1));
    // Of every component: still once.
    let frames = fake.answer(&long(GCS, (VEHICLE, 0), 2000, &[0.0, 0.0, 1.0, 3.0], 0));
    assert_eq!(acknowledged_by(&frames), [AUTOPILOT]);
    assert_eq!(of!(frames, CameraImageCaptured).len(), 1);

    let frames = fake.answer(&long(GCS, CAMERA, 512, &[262.0], 0));
    assert_eq!(of!(frames, CameraCaptureStatus)[0].image_count, 3);
}

// MAVSDK asks every component of a vehicle for GIMBAL_MANAGER_INFORMATION
// and sends one request at a time. Behind an autopilot that said nothing,
// its requests to the payload ran out and it did not always list the gimbal.
#[test]
fn the_autopilot_says_at_once_that_the_payloads_messages_are_not_its_own() {
    let mut fake = fake();
    for message in [259.0, 262.0, 280.0, 283.0] {
        let frames = fake.answer(&long(GCS, AUTOPILOT, 512, &[message], 0));
        assert_eq!(acks(&frames), [(AUTOPILOT, 512, FAILED, GCS)]);
        assert_eq!(frames.len(), 1, "and it sends no such message");
    }
    // A message of its own is answered as it was.
    let frames = fake.answer(&long(GCS, AUTOPILOT, 512, &[148.0], 0));
    assert!(acks(&frames).is_empty());
    assert_eq!(frames.len(), 1);
    // With no payload it is asked for nothing of the kind, and says nothing.
    let mut bare = FakeVehicle::new(VEHICLE, 0xAB);
    assert!(
        bare.answer(&long(GCS, AUTOPILOT, 512, &[280.0], 0))
            .is_empty()
    );
}

// The waypoint-photos plugin's test and the plugins end-to-end test run
// against a fake vehicle with no payload, whose autopilot drives a camera
// of its own.
#[test]
fn a_fake_vehicle_with_no_payload_accepts_camera_commands_as_it_did() {
    let mut fake = FakeVehicle::new(VEHICLE, 0xAB);
    assert!(fake.payload_heartbeats().is_empty());
    assert!(fake.payload_step(10.0).is_empty());
    for command in [2000, 2001, 2500, 2501, 531] {
        let frames = fake.answer(&long(GCS, AUTOPILOT, command, &[0.0, 0.0, 1.0], 0));
        assert_eq!(acks(&frames), [(AUTOPILOT, command, ACCEPTED, GCS)]);
        assert_eq!(frames.len(), 1);
    }
}

// S404: `uxduck dev fake-vehicle --camera-silent-at 5 --camera-back-at 9`.
#[test]
fn the_fake_vehicles_camera_is_silent_between_two_times_and_its_gimbal_is_not() {
    let mut fake = fake();
    fake.camera_silent_s = Some((5.0, 9.0));
    // What the payload sends in a stretch of time, by who sent it.
    let during = |fake: &mut FakeVehicle, from_s: f64, to_s: f64| {
        let mut senders = Vec::new();
        let mut t_s = from_s;
        while t_s < to_s {
            let _ = fake.telemetry(t_s);
            senders.extend(fake.payload_step(t_s).iter().map(|frame| sender(frame)));
            t_s += 0.02;
        }
        senders
    };
    let identify = long(GCS, CAMERA, 512, &[259.0], 0);
    assert!(during(&mut fake, 0.0, 5.0).contains(&CAMERA));
    assert_eq!(acknowledged_by(&fake.answer(&identify)), [CAMERA]);

    let silent = during(&mut fake, 5.0, 9.0);
    assert!(!silent.contains(&CAMERA));
    assert!(silent.contains(&GIMBAL), "the gimbal has its own power");
    assert!(fake.answer(&identify).is_empty());
    // A camera command for the autopilot is not passed on to it either.
    let frames = fake.answer(&long(GCS, AUTOPILOT, 2000, &[0.0, 0.0, 1.0], 0));
    assert!(frames.iter().all(|frame| sender(frame) != CAMERA));

    assert!(during(&mut fake, 9.0, 12.0).contains(&CAMERA));
    assert_eq!(acknowledged_by(&fake.answer(&identify)), [CAMERA]);
}

// S404: `uxduck dev fake-vehicle --autopilot-camera`, what ArduPilot 4.7
// says of a camera it drives itself (`CAM1_TYPE 1`), read from
// `AP_Camera_Backend::send_camera_information` and not run here.
#[test]
fn an_autopilot_with_a_camera_of_its_own_answers_for_it_beside_a_camera_component() {
    let mut fake = fake();
    fake.own_camera = true;
    let frames = fake.answer(&long(GCS, AUTOPILOT, 512, &[259.0], 0));
    assert_eq!(acks(&frames), [(AUTOPILOT, 512, ACCEPTED, GCS)]);
    let said = of!(frames, CameraInformation).remove(0);
    assert_eq!(sender(&frames[1]), AUTOPILOT);
    assert_eq!(
        said.flags,
        mavio::dialects::ardupilotmega::enums::CameraCapFlags::CAPTURE_IMAGE
    );
    assert_eq!((said.lens_id, said.camera_device_id), (0, 0));
    assert!(said.focal_length.is_nan());
    assert!(said.vendor_name.iter().all(|byte| *byte == 0));
    // The camera component still answers for itself.
    let frames = fake.answer(&long(GCS, CAMERA, 512, &[259.0], 0));
    assert_eq!(acknowledged_by(&frames), [CAMERA]);
    // Its other messages are still not the autopilot's to send.
    let frames = fake.answer(&long(GCS, AUTOPILOT, 512, &[262.0], 0));
    assert_eq!(acks(&frames), [(AUTOPILOT, 512, FAILED, GCS)]);
}
