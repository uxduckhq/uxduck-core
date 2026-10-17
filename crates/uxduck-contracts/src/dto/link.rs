//! Links on the wire (ADR-0004).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A USB serial adapter's identity: a replugged or re-enumerated device is
/// found again by this, whatever its path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UsbId {
    pub vendor_id: u16,
    pub product_id: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial_number: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum LinkKind {
    /// Listens on a UDP port; every source address is a peer.
    UdpListen { host: String, port: u16 },
    /// A serial device, such as an autopilot on USB or a telemetry radio.
    /// With `usb`, it is found by that identity rather than by path.
    Serial {
        device: String,
        baud: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usb: Option<UsbId>,
    },
    /// Connects to a TCP server, such as SITL's; reconnects with backoff.
    TcpClient { host: String, port: u16 },
    /// Listens for TCP connections, such as a companion computer's; every
    /// connection is a peer.
    TcpServer { host: String, port: u16 },
    /// Sends to a UDP server, such as a telemetry bridge or mavlink-router,
    /// and hears the vehicles behind it. It says hello (a ground station
    /// heartbeat, once a second) whenever the server has been quiet for 3 s,
    /// since a UDP server sends only to whoever has spoken to it.
    UdpClient { host: String, port: u16 },
    /// Sends vehicle traffic to another program or daemon over UDP. With
    /// `read-write`, what it sends back reaches the vehicles.
    UdpForward { host: String, port: u16 },
    /// A simulated vehicle this daemon runs, starting at a place. Only a
    /// daemon whose `initialize` says `simulator` accepts it.
    Simulator {
        vehicle: SimVehicle,
        /// Which autopilot flies it; ArduPilot if absent.
        #[serde(default, skip_serializing_if = "SimAutopilot::is_ardupilot")]
        autopilot: SimAutopilot,
        latitude_deg: f64,
        longitude_deg: f64,
        /// Ground level at the start, above mean sea level.
        altitude_m: f64,
        heading_deg: f64,
        /// ArduPilot's frame, such as `X` or `quadplane-tilt`; absent for
        /// the vehicle's default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        frame: Option<String>,
        /// Times real time, 1 to 10; absent for 1.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        speedup: Option<u8>,
        /// Starts flying at this height above the ground in metres,
        /// holding position; absent to start on the ground.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        in_air_m: Option<f64>,
        /// Started from this simulation snapshot (S34): its parameters and
        /// mission are written back, and its mode restored.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        snapshot_id: Option<String>,
        /// A SITL build of the user's own, instead of the one UXDuck has
        /// for the vehicle. Installed daemons only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binary: Option<String>,
        /// Forgets the vehicle's stored parameters each time it starts.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        wipe: bool,
        /// Sends it its type's example as its mission once it is up (S131),
        /// ready to fly. Off unless asked for: Quick start asks.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        with_mission: bool,
        /// Which instance it is, for its ports and system id. The daemon
        /// assigns it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instance: Option<u8>,
    },
}

// The simulator's place is checked finite when the link is added, so no
// link holds a NaN and equality is total.
impl Eq for LinkKind {}

/// What a simulated vehicle is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SimVehicle {
    Plane,
    Multicopter,
    Vtol,
    /// A ground rover: `ArduRover` (S37).
    Rover,
    /// A boat on the water: `ArduRover`'s motorboat.
    Boat,
    /// `ArduSub`'s vectored submarine.
    Submarine,
    /// ArduPilot Blimp: a lighter-than-air vehicle.
    Airship,
}

/// The autopilot a simulated vehicle runs: ArduPilot, or PX4 with its SIH
/// physics on an installed UXDuck (macOS and Linux).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SimAutopilot {
    #[default]
    Ardupilot,
    Px4,
}

impl SimAutopilot {
    #[must_use]
    pub fn is_ardupilot(&self) -> bool {
        *self == Self::Ardupilot
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum LinkPolicy {
    Read,
    ReadWrite,
}

/// How components on the link group into vehicles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Topology {
    Auto,
    Single,
    Multi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum LinkStatus {
    Configured,
    Up,
    Down,
    PortInUse,
    /// A forward whose frames came back to the daemon; off until `link/enable`.
    DisabledByLoopDetection,
    /// Turned off with `link/disable`; kept, not opened, until `link/enable`.
    Disabled,
    /// A simulation from an earlier run, not started until `sim/restart`.
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Link {
    pub id: String,
    pub name: String,
    pub kind: LinkKind,
    pub policy: LinkPolicy,
    pub topology: Topology,
    pub status: LinkStatus,
    /// Why it is down or refused, such as the bind error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_detail: Option<String>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub created_at_us: u64,
    /// For a forward: the vehicles whose traffic it carries; empty for all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vehicle_filter: Vec<String>,
    /// For a forward a simulation start made: the simulated vehicle's link,
    /// whose traffic alone it carries. It is removed with that link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_link: Option<String>,
    /// Keeps the daemon running when the last window closes.
    pub keep_alive: bool,
    /// A simulated vehicle started in the air: how the take-off that its
    /// start runs is going.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<Launch>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

/// The take-off a simulation start runs for a vehicle started in the air.
/// Its commands are recorded as the simulation start's (`simulation-start`),
/// never a person's or the agent's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Launch {
    pub stage: LaunchStage,
    /// Why it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum LaunchStage {
    /// Putting a snapshot's parameters and mission back, on the ground
    /// (S34).
    Restoring,
    /// Getting the vehicle into the air.
    Climbing,
    /// Flying, holding position, handed over.
    Flying,
    /// It did not get into the air; `detail` says why.
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LinkAddParams {
    /// Defaults to a description of the kind, such as "UDP 15010".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub kind: LinkKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<LinkPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topology: Option<Topology>,
    /// For a forward: only these vehicles' traffic. Absent or empty for all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vehicle_filter: Vec<String>,
    /// For a UDP forward: carry only this simulator link's traffic and
    /// remove the forward when that simulation stops. Cannot be combined
    /// with a vehicle filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_link: Option<String>,
    /// Keeps the daemon running when the last window closes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keep_alive: bool,
}

/// `link/update`: what changes on a link; everything else stays. A new
/// `kind` is checked as `link/add` checks one, and the link reopens with it
/// (and with a new `policy`, `topology` or `vehicleFilter`), keeping its id,
/// its name and its vehicles. A simulated vehicle's link takes only `name`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LinkUpdateParams {
    pub id: String,
    /// Empty for the default name of its kind, such as "UDP 15010".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<LinkKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<LinkPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topology: Option<Topology>,
    /// For a forward: only these vehicles' traffic; empty for all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_filter: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_alive: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IdParams {
    pub id: String,
}

/// A serial port the system has, for the add-link screen.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SerialPort {
    /// The path to open now, such as `/dev/cu.usbmodem1101` or `COM3`.
    pub device: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usb: Option<UsbId>,
    /// Maker and product, when the system reports them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SerialListResult {
    pub ports: Vec<SerialPort>,
}

/// The configured links, including disabled and failed links and their reasons.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LinkListResult {
    pub links: Vec<Link>,
}
