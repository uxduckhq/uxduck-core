//! Simulated vehicles (ADR-0011, S17 and S20): started as a `simulator`
//! link, by the installed daemon from a SITL build, or in a browser from
//! ArduPilot's WebAssembly build.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::dto::link::{LinkPolicy, SimAutopilot, SimVehicle};

/// Starts a simulated vehicle at a place. The daemon picks the build for
/// the vehicle unless `binary` names one, allocates its instance, and
/// connects to it with a link of its own.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SimStartParams {
    pub vehicle: SimVehicle,
    /// ArduPilot if absent. PX4 runs on an installed UXDuck on macOS and
    /// Linux, and flies a multicopter, plane, VTOL or rover.
    #[serde(default, skip_serializing_if = "SimAutopilot::is_ardupilot")]
    pub autopilot: SimAutopilot,
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    /// Ground level at the start, above mean sea level; 0 if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub altitude_m: Option<f64>,
    /// Which way it faces, in degrees from north; 0 if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading_deg: Option<f64>,
    /// The build's frame, such as ArduPilot's `X` or `quadplane-tilt` or
    /// PX4's `quadx`, from `sim/binaries`; the vehicle's default if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<String>,
    /// Times real time, 1 to 10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speedup: Option<u8>,
    /// Starts it flying at this height above the ground, in metres
    /// (10 to 500), holding position, so nobody has to take off: the
    /// simulation start arms it and takes off on its own. Absent to start
    /// on the ground.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_air_m: Option<f64>,
    /// The path of a SITL build of the user's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    /// Forgets the vehicle's stored parameters each time it starts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub wipe: bool,
    /// Sends it its type's example as its mission once it is up (S131),
    /// ready to fly. Off unless asked for: Quick start asks.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub with_mission: bool,
    /// Hears a few simulated aircraft from ArduPilot's ADS-B simulator
    /// (ADR-0034). Off unless asked for: beside real traffic they would
    /// confuse. ArduPilot only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub traffic: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Also sends the vehicle's traffic to another program, such as another
    /// ground station on this computer (S102). Installed daemons only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forward: Option<SimForward>,
}

/// A forward a simulation start adds beside its vehicle: a `udp-forward`
/// link that carries only that vehicle's traffic and is removed with it.
/// Checked as `link/add` checks a forward, before the vehicle starts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SimForward {
    pub host: String,
    pub port: u16,
    /// `read-write` (both ways, the default) lets the program command the
    /// vehicle; `read` sends it the vehicle's traffic only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<LinkPolicy>,
}

/// A SITL build the daemon found and probed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SimBinary {
    pub path: String,
    pub autopilot: SimAutopilot,
    /// Its program: ArduPilot's, such as `arducopter`, or `px4`.
    pub program: String,
    /// The vehicles it flies: `arduplane` flies planes and VTOLs, PX4
    /// all of its airframes' vehicles.
    pub vehicles: Vec<SimVehicle>,
    /// Its frames: ArduPilot's from the table built into it
    /// (`--list-models`) or, for an older build from the catalogue, the
    /// table shipped beside it; PX4's from its SIH airframes.
    pub frames: Vec<String>,
    pub default_frame: String,
    /// Its version, for a build from the catalogue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The catalogue build it came from (`sim/download`), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalogue_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SimBinariesResult {
    pub binaries: Vec<SimBinary>,
}

/// The simulator catalogue's manifest (S18, ADR-0011 M12): prebuilt SITL
/// builds for each platform, signed with the release key (ADR-0008) as
/// `catalogue.json.minisig`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SimCatalogue {
    /// Bumped only for incompatible changes to this format.
    pub schema: u32,
    /// RFC 3339. A daemon refuses one older than the newest it has seen.
    pub published_at: String,
    pub builds: Vec<CatalogueBuild>,
}

pub const CATALOGUE_SCHEMA: u32 = 1;

/// One firmware version's build of one program for one platform.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CatalogueBuild {
    /// Such as `ardupilot-arducopter-4.7.1-aarch64-macos`.
    pub id: String,
    /// `ardupilot` or `px4`.
    pub firmware: String,
    /// Such as `arducopter`.
    pub program: String,
    pub version: String,
    /// `<arch>-<os>` as Rust names them: `x86_64-linux`, `aarch64-macos`,
    /// `x86_64-windows`.
    pub platform: String,
    /// The program first, then what it needs beside it: Cygwin's DLLs on
    /// Windows, and for a build older than its embedded frame table,
    /// `vehicleinfo.json` and the default parameter files.
    pub files: Vec<CatalogueFile>,
    /// Where the source for exactly this build is (GPL).
    pub source_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CatalogueFile {
    /// Its name in the build's folder, such as `arducopter` or
    /// `defaults/copter.parm`.
    pub name: String,
    /// `url`, `size` and `sha256` are of the bytes downloaded, compressed
    /// when `encoding` is set.
    pub url: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub size: u64,
    /// Lowercase hex SHA-256.
    pub sha256: String,
    /// `zstd`: the download is decompressed into `name` as it arrives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
}

/// Asks for the catalogue; `refresh` fetches it again.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SimCatalogueParams {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub refresh: bool,
}

/// The catalogue's builds for this platform and where each stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SimCatalogueResult {
    /// `loading`, `ready`, or `failed` with `detail`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub builds: Vec<CatalogueEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CatalogueEntry {
    pub build: CatalogueBuild,
    /// `available`, `downloading`, `installed`, or `failed` with `detail`.
    pub state: String,
    /// While downloading, bytes so far of `total_bytes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub received_bytes: Option<u64>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub total_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A catalogue build, by id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SimBuildParams {
    pub id: String,
}

/// A simulation snapshot (S34, ADR-0011 M14): a simulated vehicle as it was
/// when saved, to start a new simulation from. Close, not exact: a new
/// vehicle is put back where it was, with its parameters and mission, at its
/// height and in its mode.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SimSnapshot {
    pub id: String,
    pub name: String,
    pub vehicle: SimVehicle,
    #[serde(default, skip_serializing_if = "SimAutopilot::is_ardupilot")]
    pub autopilot: SimAutopilot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<String>,
    /// Such as `ArduCopter 4.7.0`, when the vehicle said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    /// Ground level there, above mean sea level.
    pub ground_altitude_m: f64,
    /// Above the ground; 0 on the ground.
    pub height_m: f64,
    pub heading_deg: f64,
    pub ground_speed_mps: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    pub armed: bool,
    /// Saved armed and off the ground: it starts in the air by default.
    pub flying: bool,
    /// How many parameters it keeps.
    pub parameter_count: u32,
    /// Mission items, home included.
    pub mission_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_item: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_id: Option<String>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub created_at_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

/// Saves a snapshot of a simulated vehicle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SimSnapshotParams {
    pub vehicle_id: String,
    /// Defaults to the vehicle's name and the time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Starts a new simulation from a snapshot, at its place, with its
/// parameters and mission. One saved flying starts in the air at its height
/// unless `onGround`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotStartParams {
    pub id: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub on_ground: bool,
    /// Times real time, 1 to 10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speedup: Option<u8>,
    /// The path of a SITL build of the user's own. Installed daemons only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    /// Also sends the vehicle's traffic to another program (S102).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forward: Option<SimForward>,
}

/// Pauses or resumes a simulated vehicle: `paused` true freezes it where
/// it is, false lets its time run again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SimPauseParams {
    /// The simulated vehicle's link.
    pub id: String,
    pub paused: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotRenameParams {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotListResult {
    pub snapshots: Vec<SimSnapshot>,
}
