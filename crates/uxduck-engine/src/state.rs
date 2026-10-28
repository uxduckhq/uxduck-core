//! The state the engine owns: every entity, at the daemon-wide `seq` of the
//! last event applied (ADR-0001 D7). A checkpoint is this struct serialised;
//! restore is a checkpoint plus the events after it (ADR-0007).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uxduck_core::domain::approval::{Approval, ApprovalState};
use uxduck_core::domain::client::ClientRecord;
use uxduck_core::domain::command::Receipt;
use uxduck_core::domain::controller::{Controller, InputMapping};
use uxduck_core::domain::history::HistoryEntry;
use uxduck_core::domain::link::Link;
use uxduck_core::domain::marker::Marker;
use uxduck_core::domain::mission::Mission;
use uxduck_core::domain::notification::{Notification, NotificationState};
use uxduck_core::domain::operator::Operator;
use uxduck_core::domain::param_definitions::{OwnSet, VehicleDefinitions};
use uxduck_core::domain::parameter::{Parameter, ParameterSet, parameter_id};
use uxduck_core::domain::place::SavedPlace;
use uxduck_core::domain::plan::{Plan, PlanRevisions};
use uxduck_core::domain::plugin::{Plugin, PublishedPlugin};
use uxduck_core::domain::settings::Settings;
use uxduck_core::domain::snapshot::Snapshot;
use uxduck_core::domain::sync::{LibraryLink, LibraryVehicle, SyncState};
use uxduck_core::domain::thread::{Folder, Item, Thread};
use uxduck_core::domain::vehicle::Vehicle;
use uxduck_core::domain::video::{VideoPhoto, VideoRecording, VideoSource};
use uxduck_core::events::{Change, Event};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    /// The `seq` of the last event applied; 0 before the first.
    pub seq: u64,
    pub operators: BTreeMap<String, Operator>,
    #[serde(default)]
    pub threads: BTreeMap<String, Thread>,
    /// By item id; `items_of` gives a thread's items in order.
    #[serde(default)]
    pub items: BTreeMap<String, Item>,
    /// Including tombstones.
    #[serde(default)]
    pub folders: BTreeMap<String, Folder>,
    #[serde(default)]
    pub links: BTreeMap<String, Link>,
    /// Live records and saved vehicles, by VID.
    #[serde(default)]
    pub vehicles: BTreeMap<String, Vehicle>,
    /// The latest receipts of each vehicle; the journal keeps every one.
    #[serde(default)]
    pub receipts: BTreeMap<String, Receipt>,
    /// Notifications not yet resolved; the journal keeps resolved ones.
    #[serde(default)]
    pub notifications: BTreeMap<String, Notification>,
    /// Client identities and their capabilities.
    #[serde(default)]
    pub clients: BTreeMap<String, ClientRecord>,
    /// Approvals still pending; the journal keeps answered ones.
    #[serde(default)]
    pub approvals: BTreeMap<String, Approval>,
    /// Each vehicle's parameter set status, by vehicle id.
    #[serde(default)]
    pub parameter_sets: BTreeMap<String, ParameterSet>,
    /// Every parameter, by `<vehicle id>/<NAME>`, so a vehicle's are a range.
    #[serde(default)]
    pub parameters: BTreeMap<String, Parameter>,
    /// Each vehicle's mission, by vehicle id.
    #[serde(default)]
    pub missions: BTreeMap<String, Mission>,
    #[serde(default)]
    pub controllers: BTreeMap<String, Controller>,
    /// Edited mappings; the built-in ones are not stored until edited.
    #[serde(default)]
    pub input_mappings: BTreeMap<String, InputMapping>,
    #[serde(default)]
    pub settings: Settings,
    /// Installed plugins.
    #[serde(default)]
    pub plugins: BTreeMap<String, Plugin>,
    /// Plugins in the account, by id (S59).
    #[serde(default)]
    pub published: BTreeMap<String, PublishedPlugin>,
    /// Synced plugins removed here: the digest not to install again, by id.
    #[serde(default)]
    pub declined_plugins: BTreeMap<String, String>,
    /// Saved places, by id.
    #[serde(default)]
    pub places: BTreeMap<String, SavedPlace>,
    /// Markers on the map, by id (ADR-0035).
    #[serde(default)]
    pub markers: BTreeMap<String, Marker>,
    /// Saved plans, by id (ADR-0029).
    #[serde(default)]
    pub plans: BTreeMap<String, Plan>,
    /// Every mission each vehicle has held, by record id (S134).
    #[serde(default)]
    pub mission_records: BTreeMap<String, uxduck_core::domain::mission::MissionRecord>,
    /// Each plan's undo and redo, by plan id.
    #[serde(default)]
    pub plan_revisions: BTreeMap<String, PlanRevisions>,
    /// Simulation snapshots, by id (S34).
    #[serde(default)]
    pub snapshots: BTreeMap<String, Snapshot>,
    /// Each vehicle's latest practice attempt, by vehicle id (S100).
    #[serde(default)]
    pub practice: BTreeMap<String, uxduck_core::domain::practice::Attempt>,
    /// Downloads from vehicles and uploads to them, by id (S121).
    #[serde(default)]
    pub file_transfers: BTreeMap<String, uxduck_core::domain::files::FileTransfer>,
    /// Each vehicle's setup procedures, the one running or the last of
    /// each row, by `<vehicle id>/<row>` (S184, S189).
    #[serde(default)]
    pub setups: BTreeMap<String, uxduck_core::domain::setup::Setup>,
    /// Each vehicle's preflight checks as last evaluated, by vehicle id
    /// (S192).
    #[serde(default)]
    pub preflights: BTreeMap<String, uxduck_core::domain::preflight::Preflight>,
    /// Each vehicle's checklist, by vehicle id (S193): kept with the
    /// vehicle, removed with it.
    #[serde(default)]
    pub checklists: BTreeMap<String, uxduck_core::domain::checklist::Checklist>,
    /// The procedure each vehicle changed last, by vehicle id: its key in
    /// `setups` (S189).
    #[serde(default)]
    pub setup_latest: BTreeMap<String, String>,
    /// The best attempt at each exercise, by `<operator id>/<exercise id>`
    /// (an empty operator for attempts made with none).
    #[serde(default)]
    pub practice_best: BTreeMap<String, uxduck_core::domain::practice::Best>,
    /// Which definition set describes each vehicle's parameters, by
    /// vehicle id (S67).
    #[serde(default)]
    pub param_definitions: BTreeMap<String, VehicleDefinitions>,
    /// Vehicles' history, by entry id (S69).
    #[serde(default)]
    pub history: BTreeMap<String, HistoryEntry>,
    /// Camera streams, by source id (S38).
    #[serde(default)]
    pub video_sources: BTreeMap<String, VideoSource>,
    /// Cameras' recordings, by id (S40).
    #[serde(default)]
    pub video_recordings: BTreeMap<String, VideoRecording>,
    /// Photos from the video (S144).
    #[serde(default)]
    pub video_photos: BTreeMap<String, VideoPhoto>,
    /// Definition sets people added, by id (S67).
    #[serde(default)]
    pub own_sets: BTreeMap<String, OwnSet>,
    /// Cloud sync (S16).
    #[serde(default)]
    pub sync: SyncState,
    /// Vehicles other computers published to the account, by id.
    #[serde(default)]
    pub library: BTreeMap<String, LibraryVehicle>,
    /// Connections other computers published to the account, by id (C2).
    #[serde(default)]
    pub link_library: BTreeMap<String, LibraryLink>,
    /// Flights, by id (C8).
    #[serde(default)]
    pub flights: BTreeMap<String, uxduck_core::domain::flight::Flight>,
    /// Vehicles' parameter baselines, by id (C7). Retired on 2026-10-24:
    /// filled only from older journals, and read by nothing.
    #[serde(default)]
    pub baselines: BTreeMap<String, uxduck_core::domain::parameter::ParamBaseline>,
}

