//! Vehicles on the wire (ADR-0004).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Presence {
    /// Heartbeats arriving on time.
    Online,
    /// Overdue by a short margin: probably a dropout.
    Stale,
    /// Silent past the long threshold.
    Offline,
}

/// A vehicle: its id, where it is reached, what it is and whether it is heard.
#[allow(
    clippy::struct_excessive_bools,
    reason = "what the heartbeat says, and the user's choices about it"
)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Vehicle {
    /// The VID: a UUID v5 of the autopilot's UID once known.
    pub id: String,
    pub name: String,
    /// Earlier ids of the same vehicle, such as the provisional one.
    pub aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connections: Option<VehicleConnections>,
    /// Connections added for it with `link/add`'s `forVehicle` that have
    /// not heard it yet (S261), by link id: shown on its page as waiting.
    /// None is a route, so none is ever chosen for commands. Once one hears
    /// the vehicle it is in `connections` instead. Absent when none waits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waiting_links: Vec<String>,
    pub link_id: String,
    /// The source address it is heard from on its link.
    pub peer: String,
    pub system_id: u8,
    pub component_ids: Vec<u8>,
    /// `MAV_AUTOPILOT`, such as `Ardupilotmega` or `Px4`.
    pub autopilot: String,
    /// `MAV_TYPE`, such as `Quadrotor` or `FixedWing`.
    pub vehicle_type: String,
    /// The autopilot's hardware id, from `AUTOPILOT_VERSION`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    /// The first 8 characters of the git hash it was built from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware_commit: Option<String>,
    pub presence: Presence,
    /// Latest received heartbeat on direct vehicle reads; state snapshots
    /// keep the timestamp of the last persisted vehicle change.
    #[schemars(schema_with = "crate::safe::integer")]
    pub last_heartbeat_us: u64,
    /// Age at the time of a direct vehicle read. Absent on persisted state
    /// snapshots and hosts that cannot observe individual heartbeats.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub heartbeat_age_ms: Option<u64>,
    /// From the heartbeat.
    pub armed: bool,
    /// The flight mode by ArduPilot's name, such as `LOITER`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    pub saved: bool,
    pub dismissed: bool,
    /// Hidden by the person, set with `vehicle/hide`: out of every list
    /// until it is seen again or unhidden, kept with its name, flights and
    /// history. Absent when not hidden.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    /// Met only in a log someone opened, never heard live: a real board's
    /// UID, kept so the name given it sticks. Absent once it is heard.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub seen_in_log: bool,
    /// Kept out of the account's library although sync is on (S16).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub local_only: bool,
    /// The person's labels on it, such as "Survey" (S106): any number,
    /// meaning nothing to UXDuck. Absent when it has none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Web pages kept with it, such as its radio's settings page, set
    /// with `vehicle/bookmarks`. Absent when it has none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bookmarks: Vec<VehicleBookmark>,
    /// Its length, or wingspan, in centimetres, set with `vehicle/size` for
    /// the 3D map. Absent, the map draws it the size of the one its
    /// autopilot's simulator flies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_cm: Option<u32>,
    /// Its gripper, as the operator identified it with `vehicle/gripper`
    /// (S263). Absent when it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gripper: Option<crate::dto::gripper::GripperSetup>,
    /// Its companion computers, added with `computer/add` (S296). Absent
    /// when it has none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub companions: Vec<crate::dto::companion::CompanionComputer>,
    /// The traffic its ADS-B receiver reports is used, set with
    /// `vehicle/traffic` (S168). Absent when it is not.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub traffic: bool,
    /// The model it is drawn as, set with `vehicle/look`. Absent, the one
    /// its type gives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub look: Option<VehicleLook>,
    /// Its own 3D model, set with `vehicle/look`'s `file`: drawn in place
    /// of `look` and the one its type gives, read with
    /// `vehicle/modelRead`. Absent when it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own_model: Option<OwnModel>,
    /// Its colour, given when it was first heard or chosen with
    /// `vehicle/colour`, kept with its id. Absent only on a vehicle kept
    /// from before colours were given and not heard since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colour: Option<VehicleColour>,
    /// A replay vehicle (ADR-0039 R3): a flight played back, not live.
    /// Nothing can be sent to it and nothing is recorded of it. Absent when
    /// live.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replay: bool,
    /// A replay vehicle's original, when it is here: its name, look and
    /// size are the original's, and renaming it renames the original.
    /// Absent for a live vehicle and for a replay with no original here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_of: Option<String>,
    /// When its source begins, for a replay vehicle shown twice at once:
    /// two sources hold the same vehicle (ADR-0039 R20), and each copy
    /// says when it was. Absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub replay_at_us: Option<u64>,
    /// It said it is simulated: its SITL has `SIM_` parameters (ADR-0039
    /// R14). Absent otherwise. A simulation of UXDuck's own is simulated by
    /// its link.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sim_evidence: bool,
    /// The person's word, set with `vehicle/simulated`, over the evidence.
    /// Absent until they say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simulated: Option<bool>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleListResult {
    pub vehicles: Vec<Vehicle>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleSaveParams {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Keep it out of the account's library, or put it back (S16).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_only: Option<bool>,
}

/// A model the person chose to draw a vehicle with (ADR-0035 M5), in place
/// of the one its type gives: UXDuck's own models only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum VehicleLook {
    Plane,
    Vtol,
    Multicopter,
    Rover,
    Boat,
    Submarine,
    Airship,
    Jet,
}

