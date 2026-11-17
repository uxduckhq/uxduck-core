//! Simulated vehicles (ADR-0011, S17 and S20): started as a `simulator`
//! link, by the installed daemon from a SITL build, or in a browser from
//! ArduPilot's WebAssembly build.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::dto::link::{
    ArdupilotGimbal, LinkPolicy, PayloadProfile, SimAutopilot, SimFaultKind, SimVehicle,
};

/// Starts a simulated vehicle at a place. The daemon picks the build for
/// the vehicle unless `binary` names one, allocates its instance, and
/// connects to it with a link of its own.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent simulation start options on the wire"
)]
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
    /// Allows an own ArduPilot build without local-only support to listen
    /// on the network. Other computers may connect and control it.
    /// Set only after the operator explicitly agrees to network access.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_network: bool,
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
    /// Carries a simulated camera: component 100 of the vehicle, which
    /// answers MAVLink's camera protocol and which the autopilot speaks for
    /// (ADR-0063). Off unless asked for. UXDuck Desktop and ArduPilot only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub camera: bool,
    /// Carries a simulated gimbal: component 154 of the vehicle, a gimbal
    /// device that the autopilot manages (ADR-0063). Off unless asked for.
    /// UXDuck Desktop and ArduPilot only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gimbal: bool,
    /// Carries a gimbal that ArduPilot simulates itself, with the camera it
    /// carries, in place of UXDuck's simulated camera and gimbal: never
    /// together with `camera` or `gimbal`, since each wants the autopilot's
    /// first mount and first camera (ADR-0063). The autopilot's own driver
    /// speaks for it, and it sends no video. UXDuck Desktop and ArduPilot
    /// only, on a build that has it (`sim/binaries`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ardupilot_gimbal: Option<ArdupilotGimbal>,
    /// What kind of camera and gimbal the simulated payload is, to see what
    /// UXDuck does with hardware that can do less or is put together
    /// another way (ADR-0063). Absent for a camera that takes photos,
    /// records, zooms and focuses, with a gimbal apart from it.
    /// `stream-only` and `photo-video` need `camera`, and `integrated`
    /// needs `camera` and `gimbal` both. It is the simulation's for as
    /// long as it runs. UXDuck Desktop and ArduPilot only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_profile: Option<PayloadProfile>,
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
    /// The gimbals of ArduPilot's own that this build can simulate
    /// (`ardupilotGimbal` of `sim/start`). Empty for PX4 and for a build
    /// that has none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ardupilot_gimbals: Vec<ArdupilotGimbal>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SimBinariesResult {
    pub binaries: Vec<SimBinary>,
}

/// Ask a custom ArduPilot executable for its frames. This runs the file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SimFramesParams {
    pub binary: String,
    pub vehicle: SimVehicle,
    /// Start a new probe. Otherwise read its last result without executing anything.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub refresh: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SimFramesResult {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reading: bool,
    /// Empty when neither the executable nor a companion file supplies a table.
    pub frames: Vec<String>,
    pub default_frame: Option<String>,
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

/// Where a window draws the pictures of this computer's simulated cameras.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SimPicturesResult {
    /// Open a WebSocket on this path of the daemon's origin on this
    /// computer within 30 s. UXDuck says on it which cameras this window
    /// draws and what each sees, and takes the pictures it draws.
    pub path: String,
}

/// How much of the start of a Windows program or library
/// [`authenticode_signed`] reads.
pub const PE_HEAD: usize = 4096;

/// Whether a file is a Windows program or library, by its name.
#[must_use]
pub fn windows_image(name: &str) -> bool {
    std::path::Path::new(name)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("exe") || e.eq_ignore_ascii_case("dll"))
}

