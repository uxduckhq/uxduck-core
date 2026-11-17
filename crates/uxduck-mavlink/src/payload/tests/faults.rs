//! Faults switched on in a simulated payload (ADR-0063 Y16): what each
//! puts on the wire, and that it is gone when switched off.

use mavio::dialects::ardupilotmega::enums::GimbalDeviceErrorFlags;

use super::*;
use crate::payload::faults::SLOW_MS;
use crate::payload::{Faults, OnLink, Profile};

/// A camera and a gimbal that manages itself, as on the fake vehicle.
fn both() -> Payload {
    Payload::new(VEHICLE)
        .with_camera(None)
        .with_gimbal(ManagedBy::Itself)
}

fn senders(frames: &[Vec<u8>]) -> Vec<(u8, u8)> {
    frames.iter().map(|frame| sender(frame)).collect()
}

/// How far along an acknowledgement says its command is.
fn progress(frame: &[u8]) -> u8 {
    crate::framing::payload(frame).expect("a payload")[3]
}

fn storage_left(payload: &mut Payload) -> f32 {
    let frames = payload.receive(&long(GCS, CAMERA, 512, &[261.0], 0));
    of!(frames, StorageInformation).remove(0).available_capacity
}

fn failures(payload: &mut Payload, of: (u8, u8)) -> GimbalDeviceErrorFlags {
    let frames = payload.receive(&long(GCS, of, 512, &[285.0], 0));
    of!(frames, GimbalDeviceAttitudeStatus)
        .remove(0)
        .failure_flags
}

fn primary(payload: &mut Payload) -> (u8, u8) {
    let frames = payload.receive(&long(GCS, GIMBAL, 512, &[281.0], 0));
    let status = of!(frames, GimbalManagerStatus).remove(0);
    (status.primary_control_sysid, status.primary_control_compid)
}

#[test]
fn a_camera_that_stops_talking_says_nothing_until_it_is_back() {
    let mut payload = both();
    let silent = Faults {
        camera_silent: true,
        ..Faults::default()
    };
    payload.set_faults(silent);
    assert_eq!(payload.faults(), silent);
    assert_eq!(senders(&payload.announce()), [GIMBAL]);
    assert!(
        payload
            .receive(&long(GCS, CAMERA, 512, &[259.0], 0))
            .is_empty()
    );
    assert!(
        run(&mut payload, 0, 3000)
            .iter()
            .all(|f| sender(f) == GIMBAL)
    );
    // A frame that names it is still not the autopilot's.
    assert!(payload.takes(&long(GCS, CAMERA, 2000, &[], 0)));

    payload.set_faults(Faults::default());
    assert_eq!(payload.faults(), Faults::default());
    assert_eq!(
        payload.receive(&long(GCS, CAMERA, 512, &[259.0], 0)).len(),
        2
    );
    assert!(
        run(&mut payload, 3000, 5000)
            .iter()
            .any(|f| sender(f) == CAMERA)
    );
}

#[test]
fn a_gimbal_that_stops_talking_says_nothing_and_takes_no_target() {
    let mut payload = both();
    payload.receive(&long(GCS, GIMBAL, 1001, &[-2.0, -2.0, -1.0, -1.0], 0));
    payload.set_faults(Faults {
        gimbal_silent: true,
        ..Faults::default()
    });
    assert_eq!(senders(&payload.announce()), [CAMERA]);
    let aim = long(GCS, GIMBAL, 1000, &[-45.0, 0.0, f32::NAN, f32::NAN], 0);
    assert!(payload.receive(&aim).is_empty());
    assert!(payload.takes(&aim), "still not the autopilot's");
    assert!(
        run(&mut payload, 0, 3000)
            .iter()
            .all(|f| sender(f) == CAMERA)
    );

    payload.set_faults(Faults::default());
    assert_eq!(result(&payload.receive(&aim), GIMBAL, 1000), ACCEPTED);
    assert!(
        run(&mut payload, 3000, 5000)
            .iter()
            .any(|f| sender(f) == GIMBAL)
    );
}