/// Receipts kept per vehicle in the state (and so in snapshots).
pub const RECEIPTS_PER_VEHICLE: usize = 20;

/// An event that does not follow the state it is applied to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("expected seq {expected}, found {found}")]
pub struct Gap {
    pub expected: u64,
    pub found: u64,
}

impl State {
    /// Applies the next event. Events are contiguous by construction; a gap
    /// means the log lost something and nothing after it can be trusted.
    ///
    /// # Errors
    /// When `event.seq` is not `self.seq + 1`.
    pub fn apply(&mut self, event: &Event) -> Result<(), Gap> {
        let expected = self.seq + 1;
        if event.seq != expected {
            return Err(Gap {
                expected,
                found: event.seq,
            });
        }
        match &event.change {
            Change::OperatorUpserted { operator } => {
                self.operators.insert(operator.id.clone(), operator.clone());
            }
            Change::OperatorRemoved { id } => {
                self.operators.remove(id);
            }
            Change::ThreadUpserted { thread } => {
                self.threads.insert(thread.id.clone(), thread.clone());
            }
            Change::ItemUpserted { item } => {
                self.items.insert(item.id.clone(), item.clone());
            }
            Change::FolderUpserted { folder } => {
                self.folders.insert(folder.id.clone(), folder.clone());
            }
            Change::LinkUpserted { link } => {
                self.links.insert(link.id.clone(), link.clone());
            }
            Change::LinkRemoved { id } => {
                self.links.remove(id);
            }
            Change::VehicleUpserted { vehicle } => {
                self.vehicles.insert(vehicle.id.clone(), vehicle.clone());
            }
            Change::VehicleRemoved { id } => {
                self.vehicles.remove(id);
                // Its definitions go with it; heard again, they are chosen again.
                self.param_definitions.remove(id);
                self.preflights.remove(id);
                self.checklists.remove(id);
            }
            Change::ReceiptUpserted { receipt } => {
                self.receipts.insert(receipt.id.clone(), receipt.clone());
                self.trim_receipts(&receipt.vehicle_id);
            }
            Change::ClientUpserted { client } => {
                self.clients.insert(client.id.clone(), client.clone());
            }
            Change::ClientRemoved { id } => {
                self.clients.remove(id);
            }
            Change::ApprovalUpserted { approval } => {
                if approval.state == ApprovalState::Pending {
                    self.approvals.insert(approval.id.clone(), approval.clone());
                } else {
                    self.approvals.remove(&approval.id);
                }
            }
            Change::NotificationUpserted { notification } => {
                if notification.state == NotificationState::Resolved {
                    self.notifications.remove(&notification.id);
                } else {
                    self.notifications
                        .insert(notification.id.clone(), notification.clone());
                }
            }
            other => self.apply_devices(other),
        }
        self.seq = event.seq;
        Ok(())
    }
}

