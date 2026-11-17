//! The gimbal that manages itself, by the gimbal protocol's sequences
//! between a ground station, a manager and a device.

use mavio::dialects::ardupilotmega::enums::GimbalManagerFlags;
use mavio::dialects::ardupilotmega::messages::{
    GimbalDeviceAttitudeStatus, GimbalManagerSetAttitude, GimbalManagerSetManualControl,
    GimbalManagerSetPitchyaw, GimbalManagerStatus,
};

use super::*;
use crate::payload::gimbal::{NEUTRAL, YAW_LOCK};
use crate::payload::wire::quaternion;

const OTHER: (u8, u8) = (254, 190);
const NAN: f32 = f32::NAN;

/// A gimbal with a manager of its own, as on the fake vehicle.
fn managed() -> Payload {
    Payload::new(VEHICLE).with_gimbal(ManagedBy::Itself)
}

/// A command to the manager, and its result.
fn tell(payload: &mut Payload, from: (u8, u8), command: u16, values: &[f32]) -> u8 {
    let frames = payload.receive(&long(from, GIMBAL, command, values, 0));
    assert_eq!(frames.len(), 1, "an acknowledgement and nothing else");
    assert_eq!(acks(&frames)[0].3, from, "acknowledged to its sender");
    result(&frames, GIMBAL, command)
}

/// `MAV_CMD_DO_GIMBAL_MANAGER_CONFIGURE` with -2: the sender takes primary
/// control and leaves the secondary as it is.
fn take_control(payload: &mut Payload, who: (u8, u8)) {
    assert_eq!(
        tell(payload, who, 1001, &[-2.0, -2.0, -1.0, -1.0]),
        ACCEPTED
    );
}

fn manager_status(payload: &mut Payload) -> GimbalManagerStatus {
    let frames = payload.receive(&long(GCS, GIMBAL, 512, &[281.0], 0));
    assert_eq!(result(&frames, GIMBAL, 512), ACCEPTED);
    of!(frames, GimbalManagerStatus).remove(0)
}

fn in_control(payload: &mut Payload) -> [(u8, u8); 2] {
    let status = manager_status(payload);
    [
        (status.primary_control_sysid, status.primary_control_compid),
        (
            status.secondary_control_sysid,
            status.secondary_control_compid,
        ),
    ]
}

fn device_status(payload: &mut Payload) -> GimbalDeviceAttitudeStatus {
    let frames = payload.receive(&long(GCS, GIMBAL, 512, &[285.0], 0));
    of!(frames, GimbalDeviceAttitudeStatus).remove(0)
}

/// Roll, pitch and yaw from the nose as the device reports them.
fn reported(payload: &mut Payload) -> [f32; 3] {
    wire::angles(device_status(payload).q).expect("a quaternion")
}

fn pitch_yaw(from: (u8, u8), angles: [f32; 2], rates: [f32; 2]) -> Vec<u8> {
    let message = GimbalManagerSetPitchyaw {
        target_system: GIMBAL.0,
        target_component: GIMBAL.1,
        pitch: angles[0],
        yaw: angles[1],
        pitch_rate: rates[0],
        yaw_rate: rates[1],
        ..GimbalManagerSetPitchyaw::default()
    };
    crate::sim::frame(from.0, from.1, 0, &message)
}

