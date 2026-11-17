//! The camera, by the camera protocol's own sequences.

use mavio::dialects::ardupilotmega::enums::{CameraCapFlags, CameraMode, MavType};

use super::*;

fn camera() -> Payload {
    Payload::new(VEHICLE).with_camera(None)
}

/// A command from the ground station to the camera, and all it sends back.
fn ask(payload: &mut Payload, command: u16, values: &[f32]) -> Vec<Vec<u8>> {
    payload.receive(&long(GCS, CAMERA, command, values, 0))
}

/// The command's result when it brings nothing but its acknowledgement.
fn told(payload: &mut Payload, command: u16, values: &[f32]) -> u8 {
    let frames = ask(payload, command, values);
    assert_eq!(frames.len(), 1, "an acknowledgement and nothing else");
    result(&frames, CAMERA, command)
}

fn settings(payload: &mut Payload) -> (CameraMode, f32, f32) {
    let frames = ask(payload, 512, &[260.0]);
    assert_eq!(result(&frames, CAMERA, 512), ACCEPTED);
    let settings = of!(frames, CameraSettings).remove(0);
    (settings.mode_id, settings.zoom_level, settings.focus_level)
}

fn capture_status(
    payload: &mut Payload,
) -> mavio::dialects::ardupilotmega::messages::CameraCaptureStatus {
    let frames = ask(payload, 512, &[262.0]);
    assert_eq!(result(&frames, CAMERA, 512), ACCEPTED);
    of!(frames, CameraCaptureStatus).remove(0)
}

fn storage_used(payload: &mut Payload) -> f32 {
    let frames = ask(payload, 512, &[261.0]);
    assert_eq!(result(&frames, CAMERA, 512), ACCEPTED);
    let storage = of!(frames, StorageInformation).remove(0);
    assert!(near(
        storage.used_capacity + storage.available_capacity,
        storage.total_capacity
    ));
    storage.used_capacity
}

// Guide: camera identification. HEARTBEAT, MAV_CMD_REQUEST_MESSAGE (259),
// COMMAND_ACK, CAMERA_INFORMATION.
#[test]
fn a_camera_is_identified_by_its_heartbeat_and_then_its_information() {
    let mut payload = camera();
    let heartbeats = payload.announce();
    assert_eq!(heartbeats.len(), 1);
    assert_eq!(sender(&heartbeats[0]), CAMERA);
    let heartbeat = of!(heartbeats, Heartbeat).remove(0);
    assert_eq!(heartbeat.type_, MavType::Camera);

    let frames = ask(&mut payload, 512, &[259.0]);
    assert_eq!(frames.len(), 2);
    assert_eq!(result(&frames, CAMERA, 512), ACCEPTED);
    assert_eq!(acks(&frames)[0].3, GCS, "acknowledged to its sender");
    let information = of!(frames, CameraInformation).remove(0);
    assert!(information.vendor_name.starts_with(b"UXDuck\0"));
    assert!(information.model_name.starts_with(b"SimCamera\0"));
    // What S400 found the autopilot needs to pass commands on, and no
    // stream when it has none.
    for flag in [
        CameraCapFlags::CAPTURE_IMAGE,
        CameraCapFlags::CAPTURE_VIDEO,
        CameraCapFlags::HAS_BASIC_ZOOM,
        CameraCapFlags::HAS_BASIC_FOCUS,
        CameraCapFlags::HAS_MODES,
    ] {
        assert!(information.flags.contains(flag), "{flag:?}");
    }
    assert!(!information.flags.contains(CameraCapFlags::HAS_VIDEO_STREAM));
    // Alone it is fixed to the airframe. On a gimbal it names the gimbal.
    assert_eq!(information.gimbal_device_id, 0);
    assert_eq!(information.camera_device_id, 0);
    let mut both = camera().with_gimbal(ManagedBy::Itself);
    let frames = ask(&mut both, 512, &[259.0]);
    assert_eq!(of!(frames, CameraInformation)[0].gimbal_device_id, 154);
}

