//! The gimbal device, by the gimbal protocol's device messages and by what
//! S400 measured ArduPilot sends and needs.

use mavio::dialects::ardupilotmega::enums::{
    GimbalDeviceCapFlags, GimbalDeviceErrorFlags, GimbalDeviceFlags, MavType,
};
use mavio::dialects::ardupilotmega::messages::{
    Attitude, AutopilotStateForGimbalDevice, GimbalDeviceAttitudeStatus, GimbalDeviceSetAttitude,
    GimbalManagerSetPitchyaw,
};

use super::*;
use crate::payload::gimbal::{RETRACT, YAW_IN_EARTH_FRAME, YAW_IN_VEHICLE_FRAME, YAW_LOCK};
use crate::payload::wire::quaternion;

const UNSET: [f32; 3] = [f32::NAN; 3];

/// A gimbal device alone, managed by the autopilot as ArduPilot does.
fn device() -> Payload {
    Payload::new(VEHICLE).with_gimbal(ManagedBy::Autopilot)
}

/// `GIMBAL_DEVICE_SET_ATTITUDE` to the gimbal from anyone.
fn aim(payload: &mut Payload, from: (u8, u8), flags: u16, q: [f32; 4], rates: [f32; 3]) {
    let message = GimbalDeviceSetAttitude {
        target_system: GIMBAL.0,
        target_component: GIMBAL.1,
        flags: GimbalDeviceFlags::from_bits_retain(flags),
        q,
        angular_velocity_x: rates[0],
        angular_velocity_y: rates[1],
        angular_velocity_z: rates[2],
    };
    let frame = crate::sim::frame(from.0, from.1, 0, &message);
    assert!(payload.receive(&frame).is_empty());
}

fn status(payload: &mut Payload) -> GimbalDeviceAttitudeStatus {
    let frames = payload.receive(&long(AUTOPILOT, GIMBAL, 512, &[285.0], 0));
    assert_eq!(result(&frames, GIMBAL, 512), ACCEPTED);
    of!(frames, GimbalDeviceAttitudeStatus).remove(0)
}

/// Roll, pitch and yaw as the gimbal reports them.
fn reported(payload: &mut Payload) -> [f32; 3] {
    wire::angles(status(payload).q).expect("a quaternion")
}

// S400, "To be found": heartbeats from (vehicle system, 154), then
// MAV_CMD_REQUEST_MESSAGE (283) from the autopilot until it is answered.
// The guide's discovery, its device half: COMMAND_ACK, then
// GIMBAL_DEVICE_INFORMATION.
#[test]
fn a_gimbal_device_is_found_the_way_ardupilot_looks_for_it() {
    let mut payload = device();
    let heartbeats = payload.announce();
    assert_eq!(heartbeats.len(), 1);
    assert_eq!(sender(&heartbeats[0]), GIMBAL);
    assert_eq!(of!(heartbeats, Heartbeat)[0].type_, MavType::Gimbal);

    let frames = payload.receive(&long(AUTOPILOT, GIMBAL, 512, &[283.0], 0));
    assert_eq!(frames.len(), 2);
    assert_eq!(result(&frames, GIMBAL, 512), ACCEPTED);
    assert_eq!(acks(&frames)[0].3, AUTOPILOT, "acknowledged to its sender");
    let information = of!(frames, GimbalDeviceInformation).remove(0);
    assert!(information.vendor_name.starts_with(b"UXDuck\0"));
    assert!(information.model_name.starts_with(b"SimGimbal\0"));
    assert_eq!((information.roll_min, information.roll_max), (-0.5, 0.5));
    assert_eq!(
        (information.pitch_min, information.pitch_max),
        (-std::f32::consts::FRAC_PI_2, 0.6)
    );
    assert_eq!((information.yaw_min, information.yaw_max), (-3.1, 3.1));
    // Its manager is another component, which knows it by its own id.
    assert_eq!(information.gimbal_device_id, 0);
    for flag in [
        GimbalDeviceCapFlags::HAS_PITCH_AXIS,
        GimbalDeviceCapFlags::HAS_YAW_FOLLOW,
        GimbalDeviceCapFlags::HAS_YAW_LOCK,
        GimbalDeviceCapFlags::HAS_RETRACT,
    ] {
        assert!(information.cap_flags.contains(flag), "{flag:?}");
    }
    // S400: no CAN_POINT_LOCATION_GLOBAL. That is bit 17, which only
    // `cap_flags2` after `gimbal_device_id` (byte 144) could carry, and
    // the message ends before it.
    let raw = crate::framing::payload(&frames[1]).expect("a payload");
    assert!(raw.len() <= 145, "{}", raw.len());

    // The guide's manager asks every component at once.
    let frames = payload.receive(&long(AUTOPILOT, (VEHICLE, 0), 512, &[283.0], 0));
    assert_eq!(of!(frames, GimbalDeviceInformation).len(), 1);
    // It is no manager and no camera: it leaves their messages to them.
    assert!(
        payload
            .receive(&long(GCS, (VEHICLE, 0), 512, &[280.0], 0))
            .is_empty()
    );
    assert!(
        payload
            .receive(&long(GCS, (VEHICLE, 0), 512, &[259.0], 0))
            .is_empty()
    );
    let frames = payload.receive(&long(GCS, GIMBAL, 512, &[280.0], 0));
    assert_eq!(result(&frames, GIMBAL, 512), DENIED);
    let frames = payload.receive(&long(GCS, GIMBAL, 1000, &[0.0, 0.0], 0));
    assert_eq!(result(&frames, GIMBAL, 1000), UNSUPPORTED);
}

