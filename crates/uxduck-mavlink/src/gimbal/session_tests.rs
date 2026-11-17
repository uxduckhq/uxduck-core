//! The session and a fake vehicle's gimbal together: S406's demo without
//! the screen. The gimbal manages itself, as the simulated payload does
//! on a vehicle whose autopilot has no mount.

use uxduck_core::domain::command::Command;
use uxduck_core::domain::gimbal::{GimbalAction, GimbalHolder};

use super::{DO_GIMBAL_MANAGER_CONFIGURE, DO_GIMBAL_MANAGER_PITCHYAW, DO_SET_ROI_LOCATION};
use crate::payload::{ManagedBy, Payload, Place};
use crate::session::{CommandOrder, CommandOutcome, Config, Effect, Session};
use crate::sim::FakeVehicle;

const PEER: &str = "127.0.0.1:14550";
const SECOND: u64 = 1_000_000;
const REQUEST_MESSAGE: u16 = 512;
const SET_MESSAGE_INTERVAL: u16 = 511;

struct Rig {
    session: Session,
    vehicle: FakeVehicle,
    now_us: u64,
    /// Every command UXDuck sent: its target component, number and
    /// parameters.
    sent: Vec<(u8, u16, [f32; 7])>,
    made: Vec<Effect>,
}

impl Rig {
    /// The gimbal found, three seconds in.
    fn new() -> Self {
        let mut vehicle = FakeVehicle::new(1, 0x1234);
        let mut payload = Payload::new(1).with_gimbal(ManagedBy::Itself);
        payload.vehicle(
            [0.0; 3],
            Some(Place {
                lat: 473_977_000,
                lon: 85_456_000,
                alt_mm: 538_000,
                relative_alt_mm: 50_000,
            }),
        );
        vehicle.payload = Some(payload);
        let mut rig = Self {
            session: Session::new(Config::default()),
            vehicle,
            now_us: 0,
            sent: Vec::new(),
            made: Vec::new(),
        };
        for _ in 0..3 {
            rig.second();
        }
        rig
    }

    fn id(&self) -> String {
        self.session
            .vehicle_at("l1", PEER, 1)
            .expect("the vehicle")
            .to_owned()
    }

    /// Hands the session's frames to the vehicle and its answers back,
    /// until neither says more.
    fn converse(&mut self, effects: Vec<Effect>) {
        let mut pending = effects;
        for _ in 0..1000 {
            if pending.is_empty() {
                break;
            }
            let mut next = Vec::new();
            for effect in pending {
                let Effect::Send { bytes, .. } = effect else {
                    self.made.push(effect);
                    continue;
                };
                if let Some(command) = crate::payload::wire::command(&bytes) {
                    self.sent
                        .push((command.to.1, command.command, command.params));
                }
                for reply in self.vehicle.answer(&bytes) {
                    next.extend(self.session.ingest("l1", PEER, &reply, self.now_us));
                }
            }
            pending = next;
        }
    }

    /// A tenth of a second: the vehicle's and the payload's clocks move,
    /// what they send arrives, and time is told.
    fn step(&mut self) {
        self.now_us += SECOND / 10;
        #[allow(clippy::cast_precision_loss, reason = "a test's few seconds")]
        let t_s = self.now_us as f64 / 1e6;
        let mut frames = Vec::new();
        if self.now_us.is_multiple_of(SECOND) {
            frames.push(self.vehicle.heartbeat());
        }
        frames.extend(self.vehicle.telemetry(t_s));
        frames.extend(self.vehicle.payload_step(t_s));
        let mut effects = Vec::new();
        for frame in frames {
            effects.extend(self.session.ingest("l1", PEER, &frame, self.now_us));
        }
        effects.extend(self.session.tick(self.now_us));
        self.converse(effects);
    }

    fn second(&mut self) {
        for _ in 0..10 {
            self.step();
        }
    }