// Guide: "If CAMERA_INFORMATION is not received after receiving an ACK
// with MAV_RESULT_ACCEPTED, the protocol assumes the message was lost,
// and the cycle of sending MAV_CMD_REQUEST_MESSAGE is repeated."
#[test]
fn information_lost_on_the_way_comes_again_each_time_it_is_asked_for() {
    let mut payload = camera();
    let mut heard = Vec::new();
    for cycle in 0..3 {
        let frames = ask(&mut payload, 512, &[259.0]);
        assert_eq!(result(&frames, CAMERA, 512), ACCEPTED);
        // The first two answers never arrive.
        if cycle == 2 {
            heard = of!(frames, CameraInformation);
        }
    }
    assert_eq!(heard.len(), 1);
    // The acknowledgement lost and the command sent again: the same answer.
    let again = payload.receive(&long(GCS, CAMERA, 512, &[259.0], 1));
    assert_eq!(result(&again, CAMERA, 512), ACCEPTED);
    assert_eq!(of!(again, CameraInformation).len(), 1);
}

// Guide: camera modes. MAV_CMD_REQUEST_MESSAGE (260), COMMAND_ACK,
// CAMERA_SETTINGS. Then MAV_CMD_SET_CAMERA_MODE, COMMAND_ACK, "mode was
// changed".
#[test]
fn settings_come_when_asked_for_and_a_mode_set_is_the_mode_they_show() {
    let mut payload = camera();
    assert_eq!(settings(&mut payload).0, CameraMode::Image);
    let frames = ask(&mut payload, 530, &[0.0, 1.0]);
    assert_eq!(result(&frames, CAMERA, 530), ACCEPTED);
    assert_eq!(of!(frames, CameraSettings)[0].mode_id, CameraMode::Video);
    assert_eq!(settings(&mut payload).0, CameraMode::Video);
    // It has no survey mode, and no mode 9.
    assert_eq!(told(&mut payload, 530, &[0.0, 2.0]), DENIED);
    assert_eq!(told(&mut payload, 530, &[0.0, 9.0]), DENIED);
    assert_eq!(told(&mut payload, 530, &[0.0, f32::NAN]), DENIED);
    assert_eq!(settings(&mut payload).0, CameraMode::Video);
    // Factory settings.
    let frames = ask(&mut payload, 529, &[1.0]);
    assert_eq!(result(&frames, CAMERA, 529), ACCEPTED);
    assert_eq!(settings(&mut payload).0, CameraMode::Image);
}

// Guide: still image capture for missions. MAV_CMD_IMAGE_START_CAPTURE
// (interval, count or for ever), MAV_RESULT_ACCEPTED, CAMERA_IMAGE_CAPTURED
// broadcast for each, then MAV_CMD_IMAGE_STOP_CAPTURE, MAV_RESULT_ACCEPTED.
#[test]
fn images_for_a_mission_come_at_their_interval_until_the_count_or_a_stop() {
    let mut payload = camera();
    payload.step(1000);
    let frames = ask(&mut payload, 2000, &[0.0, 2.0, 3.0]);
    assert_eq!(result(&frames, CAMERA, 2000), ACCEPTED);
    let first = of!(frames, CameraImageCaptured);
    assert_eq!((first.len(), first[0].image_index), (1, 0));
    assert_eq!(capture_status(&mut payload).image_status, 2);
    assert!(near(capture_status(&mut payload).image_interval, 2.0));

    let later = run(&mut payload, 1000, 9000);
    let images = of!(later, CameraImageCaptured);
    let indexes: Vec<i32> = images.iter().map(|image| image.image_index).collect();
    assert_eq!(indexes, [1, 2], "three in all, and then no more");
    for (image, due_ms) in images.iter().zip([3000_u32, 5000]) {
        assert!(image.time_boot_ms.abs_diff(due_ms) <= 20, "{image:?}");
        assert_eq!(image.capture_result, 1);
    }
    let status = capture_status(&mut payload);
    assert_eq!((status.image_status, status.image_count), (0, 3));

    // For ever, until it is stopped.
    assert_eq!(
        result(&ask(&mut payload, 2000, &[0.0, 1.0, 0.0]), CAMERA, 2000),
        ACCEPTED
    );
    let endless = run(&mut payload, 9000, 14_000);
    assert_eq!(of!(endless, CameraImageCaptured).len(), 5);
    assert_eq!(told(&mut payload, 2001, &[]), ACCEPTED);
    assert!(of!(run(&mut payload, 14_000, 20_000), CameraImageCaptured).is_empty());
    assert_eq!(capture_status(&mut payload).image_count, 9);

    // No sequence faster than it can take them, and no sensor 3.
    assert_eq!(told(&mut payload, 2000, &[0.0, 0.1, 5.0]), DENIED);
    assert_eq!(told(&mut payload, 2000, &[0.0, f32::NAN, 0.0]), DENIED);
    assert_eq!(told(&mut payload, 2000, &[3.0, 0.0, 1.0]), DENIED);
}

