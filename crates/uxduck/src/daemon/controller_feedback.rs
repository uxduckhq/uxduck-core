//! A controller's battery and rumble on this computer (S419, ADR-0005 A7).
//!
//! What each platform reports, through gilrs:
//!
//! - **Linux**: the kernel's `power_supply` entry under the input device
//!   (`capacity` and `status`), which drivers such as `hid-playstation`,
//!   `hid-nintendo`, `xpadneo` and `hid-steam` fill for a wireless
//!   controller. gilrs says Wired for any device without one, so a
//!   Bluetooth device without one is Unknown here instead. Rumble is the
//!   kernel's force feedback (`FF_RUMBLE`), which needs write access to
//!   the device's event node.
//! - **Windows**: Windows.Gaming.Input's `RawGameController`: wired or
//!   wireless, and a wireless one's battery report. Rumble is its
//!   `Gamepad` vibration, for controllers Windows takes as an Xbox
//!   gamepad.
//! - **macOS**: gilrs reports no power and no rumble. UXDuck reads the
//!   `BatteryPercent` the system puts on a Bluetooth HID device that tells
//!   its battery (`hid::power`), which says neither charging nor wired.
//!   No rumble.
//!
//! The fake controller (`controllers::fake`) reports what its datagrams
//! say and answers each rumble to the address that last spoke for it.

use std::sync::{LazyLock, Mutex, PoisonError};

#[cfg(any(feature = "gamepad", test))]
use uxduck_core::domain::controller::ControllerPower;
use uxduck_core::domain::controller::Rumble;
#[cfg(feature = "gamepad")]
use uxduck_engine::controller_input::InputEvent;

type Rumbler = Box<dyn Fn(&str, Rumble) + Send>;

/// Each reader of controllers, told every rumble: each acts on its own.
static RUMBLERS: LazyLock<Mutex<Vec<Rumbler>>> = LazyLock::new(Mutex::default);

/// Rumbles the controller `id`, where the reader that has it can.
pub fn rumble(id: &str, rumble: Rumble) {
    for rumbler in RUMBLERS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
    {
        rumbler(id, rumble);
    }
}

/// Adds a reader of controllers that is told every rumble.
pub(super) fn rumbled(rumbler: impl Fn(&str, Rumble) + Send + 'static) {
    RUMBLERS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(Box::new(rumbler));
}

/// What gilrs says of a pad's power, as the engine takes it.
#[cfg(all(feature = "gamepad", any(not(target_os = "macos"), test)))]
fn power_of(info: gilrs::PowerInfo) -> ControllerPower {
    match info {
        gilrs::PowerInfo::Unknown => ControllerPower::Unknown,
        gilrs::PowerInfo::Wired => ControllerPower::Wired,
        gilrs::PowerInfo::Discharging(percent) => ControllerPower::Discharging { percent },
        gilrs::PowerInfo::Charging(percent) => ControllerPower::Charging { percent },
        gilrs::PowerInfo::Charged => ControllerPower::Charged,
    }
}

/// gilrs on Linux says Wired for every device without a `power_supply`
/// entry. On the Bluetooth bus that is a battery the driver does not tell
/// of, so it is Unknown.
#[cfg(any(all(feature = "gamepad", target_os = "linux"), test))]
fn on_linux(power: ControllerPower, bluetooth: bool) -> ControllerPower {
    if bluetooth && power == ControllerPower::Wired {
        ControllerPower::Unknown
    } else {
        power
    }
}

/// How often each pad's power is read again: a battery changes over
/// minutes, and this shows a cable plugged in or pulled out promptly.
#[cfg(feature = "gamepad")]
const POWER_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// The longest a rumble waits while gilrs has no event: gilrs cannot be
/// woken, so the gamepad thread looks this often.
#[cfg(feature = "gamepad")]
const LOOK_EVERY: std::time::Duration = std::time::Duration::from_millis(50);

/// The gamepad thread's rumbles and power readings.
#[cfg(feature = "gamepad")]
pub(super) struct Feedback {
    rumbles: std::sync::mpsc::Receiver<(String, Rumble)>,
    /// Effects playing: gilrs stops one that is dropped.
    playing: Vec<(std::time::Instant, gilrs::ff::Effect)>,
    powers: std::collections::HashMap<String, ControllerPower>,
    next_power: std::time::Instant,
}

#[cfg(feature = "gamepad")]
impl Feedback {
    pub(super) fn new() -> Self {
        let (told, rumbles) = std::sync::mpsc::channel();
        rumbled(move |id, rumble| {
            let _ = told.send((id.to_owned(), rumble));
        });
        Self {
            rumbles,
            playing: Vec::new(),
            powers: std::collections::HashMap::new(),
            next_power: std::time::Instant::now(),
        }
    }

    /// A pad connected: its power is read at once.
    pub(super) fn connected(&mut self) {
        self.next_power = std::time::Instant::now();
    }

    /// gilrs's next event, rumbling and reading power while there is none.
    /// `None` as gilrs gives it, which `gamepad_events::next` handles.
    pub(super) fn next_event(
        &mut self,
        gilrs: &mut gilrs::Gilrs,
        pads: &[(String, gilrs::GamepadId)],
        events: &impl Fn(InputEvent),
    ) -> Option<gilrs::Event> {
        loop {
            self.look(gilrs, pads, events);
            let started = std::time::Instant::now();
            if let Some(event) = gilrs.next_event_blocking(Some(LOOK_EVERY)) {
                return Some(event);
            }
            if started.elapsed() < LOOK_EVERY {
                return None;
            }
        }
    }