    /// Has the gimbal do something, as the engine would.
    fn act(&mut self, receipt: &str, action: GimbalAction) {
        let order = CommandOrder {
            receipt_id: receipt.into(),
            vehicle_id: self.id(),
            command: Command::Gimbal {
                component_id: Some(154),
                device_id: Some(1),
                action,
            },
            force: false,
        };
        let effects = self.session.command(order, self.now_us);
        self.converse(effects);
    }

    fn done(&self, receipt: &str) -> Option<CommandOutcome> {
        self.made.iter().find_map(|effect| match effect {
            Effect::CommandDone {
                receipt_id,
                outcome,
                ..
            } if receipt_id == receipt => Some(outcome.clone()),
            _ => None,
        })
    }

    /// The gimbal as it was last told.
    fn gimbal(&self) -> &super::Observation {
        self.made
            .iter()
            .rev()
            .find_map(|effect| match effect {
                Effect::Gimbal(seen) => Some(&**seen),
                _ => None,
            })
            .expect("a gimbal was told")
    }

    /// The parameters of the commands of one kind sent to a component.
    fn commands(&self, component: u8, command: u16) -> Vec<[f32; 7]> {
        self.sent
            .iter()
            .filter(|(to, sent, _)| *to == component && *sent == command)
            .map(|(_, _, params)| *params)
            .collect()
    }

    /// How often a component was sent a command about a message.
    fn about(&self, component: u8, command: u16, message: f32) -> usize {
        self.commands(component, command)
            .iter()
            .filter(|params| (params[0] - message).abs() < 0.5)
            .count()
    }
}

#[test]
fn a_gimbal_that_manages_itself_is_found_with_no_one_holding_it() {
    let mut rig = Rig::new();
    let gimbal = rig.gimbal().clone();
    assert_eq!(gimbal.vehicle_id, rig.id());
    assert_eq!((gimbal.component_id, gimbal.device_id), (154, 1));
    assert_eq!(gimbal.device_component_id, Some(154));
    assert!(gimbal.present);
    assert_eq!(gimbal.holder, GimbalHolder::NoOne);
    assert_eq!((gimbal.primary, gimbal.secondary), (None, None));
    assert_ne!(gimbal.cap_flags & 131_072, 0, "it can look at a place");
    let straight_down = gimbal.limits.pitch_min_deg.expect("a limit");
    assert!((straight_down + 90.0).abs() < 0.01, "{straight_down}");
    assert_eq!(gimbal.yaw_locked, Some(false));
    // The gimbal answered at once. The autopilot, which manages none, was
    // asked three times, three seconds apart, and then left.
    for _ in 0..10 {
        rig.second();
    }
    assert_eq!(rig.about(1, REQUEST_MESSAGE, 280.0), 3, "{:?}", rig.sent);
    // The gimbal is asked what it manages once more after it has settled,
    // as an ArduPilot's limits change after its first answer.
    for _ in 0..10 {
        rig.second();
    }
    assert_eq!(rig.about(154, REQUEST_MESSAGE, 280.0), 2, "{:?}", rig.sent);
    // It says who is in control by itself: it is not asked to.
    assert_eq!(rig.about(154, SET_MESSAGE_INTERVAL, 281.0), 0);
    // Message 286 is asked of no one.
    assert!(
        rig.sent
            .iter()
            .all(|(_, _, params)| (params[0] - 286.0).abs() > 0.5),
        "{:?}",
        rig.sent
    );
}

