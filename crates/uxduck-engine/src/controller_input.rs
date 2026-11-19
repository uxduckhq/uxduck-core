//! Controller input (ADR-0005): each controller's latest values, whether
//! its sticks are centred, which button went down and which position an
//! axis moved into, and the pilot input for the routes the engine grants.
//! The installed daemon feeds it from gilrs, the browser's daemon from the
//! page's Gamepad API; both turn [`Report`]s into engine calls and send
//! [`Controls::pilot_outputs`] at 25 Hz. A routed controller's gimbal
//! inputs (S407) go the same way: [`Controls::gimbal_outputs`] on every
//! pilot tick, and [`Controls::gimbal_stops`] after every input and route.

use std::collections::BTreeMap;

use uxduck_contracts::dto::controller::{
    AxisValue, ButtonState, ControllerInputs, ControllerInputsResult, InputKind,
};
use uxduck_core::domain::command::Command;
use uxduck_core::domain::controller::ControllerPower;
use uxduck_core::domain::controller::NAVIGATE_BUTTONS;
use uxduck_core::domain::controller::{
    ActionBinding, AxisBinding, ButtonBinding, ButtonGesture, DOUBLE_PRESS_MS, GamepadAxis,
    GamepadButton, GimbalAxis, InputMapping, PositionBinding, ShiftButton, Sticks,
};
use uxduck_core::domain::gripper::GripperAction;
use uxduck_core::domain::settings::PilotInputMethod;

use crate::runtime::{ControlRoute, GimbalRoute};

/// How fast a gimbal turns at a full push of an axis or with a button
/// held, in radians a second: 30 degrees a second (S407), so a camera
/// looking ahead looks straight down in three seconds and a short tap
/// moves it a few degrees.
pub const GIMBAL_FULL_RATE_RAD_S: f64 = 0.523_598_775_598_298_8;

/// The keyboard's gimbal keys as its device's inputs (S407): its pitch
/// and its yaw, each -1, 0 or 1.
pub const KEYBOARD_GIMBAL_AXES: [GamepadAxis; 2] = [GamepadAxis::RightZ, GamepadAxis::LeftZ];

/// What an input device did. `code` is the platform's own code for the
/// input (absent for the fake); `axis` and `button` are the gamepad input
/// the platform reads it as, if any, a guess when the layout is not known.
#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
    /// Complete keyboard values. Missing updates centre steering without releasing control.
    Keyboard {
        id: String,
        axes: [f64; 4],
        at_us: u64,
    },
    Connected {
        id: String,
        name: String,
        layout_known: bool,
    },
    Disconnected {
        id: String,
    },
    /// A second unit of the model connected under `id` was plugged in, or
    /// the last such unit unplugged. On Linux and Windows units of one model
    /// share an id, so the daemon ignores the second unit's input.
    SecondUnit {
        id: String,
        name: String,
        connected: bool,
    },
    Axis {
        id: String,
        axis: Option<GamepadAxis>,
        code: Option<u32>,
        value: f64,
    },
    Button {
        id: String,
        button: Option<GamepadButton>,
        code: Option<u32>,
        pressed: bool,
    },
    /// Every input the device has and its value now, where the platform can
    /// list them (macOS, and the browser's Gamepad API): so all show, and
    /// drive, before they move.
    Present {
        id: String,
        axes: Vec<(u32, Option<GamepadAxis>, f64)>,
        buttons: Vec<(u32, Option<GamepadButton>, bool)>,
    },
    /// The page's on-screen gripper Open or Close, held or let go (S263),
    /// for the controller `id` its connection drives the vehicle with. A
    /// hold lasts `SCREEN_HOLD_US` after `at_us` unless refreshed.
    Gripper {
        id: String,
        action: Option<GripperAction>,
        at_us: u64,
    },
    /// The window that claimed controller `id` starts, refreshes or ends
    /// Navigate (S420): while on, `NAVIGATE_BUTTONS` send nothing to its
    /// vehicle. It ends `NAVIGATE_LEASE_US` after `at_us` unless refreshed.
    Navigate {
        id: String,
        on: bool,
        at_us: u64,
    },
    /// A controller's power as the platform reports it now (S419): when it
    /// connects and whenever it changes.
    Power {
        id: String,
        power: ControllerPower,
    },
}

/// How long Navigate lasts without a refresh: the window refreshes it well
/// inside this, so a window that hangs or goes gives the direction pad,
/// South and East back to the vehicle within this and one pilot tick.
pub const NAVIGATE_LEASE_US: u64 = 2_000_000;

/// How long an on-screen gripper hold lasts without a refresh: the page
/// refreshes it well inside this, so a page that disappears stops the
/// gripper within this and one pilot tick.
pub const SCREEN_HOLD_US: u64 = 500_000;

/// What the engine must hear about.
#[derive(Clone, Debug, PartialEq)]
pub enum Report {
    Connected {
        id: String,
        name: String,
        layout_known: bool,
    },
    Disconnected {
        id: String,
    },
    /// See [`InputEvent::SecondUnit`].
    SecondUnit {
        id: String,
        name: String,
        connected: bool,
    },
    Neutral {
        id: String,
        neutral: bool,
    },
    /// A button went down: by its gamepad name, or by its code on a device
    /// whose layout is not known.
    Pressed {
        id: String,
        button: Option<GamepadButton>,
        code: Option<u32>,
    },
    /// An axis moved into a position of the routed mapping and stayed
    /// there `SETTLE_MS`, or its hold.
    Positioned {
        id: String,
        position: PositionBinding,
    },
    /// A button or position bound with a hold went down: `command` goes
    /// as `Pressed` or `Positioned` once it has been held `hold_ms`.
    Holding {
        id: String,
        command: Command,
        hold_ms: u32,
    },
    /// The hold under way ended: let go early, or complete.
    LetGo {
        id: String,
    },
    /// A routed controller's gimbal inputs began to ask it to turn (S407):
    /// an axis pushed from its centre, or a button pressed. The engine
    /// says whether it may (`Controls::steer_gimbal`), and nothing is sent
    /// until it does.
    GimbalAsked {
        id: String,
    },
    /// Its gimbal inputs ask for nothing any more: all centred and let go,
    /// or its route ended or changed.
    GimbalLetGo {
        id: String,
    },
    /// What a routed controller's camera inputs ask of its vehicle's
    /// camera now (S416): zoom then focus, each -1 to 1 with zooming in
    /// and focusing farther positive, and 0 to stop. Said when it changes,
    /// with zeros when the route ends or the controller goes, and again
    /// every [`CAMERA_REPEAT_MS`] while either is not 0, for a camera
    /// that zooms only by steps.
    CameraDrive {
        id: String,
        drive: [f32; 2],
    },
    /// A gesture of a button's went (S418): a long press, a double press,
    /// a press decided once neither came, a binding on a Shift's layer or
    /// a repeat. `binding` goes, if the controller's mapping still has
    /// it. A button's plain press (`InputMapping::button`) goes as
    /// `Pressed`, as before.
    Gesture {
        id: String,
        binding: ButtonBinding,
    },
    /// A connected controller's power changed (S419).
    Power {
        id: String,
        power: ControllerPower,
    },
}

/// How often a held zoom or focus is said again (S416), so a camera that
/// zooms only by steps takes four a second while it is held.
pub const CAMERA_REPEAT_MS: u64 = 250;

/// What turning gimbals asks of the data plane on a pilot tick.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GimbalOutputs {
    /// For the engine: who began to ask, and who let go.
    pub reports: Vec<Report>,
    /// The rates to send now, pitch then yaw in radians a second, up and
    /// right positive, one for each gimbal a controller turns.
    pub rates: Vec<(GimbalRoute, [f32; 2])>,
}

/// What is being held for its command.
#[derive(Clone, Debug, PartialEq)]
enum Held {
    Button {
        button: Option<GamepadButton>,
        code: Option<u32>,
        binding: ButtonBinding,
    },
    Position(PositionBinding),
}

impl Held {
    /// What goes once it has been held, through `mapping`.
    fn report(self, id: &str, mapping: &InputMapping) -> Report {
        match self {
            Self::Button {
                button,
                code,
                binding,
            } => fired(id, mapping, (code, button), binding),
            Self::Position(position) => Report::Positioned {
                id: id.to_owned(),
                position,
            },
        }
    }
}

/// A button's binding going: as a press of the button when it is its
/// plain press, which the engine finds by the button, else as itself.
fn fired(
    id: &str,
    mapping: &InputMapping,
    (code, button): (Option<u32>, Option<GamepadButton>),
    binding: ButtonBinding,
) -> Report {
    let id = id.to_owned();
    if mapping.button(button, code) == Some(&binding) {
        Report::Pressed { id, button, code }
    } else {
        Report::Gesture { id, binding }
    }
}

/// Which Shift's layer a press is on (S418): none held, one, or more
/// than one, when it does nothing at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Layer {
    #[default]
    Base,
    Shift(ShiftButton),
    Mixed,
}

impl Layer {
    /// The Shift its bindings name: none on the base layer, and with two
    /// Shifts held at once, which has no bindings (see `is_mixed`).
    const fn shift(self) -> Option<ShiftButton> {
        match self {
            Self::Shift(shift) => Some(shift),
            Self::Base | Self::Mixed => None,
        }
    }

    /// Two Shifts held at once: no layer, so nothing is bound on it.
    const fn is_mixed(self) -> bool {
        matches!(self, Self::Mixed)
    }
}

/// The command bindings of one input on one layer, by gesture (S418).
#[derive(Default)]
struct Gestures<'a> {
    press: Option<&'a ButtonBinding>,
    long: Option<&'a ButtonBinding>,
    double: Option<&'a ButtonBinding>,
}

impl<'a> Gestures<'a> {
    fn of(
        mapping: &'a InputMapping,
        (code, button): (Option<u32>, Option<GamepadButton>),
        shift: Option<ShiftButton>,
    ) -> Self {
        let mut gestures = Self::default();
        for binding in mapping
            .buttons
            .iter()
            .filter(|b| b.matches(button, code) && b.shift == shift)
        {
            let slot = match (binding.gesture, binding.hold_ms) {
                (ButtonGesture::DoublePress, _) => &mut gestures.double,
                (ButtonGesture::Press, Some(_)) => &mut gestures.long,
                (ButtonGesture::Press, None) => &mut gestures.press,
            };
            slot.get_or_insert(binding);
        }
        gestures
    }
}

/// A press of an input with more than one gesture bound (S418), until it
/// is decided.
#[derive(Debug)]
struct Gesturing {
    key: (Option<u32>, Option<GamepadButton>),
    /// The layer of its first press, kept if the Shift is let go.
    shift: Option<ShiftButton>,
    stage: Stage,
}

#[derive(Debug, PartialEq)]
enum Stage {
    /// Down: a long press may still complete.
    Down,
    /// Let go after a first press: a second within `DOUBLE_PRESS_MS` is a
    /// double press, timed from the first tick after.
    Up { since_us: Option<u64> },
    /// A long or a double press went: nothing more until let go.
    Done,
}

/// A step its button repeats while held (S418).
#[derive(Debug)]
struct Repeat {
    key: (Option<u32>, Option<GamepadButton>),
    binding: ButtonBinding,
    /// The first tick after the press.
    since_us: Option<u64>,
    /// How many times it went again since.
    sent: u64,
}

/// A hold under way: since the first tick after it began, as input
/// events carry no time.
#[derive(Debug)]
struct Pending {
    held: Held,
    since_us: Option<u64>,
    hold_ms: u32,
}

/// Takes those of `pending` held long enough by `now_us`, starting the
/// time of those just begun.
fn due(pending: &mut Vec<Pending>, now_us: u64) -> Vec<Held> {
    let mut done = Vec::new();
    pending.retain_mut(|pending| {
        let since = *pending.since_us.get_or_insert(now_us);
        let complete = now_us.saturating_sub(since) >= u64::from(pending.hold_ms) * 1_000;
        if complete {
            done.push(pending.held.clone());
        }
        !complete
    });
    done
}

/// How long an axis must stay in a position before its command goes
/// (FB-67), so a switch flicked through its middle sends only where it
/// stops. A flick passes a three-way switch's middle in tens of
/// milliseconds; ArduPilot's own mode switch waits 200 ms
/// (`SWITCH_DEBOUNCE_TIME_MS`), so a deliberate move goes about as soon as
/// one flown by radio would. A position bound with a hold already waits.
pub const SETTLE_MS: u32 = 200;

/// How far apart two readings of an axis must be to be different places:
/// more than a resting stick's or a switch's jitter.
const LEVEL: f64 = 0.06;

/// The smallest step between a switch's places. A stick moved from one
/// place to another reads between them; a switch jumps.
const SWITCH_STEP: f64 = 0.25;

/// A button as last reported.
#[derive(Clone, Copy, Debug, Default)]
struct Button {
    held: bool,
    /// Pressed since it was plugged in.
    seen: bool,
    /// How many times it has gone down.
    presses: u32,
    /// The Shift layer it was last pressed on (S418), which its held
    /// action keeps until it is let go.
    layer: Layer,
}

#[derive(Debug, Default)]
struct Device {
    layout_known: bool,
    keyboard_at_us: Option<u64>,
    /// Every axis it has or that has moved, by its code and its gamepad
    /// axis (only for a device whose layout is known: never a guess).
    axes: BTreeMap<(Option<u32>, Option<GamepadAxis>), f64>,
    /// Every button it has or that has been pressed.
    buttons: BTreeMap<(Option<u32>, Option<GamepadButton>), Button>,
    neutral: Option<bool>,
    route: Option<ControlRoute>,
    /// Whether the axis is in each position of the route's mapping, as last
    /// judged: a command goes only on moving into one.
    held: Vec<bool>,
    /// The places each axis has been seen at, by the same key as `axes`,
    /// to tell a switch from a stick (`AxisValue::kind`).
    levels: BTreeMap<(Option<u32>, Option<GamepadAxis>), Levels>,
    /// Holds under way, for bindings with `hold_ms`.
    holds: Vec<Pending>,
    /// Positions moved into, until they have settled (`SETTLE_MS`); one
    /// left before then, as a switch's middle flicked through, sends
    /// nothing.
    settling: Vec<Pending>,
    /// Gripper buttons held when the route began, which act only once let
    /// go and pressed again (S263).
    stale_actions: Vec<(Option<u32>, Option<GamepadButton>)>,
    /// The on-screen gripper hold and when it was last refreshed.
    screen: Option<(GripperAction, u64)>,
    /// The on-screen hold must be let go before it acts again: after a new
    /// route or an expiry.
    screen_stale: bool,
    /// Open and close were asked for together: nothing moves until every
    /// gripper input is let go.
    opposed: bool,
    /// The gripper action asked for now, after the rules above.
    gripper: Option<GripperAction>,
    /// What its gimbal inputs are doing (S407).
    turn: Turn,
    /// What its camera inputs are doing (S416).
    zoom: Zoom,
    /// When the window navigating with it last refreshed Navigate (S420).
    navigating: Option<u64>,
    /// Presses not yet decided between a press, a long press and a
    /// double press (S418).
    gestures: Vec<Gesturing>,
    /// Steps repeated while their buttons are held (S418).
    repeats: Vec<Repeat>,
}

/// A device's camera inputs (S416): what the engine was last told and
/// when.
#[derive(Debug, Default)]
struct Zoom {
    /// The route's camera axes that were off their centre when the route
    /// began, as `Turn::stale`.
    stale: Vec<bool>,
    told: [f32; 2],
    told_us: u64,
}

/// A device's gimbal inputs: what they ask for and the gimbal they turn.
#[derive(Debug, Default)]
struct Turn {
    /// The route's gimbal axes that were off their centre when the route
    /// began, in the mapping's order: each turns nothing until it has
    /// been at its centre once.
    stale: Vec<bool>,
    /// Whether the inputs ask for a turn now, as the engine was last told.
    asking: bool,
    /// The gimbal the engine lets the device turn, while it asks.
    gimbal: Option<GimbalRoute>,
    /// Whether a rate went to that gimbal: it is then told to stop when
    /// the rates end.
    sent: bool,
}

/// The distinct places an axis has read, until it reads between them.
#[derive(Debug, Default)]
struct Levels {
    seen: Vec<f64>,
    continuous: bool,
}

impl Levels {
    fn saw(&mut self, value: f64) {
        if self.continuous || self.seen.iter().any(|l| (l - value).abs() <= LEVEL) {
            return;
        }
        self.seen.push(value);
        self.seen.sort_by(f64::total_cmp);
        // Once it has read between two places, it is a stick, slider or
        // knob for good: never mistaken for a switch again.
        self.continuous = self.seen.windows(2).any(|w| w[1] - w[0] < SWITCH_STEP);
    }

    /// A stick, slider or knob; a switch (or anything the radio sends as
    /// one, such as a stick it outputs as on and off); or unknown until it
    /// has moved.
    fn kind(&self) -> Option<InputKind> {
        if self.continuous {
            Some(InputKind::Continuous)
        } else if self.seen.len() >= 2 {
            Some(InputKind::Switch)
        } else {
            None
        }
    }
}

