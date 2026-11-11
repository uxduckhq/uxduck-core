//! Bluetooth gamepads that `BlueZ` no longer links (Linux, FB-88). `BlueZ` can
//! leave a Bluetooth LE gamepad's input node behind after the link drops,
//! for example when the device is paired again under a new name, so gilrs
//! goes on reporting a joystick that is not there. Every few seconds the
//! daemon asks `BlueZ` which devices are linked; a gamepad that is not, and
//! has sent nothing since the last check, is reported unplugged until it
//! sends input again or `BlueZ` links it again.

use std::collections::HashMap;
use std::path::Path;
#[cfg(all(feature = "gamepad", target_os = "linux"))]
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};

use uxduck_engine::controller_input::InputEvent;

/// How often `BlueZ` is asked.
#[cfg(all(feature = "gamepad", target_os = "linux"))]
pub(super) const CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(all(feature = "gamepad", target_os = "linux"))]
static PRESENCE: LazyLock<Mutex<Presence>> = LazyLock::new(Mutex::default);

#[cfg(all(feature = "gamepad", target_os = "linux"))]
fn presence() -> MutexGuard<'static, Presence> {
    PRESENCE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// gilrs reports a gamepad at `devpath`, announced by `connected`: one on
/// the Bluetooth bus is watched.
#[cfg(all(feature = "gamepad", target_os = "linux"))]
pub(super) fn track(connected: &InputEvent, devpath: &Path) {
    let InputEvent::Connected { id, .. } = connected else {
        return;
    };
    if let Some(address) = address(Path::new("/sys/class/input"), devpath) {
        presence().track(id.clone(), address, connected.clone());
    }
}

/// Passes gilrs's events on to `events`, with each Bluetooth gamepad `BlueZ`
/// does not link reported unplugged, from a thread that asks `BlueZ` every
/// [`CHECK_EVERY`].
#[cfg(all(feature = "gamepad", target_os = "linux"))]
pub(super) fn watch(
    events: impl Fn(InputEvent) + Send + Clone + 'static,
) -> impl Fn(InputEvent) + Send + Clone + 'static {
    let checked = events.clone();
    let spawned = std::thread::Builder::new()
        .name("bluetooth gamepads".into())
        .spawn(move || {
            let mut bus = None;
            loop {
                std::thread::sleep(CHECK_EVERY);
                if !presence().any() {
                    continue;
                }
                let linked = linked(&mut bus);
                let mut presence = presence();
                for event in presence.check(linked.as_ref()) {
                    checked(event);
                }
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "could not start the Bluetooth gamepad thread");
    }
    move |event| {
        let mut presence = presence();
        match &event {
            InputEvent::Disconnected { id } => {
                if !presence.forget(id) {
                    return;
                }
            }
            InputEvent::Axis { id, .. }
            | InputEvent::Button { id, .. }
            | InputEvent::Present { id, .. } => {
                if let Some(back) = presence.heard(id) {
                    events(back);
                }
            }
            _ => {}
        }
        drop(presence);
        events(event);
    }
}

/// The Bluetooth address of an input node, from sysfs under `root`
/// (`/sys/class/input`): only a node on the Bluetooth bus (`0005`) has one.
pub(super) fn address(root: &Path, devpath: &Path) -> Option<String> {
    let device = root.join(devpath.file_name()?).join("device");
    let bus = std::fs::read_to_string(device.join("id/bustype")).ok()?;
    if u16::from_str_radix(bus.trim(), 16).ok()? != 0x0005 {
        return None;
    }
    let uniq = std::fs::read_to_string(device.join("uniq")).ok()?;
    let uniq = uniq.trim();
    (uniq.len() == 17).then(|| uniq.to_uppercase())
}

#[derive(Debug)]
struct Pad {
    address: String,
    connected: InputEvent,
    unlinked: bool,
    heard: bool,
}

/// The Bluetooth gamepads gilrs reports, and which of them `BlueZ` does not
/// link.
#[derive(Debug, Default)]
pub(super) struct Presence {
    pads: HashMap<String, Pad>,
}

