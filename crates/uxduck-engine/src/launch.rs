//! Starting a simulated vehicle in the air (S33, ADR-0011 M13). Neither
//! ArduPilot's SITL nor its WebAssembly build can start airborne, so the
//! simulation start takes off on the vehicle's behalf: it speeds the
//! simulator up, waits until the vehicle is ready to arm, arms it, takes off
//! in the firmware's own way, and hands over in a holding mode at normal
//! speed. Its commands are journaled as the simulation start's, never a
//! person's or the agent's, and a refusal along the way is retried quietly;
//! only the outcome is shown, on the link (`Link::launch`).
//! A new submarine also prepares here: wait for positioning and pre-arm
//! checks, select `POSHOLD`, arm, and confirm the heartbeat without taking off.
//!
//! A simulation started from a snapshot (S34) runs through here too, in the
//! air or on the ground: once the new vehicle's own parameters and mission
//! have downloaded, the snapshot's are written back over them, and after
//! the take-off its mode (and mission item) is restored.
//!
//! The engine has no timer: each step is checked again whenever the vehicle
//! is heard, which telemetry does several times a second.

use std::collections::BTreeMap;

use uxduck_core::domain::command::{Command, Receipt, ReceiptStatus};
use uxduck_core::domain::link::{
    Launch as LaunchState, LaunchStage, LinkKind, SimAutopilot, SimVehicle,
};
use uxduck_core::domain::notification::{NotificationKind, Severity, Subject};
use uxduck_core::domain::parameter::{ParamKind, TransferStatus, parameter_id, same_value};
use uxduck_core::domain::snapshot::{Snapshot, SnapshotParameter};
use uxduck_core::domain::telemetry::AltitudeFrame;
use uxduck_core::domain::vehicle::{Presence, Vehicle};
use uxduck_core::events::{Actor, Change};

use crate::Engine;
use crate::commands::Raise;
use crate::runtime::CommandOrder;

/// Who the take-off's commands and parameter writes are recorded as.
pub const ACTOR: &str = "simulation-start";

/// The speed-up while taking off. ArduPilot's SITL did not keep up at 10
/// when the maintainer tried it (2026-10-01), so 9; the browser's build
/// managed 100 in the S20 spike (docs/plan/s20-spike.md).
const TAKE_OFF_SPEEDUP: f64 = 9.0;

/// How long, in real time, the vehicle may take to be heard, to be ready
/// to arm, or to climb; each is checked separately.
const HEARD_US: u64 = 60_000_000;
const READY_US: u64 = 120_000_000;
const CLIMB_US: u64 = 120_000_000;

/// A refused mode change or take-off is tried again after this, up to
/// `TRIES` times. Arming retries use the readiness deadline instead: home
/// and position can still be settling after the pre-arm report is healthy.
/// Short: at `TAKE_OFF_SPEEDUP` a second is nine of the simulator's, and a
/// copter left armed on the ground disarms itself after ten.
const RETRY_US: u64 = 500_000;
const TRIES: u8 = 8;

/// Below this height above home a vehicle whose take-off failed is disarmed
/// rather than left holding.
const ON_THE_GROUND_M: f64 = 2.0;

/// A plane is handed over in LOITER once this high (or at 90% of its height
/// when that is lower), then climbs on to its height there. `ArduPlane`'s
/// TAKEOFF climbs at full throttle with its pitch on speed alone: a jet
/// left in it to 90 m overshot to 115 m and sank back to 75 m in LOITER,
/// while LOITER climbs at `TECS_CLMB_MAX` and stops at its height (S143,
/// 2026-10-19). Lower, the circle's bank starts just above the runway.
const LIFT_OFF_M: f64 = 30.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// Until the vehicle on the link is heard.
    Heard,
    /// `SIM_SPEEDUP` up.
    SpeedUp,
    /// A snapshot's parameters written back, one at a time, where they
    /// differ from the new vehicle's.
    Parameters,
    /// A snapshot's mission uploaded.
    Mission,
    /// Until `SYS_STATUS` says the pre-arm checks pass.
    Ready,
    /// A plane's `TKOFF_ALT`.
    TakeOffHeight,
    /// The mode it takes off in.
    TakeOffMode,
    Arm,
    /// Arming is complete only when a fresh heartbeat reports it.
    Armed,
    /// `MAV_CMD_NAV_TAKEOFF`, for a multicopter or a VTOL in `GUIDED`.
    TakeOff,
    /// Until a plane is clear of the ground (`LIFT_OFF_M`).
    LiftOff,
    /// Until it is near the height.
    Climb,
    /// The holding mode it is handed over in.
    Hold,
    /// A plane's height, sent once it holds (`MAV_CMD_DO_CHANGE_ALTITUDE`).
    Height,
    /// A snapshot's mission item, before its mode is AUTO again.
    MissionItem,
    /// A snapshot's mode.
    Mode,
    /// `SIM_SPEEDUP` back to what was asked for.
    SlowDown,
}

/// Modes a snapshot's vehicle is put back in after the take-off. The rest,
/// such as STABILIZE or FBWA, need a pilot's sticks, which a simulated
/// vehicle has none of until someone takes over; it is handed over holding.
/// So are the ones that follow the throttle stick (`follows_throttle`).
const RESUMABLE: [&str; 12] = [
    "AUTO",
    "GUIDED",
    "LOITER",
    "CIRCLE",
    "RTL",
    "SMART_RTL",
    "BRAKE",
    "ALT_HOLD",
    "POSHOLD",
    "QLOITER",
    "QHOVER",
    "QRTL",
];