// S400: "Start as soon as the link is up and repeat. In the browser a
// heartbeat written in the first instant was not heard", and at 30 times
// real time a second of the host's is half a minute of the autopilot's.
#[test]
fn heartbeats_start_at_once_and_repeat_on_the_hosts_clock_and_the_simulations() {
    let mut payload = Payload::new(VEHICLE)
        .with_camera(None)
        .with_gimbal(ManagedBy::Autopilot);
    // No clock yet, and the first ones are lost: they keep coming.
    for _ in 0..3 {
        let senders: Vec<(u8, u8)> = payload.announce().iter().map(|f| sender(f)).collect();
        assert_eq!(senders, [CAMERA, GIMBAL]);
    }
    // Thirty seconds of the simulation in no time of the host's. The first
    // of them had its heartbeat from the host a moment ago.
    let frames = run(&mut payload, 0, 30_000);
    let beats = |frames: &[Vec<u8>], from: (u8, u8)| {
        frames
            .iter()
            .filter(|frame| sender(frame) == from)
            .filter(|frame| crate::framing::header(frame).is_some_and(|h| h.message_id == 0))
            .count()
    };
    assert_eq!(beats(&frames, GIMBAL), 29);
    assert_eq!(beats(&frames, CAMERA), 29);
    // The host's own second adds none to those.
    assert!(payload.announce().is_empty());
    // The simulation stands still: the host's go on.
    assert_eq!(payload.announce().len(), 2);
    assert_eq!(payload.announce().len(), 2);
}