// Guide: discovery. MAV_CMD_REQUEST_MESSAGE (GIMBAL_MANAGER_INFORMATION),
// COMMAND_ACK, the manager's own asking of the device, then
// GIMBAL_MANAGER_INFORMATION to the ground station. Here the manager and
// the device are one component, so nothing passes between them on a link.
#[test]
fn a_ground_station_discovers_the_manager_and_then_its_device() {
    let mut payload = managed();
    let frames = payload.receive(&long(GCS, GIMBAL, 512, &[280.0], 0));
    assert_eq!(frames.len(), 2);
    assert_eq!(result(&frames, GIMBAL, 512), ACCEPTED);
    let manager = of!(frames, GimbalManagerInformation).remove(0);
    // "This field is then set to a number between 1-6" when the manager
    // and the device are the same component.
    assert_eq!(manager.gimbal_device_id, 1);
    assert_eq!(
        (manager.pitch_min, manager.pitch_max),
        (-std::f32::consts::FRAC_PI_2, 0.6)
    );
    assert_eq!((manager.yaw_min, manager.yaw_max), (-3.1, 3.1));
    // The manager can look at a place, and says so. The device never does.
    assert_ne!(manager.cap_flags.bits() & 131_072, 0);

    // MAVSDK then asks that component for the device, since the id is 1.
    let frames = payload.receive(&long(GCS, GIMBAL, 512, &[283.0], 0));
    assert_eq!(result(&frames, GIMBAL, 512), ACCEPTED);
    let device = of!(frames, GimbalDeviceInformation).remove(0);
    assert_eq!(device.gimbal_device_id, 1);
    assert!(u32::from(device.cap_flags.bits()) < 131_072);

    // Asked of every component, as MAVSDK asks each one it hears.
    let frames = payload.receive(&long(GCS, (VEHICLE, 0), 512, &[280.0], 0));
    assert_eq!(of!(frames, GimbalManagerInformation).len(), 1);

    // Both statuses, unasked: the manager's "at a low regular rate (e.g.
    // 5 Hz)" and the device's "at a regular rate, e.g. 10 Hz".
    let second = run(&mut payload, 0, 1000);
    let statuses = of!(second, GimbalManagerStatus);
    assert_eq!(statuses.len(), 5);
    assert!(statuses.iter().all(|status| status.gimbal_device_id == 1));
    let attitudes = of!(second, GimbalDeviceAttitudeStatus);
    assert_eq!(attitudes.len(), 10);
    assert!(attitudes.iter().all(|status| status.gimbal_device_id == 1));
    // The manager's can be asked for at another rate, or stopped.
    assert_eq!(tell(&mut payload, GCS, 511, &[281.0, 500_000.0]), ACCEPTED);
    assert_eq!(
        of!(run(&mut payload, 1000, 3000), GimbalManagerStatus).len(),
        4
    );
    assert_eq!(tell(&mut payload, GCS, 511, &[281.0, -1.0]), ACCEPTED);
    assert!(of!(run(&mut payload, 3000, 5000), GimbalManagerStatus).is_empty());
    // Information comes when asked for, not at an interval.
    assert_eq!(tell(&mut payload, GCS, 511, &[280.0, 500_000.0]), DENIED);
}

// Guide: starting and configuring gimbal control. "The gimbal manager is
// to ignore any gimbal controls which come from MAVLink components that
// are not explicitly set to in control."
#[test]
fn control_is_taken_shared_and_given_back() {
    let mut payload = managed();
    payload.step(1000);
    assert_eq!(in_control(&mut payload), [(0, 0), (0, 0)]);
    // No one is in control yet: the ground station's command is refused.
    assert_eq!(
        tell(&mut payload, GCS, 1000, &[-30.0, 0.0, NAN, NAN]),
        DENIED
    );
    take_control(&mut payload, GCS);
    assert_eq!(in_control(&mut payload), [GCS, (0, 0)]);
    assert_eq!(
        tell(&mut payload, GCS, 1000, &[-30.0, 0.0, NAN, NAN]),
        ACCEPTED
    );
    // Another ground station is refused, and its stream is ignored.
    assert_eq!(
        tell(&mut payload, OTHER, 1000, &[20.0, 0.0, NAN, NAN]),
        DENIED
    );
    assert!(
        payload
            .receive(&pitch_yaw(OTHER, [0.5, 0.0], [NAN, NAN]))
            .is_empty()
    );
    run(&mut payload, 1000, 2000);
    assert!(near(reported(&mut payload)[1], (-30_f32).to_radians()));
    // It cannot give up a seat it does not sit in.
    assert_eq!(
        tell(&mut payload, OTHER, 1001, &[-3.0, -3.0, -1.0, -1.0]),
        ACCEPTED
    );
    assert_eq!(in_control(&mut payload), [GCS, (0, 0)]);

    // The one in control names the other as secondary, who may then steer.
    assert_eq!(
        tell(&mut payload, GCS, 1001, &[-1.0, -1.0, 254.0, 190.0]),
        ACCEPTED
    );
    assert_eq!(in_control(&mut payload), [GCS, OTHER]);
    assert_eq!(
        tell(&mut payload, OTHER, 1000, &[20.0, 0.0, NAN, NAN]),
        ACCEPTED
    );
    run(&mut payload, 2000, 3000);
    assert!(near(reported(&mut payload)[1], 20_f32.to_radians()));

    // Given back: no one is in primary control, and the gimbal stays put.
    assert_eq!(
        tell(&mut payload, GCS, 1001, &[-3.0, -3.0, -1.0, -1.0]),
        ACCEPTED
    );
    assert_eq!(in_control(&mut payload), [(0, 0), OTHER]);
    assert_eq!(tell(&mut payload, GCS, 1000, &[0.0, 0.0, NAN, NAN]), DENIED);
    run(&mut payload, 3000, 4000);
    assert!(near(reported(&mut payload)[1], 20_f32.to_radians()));
    // By their ids, and 0 for no one.
    assert_eq!(
        tell(&mut payload, OTHER, 1001, &[255.0, 190.0, 0.0, 0.0]),
        ACCEPTED
    );
    assert_eq!(in_control(&mut payload), [GCS, (0, 0)]);
    // Not a gimbal 2, which it does not manage, and not an id of 300.
    let second_gimbal = [-2.0, -2.0, -1.0, -1.0, 0.0, 0.0, 2.0];
    assert_eq!(tell(&mut payload, OTHER, 1001, &second_gimbal), DENIED);
    assert_eq!(
        tell(&mut payload, OTHER, 1001, &[300.0, 1.0, -1.0, -1.0]),
        DENIED
    );
    assert_eq!(in_control(&mut payload), [GCS, (0, 0)]);
}