/// Modes in which the vehicle climbs or sinks with the throttle stick. With
/// nobody on the sticks a simulator's throttle rests at the bottom, so a
/// vehicle left in one sinks to the ground (maintainer 2026-10-11): a
/// take-off never hands over in one, and a snapshot's is not put back. A
/// plane's LOITER sets its own throttle.
fn follows_throttle(vehicle: SimVehicle, mode: &str) -> bool {
    let modes: &[&str] = match vehicle {
        SimVehicle::Multicopter => &["LOITER", "ALT_HOLD", "POSHOLD"],
        SimVehicle::Vtol => &["QLOITER", "QHOVER"],
        _ => &[],
    };
    modes.iter().any(|m| m.eq_ignore_ascii_case(mode))
}

/// The steps for a simulated vehicle: the take-off for one started in the
/// air, and the snapshot's parameters, mission and mode for one started
/// from a snapshot.
fn plan(vehicle: SimVehicle, in_air: bool, snapshot: Option<&Snapshot>) -> Vec<Step> {
    use Step::{
        Arm, Climb, Heard, Height, Hold, LiftOff, Mission, MissionItem, Mode, Parameters, Ready,
        SlowDown, SpeedUp, TakeOff, TakeOffHeight, TakeOffMode,
    };
    let mut steps = vec![Heard, SpeedUp];
    if let Some(snapshot) = snapshot {
        steps.push(Parameters);
        // Item 0 is home, which every vehicle has.
        if snapshot.mission.len() > 1 {
            steps.push(Mission);
        }
    }
    // A new submarine starts holding position, ready to accept Go to. A
    // snapshot keeps its existing restoration policy, including disarmed starts.
    if vehicle == SimVehicle::Submarine && snapshot.is_none() {
        return vec![Heard, Ready, Hold, Arm, Step::Armed];
    }
    if in_air {
        steps.push(Ready);
        steps.extend_from_slice(match vehicle {
            // GUIDED, arm, take off, and hold there in GUIDED.
            SimVehicle::Multicopter | SimVehicle::Vtol => &[TakeOffMode, Arm, TakeOff, Climb, Hold],
            // ArduPlane's TAKEOFF mode runs down the runway and lifts off
            // along its heading; LOITER circles where it is and climbs on to
            // the height.
            SimVehicle::Plane | SimVehicle::Jet => &[
                TakeOffHeight,
                TakeOffMode,
                Arm,
                LiftOff,
                Hold,
                Height,
                Climb,
            ],
            // Never in the air: they start ready where they are (S37).
            SimVehicle::Rover | SimVehicle::Boat | SimVehicle::Submarine | SimVehicle::Airship => {
                &[]
            }
        });
        if let Some(snapshot) = snapshot.filter(|s| s.flying())
            && let Some(mode) = snapshot.mode.as_deref()
            && RESUMABLE.iter().any(|m| m.eq_ignore_ascii_case(mode))
            && !follows_throttle(vehicle, mode)
            && !mode.eq_ignore_ascii_case(hold_mode(vehicle))
        {
            if mode.eq_ignore_ascii_case("AUTO") && snapshot.current_item.is_some_and(|i| i > 0) {
                steps.push(MissionItem);
            }
            steps.push(Mode);
        }
    }
    steps.push(SlowDown);
    steps
}

/// PX4's steps, once the vehicle is heard to be PX4: its speed is set when
/// it starts, so there is no speed-up; it takes off from the mode it is
/// in, and holds (Hold, `AUTO.LOITER`) by itself once its take-off ends. Told
/// to hold earlier, a plane loses its climb speed and glides down.
fn plan_px4(vehicle: SimVehicle, in_air: bool, snapshot: Option<&Snapshot>) -> Vec<Step> {
    use Step::{Arm, Climb, Heard, Mission, Parameters, Ready, TakeOff};
    let mut steps = vec![Heard];
    if let Some(snapshot) = snapshot {
        steps.push(Parameters);
        if snapshot.mission.len() > 1 {
            steps.push(Mission);
        }
    }
    let flies = matches!(
        vehicle,
        SimVehicle::Multicopter | SimVehicle::Vtol | SimVehicle::Plane | SimVehicle::Jet
    );
    if in_air && flies {
        steps.extend_from_slice(&[Ready, Arm, TakeOff, Climb]);
    }
    steps
}

fn is_px4(vehicle: &Vehicle) -> bool {
    vehicle.autopilot.eq_ignore_ascii_case("px4")
}

/// The mode a vehicle holds in after its take-off, or when it fails.
fn hold_mode_of(vehicle: &Vehicle, kind: SimVehicle) -> &'static str {
    if is_px4(vehicle) {
        "Hold"
    } else {
        hold_mode(kind)
    }
}

/// Steps after the take-off that change the vehicle's mode, or wait to.
fn changes_mode(step: Step) -> bool {
    matches!(
        step,
        Step::LiftOff | Step::Climb | Step::Hold | Step::Height | Step::MissionItem | Step::Mode
    )
}

fn take_off_mode(vehicle: SimVehicle) -> &'static str {
    match vehicle {
        SimVehicle::Multicopter | SimVehicle::Vtol => "GUIDED",
        SimVehicle::Plane | SimVehicle::Jet => "TAKEOFF",
        SimVehicle::Rover | SimVehicle::Boat => "HOLD",
        SimVehicle::Submarine => "ALT_HOLD",
        SimVehicle::Airship => "MANUAL",
    }
}

fn hold_mode(vehicle: SimVehicle) -> &'static str {
    match vehicle {
        SimVehicle::Plane | SimVehicle::Jet | SimVehicle::Airship => "LOITER",
        // Hold where the take-off ends: LOITER and QLOITER follow the
        // throttle stick (`follows_throttle`).
        SimVehicle::Multicopter | SimVehicle::Vtol => "GUIDED",
        SimVehicle::Rover | SimVehicle::Boat => "HOLD",
        SimVehicle::Submarine => "POSHOLD",
    }
}