// An integrated unit is one component: with its power off its gimbal says
// nothing either, and it has no gimbal to switch off apart from it.
#[test]
fn an_integrated_unit_goes_silent_whole() {
    let mut link = OnLink::new(VEHICLE, true, true)
        .expect("a payload")
        .fitted(Profile::Integrated);
    link.set_faults(Faults {
        gimbal_silent: true,
        ..Faults::default()
    });
    assert_eq!(link.faults(), Faults::default(), "not a fault it can have");
    link.set_faults(Faults {
        camera_silent: true,
        ..Faults::default()
    });
    assert!(link.announce().is_empty());
    let attitude = mavio::dialects::ardupilotmega::messages::Attitude {
        time_boot_ms: 3000,
        ..Default::default()
    };
    let said = link.from_autopilot(&crate::sim::frame(VEHICLE, 1, 0, &attitude));
    assert!(said.is_empty(), "no attitude and no manager status");
    let heard = link.from_ground(&long(GCS, CAMERA, 512, &[280.0], 0));
    assert!(heard.kept && heard.said.is_empty());
}

#[test]
fn a_full_card_fails_captures_until_it_is_formatted_or_the_fault_ends() {
    let mut payload = both();
    payload.step(1000);
    payload.receive(&long(GCS, CAMERA, 2000, &[0.0, 0.0, 1.0], 0));
    let before = storage_left(&mut payload);
    assert!(before > 0.0);
    let full = Faults {
        card_full: true,
        ..Faults::default()
    };
    payload.set_faults(full);
    assert_eq!(storage_left(&mut payload), 0.0);
    let photo = long(GCS, CAMERA, 2000, &[0.0, 0.0, 1.0], 0);
    assert_eq!(result(&payload.receive(&photo), CAMERA, 2000), FAILED);
    let record = long(GCS, CAMERA, 2500, &[0.0], 0);
    assert_eq!(result(&payload.receive(&record), CAMERA, 2500), FAILED);
    // Switched on twice it is still the one fault.
    payload.set_faults(full);
    payload.set_faults(Faults::default());
    assert!(
        near(storage_left(&mut payload), before),
        "what was on it is back"
    );

    // Formatted, the card is empty and the fault is over by itself.
    payload.set_faults(full);
    let format = long(GCS, CAMERA, 526, &[1.0, 1.0, 0.0], 0);
    assert_eq!(acks(&payload.receive(&format))[0].2, ACCEPTED);
    assert_eq!(payload.faults(), Faults::default());
    assert!(storage_left(&mut payload) > before);
    assert_eq!(acks(&payload.receive(&photo))[0].2, ACCEPTED);
}

#[test]
fn a_command_that_is_refused_is_denied_by_the_component_that_has_it() {
    let mut payload = both();
    payload.step(1000);
    payload.set_faults(Faults {
        refused: Some(2000),
        ..Faults::default()
    });
    let photo = long(GCS, CAMERA, 2000, &[0.0, 0.0, 1.0], 0);
    let frames = payload.receive(&photo);
    assert_eq!(frames.len(), 1, "no photo");
    assert_eq!(result(&frames, CAMERA, 2000), DENIED);
    // The gimbal has no such command, and says what it always did.
    let to_gimbal = long(GCS, GIMBAL, 2000, &[0.0, 0.0, 1.0], 0);
    assert_eq!(
        result(&payload.receive(&to_gimbal), GIMBAL, 2000),
        UNSUPPORTED
    );
    // Sent to every component, the one that has it refuses it, once.
    let to_all = long(GCS, (VEHICLE, 0), 2000, &[0.0, 0.0, 1.0], 0);
    assert_eq!(
        acks(&payload.receive(&to_all)),
        [(CAMERA, 2000, DENIED, GCS)]
    );
    // Other commands are carried out.
    let record = long(GCS, CAMERA, 2500, &[0.0], 0);
    assert_eq!(result(&payload.receive(&record), CAMERA, 2500), ACCEPTED);

    // A manager's command is refused by the manager.
    payload.set_faults(Faults {
        refused: Some(1001),
        ..Faults::default()
    });
    let take = long(GCS, GIMBAL, 1001, &[-2.0, -2.0, -1.0, -1.0], 0);
    assert_eq!(result(&payload.receive(&take), GIMBAL, 1001), DENIED);
    assert_eq!(primary(&mut payload), (0, 0), "no one took it");
    assert_eq!(acks(&payload.receive(&photo))[0].2, ACCEPTED);

    payload.set_faults(Faults::default());
    assert_eq!(result(&payload.receive(&take), GIMBAL, 1001), ACCEPTED);
    assert_eq!(primary(&mut payload), GCS);
}

