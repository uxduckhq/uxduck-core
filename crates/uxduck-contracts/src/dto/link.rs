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
    /// A USB receiver stick on this computer (S173): UXDuck starts a decoder
    /// (readsb or dump1090-fa) on it and reads its SBS-1 output. It carries
    /// traffic, never MAVLink.
    UsbReceiver {},
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
        /// Allows this own ArduPilot build to listen on the network when
        /// local-only connections are unsupported.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        allow_network: bool,
        /// Forgets the vehicle's stored parameters each time it starts.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        wipe: bool,
        /// Sends it its type's example as its mission once it is up (S131),
        /// ready to fly. Off unless asked for: Quick start asks.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        with_mission: bool,
        /// Hears a few simulated aircraft from ArduPilot's ADS-B simulator
        /// (ADR-0034). Off unless asked for: beside real traffic they
        /// would confuse.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        traffic: bool,
        /// Carries a simulated camera: component 100 of the vehicle, which
        /// answers MAVLink's camera protocol and which the autopilot speaks
        /// for (ADR-0063). UXDuck Desktop and ArduPilot only.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        camera: bool,
        /// Carries a simulated gimbal: component 154 of the vehicle, a
        /// gimbal device that the autopilot manages (ADR-0063). UXDuck
        /// Desktop and ArduPilot only.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        gimbal: bool,
        /// Carries a gimbal that ArduPilot simulates itself, in place of
        /// UXDuck's simulated camera and gimbal (ADR-0063). UXDuck Desktop
        /// and ArduPilot only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ardupilot_gimbal: Option<ArdupilotGimbal>,
        /// What kind of camera and gimbal its simulated payload is
        /// (ADR-0063). Absent for a camera that takes photos, records,
        /// zooms and focuses, with a gimbal apart from it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload_profile: Option<PayloadProfile>,
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
    /// A fast jet drone (S143): `ArduPlane`'s `plane-jet` frame with
    /// UXDuck's faster tuning, drawn as the jet model.
    Jet,
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

/// A gimbal that ArduPilot's simulator plays itself (ADR-0063), with the
/// camera it carries. The autopilot's own driver speaks for it, as it would
/// for hardware on a serial port. It sends no video.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ArdupilotGimbal {
    /// A Viewpro gimbal and camera on ArduPilot's serial driver. Only the
    /// autopilot speaks MAVLink for them.
    Viewpro,
    /// An AVT CM62: a MAVLink gimbal and camera, components 154 and 100 of
    /// the vehicle, behind the autopilot.
    AvtCm62,
}

/// What kind of camera and gimbal a simulation's payload is (ADR-0063), to
/// see what UXDuck does with hardware that can do less or is put together
/// another way. Each needs the simulated camera, and `integrated` and
/// `thermal-tracking` the gimbal too.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PayloadProfile {
    /// A camera that only sends video. It takes no photos, records
    /// nothing, and has no modes, lens or card.
    StreamOnly,
    /// A camera that takes photos in its photo mode and records in its
    /// video mode, never both at once, with no zoom or focus to drive.
    PhotoVideo,
    /// A camera and a gimbal that are one MAVLink component, the
    /// camera's, which manages its gimbal itself. The autopilot has no
    /// mount.
    Integrated,
    /// Such a unit with a thermal sensor beside its daylight one, which
    /// tracks a point or a box in its picture and keeps its gimbal on it.
    /// Its thermal picture is the daylight one in false colour, and the
    /// range it gives for it is a scale from 0 to 100 and no temperature.
    ThermalTracking,
}

/// What can go wrong with a simulation's camera or gimbal when it is asked
/// to (`sim/fault`, ADR-0063). Each is what hardware with that fault says
/// on the wire. Nothing else tells UXDuck of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SimFaultKind {
    /// The camera stops talking: no heartbeats and no answers. An
    /// integrated camera and gimbal stop together.
    CameraSilent,
    /// The gimbal stops talking: no heartbeats, no attitude and no
    /// answers.
    GimbalSilent,
    /// The camera's card is full. Photos and recordings fail until the
    /// card is formatted or the fault is switched off.
    CardFull,
    /// One command, named by `command`, is answered `MAV_RESULT_DENIED`
    /// and not carried out.
    CommandRefused,
    /// One command, named by `command`, takes five seconds: it is
    /// answered `MAV_RESULT_IN_PROGRESS` with its progress, and carried
    /// out at the end.
    CommandInProgress,
    /// The gimbal reports that its motors have failed, and stays where it
    /// is.
    GimbalError,
    /// The vehicle's autopilot is in primary control of the gimbal, until
    /// another takes control.
    AutopilotHolds,
}