/// One simulated vehicle's take-off in progress.
#[derive(Debug)]
pub(crate) struct Launch {
    vehicle: SimVehicle,
    /// Started in the air, rather than only put back from a snapshot.
    in_air: bool,
    steps: Vec<Step>,
    snapshot: Option<Snapshot>,
    /// A snapshot's parameters still to write, once known.
    queue: Option<Vec<SnapshotParameter>>,
    /// When it began: downloads from before do not count.
    started_us: u64,
    height_m: f64,
    /// The speed-up to go back to.
    speedup: u8,
    step: usize,
    /// When the current step started.
    since_us: u64,
    /// One readiness deadline, preserved across refused arming attempts.
    ready_since_us: Option<u64>,
    /// The command or parameter write the current step waits for.
    pending: Option<String>,
    /// When a refused step is tried again.
    retry_at_us: Option<u64>,
    tries: u8,
    /// Take-offs refused, each starting again from the mode; not
    /// reset by the steps between, so a vehicle that always refuses ends.
    restarts: u8,
    /// The vehicle's last refusal, for the reason if it never gets going.
    refusal: Option<String>,
    /// Someone commanded the vehicle during the take-off: its mode is
    /// theirs from then on, so the steps that change it are skipped.
    handed_over: bool,
}

/// Take-offs by link, and which link each of their orders belongs to.
#[derive(Debug, Default)]
pub(crate) struct Launches {
    running: BTreeMap<String, Launch>,
    orders: BTreeMap<String, String>,
}

fn actor() -> Actor {
    Actor {
        client_id: ACTOR.into(),
        operator_id: None,
        identity: None,
    }
}

/// What a step needs to know about its take-off.
#[derive(Clone, Copy)]
struct At {
    kind: SimVehicle,
    /// Telemetry from before this simulator start belongs to the previous flight.
    started_us: u64,
    height_m: f64,
    speedup: u8,
    /// Since the step started.
    waited: u64,
}

/// What a step's command or write came back with.
enum Answer {
    Done,
    Refused(String),
}

impl Engine {
    /// Starts the take-off for a simulator link asked to start in the air
    /// or from a snapshot, and shows it. Called when the link is added and
    /// again when a restart starts its simulator on the ground.
    pub(crate) fn begin_launch(&mut self, link_id: &str) {
        let Some(link) = self.state.links.get(link_id) else {
            return;
        };
        let LinkKind::Simulator {
            vehicle,
            autopilot,
            in_air_m,
            ref snapshot_id,
            speedup,
            ..
        } = link.kind
        else {
            return;
        };
        let snapshot = snapshot_id
            .as_ref()
            .and_then(|id| self.state.snapshots.get(id))
            .cloned();
        if in_air_m.is_none() && snapshot.is_none() && vehicle != SimVehicle::Submarine {
            return;
        }
        let new_submarine = vehicle == SimVehicle::Submarine && snapshot_id.is_none();
        // A new start: the last one's failure, if any, is over.
        self.resolve(&format!("launch:{link_id}"));
        let now = self.clock.now_us();
        let steps = if autopilot == SimAutopilot::Px4 {
            plan_px4(vehicle, in_air_m.is_some(), snapshot.as_ref())
        } else {
            plan(vehicle, in_air_m.is_some(), snapshot.as_ref())
        };
        self.launches.running.insert(
            link_id.to_owned(),
            Launch {
                vehicle,
                in_air: in_air_m.is_some(),
                steps,
                snapshot,
                queue: None,
                started_us: now,
                height_m: in_air_m.unwrap_or(0.0),
                speedup: speedup.unwrap_or(1),
                step: 0,
                since_us: now,
                ready_since_us: None,
                pending: None,
                retry_at_us: None,
                tries: 0,
                restarts: 0,
                refusal: None,
                handed_over: false,
            },
        );
        let stage = if in_air_m.is_some() {
            LaunchStage::Climbing
        } else {
            LaunchStage::Restoring
        };
        let detail = new_submarine.then(|| "Preparing submarine".into());
        self.show_launch(link_id, stage, detail);
    }

    /// Whether a link's simulation start is still under way.
    pub(crate) fn is_launching(&self, link_id: &str) -> bool {
        self.launches.running.contains_key(link_id)
    }

    /// Forgets a removed link's take-off.
    pub(crate) fn drop_launch(&mut self, link_id: &str) {
        self.launches.running.remove(link_id);
        self.launches.orders.retain(|_, link| link != link_id);
    }

    /// A vehicle took a person's or the agent's command during its take-off: from
    /// then on the take-off leaves its mode alone. It no longer waits for
    /// the climb, holds or puts a snapshot's mode back, and a failure does
    /// not hold it either; the simulator still goes back to its speed.
    pub(crate) fn hand_over_launch(&mut self, vehicle_id: &str) {
        let Some(link_id) = self.vehicle(vehicle_id).map(|v| v.link_id.clone()) else {
            return;
        };
        let Some(launch) = self.launches.running.get_mut(&link_id) else {
            return;
        };
        if launch.vehicle == SimVehicle::Submarine && launch.snapshot.is_none() {
            self.drop_launch(&link_id);
            self.clear_launch(&link_id);
            return;
        }
        launch.handed_over = true;
        // Climb waits for nothing sent; Hold or Mode not yet sent is skipped:
        // `next_step` from the step before moves past all of them.
        if launch.pending.is_none() && changes_mode(launch.steps[launch.step]) {
            launch.step -= 1;
            self.next_step(&link_id);
            self.drive_launch(&link_id);
        }
    }

    /// Whether a receipt or parameter write is the simulation start's, whose
    /// refusals are not told to people one by one.
    pub(crate) fn is_launch_order(&self, id: &str) -> bool {
        self.launches.orders.contains_key(id)
    }