// Guide: interactive capture. MAV_CMD_REQUEST_MESSAGE (262),
// MAV_RESULT_ACCEPTED, CAMERA_CAPTURE_STATUS until idle, then
// MAV_CMD_IMAGE_START_CAPTURE for one image, MAV_RESULT_ACCEPTED,
// CAMERA_IMAGE_CAPTURED.
#[test]
fn an_image_by_hand_is_taken_once_the_camera_says_it_is_idle() {
    let mut payload = camera();
    payload.vehicle([0.0, 0.0, 1.0], Some(over_home()));
    payload.step(5000);
    let status = capture_status(&mut payload);
    assert_eq!((status.image_status, status.video_status), (0, 0));
    assert_eq!(status.image_count, 0);

    let frames = ask(&mut payload, 2000, &[0.0, 0.0, 1.0, 1.0]);
    assert_eq!(frames.len(), 2);
    assert_eq!(result(&frames, CAMERA, 2000), ACCEPTED);
    let image = of!(frames, CameraImageCaptured).remove(0);
    assert_eq!((image.image_index, image.capture_result), (0, 1));
    assert_eq!(image.time_boot_ms, 5000);
    // Where the vehicle was, and looking where its nose pointed.
    assert_eq!((image.lat, image.lon), (-353_632_610, 1_491_652_300));
    assert_eq!((image.alt, image.relative_alt), (684_000, 100_000));
    assert!(near(wire::angles(image.q).unwrap()[2], 1.0));
    assert!(image.file_url.starts_with(b"IMG_0001.JPG\0"));
    assert_eq!(capture_status(&mut payload).image_count, 1);
    assert!(near(storage_used(&mut payload), 8.0));

    // One by hand in the middle of a sequence leaves the sequence going.
    ask(&mut payload, 2000, &[0.0, 5.0, 0.0]);
    let frames = ask(&mut payload, 2000, &[0.0, 0.0, 1.0, 2.0]);
    assert_eq!(of!(frames, CameraImageCaptured)[0].image_index, 2);
    assert_eq!(capture_status(&mut payload).image_status, 2);
}

// The command protocol: no acknowledgement, so the command is sent again.
// The camera protocol: a sequence number "to prevent double captures when
// a command is re-transmitted".
#[test]
fn an_acknowledgement_lost_on_the_way_does_not_take_the_image_twice() {
    let mut payload = camera();
    // With a sequence number, as MAVSDK and ArduPilot send it.
    let numbered = [0.0, 0.0, 1.0, 7.0];
    let first = payload.receive(&long(GCS, CAMERA, 2000, &numbered, 0));
    assert_eq!(of!(first, CameraImageCaptured).len(), 1);
    for confirmation in [1, 2] {
        let again = payload.receive(&long(GCS, CAMERA, 2000, &numbered, confirmation));
        assert_eq!(again.len(), 1, "acknowledged again, and no second image");
        assert_eq!(result(&again, CAMERA, 2000), ACCEPTED);
    }
    assert_eq!(capture_status(&mut payload).image_count, 1);
    // The next number is the next image.
    let next = payload.receive(&long(GCS, CAMERA, 2000, &[0.0, 0.0, 1.0, 8.0], 0));
    assert_eq!(of!(next, CameraImageCaptured)[0].image_index, 1);

    // With no sequence number, the count of sends tells a repeat from a
    // second press.
    let plain = [0.0, 0.0, 1.0];
    payload.receive(&long(GCS, CAMERA, 2000, &plain, 0));
    payload.receive(&long(GCS, CAMERA, 2000, &plain, 1));
    assert_eq!(capture_status(&mut payload).image_count, 3);
    payload.receive(&long(GCS, CAMERA, 2000, &plain, 0));
    assert_eq!(capture_status(&mut payload).image_count, 4);
    // Another sender's same command is its own.
    payload.receive(&long((254, 190), CAMERA, 2000, &plain, 1));
    assert_eq!(capture_status(&mut payload).image_count, 5);
}