impl Presence {
    /// gilrs reports a Bluetooth gamepad; `connected` is the event that
    /// announced it, sent again when it comes back.
    pub(super) fn track(&mut self, id: String, address: String, connected: InputEvent) {
        self.pads.insert(
            id,
            Pad {
                address,
                connected,
                unlinked: false,
                heard: true,
            },
        );
    }

    /// gilrs reports the gamepad gone; false when it was already reported
    /// unplugged.
    pub(super) fn forget(&mut self, id: &str) -> bool {
        self.pads.remove(id).is_none_or(|pad| !pad.unlinked)
    }

    /// The gamepad sent input, so it is there whatever `BlueZ` said. Returns
    /// the event announcing it again when it had been reported unplugged.
    pub(super) fn heard(&mut self, id: &str) -> Option<InputEvent> {
        let pad = self.pads.get_mut(id)?;
        pad.heard = true;
        std::mem::take(&mut pad.unlinked).then(|| pad.connected.clone())
    }

    /// Whether any Bluetooth gamepad is tracked, so `BlueZ` is asked only then.
    #[cfg(all(feature = "gamepad", target_os = "linux"))]
    pub(super) fn any(&self) -> bool {
        !self.pads.is_empty()
    }

    /// `linked` is `BlueZ`'s answer by address (a device it does not know is
    /// not linked), or None when `BlueZ` cannot be asked, which changes
    /// nothing. Returns the events for gamepads that went or came back.
    pub(super) fn check(&mut self, linked: Option<&HashMap<String, bool>>) -> Vec<InputEvent> {
        let Some(linked) = linked else {
            return Vec::new();
        };
        let mut events = Vec::new();
        for (id, pad) in &mut self.pads {
            let is_linked = linked.get(&pad.address).copied().unwrap_or(false);
            let heard = std::mem::take(&mut pad.heard);
            if is_linked && pad.unlinked {
                pad.unlinked = false;
                events.push(pad.connected.clone());
            } else if !is_linked && !heard && !pad.unlinked {
                pad.unlinked = true;
                tracing::info!(
                    id,
                    "Bluetooth gamepad is not linked; treating its input node as unplugged"
                );
                events.push(InputEvent::Disconnected { id: id.clone() });
            }
        }
        events
    }
}