// Guide: normal manual control. GIMBAL_MANAGER_SET_ATTITUDE as a stream
// from the ground station, the target on to the device, and
// GIMBAL_DEVICE_ATTITUDE_STATUS as a stream back to everyone.
#[test]
fn manual_control_streams_to_the_manager_and_the_attitude_streams_back() {
    let mut payload = managed();
    payload.step(1000);
    take_control(&mut payload, GCS);
    let setpoint = GimbalManagerSetAttitude {
        target_system: GIMBAL.0,
        target_component: GIMBAL.1,
        flags: GimbalManagerFlags::from_bits_retain(0),
        gimbal_device_id: 1,
        q: quaternion([0.0, -0.6, 0.4]),
        angular_velocity_x: NAN,
        angular_velocity_y: NAN,
        angular_velocity_z: NAN,
    };
    let mut statuses = Vec::new();
    for tick in 0..20_u64 {
        let frame = crate::sim::frame(GCS.0, GCS.1, 0, &setpoint);
        assert!(payload.receive(&frame).is_empty(), "a stream has no answer");
        let from_ms = 1000 + tick * 100;
        statuses.extend(of!(
            run(&mut payload, from_ms, from_ms + 100),
            GimbalDeviceAttitudeStatus
        ));
    }
    assert_eq!(statuses.len(), 20);
    assert!(
        statuses
            .iter()
            .all(|s| (s.target_system, s.target_component) == (0, 0))
    );
    let pitches: Vec<f32> = statuses
        .iter()
        .map(|status| wire::angles(status.q).unwrap()[1])
        .collect();
    assert!(pitches.windows(2).all(|pair| pair[1] <= pair[0] + 1e-4));
    assert!(near(pitches[0], -0.2) && near(pitches[19], -0.6));
    let [roll, pitch, yaw] = reported(&mut payload);
    assert!(near(roll, 0.0) && near(pitch, -0.6) && near(yaw, 0.4));
    // A setpoint for a gimbal it does not manage changes nothing.
    let elsewhere = GimbalManagerSetAttitude {
        gimbal_device_id: 2,
        q: quaternion([0.0, 0.3, 0.0]),
        ..setpoint
    };
    payload.receive(&crate::sim::frame(GCS.0, GCS.1, 0, &elsewhere));
    run(&mut payload, 3000, 4000);
    assert!(near(reported(&mut payload)[1], -0.6));
}

