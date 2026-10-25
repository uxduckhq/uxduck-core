//! The journal: every change is one event with the next daemon-wide `seq`
//! (ADR-0001 D7, ADR-0007). Replaying events in order over a checkpoint
//! rebuilds the state exactly.

use serde::{Deserialize, Serialize};

use crate::domain::approval::Approval;
use crate::domain::client::ClientRecord;
use crate::domain::command::Receipt;
use crate::domain::controller::{Controller, InputMapping};
use crate::domain::history::HistoryEntry;
use crate::domain::link::Link;
use crate::domain::marker::Marker;
use crate::domain::mission::Mission;
use crate::domain::notification::Notification;
use crate::domain::operator::Operator;
use crate::domain::param_definitions::{OwnSet, VehicleDefinitions};
use crate::domain::parameter::{Parameter, ParameterSet};
use crate::domain::place::SavedPlace;
use crate::domain::plan::{Plan, PlanRevision};
use crate::domain::plugin::{Plugin, PublishedPlugin};
use crate::domain::settings::Settings;
use crate::domain::snapshot::Snapshot;
use crate::domain::sync::{LibraryLink, LibraryVehicle, SyncState};
use crate::domain::thread::{Folder, Item, Thread};
use crate::domain::vehicle::Vehicle;
use crate::domain::video::{VideoPhoto, VideoRecording, VideoSource};

/// Identity captured at action time. A later rename or sign-in cannot
/// reinterpret a guest action as verified activity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActorIdentity {
    pub operator_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