#[test]
fn control_is_taken_the_gimbal_pointed_and_control_given_back() {
    let mut rig = Rig::new();
    // The simulated manager moves only for whoever is in control.
    let point = GimbalAction::Point {
        pitch_deg: -45.0,
        yaw_deg: 30.0,
        lock: false,
    };
    rig.act("early", point);
    assert!(
        matches!(rig.done("early"), Some(CommandOutcome::Rejected(_))),
        "{:?}",
        rig.done("early")
    );
    rig.act("take", GimbalAction::TakeControl);
    assert_eq!(rig.done("take"), Some(CommandOutcome::Accepted));
    assert_eq!(
        rig.commands(154, DO_GIMBAL_MANAGER_CONFIGURE),
        [[-2.0, -2.0, -1.0, -1.0, 0.0, 0.0, 1.0]]
    );
    rig.second();
    assert_eq!(rig.gimbal().holder, GimbalHolder::Uxduck);
    assert_eq!(rig.gimbal().primary, Some((255, 190)));

    rig.act("point", point);
    assert_eq!(rig.done("point"), Some(CommandOutcome::Accepted));
    for _ in 0..3 {
        rig.second();
    }
    let manager = ("l1".to_owned(), PEER.to_owned(), 1, 154);
    let (pitch, yaw, north) = rig
        .session
        .gimbal_pointing(&manager, 1)
        .expect("it says where it points");
    assert!(
        (pitch + 45.0).abs() < 0.5 && (yaw - 30.0).abs() < 0.5,
        "{pitch} {yaw}"
    );
    assert!(!north, "from the nose");

    // Holding north goes as the angles it has now, from north.
    rig.act("hold", GimbalAction::HoldNorth);
    assert_eq!(rig.done("hold"), Some(CommandOutcome::Accepted));
    let held = *rig
        .commands(154, DO_GIMBAL_MANAGER_PITCHYAW)
        .last()
        .expect("sent");
    assert!((held[0] + 45.0).abs() < 0.5, "{held:?}");
    assert!(
        held[1].is_finite() && (held[4] - 16.0).abs() < 0.5,
        "{held:?}"
    );
    rig.second();
    assert_eq!(rig.gimbal().yaw_locked, Some(true));
    // Following again goes as its yaw from the nose now, which the
    // vehicle's own turn has changed while it was held.
    let (_, from_nose, _) = rig
        .session
        .gimbal_pointing(&manager, 1)
        .expect("it says where it points");
    rig.act("follow", GimbalAction::Follow);
    assert_eq!(rig.done("follow"), Some(CommandOutcome::Accepted));
    let followed = *rig
        .commands(154, DO_GIMBAL_MANAGER_PITCHYAW)
        .last()
        .expect("sent");
    assert!((followed[1] - from_nose).abs() < 0.5, "{followed:?}");
    assert!(followed[4].abs() < 0.5, "{followed:?}");
    rig.second();
    assert_eq!(rig.gimbal().yaw_locked, Some(false));

    // A place to look at goes as a COMMAND_INT with its height.
    rig.act(
        "look",
        GimbalAction::LookAt {
            latitude_deg: 47.398,
            longitude_deg: 8.546,
            altitude_amsl_m: Some(488.0),
        },
    );
    assert_eq!(rig.done("look"), Some(CommandOutcome::Accepted));
    assert_eq!(rig.commands(154, DO_SET_ROI_LOCATION).len(), 1);
    rig.act("centre", GimbalAction::Centre);
    assert_eq!(rig.done("centre"), Some(CommandOutcome::Accepted));

    rig.act("give", GimbalAction::ReleaseControl);
    assert_eq!(rig.done("give"), Some(CommandOutcome::Accepted));
    rig.second();
    assert_eq!(rig.gimbal().holder, GimbalHolder::NoOne);
}

#[test]
fn where_it_points_comes_fast_only_while_someone_watches() {
    let mut rig = Rig::new();
    let slow = |rig: &Rig| {
        rig.commands(154, SET_MESSAGE_INTERVAL)
            .iter()
            .filter(|p| (p[0] - 285.0).abs() < 0.5 && (p[1] - 1e6).abs() < 0.5)
            .count()
    };
    let fast = |rig: &Rig| {
        rig.commands(154, SET_MESSAGE_INTERVAL)
            .iter()
            .filter(|p| (p[0] - 285.0).abs() < 0.5 && (p[1] - 1e5).abs() < 0.5)
            .count()
    };
    // The simulated gimbal says where it points ten times a second by
    // itself. No one watches: it is asked for once a second, and does so.
    for _ in 0..6 {
        rig.second();
    }
    assert_eq!((slow(&rig), fast(&rig)), (1, 0), "{:?}", rig.sent);
    let id = rig.id();
    rig.session.watch_gimbals(&id, true);
    for _ in 0..6 {
        rig.second();
    }
    assert_eq!((slow(&rig), fast(&rig)), (1, 1), "{:?}", rig.sent);
    rig.session.watch_gimbals(&id, false);
    for _ in 0..6 {
        rig.second();
    }
    assert_eq!((slow(&rig), fast(&rig)), (2, 1), "{:?}", rig.sent);
    // The autopilot, which sends no such message of this gimbal, is not
    // asked for it beyond its usual streams.
    assert!(
        rig.commands(1, SET_MESSAGE_INTERVAL)
            .iter()
            .all(|p| (p[0] - 285.0).abs() > 0.5 || (p[1] - 1e6).abs() < 0.5)
    );
}