/// A 3D model of the person's own that a vehicle is drawn with (ADR-0035
/// M5): a binary glTF (`.glb`), or a `.gltf` with its buffers and images
/// inside it, kept by UXDuck.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OwnModel {
    /// Where UXDuck keeps the file: its SHA-256, in hex. The same file for
    /// two vehicles is kept once.
    pub file: String,
    /// The file's name as the person chose it, such as `skywalker.glb`.
    pub name: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
    /// How far it is turned clockwise, seen from above, from the front
    /// glTF gives it (+Z): 0, 90, 180 or 270 degrees.
    pub turn_deg: u16,
    /// Why it may slow the maps down and what to change, in words, when
    /// it has more triangles, wider textures or more separately drawn
    /// parts than a vehicle on a map needs. It is still drawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// The largest file `vehicle/look` takes as a vehicle's own model.
pub const OWN_MODEL_MAX: u64 = 32 * 1024 * 1024;

/// The most of an own model `vehicle/modelRead` gives at once, before base64.
pub const OWN_MODEL_CHUNK: u64 = 4 * 1024 * 1024;

/// `vehicle/look`: the model a vehicle is drawn as on both maps, such as a
/// copter drawn as a boat it rides on, or a model of the person's own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleLookParams {
    /// The vehicle, by id or an earlier alias.
    pub id: String,
    /// One of UXDuck's models. Absent, with no `file` or `turnDeg`, gives
    /// it back the model its type gives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub look: Option<VehicleLook>,
    /// A model of the person's own instead: the id of a file uploaded with
    /// `attachment/begin` or attached to a message, a `.glb` or a
    /// self-contained `.gltf` of up to 32 MB. Not with `look`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Turns its own model clockwise, seen from above, from the front glTF
    /// gives it: 0, 90, 180 or 270. With `file`, or alone to turn the one
    /// it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_deg: Option<u16>,
}

/// `vehicle/modelRead`: the bytes of a vehicle's own model, to draw it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleModelReadParams {
    /// The vehicle, by id or an earlier alias.
    pub id: String,
    /// Where to start, in bytes; from the start when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub offset: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleModelReadResult {
    /// The file, as `ownModel.file` names it.
    pub file: String,
    /// The whole file's size.
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
    /// Up to 4 MiB of it from `offset`, base64; read on from `offset` plus
    /// what came until `bytes`.
    pub content_base64: String,
}

/// A vehicle's colour on the maps, in the sidebar and on its mission
/// (ADR-0029 P19). None is green, red, amber, orange or grey, which show
/// fences and whether a vehicle is heard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum VehicleColour {
    Blue,
    Pink,
    Teal,
    Purple,
    Lilac,
    Orchid,
    Violet,
    Sky,
    Magenta,
}

/// `vehicle/colour`: the colour a vehicle is drawn in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleColourParams {
    /// The vehicle, by id or an earlier alias.
    pub id: String,
    pub colour: VehicleColour,
}

/// `vehicle/simulated`: whether a vehicle is a simulation, as the person
/// says, over what it said itself (ADR-0039 R14).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleSimulatedParams {
    /// The vehicle, by id or an earlier alias.
    pub id: String,
    /// True or false; absent goes back to its own word.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simulated: Option<bool>,
}

/// `vehicle/hide`: hides an offline vehicle from every list until it is
/// seen again, or shows it again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleHideParams {
    /// The vehicle, by id or an earlier alias.
    pub id: String,
    /// True hides it; false shows it again.
    pub hidden: bool,
}

/// `vehicle/size`: a vehicle's length, or wingspan, for the 3D map.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleSizeParams {
    /// The vehicle, by id or an earlier alias.
    pub id: String,
    /// Centimetres, 1 to 100000 (1 km). Absent gives it back to the size
    /// of the one its autopilot's simulator flies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_cm: Option<u32>,
}

/// `vehicle/label`: labels put on and taken off several vehicles at once
/// (S106). Taking one off every vehicle that carries it removes it; taking
/// it off and putting another on renames it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleLabelParams {
    /// The vehicles, by id or an earlier alias.
    pub ids: Vec<String>,
    /// Labels to put on each; one already in use, in any case, keeps the
    /// spelling it has.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add: Vec<String>,
    /// Labels to take off each, in any case.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// A web page kept with a vehicle, such as the settings page its radio
/// serves on the local network.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleBookmark {
    /// 1 to 40 characters, such as "Radio".
    pub name: String,
    /// An `http` or `https` address, such as `http://192.168.1.20`.
    pub url: String,
}

/// `vehicle/bookmarks`: a vehicle's web pages, all of them, in order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleBookmarksParams {
    /// The vehicle, by id or an earlier alias.
    pub id: String,
    /// Any number; an empty list removes them all.
    pub bookmarks: Vec<VehicleBookmark>,
}

