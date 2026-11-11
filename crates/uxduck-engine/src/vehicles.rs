//! Links and vehicles on the engine's side (ADR-0004). The data plane runs
//! MAVLink outside the engine and reports observations; the engine keeps the
//! entities, the user's choices and the journal.

use std::collections::BTreeSet;

use uxduck_core::domain::link::{Link, LinkKind, LinkStatus};
use uxduck_core::domain::notification::NotificationState;
use uxduck_core::domain::vehicle::{
    Observation, Presence, Vehicle, VehicleColour, callsign, simulation_name,
};
use uxduck_core::events::{Actor, Change};

use crate::{Engine, NO_CONNECTION};

/// The condition an older UXDuck gave a real vehicle's ground-testing
/// notice, before its id.
const GROUND_TESTING: &str = "ground-testing:";

pub(crate) fn data_plane() -> Actor {
    Actor {
        client_id: "data-plane".into(),
        operator_id: None,
        identity: None,
    }
}

impl Engine {
    /// A link opened, failed or closed.
    pub fn link_status(&mut self, link_id: &str, status: LinkStatus, detail: Option<String>) {
        let Some(mut link) = self.state.links.get(link_id).cloned() else {
            return;
        };
        // A late report from the data plane does not turn a link that was
        // turned off on again.
        if link.status.is_off() {
            return;
        }
        if link.status == status && link.status_detail == detail {
            return;
        }
        // A simulator that stopped or lost its link is no longer paused.
        if status != LinkStatus::Up {
            link.paused = false;
        }
        link.status = status;
        link.status_detail = detail;
        link.version += 1;
        let receiver = !link.carries.is_mavlink();
        self.commit_as(data_plane(), Change::LinkUpserted { link });
        // A receiver's state is in its Airspace row (S172).
        if receiver {
            self.traffic_sources_changed();
        }
    }

    /// Makes sure a link of this kind exists, creating it as given if not:
    /// the links `uxduck forward` is started with. Returns its id.
    ///
    /// # Errors
    /// When the link is refused, as `link/add` would refuse it.
    pub fn ensure_link(
        &mut self,
        params: uxduck_contracts::dto::link::LinkAddParams,
    ) -> Result<String, uxduck_contracts::errors::ApiError> {
        let kind: uxduck_core::domain::link::LinkKind = params.kind.clone().into();
        if let Some(existing) = self.state.links.values().find(|l| l.kind == kind) {
            return Ok(existing.id.clone());
        }
        let actor = Actor {
            client_id: "uxduck-forward".into(),
            operator_id: None,
            identity: None,
        };
        crate::rpc::vehicles::create_link(self, actor, params).map(|link| link.id)
    }

    /// The router saw a forward's frames come back: turn it off until the
    /// user turns it on again (ADR-0004 loop detection).
    pub fn link_disabled_by_loop(&mut self, link_id: &str, received_link: &str, peer: &str) {
        let Some(mut link) = self.state.links.get(link_id).cloned() else {
            return;
        };
        if link.status.is_off() || !link.kind.is_forward() {
            return;
        }
        self.links.close(link_id);
        link.status = LinkStatus::DisabledByLoopDetection;
        let returned_through = self.state.links.get(received_link).map_or_else(
            || "a connection no longer configured".to_owned(),
            |source| format!("\"{}\"", source.name),
        );
        let destination = match &link.kind {
            uxduck_core::domain::link::LinkKind::UdpForward { host, port } => {
                format!("{host}:{port}")
            }
            _ => link.name.clone(),
        };
        link.status_detail = Some(format!(
            "A forwarding loop was detected: messages sent to {destination} returned through {returned_through} ({peer}). Check the other program's forwarding settings so it does not send these messages back to UXDuck, then turn this address on again."
        ));
        link.version += 1;
        let (name, detail) = (
            link.name.clone(),
            link.status_detail.clone().unwrap_or_default(),
        );
        self.commit_as(data_plane(), Change::LinkUpserted { link });
        self.loop_detected(link_id, &name, &detail);
    }