#[test]
fn a_vehicle_watched_under_the_id_it_had_first_is_one_vehicle() {
    let mut rig = Rig::new();
    let fast = |rig: &Rig| {
        rig.commands(154, SET_MESSAGE_INTERVAL)
            .iter()
            .filter(|p| (p[0] - 285.0).abs() < 0.5 && (p[1] - 1e5).abs() < 0.5)
            .count()
    };
    let slow = |rig: &Rig| {
        rig.commands(154, SET_MESSAGE_INTERVAL)
            .iter()
            .filter(|p| (p[0] - 285.0).abs() < 0.5 && (p[1] - 1e6).abs() < 0.5)
            .count()
    };
    // What the vehicle was called before its UID came.
    let id = rig.id();
    let first = rig
        .made
        .iter()
        .find_map(|effect| match effect {
            Effect::Vehicle(seen) if seen.id != id => Some(seen.id.clone()),
            _ => None,
        })
        .expect("the id it had first");
    for _ in 0..6 {
        rig.second();
    }
    assert_eq!((slow(&rig), fast(&rig)), (1, 0), "{:?}", rig.sent);

    // Watched under that id, as a page that heard of it early would say.
    rig.session.watch_gimbals(&first, true);
    for _ in 0..6 {
        rig.second();
    }
    assert_eq!((slow(&rig), fast(&rig)), (1, 1), "{:?}", rig.sent);

    // And no longer watched once that is said under the id it has now.
    rig.session.watch_gimbals(&id, false);
    for _ in 0..6 {
        rig.second();
    }
    assert_eq!((slow(&rig), fast(&rig)), (2, 1), "{:?}", rig.sent);
}

// S407: a gimbal turned at rates, as a controller's inputs ask.

use mavio::dialects::Ardupilotmega;

/// 30 degrees a second, as a full push asks for.
const RATE: f32 = std::f32::consts::FRAC_PI_6;

