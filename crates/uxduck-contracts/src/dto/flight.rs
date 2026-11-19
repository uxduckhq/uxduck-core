//! Flights on the wire (C8): arm to disarm, as UXDuck saw them.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How a flight ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum FlightEnd {
    /// Still armed.
    Open,
    Disarmed,
    /// UXDuck stopped while it was armed; ended when UXDuck last heard it.
    Interrupted,
    /// Someone ended it while the vehicle could not be heard (`flight/end`).
    ContactLost,
}

/// Someone who acted on the vehicle during a flight, from their first
/// command or claim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightOperator {
    pub operator_id: String,
    pub name: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub from_us: u64,
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "each is a fact about the flight"
)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Flight {
    pub id: String,
    pub vehicle_id: String,
    pub vehicle_name: String,
    pub vehicle_type: String,
    /// A person's name for it (`flight/rename`); absent, clients name it
    /// after its vehicle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub simulated: bool,
    /// UXDuck first heard it already armed, so it began earlier.
    #[serde(default)]
    pub seen_armed: bool,
    #[schemars(schema_with = "crate::safe::integer")]
    pub armed_at_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub ended_at_us: Option<u64>,
    pub end: FlightEnd,
    #[schemars(schema_with = "crate::safe::integer")]
    pub air_us: u64,
    /// Time with its ground speed above 0.5 m/s, for every vehicle.
    /// Omitted for an entry recorded before UXDuck kept it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub moving_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_height_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_speed_mps: Option<f64>,
    #[serde(default)]
    pub modes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_start_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_end_pct: Option<f64>,
    #[serde(default)]
    pub operators: Vec<FlightOperator>,
    /// Saved to the account, so it is in the logbook on every computer (C9).
    #[serde(default)]
    pub keep: bool,
    /// Recorded on this computer, rather than had from the account.
    #[serde(default)]
    pub recorded_here: bool,
    /// Whether this host has telemetry in the replay's time range, when
    /// it can check without reading the log. Omitted when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_available: Option<bool>,
    /// Deleted from the account after it was saved; it stays here only.
    #[serde(default)]
    pub left_account: bool,
    /// While open: when UXDuck last heard the vehicle, if it has gone
    /// offline since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub contact_lost_at_us: Option<u64>,
    /// Its flight log in the account on its own: `asked` while on its way,
    /// `stored` once there, as a recording named after the flight.
    #[serde(default)]
    pub log: FlightLog,
    /// Its camera video goes to the account with it (ADR-0046 B11).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sync_video: bool,
    /// The mission it flew, if it flew one (C8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission: Option<FlightMission>,
    /// The parameters it armed with (C7), when UXDuck had them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<FlightParams>,
    /// Its checklist as it stood when it armed, and every override (S193).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checklist: Option<crate::dto::checklist::ChecklistRecord>,
    /// Its `.uxflight` files on other disks (ADR-0062), written by
    /// `flight/offload` or the file it was opened from. This computer's
    /// alone: never in the account.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub copies: Vec<FlightCopy>,
    /// A `flight/offload` under way, or the last one that failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offload: Option<FlightOffload>,
    /// What its recordings take on this computer (FB-148), as
    /// `flight/list` measures them. Omitted by other methods and by a
    /// UXDuck from before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<FlightSize>,
}

/// What a flight's recordings take on this computer (FB-148), in bytes.
/// Its `.uxflight` files on other disks are not counted, nor are the videos
/// added to it from elsewhere.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightSize {
    /// Its part of its connection's tlogs, from where it began to where it
    /// ended. For one whose end UXDuck did not mark (recorded before, ended
    /// when UXDuck stopped or after contact was lost), up to where the
    /// connection's next entry began, else to the end of its files. Omitted
    /// when where it began is not known, or its log is gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub telemetry_bytes: Option<u64>,
    /// Its telemetry was recorded here, and the file it began in has been
    /// deleted since, so the size leaves it out.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub telemetry_gone: bool,
    /// UXDuck's own video recordings of it.
    #[schemars(schema_with = "crate::safe::integer")]
    pub video_bytes: u64,
    /// Videos added to it from elsewhere, as large as each was when added.
    /// They stay where they were added from, which may be another disk or
    /// gone since, so they are not part of what is on this computer.
    #[schemars(schema_with = "crate::safe::integer")]
    pub added_video_bytes: u64,
}

