//! The kinds of camera and gimbal a simulation can carry (ADR-0063 Y16),
//! each by what the guide says such hardware answers.

use mavio::dialects::ardupilotmega::enums::{CameraCapFlags, CameraMode, MavType};

use super::*;
use crate::payload::{OnLink, Profile};

const STREAM: &str = "udp://127.0.0.1:15600";

fn fitted(profile: Profile) -> OnLink {
    OnLink::new(VEHICLE, true, true)
        .expect("a payload")
        .fitted(profile)
        .streaming(STREAM)
}

/// A command from the ground station to the camera's component, and all
/// it sends back.
fn ask(link: &mut OnLink, command: u16, values: &[f32]) -> Vec<Vec<u8>> {
    link.from_ground(&long(GCS, CAMERA, command, values, 0))
        .said
}

fn told(link: &mut OnLink, command: u16, values: &[f32]) -> u8 {
    let frames = ask(link, command, values);
    assert_eq!(frames.len(), 1, "an acknowledgement and nothing else");
    result(&frames, CAMERA, command)
}

/// The command's result, whatever comes with it.
fn answered(link: &mut OnLink, command: u16, values: &[f32]) -> u8 {
    result(&ask(link, command, values), CAMERA, command)
}

fn flags(link: &mut OnLink) -> CameraCapFlags {
    of!(ask(link, 512, &[259.0]), CameraInformation)
        .remove(0)
        .flags
}

/// The autopilot's clock, which is the payload's.
fn at(link: &mut OnLink, time_boot_ms: u32) -> Vec<Vec<u8>> {
    let message = mavio::dialects::ardupilotmega::messages::Attitude {
        time_boot_ms,
        ..Default::default()
    };
    link.from_autopilot(&crate::sim::frame(AUTOPILOT.0, AUTOPILOT.1, 0, &message))
}

// Guide, camera protocol: "a camera that only supports video streaming"
// sets CAMERA_CAP_FLAGS_HAS_VIDEO_STREAM alone.
#[test]
fn a_camera_that_only_streams_has_its_stream_and_nothing_to_command() {
    let mut link = fitted(Profile::StreamOnly);
    assert_eq!(flags(&mut link), CameraCapFlags::HAS_VIDEO_STREAM);
    let frames = ask(&mut link, 512, &[269.0, 0.0]);
    assert_eq!(result(&frames, CAMERA, 512), ACCEPTED);
    let stream = of!(frames, VideoStreamInformation).remove(0);
    assert!(stream.uri.starts_with(STREAM.as_bytes()));
    for command in [2000, 2500, 530, 531, 532, 526] {
        assert_eq!(told(&mut link, command, &[0.0, 1.0, 1.0]), UNSUPPORTED);
    }
    // The gimbal beside it is the autopilot's device as ever.
    let heartbeats = link.announce();
    assert_eq!(
        heartbeats.iter().map(|f| sender(f)).collect::<Vec<_>>(),
        [CAMERA, GIMBAL]
    );
}

// Guide, camera protocol: a camera without
// CAMERA_CAP_FLAGS_CAN_CAPTURE_IMAGE_IN_VIDEO_MODE or
// CAN_CAPTURE_VIDEO_IN_IMAGE_MODE must be put in the right mode first, and
// one without the zoom and focus flags has neither command.
#[test]
fn a_camera_bound_to_its_mode_takes_photos_in_one_and_records_in_the_other() {
    let mut link = fitted(Profile::PhotoVideo);
    assert_eq!(
        flags(&mut link),
        CameraCapFlags::CAPTURE_VIDEO
            | CameraCapFlags::CAPTURE_IMAGE
            | CameraCapFlags::HAS_MODES
            | CameraCapFlags::HAS_VIDEO_STREAM
    );
    let settings = of!(ask(&mut link, 512, &[260.0]), CameraSettings).remove(0);
    assert_eq!(settings.mode_id, CameraMode::Image);
    assert!(settings.zoom_level.is_nan() && settings.focus_level.is_nan());
    assert_eq!(told(&mut link, 531, &[1.0, 1.0]), UNSUPPORTED);
    assert_eq!(told(&mut link, 532, &[1.0, 1.0]), UNSUPPORTED);

    // In photo mode: a photo, and no recording.
    assert_eq!(told(&mut link, 2500, &[0.0]), FAILED);
    let frames = ask(&mut link, 2000, &[0.0, 0.0, 1.0]);
    assert_eq!(result(&frames, CAMERA, 2000), ACCEPTED);
    assert_eq!(of!(frames, CameraImageCaptured).len(), 1);

    // In video mode: a recording, and no photo.
    assert_eq!(answered(&mut link, 530, &[0.0, 1.0]), ACCEPTED);
    assert_eq!(told(&mut link, 2000, &[0.0, 0.0, 1.0]), FAILED);
    assert_eq!(answered(&mut link, 2500, &[0.0]), ACCEPTED);
    // It cannot leave the mode it is recording in.
    assert_eq!(told(&mut link, 530, &[0.0, 0.0]), TEMPORARILY_REJECTED);
    assert_eq!(answered(&mut link, 2501, &[0.0]), ACCEPTED);
    assert_eq!(answered(&mut link, 530, &[0.0, 0.0]), ACCEPTED);
}

