//! Finding vehicles (ADR-0018, S44): what a search found, where, how, and
//! why something there is not a vehicle. Finds are never journaled: they
//! are what is true now, and expire.

use crate::domain::link::{LinkKind, UsbId};

/// How far a search goes (F1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FindScope {
    /// USB devices the system reports; nothing is opened.
    Detect,
    /// This computer's serial devices and ports, and the default gateway.
    Nearby,
    /// Every host of this computer's /24 too.
    Network,
}

/// Where a find is, as people say it (F15).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FindPlace {
    Usb,
    ThisComputer,
    /// The default gateway: a vehicle's own hotspot.
    Wifi,
    Network,
}

/// Why a find is not a vehicle (F11).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FindReason {
    NoMavlink,
    Bootloader,
    Busy,
    Permission,
    SilentKnownBoard,
    Unreachable,
}

/// What answered: the heartbeat's say, and the vehicle UXDuck knows it as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundVehicle {
    pub system_id: u8,
    /// `MAV_TYPE`, such as `Quadrotor`.
    pub vehicle_type: String,
    pub autopilot: String,
    pub armed: bool,
    /// The vehicle UXDuck already knows, by its UID.
    pub vehicle_id: Option<String>,
    pub name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Find {
    /// Stable for the same place: `serial:/dev/cu.usbmodem1101`,
    /// `udp:14550`, `tcp:127.0.0.1:5760`.
    pub id: String,
    pub place: FindPlace,
    /// The link that would connect it, with the settings that worked.
    pub link: Option<LinkKind>,
    /// How it was found, in a few words.
    pub evidence: String,
    pub usb: Option<UsbId>,
    /// What the known-id table calls it.
    pub device_name: Option<String>,
    pub vehicle: Option<FoundVehicle>,
    pub reason: Option<FindReason>,
    /// The program holding it, or the fix for a permission.
    pub detail: Option<String>,
    pub seen_us: u64,
}