impl State {
    /// Parameters, missions, controllers, settings and plugins.
    #[allow(clippy::too_many_lines, reason = "one arm per kind of change")]
    fn apply_devices(&mut self, change: &Change) {
        match change {
            Change::ParameterSetUpserted { set } => {
                self.parameter_sets
                    .insert(set.vehicle_id.clone(), set.clone());
            }
            Change::ParametersReplaced {
                set,
                parameters,
                removed,
            } => {
                for name in removed {
                    self.parameters.remove(&parameter_id(&set.vehicle_id, name));
                }
                for parameter in parameters {
                    self.parameters.insert(parameter.id(), parameter.clone());
                }
                self.parameter_sets
                    .insert(set.vehicle_id.clone(), set.clone());
            }
            Change::ParameterChanged { parameter } => {
                self.parameters.insert(parameter.id(), parameter.clone());
            }
            Change::PluginUpserted { plugin } => {
                self.plugins.insert(plugin.id.clone(), plugin.clone());
            }
            Change::PluginRemoved { id } => {
                self.plugins.remove(id);
            }
            Change::PluginPublished { plugin } => {
                self.published.insert(plugin.id.clone(), plugin.clone());
            }
            Change::PluginDeclined { id, digest } => {
                self.declined_plugins.insert(id.clone(), digest.clone());
            }
            Change::PlaceUpserted { place } => {
                self.places.insert(place.id.clone(), place.clone());
            }
            Change::PlaceRemoved { id } => {
                self.places.remove(id);
            }
            Change::MarkerUpserted { marker } => {
                self.markers.insert(marker.id.clone(), marker.clone());
            }
            Change::MarkerRemoved { id } => {
                self.markers.remove(id);
            }
            Change::PlanSaved { plan, revision } => {
                let current = self
                    .plans
                    .get(&plan.id)
                    .map_or(&plan.body, |p| &p.body)
                    .clone();
                self.plan_revisions
                    .entry(plan.id.clone())
                    .or_default()
                    .apply(&current, revision);
                self.plans.insert(plan.id.clone(), plan.clone());
            }
            Change::PlanRemoved { id } => {
                self.plans.remove(id);
                self.plan_revisions.remove(id);
            }
            Change::MissionRecorded { record } => {
                self.mission_records
                    .insert(record.id.clone(), record.clone());
            }
            Change::SnapshotUpserted { snapshot } => {
                self.snapshots.insert(snapshot.id.clone(), snapshot.clone());
            }
            Change::FileTransferUpserted { transfer } => {
                self.file_transfers
                    .insert(transfer.id.clone(), transfer.clone());
            }
            Change::FileTransferRemoved { id } => {
                self.file_transfers.remove(id);
            }
            Change::PreflightUpdated { preflight } => {
                self.preflights
                    .insert(preflight.vehicle_id.clone(), preflight.clone());
            }
            Change::ChecklistUpdated { checklist } => {
                self.checklists
                    .insert(checklist.vehicle_id.clone(), checklist.clone());
            }
            Change::SetupUpdated { setup } => {
                let id = setup.id();
                self.setup_latest
                    .insert(setup.vehicle_id.clone(), id.clone());
                self.setups.insert(id, setup.clone());
            }
            Change::PracticeAttemptUpserted { attempt } => {
                self.practice
                    .insert(attempt.vehicle_id.clone(), attempt.clone());
                if attempt.state.is_over() {
                    crate::practice::keep_best(self, attempt);
                }
            }
            Change::ParamDefinitionsChosen { definitions } => {
                self.param_definitions
                    .insert(definitions.vehicle_id.clone(), definitions.clone());
            }
            Change::HistoryUpserted { entry } => {
                self.history.insert(entry.id.clone(), entry.clone());
            }
            Change::HistoryCleared { vehicle_id } => {
                self.history.retain(|_, e| e.vehicle_id != *vehicle_id);
            }
            Change::VideoSourceUpserted { source } => {
                self.video_sources.insert(source.id.clone(), source.clone());
            }
            Change::VideoSourceRemoved { id } => {
                self.video_sources.remove(id);
            }
            Change::VideoRecordingUpserted { recording } => {
                self.video_recordings
                    .insert(recording.id.clone(), recording.clone());
            }
            Change::VideoPhotoUpserted { photo } => {
                self.video_photos.insert(photo.id.clone(), photo.clone());
            }
            Change::VideoRecordingRemoved { id } => {
                self.video_recordings.remove(id);
            }
            Change::VideoPhotoRemoved { id } => {
                self.video_photos.remove(id);
            }
            Change::OwnSetUpserted { set } => {
                self.own_sets.insert(set.id.clone(), set.clone());
            }
            Change::OwnSetRemoved { id } => {
                self.own_sets.remove(id);
            }
            Change::SnapshotRemoved { id } => {
                self.snapshots.remove(id);
            }
            Change::SettingsChanged { settings } => {
                self.settings = settings.clone();
            }
            Change::SyncChanged { sync } => {
                self.sync = sync.clone();
            }
            Change::LibraryUpserted { vehicle } => {
                self.library.insert(vehicle.id.clone(), vehicle.clone());
            }
            Change::LinkLibraryUpserted { link } => {
                self.link_library.insert(link.id.clone(), link.clone());
            }
            Change::LinkLibraryRemoved { id } => {
                self.link_library.remove(id);
            }
            Change::FlightUpserted { flight } => {
                self.flights.insert(flight.id.clone(), flight.clone());
            }
            Change::FlightReplaced { old_id, flight } => {
                self.flights.remove(old_id);
                self.flights.insert(flight.id.clone(), flight.clone());
            }
            Change::FlightRemoved { id } => {
                if let Some(flight) = self.flights.get_mut(id)
                    && flight.recorded_here
                {
                    flight.keep = false;
                    flight.left_account = true;
                    flight.version += 1;
                } else {
                    self.flights.remove(id);
                }
            }
            Change::BaselineUpserted { baseline } => {
                self.baselines.insert(baseline.id.clone(), baseline.clone());
            }
            Change::BaselineRemoved { id } => {
                self.baselines.remove(id);
            }
            Change::ControllerUpserted { controller } => {
                self.controllers
                    .insert(controller.id.clone(), controller.clone());
            }
            Change::ControllerRemoved { id } => drop(self.controllers.remove(id)),
            Change::InputMappingUpserted { mapping } => {
                self.input_mappings
                    .insert(mapping.id.clone(), mapping.clone());
            }
            Change::MissionUpserted { mission } => {
                self.missions
                    .insert(mission.vehicle_id.clone(), mission.clone());
            }
            _ => {}
        }
    }

