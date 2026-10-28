//! Controller input (ADR-0005): gamepads through gilrs on their own thread,
//! and a fake one on a UDP port for tests. Both produce the same events for
//! the data plane, whose [`Controls`] (shared with the browser's daemon)
//! keeps each controller's latest values, tells the engine when its sticks
//! are centred and when a button goes down, and sends pilot input for the
//! routes the engine grants. It also publishes every controller's raw input
//! for `controller/inputs` ([`inputs`]).

use std::collections::BTreeSet;
use std::sync::{LazyLock, Mutex, PoisonError};

use uxduck_contracts::dto::controller::ControllerInputsResult;
use uxduck_core::domain::controller::{GamepadAxis, GamepadButton};
pub use uxduck_engine::controller_input::{Controls, InputEvent, Report};

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

#[cfg(feature = "gamepad")]
fn gamepad_id(gilrs: &gilrs::Gilrs, id: gilrs::GamepadId) -> String {
    let uuid = gilrs.gamepad(id).uuid();
    if uuid == [0; 16] {
        format!("gp-{}", usize::from(id))
    } else {
        uuid.iter().fold(String::from("gp-"), |mut out, byte| {
            use std::fmt::Write;
            let _ = write!(out, "{byte:02x}");
            out
        })
    }
}

#[cfg(feature = "gamepad")]
fn gamepad_connected(gilrs: &gilrs::Gilrs, pad: gilrs::GamepadId) -> InputEvent {
    let gamepad = gilrs.gamepad(pad);
    // Otherwise gilrs guesses gamepad names, wrong for a radio in joystick mode.
    InputEvent::Connected {
        id: gamepad_id(gilrs, pad),
        name: gamepad.name().to_owned(),
        layout_known: gamepad.mapping_source() == gilrs::MappingSource::SdlMappings,
    }
}

/// Gamepads through gilrs, on a thread of their own. Without a gamepad
/// backend (no permission, no libudev) it logs once and does nothing.
#[cfg(feature = "gamepad")]
pub fn gamepads(events: impl Fn(InputEvent) + Send + Clone + 'static) {
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
            #[cfg(target_os = "macos")]
            let mut duplicates = std::collections::HashMap::new();
            let pads: Vec<gilrs::GamepadId> = gilrs.gamepads().map(|(id, _)| id).collect();
            for pad in pads {
                events(gamepad_connected(&gilrs, pad));
                #[cfg(target_os = "macos")]
                if let Some(observer) =
                    announce_native(&gilrs, pad, gamepad_id(&gilrs, pad), events.clone())
                {
                    duplicates.insert(pad, observer);
                }
            }
            let mut steady = Steady::default();
            loop {
                let event = super::gamepad_events::next(
                    || gilrs.next_event_blocking(None),
                    std::thread::sleep,
                );
                let id = gamepad_id(&gilrs, event.id);
                match event.event {
                    gilrs::EventType::Connected => {
                        #[cfg(target_os = "macos")]
                        duplicates.remove(&event.id);
                        events(gamepad_connected(&gilrs, event.id));
                        #[cfg(target_os = "macos")]
                        if let Some(observer) =
                            announce_native(&gilrs, event.id, id.clone(), events.clone())
                        {
                            duplicates.insert(event.id, observer);
                        }
                    }
                    gilrs::EventType::Disconnected => {
                        #[cfg(target_os = "macos")]
                        duplicates.remove(&event.id);
                        steady.last.retain(|(device, _), _| device != &id);
                        events(InputEvent::Disconnected { id });
                    }
                    gilrs::EventType::AxisChanged(axis, value, code) => {
                        #[cfg(target_os = "macos")]
                        if duplicates
                            .get(&event.id)
                            .is_some_and(|observer| observer.handles(code.into_u32()))
                        {
                            continue;
                        }
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

/// Native snapshots and repeated-axis events must use exactly the same codes.
#[cfg(all(feature = "gamepad", target_os = "macos"))]
fn announce_native(
    gilrs: &gilrs::Gilrs,
    pad: gilrs::GamepadId,
    id: String,
    events: impl Fn(InputEvent) + Send + Clone + 'static,
) -> Option<super::hid::Duplicates> {
    let gamepad = gilrs.gamepad(pad);
    let (vendor, product) = (gamepad.vendor_id()?, gamepad.product_id()?);
    let mut observer = (gamepad.mapping_source() != gilrs::MappingSource::SdlMappings)
        .then(|| super::hid::observe_duplicates(vendor, product, id.clone(), events.clone()))
        .flatten();
    let elements = observer.as_ref().map_or_else(
        || super::hid::elements(vendor, product),
        |observer| observer.initial.clone(),
    );
    if let Some(snapshot) = present(gilrs, pad, id, elements) {
        events(snapshot);
    }
    if let Some(observer) = &mut observer {
        observer.activate();
    }
    observer
}

/// Every input of the pad and its value now, from `IOKit`, named and scaled
/// as gilrs names and scales them so later events agree.
#[cfg(all(feature = "gamepad", target_os = "macos"))]
fn present(
    gilrs: &gilrs::Gilrs,
    pad: gilrs::GamepadId,
    id: String,
    elements: Vec<super::hid::Element>,
) -> Option<InputEvent> {
    use gilrs::{Axis as A, Button as B};
    let gamepad = gilrs.gamepad(pad);
    let known = gamepad.mapping_source() == gilrs::MappingSource::SdlMappings;
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
pub(super) struct Steady {
    last: std::collections::HashMap<(String, u32), f64>,
}

/// The smallest change of an axis passed on, as gilrs's jitter filter.
#[cfg(any(feature = "gamepad", test))]
const JITTER: f64 = 0.01;

#[cfg(any(feature = "gamepad", test))]
impl Steady {
    pub(super) fn changed(&mut self, id: &str, code: u32, value: f64) -> bool {
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
