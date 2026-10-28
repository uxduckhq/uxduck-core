//! Commands and receipts on the wire (ADR-0005 command path).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::dto::telemetry::AltitudeFrame;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum Command {
    Arm,
    Disarm,
    /// An ArduPilot mode by name, such as `LOITER` or `GUIDED`.
    SetMode {
        mode: String,
    },
    /// Return to launch.
    Rtl,
    Land,
    /// Restarts the autopilot; ArduPilot refuses while armed.
    Reboot,
    /// Goes to a place and holds there, at a height in a frame: GUIDED on
    /// ArduPilot, Hold on PX4 (`DO_REPOSITION` as a `COMMAND_INT`, so the
    /// place keeps its precision). Offline: `warn`. `command/list` offers
    /// it with a position of zeros, for its risk and class; the sender
    /// fills the place in.
    GoTo {
        latitude_deg: f64,
        longitude_deg: f64,
        /// The height, in `altitudeFrame`.
        altitude_m: f64,
        /// What the height is measured from: `above-home` when left out,
        /// `amsl`, or `above-ground` (the autopilot's terrain data).
        /// `ellipsoid` is refused.
        #[serde(default)]
        altitude_frame: AltitudeFrame,
        /// Submarine heading. Omitted commands retain the existing default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        heading: Option<GoToHeading>,
    },
    /// Any `MAV_CMD` by number with up to seven parameters, as plugins send
    /// for protocols the core does not name. Offline: `warn`.
    Long {
        command: u16,
        params: Vec<f32>,
    },
}

/// How a submarine faces while going to a location. Bearings are degrees
/// clockwise from true north.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "mode", rename_all = "kebab-case")]
pub enum GoToHeading {
    FaceDestination,
    KeepCurrent,
    Set { degrees: f64 },
}

/// What happens to a command while the vehicle is not `online`: `allow`
/// sends it, `warn` sends it after `confirm: true`, `block` refuses it
/// unless an operator sets `force`. The UI uses the same table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum OfflineBehaviour {
    Allow,
    Warn,
    Block,
}

/// Sends a command to one vehicle, unicast to its current address. The
/// response comes once the vehicle has answered, or every retry has timed
/// out: the finished receipt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommandSendParams {
    pub vehicle_id: String,
    pub command: Command,
    /// Confirms a `warn` command for a vehicle that is not online.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub confirm: bool,
    /// Sends a `block` command anyway, and forces arm or disarm past the
    /// autopilot's checks. Operators only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
    /// Arms past UXDuck's failing blocking preflight checks (ADR-0038 K4):
    /// the ids of the checks the operator held Arm anyway for, from
    /// `preflight/state`. Refused unless every check holding arming is
    /// named. Operators only, never from away; it does not force past the
    /// vehicle's own checks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub override_preflight: Vec<String>,
    /// From away (S80): the id the sender's app gave it, so a command sent
    /// twice is carried out once and its receipt found again by this id.
    /// Only the relay's connections set it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<String>,
    /// From away: when the relay received it, on the relay's clock, in
    /// milliseconds since the Unix epoch. Older than the command's
    /// `maxAgeMs` when it arrives, and it is refused as `expired`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub sent_at_ms: Option<u64>,
}

/// How a command may be sent from away, through the account's relay
/// (ADR-0003 "Command classes").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CommandClass {
    /// Hold, loiter, return, land.
    Safety,
    /// Modes, going somewhere, missions: the control lease first.
    Mission,
    /// Cameras and gimbals.
    Payload,
    /// Arming, taking off, disarming, rebooting: the lease, and approval at
    /// the computer unless remote control allows them.
    Critical,
}

/// Who sent a command from away.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RemoteBy {
    pub user_id: String,
    /// Their name or email.
    pub name: String,
    pub command_id: String,
}