impl Device {
    /// Whether Navigate has the button (S420): it then sends nothing.
    fn paused(&self, button: Option<GamepadButton>) -> bool {
        self.navigating.is_some() && button.is_some_and(|b| NAVIGATE_BUTTONS.contains(&b))
    }

    /// Navigate gives its buttons back: one bound to an action and still
    /// held acts only once let go and pressed again, as after a new route.
    fn end_navigating(&mut self) {
        if self.navigating.take().is_none() {
            return;
        }
        let held: Vec<_> = self
            .pressed_actions()
            .into_iter()
            .filter(|(_, button)| button.is_some_and(|b| NAVIGATE_BUTTONS.contains(&b)))
            .collect();
        self.stale_actions.extend(held);
        self.settle_gripper();
    }

    /// A binding's raw value: by its code when it has one, else by its
    /// gamepad axis. A known gamepad's stick at rest reports nothing and
    /// springs to the centre, so it reads zero until it moves; any other
    /// device's input is unknown until it moves, since a radio's throttle
    /// rests wherever it was left.
    fn read(&self, binding: &AxisBinding) -> Option<f64> {
        self.axes
            .iter()
            .find(|((code, axis), _)| match binding.code {
                Some(wanted) => *code == Some(wanted),
                None => *axis == Some(binding.axis),
            })
            .map(|(_, &value)| value)
            .or_else(|| (self.layout_known && binding.code.is_none()).then_some(0.0))
    }

    /// Ends the hold of `held`, if one is under way: whether there was.
    fn let_go(&mut self, held: &Held) -> bool {
        let before = self.holds.len();
        self.holds.retain(|pending| pending.held != *held);
        self.holds.len() != before
    }

    /// Ends a button's hold, whichever of its bindings it is for: whether
    /// there was one.
    fn let_go_button(&mut self, key: (Option<u32>, Option<GamepadButton>)) -> bool {
        let before = self.holds.len();
        self.holds.retain(|pending| {
            !matches!(&pending.held, Held::Button { button, code, .. } if (*code, *button) == key)
        });
        self.holds.len() != before
    }

    /// The Shift layer a press now would be on (S418): the one Shift of
    /// the route's mapping held, none, or more than one.
    fn layer(&self) -> Layer {
        let Some(route) = self.route.as_ref() else {
            return Layer::Base;
        };
        let mut held = route.mapping.shifts().filter(|shift| {
            self.buttons.iter().any(|((code, button), state)| {
                state.held && !self.paused(*button) && shift.matches(*button, *code)
            })
        });
        match (held.next(), held.next()) {
            (None, _) => Layer::Base,
            (Some(shift), None) => Layer::Shift(shift),
            (Some(_), Some(_)) => Layer::Mixed,
        }
    }

    /// The held action a button pressed on `layer` is bound to in the
    /// route's mapping, if any.
    fn held_action(
        &self,
        (code, button): (Option<u32>, Option<GamepadButton>),
        layer: Layer,
    ) -> Option<&ActionBinding> {
        if layer.is_mixed() {
            return None;
        }
        let mapping = &self.route.as_ref()?.mapping;
        mapping.action_in(button, code, layer.shift())
    }

    /// A button let go: its hold ends, its repeat stops, and a press that
    /// was waiting to see whether it was a long press goes, unless a
    /// double press may still follow (S418).
    fn button_up(
        &mut self,
        id: &str,
        key: (Option<u32>, Option<GamepadButton>),
        was: bool,
    ) -> Vec<Report> {
        let mut reports = Vec::new();
        self.repeats.retain(|repeat| repeat.key != key);
        if was && self.let_go_button(key) {
            reports.push(Report::LetGo { id: id.to_owned() });
        }
        let Some(at) = self.gestures.iter().position(|g| g.key == key) else {
            return reports;
        };
        let Some(mapping) = self.route.as_ref().map(|route| &route.mapping) else {
            return reports;
        };
        let gesture = &mut self.gestures[at];
        if gesture.stage != Stage::Down {
            if gesture.stage == Stage::Done {
                self.gestures.remove(at);
            }
            return reports;
        }
        let gestures = Gestures::of(mapping, key, gesture.shift);
        if gestures.double.is_some() {
            gesture.stage = Stage::Up { since_us: None };
            return reports;
        }
        let press = gestures.press.cloned();
        self.gestures.remove(at);
        reports.extend(press.map(|binding| fired(id, mapping, key, binding)));
        reports
    }

    /// A button pressed on `layer`: what goes now, or what it waits for.
    fn button_down(
        &mut self,
        id: &str,
        key: (Option<u32>, Option<GamepadButton>),
        layer: Layer,
    ) -> Vec<Report> {
        let (code, button) = key;
        let Some(mapping) = self.route.as_ref().map(|route| &route.mapping) else {
            return vec![Report::Pressed {
                id: id.to_owned(),
                button,
                code,
            }];
        };
        let mut reports = Vec::new();
        // The second press of a double press, on the layer of the first.
        // On another layer the first goes as a press and this one is new.
        if let Some(at) = self
            .gestures
            .iter()
            .position(|g| g.key == key && matches!(g.stage, Stage::Up { .. }))
        {
            let first = self.gestures.remove(at);
            let gestures = Gestures::of(mapping, key, first.shift);
            if let Some(double) = gestures
                .double
                .filter(|_| !layer.is_mixed() && layer.shift() == first.shift)
            {
                let double = double.clone();
                self.gestures.push(Gesturing {
                    key,
                    shift: first.shift,
                    stage: Stage::Done,
                });
                return vec![fired(id, mapping, key, double)];
            }
            reports.extend(
                gestures
                    .press
                    .cloned()
                    .map(|binding| fired(id, mapping, key, binding)),
            );
        }
        // Two Shifts held at once: no layer, so nothing.
        if layer.is_mixed() {
            return reports;
        }
        let shift = layer.shift();
        let gestures = Gestures::of(mapping, key, shift);
        let hold = |long: &ButtonBinding| {
            let hold_ms = long.hold_ms.unwrap_or_default();
            (
                Pending {
                    held: Held::Button {
                        button,
                        code,
                        binding: long.clone(),
                    },
                    since_us: None,
                    hold_ms,
                },
                Report::Holding {
                    id: id.to_owned(),
                    command: long.command.clone(),
                    hold_ms,
                },
            )
        };
        match (gestures.press, gestures.long, gestures.double) {
            (None, None, None) => {
                if shift.is_none() {
                    reports.push(Report::Pressed {
                        id: id.to_owned(),
                        button,
                        code,
                    });
                }
            }
            (Some(press), None, None) => {
                if press.repeat_ms.is_some() {
                    self.repeats.push(Repeat {
                        key,
                        binding: press.clone(),
                        since_us: None,
                        sent: 0,
                    });
                }
                reports.push(fired(id, mapping, key, press.clone()));
            }
            (None, Some(long), None) => {
                let (pending, report) = hold(long);
                self.holds.push(pending);
                reports.push(report);
            }
            (_, long, _) => {
                let started = long.map(hold);
                self.gestures.push(Gesturing {
                    key,
                    shift,
                    stage: Stage::Down,
                });
                if let Some((pending, report)) = started {
                    self.holds.push(pending);
                    reports.push(report);
                }
            }
        }
        reports
    }

    /// Presses whose double press did not come in time go as presses, and
    /// held steps repeat (S418). Timed from the first call after each
    /// began, as `due` times a hold.
    fn due_gestures(&mut self, id: &str, now_us: u64) -> Vec<Report> {
        let Some(mapping) = self.route.as_ref().map(|route| &route.mapping) else {
            return Vec::new();
        };
        let mut reports = Vec::new();
        self.gestures.retain_mut(|gesture| {
            let Stage::Up { since_us } = &mut gesture.stage else {
                return true;
            };
            let since = *since_us.get_or_insert(now_us);
            if now_us.saturating_sub(since) < u64::from(DOUBLE_PRESS_MS) * 1_000 {
                return true;
            }
            if let Some(press) = Gestures::of(mapping, gesture.key, gesture.shift).press {
                reports.push(fired(id, mapping, gesture.key, press.clone()));
            }
            false
        });
        for repeat in &mut self.repeats {
            let since = *repeat.since_us.get_or_insert(now_us);
            let every = u64::from(repeat.binding.repeat_ms.unwrap_or(u32::MAX)).max(1) * 1_000;
            let due = now_us.saturating_sub(since) / every;
            if due > repeat.sent {
                repeat.sent = due;
                reports.push(fired(id, mapping, repeat.key, repeat.binding.clone()));
            }
        }
        reports
    }

    /// The route's gripper buttons held now, by code and name.
    fn pressed_actions(&self) -> Vec<(Option<u32>, Option<GamepadButton>)> {
        self.buttons
            .iter()
            .filter(|(key, state)| {
                state.held && !self.paused(key.1) && self.held_action(**key, state.layer).is_some()
            })
            .map(|(key, _)| *key)
            .collect()
    }

    /// The gripper action asked for from the held buttons and the screen:
    /// none while open and close are both asked for, and none after that
    /// until every gripper input is let go.
    fn settle_gripper(&mut self) {
        let mut asked: Vec<GripperAction> = self
            .buttons
            .iter()
            .filter(|(key, state)| {
                state.held && !self.stale_actions.contains(key) && !self.paused(key.1)
            })
            .filter_map(|(key, state)| self.held_action(*key, state.layer))
            .filter_map(|binding| binding.action.gripper())
            .collect();
        asked.extend(self.screen.map(|(action, _)| action));
        asked.sort_unstable();
        asked.dedup();
        self.opposed = match asked.len() {
            0 => false,
            1 => self.opposed,
            _ => true,
        };
        self.gripper = if self.opposed || self.route.is_none() {
            None
        } else {
            asked.first().copied()
        };
    }

    /// What the route's gimbal inputs ask for now (S407), pitch then yaw,
    /// each -1 to 1 with up and right positive: its axes, and its buttons
    /// held since the route began. An axis that shares its input with a
    /// stick, an RC channel or a command position asks for nothing, as a
    /// position that conflicts sends nothing.
    fn gimbal_demand(&mut self) -> [f64; 2] {
        let mut stale = std::mem::take(&mut self.turn.stale);
        let Some(route) = self.route.as_ref() else {
            return [0.0; 2];
        };
        let mapping = &route.mapping;
        let inputs: Vec<_> = self.axes.keys().copied().collect();
        let axes = mapping.gimbal_axes(|binding| self.read(binding));
        stale.resize(axes.len(), false);
        let mut demand = [0.0; 2];
        let index = |control| match control {
            GimbalAxis::Pitch => 0,
            GimbalAxis::Yaw => 1,
        };
        for ((binding, value), stale) in mapping.gimbal.iter().zip(axes).zip(&mut stale) {
            if *stale {
                *stale = value != 0.0;
                continue;
            }
            if !mapping.gimbal_conflicts(binding, &inputs) {
                demand[index(binding.control)] += value;
            }
        }
        for (key, state) in self.buttons.iter().filter(|(key, state)| {
            state.held && !self.stale_actions.contains(key) && !self.paused(key.1)
        }) {
            if let Some((control, value)) = self
                .held_action(*key, state.layer)
                .and_then(|binding| binding.action.gimbal())
            {
                demand[index(control)] += value;
            }
        }
        self.turn.stale = stale;
        demand.map(|value| value.clamp(-1.0, 1.0))
    }

    /// What the route's camera inputs ask for now (S416), zoom then
    /// focus: its axes in quarters, so a trigger squeezed slowly asks the
    /// camera a few times and not on every reading, and its buttons held
    /// since the route began.
    fn camera_demand(&mut self) -> [f32; 2] {
        let mut stale = std::mem::take(&mut self.zoom.stale);
        let Some(route) = self.route.as_ref() else {
            return [0.0; 2];
        };
        let mapping = &route.mapping;
        let inputs: Vec<_> = self.axes.keys().copied().collect();
        let axes = mapping.camera_axes(|binding| self.read(binding));
        stale.resize(axes.len(), false);
        let mut demand = [0.0; 2];
        for ((binding, value), stale) in mapping.camera.iter().zip(axes).zip(&mut stale) {
            if *stale {
                *stale = value != 0.0;
                continue;
            }
            if !mapping.camera_conflicts(binding, &inputs) {
                demand[0] += (value * 4.0).round() / 4.0;
            }
        }
        for (key, state) in self.buttons.iter().filter(|(key, state)| {
            state.held && !self.stale_actions.contains(key) && !self.paused(key.1)
        }) {
            if let Some((index, value)) = self
                .held_action(*key, state.layer)
                .and_then(|binding| binding.action.camera())
            {
                demand[index] += value;
            }
        }
        self.zoom.stale = stale;
        #[allow(clippy::cast_possible_truncation, reason = "within ±1")]
        demand.map(|value| value.clamp(-1.0, 1.0) as f32)
    }

    /// The zeros the engine is owed when the camera inputs stop asking
    /// with no input let go: the route ended or changed, or the
    /// controller went.
    #[allow(clippy::float_cmp, reason = "the zeros are set exactly")]
    fn camera_ended(&mut self, id: &str) -> Option<Report> {
        let told = std::mem::take(&mut self.zoom.told);
        (told != [0.0; 2]).then(|| Report::CameraDrive {
            id: id.to_owned(),
            drive: [0.0; 2],
        })
    }

    /// The rates stop going to the gimbal it turned, which is told to
    /// stop if any went.
    fn gimbal_ended(&mut self) -> Option<GimbalRoute> {
        let gimbal = self.turn.gimbal.take();
        if std::mem::take(&mut self.turn.sent) {
            gimbal
        } else {
            None
        }
    }

    /// Whether the axis is in each position of the route's mapping now.
    fn in_positions(&self) -> Vec<bool> {
        let inputs: Vec<_> = self.axes.keys().copied().collect();
        self.route.as_ref().map_or_else(Vec::new, |route| {
            route
                .mapping
                .positions
                .iter()
                .map(|p| {
                    !route.mapping.position_conflicts(p, &inputs)
                        && self.read(&p.binding()).is_some_and(|v| p.holds(v))
                })
                .collect()
        })
    }

    /// The platform's gamepad name for an input, kept only when the layout
    /// is known; the fake, without codes, always names its inputs.
    fn named<T>(&self, code: Option<u32>, name: Option<T>) -> Option<T> {
        if self.layout_known || code.is_none() {
            name
        } else {
            None
        }
    }
}

/// Whose codes the inputs carry, for what the device calls each one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Naming {
    /// The platform's own: a HID usage on macOS, an evdev code on Linux.
    #[default]
    Native,
    /// The browser's Gamepad API: an axis's or button's index in its list.
    Browser,
}

impl Naming {
    fn name(self, code: u32, axis: bool) -> String {
        match self {
            Self::Native => input_name(code, axis),
            Self::Browser => format!("{} {}", if axis { "Axis" } else { "Button" }, code + 1),
        }
    }

    fn number(self, code: u32, axis: bool) -> Option<u32> {
        match self {
            Self::Native => input_number(code, axis),
            Self::Browser => Some(code + 1),
        }
    }
}

/// Every controller's latest input and route.
#[derive(Debug, Default)]
pub struct Controls {
    devices: BTreeMap<String, Device>,
    naming: Naming,
    /// The gimbals whose rates ended and that are still to be told to
    /// stop (`Controls::gimbal_stops`).
    stops: Vec<GimbalRoute>,
}

impl Controls {
    /// Controls whose inputs carry the browser's indices for codes.
    #[must_use]
    pub fn browser() -> Self {
        Self {
            devices: BTreeMap::new(),
            naming: Naming::Browser,
            stops: Vec::new(),
        }
    }

