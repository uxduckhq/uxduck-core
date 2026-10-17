//! Controller input (ADR-0005): each controller's latest values, whether
//! its sticks are centred, which button went down and which position an
//! axis moved into, and the pilot input for the routes the engine grants.
//! The installed daemon feeds it from gilrs, the browser's daemon from the
//! page's Gamepad API; both turn [`Report`]s into engine calls and send
//! [`Controls::pilot_outputs`] at 25 Hz.

use std::collections::BTreeMap;

use uxduck_contracts::dto::controller::{
    AxisValue, ButtonState, ControllerInputs, ControllerInputsResult,
};
use uxduck_core::domain::controller::{
    AxisBinding, GamepadAxis, GamepadButton, InputMapping, PositionBinding, Sticks,
};
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
}

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
    /// An axis moved into a position of the routed mapping.
    Positioned {
        id: String,
        position: PositionBinding,
    },
}

#[derive(Debug, Default)]
struct Device {
    layout_known: bool,
    keyboard_at_us: Option<u64>,
    /// Every axis it has or that has moved, by its code and its gamepad
    /// axis (only for a device whose layout is known: never a guess).
    axes: BTreeMap<(Option<u32>, Option<GamepadAxis>), f64>,
    /// Every button it has or that has been pressed: whether it is held,
    /// and whether it has been pressed since it was plugged in.
    buttons: BTreeMap<(Option<u32>, Option<GamepadButton>), (bool, bool)>,
    neutral: Option<bool>,
    route: Option<ControlRoute>,
    /// Whether the axis is in each position of the route's mapping, as last
    /// judged: a command goes only on moving into one.
    held: Vec<bool>,
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
                let mut reports = self.neutral_changed(&id);
                reports.extend(self.positions_entered(&id));
                reports
            }
            InputEvent::Button {
                id,
                button,
                code,
                pressed,
            } => {
                let device = self.devices.entry(id.clone()).or_default();
                let button = device.named(code, button);
                let key = (code, button);
                let (was, seen) = device.buttons.get(&key).copied().unwrap_or_default();
                if pressed || device.buttons.contains_key(&key) {
                    device.buttons.insert(key, (pressed, seen || pressed));
                }
                if pressed && !was && (button.is_some() || code.is_some()) {
                    vec![Report::Pressed { id, button, code }]
                } else {
                    Vec::new()
                }
            }
            InputEvent::Present { id, axes, buttons } => {
                let device = self.devices.entry(id.clone()).or_default();
                for (code, axis, value) in axes {
                    let axis = device.named(Some(code), axis);
                    device.axes.insert((Some(code), axis), value);
                }
                for (code, button, pressed) in buttons {
                    let button = device.named(Some(code), button);
                    device
                        .buttons
                        .entry((Some(code), button))
                        .or_insert((pressed, false));
                }
                let mut reports = self.neutral_changed(&id);
                reports.extend(self.positions_entered(&id));
                reports
            }
        }
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
                        })
                        .collect(),
                    buttons: device
                        .buttons
                        .iter()
                        .map(|(&(code, button), &(pressed, seen))| ButtonState {
                            code,
                            name: code.map_or_else(
                                || kebab(button),
                                |code| self.naming.name(code, false),
                            ),
                            number: code.and_then(|code| self.naming.number(code, false)),
                            button: button.map(Into::into),
                            pressed,
                            seen,
                        })
                        .collect(),
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
        // Neutral is judged through the mapping in use.
        self.neutral_changed(id)
    }

    /// The sticks through the mapping in use; none while an input it
    /// needs is unknown, so nothing is sent and it never counts as centred.
    fn sticks(device: &Device) -> Option<Sticks> {
        let default = InputMapping::mode_2();
        let mapping = device.route.as_ref().map_or(&default, |r| &r.mapping);
        let bindings = [mapping.roll, mapping.pitch, mapping.yaw, mapping.throttle];
        if bindings.iter().any(|b| device.read(b).is_none()) {
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
        let entered = device.route.as_ref().map_or_else(Vec::new, |route| {
            route
                .mapping
                .positions
                .iter()
                .zip(&now)
                .enumerate()
                .filter(|&(i, (_, &inside))| inside && !device.held.get(i).copied().unwrap_or(true))
                .map(|(_, (position, _))| Report::Positioned {
                    id: id.to_owned(),
                    position: position.clone(),
                })
                .collect()
        });
        device.held = now;
        entered
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
    pub fn pilot_outputs(&self) -> Vec<(String, [i16; 4], Vec<(u8, i16)>, PilotInputMethod)> {
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
            .map(|(vehicle, sticks, _, _)| (vehicle, sticks))
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

/// What the device calls an input, from the platform's code for it: on
/// macOS a HID usage page and usage (`X`, `Rz`, `Slider`, `Button 3`), on
/// Linux an evdev code. A radio in joystick mode sends its channels on these
/// in order, so the names are what someone setting one up needs.
fn input_name(code: u32, axis: bool) -> String {
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
                    PilotInputMethod::Automatic
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
        };
        let mut mapping = InputMapping::mode_2();
        mapping.positions = vec![at(-0.2, "LOITER"), at(0.2, "ALT_HOLD")];
        let routed = controls.route(
            "r",
            Some(ControlRoute {
                pilot_input_method: PilotInputMethod::Automatic,
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
        assert_eq!(positioned(controls.input(rz(0.21))), ["ALT_HOLD"]);
        assert!(
            positioned(controls.input(rz(0.19))).is_empty(),
            "still in it"
        );
        assert!(
            positioned(controls.input(rz(0.6))).is_empty(),
            "no position there"
        );
        assert_eq!(positioned(controls.input(rz(-0.2))), ["LOITER"]);
        controls.route("r", None);
        assert!(positioned(controls.input(rz(0.2))).is_empty(), "not routed");
    }
}
