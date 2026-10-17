//! Links and vehicles on the engine's side (ADR-0004). The data plane runs
//! MAVLink outside the engine and reports observations; the engine keeps the
//! entities, the user's choices and the journal.

use std::collections::BTreeSet;

use uxduck_core::domain::link::{Link, LinkKind, LinkStatus};
use uxduck_core::domain::notification::NotificationState;
use uxduck_core::domain::vehicle::{CALLSIGNS, Observation, Presence, Vehicle, callsign};
use uxduck_core::events::{Actor, Change};

use crate::{Engine, NO_CONNECTION};

/// The condition an older UXDuck gave a real vehicle's ground-testing
/// notice, before its id.
const GROUND_TESTING: &str = "ground-testing:";

fn data_plane() -> Actor {
    Actor {
        client_id: "data-plane".into(),
        operator_id: None,
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
        link.status = status;
        link.status_detail = detail;
        link.version += 1;
        self.commit_as(data_plane(), Change::LinkUpserted { link });
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
    pub fn vehicle_observed(&mut self, seen: Observation) {
        if self.closed_before(&seen.link_id) {
            return;
        }
        // A record that already knows this id as an earlier one (after a
        // restart, the session hears the vehicle under its provisional id
        // before its UID) keeps its lasting id.
        let known_as = self
            .state
            .vehicles
            .values()
            .find(|v| v.id != seen.id && v.aliases.contains(&seen.id))
            .map(|v| v.id.clone());
        let previous_id = std::iter::once(&seen.id)
            .chain(seen.aliases.iter())
            .find(|id| self.state.vehicles.contains_key(*id))
            .cloned()
            .or_else(|| known_as.clone());
        let previous = previous_id
            .as_ref()
            .and_then(|id| self.state.vehicles.get(id))
            .cloned();
        let mut vehicle = previous.clone().unwrap_or_else(|| Vehicle {
            id: seen.id.clone(),
            name: self.callsign_for(&seen.link_id),
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
            local_only: false,
            labels: Vec::new(),
            bookmarks: Vec::new(),
            size_cm: None,
            version: 0,
        });
        if known_as.is_none() {
            vehicle.id.clone_from(&seen.id);
        }
        for alias in &seen.aliases {
            if !vehicle.aliases.contains(alias) {
                vehicle.aliases.push(alias.clone());
            }
        }
        self.remove_observed_duplicates(&seen, &vehicle.id);
        vehicle.link_id = seen.link_id;
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
        vehicle.presence = seen.presence;
        vehicle.last_heartbeat_us = seen.last_heartbeat_us;
        vehicle.armed = seen.armed;
        vehicle.mode = seen.mode;

        let unchanged = previous.as_ref().is_some_and(|p| {
            let mut same = vehicle.clone();
            same.version = p.version;
            &same == p
        });
        if unchanged {
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
        self.hold_when_identified(before, after);
        if before.is_none_or(|b| b.armed != after.armed) {
            self.review_recordings();
            self.review_prearm(&after.id);
            if after.armed {
                self.vehicle_armed(after);
                self.flight_armed(&after.id);
            } else if before.is_some() {
                self.vehicle_disarmed(after);
                self.flight_disarmed(&after.id);
            }
        }
        // A simulated vehicle offline is no alarm: it stops with the
        // computer when it sleeps, and its dot says so (maintainer,
        // 2026-10-03: noise on waking).
        let quiet = after.presence == Presence::Offline && self.is_simulated(after);
        if presence_changed && !after.dismissed && !quiet {
            self.presence_changed(&after.id, &after.name, after.presence);
        }
        self.refresh_wake_lock();
        self.drive_launch_of_vehicle(&after.id);
        self.review_practice(&after.id);
        self.choose_definitions(&after.id);
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

    /// A new vehicle's name: on a simulator, the callsign of its instance
    /// (instance 0 is Alpha), so simulations started together are named
    /// the same each time; otherwise, or when that one is taken, the first
    /// free callsign.
    fn callsign_for(&self, link_id: &str) -> String {
        let instance = match self.state.links.get(link_id).map(|l| &l.kind) {
            Some(LinkKind::Simulator {
                instance: Some(instance),
                ..
            }) => Some(usize::from(*instance)),
            _ => None,
        };
        instance
            .and_then(|i| CALLSIGNS.get(i))
            .filter(|word| !self.state.vehicles.values().any(|v| v.name == **word))
            .map_or_else(|| self.free_callsign(None), |word| (*word).to_owned())
    }

    /// The first callsign no vehicle but `except` goes by.
    pub(crate) fn free_callsign(&self, except: Option<&str>) -> String {
        callsign(
            self.state
                .vehicles
                .values()
                .filter(|v| Some(v.id.as_str()) != except)
                .map(|v| v.name.as_str()),
        )
    }

    /// Nothing is heard until the data plane reports again: every vehicle
    /// starts a daemon run offline, and every link is asked to open but a
    /// simulator's. The last run's simulations are not remembered, unless
    /// "Recreate simulations when UXDuck starts" is on, when each starts
    /// again afresh where it first started (maintainer, 2026-10-11).
    pub(crate) fn start_data_plane(&mut self) {
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
        for link in &links {
            // Each fails only for a link that is gone, and this one is here.
            if link.kind.is_simulator() && !recreate {
                let _ = crate::rpc::vehicles::remove_link(self, NO_CONNECTION, &link.id);
                continue;
            }
            // A link the user turned off stays off until they turn it on.
            if link.status == LinkStatus::Disabled {
                continue;
            }
            if link.kind.is_simulator() {
                let _ = crate::sim::restart(self, NO_CONNECTION, &link.id);
                continue;
            }
            self.links.open(link);
        }
    }
}