/// A flight's `.uxflight` file (ADR-0062).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightCopy {
    pub path: String,
    /// The disk it is on, by name: `T7` for `/Volumes/T7/…`, a drive
    /// letter on Windows, else the folder's name.
    pub place: String,
    /// The file is there now: false while its disk is not plugged in.
    /// Omitted when this host cannot tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub present: Option<bool>,
}

/// A `flight/offload` of this flight, while it runs or after it failed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightOffload {
    /// The folder it is written to.
    pub to: String,
    /// The disk, by name.
    pub place: String,
    /// Move (its video leaves this computer) rather than Copy.
    #[serde(rename = "move")]
    pub moving: bool,
    #[schemars(schema_with = "crate::safe::integer")]
    pub done_bytes: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub total_bytes: u64,
    /// Why it failed. Nothing on this computer was deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `flight/offload`: writes each flight as a `.uxflight` file into a
/// folder, such as one on an external disk (ADR-0062).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightOffloadParams {
    pub ids: Vec<String>,
    /// A Log folder's path, a disk from `flights/found`'s `disks` (written
    /// into its `UXDuck Flights` folder), or any folder this computer can
    /// write. One not yet a Log folder becomes one.
    pub folder: String,
    /// Move: once every file is written and read back, the video recordings
    /// only these flights use are deleted from this computer. The telemetry
    /// log always stays. Copy (false) deletes nothing.
    #[serde(rename = "move")]
    pub moving: bool,
    /// Writes nothing and says what it would do.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub check: bool,
}

/// What `flight/offload` does or would do.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightOffloadResult {
    /// The folder the files go in.
    pub folder: String,
    /// Its disk, by name.
    pub place: String,
    /// The folder becomes a Log folder, as it is not one yet.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub adds_folder: bool,
    /// The files, one a flight, by name.
    pub files: Vec<String>,
    /// The videos' bytes to write at most, original and playable copy, of
    /// the files not on the disk already. The telemetry is added to this as
    /// each flight's log is read.
    #[schemars(schema_with = "crate::safe::integer")]
    pub video_bytes: u64,
    /// How many video recordings go in the files.
    pub videos: u32,
    /// The video Move deletes from this computer.
    #[schemars(schema_with = "crate::safe::integer")]
    pub freed_bytes: u64,
    /// Videos that stay on this computer after a Move, because another
    /// flight uses them too or they were added from elsewhere.
    pub kept_videos: u32,
    #[schemars(schema_with = "crate::safe::integer")]
    pub kept_bytes: u64,
    /// Free space on the disk, when it can be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub free_bytes: Option<u64>,
    /// With `check`: why it would be refused, in the numbers measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
    /// What may not fit, and is allowed: the disk's free space or its
    /// format's largest file within what the files may take.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// The disk a folder is on, by the name a person knows it by (ADR-0062):
/// `T7` for `/Volumes/T7` on macOS, `/media/<user>/T7` or
/// `/run/media/<user>/T7` on Linux (or `/media/T7`, `/mnt/T7`), `E:` on
/// Windows, and the folder's own name anywhere else.
#[must_use]
pub fn place_of_folder(folder: &str) -> String {
    let parts: Vec<&str> = folder
        .split(['/', '\\'])
        .filter(|p| !p.is_empty())
        .collect();
    if let Some(drive) = parts.first().filter(|p| p.len() == 2 && p.ends_with(':')) {
        return drive.to_ascii_uppercase();
    }
    let named = match parts.as_slice() {
        ["Volumes" | "mnt", name, ..]
        | ["run", "media", _, name, ..]
        | ["media", _, name, ..]
        | ["media", name] => Some(*name),
        _ => None,
    };
    named
        .or_else(|| parts.last().copied())
        .unwrap_or(folder)
        .to_owned()
}

