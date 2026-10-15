//! The edge connection (ADR-0003 C7, S78): one outbound WebSocket from a
//! signed-in UXDuck to the relay, a Durable Object per computer in the
//! cloud, speaking JSON-RPC 2.0 as on the computer (ADR-0001 D2). The first
//! methods: the computer says who it is and pings, which also gives its
//! clock's offset from the relay's and the round trip; whoever watches it
//! sees whether it is online. The relay uses these through
//! `packages/protocol`, so these stay the source of truth.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `edge/hello`, the computer's first request on a new connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EdgeHello {
    /// The UXDuck that connects, such as `0.1.0`.
    pub daemon_version: String,
}

/// The answer to `edge/hello`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EdgeWelcome {
    /// The computer as the account knows it.
    pub computer: Computer,
}

/// `edge/ping`, every few seconds, so a computer that went quiet shows as
/// offline; the answer dates it on the relay's clock.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EdgePing {
    /// The computer's clock when it sent it, in milliseconds since the Unix epoch.
    #[schemars(schema_with = "crate::safe::integer")]
    pub sent_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EdgePong {
    #[schemars(schema_with = "crate::safe::integer")]
    pub sent_at_ms: u64,
    /// The relay's clock when it answered.
    #[schemars(schema_with = "crate::safe::integer")]
    pub relay_at_ms: u64,
}

/// One of the account's computers: a UXDuck that has connected to it.
/// `edge/presence` tells whoever watches it when this changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Computer {
    /// The computer's device id, as sync uses it: `dv` and 16 hex digits.
    pub id: String,
    pub name: String,
    pub online: bool,
    /// When it was last heard, in milliseconds since the Unix epoch; while
    /// online, about now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub last_seen_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_version: Option<String>,
}

/// `GET /v1/computers`: the account's computers, online first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComputerList {
    pub computers: Vec<Computer>,
}

/// This computer's connection to its account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum EdgeState {
    /// Not signed in: nothing leaves the computer.
    SignedOut,
    /// "Connect this computer to my account" is off.
    Off,
    Connecting,
    Connected,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EdgeStatus {
    pub state: EdgeState,
    /// The round trip to the relay, at the last ping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub round_trip_ms: Option<u64>,
    /// The relay's clock minus this computer's, at the last ping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_signed_integer")]
    pub clock_offset_ms: Option<i64>,
    /// Why it is not connected, for people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// `account/computers`: the account's computers, which one this is, and how
/// its own connection is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountComputers {
    pub computers: Vec<Computer>,
    /// This computer's id once it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub this_computer: Option<String>,
    pub edge: EdgeStatus,
}

// Live telemetry's fast path (S79, ADR-0003 C8): the relay asks for what
// viewers watch; the computer sends the latest values of those vehicles,
// never a queue; the relay keeps the latest and a short trail, numbers what
// it sends each viewer, and dates every value on its own clock so viewers
// can show how old it is.

/// `edge/watch`, a viewer's request: one vehicle in detail, or the list.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EdgeWatch {
    /// The vehicle open in detail; none for the list alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
}

/// `edge/demand`, the relay to the computer: what anyone is watching. Empty
/// when nobody is, and the computer then sends nothing but its pings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EdgeDemand {
    /// Vehicles open in detail somewhere: up to 10 times a second.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub detail: Vec<String>,
    /// Someone has the list open: every vehicle about once a second.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub summary: bool,
    /// Cameras someone away is watching (S84): the computer publishes each
    /// one's live rendition to a media node while it is listed here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub video: Vec<String>,
    /// Guests watching through each watch link (S95, ADR-0023 W6), so the
    /// owner sees "12 watching" on the vehicle.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub viewers: Vec<WatchViewers>,
}

/// How many guests watch through one watch link now (S95).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WatchViewers {
    pub link_id: String,
    /// The vehicle the link shows; none for every vehicle on the computer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    pub count: u32,
}