    /// Holds the wake lock while any vehicle is online (ADR-0004), so a
    /// closed lid or an idle timer does not drop a link, and keeps the
    /// display on while a real one is armed, so the screen does not lock in
    /// flight; a simulated one never keeps it on (maintainer, 2026-10-16).
    pub(crate) fn refresh_wake_lock(&mut self) {
        let online = self
            .state
            .vehicles
            .values()
            .any(|v| v.presence == Presence::Online);
        if online != self.awake {
            self.awake = online;
            self.system.wake_lock(online);
        }
        let flying = self
            .state
            .vehicles
            .values()
            .any(|v| v.presence == Presence::Online && v.armed && !self.is_simulated(v));
        if flying != self.display_awake {
            self.display_awake = flying;
            self.system.display_awake(flying);
        }
    }

    /// Merges what the data plane saw. A vehicle seen under a new id (its
    /// UID arrived) replaces the record under its old id, keeping its name
    /// and whether it was saved or dismissed.
    #[allow(clippy::too_many_lines, reason = "a new record is one line per field")]
    pub fn vehicle_observed(&mut self, seen: Observation) {
        if self.closed_before(&seen.link_id) {
            return;
        }
        // A record that already knows this id as an earlier one (after a
        // restart, the session hears the vehicle under its provisional id
        // before its UID) keeps its lasting id.
        let known_as = self.known_as(&seen.id);
        let previous_id = std::iter::once(&seen.id)
            .chain(seen.aliases.iter())
            .find(|id| self.state.vehicles.contains_key(*id))
            .cloned()
            .or_else(|| known_as.clone());
        let previous = previous_id
            .as_ref()
            .and_then(|id| self.state.vehicles.get(id))
            .cloned();
        let mut vehicle = previous.clone().unwrap_or_else(|| self.first_heard(&seen));
        if known_as.is_none() {
            vehicle.id.clone_from(&seen.id);
        }
        // A record kept from before colours were given takes one now.
        vehicle
            .colour
            .get_or_insert_with(|| self.given_colour(&vehicle.id));
        let duplicates = self.fold_duplicates(&seen.aliases, previous_id.as_ref(), &mut vehicle);
        for alias in &seen.aliases {
            if !vehicle.aliases.contains(alias) {
                vehicle.aliases.push(alias.clone());
            }
        }
        self.remove_observed_duplicates(&seen, &vehicle.id);
        vehicle.link_id = seen.link_id;
        // Heard live: no longer known from a log alone (S248).
        vehicle.seen_in_log = false;
        vehicle.peer = seen.peer;
        vehicle.system_id = seen.system_id;
        vehicle.component_ids = seen.component_ids;
        vehicle.autopilot = seen.autopilot;
        vehicle.vehicle_type = seen.vehicle_type;
        vehicle.uid = seen.uid.or(vehicle.uid);
        // The commit belongs to the version it came with.
        if seen.firmware.is_some() {
            vehicle.firmware_commit = seen.firmware_commit;
        }
        vehicle.firmware = seen.firmware.or(vehicle.firmware);
        // Hidden until seen again (S247): only an offline vehicle is
        // hidden, so heard at all, it is shown.
        if seen.presence != Presence::Offline {
            vehicle.hidden = false;
        }
        vehicle.presence = seen.presence;
        vehicle.last_heartbeat_us = seen.last_heartbeat_us;
        vehicle.armed = seen.armed;
        vehicle.mode = seen.mode;

        let unchanged = previous.as_ref().is_some_and(|p| {
            let mut same = vehicle.clone();
            same.version = p.version;
            &same == p
        });
        if unchanged && duplicates.is_empty() {
            return;
        }
        let presence_changed = previous
            .as_ref()
            .is_none_or(|p| p.presence != vehicle.presence);
        let first_heard = previous.is_none();
        let before = previous.clone();
        vehicle.version = previous.map_or(1, |p| p.version + 1);
        if first_heard {
            let (autopilot, kind, link) = (
                vehicle.autopilot.clone(),
                vehicle.vehicle_type.clone(),
                vehicle.link_id.clone(),
            );
            self.track_vehicle(&autopilot, &kind, &link);
        }
        let after = vehicle.clone();
        self.commit_as(data_plane(), Change::VehicleUpserted { vehicle });
        self.vehicle_upserted(before.as_ref(), &after, presence_changed);
    }