// Guide: request lost CAMERA_IMAGE_CAPTURED messages. The count in
// CAMERA_CAPTURE_STATUS shows what was missed, and
// MAV_CMD_REQUEST_MESSAGE (263, the index) brings one back.
#[test]
fn an_image_message_lost_on_the_way_is_asked_for_by_its_index() {
    let mut payload = camera();
    payload.vehicle([0.0; 3], Some(over_home()));
    payload.step(1000);
    ask(&mut payload, 2000, &[0.0, 1.0, 0.0]);
    // Out of range of the ground station: none of these arrive.
    let lost = of!(run(&mut payload, 1000, 3000), CameraImageCaptured);
    assert_eq!(lost.len(), 2);
    assert_eq!(capture_status(&mut payload).image_count, 3);

    let frames = ask(&mut payload, 512, &[263.0, 1.0]);
    assert_eq!(result(&frames, CAMERA, 512), ACCEPTED);
    let again = of!(frames, CameraImageCaptured).remove(0);
    assert_eq!(again.image_index, 1);
    assert_eq!(again.time_boot_ms, lost[0].time_boot_ms);
    assert_eq!((again.lat, again.lon), (lost[0].lat, lost[0].lon));
    // There is no image 3 yet.
    assert_eq!(told(&mut payload, 512, &[263.0, 3.0]), DENIED);
}

// Guide: video streaming. MAV_CMD_REQUEST_MESSAGE (269, stream 0 for
// all), MAV_RESULT_ACCEPTED, VIDEO_STREAM_INFORMATION for each stream.
#[test]
fn a_camera_with_a_stream_says_so_and_names_it_when_asked() {
    let uri = "rtsp://192.168.144.25:8554/main";
    let mut payload = Payload::new(VEHICLE).with_camera(Some(uri.into()));
    let information = of!(ask(&mut payload, 512, &[259.0]), CameraInformation).remove(0);
    assert!(information.flags.contains(CameraCapFlags::HAS_VIDEO_STREAM));

    let frames = ask(&mut payload, 512, &[269.0, 0.0]);
    assert_eq!(result(&frames, CAMERA, 512), ACCEPTED);
    let streams = of!(frames, VideoStreamInformation);
    assert_eq!(streams.len(), 1);
    assert_eq!((streams[0].stream_id, streams[0].count), (1, 1));
    assert!(streams[0].uri.starts_with(uri.as_bytes()));
    assert_eq!(streams[0].uri[uri.len()], 0);
    let frames = ask(&mut payload, 512, &[270.0, 1.0]);
    assert_eq!(of!(frames, VideoStreamStatus).len(), 1);
    // It has no stream 2.
    assert_eq!(told(&mut payload, 512, &[269.0, 2.0]), DENIED);
    // "You still must ACK the command", though there is nothing to start.
    assert_eq!(told(&mut payload, 2502, &[1.0]), ACCEPTED);
    assert_eq!(told(&mut payload, 2503, &[1.0]), ACCEPTED);

    // A camera with no stream has none to name or to start.
    let mut plain = camera();
    assert_eq!(told(&mut plain, 512, &[269.0, 0.0]), DENIED);
    assert_eq!(told(&mut plain, 2502, &[1.0]), UNSUPPORTED);
}