/// A vehicle as it leaves the computer: only what a viewer shows. Not its
/// link, address or hardware id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RemoteVehicle {
    pub id: String,
    pub name: String,
    /// `MAV_TYPE`, such as `Quadrotor`.
    pub vehicle_type: String,
    pub autopilot: String,
    /// Its size on the map in centimetres, as the person gave it; absent,
    /// the viewer draws it the size of the one its autopilot's simulator flies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_cm: Option<u32>,
    pub presence: crate::dto::vehicle::Presence,
    pub armed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Its fused position and the values a viewer shows (attitude, speeds,
    /// battery, GPS, link, waypoint); only the position in the list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<crate::dto::telemetry::VehicleTelemetry>,
    /// Its active notifications' titles, worst first, at most five.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<String>,
    /// Who holds its control lease from away (S80).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<Lease>,
    /// Its cameras, and which of them are live from away now (S84).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cameras: Vec<RemoteCamera>,
    /// When the computer read these, on the relay's clock (milliseconds since
    /// the Unix epoch).
    #[schemars(schema_with = "crate::safe::integer")]
    pub at_ms: u64,
}

/// `edge/publish`, the computer to the relay: the latest of the vehicles
/// asked for. A vehicle not in `vehicles` is unchanged.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EdgePublish {
    /// The computer's own count of publishes, for provenance.
    #[schemars(schema_with = "crate::safe::integer")]
    pub seq: u64,
    pub vehicles: Vec<RemoteVehicle>,
    /// Vehicles the computer no longer has.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
}

/// One point of a vehicle's trail, about once a second.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TrailPoint {
    #[schemars(schema_with = "crate::safe::integer")]
    pub at_ms: u64,
    pub latitude_deg: f64,
    pub longitude_deg: f64,
}

/// `edge/snapshot`, the relay to a viewer: everything it holds now. Deltas
/// follow, numbered from `seq`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EdgeSnapshot {
    #[schemars(schema_with = "crate::safe::integer")]
    pub seq: u64,
    pub computer: Computer,
    pub vehicles: Vec<RemoteVehicle>,
    /// The current flight's trail of each vehicle, oldest first.
    pub trails: Vec<VehicleTrail>,
    /// The relay's clock as it sent this.
    #[schemars(schema_with = "crate::safe::integer")]
    pub relay_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleTrail {
    pub vehicle_id: String,
    pub points: Vec<TrailPoint>,
}

/// `edge/delta`, the relay to a viewer: what changed, one more than the
/// last `seq` it sent this viewer. A gap means asking `edge/watch` again,
/// which answers with a snapshot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EdgeDelta {
    #[schemars(schema_with = "crate::safe::integer")]
    pub seq: u64,
    /// The computer's `EdgePublish::seq` this came from.
    #[schemars(schema_with = "crate::safe::integer")]
    pub source_seq: u64,
    pub vehicles: Vec<RemoteVehicle>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub relay_at_ms: u64,
}

// Remote control (S80, ADR-0003 C9, C10): commands from away go through the
// computer's own command path; the control lease decides who may send
// Mission and Critical ones.

/// `lease/take` and `lease/release`: a vehicle's control lease.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LeaseParams {
    pub vehicle_id: String,
}

/// Who holds a vehicle's control lease from away.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Lease {
    pub vehicle_id: String,
    pub user_id: String,
    pub name: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub since_us: u64,
    /// What happens if they lose connection and nobody is at the computer
    /// (S81): "If you lose connection and nobody is at the ground station,
    /// Copter 1 will return to launch about 15 s later."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failsafe: Option<String>,
}

/// `edge/command`, the relay to the computer: a command from someone away,
/// with the cloud's signed grant naming them. Answered with the receipt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EdgeCommand {
    pub grant: String,
    pub command_id: String,
    pub vehicle_id: String,
    pub command: crate::dto::command::Command,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub confirm: bool,
    /// When the relay received it, on its clock.
    #[schemars(schema_with = "crate::safe::integer")]
    pub sent_at_ms: u64,
}

