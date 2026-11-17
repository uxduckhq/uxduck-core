//! The session and an autopilot that manages a gimbal device, as PX4 and
//! ArduPilot each do: what the session sends each for the same action
//! (S413). No manager is played here. What PX4's answers is in the
//! daemon's tests, on PX4's simulator.

use mavio::dialects::Ardupilotmega;
use mavio::dialects::ardupilotmega::enums::{
    GimbalDeviceErrorFlags, GimbalDeviceFlags, GimbalManagerCapFlags,
};
use mavio::dialects::ardupilotmega::messages::{
    GimbalDeviceAttitudeStatus, GimbalManagerInformation,
};
use uxduck_core::domain::command::Command;
use uxduck_core::domain::gimbal::GimbalAction;

use crate::session::{CommandOrder, Config, Effect, Session};
use crate::sim::FakeVehicle;

const PEER: &str = "127.0.0.1:14550";
const SECOND: u64 = 1_000_000;
/// UXDuck's own system and component, as the session's defaults have them.
const OURS: (f32, f32) = (255.0, 190.0);
const RATE: f32 = std::f32::consts::FRAC_PI_6;

/// A vehicle whose autopilot said it manages a gimbal.
struct Managed {
    session: Session,
    vehicle: FakeVehicle,
    now_us: u64,
    px4: bool,
}

impl Managed {
    /// A PX4, which names the gimbal by its device's component (154), or
    /// an ArduPilot, which calls its first gimbal 1. Three seconds in, the
    /// autopilot has said what it manages and where the gimbal points,
    /// with its yaw from the nose and held from north only if `locked`.
    fn new(px4: bool, locked: bool) -> Self {
        let mut vehicle = FakeVehicle::new(1, 0x1234);
        if px4 {
            vehicle.make_px4(false, true);
        }
        let mut rig = Self {
            session: Session::new(Config::default()),
            vehicle,
            now_us: 0,
            px4,
        };
        assert_eq!(
            (
                f32::from(Config::default().gcs_system_id),
                f32::from(crate::GCS_COMPONENT_ID)
            ),
            OURS
        );
        for _ in 0..30 {
            rig.step(locked);
        }
        rig
    }

    /// What the manager calls the gimbal.
    fn device(&self) -> u8 {
        if self.px4 { 154 } else { 1 }
    }

    fn id(&self) -> String {
        self.session
            .vehicle_at("l1", PEER, 1)
            .expect("the vehicle")
            .to_owned()
    }

    /// A tenth of a second: the vehicle speaks, its autopilot says what it
    /// manages, and the gimbal says where it points, 10° down and 30°
    /// right of the nose. PX4 leaves that to the device's own component.
    /// ArduPilot says it itself.
    fn step(&mut self, locked: bool) {
        self.now_us += SECOND / 10;
        #[allow(clippy::cast_precision_loss, reason = "a test's few seconds")]
        let t_s = self.now_us as f64 / 1e6;
        let mut frames = Vec::new();
        if self.now_us.is_multiple_of(SECOND) {
            frames.push(self.vehicle.heartbeat());
            frames.push(crate::sim::frame(
                1,
                1,
                0,
                &GimbalManagerInformation {
                    time_boot_ms: 0,
                    cap_flags: GimbalManagerCapFlags::from_bits_retain(6071),
                    gimbal_device_id: self.device(),
                    roll_min: 0.0,
                    roll_max: 0.0,
                    pitch_min: -1.57,
                    pitch_max: 0.6,
                    yaw_min: -3.1,
                    yaw_max: 3.1,
                },
            ));
        }
        frames.extend(self.vehicle.telemetry(t_s));
        let yaw_lock = if locked { 16 } else { 0 };
        frames.push(crate::sim::frame(
            1,
            if self.px4 { 154 } else { 1 },
            0,
            &GimbalDeviceAttitudeStatus {
                target_system: 0,
                target_component: 0,
                time_boot_ms: 0,
                flags: GimbalDeviceFlags::from_bits_retain(4 | 8 | 32 | yaw_lock),
                q: crate::payload::wire::quaternion([
                    0.0,
                    -10.0_f32.to_radians(),
                    30.0_f32.to_radians(),
                ]),
                angular_velocity_x: 0.0,
                angular_velocity_y: 0.0,
                angular_velocity_z: 0.0,
                failure_flags: GimbalDeviceErrorFlags::empty(),
                delta_yaw: f32::NAN,
                delta_yaw_velocity: f32::NAN,
                gimbal_device_id: u8::from(!self.px4),
            },
        ));
        let mut pending = Vec::new();
        for frame in frames {
            pending.extend(self.session.ingest("l1", PEER, &frame, self.now_us));
        }
        pending.extend(self.session.tick(self.now_us));
        // The vehicle answers what the session asks, until neither says more.
        for _ in 0..1000 {
            if pending.is_empty() {
                break;
            }
            let mut next = Vec::new();
            for effect in pending {
                let Effect::Send { bytes, .. } = effect else {
                    continue;
                };
                for reply in self.vehicle.answer(&bytes) {
                    next.extend(self.session.ingest("l1", PEER, &reply, self.now_us));
                }
            }
            pending = next;
        }
    }