// S400, "To stay healthy": GIMBAL_DEVICE_ATTITUDE_STATUS ten times a
// second counted on the autopilot's clock, which is `time_boot_ms` of
// ATTITUDE, with none of the failure flags that make the mount unhealthy.
#[test]
fn the_attitude_status_keeps_the_autopilots_clock_at_any_speed() {
    let mut payload = device();
    let mut statuses = Vec::new();
    let mut boot_ms = 20_000_u32;
    // ArduPilot sends ATTITUDE once its interval has passed, so each is a
    // little late. All 300 arrive here in no time of the host's.
    for tick in 0..300_u32 {
        boot_ms += 100 + tick % 4;
        let attitude = Attitude {
            time_boot_ms: boot_ms,
            yaw: 0.5,
            ..Attitude::default()
        };
        let frames = payload.receive(&crate::sim::frame(VEHICLE, 1, 0, &attitude));
        let now = of!(frames, GimbalDeviceAttitudeStatus);
        assert_eq!(now.len(), 1, "one for each of the autopilot's ticks");
        assert_eq!(now[0].time_boot_ms, boot_ms);
        statuses.extend(now);
    }
    assert_eq!(statuses.len(), 300);
    let unhealthy = GimbalDeviceErrorFlags::ENCODER_ERROR
        | GimbalDeviceErrorFlags::POWER_ERROR
        | GimbalDeviceErrorFlags::MOTOR_ERROR
        | GimbalDeviceErrorFlags::SOFTWARE_ERROR
        | GimbalDeviceErrorFlags::COMMS_ERROR;
    for status in &statuses {
        assert_eq!((status.target_system, status.target_component), (0, 0));
        assert!(!status.failure_flags.intersects(unhealthy));
        assert!(near(status.delta_yaw, 0.5), "the vehicle's yaw");
    }
    // The autopilot restarts and counts from zero: the status goes on.
    let restarted = Attitude {
        time_boot_ms: 500,
        ..Attitude::default()
    };
    let frames = payload.receive(&crate::sim::frame(VEHICLE, 1, 0, &restarted));
    assert_eq!(of!(frames, GimbalDeviceAttitudeStatus)[0].time_boot_ms, 500);
    // Another vehicle's clock, or a ground station's, is not this one's.
    for from in [(2, 1), GCS, (VEHICLE, 190)] {
        let other = crate::sim::frame(from.0, from.1, 0, &restarted);
        assert!(payload.receive(&other).is_empty());
    }
    // Asked for twice a second, that is what it sends.
    let frames = payload.receive(&long(GCS, GIMBAL, 511, &[285.0, 500_000.0], 0));
    assert_eq!(result(&frames, GIMBAL, 511), ACCEPTED);
    let frames = run(&mut payload, 500, 5500);
    assert_eq!(of!(frames, GimbalDeviceAttitudeStatus).len(), 10);
}

// The fake vehicle gives its heading as 0 to 2π, and the inspector showed a
// `delta_yaw` of 6.22. The status keeps the vehicle's yaw within half a turn.
#[test]
fn a_heading_past_half_a_turn_is_the_same_yaw_the_short_way() {
    let mut payload = device();
    payload.vehicle([0.0, 0.0, 6.0], None);
    let frames = run(&mut payload, 0, 200);
    let status = of!(frames, GimbalDeviceAttitudeStatus).remove(0);
    assert!(near(status.delta_yaw, 6.0 - std::f32::consts::TAU));
}

// The guide: GIMBAL_DEVICE_SET_ATTITUDE "is to be sent from the gimbal
// manager to the gimbal device". S400: it arrives from (vehicle system,
// component 1).
#[test]
fn a_target_is_taken_from_its_manager_and_from_no_one_else() {
    let mut payload = device();
    payload.step(1000);
    let down = quaternion([0.0, -0.5, 0.0]);
    for from in [GCS, CAMERA, (2, 1), (VEHICLE, 191)] {
        aim(&mut payload, from, 0, down, UNSET);
    }
    run(&mut payload, 1000, 2000);
    assert!(near(reported(&mut payload)[1], 0.0), "it has not moved");
    aim(&mut payload, AUTOPILOT, 0, down, UNSET);
    run(&mut payload, 2000, 3000);
    assert!(near(reported(&mut payload)[1], -0.5));
    // A target for another gimbal is not this one's.
    let message = GimbalDeviceSetAttitude {
        target_system: VEHICLE,
        target_component: 155,
        q: quaternion([0.0, 0.3, 0.0]),
        ..GimbalDeviceSetAttitude::default()
    };
    payload.receive(&crate::sim::frame(VEHICLE, 1, 0, &message));
    run(&mut payload, 3000, 4000);
    assert!(near(reported(&mut payload)[1], -0.5));
}