/// The disk a file is on, as [`place_of_folder`] names it, the file's
/// folder standing for anywhere else.
#[must_use]
pub fn place_of(path: &str) -> String {
    let folder = path
        .rfind(['/', '\\'])
        .map_or(path, |at| if at == 0 { "/" } else { &path[..at] });
    place_of_folder(folder)
}

/// A flight's log sent to the account by itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum FlightLog {
    #[default]
    None,
    Asked,
    Stored,
}

/// `flight/uploadLog`: sends one flight's log to the account while "Store
/// flight logs and videos" is off.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightUploadLogParams {
    pub id: String,
}

/// `flight/log`: one flight's part of its connection's log, as a tlog.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightLogParams {
    pub id: String,
}

/// `flight/remove`: one ended entry, and with `video` the recordings
/// only it reaches too (S348).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightRemoveParams {
    pub id: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub video: bool,
}

/// `flight/removal`: the flights a delete would take (S348).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightRemovalParams {
    pub ids: Vec<String>,
}

/// What deleting those flights frees on this computer (S348): the
/// telemetry recordings only they reach, and those they reach that stay
/// because another flight reaches them or their connection is writing them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightRemovalResult {
    #[schemars(schema_with = "crate::safe::integer")]
    pub freed_bytes: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub kept_bytes: u64,
    /// UXDuck's own video recordings only they reach, which `video` deletes.
    #[serde(default)]
    #[schemars(schema_with = "crate::safe::integer")]
    pub video_bytes: u64,
}

/// A flight's tlog, to save as a file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightLogResult {
    /// A file name for it, ending `.tlog`.
    pub name: String,
    /// The tlog's bytes in base64.
    pub data: String,
}

/// A mission a flight flew (C8): flights of one mission share a run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightMission {
    /// The mission on the vehicle by its content, when UXDuck knew it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_name: Option<String>,
    /// The vehicle said the mission was complete during this flight.
    #[serde(default)]
    pub completed: bool,
    /// The first flight of the run.
    pub run_id: String,
}

/// `flight/end`: ends an open flight whose vehicle cannot be heard, at when
/// it was last heard.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightEndParams {
    pub id: String,
}

/// The parameters a flight armed with (C7).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightParams {
    /// SHA-256 of every parameter, one `NAME=value` line each, by name:
    /// the same digest is the same configuration.
    pub digest: String,
    pub count: u32,
}

/// `flight/keep`: saves a flight to the account, or with `keep: false`
/// deletes it from the account: gone from the computers that had it from
/// there, kept on the one that recorded it. There is no "unsave".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightKeepParams {
    pub id: String,
    pub keep: bool,
    /// Every real flight from now on too ("Save every flight to my
    /// account"), or none when `keep` is false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub always: bool,
    /// What goes with it this once, when more than "What to sync" says
    /// (ADR-0046 B11): its telemetry log, or its video too. A synced
    /// flight takes what it lacks; never less than the setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<crate::dto::settings::SyncContent>,
}

/// `flight/rename`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightRenameParams {
    pub id: String,
    /// Up to 80 characters; empty goes back to its vehicle's name.
    pub name: String,
}

