//! Links: the persisted connection configuration the user creates, plus its
//! runtime status. A link is never a vehicle (ADR-0004).

use serde::{Deserialize, Serialize};

/// A USB serial adapter's identity, so a replugged or re-enumerated device is
/// found again whatever its path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsbId {
    pub vendor_id: u16,
    pub product_id: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial_number: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum LinkKind {
    /// Listens on a UDP port; each source address is a peer.
    UdpListen { host: String, port: u16 },
    /// A serial device. With a USB identity it is found by that, not by path.
    Serial {
        device: String,
        baud: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usb: Option<UsbId>,
    },
    /// Connects to a TCP server, such as SITL's, and reconnects with backoff.
    TcpClient { host: String, port: u16 },
    /// Listens for TCP connections; each connection is a peer.
    TcpServer { host: String, port: u16 },
    /// Sends to a UDP server, such as a telemetry bridge, and hears the
    /// vehicles behind it; says hello while it hears nothing.
    UdpClient { host: String, port: u16 },
    /// Sends vehicle traffic to a UDP destination (another program or
    /// daemon); with `read-write`, what comes back reaches the vehicles.
    UdpForward { host: String, port: u16 },
    /// A simulated vehicle this daemon runs, starting at a place: in a
    /// browser, ArduPilot's WebAssembly build (S20, ADR-0011).
    Simulator {
        vehicle: SimVehicle,
        /// Which autopilot flies it: ArduPilot unless PX4 is asked for.
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
        /// Starts flying at this height above the ground, holding position,
        /// after a take-off the simulation start runs (S33, ADR-0011 M13);
        /// absent to start on the ground.
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

/// What a simulated vehicle is (ADR-0011): the ones ArduPilot's
/// WebAssembly build flies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// A fast jet drone (S143): `ArduPlane`'s `plane-jet` frame with
    /// UXDuck's faster tuning, drawn as the jet model.
    Jet,
}

/// `ArduPlane`'s frame a jet flies (S143): its own physics, a 22 kg jet
/// with a level top speed near 100 m/s.
pub const JET_FRAME: &str = "plane-jet";

/// The autopilot a simulated vehicle runs (S17): ArduPilot, or PX4 with
/// its SIH physics on an installed UXDuck.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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

impl LinkKind {
    /// A forward carries vehicle traffic out; the others hear vehicles.
    #[must_use]
    pub fn is_forward(&self) -> bool {
        matches!(self, Self::UdpForward { .. })
    }

    /// A simulated vehicle's link: what it carries stays where it ran.
    #[must_use]
    pub fn is_simulator(&self) -> bool {
        matches!(self, Self::Simulator { .. })
    }
}

impl LinkStatus {
    /// Off on purpose: by the user, by loop detection, or a simulation not
    /// started again. A late report from the data plane does not turn such a
    /// link on, and a restart does not open a link the user turned off.
    #[must_use]
    pub fn is_off(self) -> bool {
        matches!(
            self,
            Self::Disabled | Self::DisabledByLoopDetection | Self::Stopped
        )
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LinkPolicy {
    Read,
    #[default]
    ReadWrite,
}

/// How components on the link group into vehicles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Topology {
    /// `single` while one autopilot has been seen, `multi` after a second.
    #[default]
    Auto,
    Single,
    Multi,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LinkStatus {
    #[default]
    Configured,
    Up,
    Down,
    PortInUse,
    /// A forward whose frames came back: off until the user turns it on again.
    DisabledByLoopDetection,
    /// Turned off by the user (`link/disable`): kept, not opened, until
    /// `link/enable`. Its settings can still be changed.
    Disabled,
    /// A simulation from an earlier run of UXDuck, kept with its vehicle
    /// and not started until `sim/restart` (maintainer, 2026-10-10: a start
    /// costs time and the computer's resources, so it is offered, not done).
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// For a forward: the VIDs whose traffic it carries; empty for all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vehicle_filter: Vec<String>,
    /// For a forward a simulation start made (S102): the simulator link
    /// whose traffic alone it carries, and with which it is removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_link: Option<String>,
    /// Keeps the daemon running when the last window closes (ADR-0001
    /// shutdown policy).
    #[serde(default)]
    pub keep_alive: bool,
    /// A simulated vehicle started in the air: how its take-off is going.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<Launch>,
    pub created_at_us: u64,
    pub version: u64,
}

/// Simulation preparation, take-off (S33), or snapshot restoration (S34).
/// Its commands are the simulation start's, not a person's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Launch {
    pub stage: LaunchStage,
    /// Preparation status, or why it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LaunchStage {
    /// Preparing a submarine or putting a snapshot's parameters and
    /// mission back (S34).
    Restoring,
    /// Getting the vehicle into the air.
    Climbing,
    /// Flying and holding; handed over.
    Flying,
    /// It stayed on the ground (or holds where it got to).
    Failed,
}