#[test]
fn it_turns_no_faster_than_its_top_speed_and_no_further_than_its_stops() {
    let mut payload = device();
    payload.step(1000);
    aim(
        &mut payload,
        AUTOPILOT,
        0,
        quaternion([0.0, -1.5, 0.0]),
        UNSET,
    );
    // Two radians a second: a radian in half a second.
    run(&mut payload, 1000, 1500);
    let half_way = status(&mut payload);
    assert!(near(wire::angles(half_way.q).unwrap()[1], -1.0));
    assert!(near(half_way.angular_velocity_y, -2.0));
    run(&mut payload, 1500, 2000);
    let there = status(&mut payload);
    assert!(near(wire::angles(there.q).unwrap()[1], -1.5));
    assert!(near(there.angular_velocity_y, 0.0));
    assert!(there.failure_flags.is_empty());

    // Past its stops: a yaw of nearly half a turn, and a pitch that keeps
    // being turned down.
    aim(
        &mut payload,
        AUTOPILOT,
        0,
        quaternion([0.0, 0.0, 3.13]),
        UNSET,
    );
    run(&mut payload, 2000, 5000);
    let stopped = status(&mut payload);
    assert!(near(wire::angles(stopped.q).unwrap()[2], 3.1));
    assert_eq!(stopped.failure_flags, GimbalDeviceErrorFlags::AT_YAW_LIMIT);
    aim(
        &mut payload,
        AUTOPILOT,
        0,
        [f32::NAN; 4],
        [f32::NAN, -1.0, f32::NAN],
    );
    run(&mut payload, 5000, 9000);
    let stopped = status(&mut payload);
    let straight_down = -std::f32::consts::FRAC_PI_2;
    assert!(near(wire::angles(stopped.q).unwrap()[1], straight_down));
    assert_eq!(
        stopped.failure_flags,
        GimbalDeviceErrorFlags::AT_PITCH_LIMIT
    );
    // S400: at a stop the mount stays healthy, since these are not errors.
    aim(
        &mut payload,
        AUTOPILOT,
        0,
        [f32::NAN; 4],
        [f32::NAN, 5.0, f32::NAN],
    );
    run(&mut payload, 9000, 10_000);
    assert!(
        near(reported(&mut payload)[1], straight_down + 2.0),
        "no faster than 2 rad/s"
    );
    run(&mut payload, 10_000, 12_000);
    assert!(near(reported(&mut payload)[1], 0.6));

    // Sent straight down, as a survey sends it: it is at its stop and not
    // held back by it, and it keeps the heading it was given.
    let survey = quaternion([0.0, straight_down, 0.7]);
    aim(&mut payload, AUTOPILOT, 0, survey, UNSET);
    run(&mut payload, 12_000, 15_000);
    let down = status(&mut payload);
    let [roll, pitch, yaw] = wire::angles(down.q).unwrap();
    assert!(near(roll, 0.0) && near(pitch, straight_down) && near(yaw, 0.7));
    assert!(down.failure_flags.is_empty());
}

// S400, "To move": "Flags 0 means follow the vehicle's yaw. Flags 28
// (ROLL_LOCK, PITCH_LOCK, YAW_LOCK) means the yaw is in the earth frame."
#[test]
fn its_yaw_follows_the_nose_or_holds_north() {
    let mut payload = device();
    payload.vehicle([0.0; 3], None);
    payload.step(1000);
    aim(
        &mut payload,
        AUTOPILOT,
        0,
        quaternion([0.0, 0.0, 0.5]),
        UNSET,
    );
    run(&mut payload, 1000, 2000);
    // The vehicle turns a radian to the right, and the gimbal with it.
    payload.vehicle([0.0, 0.0, 1.0], None);
    run(&mut payload, 2000, 2100);
    let following = status(&mut payload);
    assert!(near(wire::angles(following.q).unwrap()[2], 0.5));
    assert!(near(following.delta_yaw, 1.0));
    // ArduPilot reads the yaw from the nose and says so in its own copy.
    assert_ne!(following.flags.bits() & YAW_IN_VEHICLE_FRAME, 0);
    assert_eq!(following.flags.bits() & (YAW_LOCK | YAW_IN_EARTH_FRAME), 0);

    // Held half a radian from north, with the nose a radian from it.
    aim(
        &mut payload,
        AUTOPILOT,
        28,
        quaternion([0.0, 0.0, 0.5]),
        UNSET,
    );
    run(&mut payload, 2100, 3100);
    let holding = status(&mut payload);
    assert!(near(wire::angles(holding.q).unwrap()[2], -0.5));
    assert_ne!(holding.flags.bits() & YAW_LOCK, 0);
    // The vehicle turns on, and the gimbal stays on its bearing at once.
    payload.vehicle([0.0, 0.0, 1.4], None);
    run(&mut payload, 3100, 3120);
    assert!(near(reported(&mut payload)[2], -0.9));
    // Told to follow again with no new angle, it stays where it points.
    aim(&mut payload, AUTOPILOT, 0, [f32::NAN; 4], UNSET);
    payload.vehicle([0.0, 0.0, 2.0], None);
    run(&mut payload, 3120, 3200);
    assert!(near(reported(&mut payload)[2], -0.9));

    // A yaw given from north while following is turned to the nose's.
    aim(
        &mut payload,
        AUTOPILOT,
        YAW_IN_EARTH_FRAME,
        quaternion([0.0, 0.0, 2.3]),
        UNSET,
    );
    run(&mut payload, 3200, 4200);
    assert!(near(reported(&mut payload)[2], 0.3));
}