    fn look(
        &mut self,
        gilrs: &mut gilrs::Gilrs,
        pads: &[(String, gilrs::GamepadId)],
        events: &impl Fn(InputEvent),
    ) {
        let now = std::time::Instant::now();
        self.playing.retain(|(until, _)| *until > now);
        while let Ok((id, rumble)) = self.rumbles.try_recv() {
            if let Some((_, pad)) = pads.iter().find(|(pad_id, _)| *pad_id == id) {
                self.play(gilrs, *pad, rumble);
            }
        }
        if now < self.next_power {
            return;
        }
        self.next_power = now + POWER_EVERY;
        self.powers
            .retain(|id, _| pads.iter().any(|(pad, _)| pad == id));
        for (id, pad) in pads {
            let power = power(gilrs, *pad);
            if self.powers.insert(id.clone(), power) != Some(power) {
                events(InputEvent::Power {
                    id: id.clone(),
                    power,
                });
            }
        }
    }

    fn play(&mut self, gilrs: &mut gilrs::Gilrs, pad: gilrs::GamepadId, rumble: Rumble) {
        use gilrs::ff::{BaseEffect, BaseEffectType, EffectBuilder, Repeat, Replay, Ticks};
        if !gilrs.gamepad(pad).is_ff_supported() {
            return;
        }
        let mut builder = EffectBuilder::new();
        let mut at = 0;
        let total: u32 = rumble.pulses().iter().map(|p| p.on_ms + p.off_ms).sum();
        for pulse in rumble.pulses() {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a strength of 0 to 1 scaled to gilrs's u16"
            )]
            let magnitude = (f32::from(u16::MAX) * pulse.strength.clamp(0.0, 1.0)) as u16;
            let scheduling = Replay {
                after: Ticks::from_ms(at),
                play_for: Ticks::from_ms(pulse.on_ms),
                with_delay: Ticks::from_ms(total),
            };
            for kind in [
                BaseEffectType::Strong { magnitude },
                BaseEffectType::Weak { magnitude },
            ] {
                builder.add_effect(BaseEffect {
                    kind,
                    scheduling,
                    ..Default::default()
                });
            }
            at += pulse.on_ms + pulse.off_ms;
        }
        let built = builder
            .repeat(Repeat::For(Ticks::from_ms(total)))
            .gamepads(&[pad])
            .finish(gilrs);
        match built.and_then(|effect| effect.play().map(|()| effect)) {
            Ok(effect) => {
                let until =
                    std::time::Instant::now() + std::time::Duration::from_millis(u64::from(total));
                self.playing.push((until, effect));
            }
            Err(error) => tracing::debug!(%error, "the controller could not rumble"),
        }
    }
}

/// A pad's power as its platform reports it now.
#[cfg(all(feature = "gamepad", not(target_os = "macos")))]
fn power(gilrs: &gilrs::Gilrs, pad: gilrs::GamepadId) -> ControllerPower {
    let gamepad = gilrs.gamepad(pad);
    let power = power_of(gamepad.power_info());
    #[cfg(target_os = "linux")]
    let power = on_linux(
        power,
        super::bluetooth::address(
            std::path::Path::new("/sys/class/input"),
            gilrs::LinuxGamepadExt::devpath(&gamepad),
        )
        .is_some(),
    );
    power
}

/// A pad's power on macOS, from `IOKit`: gilrs reports none.
#[cfg(all(feature = "gamepad", target_os = "macos"))]
fn power(gilrs: &gilrs::Gilrs, pad: gilrs::GamepadId) -> ControllerPower {
    use gilrs::MacOSGamepadExt;
    let gamepad = gilrs.gamepad(pad);
    match (
        gamepad.vendor_id(),
        gamepad.product_id(),
        gamepad.registry_entry_id(),
    ) {
        (Some(vendor), Some(product), Some(entry)) => super::hid::power(vendor, product, entry),
        _ => ControllerPower::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uxduck_core::domain::controller::ControllerPower as P;

    #[cfg(feature = "gamepad")]
    #[test]
    fn gilrs_power_is_said_as_the_platform_said_it() {
        assert_eq!(power_of(gilrs::PowerInfo::Unknown), P::Unknown);
        assert_eq!(power_of(gilrs::PowerInfo::Wired), P::Wired);
        assert_eq!(
            power_of(gilrs::PowerInfo::Discharging(37)),
            P::Discharging { percent: 37 }
        );
        assert_eq!(
            power_of(gilrs::PowerInfo::Charging(80)),
            P::Charging { percent: 80 }
        );
        assert_eq!(power_of(gilrs::PowerInfo::Charged), P::Charged);
    }

    #[test]
    fn a_bluetooth_pad_without_a_battery_entry_is_unknown_on_linux() {
        assert_eq!(on_linux(P::Wired, true), P::Unknown);
        assert_eq!(on_linux(P::Wired, false), P::Wired);
        let low = P::Discharging { percent: 12 };
        assert_eq!(on_linux(low, true), low);
    }

    #[test]
    fn each_reader_is_told_every_rumble() {
        let (told, heard) = std::sync::mpsc::channel();
        rumbled(move |id, rumble| {
            let _ = told.send((id.to_owned(), rumble));
        });
        rumble("pad", Rumble::Refused);
        assert_eq!(heard.try_recv(), Ok(("pad".to_owned(), Rumble::Refused)));
    }
}
