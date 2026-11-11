//! Controller input (ADR-0005): each controller's latest values, whether
//! its sticks are centred, which button went down and which position an
//! axis moved into, and the pilot input for the routes the engine grants.
//! The installed daemon feeds it from gilrs, the browser's daemon from the
//! page's Gamepad API; both turn [`Report`]s into engine calls and send
//! [`Controls::pilot_outputs`] at 25 Hz.

use std::collections::BTreeMap;

use uxduck_contracts::dto::controller::{
    AxisValue, ButtonState, ControllerInputs, ControllerInputsResult, InputKind,
};
use uxduck_core::domain::command::Command;
use uxduck_core::domain::controller::{
    AxisBinding, GamepadAxis, GamepadButton, InputMapping, PositionBinding, Sticks,
};
use uxduck_core::domain::gripper::GripperAction;
use uxduck_core::domain::settings::PilotInputMethod;

use crate::runtime::ControlRoute;

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
}

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
}

/// What is being held for its command.
#[derive(Clone, Debug, PartialEq)]
enum Held {
    Button {
        button: Option<GamepadButton>,
        code: Option<u32>,
    },
    Position(PositionBinding),
}

impl Held {
    /// What goes once it has been held.
    fn report(self, id: &str) -> Report {
        let id = id.to_owned();
        match self {
            Self::Button { button, code } => Report::Pressed { id, button, code },
            Self::Position(position) => Report::Positioned { id, position },
        }
    }
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
            .or_else(|| self.layout_known.then_some(0.0))
    }

    /// Ends the hold of `held`, if one is under way: whether there was.
    fn let_go(&mut self, held: &Held) -> bool {
        let before = self.holds.len();
        self.holds.retain(|pending| pending.held != *held);
        self.holds.len() != before
    }

    /// The route's gripper buttons held now, by code and name.
    fn pressed_actions(&self) -> Vec<(Option<u32>, Option<GamepadButton>)> {
        let Some(route) = self.route.as_ref() else {
            return Vec::new();
        };
        self.buttons
            .iter()
            .filter(|((code, button), state)| {
                state.held && route.mapping.action(*button, *code).is_some()
            })
            .map(|(key, _)| *key)
            .collect()
    }

    /// The gripper action asked for from the held buttons and the screen:
    /// none while open and close are both asked for, and none after that
    /// until every gripper input is let go.
    fn settle_gripper(&mut self) {
        let mut asked: Vec<GripperAction> = self
            .route
            .as_ref()
            .map(|route| {
                self.buttons
                    .iter()
                    .filter(|(key, state)| state.held && !self.stale_actions.contains(key))
                    .filter_map(|((code, button), _)| route.mapping.action(*button, *code))
                    .filter_map(|binding| binding.action.gripper())
                    .collect()
            })
            .unwrap_or_default();
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

    /// Whether the axis is in each position of the route's mapping now.
    fn in_positions(&self) -> Vec<bool> {
        self.route.as_ref().map_or_else(Vec::new, |route| {
            route
                .mapping
                .positions
                .iter()
                .map(|p| self.read(&p.binding()).is_some_and(|v| p.holds(v)))
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
}

impl Controls {
    /// Controls whose inputs carry the browser's indices for codes.
    #[must_use]
    pub fn browser() -> Self {
        Self {
            devices: BTreeMap::new(),
            naming: Naming::Browser,
        }
    }

    /// Applies one input event; returns what the engine must hear.
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
                self.devices.remove(&id);
                vec![Report::Disconnected { id }]
            }
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
            } => self.button(id, button, code, pressed),
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
    /// binding asks for; letting go ends that hold.
    fn button(
        &mut self,
        id: String,
        button: Option<GamepadButton>,
        code: Option<u32>,
        pressed: bool,
    ) -> Vec<Report> {
        let device = self.devices.entry(id.clone()).or_default();
        let button = device.named(code, button);
        let key = (code, button);
        let before = device.buttons.get(&key).copied().unwrap_or_default();
        let was = before.held;
        if pressed || device.buttons.contains_key(&key) {
            device.buttons.insert(
                key,
                Button {
                    held: pressed,
                    seen: before.seen || pressed,
                    presses: before.presses.saturating_add(u32::from(pressed && !was)),
                },
            );
        }
        let action_bound = device
            .route
            .as_ref()
            .is_some_and(|route| route.mapping.action(button, code).is_some());
        if action_bound {
            if !pressed {
                device.stale_actions.retain(|held| *held != (code, button));
            }
            device.settle_gripper();
            return Vec::new();
        }
        if !pressed {
            let held = Held::Button { button, code };
            return if was && device.let_go(&held) {
                vec![Report::LetGo { id }]
            } else {
                Vec::new()
            };
        }
        if was || (button.is_none() && code.is_none()) {
            return Vec::new();
        }
        let hold = device.route.as_ref().and_then(|route| {
            let binding = route.mapping.button(button, code)?;
            Some((binding.command.clone(), binding.hold_ms?))
        });
        match hold {
            Some((command, hold_ms)) => {
                device.holds.push(Pending {
                    held: Held::Button { button, code },
                    since_us: None,
                    hold_ms,
                });
                vec![Report::Holding {
                    id,
                    command,
                    hold_ms,
                }]
            }
            None => vec![Report::Pressed { id, button, code }],
        }
    }

    /// Holds held long enough, and positions settled: each hold ends, and
    /// its command goes as a press or a move into the position would. A
    /// hold's or a settle's time starts at the first call after it began,
    /// so call this on every pilot tick.
    pub fn due_holds(&mut self, now_us: u64) -> Vec<Report> {
        let mut reports = Vec::new();
        for (id, device) in &mut self.devices {
            for held in due(&mut device.holds, now_us) {
                reports.push(Report::LetGo { id: id.clone() });
                reports.push(held.report(id));
            }
            for held in due(&mut device.settling, now_us) {
                reports.push(held.report(id));
            }
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
        }
        expired
    }

    /// Centre stale steering without releasing the page's keyboard session.
    /// Throttle stays at its chosen setting until the page changes or releases it.
    #[must_use]
    pub fn stale_keyboard_inputs(&self, now_us: u64) -> Vec<InputEvent> {
        self.devices
            .iter()
            .filter(|(_, device)| {
                device
                    .keyboard_at_us
                    .is_some_and(|at| now_us.saturating_sub(at) >= 500_000)
                    && [
                        GamepadAxis::RightStickX,
                        GamepadAxis::RightStickY,
                        GamepadAxis::LeftStickX,
                    ]
                    .iter()
                    .any(|axis| {
                        device
                            .axes
                            .get(&(None, Some(*axis)))
                            .is_some_and(|value| *value != 0.0)
                    })
            })
            .map(|(id, device)| InputEvent::Keyboard {
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
            })
            .collect()
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
                })
                .collect(),
        }
    }

    /// The engine's decision: what this controller drives now.
    pub fn route(&mut self, id: &str, route: Option<ControlRoute>) -> Vec<Report> {
        let device = self.devices.entry(id.to_owned()).or_default();
        device.route = route;
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
        let mut reports = Vec::new();
        if let_go {
            reports.push(Report::LetGo { id: id.to_owned() });
        }
        // Neutral is judged through the mapping in use.
        reports.extend(self.neutral_changed(id));
        reports
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
            },
            ActionBinding {
                button: GamepadButton::LeftTrigger,
                code: None,
                action: HeldAction::Close,
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