    /// The record of a vehicle heard for the first time: named and coloured
    /// as nobody chose.
    fn first_heard(&self, seen: &Observation) -> Vehicle {
        Vehicle {
            id: seen.id.clone(),
            name: self.unchosen_name(&seen.link_id, None),
            aliases: Vec::new(),
            link_id: String::new(),
            peer: String::new(),
            system_id: seen.system_id,
            component_ids: Vec::new(),
            autopilot: String::new(),
            vehicle_type: String::new(),
            uid: None,
            firmware: None,
            firmware_commit: None,
            presence: Presence::Offline,
            last_heartbeat_us: 0,
            armed: false,
            mode: None,
            saved: false,
            dismissed: false,
            hidden: false,
            seen_in_log: false,
            local_only: false,
            labels: Vec::new(),
            bookmarks: Vec::new(),
            size_cm: None,
            gripper: None,
            companions: Vec::new(),
            traffic: false,
            look: None,
            colour: Some(self.given_colour(&seen.id)),
            preflight: uxduck_core::domain::preflight::PreflightSettings::default(),
            definitions: uxduck_core::domain::param_definitions::DefinitionsChoice::default(),
            dialects: uxduck_core::domain::dialect::DialectChoice::default(),
            replay: false,
            replay_of: None,
            replay_at_us: None,
            sim_evidence: false,
            simulated: None,
            version: 0,
        }
    }

    /// The lasting record that already knows `id` as an earlier one, noted
    /// as heard provisionally (`heard_provisionally`) until heard by its
    /// lasting id.
    fn known_as(&mut self, id: &str) -> Option<String> {
        let known_as = self
            .state
            .vehicles
            .values()
            .find(|v| v.id != id && v.aliases.iter().any(|a| a == id))
            .map(|v| v.id.clone());
        match &known_as {
            Some(lasting) => self.heard_provisionally.insert(lasting.clone()),
            None => self.heard_provisionally.remove(id),
        };
        known_as
    }

    fn remove_observed_duplicates(&mut self, seen: &Observation, vehicle_id: &str) {
        // Every other record this observation names is the same vehicle:
        // heard again from a new address (its link moved, or it rebooted
        // with a new port) before its UID arrived, it was recorded under a
        // new provisional id, and that record goes now.
        let duplicates: Vec<String> = seen
            .aliases
            .iter()
            .chain(std::iter::once(&seen.id))
            .filter(|id| **id != vehicle_id && self.state.vehicles.contains_key(*id))
            .cloned()
            .collect();
        for old in duplicates {
            self.telemetry.remove(&old);
            self.link_health.remove(&old);
            self.commit_as(data_plane(), Change::VehicleRemoved { id: old });
        }
    }

    /// Whether a link was removed or turned off: a late report from the
    /// data plane about it brings no vehicle back. A simulation stopped as
    /// its first heartbeat arrived left an online vehicle on no link
    /// (maintainer, 2026-10-10).
    fn closed_before(&self, link_id: &str) -> bool {
        self.removed_links.contains(link_id)
            || self
                .state
                .links
                .get(link_id)
                .is_some_and(|link| link.status.is_off())
    }

