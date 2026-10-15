//! Controller input (ADR-0005): gamepads through gilrs on their own thread,
//! and a fake one on a UDP port for tests. Both produce the same events for
//! the data plane, which keeps each controller's latest values, tells the
//! engine when its sticks are centred and when a button goes down, and sends
//! pilot input for the routes the engine grants. It also publishes
//! every controller's raw input for `controller/inputs` ([`inputs`]).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{LazyLock, Mutex, PoisonError};

use uxduck_contracts::dto::controller::{
    AxisValue, ButtonState, ControllerInputs, ControllerInputsResult,
};
use uxduck_core::domain::controller::{
    AxisBinding, GamepadAxis, GamepadButton, InputMapping, PositionBinding, Sticks,
};
use uxduck_core::domain::settings::PilotInputMethod;
use uxduck_engine::runtime::ControlRoute;

/// What an input device did. `code` is the platform's own code for the
/// input (absent for the fake); `axis` and `button` are the gamepad input
/// the platform reads it as, if any, a guess when the layout is not known.
#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
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
    /// list them (macOS): so all show, and drive, before they move.
    #[cfg(any(test, all(feature = "gamepad", target_os = "macos")))]
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

/// Every controller's latest input and route.
#[derive(Debug, Default)]
pub struct Controls {
    devices: BTreeMap<String, Device>,
}