/// The rates messages among what the session sends: the component and
/// the device they name, their flags, and their angles and rates.
fn rate_messages(effects: &[Effect]) -> Vec<(u8, u8, u16, [f32; 4])> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Send { bytes, .. } => match crate::payload::wire::typed(bytes) {
                Some((_, Ardupilotmega::GimbalManagerSetPitchyaw(sent))) => Some((
                    sent.target_component,
                    sent.gimbal_device_id,
                    sent.flags.bits(),
                    [sent.pitch, sent.yaw, sent.pitch_rate, sent.yaw_rate],
                )),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// The rates of the only rates message sent, which has no angles.
fn only_rates(effects: &[Effect]) -> (u16, [f32; 2]) {
    let sent = rate_messages(effects);
    assert_eq!(sent.len(), 1, "{sent:?}");
    let (component, device, flags, [pitch, yaw, pitch_rate, yaw_rate]) = sent[0];
    assert_eq!((component, device), (154, 1));
    assert!(pitch.is_nan() && yaw.is_nan(), "rates and no angles");
    (flags, [pitch_rate, yaw_rate])
}

impl Rig {
    /// The gimbal taken and watched, so it says where it points ten
    /// times a second.
    fn taken() -> Self {
        let mut rig = Self::new();
        rig.act("take", GimbalAction::TakeControl);
        assert_eq!(rig.done("take"), Some(CommandOutcome::Accepted));
        let id = rig.id();
        rig.session.watch_gimbals(&id, true);
        rig.second();
        rig
    }

    fn pointing(&self) -> (f32, f32) {
        let manager = ("l1".to_owned(), PEER.to_owned(), 1, 154);
        let (pitch, yaw, _) = self
            .session
            .gimbal_pointing(&manager, 1)
            .expect("it says where it points");
        (pitch, yaw)
    }

    /// A tenth of a second in which the vehicle is heard only if `heard`:
    /// how many rates messages the session sent by itself in it.
    fn tenth(&mut self, heard: bool) -> usize {
        self.now_us += SECOND / 10;
        #[allow(clippy::cast_precision_loss, reason = "a test's few seconds")]
        let t_s = self.now_us as f64 / 1e6;
        let mut effects = Vec::new();
        if heard {
            let mut frames = Vec::new();
            if self.now_us.is_multiple_of(SECOND) {
                frames.push(self.vehicle.heartbeat());
            }
            frames.extend(self.vehicle.telemetry(t_s));
            frames.extend(self.vehicle.payload_step(t_s));
            for frame in frames {
                effects.extend(self.session.ingest("l1", PEER, &frame, self.now_us));
            }
        }
        effects.extend(self.session.tick(self.now_us));
        let sent = rate_messages(&effects).len();
        if heard {
            self.converse(effects);
        }
        sent
    }
}

#[test]
fn rates_turn_the_gimbal_and_it_goes_on_until_a_zero_rate_stops_it() {
    let mut rig = Rig::taken();
    let id = rig.id();
    let (pitch, yaw) = rig.pointing();

    // To the right for two seconds, told every tenth of one.
    for _ in 0..20 {
        let effects = rig.session.gimbal_rates(&id, 154, 1, [0.0, RATE]);
        let (flags, rates) = only_rates(&effects);
        assert_eq!(flags, 0, "its yaw follows the nose, as it did");
        assert_eq!(rates, [0.0, RATE]);
        rig.converse(effects);
        rig.step();
    }
    let turned = rig.pointing().1 - yaw;
    assert!((turned - 60.0).abs() < 8.0, "{turned}");
    assert!((rig.pointing().0 - pitch).abs() < 0.5, "only its yaw");

    // Nothing more is said: it goes on at the last rate it heard, which
    // is why whoever sends rates ends with a stop.
    rig.second();
    let ran_on = rig.pointing().1 - yaw;
    assert!(ran_on - turned > 20.0, "{turned} then {ran_on}");

    // Zero rates stop it: one now, and two more as time goes on.
    let effects = rig.session.gimbal_stop(&id, 154, 1);
    assert_eq!(only_rates(&effects).1, [0.0, 0.0]);
    rig.converse(effects);
    assert_eq!(
        [
            rig.tenth(true),
            rig.tenth(true),
            rig.tenth(true),
            rig.tenth(true)
        ],
        [1, 1, 0, 0]
    );
    let stopped = rig.pointing().1;
    rig.second();
    assert!(
        (rig.pointing().1 - stopped).abs() < 0.5,
        "it stays where it stopped"
    );

    // Up and down are its pitch, up positive.
    for _ in 0..10 {
        let effects = rig.session.gimbal_rates(&id, 154, 1, [-RATE, 0.0]);
        rig.converse(effects);
        rig.step();
    }
    let effects = rig.session.gimbal_stop(&id, 154, 1);
    rig.converse(effects);
    rig.second();
    let down = rig.pointing().0 - pitch;
    assert!((down + 30.0).abs() < 5.0, "{down}");
    assert!((rig.pointing().1 - stopped).abs() < 0.5);
}

#[test]
fn rates_leave_the_yaw_held_from_north_or_from_the_nose_as_it_was() {
    let mut rig = Rig::taken();
    let id = rig.id();
    rig.act("hold", GimbalAction::HoldNorth);
    assert_eq!(rig.done("hold"), Some(CommandOutcome::Accepted));
    rig.second();
    assert_eq!(rig.gimbal().yaw_locked, Some(true));

    let effects = rig.session.gimbal_rates(&id, 154, 1, [0.0, -RATE]);
    assert_eq!(only_rates(&effects), (16, [0.0, -RATE]));
    rig.converse(effects);
    rig.second();
    let effects = rig.session.gimbal_stop(&id, 154, 1);
    assert_eq!(only_rates(&effects), (16, [0.0, 0.0]));
    rig.converse(effects);
    rig.second();
    assert_eq!(rig.gimbal().yaw_locked, Some(true), "still held from north");
}

#[test]
fn a_stop_owed_on_a_paused_link_goes_when_it_runs_again() {
    let mut rig = Rig::taken();
    let id = rig.id();
    let effects = rig.session.gimbal_rates(&id, 154, 1, [RATE, 0.0]);
    rig.converse(effects);

    rig.session.pause("l1", true, rig.now_us);
    assert!(rig.session.gimbal_stop(&id, 154, 1).is_empty());
    for _ in 0..30 {
        assert_eq!(rig.tenth(false), 0, "nothing is sent on a paused link");
    }
    rig.session.pause("l1", false, rig.now_us);
    assert_eq!(
        [
            rig.tenth(true),
            rig.tenth(true),
            rig.tenth(true),
            rig.tenth(true)
        ],
        [1, 1, 1, 0],
        "all three, since none went"
    );
}

#[test]
fn a_vehicle_that_is_not_heard_is_told_to_stop_again_when_it_is() {
    let mut rig = Rig::taken();
    let id = rig.id();
    let effects = rig.session.gimbal_rates(&id, 154, 1, [RATE, 0.0]);
    rig.converse(effects);

    // The link goes quiet while the input is still pushed, and then the
    // input is let go: one stop goes anyway, in case it is only UXDuck
    // that cannot hear, and the rest wait.
    for _ in 0..30 {
        rig.tenth(false);
    }
    assert_eq!(
        rate_messages(&rig.session.gimbal_stop(&id, 154, 1)).len(),
        1
    );
    for _ in 0..50 {
        assert_eq!(rig.tenth(false), 0, "kept for when it is heard");
    }
    // Heard again: told twice more.
    let mut sent = 0;
    for _ in 0..20 {
        sent += rig.tenth(true);
    }
    assert_eq!(sent, 2);

    // Let go only after the vehicle was long given up as offline: told
    // three times in all once it is back.
    let effects = rig.session.gimbal_rates(&id, 154, 1, [RATE, 0.0]);
    rig.converse(effects);
    for _ in 0..600 {
        rig.tenth(false);
    }
    let mut sent = rate_messages(&rig.session.gimbal_stop(&id, 154, 1)).len();
    for _ in 0..50 {
        assert_eq!(rig.tenth(false), 0);
    }
    for _ in 0..20 {
        sent += rig.tenth(true);
    }
    assert_eq!(sent, 3);
}

#[test]
fn new_rates_are_the_last_word_and_a_vehicle_that_is_gone_is_told_nothing() {
    let mut rig = Rig::taken();
    let id = rig.id();
    let effects = rig.session.gimbal_stop(&id, 154, 1);
    rig.converse(effects);
    // Pushed again at once: the stops still owed are not sent after it.
    let effects = rig.session.gimbal_rates(&id, 154, 1, [0.0, RATE]);
    rig.converse(effects);
    assert_eq!([rig.tenth(true), rig.tenth(true)], [0, 0]);

    assert!(
        rig.session
            .gimbal_rates("nobody", 154, 1, [0.0, RATE])
            .is_empty()
    );
    assert!(rig.session.gimbal_stop("nobody", 154, 1).is_empty());
    let effects = rig.session.gimbal_stop(&id, 154, 1);
    rig.converse(effects);
    rig.session.remove_link("l1");
    assert_eq!(rig.tenth(false), 0);
}