// Guide, command protocol: "Long running commands". IN_PROGRESS with its
// progress while the command runs, then its result. A resend is told the
// progress again, and the same command afresh that it is busy.
#[test]
fn a_slow_command_says_it_is_in_progress_and_then_how_it_ended() {
    let mut payload = both();
    payload.step(1000);
    payload.set_faults(Faults {
        slow: Some(2000),
        ..Faults::default()
    });
    let photo = long(GCS, CAMERA, 2000, &[0.0, 0.0, 1.0], 0);
    let frames = payload.receive(&photo);
    assert_eq!(frames.len(), 1, "no photo yet");
    assert_eq!(result(&frames, CAMERA, 2000), wire::IN_PROGRESS);
    assert_eq!(progress(&frames[0]), 0);
    assert_eq!(acks(&frames)[0].3, GCS, "acknowledged to its sender");

    // How far along it is, twice a second.
    let frames = run(&mut payload, 1000, 1000 + SLOW_MS - 20);
    let told: Vec<u8> = frames
        .iter()
        .filter(|frame| !acks(std::slice::from_ref(frame)).is_empty())
        .map(|frame| {
            assert_eq!(acks(std::slice::from_ref(frame))[0].2, wire::IN_PROGRESS);
            progress(frame)
        })
        .collect();
    assert_eq!(told, [10, 20, 30, 40, 50, 60, 70, 80, 90]);
    assert!(of!(frames, CameraImageCaptured).is_empty());

    // Sent again by the one who waits, it is told again. From another, or
    // afresh, the camera is busy with it.
    let again = long(GCS, CAMERA, 2000, &[0.0, 0.0, 1.0], 1);
    let frames = payload.receive(&again);
    assert_eq!(result(&frames, CAMERA, 2000), wire::IN_PROGRESS);
    assert!(progress(&frames[0]) >= 90);
    let other = long((254, 190), CAMERA, 2000, &[0.0, 0.0, 1.0], 0);
    assert_eq!(
        result(&payload.receive(&other), CAMERA, 2000),
        TEMPORARILY_REJECTED
    );
    assert_eq!(
        result(&payload.receive(&photo), CAMERA, 2000),
        TEMPORARILY_REJECTED
    );

    // At its end it is carried out, and its true result follows.
    let frames = run(&mut payload, 1000 + SLOW_MS - 20, 1000 + SLOW_MS + 40);
    assert_eq!(acks(&frames), [(CAMERA, 2000, ACCEPTED, GCS)]);
    assert_eq!(of!(frames, CameraImageCaptured).len(), 1);

    // Its true result is a failure when that is how it ends.
    payload.set_faults(Faults {
        slow: Some(2000),
        card_full: true,
        ..Faults::default()
    });
    assert_eq!(
        result(&payload.receive(&photo), CAMERA, 2000),
        wire::IN_PROGRESS
    );
    let frames = run(&mut payload, 7000, 7000 + SLOW_MS + 40);
    assert_eq!(acks(&frames).last(), Some(&(CAMERA, 2000, FAILED, GCS)));

    // Switched off, a command is answered at once.
    payload.set_faults(Faults::default());
    assert_eq!(acks(&payload.receive(&photo))[0].2, ACCEPTED);
}