#[test]
fn pitch_and_yaw_are_set_by_angle_and_by_rate() {
    let mut payload = managed();
    payload.step(1000);
    take_control(&mut payload, GCS);
    // As MAVSDK sends angles: a COMMAND_LONG in degrees.
    let angles = [-45.0, 30.0, NAN, NAN, 0.0, 0.0, 1.0];
    assert_eq!(tell(&mut payload, GCS, 1000, &angles), ACCEPTED);
    run(&mut payload, 1000, 3000);
    let [_, pitch, yaw] = reported(&mut payload);
    assert!(near(pitch, (-45_f32).to_radians()) && near(yaw, 30_f32.to_radians()));
    // As MAVSDK sends rates: a COMMAND_INT in degrees a second, with the
    // flags in x and the gimbal's id in z.
    let rates = int(GCS, GIMBAL, 1000, 0, [NAN, NAN, 10.0, -20.0], (0, 0, 1.0));
    assert_eq!(result(&payload.receive(&rates), GIMBAL, 1000), ACCEPTED);
    run(&mut payload, 3000, 4000);
    let [_, pitch, yaw] = reported(&mut payload);
    assert!(near(pitch, (-35_f32).to_radians()) && near(yaw, 10_f32.to_radians()));

    // GIMBAL_MANAGER_SET_PITCHYAW in radians. A NaN leaves an axis alone.
    payload.receive(&pitch_yaw(GCS, [NAN, -1.0], [NAN, NAN]));
    run(&mut payload, 4000, 6000);
    let [_, pitch, yaw] = reported(&mut payload);
    assert!(near(pitch, (-35_f32).to_radians()) && near(yaw, -1.0));
    payload.receive(&pitch_yaw(GCS, [NAN, NAN], [0.25, NAN]));
    run(&mut payload, 6000, 7000);
    assert!(near(
        reported(&mut payload)[1],
        (-35_f32).to_radians() + 0.25
    ));

    // Sticks from -1 to 1: half way down, and hard right to the stop.
    let sticks = |pitch, yaw, pitch_rate, yaw_rate| {
        let message = GimbalManagerSetManualControl {
            target_system: GIMBAL.0,
            target_component: GIMBAL.1,
            pitch,
            yaw,
            pitch_rate,
            yaw_rate,
            ..GimbalManagerSetManualControl::default()
        };
        crate::sim::frame(GCS.0, GCS.1, 0, &message)
    };
    payload.receive(&sticks(-0.5, 1.0, NAN, NAN));
    run(&mut payload, 7000, 10_000);
    let [_, pitch, yaw] = reported(&mut payload);
    let half_way_down = -std::f32::consts::FRAC_PI_4;
    assert!(near(pitch, half_way_down) && near(yaw, 3.1));
    // And as rates: half of its top speed up and to the left.
    payload.receive(&sticks(NAN, NAN, 0.5, -0.5));
    run(&mut payload, 10_000, 11_000);
    let [_, pitch, yaw] = reported(&mut payload);
    assert!(near(pitch, half_way_down + 1.0) && near(yaw, 2.1));
}