    /// Drops a vehicle's oldest finished receipts past the limit. Part of
    /// applying an event, so replay trims the same way.
    fn trim_receipts(&mut self, vehicle_id: &str) {
        let mut finished: Vec<(u64, String)> = self
            .receipts
            .values()
            .filter(|r| r.vehicle_id == vehicle_id && r.status.is_final())
            .map(|r| (r.created_at_us, r.id.clone()))
            .collect();
        if finished.len() <= RECEIPTS_PER_VEHICLE {
            return;
        }
        finished.sort();
        for (_, id) in &finished[..finished.len() - RECEIPTS_PER_VEHICLE] {
            self.receipts.remove(id);
        }
    }

    /// A thread's items, in order.
    #[must_use]
    pub fn items_of(&self, thread_id: &str) -> Vec<&Item> {
        let mut items: Vec<&Item> = self
            .items
            .values()
            .filter(|item| item.thread_id == thread_id)
            .collect();
        items.sort_by_key(|item| item.index);
        items
    }
}

/// What restore found.
#[derive(Debug, Default)]
pub struct Restored {
    pub state: State,
    /// Events applied on top of the checkpoint.
    pub replayed: u64,
    /// Where replay stopped early because the log has a hole.
    pub gap: Option<Gap>,
}

/// Rebuilds the state from the last checkpoint and the log. Events at or
/// below the checkpoint are skipped; replay stops at the first gap.
pub fn restore(checkpoint: Option<State>, events: impl IntoIterator<Item = Event>) -> Restored {
    let mut restored = Restored {
        state: checkpoint.unwrap_or_default(),
        ..Restored::default()
    };
    for event in events {
        if event.seq <= restored.state.seq {
            continue;
        }
        if let Err(gap) = restored.state.apply(&event) {
            restored.gap = Some(gap);
            break;
        }
        restored.replayed += 1;
    }
    restored
}