    /// What follows a vehicle heard anew or changed: its history, presence
    /// notices, the wake lock, a launch waiting for it and its parameter
    /// definitions.
    fn vehicle_upserted(
        &mut self,
        before: Option<&Vehicle>,
        after: &Vehicle,
        presence_changed: bool,
    ) {
        self.record_vehicle_change(before, after);
        self.observe_flight_usage(&after.id);
        self.hold_when_identified(before, after);
        if before.is_none_or(|b| b.armed != after.armed) {
            // ArduPilot arms itself for a motor test: no flight (S188).
            let testing = self.armed_for_motor_test(&after.id, after.armed);
            // Landed during an airspeed ratio (S190).
            self.airspeed_armed(&after.id, after.armed);
            if after.armed && !testing {
                self.video_vehicle_arming(&after.id);
            }
            self.review_recordings();
            self.review_prearm(&after.id);
            if testing {
            } else if after.armed {
                self.vehicle_armed(after);
                self.flight_armed(&after.id);
                // Its checklist as it stood, in its history and flight
                // (S193, ADR-0038 K14).
                self.checklist_armed(after);
            } else if before.is_some() {
                self.vehicle_disarmed(after);
                self.flight_disarmed(&after.id);
                self.checklist_disarmed(&after.id);
            }
        }
        // A simulated vehicle offline is no alarm: it stops with the
        // computer when it sleeps, and its dot says so (maintainer,
        // 2026-10-03: noise on waking).
        let quiet = after.presence == Presence::Offline && self.is_simulated(after);
        if presence_changed && !after.dismissed && !quiet {
            self.presence_changed(&after.id, &after.name, after.presence);
        }
        if presence_changed {
            self.flight_presence(&after.id);
        }
        self.refresh_wake_lock();
        self.drive_launch_of_vehicle(&after.id);
        self.drive_plan_run_of_vehicle(&after.id);
        self.review_practice(&after.id);
        self.choose_definitions(&after.id);
        // An install waiting for the vehicle's new version (S189).
        self.review_reconnect(&after.id);
        // Its checks: armed, heard, set (S192).
        self.review_preflight(&after.id);
    }

    /// Resolves the "is a real vehicle" notices an older UXDuck raised
    /// while it held real vehicles on the ground, which no longer applies.
    pub(crate) fn retire_ground_testing_notices(&mut self) {
        let conditions: BTreeSet<String> = self
            .state
            .notifications
            .values()
            .filter(|n| n.state != NotificationState::Resolved)
            .filter_map(|n| n.condition.clone())
            .filter(|c| c.starts_with(GROUND_TESTING))
            .collect();
        for condition in conditions {
            self.resolve(&condition);
        }
    }

    /// Other records under the observation's earlier ids: the same vehicle
    /// heard at a second address before its UID said so, such as after its
    /// system id changed. They fold into `vehicle` (their aliases, and
    /// whether one was saved); returns their ids, to remove.
    fn fold_duplicates(
        &self,
        aliases: &[String],
        previous_id: Option<&String>,
        vehicle: &mut Vehicle,
    ) -> Vec<String> {
        let duplicates: Vec<&Vehicle> = aliases
            .iter()
            .filter(|alias| previous_id != Some(*alias) && **alias != vehicle.id)
            .filter_map(|alias| self.state.vehicles.get(alias))
            .collect();
        for duplicate in &duplicates {
            vehicle.saved |= duplicate.saved;
            for alias in &duplicate.aliases {
                if *alias != vehicle.id && !vehicle.aliases.contains(alias) {
                    vehicle.aliases.push(alias.clone());
                }
            }
        }
        duplicates.into_iter().map(|d| d.id.clone()).collect()
    }

    /// A vehicle by id or by an earlier alias.
    #[must_use]
    pub fn vehicle(&self, id: &str) -> Option<&Vehicle> {
        self.state.vehicles.get(id).or_else(|| {
            self.state
                .vehicles
                .values()
                .find(|v| v.aliases.iter().any(|a| a == id))
        })
    }

    /// The colour for a vehicle with none yet: one no other vehicle heard
    /// now has, if there is one (ADR-0029 P19).
    pub(crate) fn given_colour(&self, except: &str) -> VehicleColour {
        VehicleColour::given(
            self.state
                .vehicles
                .values()
                .filter(|v| v.id != except && !v.replay)
                .filter_map(|v| {
                    v.colour
                        .as_ref()
                        .map(|c| (c, v.presence != Presence::Offline))
                }),
        )
    }