    /// A take-off's command or parameter write ended.
    pub(crate) fn launch_order_done(&mut self, id: &str, accepted: bool, detail: Option<String>) {
        let answer = if accepted {
            Answer::Done
        } else {
            Answer::Refused(detail.unwrap_or_else(|| "not accepted".into()))
        };
        self.launch_answered(id, answer);
    }

    fn launch_answered(&mut self, id: &str, answer: Answer) {
        let Some(link_id) = self.launches.orders.remove(id) else {
            return;
        };
        let now = self.clock.now_us();
        // A take-off answered "failed" by a vehicle already climbing is a
        // repeat of one it took (the first answer lost, the command sent
        // again).
        let Some(started_us) = self.launches.running.get(&link_id).map(|l| l.started_us) else {
            return;
        };
        let off_the_ground = self
            .launch_vehicle(&link_id)
            .and_then(|vehicle| self.height_above_home_since(&vehicle.id, started_us))
            .is_some_and(|height| height >= ON_THE_GROUND_M);
        let Some(launch) = self.launches.running.get_mut(&link_id) else {
            return;
        };
        if launch.pending.as_deref() != Some(id) {
            return;
        }
        launch.pending = None;
        let step = launch.steps[launch.step];
        match answer {
            // One parameter written back, or refused (a read-only one):
            // either way, on to the next.
            _ if step == Step::Parameters => {
                if let Some(queue) = launch.queue.as_mut()
                    && !queue.is_empty()
                {
                    queue.remove(0);
                }
            }
            Answer::Done => self.next_step(&link_id),
            // The speed-up is a convenience: a simulator that keeps its own
            // pace still gets into the air, just more slowly. What a
            // snapshot puts back is best effort too: a vehicle without its
            // mission or mode is still flying, holding.
            Answer::Refused(_)
                if matches!(
                    step,
                    Step::SpeedUp | Step::SlowDown | Step::Mission | Step::MissionItem | Step::Mode
                ) =>
            {
                self.next_step(&link_id);
            }
            Answer::Refused(_) if step == Step::TakeOff && off_the_ground => {
                self.next_step(&link_id);
            }
            Answer::Refused(why)
                if step == Step::Arm
                    || (step == Step::Hold && launch.vehicle == SimVehicle::Submarine) =>
            {
                launch.refusal = Some(why.clone());
                let since = launch.ready_since_us.unwrap_or(launch.started_us);
                if now.saturating_sub(since) >= READY_US {
                    self.fail_launch(&link_id, format!("it would not arm: {why}"));
                } else {
                    // SYS_STATUS can become healthy before GUIDED's home
                    // and position checks settle. Recheck readiness within
                    // its original deadline; do not spend take-off retries.
                    if let Some(ready) = launch.steps.iter().position(|s| *s == Step::Ready) {
                        launch.step = ready;
                    }
                    launch.retry_at_us = Some(now + RETRY_US);
                }
            }
            Answer::Refused(why) => {
                launch.tries += 1;
                launch.refusal = Some(why.clone());
                // A refused take-off starts again from the mode: by
                // the time it is tried again the vehicle may have disarmed
                // itself, or left the mode, and the same step alone would
                // be refused every time.
                if step == Step::TakeOff
                    && let Some(mode) = launch.steps.iter().position(|s| *s == Step::TakeOffMode)
                {
                    launch.step = mode;
                    launch.restarts += 1;
                }
                if launch.tries.max(launch.restarts) >= TRIES {
                    let what = match step {
                        Step::TakeOffHeight => "it didn't reach its take-off height",
                        Step::Arm => "it would not arm",
                        Step::TakeOff => "it would not take off",
                        _ => "it would not change mode",
                    };
                    self.fail_launch(&link_id, format!("{what}: {why}"));
                } else {
                    launch.retry_at_us = Some(now + RETRY_US);
                }
            }
        }
        self.drive_launch(&link_id);
    }

    fn next_step(&mut self, link_id: &str) {
        let now = self.clock.now_us();
        let Some(launch) = self.launches.running.get_mut(link_id) else {
            return;
        };
        launch.step += 1;
        while launch.handed_over
            && launch
                .steps
                .get(launch.step)
                .is_some_and(|step| changes_mode(*step))
        {
            launch.step += 1;
        }
        launch.since_us = now;
        launch.tries = 0;
        launch.retry_at_us = None;
        if launch.step >= launch.steps.len() {
            let in_air = launch.in_air;
            self.launches.running.remove(link_id);
            if in_air {
                self.show_launch(link_id, LaunchStage::Flying, None);
            } else {
                self.clear_launch(link_id);
            }
        }
    }

    /// Something was heard from a vehicle: move its take-off on, if it has
    /// one.
    /// A simulated rover or boat starts ready, in HOLD (S37): `ArduRover`
    /// itself starts in MANUAL. Sent once, when the vehicle is first
    /// identified, unless it starts from a snapshot, which brings its own
    /// mode. PX4 starts holding already.
    pub(crate) fn hold_when_identified(&mut self, before: Option<&Vehicle>, after: &Vehicle) {
        let identified =
            after.uid.is_some() && before.is_none_or(|b| b.uid.is_none()) && !is_px4(after);
        let ready_in_hold = self.state.links.get(&after.link_id).is_some_and(|link| {
            matches!(
                link.kind,
                LinkKind::Simulator {
                    vehicle: SimVehicle::Rover | SimVehicle::Boat,
                    snapshot_id: None,
                    ..
                }
            )
        });
        if identified && ready_in_hold {
            self.send_as_simulation_start(
                after,
                Command::SetMode {
                    mode: "HOLD".into(),
                },
            );
        }
    }