// S400: "Rate commands arrive as angular velocities with a NaN
// quaternion." The guide: an angle with a rate goes there at that rate.
#[test]
fn a_rate_turns_it_and_a_rate_with_an_angle_is_the_speed_to_get_there() {
    let mut payload = device();
    payload.step(1000);
    aim(
        &mut payload,
        AUTOPILOT,
        0,
        [f32::NAN; 4],
        [f32::NAN, -0.2, 0.3],
    );
    run(&mut payload, 1000, 3000);
    let turning = status(&mut payload);
    let [roll, pitch, yaw] = wire::angles(turning.q).unwrap();
    assert!(near(roll, 0.0) && near(pitch, -0.4) && near(yaw, 0.6));
    assert!(near(turning.angular_velocity_y, -0.2));
    assert!(near(turning.angular_velocity_z, 0.3));
    // A zero rate stops it there.
    aim(&mut payload, AUTOPILOT, 0, [f32::NAN; 4], [0.0; 3]);
    run(&mut payload, 3000, 4000);
    assert!(near(reported(&mut payload)[1], -0.4));
    // To -0.8 at 0.2 rad/s: half way after a second, as MAVSDK's gimbal
    // device tester checks.
    aim(
        &mut payload,
        AUTOPILOT,
        0,
        quaternion([0.0, -0.8, 0.6]),
        [f32::NAN, -0.2, f32::NAN],
    );
    run(&mut payload, 4000, 5000);
    assert!(near(reported(&mut payload)[1], -0.6));
    run(&mut payload, 5000, 7000);
    assert!(near(reported(&mut payload)[1], -0.8));
}

// S400: "RETRACT arrives as the RETRACT flag with a NaN quaternion."
#[test]
fn retract_and_neutral_take_it_to_their_places() {
    let mut payload = device();
    payload.step(1000);
    aim(
        &mut payload,
        AUTOPILOT,
        0,
        quaternion([0.0, -1.0, 1.0]),
        UNSET,
    );
    run(&mut payload, 1000, 2000);
    aim(&mut payload, AUTOPILOT, RETRACT, [f32::NAN; 4], UNSET);
    run(&mut payload, 2000, 4000);
    let retracted = status(&mut payload);
    let [roll, pitch, yaw] = wire::angles(retracted.q).unwrap();
    assert!(near(roll, 0.0) && near(pitch, 0.6) && near(yaw, 0.0));
    assert_ne!(retracted.flags.bits() & RETRACT, 0);
    // Neutral: level and straight ahead.
    aim(&mut payload, AUTOPILOT, 2, [f32::NAN; 4], UNSET);
    run(&mut payload, 4000, 5000);
    let neutral = status(&mut payload);
    assert!(
        wire::angles(neutral.q)
            .unwrap()
            .iter()
            .all(|a| near(*a, 0.0))
    );
    assert_eq!(neutral.flags.bits() & (RETRACT | 2), 2);
}