/// `edge/lease`, the relay to the computer: take or give back a vehicle's
/// control lease. Answered with the lease, or nothing once given back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EdgeLease {
    pub grant: String,
    pub vehicle_id: String,
    pub take: bool,
}

/// `edge/receipt`, the relay to the computer: a command's receipt by the id
/// its sender gave it, after a connection dropped before the answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EdgeReceipt {
    pub grant: String,
    pub command_id: String,
}

// Remote vehicles in the app (S86, ADR-0003 C8): an installed UXDuck
// watches another of the account's computers through the relay as its own
// signed-in account, for the windows that ask. Its token never reaches a
// page, and the page opens no socket but its own UXDuck's (ADR-0008).

/// `remote/watch`: what this window watches from away. One computer, and one
/// of its vehicles in detail; no computer stops.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RemoteWatchParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computer_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    /// One of that computer's cameras to watch live (S84).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_id: Option<String>,
}

/// `remote/changed`: the computer watched, as this UXDuck sees it through the
/// relay. Each vehicle's `atMs` is on this computer's clock, so its age is
/// now minus it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RemoteView {
    pub computer_id: String,
    pub computer_name: String,
    /// Connected to the relay itself.
    pub online: bool,
    /// Why nothing is coming: not signed in, the relay out of reach.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    pub vehicles: Vec<RemoteVehicle>,
    /// The current flight's trail of each vehicle, oldest first.
    pub trails: Vec<VehicleTrail>,
    /// What this window needs to play the camera it asked for (S84).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub video: Vec<RemoteVideo>,
}

// Live video from anywhere (S84, ADR-0003 C11 to C13, "Live path"): a
// computer publishes a camera's live rendition to a cloud media node only
// while someone away watches it, with a publish grant from the media
// coordinator in `apps/api`; a viewer plays it by WHEP with a read grant.
// The media node checks both grants itself against the coordinator's keys,
// so a stream starts without a database (C6).

/// A camera as it leaves the computer: its name, and whether it is live.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RemoteCamera {
    /// The camera's video source id on its computer.
    pub id: String,
    pub name: String,
    /// The media node it is published to now; none while nobody watches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_on: Option<String>,
    /// Why it cannot be sent while someone wants it, for people: no
    /// publisher on the computer, the media nodes out of reach.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// `edge/watchVideo`, a viewer's request: the cameras it plays now. Empty
/// stops them; the relay adds everyone's up into `EdgeDemand::video`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EdgeWatchVideo {
    #[serde(default)]
    pub cameras: Vec<String>,
}

/// A STUN or TURN server for WebRTC, as `RTCIceServer` takes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

/// What a window needs to play one camera from away: where, with what
/// grant, through which servers. The grant reads this one stream and lasts
/// minutes; the account's own token never reaches a page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RemoteVideo {
    pub camera_id: String,
    /// The WHEP endpoint on the media node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub whep_url: Option<String>,
    /// The read grant, sent as `Authorization: Bearer`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ice_servers: Vec<IceServer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub expires_at_ms: Option<u64>,
    /// Why it cannot play, for people: not signed in, not set up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// How a computer sends a live rendition to a media node (C12).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Contribution {
    Whip,
    Srt,
}

/// `POST /v1/media/publish`, a computer asking to publish one camera.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MediaPublishRequest {
    pub device_id: String,
    pub camera_id: String,
    /// The camera's recording in progress on the computer, whose original
    /// replaces the cloud's live recording once stored (C13).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_id: Option<String>,
    /// "Store flights and recordings in my account" (C17): without it the
    /// media node keeps no live recording.
    pub store: bool,
}

/// One media node a computer may publish to, best first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MediaNodeOffer {
    pub id: String,
    pub region: String,
    pub whip_url: String,
    /// The SRT address with the stream id and grant in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub srt_url: Option<String>,
}