// Guide: video capture. MAV_CMD_VIDEO_START_CAPTURE, with
// CAMERA_CAPTURE_STATUS "at a set interval" if asked, then
// MAV_CMD_VIDEO_STOP_CAPTURE.
#[test]
fn a_recording_counts_its_time_and_fills_the_card_until_it_is_stopped() {
    let mut payload = camera();
    payload.step(10_000);
    assert_eq!(told(&mut payload, 2500, &[0.0, 1.0]), ACCEPTED);
    let frames = run(&mut payload, 10_000, 14_000);
    let statuses = of!(frames, CameraCaptureStatus);
    assert_eq!(statuses.len(), 4, "once a second, as asked");
    assert!(statuses.iter().all(|status| status.video_status == 1));
    assert!(statuses[3].recording_time_ms.abs_diff(4000) <= 20);
    assert!(near(storage_used(&mut payload), 6.0));
    // Started again while it records, it goes on with the same recording.
    assert_eq!(told(&mut payload, 2500, &[0.0, 1.0]), ACCEPTED);
    assert!(capture_status(&mut payload).recording_time_ms >= 4000);

    assert_eq!(told(&mut payload, 2501, &[]), ACCEPTED);
    let status = capture_status(&mut payload);
    assert_eq!((status.video_status, status.recording_time_ms), (0, 0));
    assert!(of!(run(&mut payload, 14_000, 17_000), CameraCaptureStatus).is_empty());
    assert!(near(storage_used(&mut payload), 6.0));
    // Stopped twice is still stopped.
    assert_eq!(told(&mut payload, 2501, &[]), ACCEPTED);
}

#[test]
fn zoom_and_focus_go_by_step_by_range_and_at_a_rate() {
    let mut payload = camera();
    payload.step(1000);
    assert_eq!(settings(&mut payload), (CameraMode::Image, 0.0, 100.0));
    // "Camera must respond with a CAMERA_SETTINGS message (on success)."
    let frames = ask(&mut payload, 531, &[2.0, 50.0]);
    assert_eq!(result(&frames, CAMERA, 531), ACCEPTED);
    assert!(near(of!(frames, CameraSettings)[0].zoom_level, 50.0));
    // Running towards the long end, a quarter of the range a second.
    ask(&mut payload, 531, &[1.0, 1.0]);
    run(&mut payload, 1000, 2000);
    assert!(near(settings(&mut payload).1, 75.0));
    ask(&mut payload, 531, &[1.0, 0.0]);
    run(&mut payload, 2000, 3000);
    assert!(near(settings(&mut payload).1, 75.0));
    // Two steps back, a focal length, and a field of view.
    ask(&mut payload, 531, &[0.0, -2.0]);
    assert!(near(settings(&mut payload).1, 55.0));
    assert_eq!(told(&mut payload, 531, &[3.0, 200.0]), DENIED);
    ask(&mut payload, 531, &[3.0, 80.0]);
    assert!(near(settings(&mut payload).1, 100.0));
    let narrow = of!(ask(&mut payload, 512, &[271.0]), CameraFovStatus).remove(0);
    ask(&mut payload, 531, &[4.0, 44.8]);
    assert!(settings(&mut payload).1 < 0.5);
    let wide = of!(ask(&mut payload, 512, &[271.0]), CameraFovStatus).remove(0);
    assert!(near(wide.hfov, 44.8) && near(narrow.hfov, 9.43));
    assert!(wide.vfov < wide.hfov);
    // Not past its range, and not a kind of zoom there is not.
    assert_eq!(told(&mut payload, 531, &[2.0, 120.0]), DENIED);
    assert_eq!(told(&mut payload, 531, &[9.0, 1.0]), DENIED);
    assert_eq!(told(&mut payload, 531, &[2.0, f32::NAN]), DENIED);

    let frames = ask(&mut payload, 532, &[2.0, 40.0]);
    assert!(near(of!(frames, CameraSettings)[0].focus_level, 40.0));
    ask(&mut payload, 532, &[0.0, 1.0]);
    assert!(near(settings(&mut payload).2, 50.0));
    ask(&mut payload, 532, &[1.0, -1.0]);
    run(&mut payload, 3000, 4000);
    assert!(near(settings(&mut payload).2, 25.0));
    // A metre away is half its range, and on its own it finds the ground.
    ask(&mut payload, 532, &[3.0, 1.0]);
    assert!(near(settings(&mut payload).2, 50.0));
    assert_eq!(told(&mut payload, 532, &[3.0, 0.1]), DENIED);
    ask(&mut payload, 532, &[4.0]);
    assert!(near(settings(&mut payload).2, 100.0));
    assert_eq!(told(&mut payload, 532, &[7.0, 1.0]), DENIED);
}