/// `vehicle/forgetConnection`: one connection off one vehicle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleForgetConnectionParams {
    /// The vehicle, by id or an earlier alias.
    pub id: String,
    /// The connection, by its link id.
    pub link_id: String,
}

/// `vehicle/useConnection`: one vehicle onto one of its connections now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleUseConnectionParams {
    /// The vehicle, by id or an earlier alias.
    pub id: String,
    /// The connection, by its link id.
    pub link_id: String,
}

/// `vehicle/connectionHistory`: how a vehicle's connections have done.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleConnectionHistoryParams {
    /// The vehicle, by id or an earlier alias.
    pub id: String,
    /// Only samples after this time, the daemon's clock in Unix
    /// microseconds. Without it, every sample kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub after_us: Option<u64>,
}

/// `vehicle/connectionHistory`'s result (ADR-0004 2026-11-16): each of a
/// vehicle's connections a second at a time, oldest first, kept in memory
/// for `keptS` while UXDuck runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleConnectionHistory {
    pub vehicle_id: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub now_us: u64,
    /// How far back samples go: the last 30 minutes.
    pub kept_s: u32,
    pub connections: Vec<ConnectionHistory>,
}

/// One connection's samples, by its link id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionHistory {
    pub link_id: String,
    pub samples: Vec<ConnectionSample>,
}

/// One second of a connection. A value it did not have then is absent.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionSample {
    #[schemars(schema_with = "crate::safe::integer")]
    pub time_us: u64,
    /// It carried the vehicle's commands, or its telemetry while none
    /// could carry commands.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub in_use: bool,
    /// UXDuck counted the vehicle not responding on it then, as its
    /// Connections list says: absent while it was up.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub down: bool,
    /// UXDuck heard the vehicle on it then but the vehicle did not answer
    /// on it, as its Connections list says "Receiving only": absent
    /// otherwise, and while it was down.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub receiving_only: bool,
    /// MAVLink's round trip on it, the median of the last 5 s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round_trip_ms: Option<f64>,
    /// The share of the vehicle's messages lost on it over the last 10 s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss_percent: Option<f64>,
    /// Since the vehicle was last heard on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub age_ms: Option<u64>,
    /// What the vehicle's open cameras received over it, in bits a second:
    /// 0 while a camera with an address on it plays on another. Absent
    /// when no open camera has an address on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub camera_bps: Option<u64>,
    /// Its telemetry radio's signal (`RADIO_STATUS`), as a share of the
    /// radio's own scale, while the radio reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rssi_percent: Option<f64>,
    /// The signal at the radio on the other end, the same way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_rssi_percent: Option<f64>,
    /// The noise its telemetry radio hears, on the same scale as its
    /// signal, while the radio reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noise_percent: Option<f64>,
    /// The noise the radio on the other end hears, the same way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_noise_percent: Option<f64>,
    /// How much of its telemetry radio's transmit buffer is free, in
    /// percent, while the radio reports it. Near 0, the radio is given more
    /// than it can send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radio_buffer_percent: Option<f64>,
    /// The MAVLink UXDuck received on it in the last second, in bits a
    /// second, from every system on it. Absent before UXDuck has measured
    /// a second of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub received_bps: Option<u64>,
    /// The MAVLink UXDuck sent on it in the last second, the same way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub sent_bps: Option<u64>,
}

/// Discovered connections only. Configuring a link is never vehicle evidence.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleConnections {
    pub routes: Vec<VehicleRoute>,
    pub command_available: bool,
    pub reason: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub epoch: u64,
    /// What the connection in use is doing that keeps UXDuck from going
    /// back to one higher in the order by itself, while another connection
    /// could take over. `vehicle/useConnection` goes ahead anyway.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub holds: Vec<RouteHold>,
    /// The link a person switched the vehicle to with
    /// `vehicle/useConnection`, kept until it stops answering, they switch
    /// again or the order changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chosen: Option<String>,
    /// A link higher in the order that has answered steadily for 10
    /// seconds, which UXDuck moves to once nothing in `holds` is left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returns_to: Option<String>,
}

/// Why UXDuck stays on the connection in use rather than go back to one
/// higher in the order: the vehicle is armed, or switching would end a
/// command waiting for its answer, a parameter, plan or file transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RouteHold {
    Armed,
    Command,
    Parameters,
    Plan,
    Files,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent transport capability and selection evidence"
)]
pub struct VehicleRoute {
    pub link_id: String,
    pub peer: String,
    pub system_id: u8,
    pub priority: u32,
    pub reachable: bool,
    pub writable: bool,
    pub selected: bool,
    pub telemetry: bool,
    #[schemars(schema_with = "crate::safe::integer")]
    pub last_heartbeat_us: u64,
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub round_trip_us: Option<u64>,
    pub loss_per_mille: Option<u32>,
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub last_receive_age_ms: Option<u64>,
    pub reason: String,
    /// Kept from before and not heard since UXDuck started (S261): never
    /// selected, and `vehicle/forgetConnection` can take it off the
    /// vehicle. Absent for a route heard in this run.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub remembered: bool,
}