impl Controls {
    /// Applies one input event; returns what the engine must hear.
    pub fn input(&mut self, event: InputEvent) -> Vec<Report> {
        match event {
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
            #[cfg(any(test, all(feature = "gamepad", target_os = "macos")))]
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
                            name: code.map_or_else(|| kebab(axis), |code| input_name(code, true)),
                            number: code.and_then(|code| input_number(code, true)),
                            axis: axis.map(Into::into),
                            value,
                        })
                        .collect(),
                    buttons: device
                        .buttons
                        .iter()
                        .map(|(&(code, button), &(pressed, seen))| ButtonState {
                            code,
                            name: code
                                .map_or_else(|| kebab(button), |code| input_name(code, false)),
                            number: code.and_then(|code| input_number(code, false)),
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

static INPUTS: LazyLock<Mutex<ControllerInputsResult>> = LazyLock::new(Mutex::default);

/// Replaces what `controller/inputs` returns; the data plane calls it after
/// each input.
pub fn publish(snapshot: ControllerInputsResult) {
    *INPUTS.lock().unwrap_or_else(PoisonError::into_inner) = snapshot;
}

/// Every controller's raw input, as last published.
pub fn inputs() -> ControllerInputsResult {
    INPUTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Reads a fake controller from UDP datagrams on loopback, for tests and
/// development without a gamepad. Each datagram is JSON:
/// `{"id":"fake","name":"Fake gamepad","connected":true,
///   "axes":{"left-stick-y":0.5},"buttons":{"east":true}}`; every field but
/// `id` is optional, and `"connected":false` unplugs it. A device whose
/// layout is not known, such as a radio, is `"layoutKnown":false` with its
/// inputs by code: `"codes":{"65587":0.4}`, `"buttonCodes":{"589825":true}`.
///
/// # Errors
/// When the port cannot be bound.
pub fn fake(port: u16, events: impl Fn(InputEvent) + Send + 'static) -> std::io::Result<()> {
    let socket = std::net::UdpSocket::bind(("127.0.0.1", port))?;
    socket.set_nonblocking(true)?;
    let socket = tokio::net::UdpSocket::from_std(socket)?;
    tokio::spawn(async move {
        let mut buffer = vec![0u8; 4096];
        let mut known = BTreeSet::new();
        while let Ok((len, _)) = socket.recv_from(&mut buffer).await {
            let Ok(message) = serde_json::from_slice::<serde_json::Value>(&buffer[..len]) else {
                continue;
            };
            for event in fake_events(&message, &mut known) {
                events(event);
            }
        }
    });
    Ok(())
}

fn fake_events(message: &serde_json::Value, known: &mut BTreeSet<String>) -> Vec<InputEvent> {
    let Some(id) = message["id"].as_str().map(str::to_owned) else {
        return Vec::new();
    };
    if message["connected"] == false {
        known.remove(&id);
        return vec![InputEvent::Disconnected { id }];
    }
    let mut events = Vec::new();
    if known.insert(id.clone()) {
        events.push(InputEvent::Connected {
            id: id.clone(),
            name: message["name"]
                .as_str()
                .unwrap_or("Fake gamepad")
                .to_owned(),
            layout_known: message["layoutKnown"] != false,
        });
    }
    let code_of = |name: &String| name.parse::<u32>().ok();
    if let Some(axes) = message["axes"].as_object() {
        for (name, value) in axes {
            if let (Ok(axis), Some(value)) = (
                serde_json::from_value(serde_json::Value::String(name.clone())),
                value.as_f64(),
            ) {
                events.push(InputEvent::Axis {
                    id: id.clone(),
                    axis: Some(axis),
                    code: None,
                    value,
                });
            }
        }
    }
    if let Some(codes) = message["codes"].as_object() {
        for (name, value) in codes {
            if let (Some(code), Some(value)) = (code_of(name), value.as_f64()) {
                events.push(InputEvent::Axis {
                    id: id.clone(),
                    axis: None,
                    code: Some(code),
                    value,
                });
            }
        }
    }
    if let Some(codes) = message["buttonCodes"].as_object() {
        for (name, pressed) in codes {
            if let (Some(code), Some(pressed)) = (code_of(name), pressed.as_bool()) {
                events.push(InputEvent::Button {
                    id: id.clone(),
                    button: None,
                    code: Some(code),
                    pressed,
                });
            }
        }
    }
    if let Some(buttons) = message["buttons"].as_object() {
        for (name, pressed) in buttons {
            if let (Ok(button), Some(pressed)) = (
                serde_json::from_value(serde_json::Value::String(name.clone())),
                pressed.as_bool(),
            ) {
                events.push(InputEvent::Button {
                    id: id.clone(),
                    button: Some(button),
                    code: None,
                    pressed,
                });
            }
        }
    }
    events
}

/// Gamepads through gilrs, on a thread of their own. Without a gamepad
/// backend (no permission, no libudev) it logs once and does nothing.
#[cfg(feature = "gamepad")]
pub fn gamepads(events: impl Fn(InputEvent) + Send + 'static) {
    let spawned = std::thread::Builder::new()
        .name("gamepads".into())
        .spawn(move || {
            // Without gilrs's gamepad filters: its deadzone pairs axes into
            // sticks by its own guesses and can move one axis when another
            // changes (a radio's switch moved its throttle); the mapping has
            // its own deadzone. Its jitter and D-pad filters cannot be run
            // after it hands an event over, since its state already holds
            // the new value (every move was dropped), so `Steady` and
            // `dpad` do their work here.
            let mut gilrs = match gilrs::GilrsBuilder::new()
                .with_default_filters(false)
                .build()
            {
                Ok(gilrs) => gilrs,
                Err(error) => {
                    tracing::warn!(%error, "gamepads are not available");
                    return;
                }
            };
            let id_of = |gilrs: &gilrs::Gilrs, id: gilrs::GamepadId| {
                let pad = gilrs.gamepad(id);
                let uuid = pad.uuid();
                if uuid == [0; 16] {
                    format!("gp-{}", usize::from(id))
                } else {
                    uuid.iter().fold(String::from("gp-"), |mut out, byte| {
                        use std::fmt::Write;
                        let _ = write!(out, "{byte:02x}");
                        out
                    })
                }
            };
            // Known when an SDL mapping names its inputs; otherwise gilrs
            // guesses gamepad names, wrong for a radio in joystick mode.
            let connected = |gilrs: &gilrs::Gilrs, pad: gilrs::GamepadId| {
                let gamepad = gilrs.gamepad(pad);
                InputEvent::Connected {
                    id: id_of(gilrs, pad),
                    name: gamepad.name().to_owned(),
                    layout_known: gamepad.mapping_source() == gilrs::MappingSource::SdlMappings,
                }
            };
            let announce = |gilrs: &gilrs::Gilrs, pad: gilrs::GamepadId| {
                events(connected(gilrs, pad));
                #[cfg(target_os = "macos")]
                if let Some(present) = present(gilrs, pad, id_of(gilrs, pad)) {
                    events(present);
                }
            };
            let pads: Vec<gilrs::GamepadId> = gilrs.gamepads().map(|(id, _)| id).collect();
            for pad in pads {
                announce(&gilrs, pad);
            }
            let mut steady = Steady::default();
            loop {
                let Some(event) = gilrs.next_event_blocking(None) else {
                    // Only when gilrs has lost its event source; without a
                    // pause this would spin a core (S24).
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                };
                let id = id_of(&gilrs, event.id);
                match event.event {
                    gilrs::EventType::Connected => announce(&gilrs, event.id),
                    gilrs::EventType::Disconnected => events(InputEvent::Disconnected { id }),
                    gilrs::EventType::AxisChanged(axis, value, code) => {
                        let known = gilrs.gamepad(event.id).mapping_source()
                            == gilrs::MappingSource::SdlMappings;
                        let value = as_sent(known, axis, f64::from(value));
                        if let Some(buttons) = dpad(axis, value) {
                            for (button, pressed) in buttons {
                                events(InputEvent::Button {
                                    id: id.clone(),
                                    button: Some(button),
                                    code: None,
                                    pressed,
                                });
                            }
                        } else if steady.changed(&id, code.into_u32(), value) {
                            events(InputEvent::Axis {
                                id,
                                axis: axis_of(axis),
                                code: Some(code.into_u32()),
                                value,
                            });
                        }
                    }
                    gilrs::EventType::ButtonPressed(button, code)
                    | gilrs::EventType::ButtonReleased(button, code) => {
                        let pressed = matches!(event.event, gilrs::EventType::ButtonPressed(..));
                        events(InputEvent::Button {
                            id,
                            button: button_of(button),
                            code: Some(code.into_u32()),
                            pressed,
                        });
                    }
                    _ => {}
                }
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "could not start the gamepad thread");
    }
}

/// Every input of the pad and its value now, from `IOKit`, named and scaled
/// as gilrs names and scales them so later events agree.
#[cfg(all(feature = "gamepad", target_os = "macos"))]
fn present(gilrs: &gilrs::Gilrs, pad: gilrs::GamepadId, id: String) -> Option<InputEvent> {
    use gilrs::{Axis as A, Button as B};
    let gamepad = gilrs.gamepad(pad);
    let known = gamepad.mapping_source() == gilrs::MappingSource::SdlMappings;
    let elements = super::hid::elements(gamepad.vendor_id()?, gamepad.product_id()?);
    if elements.is_empty() {
        return None;
    }
    let axis_codes: Vec<(u32, A)> = [
        A::LeftStickX,
        A::LeftStickY,
        A::LeftZ,
        A::RightStickX,
        A::RightStickY,
        A::RightZ,
        A::DPadX,
        A::DPadY,
    ]
    .into_iter()
    .filter_map(|axis| Some((gamepad.axis_code(axis)?.into_u32(), axis)))
    .collect();
    let button_codes: Vec<(u32, B)> = [
        B::South,
        B::East,
        B::North,
        B::West,
        B::LeftTrigger,
        B::LeftTrigger2,
        B::RightTrigger,
        B::RightTrigger2,
        B::Select,
        B::Start,
        B::Mode,
        B::LeftThumb,
        B::RightThumb,
        B::DPadUp,
        B::DPadDown,
        B::DPadLeft,
        B::DPadRight,
    ]
    .into_iter()
    .filter_map(|button| Some((gamepad.button_code(button)?.into_u32(), button)))
    .collect();
    let (mut axes, mut buttons) = (Vec::new(), Vec::new());
    for element in elements {
        if element.axis {
            let named = axis_codes
                .iter()
                .find(|(c, _)| *c == element.code)
                .map(|&(_, a)| a);
            // As gilrs reports it, then as `as_sent` keeps it.
            let reversed = known
                && GILRS_FLIPS_Y
                && matches!(named, Some(A::LeftStickY | A::RightStickY | A::DPadY));
            axes.push((
                element.code,
                named.and_then(axis_of),
                element.normalized(reversed),
            ));
        } else {
            let named = button_codes
                .iter()
                .find(|(c, _)| *c == element.code)
                .map(|&(_, b)| b);
            buttons.push((
                element.code,
                named.and_then(button_of),
                element.value > element.min,
            ));
        }
    }
    Some(InputEvent::Present { id, axes, buttons })
}

/// Drops an axis's changes smaller than `JITTER`, as gilrs's own jitter
/// filter would, judged against the last value passed on for that input.
#[cfg(any(feature = "gamepad", test))]
#[derive(Debug, Default)]
struct Steady {
    last: std::collections::HashMap<(String, u32), f64>,
}

/// The smallest change of an axis passed on, as gilrs's jitter filter.
#[cfg(any(feature = "gamepad", test))]
const JITTER: f64 = 0.01;

#[cfg(any(feature = "gamepad", test))]
impl Steady {
    fn changed(&mut self, id: &str, code: u32, value: f64) -> bool {
        let key = (id.to_owned(), code);
        let moved = self.last.get(&key).is_none_or(|last| {
            let change = (value - last).abs();
            // Reaching an end always passes, however small the step.
            change >= JITTER || (change > f64::EPSILON && value.abs() >= 1.0)
        });
        if moved {
            self.last.insert(key, value);
        }
        moved
    }
}

/// A hat D-pad, which gilrs reports as two axes, as D-pad buttons.
#[cfg(feature = "gamepad")]
fn dpad(axis: gilrs::Axis, value: f64) -> Option<[(GamepadButton, bool); 2]> {
    match axis {
        gilrs::Axis::DPadX => Some([
            (GamepadButton::DpadLeft, value < -0.5),
            (GamepadButton::DpadRight, value > 0.5),
        ]),
        gilrs::Axis::DPadY => Some([
            (GamepadButton::DpadDown, value < -0.5),
            (GamepadButton::DpadUp, value > 0.5),
        ]),
        _ => None,
    }
}

/// Whether gilrs reverses the axes it takes for Y sticks, so that up is
/// positive: true of its macOS, Linux and Windows (WGI) backends.
#[cfg(feature = "gamepad")]
const GILRS_FLIPS_Y: bool = cfg!(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "windows"
));

/// An axis value as the device sent it, for a device whose layout gilrs
/// does not know: there gilrs only guesses which axes are Y sticks (a
/// radio's elevator and an aux channel), so its flip is undone rather than
/// shown as the device's.
#[cfg(feature = "gamepad")]
fn as_sent(known: bool, axis: gilrs::Axis, value: f64) -> f64 {
    let guessed_y = matches!(
        axis,
        gilrs::Axis::LeftStickY | gilrs::Axis::RightStickY | gilrs::Axis::DPadY
    );
    if !known && GILRS_FLIPS_Y && guessed_y && value != 0.0 {
        -value
    } else {
        value
    }
}

#[cfg(feature = "gamepad")]
fn axis_of(axis: gilrs::Axis) -> Option<GamepadAxis> {
    Some(match axis {
        gilrs::Axis::LeftStickX => GamepadAxis::LeftStickX,
        gilrs::Axis::LeftStickY => GamepadAxis::LeftStickY,
        gilrs::Axis::RightStickX => GamepadAxis::RightStickX,
        gilrs::Axis::RightStickY => GamepadAxis::RightStickY,
        gilrs::Axis::LeftZ => GamepadAxis::LeftZ,
        gilrs::Axis::RightZ => GamepadAxis::RightZ,
        _ => return None,
    })
}

#[cfg(feature = "gamepad")]
fn button_of(button: gilrs::Button) -> Option<GamepadButton> {
    Some(match button {
        gilrs::Button::South => GamepadButton::South,
        gilrs::Button::East => GamepadButton::East,
        gilrs::Button::North => GamepadButton::North,
        gilrs::Button::West => GamepadButton::West,
        gilrs::Button::LeftTrigger => GamepadButton::LeftTrigger,
        gilrs::Button::RightTrigger => GamepadButton::RightTrigger,
        gilrs::Button::LeftTrigger2 => GamepadButton::LeftTrigger2,
        gilrs::Button::RightTrigger2 => GamepadButton::RightTrigger2,
        gilrs::Button::Select => GamepadButton::Select,
        gilrs::Button::Start => GamepadButton::Start,
        gilrs::Button::Mode => GamepadButton::Mode,
        gilrs::Button::LeftThumb => GamepadButton::LeftThumb,
        gilrs::Button::RightThumb => GamepadButton::RightThumb,
        gilrs::Button::DPadUp => GamepadButton::DpadUp,
        gilrs::Button::DPadDown => GamepadButton::DpadDown,
        gilrs::Button::DPadLeft => GamepadButton::DpadLeft,
        gilrs::Button::DPadRight => GamepadButton::DpadRight,
        _ => return None,
    })
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

    #[cfg(feature = "gamepad")]
    #[test]
    fn a_radio_axis_is_as_the_radio_sent_it() {
        let y = gilrs::Axis::LeftStickY;
        assert!(
            (as_sent(true, y, 0.5) - 0.5).abs() < f64::EPSILON,
            "a known pad: gilrs's"
        );
        let radio = as_sent(false, y, 0.5);
        if GILRS_FLIPS_Y {
            assert!(
                (radio + 0.5).abs() < f64::EPSILON,
                "gilrs's guessed flip undone"
            );
        }
        let x = as_sent(false, gilrs::Axis::LeftStickX, 0.5);
        assert!((x - 0.5).abs() < f64::EPSILON, "never flipped");
    }

    #[test]
    fn a_steady_axis_passes_every_real_move_and_drops_noise() {
        let mut steady = Steady::default();
        assert!(steady.changed("r", 1, 0.5), "the first value");
        assert!(!steady.changed("r", 1, 0.505), "noise");
        assert!(
            steady.changed("r", 1, 0.52),
            "a move, judged from the last passed"
        );
        assert!(steady.changed("r", 2, 0.52), "each input on its own");
        assert!(steady.changed("r", 1, 1.0));
        assert!(steady.changed("r", 1, 0.0));
        assert!(!steady.changed("r", 1, 0.0), "unchanged");
        assert!(
            steady.changed("other", 1, 0.0),
            "each controller on its own"
        );
    }

    #[cfg(feature = "gamepad")]
    #[test]
    fn a_hat_is_d_pad_buttons() {
        assert_eq!(
            dpad(gilrs::Axis::DPadX, -1.0),
            Some([
                (GamepadButton::DpadLeft, true),
                (GamepadButton::DpadRight, false)
            ])
        );
        assert_eq!(
            dpad(gilrs::Axis::DPadY, 0.0),
            Some([
                (GamepadButton::DpadDown, false),
                (GamepadButton::DpadUp, false)
            ])
        );
        assert_eq!(dpad(gilrs::Axis::LeftStickX, 1.0), None);
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

    #[test]
    fn the_fake_reads_json() {
        let mut known = BTreeSet::new();
        let events = fake_events(
            &serde_json::json!({ "id": "f", "axes": { "left-stick-y": 0.5, "nope": 1 },
                                 "buttons": { "east": true } }),
            &mut known,
        );
        assert_eq!(events.len(), 3);
        assert!(
            matches!(&events[0], InputEvent::Connected { name, layout_known: true, .. } if name == "Fake gamepad")
        );
        let radio = fake_events(
            &serde_json::json!({ "id": "r", "layoutKnown": false, "codes": { "65587": 0.4 },
                                 "buttonCodes": { "589825": true } }),
            &mut known,
        );
        assert!(matches!(
            &radio[0],
            InputEvent::Connected {
                layout_known: false,
                ..
            }
        ));
        assert!(matches!(
            radio[1],
            InputEvent::Axis {
                axis: None,
                code: Some(65_587),
                ..
            }
        ));
        assert!(matches!(
            radio[2],
            InputEvent::Button {
                button: None,
                code: Some(589_825),
                pressed: true,
                ..
            }
        ));
        let gone = fake_events(
            &serde_json::json!({ "id": "f", "connected": false }),
            &mut known,
        );
        assert_eq!(gone, [InputEvent::Disconnected { id: "f".into() }]);
    }
}