#[test]
fn a_gimbal_whose_motors_fail_says_so_and_stays_where_it_is() {
    let mut payload = both();
    payload.receive(&long(GCS, GIMBAL, 1001, &[-2.0, -2.0, -1.0, -1.0], 0));
    payload.receive(&long(
        GCS,
        GIMBAL,
        1000,
        &[-30.0, 0.0, f32::NAN, f32::NAN],
        0,
    ));
    run(&mut payload, 0, 3000);
    assert_eq!(
        failures(&mut payload, GIMBAL),
        GimbalDeviceErrorFlags::empty()
    );

    payload.set_faults(Faults {
        gimbal_error: true,
        ..Faults::default()
    });
    assert_eq!(
        failures(&mut payload, GIMBAL),
        GimbalDeviceErrorFlags::MOTOR_ERROR
    );
    // Its manager still takes a target, and the device does not get there.
    let aim = long(GCS, GIMBAL, 1000, &[-80.0, 0.0, f32::NAN, f32::NAN], 0);
    assert_eq!(result(&payload.receive(&aim), GIMBAL, 1000), ACCEPTED);
    let frames = run(&mut payload, 3000, 6000);
    let status = of!(frames, GimbalDeviceAttitudeStatus)
        .pop()
        .expect("status");
    assert_eq!(status.failure_flags, GimbalDeviceErrorFlags::MOTOR_ERROR);
    let [_, pitch, _] = wire::angles(status.q).expect("a quaternion");
    assert!(near(pitch, -30f32.to_radians()), "{pitch}");

    // Mended, it goes where it was last told.
    payload.set_faults(Faults::default());
    let frames = run(&mut payload, 6000, 9000);
    let status = of!(frames, GimbalDeviceAttitudeStatus)
        .pop()
        .expect("status");
    assert_eq!(status.failure_flags, GimbalDeviceErrorFlags::empty());
    let [_, pitch, _] = wire::angles(status.q).expect("a quaternion");
    assert!(near(pitch, -80f32.to_radians()), "{pitch}");
}

// Guide, gimbal protocol: a ground station that is not in control "should
// not send" setpoints, and takes control with
// MAV_CMD_DO_GIMBAL_MANAGER_CONFIGURE.
#[test]
fn an_autopilot_that_holds_the_gimbal_has_it_until_another_takes_it() {
    let mut payload = both();
    let holds = Faults {
        autopilot_holds: true,
        ..Faults::default()
    };
    payload.set_faults(holds);
    assert_eq!(primary(&mut payload), AUTOPILOT);
    assert_eq!(payload.faults(), holds);
    // Whoever is not in control is not obeyed.
    let aim = long(GCS, GIMBAL, 1000, &[-45.0, 0.0, f32::NAN, f32::NAN], 0);
    assert_ne!(result(&payload.receive(&aim), GIMBAL, 1000), ACCEPTED);

    // Switched off, the autopilot lets go.
    payload.set_faults(Faults::default());
    assert_eq!(primary(&mut payload), (0, 0));

    // Taken by another, it is theirs and the fault is over by itself. One
    // who then gives it up is not followed by the autopilot again.
    payload.set_faults(holds);
    payload.receive(&long(GCS, GIMBAL, 1001, &[-2.0, -2.0, -1.0, -1.0], 0));
    assert_eq!(primary(&mut payload), GCS);
    assert_eq!(payload.faults(), Faults::default());
    payload.set_faults(Faults::default());
    assert_eq!(primary(&mut payload), GCS, "not the autopilot's to give up");

    // A gimbal the autopilot manages has no manager here to hold it.
    let mut link = OnLink::new(VEHICLE, true, true).expect("a payload");
    link.set_faults(holds);
    assert_eq!(link.faults(), Faults::default());
}

// The payload starts over as another system's when its autopilot says so
// (S402). Its faults are its hardware's, and are still on.
#[test]
fn faults_stay_on_when_the_payload_starts_over() {
    let mut link = OnLink::new(VEHICLE, true, true).expect("a payload");
    let faults = Faults {
        card_full: true,
        gimbal_error: true,
        refused: Some(2500),
        ..Faults::default()
    };
    link.set_faults(faults);
    let heartbeat = mavio::dialects::ardupilotmega::messages::Heartbeat::default();
    link.from_autopilot(&crate::sim::frame(7, 1, 0, &heartbeat));
    assert_eq!(link.faults(), faults);
    let record = long(GCS, (7, 100), 2500, &[0.0], 0);
    assert_eq!(acks(&link.from_ground(&record).said)[0].2, DENIED);
}