/// Who caused a change: the connection and the operator it was acting as.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Actor {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<ActorIdentity>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Change {
    /// Created or changed; carries the whole entity after the change.
    OperatorUpserted {
        operator: Operator,
    },
    OperatorRemoved {
        id: String,
    },
    ThreadUpserted {
        thread: Thread,
    },
    ItemUpserted {
        item: Item,
    },
    FolderUpserted {
        folder: Folder,
    },
    LinkUpserted {
        link: Link,
    },
    LinkRemoved {
        id: String,
    },
    VehicleUpserted {
        vehicle: Vehicle,
    },
    /// The live record is gone (removed, or replaced under a new id).
    VehicleRemoved {
        id: String,
    },
    /// A command was sent, or it ended.
    ReceiptUpserted {
        receipt: Receipt,
    },
    NotificationUpserted {
        notification: Notification,
    },
    /// A client identity was first seen, or its capabilities were edited.
    ClientUpserted {
        client: ClientRecord,
    },
    /// A client forgotten at the computer: a phone or tablet unpaired (S97).
    ClientRemoved {
        id: String,
    },
    ApprovalUpserted {
        approval: Approval,
    },
    /// A parameter set's status changed (a download started or failed).
    ParameterSetUpserted {
        set: ParameterSet,
    },
    /// A download finished: every value, and the names no longer there.
    ParametersReplaced {
        set: ParameterSet,
        parameters: Vec<Parameter>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        removed: Vec<String>,
    },
    /// One value changed, confirmed by the vehicle.
    ParameterChanged {
        parameter: Parameter,
    },
    MissionUpserted {
        mission: Mission,
    },
    /// Plugged in or out, claimed or released, or its target changed.
    ControllerUpserted {
        controller: Controller,
    },
    /// Forgotten while unplugged; plugging it in again brings it back.
    ControllerRemoved {
        id: String,
    },
    InputMappingUpserted {
        mapping: InputMapping,
    },
    SettingsChanged {
        settings: Settings,
    },
    /// Installed, described, failed.
    PluginUpserted {
        plugin: Plugin,
    },
    PluginRemoved {
        id: String,
    },
    /// A plugin went into the account, or was removed from it (S59): every
    /// other computer of the operator's installs or removes it.
    PluginPublished {
        plugin: PublishedPlugin,
    },
    /// A synced plugin removed here: this version is not installed again.
    PluginDeclined {
        id: String,
        digest: String,
    },
    /// Someone ran a plugin's command (S57): the journal's record of who
    /// asked which plugin to do what, with the parameters as JSON text.
    /// No vehicle for a command from a plugin's own tab (S101).
    PluginCalled {
        plugin_id: String,
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        vehicle_id: Option<String>,
        params: String,
    },
    PlaceUpserted {
        place: SavedPlace,
    },
    PlaceRemoved {
        id: String,
    },
    /// A marker added or changed (ADR-0035).
    MarkerUpserted {
        marker: Marker,
    },
    MarkerRemoved {
        id: String,
    },
    /// A saved plan made or changed (ADR-0029), with how its undo history
    /// moves.
    PlanSaved {
        plan: Plan,
        revision: PlanRevision,
    },
    /// A plan deleted more than 30 days ago, gone for good.
    PlanRemoved {
        id: String,
    },
    /// A flight began, or ended (C8).
    FlightUpserted {
        flight: crate::domain::flight::Flight,
    },
    /// A flight deleted from the account (C9): gone from computers that had
    /// it from the account; on the one that recorded it, kept, no longer saved.
    FlightRemoved {
        id: String,
    },
    /// A flight deleted from the account saved there again (2026-10-24):
    /// under a new id, since the account keeps the old one's removal.
    FlightReplaced {
        old_id: String,
        flight: crate::domain::flight::Flight,
    },
    /// A vehicle's parameter baseline set or changed (C7). Retired on
    /// 2026-10-24 (maintainer): nothing makes these now; kept so journals
    /// that have them still read.
    BaselineUpserted {
        baseline: crate::domain::parameter::ParamBaseline,
    },
    BaselineRemoved {
        id: String,
    },
    /// A mission a vehicle holds, new to it or held again (S134).
    MissionRecorded {
        record: crate::domain::mission::MissionRecord,
    },
    /// A simulation snapshot saved or renamed (S34).
    SnapshotUpserted {
        snapshot: Snapshot,
    },
    SnapshotRemoved {
        id: String,
    },
    /// A note written, edited or deleted, or an event recorded, in a
    /// vehicle's history (ADR-0022).
    HistoryUpserted {
        entry: HistoryEntry,
    },
    /// A simulated vehicle's history, gone with its simulation: a new
    /// simulation is a new vehicle, so nothing would ever show it again.
    HistoryCleared {
        vehicle_id: String,
    },
    /// Which set now describes a vehicle's parameters (ADR-0021 P3).
    ParamDefinitionsChosen {
        definitions: VehicleDefinitions,
    },
    /// A person added a definition set (P5).
    OwnSetUpserted {
        set: OwnSet,
    },
    OwnSetRemoved {
        id: String,
    },
    /// Sync switched, or a batch went to the account or came from it
    /// (S16): every upload is journaled, so what left is on the record.
    SyncChanged {
        sync: SyncState,
    },
    /// This computer connected to its account's relay, or dropped from it
    /// (S78, ADR-0003 C7): journaled, as everything that leaves is.
    EdgeConnection {
        connected: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// A download from a vehicle or an upload to it was asked for, moved
    /// on or ended (S121, ADR-0027 F4). Its progress is journaled about
    /// once a second while it runs.
    FileTransferUpserted {
        transfer: crate::domain::files::FileTransfer,
    },
    /// A transfer was cancelled, or a finished one put away with its copy.
    FileTransferRemoved {
        id: String,
    },
    /// An attempt at a practice exercise began, moved on or ended (S100,
    /// ADR-0024 X7): on the operator, so results stay with them.
    PracticeAttemptUpserted {
        attempt: crate::domain::practice::Attempt,
    },
    /// A watch link was made for this computer (S95, ADR-0023): journaled,
    /// as everything that lets anyone see out is. Never its token.
    WatchLinkCreated {
        link: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        vehicle: Option<String>,
        expires_at_ms: u64,
    },
    /// A watch link was revoked, or all of this computer's were (S95).
    WatchLinkRevoked {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        link: Option<String>,
    },
    /// Chunks of a recording the account now keeps (S82, ADR-0003 C17):
    /// journaled, as everything that leaves is.
    FlightStored {
        recording: String,
        chunks: u32,
        bytes: u64,
    },
    /// Part of a recording was deleted before it reached the account (S83,
    /// ADR-0003 "Eviction"): to free disk space, or by hand. The exact range,
    /// in wall-clock microseconds.
    VideoLost {
        recording: String,
        from_us: u64,
        to_us: u64,
        evicted: bool,
    },
    /// A vehicle from the account's library, as another computer published
    /// it.
    LibraryUpserted {
        vehicle: LibraryVehicle,
    },
    /// A connection from the account's library, as another computer
    /// published it (C2).
    LinkLibraryUpserted {
        link: LibraryLink,
    },
    /// Another computer removed a connection it had published (C2).
    LinkLibraryRemoved {
        id: String,
    },
    /// A camera stream appeared, started or stopped, changed its picture,
    /// or was assigned to a vehicle (ADR-0017 V4). Its counters are written
    /// with those, and at most every half minute while it receives.
    VideoSourceUpserted {
        source: VideoSource,
    },
    /// A source added by hand was removed (S39).
    VideoSourceRemoved {
        id: String,
    },
    /// A recording started, grew, ended or was recovered (S40).
    VideoRecordingUpserted {
        recording: VideoRecording,
    },
    /// A photo from the video was kept (S144).
    VideoPhotoUpserted {
        photo: VideoPhoto,
    },
    /// A recording's file was deleted or moved out of the media folder
    /// outside UXDuck, also while it was being written (S151).
    VideoRecordingRemoved {
        id: String,
    },
    /// A photo's file was deleted or moved out of the media folder (S151).
    VideoPhotoRemoved {
        id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub seq: u64,
    /// Microseconds since the Unix epoch.
    pub ts_us: u64,
    pub actor: Actor,
    pub change: Change,
}