/// A fault of a simulation's camera or gimbal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SimFault {
    pub kind: SimFaultKind,
    /// The `MAV_CMD` number a `command-refused` or
    /// `command-in-progress` fault is about, such as 2000 for
    /// `MAV_CMD_IMAGE_START_CAPTURE`. `sim/faults` lists the ones a
    /// simulation's camera and gimbal have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<u16>,
}

/// What a link's bytes are (S172, ADR-0034): MAVLink from vehicles, or
/// traffic from a receiver on the ground. A traffic link is read-only and
/// a traffic source of its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum LinkCarries {
    #[default]
    Mavlink,
    /// SBS-1 (`BaseStation`) text, as readsb serves on TCP 30003.
    Sbs,
    /// GDL90, as Stratux broadcasts on UDP 4000.
    Gdl90,
}

impl LinkCarries {
    #[must_use]
    pub fn is_mavlink(&self) -> bool {
        *self == Self::Mavlink
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

/// What kind of failure stopped a link, whatever the operating system
/// (FB-267).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum LinkFailureKind {
    NoAnswer,
    Refused,
    Reset,
    Aborted,
    HostUnreachable,
    NetworkUnreachable,
    AddressNotAvailable,
    AddressInUse,
    NameNotFound,
    DeviceMissing,
    DeviceUnavailable,
    DeviceBusy,
    AccessDenied,
    Other,
}

/// What a link was doing when it failed (FB-267).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum LinkOperation {
    Connect,
    Connected,
    Lookup,
    Bind,
    Socket,
    Datagram,
    Device,
}

/// A link's failure as the operating system reported it (FB-267).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LinkFailure {
    pub kind: LinkFailureKind,
    pub operation: LinkOperation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_code: Option<i32>,
    /// The system's own message, kept whole.
    pub message: String,
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
    /// Why it is down or refused, such as the bind error, in plain words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_detail: Option<String>,
    /// What the operating system said when it failed, which `statusDetail`
    /// puts in plain words, for Connection diagnostics (FB-267).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_error: Option<LinkFailure>,
    /// A vehicle link that has stayed down for a while without reaching a
    /// vehicle in this run of the daemon, so its address may be one the
    /// vehicle no longer uses (FB-267).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keeps_failing: bool,
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
    /// MAVLink, or traffic from a receiver on the ground (S172).
    #[serde(default, skip_serializing_if = "LinkCarries::is_mavlink")]
    pub carries: LinkCarries,
    /// A simulated vehicle started in the air: how the take-off that its
    /// start runs is going.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<Launch>,
    /// A simulated vehicle frozen where it is (`sim/pause`): no time passes
    /// for it until it resumes, and commands wait for that.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paused: bool,
    /// Where a simulated vehicle touched the 3D scenery and was paused
    /// (`sim/crash`). Once it resumes, contact within 100 m of it across
    /// the ground is not reported, until the vehicle has flown clear.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scenery_hit: Option<SceneryHit>,
    /// The faults that are on in a simulated vehicle's camera and gimbal
    /// (`sim/fault`). They last until they are switched off or the
    /// simulation stops.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub faults: Vec<SimFault>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

/// Where on the ground a simulated vehicle touched the 3D scenery, in
/// degrees times 10⁷.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SceneryHit {
    pub latitude_e7: i32,
    pub longitude_e7: i32,
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
    /// MAVLink unless a receiver's traffic (S172): `sbs` on a TCP client,
    /// `gdl90` on a UDP listener. A traffic link is read-only.
    #[serde(default, skip_serializing_if = "LinkCarries::is_mavlink")]
    pub carries: LinkCarries,
    /// Added for this vehicle, by id or an earlier alias (S261): the
    /// vehicle's page shows it as waiting until it hears the vehicle. Only
    /// a connection that carries MAVLink from vehicles, never a forward.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub for_vehicle: Option<String>,
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