// S400: AUTOPILOT_STATE_FOR_GIMBAL_DEVICE "arrives at 50 Hz, addressed to
// (vehicle system, component 0). It carries the vehicle's attitude".
#[test]
fn the_vehicles_attitude_comes_from_the_autopilots_state_for_the_gimbal() {
    let mut payload = device();
    let state = AutopilotStateForGimbalDevice {
        target_system: VEHICLE,
        target_component: 0,
        q: quaternion([0.1, -0.05, 0.7]),
        ..AutopilotStateForGimbalDevice::default()
    };
    // Not the autopilot's: not the vehicle's attitude.
    payload.receive(&crate::sim::frame(GCS.0, GCS.1, 0, &state));
    payload.step(1000);
    assert!(status(&mut payload).delta_yaw.is_nan());
    payload.receive(&crate::sim::frame(VEHICLE, 1, 0, &state));
    payload.step(1100);
    assert!(near(status(&mut payload).delta_yaw, 0.7));
}

// MAVSDK's gimbal device tester reads the reported yaw from north and
// never looks at the flags. The device can be set to report it so.
#[test]
fn its_status_can_give_the_yaw_from_north() {
    let mut payload = device();
    payload.gimbal.as_mut().unwrap().reports_from_north = true;
    payload.vehicle([0.0, 0.0, 1.0], None);
    payload.step(1000);
    aim(
        &mut payload,
        AUTOPILOT,
        0,
        quaternion([0.0, 0.0, 0.5]),
        UNSET,
    );
    run(&mut payload, 1000, 2000);
    let status = status(&mut payload);
    assert!(near(wire::angles(status.q).unwrap()[2], 1.5));
    assert_ne!(status.flags.bits() & YAW_IN_EARTH_FRAME, 0);
    assert_eq!(status.flags.bits() & YAW_IN_VEHICLE_FRAME, 0);

    // The fake vehicle gives its heading as 0 to 2π. The yaw between the
    // two frames is still reported within half a turn.
    payload.vehicle([0.0, 0.0, 6.0], None);
    run(&mut payload, 2000, 2100);
    let turned = self::status(&mut payload);
    assert!(near(turned.delta_yaw, 6.0 - std::f32::consts::TAU));
}

// S400, "On the daemon's side": "Do not send the autopilot any frame
// addressed to component 154 or 100." The payload says which those are.
#[test]
fn a_frame_that_names_the_camera_or_the_gimbal_is_the_payloads_to_take() {
    let payload = Payload::new(VEHICLE)
        .with_camera(None)
        .with_gimbal(ManagedBy::Autopilot);
    assert!(payload.takes(&long(GCS, CAMERA, 2000, &[0.0, 0.0, 1.0], 0)));
    assert!(payload.takes(&long(GCS, GIMBAL, 512, &[283.0], 0)));
    assert!(payload.takes(&int(GCS, GIMBAL, 1000, 0, [0.0; 4], (0, 0, 0.0))));
    let setpoint = GimbalManagerSetPitchyaw {
        target_system: VEHICLE,
        target_component: 154,
        ..GimbalManagerSetPitchyaw::default()
    };
    assert!(payload.takes(&crate::sim::frame(GCS.0, GCS.1, 0, &setpoint)));
    // The autopilot's own, everyone's, and another vehicle's stay theirs.
    assert!(!payload.takes(&long(GCS, AUTOPILOT, 2000, &[0.0, 0.0, 1.0], 0)));
    assert!(!payload.takes(&long(GCS, (VEHICLE, 0), 512, &[259.0], 0)));
    assert!(!payload.takes(&long(GCS, (2, 100), 512, &[259.0], 0)));
    assert!(!payload.takes(&crate::sim::heartbeat_frame(255, 190, 0)));
    // A payload with no camera leaves component 100 alone.
    assert!(!device().takes(&long(GCS, CAMERA, 2000, &[0.0, 0.0, 1.0], 0)));
}