/// `flight/list`: this computer's flights, newest first, a page at a time.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightListParams {
    /// Only this vehicle's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    /// At most this many (default 100, at most 500).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// The page after the one that gave this `older`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    /// Only these flights, every one found, wherever they are in the
    /// logbook: `limit` and `before` do not apply.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<String>,
    /// Also totals since this time, such as the start of the month where
    /// the page is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub totals_since_us: Option<u64>,
    /// Only the entries this operator acted on, and totals over them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_id: Option<String>,
    /// Only the entries on this computer, and totals over them: recorded
    /// here, with none of their video moved to a disk. One copied to a disk
    /// is here too (S379).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub here: bool,
    /// Only the entries with a copy on this disk or folder, by its
    /// `place` (ADR-0062, S379), and totals over them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place: Option<String>,
    /// Only the entries with a copy in this folder or below it, such as a
    /// Log folder, and totals over them (S379).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub under: Option<String>,
    /// Only the entries armed at or after this time, such as the start of
    /// a day, and totals over them (FB-226).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub armed_since_us: Option<u64>,
    /// Only the entries armed before this time, and totals over them
    /// (FB-226).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub armed_before_us: Option<u64>,
    /// Only the entries armed for longer than this, arm to disarm, and
    /// totals over them (FB-226). One still armed counts its time so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub longer_than_us: Option<u64>,
    /// Only the entries armed for less time than this, arm to disarm, and
    /// totals over them (FB-226). One still armed counts its time so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub shorter_than_us: Option<u64>,
    /// Only the simulated entries (true) or only the real ones (false),
    /// and totals over them (FB-148).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simulated: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightListResult {
    pub flights: Vec<Flight>,
    /// Pass as `before` for the next page. None when no older flight is
    /// left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub older: Option<String>,
    /// Time in the air and time armed over every entry, listed or not (C9).
    #[serde(default)]
    pub totals: FlightTotals,
    /// The operators who acted on entries, with how many each, over the
    /// whole logbook whatever `operatorId` asked, by name.
    #[serde(default)]
    pub operators: Vec<FlightListOperator>,
    /// The disks and folders holding entries' copies, with how many each,
    /// over the whole logbook whatever `here`, `place` or `operatorId`
    /// asked, by name (S379).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub places: Vec<FlightListPlace>,
}

/// A disk or folder holding copies of entries (ADR-0062), as
/// [`FlightCopy::place`] names it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightListPlace {
    pub place: String,
    /// Entries with a copy there.
    pub count: u32,
    /// No copy there is there now: its disk is not plugged in. False when
    /// this host cannot tell.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unplugged: bool,
}

/// An operator with entries in the logbook.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightListOperator {
    pub operator_id: String,
    pub name: String,
    pub count: u32,
}

/// Time in the air, time armed and time moving, real and simulated apart
/// (C9, FB-228), all time
/// and since `totalsSinceUs` (zero when not asked).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightTotals {
    #[schemars(schema_with = "crate::safe::integer")]
    pub real_air_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub simulated_air_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub real_air_us_since: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub simulated_air_us_since: u64,
    /// Time armed, in the air or not, over every vehicle: a rover's, boat's
    /// or sub's time out counts here, never in the air.
    #[serde(default)]
    #[schemars(schema_with = "crate::safe::integer")]
    pub real_armed_us: u64,
    #[serde(default)]
    #[schemars(schema_with = "crate::safe::integer")]
    pub simulated_armed_us: u64,
    #[serde(default)]
    #[schemars(schema_with = "crate::safe::integer")]
    pub real_armed_us_since: u64,
    #[serde(default)]
    #[schemars(schema_with = "crate::safe::integer")]
    pub simulated_armed_us_since: u64,
    /// Time moving, ground speed above 0.5 m/s, over every vehicle and
    /// the entries that have it (FB-228).
    #[serde(default)]
    #[schemars(schema_with = "crate::safe::integer")]
    pub real_moving_us: u64,
    #[serde(default)]
    #[schemars(schema_with = "crate::safe::integer")]
    pub simulated_moving_us: u64,
    #[serde(default)]
    #[schemars(schema_with = "crate::safe::integer")]
    pub real_moving_us_since: u64,
    #[serde(default)]
    #[schemars(schema_with = "crate::safe::integer")]
    pub simulated_moving_us_since: u64,
    /// Of `count`, entries with no moving time: recorded before UXDuck
    /// kept it.
    #[serde(default)]
    pub without_moving_time: u32,
    /// The vehicles among the entries.
    #[serde(default)]
    pub vehicles: u32,
    /// Entries in all, of which `flights` holds the newest.
    pub count: u32,
    /// Of `count`, those of vehicles that never fly: rovers, boats and subs.
    #[serde(default)]
    pub grounded: u32,
}

