//! Pushed state: `state/subscribe` returns a snapshot at `seq`; from then on
//! `state/delta` notifications arrive numbered `seq + 1`, `seq + 2`, ... with
//! no gaps on a connection (ADR-0001 D7).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::dto::client::{Approval, ClientRecord};
use crate::dto::command::Receipt;
use crate::dto::controller::{Controller, InputMapping};
use crate::dto::files::FileTransfer;
use crate::dto::history::HistoryEntry;
use crate::dto::link::Link;
use crate::dto::marker::Marker;
use crate::dto::mission::Mission;
use crate::dto::notification::Notification;
use crate::dto::operator::Operator;
use crate::dto::param_definitions::VehicleDefinitions;
use crate::dto::parameter::{Parameter, ParameterSet};
use crate::dto::plan::Plan;
use crate::dto::plugin::Plugin;
use crate::dto::practice::PracticeAttempt;
use crate::dto::preflight::Preflight;
use crate::dto::search::SavedPlace;
use crate::dto::settings::Settings;
use crate::dto::setup::Setup;
use crate::dto::sim::SimSnapshot;
use crate::dto::thread::{Folder, Item, Thread};
use crate::dto::vehicle::Vehicle;
use crate::dto::video::{VideoPhoto, VideoRecording, VideoSource};

/// The entity types a client can subscribe to.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum EntityType {
    Operator,
    Thread,
    Item,
    Folder,
    Link,
    Vehicle,
    Receipt,
    Notification,
    Client,
    Approval,
    /// A vehicle's parameter set status, keyed by vehicle id.
    ParameterSet,
    /// One parameter, keyed `<vehicle id>/<NAME>`.
    Parameter,
    /// A vehicle's mission, keyed by vehicle id.
    Mission,
    Controller,
    InputMapping,
    /// The daemon's settings; one entity, id `daemon`.
    Settings,
    Plugin,
    /// A saved place.
    Place,
    /// A marker on the map (ADR-0035).
    Marker,
    /// A saved plan (ADR-0029).
    Plan,
    /// A simulation snapshot (S34).
    Snapshot,
    /// Which definition set describes a vehicle's parameters, keyed by
    /// vehicle id (S67).
    ParamDefinitions,
    /// One entry of a vehicle's history (S69).
    HistoryEntry,
    /// A camera stream arriving at UXDuck (S38).
    VideoSource,
    /// A camera's recorded original (S40).
    VideoRecording,
    /// A photo from the video (S144).
    VideoPhoto,
    /// A vehicle's latest practice attempt, keyed by vehicle id (S100).
    PracticeAttempt,
    /// A download from a vehicle or an upload to it (S121).
    FileTransfer,
    /// A vehicle's setup procedure, running or the last, keyed by vehicle
    /// id (S184).
    Setup,
    /// A vehicle's preflight checks, keyed by vehicle id (S192).
    Preflight,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StateSubscribeParams {
    pub entities: Vec<EntityType>,
}

/// The current value of every subscribed entity type. A type that was not
/// subscribed to is absent.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operators: Option<Vec<Operator>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threads: Option<Vec<Thread>>,
    /// Items of every thread, in thread order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<Item>>,
    /// Every folder, including tombstones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folders: Option<Vec<Folder>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub links: Option<Vec<Link>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicles: Option<Vec<Vehicle>>,
    /// The latest receipts of each vehicle (the journal keeps every one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipts: Option<Vec<Receipt>>,
    /// Notifications not yet resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notifications: Option<Vec<Notification>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clients: Option<Vec<ClientRecord>>,
    /// Approvals still pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approvals: Option<Vec<Approval>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_sets: Option<Vec<ParameterSet>>,
    /// Every known parameter of every vehicle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Vec<Parameter>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missions: Option<Vec<Mission>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controllers: Option<Vec<Controller>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_mappings: Option<Vec<InputMapping>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Settings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<Vec<Plugin>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub places: Option<Vec<SavedPlace>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markers: Option<Vec<Marker>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plans: Option<Vec<Plan>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshots: Option<Vec<SimSnapshot>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param_definitions: Option<Vec<VehicleDefinitions>>,
    /// Every vehicle's history, deleted notes included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<HistoryEntry>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_sources: Option<Vec<VideoSource>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_recordings: Option<Vec<VideoRecording>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_photos: Option<Vec<VideoPhoto>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub practice_attempts: Option<Vec<PracticeAttempt>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_transfers: Option<Vec<FileTransfer>>,
    /// Each vehicle's setup procedure, running or the last (S184).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setups: Option<Vec<Setup>>,
    /// Each vehicle's preflight checks (S192).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preflights: Option<Vec<Preflight>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StateSubscribeResult {
    /// The first delta after this snapshot is `seq + 1`.
    #[schemars(schema_with = "crate::safe::integer")]
    pub seq: u64,
    pub snapshot: Snapshot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ChangeOp {
    /// Created or changed; `value` is the whole entity.
    Upsert,
    Remove,
}

/// One entity changed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EntityChange {
    pub entity: EntityType,
    pub id: String,
    pub op: ChangeOp,
    /// On `upsert`: the entity, in the field named after its type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator: Option<Operator>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<Thread>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<Item>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<Folder>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<Link>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle: Option<Vehicle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<Receipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification: Option<Notification>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<ClientRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<Approval>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_set: Option<ParameterSet>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter: Option<Parameter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission: Option<Mission>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller: Option<Controller>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_mapping: Option<InputMapping>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Settings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<Plugin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place: Option<SavedPlace>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker: Option<Marker>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Plan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SimSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param_definitions: Option<VehicleDefinitions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_entry: Option<HistoryEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_source: Option<VideoSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_recording: Option<VideoRecording>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_photo: Option<VideoPhoto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub practice_attempt: Option<PracticeAttempt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_transfer: Option<FileTransfer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<Setup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preflight: Option<Preflight>,
}

/// Immutable identity at the time an action began.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ActorIdentity {
    pub operator_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

/// Who made a change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Actor {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<ActorIdentity>,
}

/// One journal entry as seen by a subscriber. Every `seq` reaches every
/// subscriber, so contiguity holds; `changes` holds only the subscribed
/// types and can be empty.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StateDelta {
    #[schemars(schema_with = "crate::safe::integer")]
    pub seq: u64,
    /// Microseconds since the Unix epoch.
    #[schemars(schema_with = "crate::safe::integer")]
    pub ts_us: u64,
    pub actor: Actor,
    pub changes: Vec<EntityChange>,
}