/// The answer to `POST /v1/media/publish`: the media nodes in order, and a
/// publish grant every one of them accepts. The computer fails over down
/// the list without asking again (ADR-0003 "Placement").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MediaPublishGrant {
    pub stream_id: String,
    /// Which contribution protocol to prefer; the other where the
    /// computer's publisher lacks it.
    pub protocol: Contribution,
    pub nodes: Vec<MediaNodeOffer>,
    pub grant: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub expires_at_ms: u64,
}

/// `POST /v1/media/watch`, a viewer asking to play one camera.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MediaWatchRequest {
    pub device_id: String,
    pub camera_id: String,
    /// The node the computer says it publishes to, if it said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
}

/// The answer to `POST /v1/media/watch`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MediaWatchGrant {
    pub stream_id: String,
    pub whep_url: String,
    pub grant: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub expires_at_ms: u64,
    pub ice_servers: Vec<IceServer>,
}

/// A watch link (S95, ADR-0023): anyone holding its address watches one
/// computer, one vehicle on it or all of them, until it ends. The token is
/// shown once, when it is made; the API keeps only its hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WatchLink {
    /// `wl` and 16 hex digits.
    pub id: String,
    pub device_id: String,
    /// The vehicle it shows; none for every vehicle on the computer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    /// The vehicle's name when the link was made, for the owner's list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_name: Option<String>,
    /// Who flies, as the owner typed it: shown on the watch page, never an email.
    pub flown_by: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub created_at_ms: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub expires_at_ms: u64,
}

/// `POST /v1/watch-links`: the owner making a link at their computer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WatchLinkCreate {
    pub device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_name: Option<String>,
    pub flown_by: String,
    /// How long it lasts: 24 by default, 168 (a week) at most.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hours: Option<u32>,
}

/// The answer to `POST /v1/watch-links`: the link, and its token, once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WatchLinkCreated {
    pub link: WatchLink,
    pub token: String,
}

/// `GET /v1/watch-links`: the owner's links that have not ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WatchLinkList {
    pub links: Vec<WatchLink>,
}

/// `GET /v1/watch/{token}`: what a guest's page needs before it connects.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WatchInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_name: Option<String>,
    pub flown_by: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub expires_at_ms: u64,
}

/// `POST /v1/watch/{token}/video`: a guest asking to play one camera.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WatchVideoRequest {
    pub camera_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
}

/// `watchLink/create`: a watch link for this computer (S95), or for one
/// vehicle on it. Needs the account signed in and "Let my account watch
/// this computer" on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WatchLinkCreateParams {
    /// The vehicle it shows; every vehicle on this computer when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    /// Who flies, shown on the page: the operator's name, never an email.
    pub flown_by: String,
    /// How long it lasts: 24 by default, 168 at most.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hours: Option<u32>,
}

/// The answer to `watchLink/create`: the link and its address, shown once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WatchLinkCreateResult {
    pub link: WatchLink,
    /// The watch page's address, with the token in it.
    pub url: String,
}

/// One of this computer's watch links, with how many watch through it now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WatchLinkStatus {
    pub link: WatchLink,
    pub watching: u32,
}

/// How many guests watch a vehicle now, through any watch link.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleWatching {
    pub vehicle_id: String,
    pub watching: u32,
}

/// The answer to `watchLink/viewers`, and `watchLink/viewersChanged`: the
/// vehicles someone watches now, for "N watching" beside each (S95, W6).
/// A vehicle nobody watches is left out.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WatchViewersResult {
    pub vehicles: Vec<VehicleWatching>,
}

/// The answer to `watchLink/list`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WatchLinkListResult {
    pub links: Vec<WatchLinkStatus>,
}

/// `watchLink/revoke`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WatchLinkRevokeParams {
    pub link_id: String,
}