// Guide: ROI initiated from the ground station.
// MAV_CMD_DO_SET_ROI_LOCATION, COMMAND_ACK, the device's target and its
// status as streams, then MAV_CMD_DO_SET_ROI_NONE, COMMAND_ACK.
#[test]
fn a_place_to_look_at_is_kept_in_view_until_it_is_let_go() {
    // 100 m north of home, on the ground there.
    let place = (-353_623_627, 1_491_652_300);
    let roi = |frame: u8, height: f32| {
        int(
            GCS,
            GIMBAL,
            195,
            frame,
            [0.0; 4],
            (place.0, place.1, height),
        )
    };
    let mut payload = managed();
    take_control(&mut payload, GCS);
    // It cannot point at a place before it knows its own.
    payload.step(1000);
    assert_eq!(
        result(&payload.receive(&roi(0, 584.0)), GIMBAL, 195),
        TEMPORARILY_REJECTED
    );
    // 100 m up over home, with its nose half a radian east of north.
    payload.vehicle([0.0, 0.0, 0.5], Some(over_home()));
    payload.step(1020);
    let frames = payload.receive(&roi(0, 584.0));
    assert_eq!(frames.len(), 1);
    assert_eq!(result(&frames, GIMBAL, 195), ACCEPTED);
    let statuses = of!(run(&mut payload, 1020, 3020), GimbalDeviceAttitudeStatus);
    assert_eq!(statuses.len(), 20, "the stream goes on");
    // 45° down and due north, which is half a radian left of the nose.
    let looking = device_status(&mut payload);
    let [_, pitch, yaw] = wire::angles(looking.q).unwrap();
    assert!(near(pitch, (-45_f32).to_radians()) && near(yaw, -0.5));
    assert_ne!(looking.flags.bits() & YAW_LOCK, 0);
    assert_ne!(manager_status(&mut payload).flags.bits() & YAW_LOCK, 0);

    // The vehicle flies over the place: straight down.
    let over_it = Place {
        lat: place.0,
        ..over_home()
    };
    payload.vehicle([0.0, 0.0, 0.5], Some(over_it));
    run(&mut payload, 3020, 5020);
    assert!(near(
        reported(&mut payload)[1],
        -std::f32::consts::FRAC_PI_2
    ));
    // The same place by its height above home, back where it started.
    payload.vehicle([0.0, 0.0, 0.5], Some(over_home()));
    assert_eq!(
        result(&payload.receive(&roi(3, 0.0)), GIMBAL, 195),
        ACCEPTED
    );
    run(&mut payload, 5020, 7020);
    assert!(near(reported(&mut payload)[1], (-45_f32).to_radians()));
    // A height above terrain it has no map for, and a place not on Earth.
    assert_eq!(result(&payload.receive(&roi(10, 0.0)), GIMBAL, 195), DENIED);
    let nowhere = long(GCS, GIMBAL, 195, &[0.0, 0.0, 0.0, 0.0, 95.0, 10.0, 0.0], 0);
    assert_eq!(result(&payload.receive(&nowhere), GIMBAL, 195), DENIED);
    // Only from whoever is in control.
    assert_eq!(tell(&mut payload, OTHER, 197, &[]), DENIED);

    // Let go: "assume a neutral position".
    assert_eq!(tell(&mut payload, GCS, 197, &[]), ACCEPTED);
    run(&mut payload, 7020, 9020);
    let ahead = device_status(&mut payload);
    assert!(wire::angles(ahead.q).unwrap().iter().all(|a| near(*a, 0.0)));
    assert_eq!(ahead.flags.bits() & YAW_LOCK, 0);
    assert_eq!(manager_status(&mut payload).flags.bits(), NEUTRAL);
    // And the vehicle moving no longer moves it.
    payload.vehicle([0.0, 0.0, 0.5], Some(over_it));
    run(&mut payload, 9020, 10_020);
    assert!(near(reported(&mut payload)[1], 0.0));

    // The place as a COMMAND_LONG gives it: degrees, and above sea level.
    let degrees = long(
        GCS,
        GIMBAL,
        195,
        &[0.0, 0.0, 0.0, 0.0, -35.362_362, 149.165_23, 584.0],
        0,
    );
    payload.vehicle([0.0, 0.0, 0.5], Some(over_home()));
    assert_eq!(result(&payload.receive(&degrees), GIMBAL, 195), ACCEPTED);
    run(&mut payload, 10_020, 12_020);
    assert!((reported(&mut payload)[1] - (-45_f32).to_radians()).abs() < 0.05);
}

// Guide: attitude set during a mission. MAV_CMD_DO_GIMBAL_MANAGER_PITCHYAW
// from whoever flies the mission, the device's target and status as
// streams, then the command again with "Flag: NONE".
#[test]
fn a_mission_sets_the_attitude_and_then_hands_the_yaw_back() {
    let mut payload = managed();
    payload.vehicle([0.0; 3], None);
    payload.step(1000);
    // The autopilot flies the mission, and puts itself in control for it.
    take_control(&mut payload, AUTOPILOT);
    let survey = [-90.0, 0.0, NAN, NAN, f32::from(YAW_LOCK)];
    assert_eq!(tell(&mut payload, AUTOPILOT, 1000, &survey), ACCEPTED);
    let statuses = of!(run(&mut payload, 1000, 3000), GimbalDeviceAttitudeStatus);
    assert_eq!(statuses.len(), 20);
    let last = &statuses[19];
    assert!(near(
        wire::angles(last.q).unwrap()[1],
        -std::f32::consts::FRAC_PI_2
    ));
    assert_ne!(last.flags.bits() & YAW_LOCK, 0);
    // The vehicle turns at a waypoint and the camera keeps its bearing.
    payload.vehicle([0.0, 0.0, 1.0], None);
    run(&mut payload, 3000, 3100);
    assert_eq!(manager_status(&mut payload).flags.bits(), YAW_LOCK);

    // Done with it: no flags, level, and following the nose again.
    assert_eq!(
        tell(&mut payload, AUTOPILOT, 1000, &[0.0, 0.0, NAN, NAN, 0.0]),
        ACCEPTED
    );
    let statuses = of!(run(&mut payload, 3100, 5100), GimbalDeviceAttitudeStatus);
    let last = &statuses[statuses.len() - 1];
    assert!(wire::angles(last.q).unwrap().iter().all(|a| near(*a, 0.0)));
    assert_eq!(last.flags.bits() & YAW_LOCK, 0);
    assert_eq!(manager_status(&mut payload).flags.bits(), 0);
}