    /// Moves every take-off on, once a second: its deadlines pass also when
    /// the simulator says nothing, so one that never speaks (`jsbsim`
    /// without `JSBSim`) fails as never connected instead of climbing for
    /// ever.
    pub fn drive_launches(&mut self) {
        let links: Vec<String> = self.launches.running.keys().cloned().collect();
        for link_id in links {
            self.drive_launch(&link_id);
        }
    }

    pub(crate) fn drive_launch_of_vehicle(&mut self, vehicle_id: &str) {
        let Some(link_id) = self.vehicle(vehicle_id).map(|v| v.link_id.clone()) else {
            return;
        };
        if self.launches.running.contains_key(&link_id) {
            self.drive_launch(&link_id);
        }
    }

    /// Takes the next action of a link's take-off, or checks the condition
    /// it waits for. Loops while steps complete at once.
    fn drive_launch(&mut self, link_id: &str) {
        loop {
            let now = self.clock.now_us();
            let Some(launch) = self.launches.running.get(link_id) else {
                return;
            };
            if launch.pending.is_some() || launch.retry_at_us.is_some_and(|at| now < at) {
                return;
            }
            let (kind, height_m, speedup, since_us) = (
                launch.vehicle,
                launch.height_m,
                launch.speedup,
                launch.since_us,
            );
            let step = launch.steps[launch.step];
            let waited = now.saturating_sub(since_us);
            let Some(vehicle) = self.launch_vehicle(link_id) else {
                if waited > HEARD_US {
                    self.fail_launch(link_id, "the simulated vehicle never connected".into());
                }
                return;
            };
            let at = At {
                kind,
                started_us: launch.started_us,
                height_m,
                speedup,
                waited,
            };
            if !self.launch_act(link_id, step, &vehicle, &at) {
                return;
            }
        }
    }

    /// Once the vehicle is heard: PX4 takes its own steps, also when it was
    /// started from a build of the user's own without saying it is PX4.
    fn plan_for(&mut self, link_id: &str, vehicle: &Vehicle) {
        if is_px4(vehicle)
            && let Some(launch) = self.launches.running.get_mut(link_id)
        {
            launch.steps = plan_px4(launch.vehicle, launch.in_air, launch.snapshot.as_ref());
        }
    }

    /// Carries out one step. True when it is already done, so the next can
    /// start at once.
    fn launch_act(&mut self, link_id: &str, step: Step, vehicle: &Vehicle, at: &At) -> bool {
        let (kind, height_m, speedup, waited) = (at.kind, at.height_m, at.speedup, at.waited);
        match step {
            Step::Heard => {
                self.plan_for(link_id, vehicle);
                self.next_step(link_id);
                true
            }
            Step::SpeedUp => {
                self.launch_parameter(
                    link_id,
                    vehicle,
                    "SIM_SPEEDUP",
                    TAKE_OFF_SPEEDUP,
                    ParamKind::Real32,
                );
                false
            }
            Step::Parameters => self.restore_parameters(link_id, vehicle, waited),
            Step::Mission => self.restore_mission(link_id, vehicle, waited),
            Step::MissionItem | Step::Mode => {
                self.restore_mode(link_id, vehicle, step, kind);
                false
            }
            Step::Ready => self.wait_until_ready(link_id, vehicle, at.started_us),
            Step::TakeOffHeight => {
                self.launch_parameter(
                    link_id,
                    vehicle,
                    "TKOFF_ALT",
                    height_m.round(),
                    ParamKind::Int16,
                );
                false
            }
            Step::TakeOffMode => {
                let mode = take_off_mode(kind).into();
                self.launch_command(link_id, vehicle, Command::SetMode { mode });
                false
            }
            Step::Arm => {
                self.launch_arm(link_id, vehicle);
                false
            }
            Step::Armed => {
                if vehicle.armed && vehicle.last_heartbeat_us >= at.started_us {
                    self.next_step(link_id);
                    true
                } else {
                    if waited >= READY_US {
                        self.fail_launch(
                            link_id,
                            "arming was acknowledged but no armed heartbeat arrived".into(),
                        );
                    }
                    false
                }
            }
            Step::TakeOff => {
                #[allow(clippy::cast_possible_truncation, reason = "a height in metres")]
                let height = height_m as f32;
                // MAV_CMD_NAV_TAKEOFF: param 7 is the height above home.
                let params = vec![0.0, 0.0, 0.0, 0.0, 0.0, 0.0, height];
                self.launch_command(
                    link_id,
                    vehicle,
                    Command::Long {
                        command: 22,
                        params,
                    },
                );
                false
            }
            Step::LiftOff => self.climbed_to(link_id, vehicle, at, LIFT_OFF_M.min(0.9 * height_m)),
            Step::Climb => self.climbed_to(link_id, vehicle, at, 0.9 * height_m),
            Step::Hold => {
                let mode = hold_mode_of(vehicle, kind).into();
                self.launch_command(link_id, vehicle, Command::SetMode { mode });
                false
            }
            Step::Height => {
                #[allow(clippy::cast_possible_truncation, reason = "a height in metres")]
                // param 1 the height, param 2 its frame: 3 is above home.
                let params = vec![height_m as f32, 3.0];
                let command = Command::Long {
                    command: 186,
                    params,
                };
                self.launch_command(link_id, vehicle, command);
                false
            }
            Step::SlowDown => {
                self.launch_parameter(
                    link_id,
                    vehicle,
                    "SIM_SPEEDUP",
                    f64::from(speedup),
                    ParamKind::Real32,
                );
                false
            }
        }
    }