/// Why a command was refused before it was sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RefusalReason {
    /// Only an older UXDuck sends this: arming a real vehicle needed a
    /// ground-test confirmation. Kept so its receipts still read.
    GroundTestRequired,
    /// Only an older UXDuck sends this: it did not fly real vehicles. Kept
    /// so its receipts still read.
    GroundTestingOnly,
    /// From away, and older than its `maxAgeMs` when it reached the computer.
    Expired,
    /// It would arm the vehicle while a blocking preflight check does not
    /// pass; `preflight` lists them. An operator may arm anyway with
    /// `overridePreflight`.
    Preflight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ReceiptStatus {
    /// On its way; waiting for the acknowledgement.
    Sent,
    Accepted,
    /// The vehicle said no; `detail` has its reason.
    Rejected,
    /// No acknowledgement after every retry.
    Timeout,
    /// Never sent; `detail` says why.
    NotSent,
    /// The daemon restarted before the vehicle answered: it may or may not
    /// have been carried out.
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Receipt {
    pub id: String,
    pub vehicle_id: String,
    pub command: Command,
    pub status: ReceiptStatus,
    /// The acknowledgement's result, such as `Denied`, or why it was not sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Set when the daemon refused it for a reason a client can act on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<RefusalReason>,
    /// The blocking preflight checks that did not pass: why it was refused,
    /// or what it was sent past.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preflight: Vec<crate::dto::preflight::CheckRef>,
    /// Sent although blocking checks did not pass: by whom.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preflight_override: Option<crate::dto::preflight::PreflightOverride>,
    /// Sent while the vehicle was stale or offline.
    pub sent_without_recent_heartbeat: bool,
    pub forced: bool,
    /// Transmissions, counting retries.
    pub attempts: u8,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_id: Option<String>,
    /// Sent from away, through the account's relay (S80).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteBy>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub created_at_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub completed_at_us: Option<u64>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

/// How much an operator's own command can change (ADR-0020 A5): `none` is
/// sent at once, `confirm` asks first unless the operator stopped it asking,
/// `dangerous` always asks and says why.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Risk {
    None,
    Confirm,
    Dangerous,
}

/// A command's risk in one situation, and why when it is dangerous.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommandRisk {
    pub level: Risk,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// How much a mode flies the vehicle itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ModeKind {
    /// It flies, drives or lands itself.
    Automatic,
    /// It holds a height or a position while the pilot steers.
    Assisted,
    /// The pilot flies it.
    Manual,
}

/// One command a vehicle offers, for buttons and actions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommandInfo {
    pub command: Command,
    pub label: String,
    /// What it does, in a few plain words: for a mode, what the vehicle
    /// does in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// A mode from the longer, second part of the list, chosen less often:
    /// for a menu's "More modes" group.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub advanced: bool,
    /// For a mode, how much it flies the vehicle itself: the mode menu's
    /// group. None for other commands and for modes UXDuck does not know.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode_kind: Option<ModeKind>,
    /// A mode that holds or flies the vehicle without GPS, tagged so in the
    /// mode menu.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub without_gps: bool,
    pub offline_behaviour: OfflineBehaviour,
    /// Its risk on the ground.
    pub risk: CommandRisk,
    /// Its risk while flying: armed and at least `flyingAboveHomeM` above home.
    pub risk_in_flight: CommandRisk,
    /// Arm and disarm can be forced past the autopilot's checks; this is
    /// the risk of that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forced_risk: Option<CommandRisk>,
    /// How it may be sent from away (S80).
    pub class: CommandClass,
    /// How old it may be when it reaches the computer from away.
    #[schemars(schema_with = "crate::safe::integer")]
    pub max_age_ms: u64,
}

/// Asks for the modes vehicles can be put in, by family, for a controller's
/// mapping, which is a device's and may drive any vehicle. With a vehicle,
/// the answer says which family is that vehicle's.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModeListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModeListResult {
    pub families: Vec<ModeFamily>,
    /// The `vehicleId`'s family, by its key, when the vehicle is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_family: Option<String>,
}

/// One family's modes: ArduPilot's Copter, Plane, Rover and Sub, or PX4's.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModeFamily {
    /// `copter`, `plane`, `rover`, `sub` or `px4`.
    pub key: String,
    /// For a heading: "Multicopters", "Planes", "Rovers and boats",
    /// "Submarines" or "PX4".
    pub name: String,
    /// Every mode of every airframe in the family, the usual ones first.
    pub modes: Vec<ModeInfo>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModeInfo {
    /// Its name, as `Command::SetMode` takes it.
    pub mode: String,
    /// What the vehicle does in it, in a few plain words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// As `CommandInfo::advanced`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub advanced: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommandListResult {
    pub commands: Vec<CommandInfo>,
    /// How high above home an armed vehicle counts as flying, for
    /// `riskInFlight`.
    pub flying_above_home_m: f64,
}
