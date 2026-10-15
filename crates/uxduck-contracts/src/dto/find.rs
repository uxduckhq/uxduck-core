//! Finding vehicles on the wire (ADR-0018 F7, F10 to F12).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::dto::link::{LinkKind, UsbId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum FindScope {
    /// USB devices the system reports; nothing is opened.
    Detect,
    /// This computer's serial devices, UDP 14550 and 15010, TCP 5760 to
    /// 5763 on this computer, and the default gateway.
    Nearby,
    /// Every host of this computer's /24 too, after an approval.
    Network,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum FindPlace {
    Usb,
    ThisComputer,
    Wifi,
    Network,
}

/// Why a find is not a vehicle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum FindReason {
    /// Opened, but no valid MAVLink at any speed tried.
    NoMavlink,
    /// A known board in its bootloader.
    Bootloader,
    /// Another program has it; `detail` names it when the system says.
    Busy,
    /// Not allowed to open it; `detail` says how to fix that.
    Permission,
    /// A flight controller that did not answer a ground station's heartbeat.
    SilentKnownBoard,
    /// The gateway answered on neither port.
    Unreachable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FoundVehicle {
    pub system_id: u8,
    pub vehicle_type: String,
    pub autopilot: String,
    pub armed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// What a search found at one place.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Find {
    pub id: String,
    pub place: FindPlace,
    /// `link/add` with this connects it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<LinkKind>,
    pub evidence: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usb: Option<UsbId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle: Option<FoundVehicle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<FindReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub seen_us: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FindStartParams {
    pub scope: FindScope,
    /// Look at serial devices the known-id table does not know too ("Look
    /// at every serial port").
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all_serial: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FindResult {
    pub finds: Vec<Find>,
    /// The subnets a network search covers, such as `192.168.1.0/24`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subnets: Vec<String>,
}