    /// Applies one input event; returns what the engine must hear.
    #[allow(clippy::too_many_lines, reason = "one arm per input event")]
    pub fn input(&mut self, event: InputEvent) -> Vec<Report> {
        match event {
            InputEvent::Keyboard { id, axes, at_us } => {
                let device = self.devices.entry(id.clone()).or_default();
                device.layout_known = true;
                device.keyboard_at_us = Some(at_us);
                for (axis, value) in [
                    GamepadAxis::RightStickX,
                    GamepadAxis::RightStickY,
                    GamepadAxis::LeftStickX,
                    GamepadAxis::LeftStickY,
                ]
                .into_iter()
                .zip(axes)
                {
                    device.axes.insert((None, Some(axis)), value);
                }
                self.neutral_changed(&id)
            }
            InputEvent::Connected {
                id,
                name,
                layout_known,
            } => {
                self.devices.entry(id.clone()).or_default().layout_known = layout_known;
                let mut reports = vec![Report::Connected {
                    id: id.clone(),
                    name,
                    layout_known,
                }];
                reports.extend(self.neutral_changed(&id));
                reports
            }
            InputEvent::Disconnected { id } => {
                // A gimbal it was turning stops with it (S407).
                let mut reports = Vec::new();
                if let Some(mut device) = self.devices.remove(&id) {
                    self.stops.extend(device.gimbal_ended());
                    if device.turn.asking {
                        reports.push(Report::GimbalLetGo { id: id.clone() });
                    }
                    // And a zoom or focus it was holding (S416).
                    reports.extend(device.camera_ended(&id));
                }
                reports.push(Report::Disconnected { id });
                reports
            }
            InputEvent::SecondUnit {
                id,
                name,
                connected,
            } => vec![Report::SecondUnit {
                id,
                name,
                connected,
            }],
            InputEvent::Axis {
                id,
                axis,
                code,
                value,
            } => {
                let device = self.devices.entry(id.clone()).or_default();
                let axis = device.named(code, axis);
                device.axes.insert((code, axis), value);
                device.levels.entry((code, axis)).or_default().saw(value);
                let mut reports = self.neutral_changed(&id);
                reports.extend(self.positions_entered(&id));
                reports
            }
            InputEvent::Button {
                id,
                button,
                code,
                pressed,
            } => self.button(&id, button, code, pressed),
            InputEvent::Power { id, power } => {
                if self.devices.contains_key(&id) {
                    vec![Report::Power { id, power }]
                } else {
                    Vec::new()
                }
            }
            InputEvent::Gripper { id, action, at_us } => {
                if let Some(device) = self.devices.get_mut(&id) {
                    match action {
                        None => {
                            device.screen = None;
                            device.screen_stale = false;
                        }
                        Some(action) if !device.screen_stale => {
                            device.screen = Some((action, at_us));
                        }
                        Some(_) => {}
                    }
                    device.settle_gripper();
                }
                Vec::new()
            }
            InputEvent::Navigate { id, on, at_us } => self.navigate(&id, on, at_us),
            InputEvent::Present { id, axes, buttons } => {
                let device = self.devices.entry(id.clone()).or_default();
                for (code, axis, value) in axes {
                    let axis = device.named(Some(code), axis);
                    device.axes.insert((Some(code), axis), value);
                    device
                        .levels
                        .entry((Some(code), axis))
                        .or_default()
                        .saw(value);
                }
                for (code, button, pressed) in buttons {
                    let button = device.named(Some(code), button);
                    device
                        .buttons
                        .entry((Some(code), button))
                        .or_insert(Button {
                            held: pressed,
                            ..Button::default()
                        });
                }
                let mut reports = self.neutral_changed(&id);
                reports.extend(self.positions_entered(&id));
                reports
            }
        }
    }

    /// A button went down or up: a press sends, or starts the hold its
    /// binding asks for; letting go ends that hold. With more than one
    /// gesture bound on the press's layer (S418), the press goes when let
    /// go, or once no second press came when a double press is bound.
    fn button(
        &mut self,
        id: &str,
        button: Option<GamepadButton>,
        code: Option<u32>,
        pressed: bool,
    ) -> Vec<Report> {
        let device = self.devices.entry(id.to_owned()).or_default();
        let button = device.named(code, button);
        let key = (code, button);
        let before = device.buttons.get(&key).copied().unwrap_or_default();
        let was = before.held;
        // A press's layer is the Shift held as it goes down (S418).
        let layer = if pressed && !was {
            device.layer()
        } else {
            before.layer
        };
        if pressed || device.buttons.contains_key(&key) {
            device.buttons.insert(
                key,
                Button {
                    held: pressed,
                    seen: before.seen || pressed,
                    presses: before.presses.saturating_add(u32::from(pressed && !was)),
                    layer,
                },
            );
        }
        if device.paused(button) {
            if !pressed {
                device.stale_actions.retain(|held| *held != (code, button));
            }
            return Vec::new();
        }
        if device.held_action(key, layer).is_some() {
            if !pressed {
                device.stale_actions.retain(|held| *held != key);
            }
            device.settle_gripper();
            return Vec::new();
        }
        if !pressed {
            return device.button_up(id, key, was);
        }
        if was || (button.is_none() && code.is_none()) {
            return Vec::new();
        }
        device.button_down(id, key, layer)
    }

    /// Navigate on or off for a controller (S420). Starting it lets go a
    /// hold under way on one of its buttons and stops what they hold, such
    /// as the gimbal; ending it gives them back.
    fn navigate(&mut self, id: &str, on: bool, at_us: u64) -> Vec<Report> {
        let Some(device) = self.devices.get_mut(id) else {
            return Vec::new();
        };
        if !on {
            device.end_navigating();
            return Vec::new();
        }
        let started = device.navigating.replace(at_us).is_none();
        if !started {
            return Vec::new();
        }
        let before = device.holds.len();
        device.holds.retain(|pending| {
            !matches!(pending.held, Held::Button { button, .. }
                if button.is_some_and(|b| NAVIGATE_BUTTONS.contains(&b)))
        });
        // Its buttons' undecided gestures and repeats end too (S418).
        let navigating = |(_, button): &(Option<u32>, Option<GamepadButton>)| {
            button.is_some_and(|b| NAVIGATE_BUTTONS.contains(&b))
        };
        device.gestures.retain(|gesture| !navigating(&gesture.key));
        device.repeats.retain(|repeat| !navigating(&repeat.key));
        device.settle_gripper();
        if device.holds.len() == before {
            Vec::new()
        } else {
            vec![Report::LetGo { id: id.to_owned() }]
        }
    }

    /// Holds held long enough, and positions settled: each hold ends, and
    /// its command goes as a press or a move into the position would. A
    /// hold's or a settle's time starts at the first call after it began,
    /// so call this on every pilot tick.
    pub fn due_holds(&mut self, now_us: u64) -> Vec<Report> {
        let mut reports = Vec::new();
        for (id, device) in &mut self.devices {
            let inputs: Vec<_> = device.axes.keys().copied().collect();
            let safe = |held: &Held| match held {
                Held::Button { binding, .. } => device
                    .route
                    .as_ref()
                    .is_some_and(|r| r.mapping.buttons.contains(binding)),
                Held::Position(p) => device.route.as_ref().is_some_and(|r| {
                    r.mapping.positions.contains(p) && !r.mapping.position_conflicts(p, &inputs)
                }),
            };
            let mut done = Vec::new();
            for held in due(&mut device.holds, now_us) {
                reports.push(Report::LetGo { id: id.clone() });
                if let Held::Button { button, code, .. } = &held {
                    done.push((*code, *button));
                }
                if safe(&held)
                    && let Some(route) = device.route.as_ref()
                {
                    reports.push(held.report(id, &route.mapping));
                }
            }
            // A long press that went decides its press: nothing more goes.
            for gesture in &mut device.gestures {
                if done.contains(&gesture.key) {
                    gesture.stage = Stage::Done;
                }
            }
            for held in due(&mut device.settling, now_us) {
                if safe(&held)
                    && let Some(route) = device.route.as_ref()
                {
                    reports.push(held.report(id, &route.mapping));
                }
            }
            reports.extend(device.due_gestures(id, now_us));
        }
        reports
    }

    /// Lets go on-screen gripper holds not refreshed for `SCREEN_HOLD_US`,
    /// as a page that disappeared stops refreshing; the page must let go
    /// and press again. Call on every pilot tick; true when one expired.
    pub fn expire_screen_holds(&mut self, now_us: u64) -> bool {
        let mut expired = false;
        for device in self.devices.values_mut() {
            if device
                .screen
                .is_some_and(|(_, at)| now_us.saturating_sub(at) >= SCREEN_HOLD_US)
            {
                device.screen = None;
                device.screen_stale = true;
                device.settle_gripper();
                expired = true;
            }
            // A window that stopped refreshing Navigate gives its buttons back (S420).
            if device
                .navigating
                .is_some_and(|at| now_us.saturating_sub(at) >= NAVIGATE_LEASE_US)
            {
                device.end_navigating();
            }
        }
        expired
    }

    /// Centre stale steering without releasing the page's keyboard session.
    /// Throttle stays at its chosen setting until the page changes or releases it.
    /// The gimbal's keys are let go with the steering (S407).
    #[must_use]
    pub fn stale_keyboard_inputs(&self, now_us: u64) -> Vec<InputEvent> {
        let pushed = |device: &Device, axes: &[GamepadAxis]| {
            axes.iter().any(|axis| {
                device
                    .axes
                    .get(&(None, Some(*axis)))
                    .is_some_and(|value| *value != 0.0)
            })
        };
        let mut events = Vec::new();
        for (id, device) in self.devices.iter().filter(|(_, device)| {
            device
                .keyboard_at_us
                .is_some_and(|at| now_us.saturating_sub(at) >= 500_000)
        }) {
            if pushed(
                device,
                &[
                    GamepadAxis::RightStickX,
                    GamepadAxis::RightStickY,
                    GamepadAxis::LeftStickX,
                ],
            ) {
                events.push(InputEvent::Keyboard {
                    id: id.clone(),
                    axes: [
                        0.0,
                        0.0,
                        0.0,
                        device
                            .axes
                            .get(&(None, Some(GamepadAxis::LeftStickY)))
                            .copied()
                            .unwrap_or(-1.0),
                    ],
                    at_us: now_us,
                });
            }
            for axis in KEYBOARD_GIMBAL_AXES {
                if pushed(device, &[axis]) {
                    events.push(InputEvent::Axis {
                        id: id.clone(),
                        axis: Some(axis),
                        code: None,
                        value: 0.0,
                    });
                }
            }
        }
        events
    }

    /// Every controller's raw input, as `controller/inputs` returns it.
    #[must_use]
    pub fn snapshot(&self) -> ControllerInputsResult {
        ControllerInputsResult {
            controllers: self
                .devices
                .iter()
                .map(|(id, device)| ControllerInputs {
                    id: id.clone(),
                    layout_known: device.layout_known,
                    axes: device
                        .axes
                        .iter()
                        .map(|(&(code, axis), &value)| AxisValue {
                            code,
                            name: code
                                .map_or_else(|| kebab(axis), |code| self.naming.name(code, true)),
                            number: code.and_then(|code| self.naming.number(code, true)),
                            axis: axis.map(Into::into),
                            value,
                            // A known gamepad's sticks are sticks: only a
                            // device without a known layout is judged.
                            kind: (!device.layout_known)
                                .then(|| device.levels.get(&(code, axis)).and_then(Levels::kind))
                                .flatten(),
                        })
                        .collect(),
                    buttons: device
                        .buttons
                        .iter()
                        .map(|(&(code, button), state)| ButtonState {
                            code,
                            name: code.map_or_else(
                                || kebab(button),
                                |code| self.naming.name(code, false),
                            ),
                            number: code.and_then(|code| self.naming.number(code, false)),
                            button: button.map(Into::into),
                            pressed: state.held,
                            seen: state.seen,
                            presses: state.presses,
                        })
                        .collect(),
                    gripper: device.gripper.map(Into::into),
                    navigating: device.navigating.is_some(),
                })
                .collect(),
        }
    }

    /// The engine's decision: what this controller drives now.
    pub fn route(&mut self, id: &str, route: Option<ControlRoute>) -> Vec<Report> {
        let device = self.devices.entry(id.to_owned()).or_default();
        // A gimbal it was turning under its last route stops (S407), and
        // an axis off its centre now turns nothing until centred once.
        self.stops.extend(device.gimbal_ended());
        let asked = std::mem::take(&mut device.turn.asking);
        // So does a zoom or focus it was holding (S416).
        let zoomed = device.camera_ended(id);
        device.route = route;
        device.zoom.stale = device.route.as_ref().map_or_else(Vec::new, |route| {
            route
                .mapping
                .camera_axes(|binding| device.read(binding))
                .into_iter()
                .map(|value| value != 0.0)
                .collect()
        });
        device.turn.stale = device.route.as_ref().map_or_else(Vec::new, |route| {
            route
                .mapping
                .gimbal_axes(|binding| device.read(binding))
                .into_iter()
                .map(|value| value != 0.0)
                .collect()
        });
        // Where the axes are now is no move: a claim never sends a command.
        device.held = device.in_positions();
        // Nor does a gripper button already down, or a hold the page keeps
        // refreshing: each needs letting go and a fresh press.
        device.stale_actions = device.pressed_actions();
        device.screen_stale = device.screen.is_some();
        device.screen = None;
        device.opposed = false;
        device.settle_gripper();
        // A hold begun under another route sends nothing, nor a position
        // still settling.
        let let_go = !device.holds.is_empty();
        device.holds.clear();
        device.settling.clear();
        // Nor a press waiting to be told from a long or double press, or a
        // repeat (S418).
        device.gestures.clear();
        device.repeats.clear();
        let mut reports = Vec::new();
        if let_go {
            reports.push(Report::LetGo { id: id.to_owned() });
        }
        if asked {
            reports.push(Report::GimbalLetGo { id: id.to_owned() });
        }
        reports.extend(zoomed);
        // Neutral is judged through the mapping in use.
        reports.extend(self.neutral_changed(id));
        reports
    }

    /// The engine's answer to [`Report::GimbalAsked`] (S407): the gimbal a
    /// controller may turn while its inputs go on asking, or none when it
    /// may not any more. One it was turning is then told to stop, and its
    /// inputs turn nothing until all are let go and one is pushed again.
    pub fn steer_gimbal(&mut self, id: &str, gimbal: Option<GimbalRoute>) {
        let Some(device) = self.devices.get_mut(id) else {
            return;
        };
        if device.turn.gimbal == gimbal {
            return;
        }
        self.stops.extend(device.gimbal_ended());
        if device.turn.asking && device.route.is_some() {
            device.turn.gimbal = gimbal;
        }
    }

    /// What the gimbal inputs of the routed controllers ask for now
    /// (S407). Call on every pilot tick: who began to ask and who let go
    /// is for the engine, and the rates go to the gimbals the engine let
    /// them turn. One that lets go stops its gimbal
    /// ([`Controls::gimbal_stops`]).
    pub fn gimbal_outputs(&mut self) -> GimbalOutputs {
        let mut outputs = GimbalOutputs::default();
        for (id, device) in &mut self.devices {
            let demand = device.gimbal_demand();
            let asking = demand.iter().any(|value| value.abs() > 0.0);
            if asking != device.turn.asking {
                device.turn.asking = asking;
                outputs.reports.push(if asking {
                    Report::GimbalAsked { id: id.clone() }
                } else {
                    Report::GimbalLetGo { id: id.clone() }
                });
            }
            if !asking {
                self.stops.extend(device.gimbal_ended());
                continue;
            }
            if let Some(gimbal) = device.turn.gimbal.clone() {
                device.turn.sent = true;
                #[allow(clippy::cast_possible_truncation, reason = "a rate within ±1")]
                outputs.rates.push((
                    gimbal,
                    demand.map(|value| (value * GIMBAL_FULL_RATE_RAD_S) as f32),
                ));
            }
        }
        outputs
    }

    /// What the camera inputs of the routed controllers ask for now
    /// (S416), for the engine: call on every pilot tick. A change is said
    /// at once, and a zoom or focus still held is said again every
    /// [`CAMERA_REPEAT_MS`].
    #[allow(clippy::float_cmp, reason = "demands are quarter steps and exact")]
    pub fn camera_outputs(&mut self, now_us: u64) -> Vec<Report> {
        let mut reports = Vec::new();
        for (id, device) in &mut self.devices {
            let drive = device.camera_demand();
            let again = drive != [0.0; 2]
                && now_us.saturating_sub(device.zoom.told_us) >= CAMERA_REPEAT_MS * 1_000;
            if drive != device.zoom.told || again {
                device.zoom.told = drive;
                device.zoom.told_us = now_us;
                reports.push(Report::CameraDrive {
                    id: id.clone(),
                    drive,
                });
            }
        }
        reports
    }

    /// The gimbals whose rates just ended, each to be told to stop: after
    /// a pilot tick, an input event, a route and an answer of the
    /// engine's.
    pub fn gimbal_stops(&mut self) -> Vec<GimbalRoute> {
        std::mem::take(&mut self.stops)
    }

    /// Every gimbal a controller is turning now, to be told to stop when
    /// input is no longer read at all, such as when the daemon stops.
    pub fn gimbal_stops_all(&mut self) -> Vec<GimbalRoute> {
        let mut stops = std::mem::take(&mut self.stops);
        for device in self.devices.values_mut() {
            stops.extend(device.gimbal_ended());
        }
        stops
    }

    /// The sticks through the mapping in use; none while an input it
    /// needs is unknown, so nothing is sent and it never counts as centred.
    fn sticks(device: &Device) -> Option<Sticks> {
        let default = InputMapping::mode_2();
        let mapping = device.route.as_ref().map_or(&default, |r| &r.mapping);
        if mapping.stick_bindings().any(|b| device.read(b).is_none()) {
            return None;
        }
        Some(mapping.sticks(|binding| device.read(binding).unwrap_or(0.0)))
    }

    /// The positions an axis has just moved into.
    fn positions_entered(&mut self, id: &str) -> Vec<Report> {
        let Some(device) = self.devices.get_mut(id) else {
            return Vec::new();
        };
        let now = device.in_positions();
        let positions = device
            .route
            .as_ref()
            .map_or_else(Vec::new, |route| route.mapping.positions.clone());
        let mut reports = Vec::new();
        for (i, (position, &inside)) in positions.iter().zip(&now).enumerate() {
            let was = device.held.get(i).copied().unwrap_or(true);
            let held = Held::Position(position.clone());
            if !inside && was && device.let_go(&held) {
                reports.push(Report::LetGo { id: id.to_owned() });
            }
            if !inside {
                device.settling.retain(|pending| pending.held != held);
            }
            if !inside || was {
                continue;
            }
            match position.hold_ms {
                Some(hold_ms) => {
                    device.holds.push(Pending {
                        held,
                        since_us: None,
                        hold_ms,
                    });
                    reports.push(Report::Holding {
                        id: id.to_owned(),
                        command: position.command.clone(),
                        hold_ms,
                    });
                }
                None => device.settling.push(Pending {
                    held,
                    since_us: None,
                    hold_ms: SETTLE_MS,
                }),
            }
        }
        device.held = now;
        reports
    }