    /// Whether it is this high, moving on if so. True when it is.
    fn climbed_to(&mut self, link_id: &str, vehicle: &Vehicle, at: &At, to_m: f64) -> bool {
        let reached = self
            .height_above_home_since(&vehicle.id, at.started_us)
            .is_some_and(|h| h >= to_m);
        if reached {
            self.next_step(link_id);
            return true;
        }
        if at.waited > CLIMB_US {
            let height_m = at.height_m;
            self.fail_launch(link_id, format!("it did not climb to {height_m:.0} m"));
        }
        false
    }

    fn wait_until_ready(&mut self, link_id: &str, vehicle: &Vehicle, started_us: u64) -> bool {
        let now = self.clock.now_us();
        let Some(launch) = self.launches.running.get_mut(link_id) else {
            return false;
        };
        let since = *launch.ready_since_us.get_or_insert(now);
        if now.saturating_sub(since) >= READY_US {
            let why = launch.refusal.as_ref().map_or_else(
                || {
                    if launch.vehicle == SimVehicle::Submarine {
                        "position or pre-arm checks did not become ready (Messages says why)".into()
                    } else {
                        "it failed its pre-arm checks (Messages says why)".into()
                    }
                },
                |why| format!("it would not arm: {why}"),
            );
            self.fail_launch(link_id, why);
            return false;
        }
        let position_ready = launch.vehicle != SimVehicle::Submarine
            || self.telemetry(&vehicle.id).is_some_and(|telemetry| {
                telemetry.items.iter().any(|item| {
                    item.key == "ekf.status"
                        && item.text.as_deref() == Some("Good")
                        && item.time_us >= started_us
                        && now.saturating_sub(item.time_us) <= 5_000_000
                }) && telemetry.positions.iter().any(|position| {
                    position.source == "fused"
                        && position.time_us >= started_us
                        && now.saturating_sub(position.time_us) <= 5_000_000
                        && position.latitude_deg.is_finite()
                        && position.longitude_deg.is_finite()
                        && (position.latitude_deg != 0.0 || position.longitude_deg != 0.0)
                })
            });
        if position_ready && self.ready_to_arm(&vehicle.id, started_us) {
            self.next_step(link_id);
            return true;
        }
        false
    }

    /// Writes back the next of a snapshot's parameters that differs from
    /// the new vehicle's, once those have downloaded. True when done.
    fn restore_parameters(&mut self, link_id: &str, vehicle: &Vehicle, waited: u64) -> bool {
        let Some(launch) = self.launches.running.get(link_id) else {
            return false;
        };
        if launch.queue.is_none() {
            let set = self.parameter_set(&vehicle.id);
            let fresh = set.status == TransferStatus::Synced
                && set.synced_at_us.is_some_and(|at| at >= launch.started_us);
            if !fresh {
                // Without its own list there is nothing to compare with:
                // the vehicle keeps its defaults.
                if waited > READY_US {
                    self.next_step(link_id);
                    return true;
                }
                return false;
            }
            let queue: Vec<SnapshotParameter> = launch
                .snapshot
                .iter()
                .flat_map(|s| &s.parameters)
                .filter(|p| crate::snapshots::restorable(&p.name))
                .filter(|p| {
                    self.state
                        .parameters
                        .get(&parameter_id(&vehicle.id, &p.name))
                        .is_some_and(|now| !same_value(now.value, p.value))
                })
                .cloned()
                .collect();
            if let Some(launch) = self.launches.running.get_mut(link_id) {
                launch.queue = Some(queue);
            }
        }
        let next = self
            .launches
            .running
            .get(link_id)
            .and_then(|l| l.queue.as_ref())
            .and_then(|q| q.first())
            .cloned();
        if let Some(parameter) = next {
            self.launch_parameter(
                link_id,
                vehicle,
                &parameter.name,
                parameter.value,
                parameter.kind,
            );
            false
        } else {
            self.next_step(link_id);
            true
        }
    }

    /// A snapshot's mission item (`MAV_CMD_DO_SET_MISSION_CURRENT`) or its
    /// mode, after the take-off.
    fn restore_mode(&mut self, link_id: &str, vehicle: &Vehicle, step: Step, kind: SimVehicle) {
        let snapshot = self
            .launches
            .running
            .get(link_id)
            .and_then(|l| l.snapshot.as_ref());
        let command = if step == Step::MissionItem {
            let seq = snapshot.and_then(|s| s.current_item).unwrap_or(1);
            Command::Long {
                command: 224,
                params: vec![f32::from(seq)],
            }
        } else {
            let mode = snapshot
                .and_then(|s| s.mode.clone())
                .unwrap_or_else(|| hold_mode(kind).into());
            Command::SetMode { mode }
        };
        self.launch_command(link_id, vehicle, command);
    }

    /// Uploads a snapshot's mission once the new vehicle's own has
    /// downloaded, so the two do not cross. True when skipped.
    fn restore_mission(&mut self, link_id: &str, vehicle: &Vehicle, waited: u64) -> bool {
        let Some(launch) = self.launches.running.get(link_id) else {
            return false;
        };
        let mission = self.mission(&vehicle.id);
        let fresh = mission.status == TransferStatus::Synced
            && mission
                .synced_at_us
                .is_some_and(|at| at >= launch.started_us);
        if !fresh {
            if waited > READY_US {
                self.next_step(link_id);
                return true;
            }
            return false;
        }
        let items = launch
            .snapshot
            .as_ref()
            .map(|s| s.mission.clone())
            .unwrap_or_default();
        let id = self.upload_mission_as(actor(), &vehicle.id, items);
        self.await_launch_order(link_id, id);
        false
    }

    /// The online vehicle on a link.
    fn launch_vehicle(&self, link_id: &str) -> Option<Vehicle> {
        self.state
            .vehicles
            .values()
            .find(|v| v.link_id == link_id && v.presence == Presence::Online)
            .cloned()
    }