// "These values iterate until explicitly cleared using
// MAV_CMD_STORAGE_FORMAT", and "once format is complete, a
// STORAGE_INFORMATION message is sent".
#[test]
fn a_format_empties_the_card_and_starts_the_image_log_again() {
    let mut payload = camera();
    for number in 1..=3_u8 {
        ask(&mut payload, 2000, &[0.0, 0.0, 1.0, f32::from(number)]);
    }
    assert!(near(storage_used(&mut payload), 24.0));
    // The log alone: the count starts again and the card stays as full.
    assert_eq!(told(&mut payload, 526, &[1.0, 0.0, 1.0]), ACCEPTED);
    assert_eq!(capture_status(&mut payload).image_count, 0);
    assert!(near(storage_used(&mut payload), 24.0));
    let frames = ask(&mut payload, 2000, &[0.0, 0.0, 1.0, 4.0]);
    assert_eq!(of!(frames, CameraImageCaptured)[0].image_index, 0);

    // Not while it records.
    payload.step(1000);
    ask(&mut payload, 2500, &[]);
    assert_eq!(told(&mut payload, 526, &[1.0, 1.0]), TEMPORARILY_REJECTED);
    ask(&mut payload, 2501, &[]);
    let frames = ask(&mut payload, 526, &[1.0, 1.0]);
    assert_eq!(result(&frames, CAMERA, 526), ACCEPTED);
    let storage = of!(frames, StorageInformation).remove(0);
    assert_eq!(storage.used_capacity, 0.0);
    assert_eq!(capture_status(&mut payload).image_count, 0);
    // Neither a card 2 nor a flag of 5.
    assert_eq!(told(&mut payload, 526, &[2.0, 1.0]), DENIED);
    assert_eq!(told(&mut payload, 526, &[1.0, 5.0]), DENIED);
}

#[test]
fn a_full_card_fails_a_capture_and_ends_a_sequence() {
    let mut payload = camera();
    payload.step(1000);
    // 32 000 MiB at 8 MiB an image.
    ask(&mut payload, 2000, &[0.0, 0.5, 0.0]);
    let mut now_ms = 1000;
    for _ in 0..4100 {
        now_ms += 500;
        payload.step(now_ms);
    }
    let status = capture_status(&mut payload);
    assert_eq!((status.image_count, status.image_status), (4000, 0));
    assert_eq!(status.available_capacity, 0.0);
    assert_eq!(told(&mut payload, 2000, &[0.0, 0.0, 1.0]), FAILED);
    assert_eq!(told(&mut payload, 2500, &[]), FAILED);
}

// Guide: "Cameras should still respond to the old commands" while ground
// stations move to MAV_CMD_REQUEST_MESSAGE.
#[test]
fn the_request_commands_from_before_request_message_are_still_answered() {
    let mut payload = Payload::new(VEHICLE).with_camera(Some("udp://127.0.0.1:5600".into()));
    let frames = ask(&mut payload, 521, &[1.0]);
    assert_eq!(result(&frames, CAMERA, 521), ACCEPTED);
    assert_eq!(of!(frames, CameraInformation).len(), 1);
    assert_eq!(of!(ask(&mut payload, 522, &[1.0]), CameraSettings).len(), 1);
    assert_eq!(
        of!(ask(&mut payload, 525, &[1.0, 1.0]), StorageInformation).len(),
        1
    );
    assert_eq!(
        of!(ask(&mut payload, 527, &[1.0]), CameraCaptureStatus).len(),
        1
    );
    assert_eq!(
        of!(ask(&mut payload, 2504, &[0.0]), VideoStreamInformation).len(),
        1
    );
    assert_eq!(
        of!(ask(&mut payload, 2505, &[1.0]), VideoStreamStatus).len(),
        1
    );
    // 0 asks for nothing, and 2 is not a value.
    assert_eq!(told(&mut payload, 521, &[0.0]), ACCEPTED);
    assert_eq!(told(&mut payload, 521, &[2.0]), DENIED);
}