    /// The name of a vehicle nobody named, on `link_id`: a simulation's
    /// "Sim n" (instance 0 is Sim 1), so simulations started together are
    /// named the same each time; a real vehicle's callsign, never one
    /// another real vehicle went by. `except` is the vehicle being named,
    /// which gets its own callsign back.
    pub(crate) fn unchosen_name(&self, link_id: &str, except: Option<&str>) -> String {
        let others = || {
            self.state
                .vehicles
                .values()
                .filter(move |v| Some(v.id.as_str()) != except)
                .map(|v| v.name.as_str())
        };
        if let Some(LinkKind::Simulator { instance, .. }) =
            self.state.links.get(link_id).map(|l| &l.kind)
        {
            return simulation_name(instance.map(usize::from), others());
        }
        let retired = self
            .state
            .retired_callsigns
            .iter()
            .filter(|(_, id)| Some(id.as_str()) != except)
            .map(|(word, _)| word.as_str());
        callsign(others().chain(retired))
    }

    /// Nothing is heard until the data plane reports again: every vehicle
    /// starts a daemon run offline, and every link is asked to open but a
    /// simulator's. The last run's simulations are forgotten;
    /// with "Recreate simulations when UXDuck starts" on, each that was
    /// running starts again afresh where it first started. Stopped ones go.
    pub(crate) fn start_data_plane(&mut self) {
        // A record under another's earlier id is that vehicle, left behind
        // by a daemon that did not fold it in: it goes, its saved mark with
        // the record it belongs to.
        let mut folded = Vec::new();
        for duplicate in self.state.vehicles.values() {
            if let Some(owner) = self
                .state
                .vehicles
                .values()
                .find(|v| v.id != duplicate.id && v.aliases.contains(&duplicate.id))
            {
                folded.push((owner.id.clone(), duplicate.id.clone(), duplicate.saved));
            }
        }
        for (owner, duplicate, saved) in folded {
            if saved
                && let Some(mut vehicle) = self.state.vehicles.get(&owner).cloned()
                && !vehicle.saved
            {
                vehicle.saved = true;
                vehicle.version += 1;
                self.commit_as(data_plane(), Change::VehicleUpserted { vehicle });
            }
            self.commit_as(data_plane(), Change::VehicleRemoved { id: duplicate });
        }
        // Nothing is heard yet, and a dismissal lasts one daemon run.
        let stale: Vec<Vehicle> = self
            .state
            .vehicles
            .values()
            .filter(|v| v.presence != Presence::Offline || v.dismissed)
            .cloned()
            .collect();
        for mut vehicle in stale {
            vehicle.presence = Presence::Offline;
            vehicle.dismissed = false;
            vehicle.version += 1;
            self.commit_as(data_plane(), Change::VehicleUpserted { vehicle });
        }
        // Who UXDuck is on MAVLink, before anything is sent.
        self.links.gcs_system_id(self.state.settings.gcs_system_id);
        let links: Vec<Link> = self.state.links.values().cloned().collect();
        let recreate = self.state.settings.recreate_simulations;
        // Remove old sessions before opening anything: their dependent
        // forwards must never briefly reopen or reserve another link's port.
        for link in &links {
            if link.kind.is_simulator() && (!recreate || link.status.is_off()) {
                let _ = crate::sim::forget_previous(self, &link.id);
            }
        }
        for link in &links {
            if !self.state.links.contains_key(&link.id) {
                continue;
            }
            // Each fails only for a link that is gone, and this one is here.
            if link.kind.is_simulator() {
                let _ = crate::sim::restart(self, NO_CONNECTION, &link.id);
                continue;
            }
            // A link the user turned off stays off until they turn it on.
            if link.status == LinkStatus::Disabled {
                continue;
            }
            self.links.open(link);
        }
    }
}