    /// `SYS_STATUS`'s pre-arm check bit, as the `arming.ready` item.
    /// Its pre-arm checks pass in this simulator run. An autopilot that
    /// does not report them, as PX4 does not in `SYS_STATUS`, waits for its
    /// new position above home; a refused arm is tried again.
    fn ready_to_arm(&self, vehicle_id: &str, started_us: u64) -> bool {
        let Some(telemetry) = self.telemetry(vehicle_id) else {
            return false;
        };
        match telemetry.items.iter().find(|i| i.key == "arming.ready") {
            Some(item) => item.time_us >= started_us && item.value == Some(1.0),
            None => self
                .height_above_home_since(vehicle_id, started_us)
                .is_some(),
        }
    }

    /// The autopilot's own height above home.
    pub(crate) fn height_above_home(&self, vehicle_id: &str) -> Option<f64> {
        self.height_above_home_since(vehicle_id, 0)
    }

    fn height_above_home_since(&self, vehicle_id: &str, since_us: u64) -> Option<f64> {
        self.telemetry(vehicle_id)?
            .positions
            .iter()
            .filter(|p| p.source == "fused" && p.time_us >= since_us)
            .flat_map(|p| &p.altitudes)
            .find(|a| a.frame == AltitudeFrame::AboveHome)
            .map(|a| a.meters)
    }

    /// Sends a command as the simulation start: journaled with a receipt
    /// like any other, attributed to `simulation-start`.
    /// UXDuck's own take-off arms too: never past a blocking preflight
    /// check (S192, ADR-0038 K3).
    fn launch_arm(&mut self, link_id: &str, vehicle: &Vehicle) {
        let gating = self.preflight_gating(&vehicle.id);
        if gating.is_empty() {
            self.launch_command(link_id, vehicle, Command::Arm);
        } else {
            let why = crate::preflight::refusal("Arm", &vehicle.name, &gating, false);
            self.fail_launch(link_id, why);
        }
    }

    fn launch_command(&mut self, link_id: &str, vehicle: &Vehicle, command: Command) {
        let id = self.send_as_simulation_start(vehicle, command);
        self.await_launch_order(link_id, id);
    }

    fn launch_parameter(
        &mut self,
        link_id: &str,
        vehicle: &Vehicle,
        name: &str,
        value: f64,
        kind: ParamKind,
    ) {
        let id = self.write_parameter_as(actor(), &vehicle.id, name, value, kind);
        self.await_launch_order(link_id, id);
    }

    fn await_launch_order(&mut self, link_id: &str, id: String) {
        self.launches.orders.insert(id.clone(), link_id.to_owned());
        if let Some(launch) = self.launches.running.get_mut(link_id) {
            launch.pending = Some(id);
            launch.retry_at_us = None;
        }
    }

    fn send_as_simulation_start(&mut self, vehicle: &Vehicle, command: Command) -> String {
        let receipt = Receipt {
            id: format!("rc{}", &self.random.token()[..16]),
            vehicle_id: vehicle.id.clone(),
            command: command.clone(),
            status: ReceiptStatus::Sent,
            detail: None,
            reason: None,
            preflight: Vec::new(),
            preflight_override: None,
            sent_without_recent_heartbeat: false,
            forced: false,
            attempts: 0,
            client_id: ACTOR.into(),
            operator_id: None,
            remote: None,
            created_at_us: self.clock.now_us(),
            completed_at_us: None,
            version: 1,
        };
        let id = receipt.id.clone();
        self.commit_as(actor(), Change::ReceiptUpserted { receipt });
        self.links.command(CommandOrder {
            receipt_id: id.clone(),
            vehicle_id: vehicle.id.clone(),
            command,
            force: false,
        });
        id
    }

    /// The take-off failed: say why, and leave the vehicle safe. On the
    /// ground it is disarmed; in the air it holds where it got to. The
    /// simulator goes back to the asked-for speed.
    fn fail_launch(&mut self, link_id: &str, why: String) {
        let Some(launch) = self.launches.running.remove(link_id) else {
            return;
        };
        if let Some(vehicle) = self.launch_vehicle(link_id) {
            // Someone already flying it keeps the mode they chose.
            if vehicle.armed && !launch.handed_over {
                let low = self
                    .height_above_home_since(&vehicle.id, launch.started_us)
                    .is_none_or(|h| h < ON_THE_GROUND_M);
                let command = if low {
                    Command::Disarm
                } else {
                    Command::SetMode {
                        mode: hold_mode_of(&vehicle, launch.vehicle).into(),
                    }
                };
                let id = self.send_as_simulation_start(&vehicle, command);
                self.launches.orders.insert(id, link_id.to_owned());
            }
            // Only a build that was sped up is slowed down: not PX4.
            if launch.steps.contains(&Step::SpeedUp)
                && launch.steps.get(launch.step) != Some(&Step::SpeedUp)
                && launch.step > 1
            {
                let id = self.write_parameter_as(
                    actor(),
                    &vehicle.id,
                    "SIM_SPEEDUP",
                    f64::from(launch.speedup),
                    ParamKind::Real32,
                );
                self.launches.orders.insert(id, link_id.to_owned());
            }
        }
        let detail = match &launch.refusal {
            Some(refusal) if !why.contains(refusal.as_str()) => format!("{why} (last: {refusal})"),
            _ => why,
        };
        let name = self
            .state
            .links
            .get(link_id)
            .map_or_else(|| "The simulated vehicle".to_owned(), |l| l.name.clone());
        self.show_launch(link_id, LaunchStage::Failed, Some(detail.clone()));
        self.raise(Raise {
            severity: Severity::Warning,
            kind: NotificationKind::Transient,
            subject: Subject::Link {
                id: link_id.to_owned(),
            },
            title: if launch.vehicle == SimVehicle::Submarine {
                format!("{name} failed to start")
            } else {
                format!("{name} stayed on the ground")
            },
            body: Some(detail),
            actions: Vec::new(),
            // One per simulation, however often it is started again.
            condition: Some(format!("launch:{link_id}")),
            report: Some("launch-failed".into()),
        });
    }