impl From<&uxduck_core::domain::flight::Flight> for Flight {
    fn from(f: &uxduck_core::domain::flight::Flight) -> Self {
        use uxduck_core::domain::flight::FlightEnd as Core;
        Self {
            id: f.id.clone(),
            vehicle_id: f.vehicle_id.clone(),
            vehicle_name: f.vehicle_name.clone(),
            vehicle_type: f.vehicle_type.clone(),
            name: f.name.clone(),
            simulated: f.simulated,
            seen_armed: f.seen_armed,
            armed_at_us: f.armed_at_us,
            ended_at_us: f.ended_at_us,
            end: match f.end {
                Core::Open => FlightEnd::Open,
                Core::Disarmed => FlightEnd::Disarmed,
                Core::Interrupted => FlightEnd::Interrupted,
                Core::ContactLost => FlightEnd::ContactLost,
            },
            air_us: f.air_us,
            moving_us: f.moving_us,
            max_height_m: f.max_height_m,
            max_speed_mps: f.max_speed_mps,
            modes: f.modes.clone(),
            battery_start_pct: f.battery_start_pct,
            battery_end_pct: f.battery_end_pct,
            operators: f
                .operators
                .iter()
                .map(|o| FlightOperator {
                    operator_id: o.operator_id.clone(),
                    name: o.name.clone(),
                    from_us: o.from_us,
                })
                .collect(),
            keep: f.keep,
            recorded_here: f.recorded_here,
            replay_available: None,
            left_account: f.left_account,
            contact_lost_at_us: f.contact_lost_at_us,
            log: match f.log {
                uxduck_core::domain::flight::FlightLog::None => FlightLog::None,
                uxduck_core::domain::flight::FlightLog::Asked => FlightLog::Asked,
                uxduck_core::domain::flight::FlightLog::Stored => FlightLog::Stored,
            },
            sync_video: f.sync_video,
            mission: f.mission.as_ref().map(|m| FlightMission {
                mission_id: m.mission_id.clone(),
                plan_id: m.plan_id.clone(),
                plan_name: m.plan_name.clone(),
                completed: m.completed,
                run_id: m.run_id.clone(),
            }),
            params: f.params.as_ref().map(|p| FlightParams {
                digest: p.digest.clone(),
                count: p.count,
            }),
            checklist: f.checklist.as_ref().map(Into::into),
            copies: f
                .copies
                .iter()
                .map(|c| FlightCopy {
                    path: c.path.clone(),
                    place: place_of(&c.path),
                    present: None,
                })
                .collect(),
            offload: None,
            size: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{place_of, place_of_folder};

    #[test]
    fn a_disk_is_named_as_its_system_names_it() {
        assert_eq!(place_of("/Volumes/T7/UXDuck Flights/a.uxflight"), "T7");
        assert_eq!(place_of("/media/ben/T7/a.uxflight"), "T7");
        assert_eq!(
            place_of("/run/media/ben/Field SSD/logs/a.uxflight"),
            "Field SSD"
        );
        assert_eq!(place_of("/mnt/backup/a.uxflight"), "backup");
        assert_eq!(place_of("e:\\Flights\\a.uxflight"), "E:");
        assert_eq!(place_of("/home/ben/Flights/a.uxflight"), "Flights");
        assert_eq!(place_of_folder("/Volumes/T7"), "T7");
        assert_eq!(place_of_folder("/home/ben/Flights"), "Flights");
    }
}