// Guide: camera capture status. MAV_CMD_SET_MESSAGE_INTERVAL, COMMAND_ACK,
// then the message "streamed at the specified rate".
#[test]
fn a_message_asked_for_at_an_interval_comes_at_it_on_the_simulations_clock() {
    let mut payload = camera();
    payload.step(1000);
    assert_eq!(told(&mut payload, 511, &[262.0, 500_000.0]), ACCEPTED);
    assert_eq!(told(&mut payload, 511, &[260.0, 1_000_000.0]), ACCEPTED);
    let frames = run(&mut payload, 1000, 4000);
    assert_eq!(of!(frames, CameraCaptureStatus).len(), 6);
    assert_eq!(of!(frames, CameraSettings).len(), 3);
    assert_eq!(told(&mut payload, 511, &[262.0, -1.0]), ACCEPTED);
    let frames = run(&mut payload, 4000, 6000);
    assert!(of!(frames, CameraCaptureStatus).is_empty());
    assert_eq!(of!(frames, CameraSettings).len(), 2);
    // Not a message of the camera's.
    assert_eq!(told(&mut payload, 511, &[30.0, 100_000.0]), DENIED);
    assert_eq!(told(&mut payload, 512, &[30.0]), DENIED);
}

#[test]
fn a_command_it_does_not_have_is_refused_and_one_for_everyone_is_left_alone() {
    let mut payload = camera();
    // MAV_CMD_COMPONENT_ARM_DISARM is not a camera's.
    assert_eq!(told(&mut payload, 400, &[1.0]), UNSUPPORTED);
    // Sent to every component, it is the autopilot's to answer.
    assert!(
        payload
            .receive(&long(GCS, (VEHICLE, 0), 400, &[1.0], 0))
            .is_empty()
    );
    assert!(
        payload
            .receive(&long(GCS, (VEHICLE, 0), 512, &[148.0], 0))
            .is_empty()
    );
    // A camera command sent to every component is the camera's.
    let frames = payload.receive(&long(GCS, (0, 0), 512, &[259.0], 0));
    assert_eq!(of!(frames, CameraInformation).len(), 1);
    // Another vehicle's, and another component's, are not.
    assert!(
        payload
            .receive(&long(GCS, (2, 100), 512, &[259.0], 0))
            .is_empty()
    );
    assert!(
        payload
            .receive(&long(GCS, (VEHICLE, 1), 512, &[259.0], 0))
            .is_empty()
    );
    // A frame with a bad checksum is not a command.
    let mut broken = long(GCS, CAMERA, 512, &[259.0], 0);
    let last = broken.len() - 1;
    broken[last] ^= 0xFF;
    assert!(payload.receive(&broken).is_empty());
}