    /// The command the session sends for an action on the gimbal the
    /// component `manager` calls `device`: its number, its seven
    /// parameters, and a `COMMAND_INT`'s frame and place.
    fn sends(
        &mut self,
        manager: u8,
        device: u8,
        action: GimbalAction,
    ) -> crate::payload::wire::Command {
        let order = CommandOrder {
            receipt_id: "r".into(),
            vehicle_id: self.id(),
            command: Command::Gimbal {
                component_id: Some(manager),
                device_id: Some(device),
                action,
            },
            force: false,
        };
        let effects = self.session.command(order, self.now_us);
        let sent: Vec<_> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Send { bytes, .. } => crate::payload::wire::command(bytes),
                _ => None,
            })
            .collect();
        let [command] = sent.try_into().expect("one command");
        assert_eq!(command.to, (1, manager));
        command
    }

    /// The one message the session sends to turn the gimbal at rates.
    fn turns(&mut self, manager: u8, device: u8, rates: [f32; 2]) -> Ardupilotmega {
        let id = self.id();
        let effects = self.session.gimbal_rates(&id, manager, device, rates);
        let [Effect::Send { bytes, .. }] = &effects[..] else {
            panic!("one message: {effects:?}");
        };
        let (from, message) = crate::payload::wire::typed(bytes).expect("a message");
        assert_eq!((f32::from(from.0), f32::from(from.1)), OURS);
        message
    }
}

/// The parameters an autopilot's manager is sent for an action, on a
/// vehicle of its own each, since one command of a kind waits for its
/// answer.
fn sent(px4: bool, action: GimbalAction) -> (u16, [f32; 7]) {
    let mut rig = Managed::new(px4, false);
    let device = rig.device();
    let command = rig.sends(1, device, action);
    (command.command, command.params)
}

fn point(lock: bool) -> GimbalAction {
    GimbalAction::Point {
        pitch_deg: -45.0,
        yaw_deg: 30.0,
        lock,
    }
}

/// S413: PX4 reads -2 as the autopilot itself, so UXDuck names its own
/// system and component, and PX4 hands a movement's flags to the device,
/// so roll and pitch are held from the horizon (4 and 8) beside the yaw's
/// own flag (16).
#[test]
fn px4s_manager_is_told_who_takes_the_gimbal_and_to_hold_the_horizon() {
    assert_eq!(
        sent(true, GimbalAction::TakeControl),
        (1001, [OURS.0, OURS.1, -1.0, -1.0, 0.0, 0.0, 154.0])
    );
    // Given back as any manager is told: -3, which PX4 takes from whoever
    // is in control.
    assert_eq!(
        sent(true, GimbalAction::ReleaseControl),
        (1001, [-3.0, -3.0, -1.0, -1.0, 0.0, 0.0, 154.0])
    );
    for (action, pitch, yaw, flags) in [
        (point(false), -45.0, 30.0, 12.0),
        (point(true), -45.0, 30.0, 28.0),
        (GimbalAction::Centre, 0.0, 0.0, 12.0),
        // Where it points now, 10° down and 30° right, kept.
        (GimbalAction::Follow, -10.0, 30.0, 12.0),
    ] {
        let (command, params) = sent(true, action);
        assert_eq!(command, 1000, "{action:?}");
        assert!(
            (params[0] - pitch).abs() < 0.01 && (params[1] - yaw).abs() < 0.01,
            "{action:?}: {params:?}"
        );
        assert!(params[2].is_nan() && params[3].is_nan(), "no rates");
        assert_eq!(params[4..], [flags, 0.0, 154.0], "{action:?}");
    }
    // Holding north turns the yaw to be from north, by the vehicle's own
    // heading, and sets all three.
    let (command, params) = sent(true, GimbalAction::HoldNorth);
    assert_eq!((command, params[4]), (1000, 28.0));
    assert!((params[0] + 10.0).abs() < 0.01, "{params:?}");
}

/// A place to look at goes to a PX4 as it goes to any autopilot.
#[test]
fn a_place_to_look_at_goes_to_px4_as_it_is() {
    let place = GimbalAction::LookAt {
        latitude_deg: -35.362_938,
        longitude_deg: 149.165_085,
        altitude_amsl_m: Some(584.0),
    };
    let mut seen = Vec::new();
    for px4 in [true, false] {
        let mut rig = Managed::new(px4, false);
        let device = rig.device();
        let command = rig.sends(1, device, place);
        assert_eq!(command.command, 195);
        assert_eq!(command.params[0], f32::from(device));
        assert_eq!(command.place, Some((0, -353_629_380, 1_491_650_850)));
        seen.push((command.params[1..4].to_vec(), command.params[6]));
    }
    assert_eq!(seen[0], seen[1]);
}