    fn neutral_changed(&mut self, id: &str) -> Vec<Report> {
        let Some(device) = self.devices.get_mut(id) else {
            return Vec::new();
        };
        let neutral = Self::sticks(device).is_some_and(|sticks| sticks.neutral());
        if device.neutral == Some(neutral) {
            return Vec::new();
        }
        device.neutral = Some(neutral);
        vec![Report::Neutral {
            id: id.to_owned(),
            neutral,
        }]
    }

    /// Whether any controller is routed to a vehicle, so sticks are sent.
    #[must_use]
    pub fn routed(&self) -> bool {
        self.devices.values().any(|device| device.route.is_some())
    }

    #[must_use]
    pub fn target(&self, id: &str) -> Option<String> {
        self.devices
            .get(id)?
            .route
            .as_ref()
            .map(|r| r.vehicle_id.clone())
    }

    /// Pilot inputs plus explicit additional RC channels, one per controller.
    #[allow(
        clippy::type_complexity,
        reason = "pilot inputs and auxiliary RC inputs"
    )]
    #[must_use]
    pub fn pilot_outputs(&self) -> Vec<(String, [i16; 4], Vec<(u8, i16)>, PilotInputMethod, u32)> {
        self.devices
            .values()
            .filter_map(|device| {
                let route = device.route.as_ref()?;
                let mut auxiliary = Vec::new();
                for binding in &route.mapping.rc_channels {
                    let value = binding
                        .input
                        .calibrated(device.read(&binding.input)?.clamp(-1.0, 1.0));
                    let value = if binding.input.inverted {
                        -value
                    } else {
                        value
                    };
                    #[allow(clippy::cast_possible_truncation, reason = "clamped input")]
                    auxiliary.push((binding.channel, (value * 1000.0).round() as i16));
                }
                Some((
                    route.vehicle_id.clone(),
                    Self::sticks(device)?.manual_control(),
                    auxiliary,
                    route.pilot_input_method,
                    route
                        .gripper
                        .as_ref()
                        .map_or(0, |wiring| wiring.buttons(device.gripper)),
                ))
            })
            .collect()
    }

    /// The `MANUAL_CONTROL` values to send now, one per routed controller.
    #[must_use]
    #[cfg(test)]
    pub fn outputs(&self) -> Vec<(String, [i16; 4])> {
        self.pilot_outputs()
            .into_iter()
            .map(|(vehicle, sticks, _, _, _)| (vehicle, sticks))
            .collect()
    }
}