/// Whether a Windows program or library carries an Authenticode signature,
/// from its first [`PE_HEAD`] bytes: its header names a certificate table.
/// `None` when they are not the start of one. Smart App Control refuses an
/// unsigned one with a Bad Image box, so the catalogue lists only signed
/// Windows builds and the daemon keeps no other (2026-11-10). Windows checks
/// the signature itself.
#[must_use]
pub fn authenticode_signed(head: &[u8]) -> Option<bool> {
    let u16_at = |at: usize| Some(u16::from_le_bytes(head.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_le_bytes(head.get(at..at + 4)?.try_into().ok()?));
    if head.get(..2)? != b"MZ" {
        return None;
    }
    let pe = usize::try_from(u32_at(0x3C)?)
        .ok()
        .filter(|pe| *pe < head.len())?;
    if head.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    let optional = pe + 24;
    let (count, directories) = match u16_at(optional)? {
        0x10B => (optional + 92, optional + 96),
        0x20B => (optional + 108, optional + 112),
        _ => return None,
    };
    if u32_at(count)? <= 4 {
        return Some(false);
    }
    Some(u32_at(directories + 4 * 8 + 4)? > 0)
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
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent facts about a saved vehicle on the wire"
)]
pub struct SimSnapshot {
    pub id: String,
    pub name: String,
    pub vehicle: SimVehicle,
    #[serde(default, skip_serializing_if = "SimAutopilot::is_ardupilot")]
    pub autopilot: SimAutopilot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<String>,
    /// It carried a simulated camera, and a simulation started from it does
    /// too (ADR-0063).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub camera: bool,
    /// It carried a simulated gimbal, and a simulation started from it does
    /// too (ADR-0063).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gimbal: bool,
    /// It carried a gimbal that ArduPilot simulates itself, and a
    /// simulation started from it does too (ADR-0063).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ardupilot_gimbal: Option<ArdupilotGimbal>,
    /// The kind of camera and gimbal it carried, and a simulation started
    /// from it carries (ADR-0063).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_profile: Option<PayloadProfile>,
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
    /// Allows the supplied ArduPilot build to listen on the network if
    /// it does not support local-only connections.
    /// Set only after the operator explicitly agrees to network access.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_network: bool,
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

/// Switches a fault on or off in a simulated vehicle's camera or gimbal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SimFaultParams {
    /// The simulated vehicle's link, never a real vehicle's connection.
    pub id: String,
    pub kind: SimFaultKind,
    /// The `MAV_CMD` number a `command-refused` or
    /// `command-in-progress` fault is about, one of `commands` of
    /// `sim/faults`. Needed to switch such a fault on. Naming another
    /// while one is on moves the fault to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<u16>,
    /// True switches the fault on, false off.
    pub on: bool,
}

/// The faults a simulated vehicle's camera and gimbal can have.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SimFaultsResult {
    /// Every fault, in the order they are shown.
    pub faults: Vec<SimFaultState>,
    /// The commands a `command-refused` or `command-in-progress` fault can
    /// name in this simulation: the ones its camera and gimbal have.
    pub commands: Vec<SimFaultCommand>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SimFaultState {
    pub kind: SimFaultKind,
    pub on: bool,
    /// The command it is about, while it is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<u16>,
    /// Why this simulation cannot have the fault, when it cannot: a
    /// sentence to show beside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
}

/// A command a fault can name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SimFaultCommand {
    /// Its `MAV_CMD` number.
    pub command: u16,
    /// What it does, in a few words, such as `Take a photo`.
    pub label: String,
    /// Its MAVLink name, such as `MAV_CMD_IMAGE_START_CAPTURE`.
    pub name: String,
}

/// Contact with loaded 3D scenery: pauses a simulation where it touched.
/// The link version prevents a late report from ending a newly restarted
/// flight.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SimCrashParams {
    /// The simulated vehicle's link, never a real vehicle's connection.
    pub id: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The start of a 64-bit Windows program, with a certificate table of
    /// `certificates` bytes.
    fn program(certificates: u32) -> Vec<u8> {
        let mut head = vec![0; 1024];
        head[..2].copy_from_slice(b"MZ");
        head[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        head[0x80..0x84].copy_from_slice(b"PE\0\0");
        head[0x98..0x9A].copy_from_slice(&0x20Bu16.to_le_bytes());
        head[0x104..0x108].copy_from_slice(&16u32.to_le_bytes());
        head[0x128..0x12C].copy_from_slice(&0x4000u32.to_le_bytes());
        head[0x12C..0x130].copy_from_slice(&certificates.to_le_bytes());
        head
    }

    #[test]
    fn a_windows_program_is_signed_when_its_header_names_a_certificate_table() {
        assert_eq!(authenticode_signed(&program(0x2A00)), Some(true));
        assert_eq!(authenticode_signed(&program(0)), Some(false));

        let mut thirty_two_bit = program(0);
        thirty_two_bit[0x98..0x9A].copy_from_slice(&0x10Bu16.to_le_bytes());
        thirty_two_bit[0xF4..0xF8].copy_from_slice(&16u32.to_le_bytes());
        thirty_two_bit[0x11C..0x120].copy_from_slice(&0x2A00u32.to_le_bytes());
        assert_eq!(authenticode_signed(&thirty_two_bit), Some(true));

        let mut few_directories = program(0x2A00);
        few_directories[0x104..0x108].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(authenticode_signed(&few_directories), Some(false));

        assert_eq!(
            authenticode_signed(b"\x7fELF"),
            None,
            "not a Windows program"
        );
        assert_eq!(
            authenticode_signed(&program(0x2A00)[..0x100]),
            None,
            "cut short"
        );
        let mut far = program(0x2A00);
        far[0x3C..0x40].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(authenticode_signed(&far), None);

        assert!(windows_image("cygwin1.dll"));
        assert!(windows_image("ArduPlane.EXE"));
        assert!(!windows_image("defaults/plane.parm"));
    }
}