/// What ArduPilot is sent did not change with S413: -2 for the sender, and
/// no flag but the yaw's.
#[test]
fn an_ardupilots_manager_is_sent_what_it_was() {
    assert_eq!(
        sent(false, GimbalAction::TakeControl),
        (1001, [-2.0, -2.0, -1.0, -1.0, 0.0, 0.0, 1.0])
    );
    assert_eq!(
        sent(false, GimbalAction::ReleaseControl),
        (1001, [-3.0, -3.0, -1.0, -1.0, 0.0, 0.0, 1.0])
    );
    for (action, flags) in [
        (point(false), 0.0),
        (point(true), 16.0),
        (GimbalAction::Centre, 0.0),
        (GimbalAction::Follow, 0.0),
        (GimbalAction::HoldNorth, 16.0),
    ] {
        let (command, params) = sent(false, action);
        assert_eq!((command, params[4]), (1000, flags), "{action:?}");
        assert_eq!(params[6], 1.0);
    }
}

/// A gimbal that manages itself is not PX4's manager, whatever autopilot
/// its vehicle has: it is sent what any such gimbal is.
#[test]
fn a_gimbal_that_manages_itself_on_a_px4_is_sent_what_any_is() {
    let mut rig = Managed::new(true, false);
    assert_eq!(
        rig.sends(154, 1, GimbalAction::TakeControl).params,
        [-2.0, -2.0, -1.0, -1.0, 0.0, 0.0, 1.0]
    );
    let mut rig = Managed::new(true, false);
    assert_eq!(rig.sends(154, 1, point(true)).params[4], 16.0);
    let Ardupilotmega::GimbalManagerSetPitchyaw(rates) = rig.turns(154, 1, [RATE, 0.0]) else {
        panic!("rates as any gimbal's manager takes them");
    };
    assert_eq!((rates.target_component, rates.flags.bits()), (154, 0));
}

/// S413: PX4 has no `GIMBAL_MANAGER_SET_PITCHYAW`. Its rates go in a
/// `GIMBAL_MANAGER_SET_ATTITUDE` with no quaternion: no roll, pitch and
/// yaw, with roll and pitch held from the horizon and the yaw held as the
/// device last said it is. Zero rates stop it.
#[test]
fn rates_go_to_px4s_manager_as_an_attitude_with_no_quaternion() {
    for (locked, flags) in [(false, 12), (true, 28)] {
        let mut rig = Managed::new(true, locked);
        for rates in [[RATE, -0.5 * RATE], [0.0, 0.0]] {
            let Ardupilotmega::GimbalManagerSetAttitude(sent) = rig.turns(1, 154, rates) else {
                panic!("rates as PX4 takes them");
            };
            assert_eq!((sent.target_system, sent.target_component), (1, 1));
            assert_eq!(sent.gimbal_device_id, 154);
            assert_eq!(sent.flags.bits(), flags);
            assert!(sent.q.iter().all(|c| c.is_nan()), "{:?}", sent.q);
            assert_eq!(
                [
                    sent.angular_velocity_x,
                    sent.angular_velocity_y,
                    sent.angular_velocity_z
                ],
                [0.0, rates[0], rates[1]]
            );
        }
        // The stops that follow a let-go are the same message.
        let id = rig.id();
        let effects = rig.session.gimbal_stop(&id, 1, 154);
        let [Effect::Send { bytes, .. }] = &effects[..] else {
            panic!("one stop now: {effects:?}");
        };
        assert!(matches!(
            crate::payload::wire::typed(bytes),
            Some((_, Ardupilotmega::GimbalManagerSetAttitude(stop)))
                if stop.angular_velocity_y == 0.0 && stop.angular_velocity_z == 0.0
        ));
    }
}

/// And ArduPilot's go as they did: a `GIMBAL_MANAGER_SET_PITCHYAW` with
/// no angles and no flag but the yaw's.
#[test]
fn rates_go_to_an_ardupilots_manager_as_they_did() {
    for (locked, flags) in [(false, 0), (true, 16)] {
        let mut rig = Managed::new(false, locked);
        let Ardupilotmega::GimbalManagerSetPitchyaw(sent) = rig.turns(1, 1, [RATE, -RATE]) else {
            panic!("rates as ArduPilot takes them");
        };
        assert_eq!((sent.target_component, sent.gimbal_device_id), (1, 1));
        assert_eq!(sent.flags.bits(), flags);
        assert!(sent.pitch.is_nan() && sent.yaw.is_nan());
        assert_eq!([sent.pitch_rate, sent.yaw_rate], [RATE, -RATE]);
    }
}