    fn clear_launch(&mut self, link_id: &str) {
        let Some(mut link) = self.state.links.get(link_id).cloned() else {
            return;
        };
        if link.launch.take().is_some() {
            link.version += 1;
            self.commit_as(actor(), Change::LinkUpserted { link });
        }
    }

    fn show_launch(&mut self, link_id: &str, stage: LaunchStage, detail: Option<String>) {
        let Some(mut link) = self.state.links.get(link_id).cloned() else {
            return;
        };
        let launch = Some(LaunchState { stage, detail });
        if link.launch == launch {
            return;
        }
        link.launch = launch;
        link.version += 1;
        self.commit_as(actor(), Change::LinkUpserted { link });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn px4_takes_off_without_a_speed_up_and_holds_by_itself() {
        use Step::{Arm, Climb, Heard, Ready, TakeOff};
        assert_eq!(
            plan_px4(SimVehicle::Multicopter, true, None),
            [Heard, Ready, Arm, TakeOff, Climb]
        );
        assert_eq!(
            plan_px4(SimVehicle::Plane, true, None),
            [Heard, Ready, Arm, TakeOff, Climb]
        );
        assert_eq!(plan_px4(SimVehicle::Rover, true, None), [Heard]);
        assert_eq!(plan_px4(SimVehicle::Multicopter, false, None), [Heard]);
        // ArduPilot's own still speeds up, takes off in GUIDED and holds.
        assert_eq!(
            plan(SimVehicle::Multicopter, true, None),
            [
                Heard,
                Step::SpeedUp,
                Ready,
                Step::TakeOffMode,
                Arm,
                TakeOff,
                Climb,
                Step::Hold,
                Step::SlowDown
            ]
        );
    }

    /// A jet (S143) takes off and holds as a plane does: `ArduPlane`'s
    /// TAKEOFF mode to its height, then LOITER.
    #[test]
    fn a_jet_takes_off_as_a_plane() {
        assert_eq!(
            plan(SimVehicle::Jet, true, None),
            plan(SimVehicle::Plane, true, None)
        );
        assert_eq!(take_off_mode(SimVehicle::Jet), "TAKEOFF");
        assert_eq!(hold_mode(SimVehicle::Jet), "LOITER");
        assert_eq!(plan_px4(SimVehicle::Jet, true, None).len(), 5);
    }

    /// A plane holds once clear of the ground and climbs on in LOITER, so
    /// TAKEOFF's full-throttle climb never overshoots (S143, 2026-10-19).
    /// Someone taking over skips the hold, the height and the climb alike.
    #[test]
    fn a_plane_holds_once_off_the_ground_and_climbs_on_to_its_height() {
        use Step::{Arm, Climb, Height, Hold, LiftOff, TakeOffHeight, TakeOffMode};
        assert_eq!(
            plan(SimVehicle::Plane, true, None),
            [
                Step::Heard,
                Step::SpeedUp,
                Step::Ready,
                TakeOffHeight,
                TakeOffMode,
                Arm,
                LiftOff,
                Hold,
                Height,
                Climb,
                Step::SlowDown
            ]
        );
        assert!([LiftOff, Hold, Height, Climb].into_iter().all(changes_mode));
    }

    /// No simulated vehicle is left in a mode that climbs or sinks with the
    /// throttle stick, which rests at the bottom in a simulator: a VTOL
    /// handed over in QLOITER and a multicopter in LOITER sank to the
    /// ground (maintainer 2026-10-11).
    #[test]
    fn a_take_off_never_leaves_a_vehicle_in_a_throttle_mode() {
        for vehicle in [SimVehicle::Multicopter, SimVehicle::Vtol] {
            assert_eq!(hold_mode(vehicle), "GUIDED");
            assert_eq!(
                plan(vehicle, true, None).last_chunk(),
                Some(&[Step::Hold, Step::SlowDown])
            );
        }
        for vehicle in [SimVehicle::Multicopter, SimVehicle::Vtol, SimVehicle::Plane] {
            assert!(
                !follows_throttle(vehicle, hold_mode(vehicle)),
                "{vehicle:?}"
            );
        }

        // A snapshot saved in one holds in GUIDED instead; RTL comes back.
        let saved = |vehicle, mode: &str| Snapshot {
            id: "s".into(),
            name: "s".into(),
            vehicle,
            autopilot: SimAutopilot::default(),
            frame: None,
            firmware: None,
            latitude_deg: 0.0,
            longitude_deg: 0.0,
            ground_altitude_m: 0.0,
            height_m: 50.0,
            heading_deg: 0.0,
            ground_speed_mps: 0.0,
            mode: Some(mode.into()),
            armed: true,
            parameters: Vec::new(),
            mission: Vec::new(),
            current_item: None,
            operator_id: None,
            created_at_us: 0,
            version: 1,
        };
        let puts_back =
            |vehicle, mode| plan(vehicle, true, Some(&saved(vehicle, mode))).contains(&Step::Mode);
        for mode in ["LOITER", "ALT_HOLD", "POSHOLD"] {
            assert!(!puts_back(SimVehicle::Multicopter, mode), "{mode}");
        }
        for mode in ["QLOITER", "QHOVER"] {
            assert!(!puts_back(SimVehicle::Vtol, mode), "{mode}");
        }
        assert!(puts_back(SimVehicle::Multicopter, "RTL"));
        assert!(puts_back(SimVehicle::Vtol, "QRTL"));
    }
}