// CAMERA_FOV_STATUS: where the camera is and where the middle of its image
// falls, which is nowhere when it does not look at the ground.
#[test]
fn the_field_of_view_says_where_the_image_falls_on_the_ground() {
    let mut payload = camera().with_gimbal(ManagedBy::Itself);
    payload.vehicle([0.0; 3], Some(over_home()));
    payload.step(1000);
    let level = of!(ask(&mut payload, 512, &[271.0]), CameraFovStatus).remove(0);
    assert_eq!(level.lat_camera, -353_632_610);
    assert_eq!(level.alt_camera, 684_000);
    assert_eq!(level.lat_image, i32::MAX, "it looks at the horizon");

    // 45° down and to the east, from 100 m: 100 m east, on the ground.
    payload.receive(&long(GCS, GIMBAL, 1001, &[-2.0, -2.0, -1.0, -1.0], 0));
    payload.receive(&long(
        GCS,
        GIMBAL,
        1000,
        &[-45.0, 90.0, f32::NAN, f32::NAN],
        0,
    ));
    run(&mut payload, 1000, 4000);
    let down = of!(ask(&mut payload, 512, &[271.0]), CameraFovStatus).remove(0);
    assert!(down.lat_image.abs_diff(-353_632_610) < 200, "{down:?}");
    let east_m = f64::from(down.lon_image - 1_491_652_300) / 1e7
        * 111_320.0
        * (-35.363_261_f64).to_radians().cos();
    assert!((east_m - 100.0).abs() < 1.0, "{east_m}");
    assert_eq!(down.alt_image, 584_000);
}

// S404: a camera that only sends video, for a ground station to tell from
// one with a shutter.
#[test]
fn a_camera_that_only_sends_video_says_so_and_has_no_shutter_modes_lens_or_card() {
    let mut payload = Payload::new(VEHICLE).with_camera(Some("udp://127.0.0.1:5600".into()));
    payload.camera.as_mut().expect("a camera").video_only = true;
    let frames = ask(&mut payload, 512, &[259.0]);
    assert_eq!(result(&frames, CAMERA, 512), ACCEPTED);
    let information = of!(frames, CameraInformation).remove(0);
    assert_eq!(information.flags, CameraCapFlags::HAS_VIDEO_STREAM);
    assert!(information.focal_length.is_nan() && information.sensor_size_h.is_nan());
    assert_eq!((information.resolution_h, information.resolution_v), (0, 0));
    // Its stream is named as any camera's.
    let frames = ask(&mut payload, 512, &[269.0, 0.0]);
    assert_eq!(result(&frames, CAMERA, 512), ACCEPTED);
    assert_eq!(of!(frames, VideoStreamInformation).len(), 1);
    assert_eq!(told(&mut payload, 2502, &[0.0]), ACCEPTED);
    // Images, video, the card, a reset, modes, zoom and focus are commands
    // it does not have.
    for command in [2000, 2001, 2500, 2501, 526, 529, 530, 531, 532] {
        assert_eq!(
            told(&mut payload, command, &[0.0, 1.0, 1.0]),
            UNSUPPORTED,
            "{command}"
        );
    }
    // And its settings, card, capture status and field of view are messages
    // it does not have.
    for message in [260.0, 261.0, 262.0, 271.0] {
        assert_eq!(told(&mut payload, 512, &[message]), DENIED, "{message}");
        assert_eq!(told(&mut payload, 511, &[message, 1e6]), DENIED);
    }
    assert!(run(&mut payload, 0, 3000).iter().all(|frame| {
        crate::framing::header(frame).is_some_and(|header| header.message_id == 0)
    }));
}

// S404: a camera unplugged and plugged in again, for a ground station to
// mark gone and identify again.
#[test]
fn a_camera_without_power_says_nothing_and_is_there_again_with_it() {
    let mut payload = camera().with_gimbal(ManagedBy::Itself);
    payload.camera_off = true;
    let heartbeats = payload.announce();
    assert_eq!(heartbeats.len(), 1);
    assert_eq!(sender(&heartbeats[0]), GIMBAL);
    assert!(ask(&mut payload, 512, &[259.0]).is_empty());
    assert!(
        run(&mut payload, 0, 3000)
            .iter()
            .all(|frame| sender(frame) == GIMBAL)
    );
    // What names it is still not the autopilot's to answer.
    assert!(payload.takes(&long(GCS, CAMERA, 512, &[259.0], 0)));

    payload.camera_off = false;
    let frames = ask(&mut payload, 512, &[259.0]);
    assert_eq!(result(&frames, CAMERA, 512), ACCEPTED);
    assert_eq!(of!(frames, CameraInformation).len(), 1);
    assert!(
        run(&mut payload, 3000, 5000)
            .iter()
            .any(|frame| sender(frame) == CAMERA)
    );
}