#[cfg(test)]
mod tests {
    use uxduck_core::domain::operator::{CoordinateFormat, UnitSystem};
    use uxduck_core::events::Actor;

    use super::*;

    fn upsert(seq: u64, id: &str, name: &str, version: u64) -> Event {
        Event {
            seq,
            ts_us: seq,
            actor: Actor::default(),
            change: Change::OperatorUpserted {
                operator: Operator {
                    id: id.into(),
                    name: name.into(),
                    units: UnitSystem::Metric,
                    quantity_units: None,
                    coordinate_format: CoordinateFormat::Decimal,
                    altitude_frame: uxduck_core::domain::telemetry::AltitudeFrame::default(),
                    layout: uxduck_core::domain::operator::Layout::initial(),
                    place_search: true,
                    basemap: uxduck_core::domain::operator::Basemap::Imagery,
                    shortcuts: std::collections::BTreeMap::new(),
                    skipped_confirmations: Vec::new(),
                    telemetry_layouts: Vec::new(),
                    vehicle_telemetry: std::collections::BTreeMap::new(),
                    parameter_groups: Vec::new(),
                    inspector_pinned: Vec::new(),
                    control_actions: Vec::new(),
                    me_marker_id: None,
                    display: None,
                    account_id: None,
                    controller_mappings: Vec::new(),
                    version,
                },
            },
        }
    }

    #[test]
    fn a_checkpoint_from_before_newer_fields_parses() {
        let old: State = serde_json::from_str(r#"{"seq":7,"operators":{}}"#).unwrap();
        assert_eq!(
            old,
            State {
                seq: 7,
                ..State::default()
            }
        );
    }

    #[test]
    fn replay_over_a_checkpoint_matches_applying_everything() {
        let events = vec![
            upsert(1, "a", "Ana", 1),
            upsert(2, "b", "Ben", 1),
            upsert(3, "a", "Ana K", 2),
            Event {
                seq: 4,
                ts_us: 4,
                actor: Actor::default(),
                change: Change::OperatorRemoved { id: "b".into() },
            },
            upsert(5, "c", "Cy", 1),
        ];
        let full = restore(None, events.clone());
        assert_eq!(full.replayed, 5);
        assert_eq!(full.state.seq, 5);

        let checkpoint = restore(None, events[..3].to_vec()).state;
        let resumed = restore(Some(checkpoint), events.clone());
        assert_eq!(
            resumed.replayed, 2,
            "events at or below the checkpoint are skipped"
        );
        assert_eq!(resumed.state, full.state);
        assert_eq!(
            resumed.state.operators.keys().collect::<Vec<_>>(),
            ["a", "c"]
        );
    }

    #[test]
    fn replay_stops_at_a_gap() {
        let restored = restore(
            None,
            vec![upsert(1, "a", "Ana", 1), upsert(3, "b", "Ben", 1)],
        );
        assert_eq!(restored.state.seq, 1);
        assert_eq!(
            restored.gap,
            Some(Gap {
                expected: 2,
                found: 3
            })
        );
    }
}