/// A gamepad input's wire name, for the fake, which has no codes.
fn kebab(input: Option<impl serde::Serialize>) -> String {
    input
        .and_then(|input| serde_json::to_value(input).ok())
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// macOS duplicate HID axes use a reserved page and their one-based descriptor
/// order. Ordinary axes retain their usage codes, so existing mappings survive.
/// The raw HID reader excludes vendor-defined pages, including this one.
#[must_use]
pub const fn macos_hid_axis_code(number: u16) -> u32 {
    0xFFFF_0000 | number as u32
}

fn macos_hid_axis_number(code: u32) -> Option<u32> {
    (code >> 16 == 0xFFFF && code & 0xFFFF != 0).then_some(code & 0xFFFF)
}

/// What the device calls an input, from the platform's code for it: on
/// macOS a HID usage page and usage (`X`, `Rz`, `Slider`, `Button 3`), on
/// Linux an evdev code. A radio in joystick mode sends its channels on these
/// in order, so the names are what someone setting one up needs.
fn input_name(code: u32, axis: bool) -> String {
    if cfg!(target_os = "macos")
        && axis
        && let Some(number) = macos_hid_axis_number(code)
    {
        return format!("Axis {number}");
    }
    let (high, low) = (code >> 16, code & 0xFFFF);
    let named = if cfg!(target_os = "macos") {
        match (high, low) {
            (0x01, 0x30) => Some("X".to_owned()),
            (0x01, 0x31) => Some("Y".to_owned()),
            (0x01, 0x32) => Some("Z".to_owned()),
            (0x01, 0x33) => Some("Rx".to_owned()),
            (0x01, 0x34) => Some("Ry".to_owned()),
            (0x01, 0x35) => Some("Rz".to_owned()),
            (0x01, 0x36) => Some("Slider".to_owned()),
            (0x01, 0x37) => Some("Dial".to_owned()),
            (0x01, 0x38) => Some("Wheel".to_owned()),
            (0x01, 0x39) => Some("Hat".to_owned()),
            (0x02, 0xBA) => Some("Rudder".to_owned()),
            (0x02, 0xBB) => Some("Throttle".to_owned()),
            (0x02, 0xC4) => Some("Accelerator".to_owned()),
            (0x02, 0xC5) => Some("Brake".to_owned()),
            (0x09, n) => Some(format!("Button {n}")),
            _ => None,
        }
    } else if cfg!(target_os = "linux") && axis {
        const ABS: [&str; 11] = [
            "X", "Y", "Z", "Rx", "Ry", "Rz", "Throttle", "Rudder", "Wheel", "Gas", "Brake",
        ];
        ABS.get(low as usize).map(|&name| name.to_owned())
    } else if cfg!(target_os = "linux") && (0x120..0x130).contains(&low) {
        Some(format!("Button {}", low - 0x120 + 1))
    } else {
        None
    };
    named.unwrap_or_else(|| format!("{} {code:#x}", if axis { "Axis" } else { "Button" }))
}

/// An input's number on the device, from the platform's code: on macOS a
/// USB joystick's axes in HID order (X 1, Y 2, Z 3, Rx 4, Ry 5, Rz 6,
/// Slider 7, Dial 8, Wheel 9) and its buttons' own numbers; on Linux the
/// evdev order.
fn input_number(code: u32, axis: bool) -> Option<u32> {
    if cfg!(target_os = "macos")
        && axis
        && let Some(number) = macos_hid_axis_number(code)
    {
        return Some(number);
    }
    let (high, low) = (code >> 16, code & 0xFFFF);
    if cfg!(target_os = "macos") {
        match (high, low) {
            (0x01, 0x30..=0x38) if axis => Some(low - 0x2F),
            (0x09, n) if !axis => Some(n),
            _ => None,
        }
    } else if cfg!(target_os = "linux") {
        match low {
            0..=10 if axis => Some(low + 1),
            0x120..=0x12F if !axis => Some(low - 0x11F),
            _ => None,
        }
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn axis(value: f64) -> InputEvent {
        InputEvent::Axis {
            id: "g".into(),
            axis: Some(GamepadAxis::RightStickY),
            code: None,
            value,
        }
    }

    #[test]
    fn keyboard_centres_stale_steering_without_releasing_or_changing_throttle() {
        let mut controls = Controls::default();
        controls.input(InputEvent::Keyboard {
            id: "keyboard:1".into(),
            axes: [0.0, 0.0, 0.0, -0.5],
            at_us: 100,
        });
        assert!(controls.outputs().is_empty());
        controls.route(
            "keyboard:1",
            Some(ControlRoute {
                vehicle_id: "sim".into(),
                mapping: InputMapping::mode_2(),
                pilot_input_method: PilotInputMethod::Automatic,
                gripper: None,
            }),
        );
        let centred = controls.outputs();
        assert!(controls.stale_keyboard_inputs(60_000_000).is_empty());
        assert_eq!(controls.outputs(), centred);
        controls.input(InputEvent::Keyboard {
            id: "keyboard:1".into(),
            axes: [0.5, 0.0, 0.0, -0.5],
            at_us: 60_000_000,
        });
        assert!(controls.stale_keyboard_inputs(60_499_999).is_empty());
        let stale = controls.stale_keyboard_inputs(60_500_000);
        assert_eq!(stale.len(), 1);
        // Only the yaw stays held; the stick that went quiet is centred.
        assert!(matches!(&stale[0], InputEvent::Keyboard { axes, .. }
            if axes.iter().zip([0.0, 0.0, 0.0, -0.5]).all(|(a, b)| (a - b).abs() < 1e-6)));
        for event in stale {
            controls.input(event);
        }
        assert_eq!(controls.outputs(), centred);
        assert!(controls.routed());
        controls.input(InputEvent::Keyboard {
            id: "keyboard:1".into(),
            axes: [-0.5, 0.0, 0.0, -0.5],
            at_us: 120_000_000,
        });
        assert_ne!(controls.outputs(), centred);
        controls.input(InputEvent::Disconnected {
            id: "keyboard:1".into(),
        });
        assert!(controls.outputs().is_empty());
    }

    #[test]
    fn repeated_hid_axes_survive_saved_mapping_and_drive_separate_rc_channels() {
        let codes = [0x1_0036, macos_hid_axis_code(8)];
        let mut mapping = InputMapping::mode_2();
        mapping.roll.code = Some(0x1_0030);
        mapping.pitch.code = Some(0x1_0031);
        mapping.throttle.code = Some(0x1_0032);
        mapping.yaw.code = Some(0x1_0033);
        for (channel, code) in [(7, codes[0]), (8, codes[1])] {
            mapping
                .rc_channels
                .push(uxduck_core::domain::controller::RcAxisBinding {
                    channel,
                    input: AxisBinding {
                        axis: GamepadAxis::LeftZ,
                        code: Some(code),
                        inverted: false,
                        calibration: None,
                    },
                });
        }
        // Exercise the persisted mapping, including a code above signed i32.
        let saved = serde_json::to_string(&mapping).unwrap();
        let mut controls = Controls::default();
        for _ in 0..2 {
            controls.input(InputEvent::Connected {
                id: "r".into(),
                name: "TBS Joystick".into(),
                layout_known: false,
            });
            controls.input(InputEvent::Present {
                id: "r".into(),
                axes: vec![
                    (0x1_0030, None, 0.0),
                    (0x1_0031, None, 0.0),
                    (0x1_0032, None, -1.0),
                    (0x1_0033, None, 0.0),
                    (codes[0], None, -1.0),
                    (codes[1], None, -1.0),
                ],
                buttons: vec![],
            });
            controls.route(
                "r",
                Some(ControlRoute {
                    pilot_input_method: PilotInputMethod::Automatic,
                    gripper: None,
                    vehicle_id: "v".into(),
                    mapping: serde_json::from_str(&saved).unwrap(),
                }),
            );
            for (code, value, expected) in [
                (codes[0], 1.0, vec![(7, 1000), (8, -1000)]),
                (codes[1], 1.0, vec![(7, 1000), (8, 1000)]),
                (codes[0], 0.0, vec![(7, 0), (8, 1000)]),
            ] {
                controls.input(InputEvent::Axis {
                    id: "r".into(),
                    axis: None,
                    code: Some(code),
                    value,
                });
                assert_eq!(
                    controls.pilot_outputs(),
                    [(
                        "v".into(),
                        [0, 0, 0, 0],
                        expected,
                        PilotInputMethod::Automatic,
                        0
                    )]
                );
            }
            let snapshot = controls.snapshot();
            let axes = &snapshot.controllers[0].axes;
            for (code, expected) in [(codes[0], 0.0), (codes[1], 1.0)] {
                let value = axes.iter().find(|a| a.code == Some(code)).unwrap().value;
                assert!((value - expected).abs() < f64::EPSILON);
            }
            #[cfg(target_os = "macos")]
            {
                let eighth = axes.iter().find(|a| a.code == Some(codes[1])).unwrap();
                assert_eq!(eighth.name, "Axis 8");
                assert_eq!(eighth.number, Some(8));
            }
            controls.input(InputEvent::Disconnected { id: "r".into() });
            assert!(controls.pilot_outputs().is_empty());
            assert!(controls.snapshot().controllers.is_empty());
        }
    }

    #[test]
    fn extra_rc_axis_is_continuous_reversible_and_stops_with_its_route() {
        let mut controls = Controls::default();
        controls.input(InputEvent::Connected {
            id: "g".into(),
            name: "Pad".into(),
            layout_known: true,
        });
        let mut mapping = InputMapping::mode_2();
        mapping
            .rc_channels
            .push(uxduck_core::domain::controller::RcAxisBinding {
                channel: 9,
                input: AxisBinding {
                    axis: GamepadAxis::LeftZ,
                    calibration: None,
                    code: None,
                    inverted: true,
                },
            });
        controls.route(
            "g",
            Some(ControlRoute {
                pilot_input_method: PilotInputMethod::Automatic,
                gripper: None,
                vehicle_id: "v".into(),
                mapping,
            }),
        );
        for (value, expected) in [(-1.0, 1000), (0.0, 0), (1.0, -1000)] {
            controls.input(InputEvent::Axis {
                id: "g".into(),
                axis: Some(GamepadAxis::LeftZ),
                code: None,
                value,
            });
            assert_eq!(
                controls.pilot_outputs(),
                [(
                    "v".into(),
                    [0, 0, 500, 0],
                    vec![(9, expected)],
                    PilotInputMethod::Automatic,
                    0
                )]
            );
        }
        controls.route("g", None);
        assert!(controls.pilot_outputs().is_empty());
    }

    #[test]
    fn routed_centre_to_top_throttle_returns_to_zero_when_released() {
        let mut controls = Controls::default();
        controls.input(InputEvent::Connected {
            id: "g".into(),
            name: "Pad".into(),
            layout_known: true,
        });
        let mut mapping = InputMapping::mode_2();
        mapping.throttle_range = uxduck_core::domain::controller::ThrottleRange::CentreToTop;
        controls.route(
            "g",
            Some(ControlRoute {
                pilot_input_method: PilotInputMethod::Automatic,
                gripper: None,
                vehicle_id: "v".into(),
                mapping,
            }),
        );
        for (value, expected) in [(1.0, 1000), (0.525, 500), (0.0, 0), (-1.0, 0)] {
            controls.input(InputEvent::Axis {
                id: "g".into(),
                axis: Some(GamepadAxis::LeftStickY),
                code: None,
                value,
            });
            assert_eq!(controls.outputs(), [("v".into(), [0, 0, expected, 0])]);
        }
    }

    #[test]
    fn neutral_is_reported_on_change_and_only_routed_controllers_send() {
        let mut controls = Controls::default();
        let connected = controls.input(InputEvent::Connected {
            id: "g".into(),
            name: "Pad".into(),
            layout_known: true,
        });
        assert_eq!(
            connected[1],
            Report::Neutral {
                id: "g".into(),
                neutral: true
            }
        );
        assert_eq!(
            controls.input(axis(0.8)),
            [Report::Neutral {
                id: "g".into(),
                neutral: false
            }]
        );
        assert!(controls.input(axis(0.9)).is_empty(), "no change");
        assert!(controls.outputs().is_empty(), "not routed: silent");
        controls.route(
            "g",
            Some(ControlRoute {
                pilot_input_method: PilotInputMethod::Automatic,
                gripper: None,
                vehicle_id: "v".into(),
                mapping: InputMapping::mode_2(),
            }),
        );
        let [(vehicle, sticks)] = controls.outputs().try_into().unwrap();
        assert_eq!(vehicle, "v");
        assert!(sticks[0] > 800, "pitch forward: {sticks:?}");
        controls.route("g", None);
        assert!(controls.outputs().is_empty());
        let press = |pressed| InputEvent::Button {
            id: "g".into(),
            button: Some(GamepadButton::East),
            code: None,
            pressed,
        };
        assert_eq!(controls.input(press(true)).len(), 1);
        assert!(
            controls.input(press(true)).is_empty(),
            "held, not pressed again"
        );
        assert!(controls.input(press(false)).is_empty());
    }

    #[test]
    fn throttle_never_blocks_neutral() {
        let mut controls = Controls::default();
        controls.input(InputEvent::Connected {
            id: "g".into(),
            name: "Pad".into(),
            layout_known: true,
        });
        let idle = controls.input(InputEvent::Axis {
            id: "g".into(),
            axis: Some(GamepadAxis::LeftStickY),
            code: None,
            value: -1.0,
        });
        assert!(idle.is_empty(), "still neutral with throttle at idle");
        let full = controls.input(InputEvent::Axis {
            id: "g".into(),
            axis: Some(GamepadAxis::LeftStickY),
            code: None,
            value: 1.0,
        });
        assert!(full.is_empty(), "and at full");
    }

    #[test]
    fn a_radio_is_shown_by_its_own_names_and_driven_by_code() {
        // A Tango 2 in joystick mode on macOS: channels on X, Y, Z, Rx...
        const X: u32 = 0x1_0030;
        const Z: u32 = 0x1_0032;
        const RX: u32 = 0x1_0033;
        let mut controls = Controls::default();
        let axis = |code, axis, value| InputEvent::Axis {
            id: "r".into(),
            axis,
            code: Some(code),
            value,
        };
        controls.input(InputEvent::Connected {
            id: "r".into(),
            name: "TBS Joystick".into(),
            layout_known: false,
        });
        // gilrs guesses gamepad names for it; they are dropped, never shown.
        controls.input(axis(X, Some(GamepadAxis::LeftStickX), 0.02));
        controls.input(axis(Z, Some(GamepadAxis::RightStickX), -1.0));
        let pressed = controls.input(InputEvent::Button {
            id: "r".into(),
            button: Some(GamepadButton::South),
            code: Some(0x9_0018),
            pressed: true,
        });
        assert_eq!(
            pressed,
            [Report::Pressed {
                id: "r".into(),
                button: None,
                code: Some(0x9_0018)
            }],
            "a radio's button reaches the engine by its code alone"
        );
        let [inputs] = controls.snapshot().controllers.try_into().unwrap();
        assert!(!inputs.layout_known);
        assert!(inputs.axes.iter().all(|a| a.axis.is_none()), "no guesses");
        assert!(inputs.buttons[0].button.is_none(), "no guesses");
        if cfg!(target_os = "macos") {
            let numbered: Vec<(&str, Option<u32>)> = inputs
                .axes
                .iter()
                .map(|a| (a.name.as_str(), a.number))
                .collect();
            assert_eq!(numbered, [("X", Some(1)), ("Z", Some(3))]);
            assert_eq!(inputs.buttons[0].name, "Button 24");
            assert_eq!(inputs.buttons[0].number, Some(24));
        }
        assert!(inputs.buttons[0].pressed);

        // Mapped by code: throttle on Z, yaw on Rx, roll on X, pitch on Y.
        let mut mapping = InputMapping::mode_2();
        mapping.roll.code = Some(X);
        mapping.pitch.code = Some(0x1_0031);
        mapping.throttle.code = Some(Z);
        mapping.yaw.code = Some(RX);
        let routed = controls.route(
            "r",
            Some(ControlRoute {
                pilot_input_method: PilotInputMethod::Automatic,
                gripper: None,
                vehicle_id: "v".into(),
                mapping,
            }),
        );
        assert!(
            controls.outputs().is_empty(),
            "Y and Rx have not moved: their positions are unknown, so nothing is sent"
        );
        assert!(
            routed.is_empty()
                || routed
                    == [Report::Neutral {
                        id: "r".into(),
                        neutral: false
                    }]
        );
        controls.input(axis(0x1_0031, None, 0.0));
        let centred = controls.input(axis(RX, None, 0.5));
        assert!(centred.is_empty(), "yaw off centre: still not neutral");
        let [(_, sticks)] = controls.outputs().try_into().unwrap();
        assert_eq!(sticks[2], 0, "throttle at idle, read from Z");
        assert_eq!(sticks[3], 474, "yaw from Rx, past the deadzone");
        assert_eq!(
            controls.input(axis(RX, None, 0.0)),
            [Report::Neutral {
                id: "r".into(),
                neutral: true
            }],
            "every input known, roll, pitch and yaw centred, throttle at idle"
        );
    }

    #[test]
    fn a_radio_listed_at_connect_shows_every_input_and_drives_before_moving() {
        let mut controls = Controls::default();
        controls.input(InputEvent::Connected {
            id: "r".into(),
            name: "TBS Joystick".into(),
            layout_known: false,
        });
        let present = controls.input(InputEvent::Present {
            id: "r".into(),
            axes: vec![
                (0x1_0030, Some(GamepadAxis::LeftStickX), 0.0),
                (0x1_0031, None, 0.0),
                (0x1_0032, None, -1.0),
                (0x1_0033, None, 0.0),
                (0x1_0036, None, 1.0),
            ],
            buttons: (1..=24).map(|n| (0x9_0000 | n, None, false)).collect(),
        });
        assert!(
            present.is_empty(),
            "Mode 2 reads nothing of a radio: still not neutral"
        );
        let [inputs] = controls.snapshot().controllers.try_into().unwrap();
        assert_eq!(inputs.axes.len(), 5);
        assert_eq!(inputs.buttons.len(), 24, "every button, pressed or not");
        assert!(inputs.buttons.iter().all(|b| !b.seen && !b.pressed));
        assert!(inputs.axes.iter().all(|a| a.axis.is_none()), "no guesses");

        let mut mapping = InputMapping::mode_2();
        mapping.roll.code = Some(0x1_0030);
        mapping.pitch.code = Some(0x1_0031);
        mapping.throttle.code = Some(0x1_0032);
        mapping.yaw.code = Some(0x1_0033);
        let routed = controls.route(
            "r",
            Some(ControlRoute {
                pilot_input_method: PilotInputMethod::Automatic,
                gripper: None,
                vehicle_id: "v".into(),
                mapping,
            }),
        );
        assert_eq!(
            routed,
            [Report::Neutral {
                id: "r".into(),
                neutral: true
            }],
            "every input known from the start: claimable without moving"
        );
        let [(_, sticks)] = controls.outputs().try_into().unwrap();
        assert_eq!(sticks, [0, 0, 0, 0], "throttle at idle, the rest centred");
        controls.input(InputEvent::Button {
            id: "r".into(),
            button: None,
            code: Some(0x9_0003),
            pressed: true,
        });
        let [inputs] = controls.snapshot().controllers.try_into().unwrap();
        let three = inputs
            .buttons
            .iter()
            .find(|b| b.code == Some(0x9_0003))
            .unwrap();
        assert!(three.pressed && three.seen);
    }

    /// FB-60/FB-61: a rover's radio with its left stick's channels turned
    /// off reports only steering and throttle. With pitch and yaw not used
    /// it is neutral at rest and drives; with them used it never would.
    #[test]
    fn controls_not_used_need_no_input_and_send_centred() {
        use uxduck_core::domain::controller::StickControl;
        let mut controls = Controls::default();
        controls.input(InputEvent::Connected {
            id: "r".into(),
            name: "TBS Joystick".into(),
            layout_known: false,
        });
        controls.input(InputEvent::Present {
            id: "r".into(),
            axes: vec![(0x1_0030, None, 0.0), (0x1_0032, None, -1.0)],
            buttons: Vec::new(),
        });
        let mut rover = InputMapping::radio();
        assert!(
            routed_to(&mut controls, "r", rover.clone()).is_empty(),
            "pitch and yaw read nothing: never neutral"
        );
        assert!(controls.outputs().is_empty(), "and nothing is sent");
        rover.unused = vec![StickControl::Pitch, StickControl::Yaw];
        assert_eq!(
            routed_to(&mut controls, "r", rover),
            [Report::Neutral {
                id: "r".into(),
                neutral: true
            }]
        );
        controls.input(InputEvent::Axis {
            id: "r".into(),
            axis: None,
            code: Some(0x1_0030),
            value: 1.0,
        });
        let [(_, sticks)] = controls.outputs().try_into().unwrap();
        assert_eq!(
            sticks,
            [0, 1000, 0, 0],
            "steering full right, throttle idle, pitch and yaw centred"
        );
    }

    #[test]
    fn the_snapshot_shows_raw_axes_and_buttons_pressed_since_plugged_in() {
        let mut controls = Controls::default();
        controls.input(axis(-0.4));
        let button = |button, pressed| InputEvent::Button {
            id: "g".into(),
            button: Some(button),
            code: None,
            pressed,
        };
        controls.input(button(GamepadButton::LeftTrigger2, true));
        controls.input(button(GamepadButton::South, true));
        controls.input(button(GamepadButton::South, false));
        controls.input(button(GamepadButton::East, false));
        let [inputs] = controls.snapshot().controllers.try_into().unwrap();
        assert_eq!(inputs.id, "g");
        assert_eq!(inputs.axes.len(), 1);
        assert_eq!(inputs.axes[0].name, "right-stick-y");
        assert!((inputs.axes[0].value + 0.4).abs() < f64::EPSILON);
        let held: Vec<(&str, bool)> = inputs
            .buttons
            .iter()
            .map(|b| (b.name.as_str(), b.pressed))
            .collect();
        assert_eq!(
            held,
            [("south", false), ("left-trigger-2", true)],
            "a release never pressed is not shown"
        );
        controls.input(InputEvent::Disconnected { id: "g".into() });
        assert!(controls.snapshot().controllers.is_empty());
    }

    #[test]
    fn moving_into_a_position_sends_its_command_once_and_never_at_a_claim() {
        use uxduck_core::domain::command::Command;
        // A six-way button group on Rz, its buttons a step of 0.4 apart.
        const RZ: u32 = 0x1_0035;
        let mut controls = Controls::default();
        let rz = |value| InputEvent::Axis {
            id: "r".into(),
            axis: None,
            code: Some(RZ),
            value,
        };
        controls.input(InputEvent::Connected {
            id: "r".into(),
            name: "GX12".into(),
            layout_known: false,
        });
        controls.input(rz(-0.2));
        let at = |at, mode: &str| PositionBinding {
            axis: GamepadAxis::LeftStickX,
            code: Some(RZ),
            at,
            command: Command::SetMode { mode: mode.into() },
            hold_ms: None,
        };
        let mut mapping = InputMapping::mode_2();
        mapping.positions = vec![at(-0.2, "LOITER"), at(0.2, "ALT_HOLD")];
        let routed = controls.route(
            "r",
            Some(ControlRoute {
                pilot_input_method: PilotInputMethod::Automatic,
                gripper: None,
                vehicle_id: "v".into(),
                mapping,
            }),
        );
        assert!(
            !routed
                .iter()
                .any(|r| matches!(r, Report::Positioned { .. })),
            "already in LOITER's position at the claim: nothing sent"
        );
        let positioned = |reports: Vec<Report>| -> Vec<String> {
            reports
                .into_iter()
                .filter_map(|r| match r {
                    Report::Positioned { position, .. } => match position.command {
                        Command::SetMode { mode } => Some(mode),
                        _ => None,
                    },
                    _ => None,
                })
                .collect()
        };
        let mut now = 0;
        let mut settled = |controls: &mut Controls, value| {
            let mut reports = controls.input(rz(value));
            now += 1_000_000;
            reports.extend(controls.due_holds(now));
            reports.extend(controls.due_holds(now + u64::from(SETTLE_MS) * 1_000));
            positioned(reports)
        };
        assert_eq!(settled(&mut controls, 0.21), ["ALT_HOLD"]);
        assert!(settled(&mut controls, 0.19).is_empty(), "still in it");
        assert!(settled(&mut controls, 0.6).is_empty(), "no position there");
        assert_eq!(settled(&mut controls, -0.2), ["LOITER"]);
        controls.route("r", None);
        assert!(settled(&mut controls, 0.2).is_empty(), "not routed");
    }

    #[test]
    fn a_switch_flicked_through_a_position_sends_only_where_it_settles() {
        use uxduck_core::domain::command::Command;
        // FB-67: a three-way switch on Rz.
        const RZ: u32 = 0x1_0035;
        let mut controls = Controls::default();
        let rz = |value| InputEvent::Axis {
            id: "r".into(),
            axis: None,
            code: Some(RZ),
            value,
        };
        controls.input(InputEvent::Connected {
            id: "r".into(),
            name: "TX16S".into(),
            layout_known: false,
        });
        controls.input(rz(-1.0));
        let at = |at, mode: &str| PositionBinding {
            axis: GamepadAxis::LeftStickX,
            code: Some(RZ),
            at,
            command: Command::SetMode { mode: mode.into() },
            hold_ms: None,
        };
        let mut mapping = InputMapping::mode_2();
        mapping.positions = vec![
            at(-1.0, "STABILIZE"),
            at(0.0, "ALT_HOLD"),
            at(1.0, "LOITER"),
        ];
        routed_to(&mut controls, "r", mapping.clone());
        let modes = |reports: Vec<Report>| -> Vec<String> {
            reports
                .into_iter()
                .filter_map(|r| match r {
                    Report::Positioned { position, .. } => match position.command {
                        Command::SetMode { mode } => Some(mode),
                        _ => None,
                    },
                    _ => None,
                })
                .collect()
        };

        // From one end to the other, a tick in the middle.
        assert!(modes(controls.input(rz(0.0))).is_empty());
        assert!(modes(controls.due_holds(1_000_000)).is_empty());
        assert!(modes(controls.input(rz(1.0))).is_empty());
        assert!(modes(controls.due_holds(1_040_000)).is_empty());
        assert!(
            modes(controls.due_holds(1_239_000)).is_empty(),
            "LOITER has not settled; ALT_HOLD was passed"
        );
        assert_eq!(
            modes(controls.due_holds(1_240_000)),
            ["LOITER"],
            "settled: only where it stopped"
        );
        assert!(modes(controls.due_holds(5_000_000)).is_empty(), "once");

        // A deliberate move goes as soon as it has settled.
        assert!(modes(controls.input(rz(0.0))).is_empty());
        assert!(modes(controls.due_holds(6_000_000)).is_empty());
        assert_eq!(modes(controls.due_holds(6_200_000)), ["ALT_HOLD"]);

        // A settle begun under another route sends nothing.
        controls.input(rz(1.0));
        controls.due_holds(7_000_000);
        routed_to(&mut controls, "r", mapping);
        assert!(
            modes(controls.due_holds(8_000_000)).is_empty(),
            "a claim never sends a command"
        );
    }

    fn routed_to(controls: &mut Controls, id: &str, mapping: InputMapping) -> Vec<Report> {
        controls.route(
            id,
            Some(ControlRoute {
                pilot_input_method: PilotInputMethod::Automatic,
                gripper: None,
                vehicle_id: "v".into(),
                mapping,
            }),
        )
    }

    #[test]
    fn a_held_button_sends_once_held_long_enough_and_nothing_if_let_go_early() {
        use uxduck_core::domain::command::Command;
        let mut controls = Controls::default();
        controls.input(InputEvent::Connected {
            id: "g".into(),
            name: "Pad".into(),
            layout_known: true,
        });
        let mut mapping = InputMapping::mode_2();
        mapping.buttons[0].command = Command::Arm;
        mapping.buttons[0].hold_ms = Some(1_000);
        routed_to(&mut controls, "g", mapping);
        let east = |pressed| InputEvent::Button {
            id: "g".into(),
            button: Some(GamepadButton::East),
            code: None,
            pressed,
        };
        let holding = || Report::Holding {
            id: "g".into(),
            command: Command::Arm,
            hold_ms: 1_000,
        };
        let let_go = || Report::LetGo { id: "g".into() };

        assert_eq!(controls.input(east(true)), [holding()]);
        assert!(controls.due_holds(10_000_000).is_empty(), "its time starts");
        assert!(controls.due_holds(10_900_000).is_empty(), "0.9 s: not yet");
        assert_eq!(controls.input(east(false)), [let_go()], "let go early");
        assert!(controls.due_holds(12_000_000).is_empty(), "so nothing goes");

        assert_eq!(controls.input(east(true)), [holding()]);
        assert!(controls.due_holds(20_000_000).is_empty());
        assert_eq!(
            controls.due_holds(21_000_000),
            [
                let_go(),
                Report::Pressed {
                    id: "g".into(),
                    button: Some(GamepadButton::East),
                    code: None,
                }
            ],
            "held 1 s: sent once"
        );
        assert!(controls.due_holds(22_000_000).is_empty(), "and only once");
        assert!(controls.input(east(false)).is_empty(), "nothing to let go");

        // The other button sends on the press, as before.
        let north = InputEvent::Button {
            id: "g".into(),
            button: Some(GamepadButton::North),
            code: None,
            pressed: true,
        };
        assert!(matches!(
            controls.input(north).as_slice(),
            [Report::Pressed { .. }]
        ));
    }

    #[test]
    fn a_held_position_sends_once_held_and_a_new_route_drops_the_hold() {
        use uxduck_core::domain::command::Command;
        const CH5: u32 = 0x1_0034;
        let mut controls = Controls::default();
        let ch5 = |value| InputEvent::Axis {
            id: "r".into(),
            axis: None,
            code: Some(CH5),
            value,
        };
        controls.input(InputEvent::Connected {
            id: "r".into(),
            name: "TBS Joystick".into(),
            layout_known: false,
        });
        controls.input(ch5(-1.0));
        let mut mapping = InputMapping::mode_2();
        mapping.positions = vec![PositionBinding {
            axis: GamepadAxis::LeftStickX,
            code: Some(CH5),
            at: 1.0,
            command: Command::ArmToggle,
            hold_ms: Some(2_000),
        }];
        routed_to(&mut controls, "r", mapping.clone());
        assert!(matches!(
            controls.input(ch5(1.0)).as_slice(),
            [Report::Holding { hold_ms: 2_000, .. }]
        ));
        controls.due_holds(0);
        assert_eq!(
            controls.input(ch5(-1.0)),
            [Report::LetGo { id: "r".into() }],
            "moved out after a moment"
        );
        controls.input(ch5(1.0));
        controls.due_holds(5_000_000);
        let done = controls.due_holds(7_000_000);
        assert!(
            matches!(done.as_slice(), [Report::LetGo { .. }, Report::Positioned { position, .. }] if position.command == Command::ArmToggle),
            "{done:?}"
        );

        controls.input(ch5(-1.0));
        controls.input(ch5(1.0));
        assert_eq!(
            routed_to(&mut controls, "r", mapping)
                .into_iter()
                .filter(|r| matches!(r, Report::LetGo { .. }))
                .count(),
            1,
            "a hold begun under another route is dropped"
        );
        assert!(controls.due_holds(60_000_000).is_empty());
    }

    #[test]
    fn a_switch_is_told_from_a_stick_by_the_places_it_reads() {
        const STICK: u32 = 0x1_0030;
        const SWITCH: u32 = 0x1_0034;
        const SIX_WAY: u32 = 0x1_0035;
        const FLICKED: u32 = 0x1_0031;
        let mut controls = Controls::default();
        let axis = |code, value| InputEvent::Axis {
            id: "r".into(),
            axis: None,
            code: Some(code),
            value,
        };
        let kind = |controls: &Controls, code| {
            let inputs = controls
                .snapshot()
                .controllers
                .into_iter()
                .find(|c| c.id == "r")
                .unwrap();
            inputs
                .axes
                .into_iter()
                .find(|a| a.code == Some(code))
                .unwrap()
                .kind
        };
        for value in [0.0, 0.01, -0.01] {
            controls.input(axis(STICK, value));
            controls.input(axis(SWITCH, value - 1.0));
        }
        assert_eq!(kind(&controls, STICK), None, "jitter at rest is no move");
        assert_eq!(kind(&controls, SWITCH), None);

        // A three-position switch jumps, a stick passes through.
        for value in [0.0, 1.0, -1.0] {
            controls.input(axis(SWITCH, value));
        }
        for value in [0.1, 0.3, 0.6, 1.0] {
            controls.input(axis(STICK, value));
        }
        for value in [-1.0, -0.6, -0.2, 0.2, 0.6, 1.0] {
            controls.input(axis(SIX_WAY, value));
        }
        assert_eq!(kind(&controls, SWITCH), Some(InputKind::Switch));
        assert_eq!(kind(&controls, SIX_WAY), Some(InputKind::Switch));
        assert_eq!(kind(&controls, STICK), Some(InputKind::Continuous));

        // A stick flicked end to end in one reading looks like a switch
        // until it reads between; then it is a stick for good.
        controls.input(axis(FLICKED, 0.0));
        controls.input(axis(FLICKED, 1.0));
        assert_eq!(kind(&controls, FLICKED), Some(InputKind::Switch));
        for value in [0.9, 0.7, 0.4, 0.1] {
            controls.input(axis(FLICKED, value));
        }
        assert_eq!(kind(&controls, FLICKED), Some(InputKind::Continuous));
        controls.input(axis(FLICKED, 1.0));
        assert_eq!(kind(&controls, FLICKED), Some(InputKind::Continuous));

        // A known gamepad's stick is never judged, however it moves.
        controls.input(InputEvent::Connected {
            id: "g".into(),
            name: "Pad".into(),
            layout_known: true,
        });
        for value in [0.0, 1.0, -1.0] {
            controls.input(InputEvent::Axis {
                id: "g".into(),
                axis: Some(GamepadAxis::LeftStickY),
                code: None,
                value,
            });
        }
        let pad = controls
            .snapshot()
            .controllers
            .into_iter()
            .find(|c| c.id == "g")
            .unwrap();
        assert_eq!(pad.axes[0].kind, None);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod gripper_tests {
    use uxduck_core::domain::controller::{
        ActionBinding, GamepadAxis, GamepadButton, HeldAction, InputMapping,
    };
    use uxduck_core::domain::gripper::{GripperAction, GripperEnd, GripperWiring};
    use uxduck_core::domain::settings::PilotInputMethod;

    use super::{Controls, InputEvent, SCREEN_HOLD_US};
    use crate::runtime::ControlRoute;

    const OPEN: u32 = 1 << 15;
    const CLOSE: u32 = 1;

    fn route() -> ControlRoute {
        let mut mapping = InputMapping::mode_2();
        mapping.actions = vec![
            ActionBinding {
                button: GamepadButton::RightTrigger,
                code: None,
                action: HeldAction::Open,
                shift: None,
            },
            ActionBinding {
                button: GamepadButton::LeftTrigger,
                code: None,
                action: HeldAction::Close,
                shift: None,
            },
        ];
        ControlRoute {
            vehicle_id: "sub".into(),
            mapping,
            pilot_input_method: PilotInputMethod::ManualControl,
            gripper: Some(GripperWiring {
                actuator: 1,
                opens_at: GripperEnd::Max,
                output: 11,
                open_button: 15,
                close_button: 0,
                min_us: 1100,
                max_us: 1900,
            }),
        }
    }

    fn pad(controls: &mut Controls) {
        controls.input(InputEvent::Connected {
            id: "g".into(),
            name: "Pad".into(),
            layout_known: true,
        });
        controls.input(InputEvent::Present {
            id: "g".into(),
            axes: [
                GamepadAxis::LeftStickX,
                GamepadAxis::LeftStickY,
                GamepadAxis::RightStickX,
                GamepadAxis::RightStickY,
            ]
            .into_iter()
            .enumerate()
            .map(|(i, axis)| (u32::try_from(i).unwrap(), Some(axis), 0.0))
            .collect(),
            buttons: vec![],
        });
    }

    fn press(button: GamepadButton, pressed: bool) -> InputEvent {
        InputEvent::Button {
            id: "g".into(),
            button: Some(button),
            code: None,
            pressed,
        }
    }

    fn buttons(controls: &Controls) -> u32 {
        let [(_, _, _, _, buttons)] = controls.pilot_outputs().try_into().unwrap();
        buttons
    }

    #[test]
    fn a_talk_button_asks_nothing_of_the_vehicle_and_counts_its_presses_for_the_window() {
        let mut controls = Controls::default();
        pad(&mut controls);
        let mut talking = route();
        talking.mapping.actions.push(ActionBinding {
            button: GamepadButton::North,
            code: None,
            action: HeldAction::Talk,
            shift: None,
        });
        controls.route("g", Some(talking));
        let sticks = controls.outputs();
        assert!(controls.input(press(GamepadButton::North, true)).is_empty());
        assert_eq!(buttons(&controls), 0, "no gripper bit");
        assert_eq!(controls.outputs(), sticks, "steering is unchanged");
        let north = |controls: &Controls| {
            let [inputs] = controls.snapshot().controllers.try_into().unwrap();
            assert!(inputs.gripper.is_none());
            let state = inputs
                .buttons
                .into_iter()
                .find(|b| b.button == Some(uxduck_contracts::dto::controller::GamepadButton::North))
                .expect("the window reads it from controller/inputs");
            (state.pressed, state.presses)
        };
        assert_eq!(north(&controls), (true, 1));
        // A tap between two reads still counts.
        controls.input(press(GamepadButton::North, false));
        controls.input(press(GamepadButton::North, true));
        controls.input(press(GamepadButton::North, false));
        assert_eq!(north(&controls), (false, 2));
        controls.input(press(GamepadButton::North, false));
        assert_eq!(north(&controls), (false, 2), "only going down counts");
    }

    #[test]
    fn a_held_gripper_button_sets_its_bit_until_let_go_and_sends_no_command() {
        let mut controls = Controls::default();
        pad(&mut controls);
        controls.route("g", Some(route()));
        let sticks = controls.outputs();
        assert_eq!(buttons(&controls), 0);
        assert!(
            controls
                .input(press(GamepadButton::RightTrigger, true))
                .is_empty()
        );
        assert_eq!(buttons(&controls), OPEN);
        assert_eq!(controls.outputs(), sticks, "steering is unchanged");
        controls.input(press(GamepadButton::RightTrigger, false));
        assert_eq!(buttons(&controls), 0);
        controls.input(press(GamepadButton::LeftTrigger, true));
        assert_eq!(buttons(&controls), CLOSE);
        let [inputs] = controls.snapshot().controllers.try_into().unwrap();
        assert!(inputs.gripper.is_some());
    }

    #[test]
    fn open_and_close_together_stop_it_until_both_are_let_go() {
        let mut controls = Controls::default();
        pad(&mut controls);
        controls.route("g", Some(route()));
        controls.input(press(GamepadButton::RightTrigger, true));
        controls.input(press(GamepadButton::LeftTrigger, true));
        assert_eq!(buttons(&controls), 0);
        controls.input(press(GamepadButton::LeftTrigger, false));
        assert_eq!(buttons(&controls), 0, "still held from before: no movement");
        controls.input(press(GamepadButton::RightTrigger, false));
        controls.input(press(GamepadButton::RightTrigger, true));
        assert_eq!(buttons(&controls), OPEN);
    }

    #[test]
    fn a_button_held_through_a_new_route_needs_a_fresh_press() {
        let mut controls = Controls::default();
        pad(&mut controls);
        controls.input(press(GamepadButton::RightTrigger, true));
        controls.route("g", Some(route()));
        assert_eq!(buttons(&controls), 0);
        controls.input(press(GamepadButton::RightTrigger, false));
        controls.input(press(GamepadButton::RightTrigger, true));
        assert_eq!(buttons(&controls), OPEN);
        controls.route("g", Some(route()));
        assert_eq!(
            buttons(&controls),
            0,
            "a vehicle switch or reconnect lets go"
        );
        controls.route("g", None);
        assert!(controls.pilot_outputs().is_empty());
    }

    #[test]
    fn a_route_without_a_gripper_sends_no_bits() {
        let mut controls = Controls::default();
        pad(&mut controls);
        controls.route(
            "g",
            Some(ControlRoute {
                gripper: None,
                ..route()
            }),
        );
        controls.input(press(GamepadButton::RightTrigger, true));
        assert_eq!(buttons(&controls), 0);
    }

    #[test]
    fn the_screen_hold_expires_unless_refreshed_and_then_needs_letting_go() {
        let mut controls = Controls::default();
        pad(&mut controls);
        controls.route("g", Some(route()));
        let screen = |action, at_us| InputEvent::Gripper {
            id: "g".into(),
            action,
            at_us,
        };
        controls.input(screen(Some(GripperAction::Close), 1_000_000));
        assert_eq!(buttons(&controls), CLOSE);
        controls.input(screen(Some(GripperAction::Close), 1_400_000));
        assert!(!controls.expire_screen_holds(1_400_000 + SCREEN_HOLD_US - 1));
        assert_eq!(buttons(&controls), CLOSE);
        assert!(controls.expire_screen_holds(1_400_000 + SCREEN_HOLD_US));
        assert_eq!(buttons(&controls), 0);
        controls.input(screen(Some(GripperAction::Close), 2_000_000));
        assert_eq!(buttons(&controls), 0, "a late refresh does not resume it");
        controls.input(screen(None, 2_100_000));
        controls.input(screen(Some(GripperAction::Open), 2_200_000));
        assert_eq!(buttons(&controls), OPEN);
        controls.route("g", Some(route()));
        assert_eq!(
            buttons(&controls),
            0,
            "a new route lets go of the screen too"
        );
    }

    #[test]
    fn screen_and_button_asking_opposite_ways_stop_it() {
        let mut controls = Controls::default();
        pad(&mut controls);
        controls.route("g", Some(route()));
        controls.input(press(GamepadButton::RightTrigger, true));
        controls.input(InputEvent::Gripper {
            id: "g".into(),
            action: Some(GripperAction::Close),
            at_us: 0,
        });
        assert_eq!(buttons(&controls), 0);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod gimbal_tests {
    use uxduck_core::domain::controller::{
        AxisBinding, GamepadAxis, GamepadButton, GimbalAxis, GimbalAxisBinding, InputMapping,
    };
    use uxduck_core::domain::settings::PilotInputMethod;

    use super::{Controls, GIMBAL_FULL_RATE_RAD_S, InputEvent, KEYBOARD_GIMBAL_AXES, Report};
    use crate::runtime::{ControlRoute, GimbalRoute};

    #[allow(clippy::cast_possible_truncation, reason = "a constant")]
    const FULL: f32 = GIMBAL_FULL_RATE_RAD_S as f32;

    fn gimbal() -> GimbalRoute {
        GimbalRoute {
            vehicle_id: "copter".into(),
            component_id: 1,
            device_id: 1,
        }
    }

    fn route(mapping: InputMapping) -> ControlRoute {
        ControlRoute {
            vehicle_id: "copter".into(),
            mapping,
            pilot_input_method: PilotInputMethod::ManualControl,
            gripper: None,
        }
    }

    /// Mode 2 with the right trigger as the gimbal's pitch and the left as
    /// its yaw, beside the direction pad.
    fn with_axes() -> InputMapping {
        let on = |axis| AxisBinding {
            axis,
            code: None,
            inverted: false,
            calibration: None,
        };
        InputMapping {
            gimbal: vec![
                GimbalAxisBinding {
                    control: GimbalAxis::Pitch,
                    input: on(GamepadAxis::RightZ),
                },
                GimbalAxisBinding {
                    control: GimbalAxis::Yaw,
                    input: on(GamepadAxis::LeftZ),
                },
            ],
            ..InputMapping::mode_2()
        }
    }

    fn pad(controls: &mut Controls, mapping: InputMapping) {
        controls.input(InputEvent::Connected {
            id: "g".into(),
            name: "Pad".into(),
            layout_known: true,
        });
        controls.route("g", Some(route(mapping)));
    }

    fn press(button: GamepadButton, pressed: bool) -> InputEvent {
        InputEvent::Button {
            id: "g".into(),
            button: Some(button),
            code: None,
            pressed,
        }
    }

    fn axis(axis: GamepadAxis, value: f64) -> InputEvent {
        InputEvent::Axis {
            id: "g".into(),
            axis: Some(axis),
            code: None,
            value,
        }
    }

    fn asked() -> Vec<Report> {
        vec![Report::GimbalAsked { id: "g".into() }]
    }

    fn let_go() -> Vec<Report> {
        vec![Report::GimbalLetGo { id: "g".into() }]
    }

    /// The rates of a tick, each in hundredths of a full push.
    fn rates(controls: &mut Controls) -> Vec<[i32; 2]> {
        #[allow(clippy::cast_possible_truncation, reason = "a rate within ±1")]
        controls
            .gimbal_outputs()
            .rates
            .into_iter()
            .map(|(to, rates)| {
                assert_eq!(to, gimbal());
                rates.map(|rate| (rate / FULL * 100.0).round() as i32)
            })
            .collect()
    }

    #[test]
    fn a_held_button_asks_first_and_turns_only_the_gimbal_the_engine_grants() {
        let mut controls = Controls::default();
        pad(&mut controls, InputMapping::mode_2().with_gimbal_pad());
        assert_eq!(controls.gimbal_outputs(), super::GimbalOutputs::default());

        // A gimbal button asks nothing of the vehicle as a command.
        assert!(
            controls
                .input(press(GamepadButton::DpadUp, true))
                .is_empty()
        );
        let first = controls.gimbal_outputs();
        assert_eq!(first.reports, asked());
        assert!(first.rates.is_empty(), "nothing goes until the engine says");
        assert_eq!(controls.gimbal_outputs(), super::GimbalOutputs::default());

        controls.steer_gimbal("g", Some(gimbal()));
        assert_eq!(rates(&mut controls), [[100, 0]], "up at the full rate");
        controls.input(press(GamepadButton::DpadRight, true));
        assert_eq!(rates(&mut controls), [[100, 100]]);
        controls.input(press(GamepadButton::DpadUp, false));
        controls.input(press(GamepadButton::DpadDown, true));
        assert_eq!(rates(&mut controls), [[-100, 100]]);
        assert!(controls.gimbal_stops().is_empty(), "still turning");

        // Opposite ways together ask for nothing, which is letting go.
        controls.input(press(GamepadButton::DpadLeft, true));
        controls.input(press(GamepadButton::DpadRight, true));
        controls.input(press(GamepadButton::DpadUp, true));
        let stopped = controls.gimbal_outputs();
        assert_eq!(stopped.reports, let_go());
        assert!(stopped.rates.is_empty());
        assert_eq!(controls.gimbal_stops(), [gimbal()]);
        assert!(controls.gimbal_stops().is_empty(), "told to stop once");
    }

    #[test]
    fn letting_go_stops_the_gimbal_and_the_next_push_asks_again() {
        let mut controls = Controls::default();
        pad(&mut controls, InputMapping::mode_2().with_gimbal_pad());
        controls.input(press(GamepadButton::DpadLeft, true));
        assert_eq!(controls.gimbal_outputs().reports, asked());
        controls.steer_gimbal("g", Some(gimbal()));
        assert_eq!(rates(&mut controls), [[0, -100]]);

        controls.input(press(GamepadButton::DpadLeft, false));
        let after = controls.gimbal_outputs();
        assert_eq!(after.reports, let_go());
        assert!(after.rates.is_empty());
        assert_eq!(controls.gimbal_stops(), [gimbal()]);

        // What it may turn ended with the push: the next asks anew.
        controls.input(press(GamepadButton::DpadLeft, true));
        let again = controls.gimbal_outputs();
        assert_eq!(again.reports, asked());
        assert!(again.rates.is_empty());
        // Let go before the engine answered: nothing was sent, so there
        // is nothing to stop, and a late answer turns nothing.
        controls.input(press(GamepadButton::DpadLeft, false));
        assert_eq!(controls.gimbal_outputs().reports, let_go());
        assert!(controls.gimbal_stops().is_empty());
        controls.steer_gimbal("g", Some(gimbal()));
        assert_eq!(controls.gimbal_outputs(), super::GimbalOutputs::default());
        assert!(controls.gimbal_stops().is_empty());
    }

    #[test]
    fn an_axis_turns_it_as_far_as_it_is_pushed_once_it_has_been_centred() {
        let mut controls = Controls::default();
        controls.input(InputEvent::Connected {
            id: "g".into(),
            name: "Pad".into(),
            layout_known: true,
        });
        // Off its centre when the route begins: it turns nothing until it
        // has been at its centre once, as a held button needs a new press.
        controls.input(axis(GamepadAxis::RightZ, 0.8));
        controls.route("g", Some(route(with_axes())));
        assert_eq!(controls.gimbal_outputs(), super::GimbalOutputs::default());
        controls.input(axis(GamepadAxis::RightZ, 0.3));
        assert_eq!(controls.gimbal_outputs(), super::GimbalOutputs::default());
        controls.input(axis(GamepadAxis::RightZ, 0.0));
        assert_eq!(controls.gimbal_outputs(), super::GimbalOutputs::default());

        // Half way past the deadzone is half the rate, down negative.
        controls.input(axis(GamepadAxis::RightZ, -0.525));
        assert_eq!(controls.gimbal_outputs().reports, asked());
        controls.steer_gimbal("g", Some(gimbal()));
        assert_eq!(rates(&mut controls), [[-50, 0]]);
        controls.input(axis(GamepadAxis::LeftZ, 1.0));
        assert_eq!(rates(&mut controls), [[-50, 100]]);
        // Inside the deadzone is the centre.
        controls.input(axis(GamepadAxis::RightZ, 0.04));
        assert_eq!(rates(&mut controls), [[0, 100]]);
        // An axis and a button the same way are no faster than either.
        controls.input(press(GamepadButton::DpadRight, true));
        assert_eq!(rates(&mut controls), [[0, 100]]);
        controls.input(press(GamepadButton::DpadRight, false));
        controls.input(axis(GamepadAxis::LeftZ, 0.0));
        assert_eq!(controls.gimbal_outputs().reports, let_go());
        assert_eq!(controls.gimbal_stops(), [gimbal()]);
    }

    #[test]
    fn a_button_held_through_a_new_route_needs_a_fresh_press() {
        let mut controls = Controls::default();
        pad(&mut controls, InputMapping::mode_2().with_gimbal_pad());
        controls.input(press(GamepadButton::DpadUp, true));
        assert_eq!(controls.gimbal_outputs().reports, asked());
        controls.steer_gimbal("g", Some(gimbal()));
        assert_eq!(rates(&mut controls), [[100, 0]]);

        // The route changes, as when its mapping is edited or its target
        // switches: the gimbal stops, and the button still down does
        // nothing under the new route.
        let reports = controls.route("g", Some(route(InputMapping::mode_2().with_gimbal_pad())));
        assert!(reports.contains(&Report::GimbalLetGo { id: "g".into() }));
        assert_eq!(controls.gimbal_stops(), [gimbal()]);
        assert_eq!(controls.gimbal_outputs(), super::GimbalOutputs::default());
        controls.steer_gimbal("g", Some(gimbal()));
        assert_eq!(controls.gimbal_outputs(), super::GimbalOutputs::default());
        controls.input(press(GamepadButton::DpadUp, false));
        controls.input(press(GamepadButton::DpadUp, true));
        assert_eq!(controls.gimbal_outputs().reports, asked());
    }

    #[test]
    fn the_gimbal_stops_when_the_route_ends_or_the_controller_goes() {
        // The lease ends, the window goes or its input stops: no route.
        let mut controls = Controls::default();
        pad(&mut controls, InputMapping::mode_2().with_gimbal_pad());
        controls.input(press(GamepadButton::DpadDown, true));
        controls.gimbal_outputs();
        controls.steer_gimbal("g", Some(gimbal()));
        assert_eq!(rates(&mut controls), [[-100, 0]]);
        assert_eq!(controls.route("g", None), let_go());
        assert_eq!(controls.gimbal_stops(), [gimbal()]);
        assert_eq!(controls.gimbal_outputs(), super::GimbalOutputs::default());

        // Unplugged while it turns.
        let mut controls = Controls::default();
        pad(&mut controls, InputMapping::mode_2().with_gimbal_pad());
        controls.input(press(GamepadButton::DpadDown, true));
        controls.gimbal_outputs();
        controls.steer_gimbal("g", Some(gimbal()));
        assert_eq!(rates(&mut controls), [[-100, 0]]);
        assert_eq!(
            controls.input(InputEvent::Disconnected { id: "g".into() }),
            [
                Report::GimbalLetGo { id: "g".into() },
                Report::Disconnected { id: "g".into() }
            ]
        );
        assert_eq!(controls.gimbal_stops(), [gimbal()]);

        // Input is read no more at all, as when the daemon stops.
        let mut controls = Controls::default();
        pad(&mut controls, InputMapping::mode_2().with_gimbal_pad());
        controls.input(press(GamepadButton::DpadDown, true));
        controls.gimbal_outputs();
        controls.steer_gimbal("g", Some(gimbal()));
        assert_eq!(rates(&mut controls), [[-100, 0]]);
        assert_eq!(controls.gimbal_stops_all(), [gimbal()]);
        assert!(controls.gimbal_stops_all().is_empty());
    }

    #[test]
    fn a_gimbal_the_engine_takes_back_stops_and_waits_for_a_fresh_push() {
        let mut controls = Controls::default();
        pad(&mut controls, InputMapping::mode_2().with_gimbal_pad());
        controls.input(press(GamepadButton::DpadRight, true));
        controls.gimbal_outputs();
        controls.steer_gimbal("g", Some(gimbal()));
        assert_eq!(rates(&mut controls), [[0, 100]]);

        // Another has the gimbal now, or a command for it goes first.
        controls.steer_gimbal("g", None);
        assert_eq!(controls.gimbal_stops(), [gimbal()]);
        assert_eq!(
            controls.gimbal_outputs(),
            super::GimbalOutputs::default(),
            "still pushed: nothing is sent and nothing asked"
        );
        controls.input(press(GamepadButton::DpadRight, false));
        assert_eq!(controls.gimbal_outputs().reports, let_go());
        assert!(controls.gimbal_stops().is_empty(), "it was told already");
        controls.input(press(GamepadButton::DpadRight, true));
        assert_eq!(controls.gimbal_outputs().reports, asked());
    }

    #[test]
    fn the_keyboards_gimbal_keys_are_let_go_when_its_frames_stop() {
        let [pitch, yaw] = KEYBOARD_GIMBAL_AXES;
        let on = |axis| AxisBinding {
            axis,
            code: None,
            inverted: false,
            calibration: None,
        };
        let mapping = InputMapping {
            deadzone: 0.0,
            actions: Vec::new(),
            gimbal: vec![
                GimbalAxisBinding {
                    control: GimbalAxis::Pitch,
                    input: on(pitch),
                },
                GimbalAxisBinding {
                    control: GimbalAxis::Yaw,
                    input: on(yaw),
                },
            ],
            ..InputMapping::mode_2()
        };
        let frame = |at_us| InputEvent::Keyboard {
            id: "g".into(),
            axes: [0.0, 0.0, 0.0, -1.0],
            at_us,
        };
        let mut controls = Controls::default();
        controls.input(frame(0));
        controls.route("g", Some(route(mapping)));
        controls.input(frame(40_000));
        controls.input(axis(pitch, 1.0));
        controls.input(axis(yaw, -1.0));
        assert_eq!(controls.gimbal_outputs().reports, asked());
        controls.steer_gimbal("g", Some(gimbal()));
        assert_eq!(rates(&mut controls), [[100, -100]]);

        // The page stops sending, as a window hidden or closed does.
        assert!(controls.stale_keyboard_inputs(539_999).is_empty());
        let centred = controls.stale_keyboard_inputs(540_000);
        assert_eq!(centred, [axis(pitch, 0.0), axis(yaw, 0.0)]);
        for event in centred {
            controls.input(event);
        }
        assert_eq!(controls.gimbal_outputs().reports, let_go());
        assert_eq!(controls.gimbal_stops(), [gimbal()]);
        assert!(controls.stale_keyboard_inputs(600_000).is_empty());
    }

    #[test]
    fn an_axis_shared_with_a_stick_turns_nothing() {
        // An old or hand-made mapping whose gimbal pitch reads the roll
        // stick: flying must never turn the gimbal.
        let mut mapping = with_axes();
        mapping.gimbal[0].input.axis = GamepadAxis::RightStickX;
        let mut controls = Controls::default();
        pad(&mut controls, mapping);
        controls.input(axis(GamepadAxis::RightStickX, 1.0));
        assert_eq!(controls.gimbal_outputs(), super::GimbalOutputs::default());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod camera_tests {
    use uxduck_core::domain::controller::{
        ActionBinding, AxisBinding, CameraAxis, CameraAxisBinding, GamepadAxis, GamepadButton,
        HeldAction, InputMapping,
    };
    use uxduck_core::domain::settings::PilotInputMethod;

    use super::{CAMERA_REPEAT_MS, Controls, InputEvent, Report};
    use crate::runtime::ControlRoute;

    /// Mode 2 with Zoom in and Focus far held on the bumpers and the right
    /// trigger as the zoom axis.
    fn camera_pad() -> InputMapping {
        let held = |button, action| ActionBinding {
            button,
            code: None,
            action,
            shift: None,
        };
        InputMapping {
            actions: vec![
                held(GamepadButton::RightTrigger, HeldAction::ZoomIn),
                held(GamepadButton::LeftTrigger, HeldAction::FocusFar),
            ],
            camera: vec![CameraAxisBinding {
                control: CameraAxis::Zoom,
                input: AxisBinding {
                    axis: GamepadAxis::RightZ,
                    code: None,
                    inverted: false,
                    calibration: None,
                },
            }],
            ..InputMapping::mode_2()
        }
    }

    fn pad(controls: &mut Controls) {
        controls.input(InputEvent::Connected {
            id: "g".into(),
            name: "Pad".into(),
            layout_known: true,
        });
        controls.route(
            "g",
            Some(ControlRoute {
                vehicle_id: "copter".into(),
                mapping: camera_pad(),
                pilot_input_method: PilotInputMethod::ManualControl,
                gripper: None,
            }),
        );
    }

    fn press(button: GamepadButton, pressed: bool) -> InputEvent {
        InputEvent::Button {
            id: "g".into(),
            button: Some(button),
            code: None,
            pressed,
        }
    }

    fn drive(drive: [f32; 2]) -> Vec<Report> {
        vec![Report::CameraDrive {
            id: "g".into(),
            drive,
        }]
    }

    #[test]
    fn a_held_zoom_is_said_at_once_again_while_held_and_zero_when_let_go() {
        let mut controls = Controls::default();
        pad(&mut controls);
        assert!(controls.camera_outputs(0).is_empty());
        controls.input(press(GamepadButton::RightTrigger, true));
        assert_eq!(controls.camera_outputs(0), drive([1.0, 0.0]));
        assert!(controls.camera_outputs(100_000).is_empty(), "said once");
        let again = CAMERA_REPEAT_MS * 1_000;
        assert_eq!(controls.camera_outputs(again), drive([1.0, 0.0]));
        controls.input(press(GamepadButton::LeftTrigger, true));
        assert_eq!(controls.camera_outputs(again + 1), drive([1.0, 1.0]));
        controls.input(press(GamepadButton::RightTrigger, false));
        controls.input(press(GamepadButton::LeftTrigger, false));
        assert_eq!(controls.camera_outputs(again + 2), drive([0.0, 0.0]));
        assert!(
            controls.camera_outputs(10 * again).is_empty(),
            "nothing held"
        );
    }

    #[test]
    fn the_zoom_axis_goes_in_quarters_and_only_once_it_has_been_centred() {
        let mut controls = Controls::default();
        controls.input(InputEvent::Connected {
            id: "g".into(),
            name: "Pad".into(),
            layout_known: true,
        });
        let squeeze = |value| InputEvent::Axis {
            id: "g".into(),
            axis: Some(GamepadAxis::RightZ),
            code: None,
            value,
        };
        controls.input(squeeze(0.9));
        pad(&mut controls);
        assert!(controls.camera_outputs(0).is_empty(), "not centred yet");
        controls.input(squeeze(0.0));
        assert!(controls.camera_outputs(1).is_empty());
        controls.input(squeeze(1.0));
        assert_eq!(controls.camera_outputs(2), drive([1.0, 0.0]));
        controls.input(squeeze(0.98));
        assert!(controls.camera_outputs(3).is_empty(), "the same quarter");
        controls.input(squeeze(-0.55));
        let [zoom, focus] = match controls.camera_outputs(4).as_slice() {
            [Report::CameraDrive { drive, .. }] => *drive,
            other => panic!("{other:?}"),
        };
        assert!((zoom + 0.5).abs() < 1e-6 && focus == 0.0, "{zoom}");
    }

    #[test]
    fn a_zoom_held_when_the_route_ends_or_the_controller_goes_is_owed_its_zero() {
        let mut controls = Controls::default();
        pad(&mut controls);
        controls.input(press(GamepadButton::RightTrigger, true));
        assert_eq!(controls.camera_outputs(0), drive([1.0, 0.0]));
        assert_eq!(controls.route("g", None), drive([0.0, 0.0]));
        assert!(controls.camera_outputs(1).is_empty());

        pad(&mut controls);
        controls.input(press(GamepadButton::RightTrigger, false));
        controls.input(press(GamepadButton::RightTrigger, true));
        assert_eq!(controls.camera_outputs(2), drive([1.0, 0.0]));
        let gone = controls.input(InputEvent::Disconnected { id: "g".into() });
        assert!(
            gone.contains(&Report::CameraDrive {
                id: "g".into(),
                drive: [0.0, 0.0]
            }),
            "{gone:?}"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod navigate_tests {
    use uxduck_core::domain::command::Command;
    use uxduck_core::domain::controller::{
        ActionBinding, ButtonBinding, ButtonGesture, GamepadAxis, GamepadButton, HeldAction,
        InputMapping,
    };
    use uxduck_core::domain::settings::PilotInputMethod;

    use super::{Controls, InputEvent, NAVIGATE_LEASE_US, Report};
    use crate::runtime::{ControlRoute, GimbalRoute};

    /// Mode 2 with the direction pad on the gimbal, Select as Navigate and
    /// South held for two seconds to land.
    fn mapping() -> InputMapping {
        let mut mapping = InputMapping::mode_2().with_gimbal_pad();
        mapping.actions.push(ActionBinding {
            button: GamepadButton::Select,
            code: None,
            action: HeldAction::Navigate,
            shift: None,
        });
        mapping.buttons.push(ButtonBinding {
            button: GamepadButton::South,
            code: None,
            command: Command::Land,
            hold_ms: Some(2_000),
            gesture: ButtonGesture::Press,
            shift: None,
            repeat_ms: None,
        });
        mapping
    }

    fn routed() -> Controls {
        let mut controls = Controls::default();
        controls.input(InputEvent::Connected {
            id: "g".into(),
            name: "Pad".into(),
            layout_known: true,
        });
        controls.route(
            "g",
            Some(ControlRoute {
                vehicle_id: "copter".into(),
                mapping: mapping(),
                pilot_input_method: PilotInputMethod::ManualControl,
                gripper: None,
            }),
        );
        controls
    }

    fn press(button: GamepadButton, pressed: bool) -> InputEvent {
        InputEvent::Button {
            id: "g".into(),
            button: Some(button),
            code: None,
            pressed,
        }
    }

    fn navigate(on: bool, at_us: u64) -> InputEvent {
        InputEvent::Navigate {
            id: "g".into(),
            on,
            at_us,
        }
    }

    fn tap(controls: &mut Controls, button: GamepadButton) -> Vec<Report> {
        let mut reports = controls.input(press(button, true));
        reports.extend(controls.input(press(button, false)));
        reports
    }

    fn presses(controls: &Controls, button: GamepadButton) -> u32 {
        controls.snapshot().controllers[0]
            .buttons
            .iter()
            .find(|b| b.button == Some(button.into()))
            .map_or(0, |b| b.presses)
    }

    fn turning(controls: &mut Controls) -> bool {
        let outputs = controls.gimbal_outputs();
        outputs
            .reports
            .iter()
            .any(|r| *r == Report::GimbalAsked { id: "g".into() })
            || !outputs.rates.is_empty()
    }

    #[test]
    fn navigating_pauses_the_direction_pad_south_and_east_and_the_sticks_still_fly() {
        let mut controls = routed();
        assert!(controls.input(navigate(true, 0)).is_empty());
        assert!(controls.snapshot().controllers[0].navigating);

        // East is Return in Mode 2 and South a hold: neither goes, and the
        // window still counts their presses.
        assert!(tap(&mut controls, GamepadButton::East).is_empty());
        assert!(controls.input(press(GamepadButton::South, true)).is_empty());
        assert!(controls.due_holds(3_000_000).is_empty());
        controls.input(press(GamepadButton::South, false));
        assert!(tap(&mut controls, GamepadButton::DpadUp).is_empty());
        assert_eq!(presses(&controls, GamepadButton::East), 1);
        assert_eq!(presses(&controls, GamepadButton::DpadUp), 1);
        controls.input(press(GamepadButton::DpadLeft, true));
        assert!(!turning(&mut controls), "the direction pad turns no gimbal");
        controls.input(press(GamepadButton::DpadLeft, false));

        // North still sends its mode, and the sticks fly as before.
        assert_eq!(
            tap(&mut controls, GamepadButton::North),
            [Report::Pressed {
                id: "g".into(),
                button: Some(GamepadButton::North),
                code: None,
            }]
        );
        let before = controls.outputs();
        controls.input(InputEvent::Axis {
            id: "g".into(),
            axis: Some(GamepadAxis::RightStickX),
            code: None,
            value: 0.8,
        });
        assert_ne!(controls.outputs(), before, "the stick still rolls");

        // Ended, East is Return again.
        controls.input(navigate(false, 0));
        assert!(!controls.snapshot().controllers[0].navigating);
        assert_eq!(
            controls.input(press(GamepadButton::East, true)),
            [Report::Pressed {
                id: "g".into(),
                button: Some(GamepadButton::East),
                code: None,
            }]
        );
    }

    #[test]
    fn starting_navigate_stops_what_its_buttons_hold_and_ending_it_needs_a_fresh_press() {
        let mut controls = routed();
        controls.input(press(GamepadButton::DpadUp, true));
        assert!(turning(&mut controls));
        controls.steer_gimbal(
            "g",
            Some(GimbalRoute {
                vehicle_id: "copter".into(),
                component_id: 1,
                device_id: 1,
            }),
        );
        assert!(turning(&mut controls));

        // A hold on South under way is let go.
        controls.input(press(GamepadButton::South, true));
        assert_eq!(
            controls.input(navigate(true, 0)),
            [Report::LetGo { id: "g".into() }]
        );
        assert!(controls.due_holds(3_000_000).is_empty());
        let outputs = controls.gimbal_outputs();
        assert_eq!(outputs.reports, [Report::GimbalLetGo { id: "g".into() }]);
        assert_eq!(
            controls.gimbal_stops().len(),
            1,
            "the gimbal is told to stop"
        );

        // Ended with the button still down: nothing until it is pressed again.
        controls.input(navigate(false, 0));
        assert!(!turning(&mut controls));
        assert!(controls.due_holds(6_000_000).is_empty());
        controls.input(press(GamepadButton::DpadUp, false));
        controls.input(press(GamepadButton::DpadUp, true));
        assert!(turning(&mut controls));
    }

    #[test]
    fn navigate_not_refreshed_gives_the_buttons_back() {
        let mut controls = routed();
        controls.input(navigate(true, 0));
        controls.input(navigate(true, NAVIGATE_LEASE_US - 1));
        controls.expire_screen_holds(NAVIGATE_LEASE_US + 1);
        assert!(
            controls.snapshot().controllers[0].navigating,
            "refreshed in time"
        );
        controls.expire_screen_holds(2 * NAVIGATE_LEASE_US);
        assert!(!controls.snapshot().controllers[0].navigating);
        assert_eq!(
            tap(&mut controls, GamepadButton::East).len(),
            1,
            "Return again"
        );
    }

    #[test]
    fn a_window_action_asks_nothing_of_the_vehicle() {
        let mut controls = routed();
        let sticks = controls.outputs();
        for action in [
            HeldAction::NextVehicle,
            HeldAction::PreviousVehicle,
            HeldAction::CockpitTab,
            HeldAction::CameraTab,
            HeldAction::MapTab,
            HeldAction::NextCamera,
            HeldAction::CameraFullWindow,
            HeldAction::ShowOnMap,
            HeldAction::Navigate,
        ] {
            assert!(action.for_window());
            assert!(action.gripper().is_none() && action.gimbal().is_none());
        }
        assert!(tap(&mut controls, GamepadButton::Select).is_empty());
        assert!(
            !controls.snapshot().controllers[0].navigating,
            "the window starts it"
        );
        assert_eq!(presses(&controls, GamepadButton::Select), 1);
        assert_eq!(controls.outputs(), sticks);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod gesture_tests {
    //! Gestures, Shift layers and repeats (S418).
    use super::*;
    use uxduck_core::domain::camera::CameraAction;
    use uxduck_core::domain::controller::{HeldAction, ShiftButton};
    use uxduck_core::domain::settings::PilotInputMethod;

    const LB: ShiftButton = ShiftButton {
        button: GamepadButton::LeftTrigger,
        code: None,
    };

    fn binding(button: GamepadButton, command: Command) -> ButtonBinding {
        ButtonBinding {
            button,
            code: None,
            command,
            hold_ms: None,
            gesture: ButtonGesture::Press,
            shift: None,
            repeat_ms: None,
        }
    }

    fn photo() -> Command {
        Command::Camera {
            component_id: None,
            device_id: None,
            action: CameraAction::TakePhoto,
        }
    }

    fn zoom() -> Command {
        Command::Camera {
            component_id: None,
            device_id: None,
            action: CameraAction::ZoomStep { steps: 1 },
        }
    }

    fn shift(button: GamepadButton) -> ActionBinding {
        ActionBinding {
            button,
            code: None,
            action: HeldAction::Shift,
            shift: None,
        }
    }

    /// South: a photo on a press, Return on a long press of 1 s and Land on
    /// a double press; LB a Shift, with Shift + South as Arm.
    fn mapping() -> InputMapping {
        let mut mapping = InputMapping::mode_2();
        mapping.buttons = vec![
            binding(GamepadButton::South, photo()),
            ButtonBinding {
                hold_ms: Some(1_000),
                ..binding(GamepadButton::South, Command::Rtl)
            },
            ButtonBinding {
                gesture: ButtonGesture::DoublePress,
                ..binding(GamepadButton::South, Command::Land)
            },
            ButtonBinding {
                shift: Some(LB),
                ..binding(GamepadButton::South, Command::Arm)
            },
            binding(GamepadButton::East, Command::Rtl),
        ];
        mapping.actions = vec![shift(GamepadButton::LeftTrigger)];
        assert_eq!(mapping.button_problem(), None);
        mapping
    }

    fn pad(mapping: InputMapping) -> Controls {
        let mut controls = Controls::default();
        controls.input(InputEvent::Connected {
            id: "g".into(),
            name: "Pad".into(),
            layout_known: true,
        });
        controls.route(
            "g",
            Some(ControlRoute {
                pilot_input_method: PilotInputMethod::Automatic,
                gripper: None,
                vehicle_id: "v".into(),
                mapping,
            }),
        );
        controls
    }

    fn press(controls: &mut Controls, button: GamepadButton, pressed: bool) -> Vec<Report> {
        controls.input(InputEvent::Button {
            id: "g".into(),
            button: Some(button),
            code: None,
            pressed,
        })
    }

    /// The commands among `reports`, as the engine would send them.
    fn sent(reports: &[Report], mapping: &InputMapping) -> Vec<Command> {
        reports
            .iter()
            .filter_map(|report| match report {
                Report::Pressed { button, code, .. } => {
                    mapping.button(*button, *code).map(|b| b.command.clone())
                }
                Report::Gesture { binding, .. } => Some(binding.command.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_press_waits_only_as_long_as_its_double_press_needs() {
        let m = mapping();
        let mut c = pad(m.clone());
        // A press: down, up, and it goes once 300 ms pass without a second.
        assert!(sent(&press(&mut c, GamepadButton::South, true), &m).is_empty());
        assert!(matches!(
            c.due_holds(1_000_000).as_slice(),
            [] | [Report::Holding { .. }]
        ));
        let up = press(&mut c, GamepadButton::South, false);
        assert_eq!(up, [Report::LetGo { id: "g".into() }], "no long press");
        assert!(
            sent(&c.due_holds(1_040_000), &m).is_empty(),
            "its window starts"
        );
        assert!(
            sent(&c.due_holds(1_300_000), &m).is_empty(),
            "260 ms: waits"
        );
        assert_eq!(sent(&c.due_holds(1_340_000), &m), [photo()], "300 ms: goes");
        assert!(sent(&c.due_holds(3_000_000), &m).is_empty(), "once");

        // A double press: the second press goes at once and nothing else.
        press(&mut c, GamepadButton::South, true);
        press(&mut c, GamepadButton::South, false);
        c.due_holds(4_000_000);
        assert_eq!(
            sent(&press(&mut c, GamepadButton::South, true), &m),
            [Command::Land]
        );
        assert!(sent(&press(&mut c, GamepadButton::South, false), &m).is_empty());
        assert!(sent(&c.due_holds(9_000_000), &m).is_empty());

        // East has only a press, so it goes on the press without waiting.
        assert_eq!(
            press(&mut c, GamepadButton::East, true),
            [Report::Pressed {
                id: "g".into(),
                button: Some(GamepadButton::East),
                code: None,
            }]
        );
    }

    #[test]
    fn a_long_press_goes_when_held_and_its_press_does_not() {
        let m = mapping();
        let mut c = pad(m.clone());
        assert_eq!(
            press(&mut c, GamepadButton::South, true),
            [Report::Holding {
                id: "g".into(),
                command: Command::Rtl,
                hold_ms: 1_000,
            }]
        );
        c.due_holds(10_000_000);
        assert!(sent(&c.due_holds(10_900_000), &m).is_empty());
        let done = c.due_holds(11_000_000);
        assert_eq!(done[0], Report::LetGo { id: "g".into() });
        assert_eq!(sent(&done, &m), [Command::Rtl]);
        assert!(press(&mut c, GamepadButton::South, false).is_empty());
        assert!(
            sent(&c.due_holds(20_000_000), &m).is_empty(),
            "no press after"
        );

        // Without a double press bound, its press goes on letting go.
        let mut m = mapping();
        m.buttons.retain(|b| b.gesture.is_press());
        let mut c = pad(m.clone());
        press(&mut c, GamepadButton::South, true);
        c.due_holds(0);
        assert_eq!(
            sent(&press(&mut c, GamepadButton::South, false), &m),
            [photo()]
        );
    }

    #[test]
    fn a_shift_gives_every_other_button_its_layer_and_sends_nothing_itself() {
        let m = mapping();
        let mut c = pad(m.clone());
        assert!(press(&mut c, GamepadButton::LeftTrigger, true).is_empty());
        let layered = press(&mut c, GamepadButton::South, true);
        assert_eq!(
            layered,
            [Report::Gesture {
                id: "g".into(),
                binding: m.buttons[3].clone(),
            }]
        );
        press(&mut c, GamepadButton::South, false);
        // East has nothing on the Shift's layer, so nothing goes.
        assert!(press(&mut c, GamepadButton::East, true).is_empty());
        press(&mut c, GamepadButton::East, false);
        assert!(press(&mut c, GamepadButton::LeftTrigger, false).is_empty());
        assert!(sent(&c.due_holds(5_000_000), &m).is_empty());
        // Without the Shift, South is its own again.
        press(&mut c, GamepadButton::South, true);
        assert_eq!(
            sent(&press(&mut c, GamepadButton::East, true), &m),
            [Command::Rtl]
        );
    }

    #[test]
    fn a_shift_let_go_mid_press_keeps_the_layer_the_press_began_on() {
        let mut m = mapping();
        m.buttons.push(ButtonBinding {
            shift: Some(LB),
            hold_ms: Some(1_000),
            ..binding(GamepadButton::North, Command::Land)
        });
        let mut c = pad(m.clone());
        press(&mut c, GamepadButton::LeftTrigger, true);
        assert!(matches!(
            press(&mut c, GamepadButton::North, true).as_slice(),
            [Report::Holding {
                command: Command::Land,
                ..
            }]
        ));
        c.due_holds(0);
        press(&mut c, GamepadButton::LeftTrigger, false);
        assert_eq!(sent(&c.due_holds(1_000_000), &m), [Command::Land]);

        // Two Shifts held at once: no layer, so nothing goes.
        m.actions.push(shift(GamepadButton::RightTrigger));
        let mut c = pad(m.clone());
        press(&mut c, GamepadButton::LeftTrigger, true);
        press(&mut c, GamepadButton::RightTrigger, true);
        assert!(press(&mut c, GamepadButton::South, true).is_empty());
        press(&mut c, GamepadButton::South, false);
        assert!(sent(&c.due_holds(5_000_000), &m).is_empty());
    }

    #[test]
    fn a_route_ending_mid_gesture_sends_nothing_of_it() {
        let m = mapping();
        let mut c = pad(m.clone());
        press(&mut c, GamepadButton::South, true);
        press(&mut c, GamepadButton::South, false);
        c.due_holds(0);
        // Its press is waiting for a second when the route ends.
        assert!(c.route("g", None).is_empty() || !c.routed());
        assert!(c.due_holds(5_000_000).is_empty());
        // And a long press under way.
        let mut c = pad(m.clone());
        press(&mut c, GamepadButton::South, true);
        c.due_holds(0);
        let ended = c.route("g", None);
        assert!(
            ended.contains(&Report::LetGo { id: "g".into() }),
            "{ended:?}"
        );
        assert!(c.due_holds(5_000_000).is_empty());
        assert!(press(&mut c, GamepadButton::South, false).is_empty());
        // Unplugged mid-gesture, the same.
        let mut c = pad(m);
        press(&mut c, GamepadButton::South, true);
        press(&mut c, GamepadButton::South, false);
        c.input(InputEvent::Disconnected { id: "g".into() });
        assert!(c.due_holds(5_000_000).is_empty());
    }

    #[test]
    fn a_step_repeats_while_held_and_stops_when_let_go() {
        let mut m = InputMapping::mode_2();
        m.buttons = vec![ButtonBinding {
            repeat_ms: Some(250),
            ..binding(GamepadButton::DpadUp, zoom())
        }];
        assert_eq!(m.button_problem(), None);
        let mut c = pad(m.clone());
        assert_eq!(
            sent(&press(&mut c, GamepadButton::DpadUp, true), &m),
            [zoom()]
        );
        assert!(
            sent(&c.due_holds(1_000_000), &m).is_empty(),
            "its time starts"
        );
        assert!(sent(&c.due_holds(1_200_000), &m).is_empty());
        assert_eq!(sent(&c.due_holds(1_250_000), &m), [zoom()]);
        assert_eq!(sent(&c.due_holds(1_500_000), &m), [zoom()]);
        // A late tick sends one, not every one it missed.
        assert_eq!(sent(&c.due_holds(2_600_000), &m), [zoom()]);
        press(&mut c, GamepadButton::DpadUp, false);
        assert!(sent(&c.due_holds(5_000_000), &m).is_empty());
    }

    /// Navigate (S420) takes the direction pad, South and East: a step
    /// repeating on one stops, and a press waiting for its double press
    /// sends nothing.
    #[test]
    fn navigate_ends_its_buttons_repeats_and_undecided_presses() {
        let mut m = mapping();
        m.buttons.push(ButtonBinding {
            repeat_ms: Some(250),
            ..binding(GamepadButton::DpadUp, zoom())
        });
        assert_eq!(m.button_problem(), None);
        let mut c = pad(m.clone());
        assert_eq!(
            sent(&press(&mut c, GamepadButton::DpadUp, true), &m),
            [zoom()]
        );
        c.due_holds(1_000_000);
        press(&mut c, GamepadButton::South, true);
        press(&mut c, GamepadButton::South, false);
        c.input(InputEvent::Navigate {
            id: "g".into(),
            on: true,
            at_us: 1_000_000,
        });
        assert!(sent(&c.due_holds(1_100_000), &m).is_empty());
        assert!(sent(&c.due_holds(3_000_000), &m).is_empty());
    }

    #[test]
    fn a_held_action_on_a_shift_layer_holds_while_its_button_does() {
        let mut m = InputMapping::mode_2();
        m.actions = vec![
            shift(GamepadButton::LeftTrigger),
            ActionBinding {
                button: GamepadButton::DpadUp,
                code: None,
                action: HeldAction::GimbalUp,
                shift: Some(LB),
            },
        ];
        m.buttons = vec![binding(GamepadButton::DpadUp, Command::Rtl)];
        assert_eq!(m.button_problem(), None);
        let mut c = pad(m.clone());
        press(&mut c, GamepadButton::LeftTrigger, true);
        assert!(
            press(&mut c, GamepadButton::DpadUp, true).is_empty(),
            "no command"
        );
        assert_eq!(
            c.gimbal_outputs().reports,
            [Report::GimbalAsked { id: "g".into() }]
        );
        // Shift let go: Up still holds the gimbal's action it was pressed for.
        press(&mut c, GamepadButton::LeftTrigger, false);
        assert!(c.gimbal_outputs().reports.is_empty());
        press(&mut c, GamepadButton::DpadUp, false);
        assert_eq!(
            c.gimbal_outputs().reports,
            [Report::GimbalLetGo { id: "g".into() }]
        );
        // Without the Shift, Up sends its command.
        assert_eq!(
            sent(&press(&mut c, GamepadButton::DpadUp, true), &m),
            [Command::Rtl]
        );
    }
}