/// Which Bluetooth devices `BlueZ` links, by address, or None without `BlueZ`
/// on the system bus.
#[cfg(all(feature = "gamepad", target_os = "linux"))]
pub(super) fn linked(bus: &mut Option<zbus::Connection>) -> Option<HashMap<String, bool>> {
    use zbus::OwnedObjectPath;
    use zbus::wire::OwnedVariant;
    type Objects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedVariant>>>;

    futures::executor::block_on(async {
        if bus.is_none() {
            *bus = zbus::Connection::system().await.ok();
        }
        let connection = bus.as_ref()?;
        let manager = zbus::Proxy::new(
            connection,
            "org.bluez",
            "/",
            "org.freedesktop.DBus.ObjectManager",
        )
        .await
        .ok()?;
        let objects: Objects = match manager.call("GetManagedObjects", &()).await {
            Ok(objects) => objects,
            Err(error) => {
                tracing::debug!(%error, "BlueZ could not be asked about Bluetooth gamepads");
                *bus = None;
                return None;
            }
        };
        let mut linked = HashMap::new();
        for interfaces in objects.values() {
            let Some(device) = interfaces.get("org.bluez.Device1") else {
                continue;
            };
            let address = device
                .get("Address")
                .and_then(|value| <&str>::try_from(value).ok());
            let connected = device
                .get("Connected")
                .and_then(|value| bool::try_from(value).ok());
            if let (Some(address), Some(connected)) = (address, connected) {
                *linked.entry(address.to_uppercase()).or_insert(false) |= connected;
            }
        }
        Some(linked)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDRESS: &str = "14:33:5C:8C:86:FA";

    fn connected(id: &str, name: &str) -> InputEvent {
        InputEvent::Connected {
            id: id.into(),
            name: name.into(),
            layout_known: false,
        }
    }

    fn linked(connected: bool) -> HashMap<String, bool> {
        HashMap::from([(ADDRESS.to_owned(), connected)])
    }

    fn disconnected(events: &[InputEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|event| match event {
                InputEvent::Disconnected { id } => Some(id.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_left_behind_node_of_a_re_paired_radio_is_unplugged() {
        // FB-88: the radio came back as "ELRS Joystick" on a new node, and
        // BlueZ left the old "ExpressLRS Joystick" node, silent, while it
        // reports the address not linked.
        let mut presence = Presence::default();
        presence.track(
            "gp-old".into(),
            ADDRESS.into(),
            connected("gp-old", "ExpressLRS Joystick"),
        );
        assert!(presence.check(Some(&linked(true))).is_empty());
        let events = presence.check(Some(&linked(false)));
        assert_eq!(disconnected(&events), ["gp-old"]);
        assert!(
            presence.check(Some(&linked(false))).is_empty(),
            "reported unplugged once"
        );
        assert!(
            !presence.forget("gp-old"),
            "gilrs's own unplug is not repeated"
        );
    }

    #[test]
    fn a_device_bluez_no_longer_knows_is_unplugged() {
        let mut presence = Presence::default();
        presence.track("gp-old".into(), ADDRESS.into(), connected("gp-old", "Pad"));
        assert!(presence.check(Some(&HashMap::new())).is_empty());
        let events = presence.check(Some(&HashMap::new()));
        assert_eq!(disconnected(&events), ["gp-old"]);
    }

    #[test]
    fn input_keeps_a_gamepad_and_brings_an_unplugged_one_back() {
        let mut presence = Presence::default();
        presence.track("gp-1".into(), ADDRESS.into(), connected("gp-1", "Pad"));
        presence.check(Some(&linked(false)));
        assert!(presence.heard("gp-1").is_none());
        assert!(
            presence.check(Some(&linked(false))).is_empty(),
            "input since the last check proves it is there"
        );
        assert_eq!(
            disconnected(&presence.check(Some(&linked(false)))),
            ["gp-1"]
        );
        assert!(matches!(
            presence.heard("gp-1"),
            Some(InputEvent::Connected { id, .. }) if id == "gp-1"
        ));
        assert!(presence.heard("gp-1").is_none(), "announced again once");
        assert!(presence.forget("gp-1"));
    }

    #[test]
    fn bluez_linking_it_again_brings_it_back() {
        let mut presence = Presence::default();
        presence.track("gp-1".into(), ADDRESS.into(), connected("gp-1", "Pad"));
        presence.check(Some(&linked(false)));
        presence.check(Some(&linked(false)));
        let events = presence.check(Some(&linked(true)));
        assert!(matches!(
            events.as_slice(),
            [InputEvent::Connected { id, .. }] if id == "gp-1"
        ));
    }

    #[test]
    fn without_bluez_nothing_changes() {
        let mut presence = Presence::default();
        presence.track("gp-1".into(), ADDRESS.into(), connected("gp-1", "Pad"));
        for _ in 0..3 {
            assert!(presence.check(None).is_empty());
        }
        assert!(presence.forget("gp-1"));
    }

    #[test]
    fn only_a_bluetooth_node_has_an_address() {
        let root = tempfile::tempdir().expect("temporary directory");
        let node = |name: &str, bus: &str, uniq: &str| {
            let device = root.path().join(name).join("device");
            std::fs::create_dir_all(device.join("id")).expect("sysfs directory");
            std::fs::write(device.join("id/bustype"), format!("{bus}\n")).expect("bustype");
            std::fs::write(device.join("uniq"), format!("{uniq}\n")).expect("uniq");
        };
        node("event14", "0005", "14:33:5c:8c:86:fa");
        node("event3", "0003", "14:33:5c:8c:86:fa");
        node("event5", "0005", "");
        assert_eq!(
            address(root.path(), Path::new("/dev/input/event14")).as_deref(),
            Some(ADDRESS)
        );
        assert_eq!(address(root.path(), Path::new("/dev/input/event3")), None);
        assert_eq!(address(root.path(), Path::new("/dev/input/event5")), None);
        assert_eq!(address(root.path(), Path::new("/dev/input/event9")), None);
    }
}