// Guide, gimbal protocol: "Standalone integrated camera/gimbal", one
// component that is camera, gimbal manager and gimbal device, whose
// manager calls its device by a number from 1 to 6.
#[test]
fn an_integrated_unit_is_one_component_that_is_camera_and_gimbal_manager() {
    let mut link = fitted(Profile::Integrated);
    let heartbeats = link.announce();
    assert_eq!(heartbeats.len(), 1, "one component, one heartbeat");
    assert_eq!(sender(&heartbeats[0]), CAMERA);
    assert_eq!(of!(heartbeats, Heartbeat).remove(0).type_, MavType::Camera);

    let information = of!(ask(&mut link, 512, &[259.0]), CameraInformation).remove(0);
    assert_eq!(information.gimbal_device_id, 1);

    // The manager is found where the camera is, and its device has no
    // component to be asked at.
    let frames = ask(&mut link, 512, &[280.0]);
    assert_eq!(result(&frames, CAMERA, 512), ACCEPTED);
    assert_eq!(sender(&frames[1]), CAMERA);
    assert_eq!(
        of!(frames, GimbalManagerInformation)
            .remove(0)
            .gimbal_device_id,
        1
    );
    assert_eq!(told(&mut link, 512, &[283.0]), DENIED);
    let to_device = long(GCS, GIMBAL, 512, &[283.0], 0);
    let heard = link.from_ground(&to_device);
    assert!(!heard.kept && heard.said.is_empty(), "no component 154");

    // The ground station takes control and points it, at the one component.
    assert_eq!(told(&mut link, 1001, &[-2.0, -2.0, -1.0, -1.0]), ACCEPTED);
    assert_eq!(
        told(&mut link, 1000, &[-45.0, 0.0, f32::NAN, f32::NAN]),
        ACCEPTED
    );
    let mut frames = Vec::new();
    for ms in (20..=3000).step_by(20) {
        frames.extend(at(&mut link, ms));
    }
    assert!(frames.iter().all(|frame| sender(frame) == CAMERA));
    let status = of!(frames, GimbalManagerStatus).pop().expect("its status");
    assert_eq!(status.gimbal_device_id, 1);
    assert_eq!(
        (status.primary_control_sysid, status.primary_control_compid),
        GCS
    );
    let attitude = of!(frames, GimbalDeviceAttitudeStatus)
        .pop()
        .expect("its attitude");
    assert_eq!(attitude.gimbal_device_id, 1);
    let [_, pitch, _] = wire::angles(attitude.q).expect("a quaternion");
    assert!(near(pitch, -45f32.to_radians()), "{pitch}");

    // One component counts its frames as one: none is missing between the
    // camera's and the gimbal's.
    let frames = [
        ask(&mut link, 512, &[259.0]),
        ask(&mut link, 512, &[281.0]),
        ask(&mut link, 512, &[260.0]),
    ]
    .concat();
    let numbers: Vec<u8> = frames
        .iter()
        .map(|frame| crate::framing::header(frame).expect("a frame").sequence)
        .collect();
    assert!(
        numbers
            .windows(2)
            .all(|pair| pair[1] == pair[0].wrapping_add(1)),
        "{numbers:?}"
    );

    // A command neither part has is refused once, and one for every
    // component that neither has is left to whoever has it.
    assert_eq!(told(&mut link, 400, &[1.0]), UNSUPPORTED);
    assert!(
        link.from_ground(&long(GCS, (VEHICLE, 0), 400, &[1.0], 0))
            .said
            .is_empty()
    );
}

// The payload starts over as another system's when its autopilot says so
// (S402), and is the same kind of hardware after.
#[test]
fn a_payload_that_starts_over_is_the_same_kind() {
    let mut link = fitted(Profile::Integrated);
    let heartbeat = mavio::dialects::ardupilotmega::messages::Heartbeat::default();
    link.from_autopilot(&crate::sim::frame(7, 1, 0, &heartbeat));
    let heartbeats = link.announce();
    assert_eq!(
        heartbeats.iter().map(|f| sender(f)).collect::<Vec<_>>(),
        [(7, 100)]
    );
    // Without a camera and a gimbal both there is no unit to be.
    let mut alone = OnLink::new(VEHICLE, false, true)
        .expect("a payload")
        .fitted(Profile::Integrated);
    assert_eq!(sender(&alone.announce()[0]), GIMBAL);
}
