//! Links and vehicles on the engine's side (ADR-0004). The data plane runs
//! MAVLink outside the engine and reports observations; the engine keeps the
//! entities, the user's choices and the journal.

use std::collections::BTreeSet;

use uxduck_core::domain::link::{Link, LinkKind, LinkStatus};
use uxduck_core::domain::link_failure::LinkFailure;
use uxduck_core::domain::notification::{
    Action, NotificationAction, NotificationKind, NotificationState, Severity, Subject,
};
use uxduck_core::domain::vehicle::{
    Observation, Presence, RouteHold, Vehicle, VehicleColour, VehicleRoute, callsign,
    simulation_name,
};
use uxduck_core::events::{Actor, Change};

use crate::commands::Raise;
use crate::{Engine, NO_CONNECTION};

/// The notice that a power-saving mode is on while a vehicle is connected.
const POWER_SAVING: &str = "power-saving";

/// How long a vehicle link stays down before its row says it keeps failing
/// (FB-267). By then a TCP connection has been tried six times
/// (`TCP_BACKOFF` in uxduck-mavlink waits 1, 2, 5 and then 10 s), so a vehicle
/// that is still starting up is not taken for an address it no longer uses.
pub const KEEPS_FAILING_US: u64 = 30_000_000;

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

/// A route the data plane does not have in this run: kept, so the
/// vehicle's page still lists the connection, and never selected.
fn not_heard(route: &mut VehicleRoute) {
    route.reachable = false;
    route.selected = false;
    route.telemetry = false;
    route.reason = "Not heard in this session".into();
    route.round_trip_us = None;
    route.loss_per_mille = None;
    route.last_receive_age_ms = None;
    route.remembered = true;
}

/// Takes a connection off a vehicle: its routes and its having been added
/// for it. Whether anything changed.
pub(crate) fn drop_link(vehicle: &mut Vehicle, link_id: &str) -> bool {
    let added = vehicle.added_links.len();
    vehicle.added_links.retain(|link| link != link_id);
    let mut changed = vehicle.added_links.len() != added;
    if let Some(connections) = &mut vehicle.connections {
        let routes = connections.routes.len();
        connections.routes.retain(|route| route.link_id != link_id);
        changed |= connections.routes.len() != routes;
    }
    changed
}

impl Engine {
    /// A link opened, failed or closed.
    pub fn link_status(&mut self, link_id: &str, status: LinkStatus, detail: Option<String>) {
        self.link_failed(link_id, status, detail, None);
    }

    /// A link opened, failed or closed, with what the operating system said
    /// when it failed (FB-267).
    pub fn link_failed(
        &mut self,
        link_id: &str,
        status: LinkStatus,
        detail: Option<String>,
        error: Option<LinkFailure>,
    ) {
        let Some(mut link) = self.state.links.get(link_id).cloned() else {
            return;
        };
        // A late report from the data plane does not turn a link that was
        // turned off on again.
        if link.status.is_off() {
            return;
        }
        let down = matches!(status, LinkStatus::Down | LinkStatus::PortInUse);
        // Kept through every failed retry, so the row can tell a link that
        // keeps failing from one that only just went down.
        let down_since_monotonic_us = if down {
            link.down_since_monotonic_us
                .or_else(|| Some(self.clock.monotonic_us()))
        } else {
            None
        };
        let keeps_failing = down && link.keeps_failing;
        if link.status == status
            && link.status_detail == detail
            && link.status_error == error
            && link.down_since_monotonic_us == down_since_monotonic_us
            && link.keeps_failing == keeps_failing
        {
            return;
        }
        link.status_error = error;
        link.down_since_monotonic_us = down_since_monotonic_us;
        link.keeps_failing = keeps_failing;
        // A simulator that stopped or lost its link is no longer paused,
        // and its camera and gimbal have no faults when it is back.
        if status != LinkStatus::Up {
            link.paused = false;
            if !link.faults.is_empty() {
                link.faults.clear();
                self.links.simulation_faults(link_id, &[]);
            }
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

    /// Says which vehicle links keep failing (FB-267): down for
    /// [`KEEPS_FAILING_US`] on the daemon's monotonic clock, so a step of
    /// the wall clock can't bring it early, and never heard a vehicle in this
    /// run. Called once a second.
    pub fn links_keep_failing(&mut self) {
        let now_us = self.clock.monotonic_us();
        let due: Vec<Link> = self
            .state
            .links
            .values()
            .filter(|link| {
                !link.keeps_failing
                    && link.carries.is_mavlink()
                    && !link.kind.is_simulator()
                    && !link.kind.is_forward()
                    && !matches!(link.kind, LinkKind::UsbReceiver {})
                    && !self.heard_links.contains(&link.id)
                    && link
                        .down_since_monotonic_us
                        .is_some_and(|since| now_us.saturating_sub(since) >= KEEPS_FAILING_US)
            })
            .cloned()
            .collect();
        for mut link in due {
            link.keeps_failing = true;
            link.version += 1;
            self.commit_as(data_plane(), Change::LinkUpserted { link });
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

    /// Holds the wake lock while any vehicle is connected, online or late
    /// with its heartbeat (ADR-0004), so a closed lid or an idle timer does
    /// not drop a link and macOS does not slow UXDuck down (FB-254). A late
    /// heartbeat keeps it held: letting go then slows UXDuck just when the
    /// link needs it. Keeps the display on while a real one is armed, so the
    /// screen does not lock in flight; a simulated one never keeps it on
    /// (maintainer, 2026-10-16).
    pub(crate) fn refresh_wake_lock(&mut self) {
        let connected = self
            .state
            .vehicles
            .values()
            .any(|v| v.presence != Presence::Offline);
        if connected != self.awake {
            self.awake = connected;
            self.system.wake_lock(connected);
            self.review_power_saving();
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

    /// Says while a vehicle is connected that the operating system's
    /// power-saving mode is on: it slows the whole computer, which the wake
    /// lock cannot lift (FB-254). Checked when a vehicle connects and once
    /// a second after.
    pub fn review_power_saving(&mut self) {
        use crate::runtime::PowerSaving;
        let saving = if self.awake {
            self.system.power_saving()
        } else {
            None
        };
        if saving == self.power_saving {
            return;
        }
        if self.power_saving.is_some() {
            self.resolve(POWER_SAVING);
        }
        self.power_saving = saving;
        let Some(saving) = saving else {
            return;
        };
        let (title, body) = match saving {
            PowerSaving::LowPowerMode => (
                "Low Power Mode is on",
                "macOS may slow the ground station. Turn it off in System Settings while flying.",
            ),
            PowerSaving::EnergySaver => (
                "Windows is saving power",
                "Energy saver or battery saver may slow the ground station. Turn it off while flying.",
            ),
            PowerSaving::PowerSaver => (
                "Power Saver is on",
                "It may slow the ground station. Choose Balanced in the power settings while flying.",
            ),
        };
        self.raise(Raise {
            severity: Severity::Warning,
            kind: NotificationKind::Transient,
            subject: Subject::System,
            title: title.into(),
            body: Some(body.into()),
            actions: Vec::new(),
            condition: Some(POWER_SAVING.into()),
            report: None,
        });
    }

    /// Merges what the data plane saw. A vehicle seen under a new id (its
    /// UID arrived) replaces the record under its old id, keeping its name
    /// and whether it was saved or dismissed.
    #[allow(clippy::too_many_lines, reason = "a new record is one line per field")]
    pub fn vehicle_observed(&mut self, seen: Observation) {
        if self.closed_before(&seen.link_id) {
            return;
        }
        if seen.presence != Presence::Offline {
            self.heard_links.insert(seen.link_id.clone());
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
        if let Some(mut connections) = seen.connections {
            if let Some(previous) = &vehicle.connections {
                for route in &previous.routes {
                    // A new address on the same connection supersedes its old
                    // endpoint, rather than inventing another redundant link.
                    // A removed connection is gone from every vehicle.
                    if self.state.links.contains_key(&route.link_id)
                        && !connections
                            .routes
                            .iter()
                            .any(|r| r.link_id == route.link_id)
                    {
                        let mut old = route.clone();
                        not_heard(&mut old);
                        connections.routes.push(old);
                    }
                }
            }
            connections
                .routes
                .sort_by_key(|r| (r.priority, r.link_id.clone(), r.peer.clone()));
            vehicle.connections = Some(connections);
        }
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
            self.track_vehicle(&vehicle);
        }
        let after = vehicle.clone();
        self.commit_as(data_plane(), Change::VehicleUpserted { vehicle });
        self.vehicle_upserted(before.as_ref(), &after, presence_changed);
        self.flight_heard_on(&after);
        let bindings: Vec<_> = self
            .state
            .controllers
            .values()
            .filter_map(|controller| {
                let binding = controller.last_target.as_ref()?;
                let id = binding.vehicle_id.as_ref()?;
                if id == &after.id || !after.aliases.contains(id) {
                    return None;
                }
                let mut controller = controller.clone();
                if let Some(binding) = controller.last_target.as_mut() {
                    binding.vehicle_id = Some(after.id.clone());
                }
                controller.version += 1;
                Some(controller)
            })
            .collect();
        for controller in bindings {
            self.commit_as(data_plane(), Change::ControllerUpserted { controller });
        }
    }

    /// Armed, UXDuck never goes back up the connection order by itself
    /// (S261), so while one higher in the order answers steadily again it
    /// asks once, with Switch now. Disarmed, it goes back by itself, and the
    /// notice is answered.
    fn connection_return_notice(&mut self, vehicle: &Vehicle) {
        let prefix = format!("connection-return:{}:", vehicle.id);
        let returns = vehicle
            .connections
            .as_ref()
            .filter(|c| {
                c.holds.contains(&RouteHold::Armed) && vehicle.presence != Presence::Offline
            })
            .and_then(|c| c.returns_to.clone());
        let wanted = returns.as_ref().map(|link| format!("{prefix}{link}"));
        let raised: Vec<String> = self
            .state
            .notifications
            .values()
            .filter_map(|n| n.condition.clone())
            .filter(|c| c.starts_with(&prefix) && Some(c) != wanted.as_ref())
            .collect();
        for condition in raised {
            self.resolve(&condition);
        }
        let (Some(link_id), Some(condition)) = (returns, wanted) else {
            return;
        };
        let name = |id: &str| {
            self.state
                .links
                .get(id)
                .map_or_else(|| id.to_owned(), |link| link.name.clone())
        };
        let link = name(&link_id);
        let current = vehicle
            .connections
            .iter()
            .flat_map(|c| &c.routes)
            .find(|route| route.selected)
            .map_or_else(|| "its connection".to_owned(), |route| name(&route.link_id));
        self.raise(Raise {
            severity: Severity::Info,
            kind: NotificationKind::Persistent,
            subject: Subject::Vehicle {
                id: vehicle.id.clone(),
            },
            title: format!("{link} is answering again"),
            body: Some(format!(
                "Switch {} to {link}? UXDuck stays on {current} while {} is armed.",
                vehicle.name, vehicle.name
            )),
            actions: vec![NotificationAction {
                label: "Switch now".into(),
                action: Action::UseConnection {
                    vehicle_id: vehicle.id.clone(),
                    link_id,
                },
            }],
            condition: Some(condition),
            report: None,
        });
    }

    /// The record of a vehicle heard for the first time: named and coloured
    /// as nobody chose.
    fn first_heard(&self, seen: &Observation) -> Vehicle {
        Vehicle {
            connections: None,
            added_links: Vec::new(),
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
            own_model: None,
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
            self.placed_positions.remove(&old);
            self.link_health.remove(&old);
            self.computer_clocks
                .retain(|(vehicle_id, _), _| *vehicle_id != old);
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
        self.video_vehicle_changed(before, after);
        self.connection_return_notice(after);
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
            for link in &duplicate.added_links {
                if !vehicle.added_links.contains(link) {
                    vehicle.added_links.push(link.clone());
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
        // Nothing is heard yet, and a dismissal lasts one daemon run. Every
        // route is kept from before, and one on a connection that is gone,
        // such as one removed by an older UXDuck, goes with it.
        let vehicles: Vec<Vehicle> = self.state.vehicles.values().cloned().collect();
        for before in vehicles {
            let mut vehicle = before.clone();
            vehicle.presence = Presence::Offline;
            vehicle.dismissed = false;
            let links = &self.state.links;
            vehicle.added_links.retain(|link| links.contains_key(link));
            if let Some(connections) = &mut vehicle.connections {
                connections.command_available = false;
                connections.reason = "Waiting for vehicle after daemon restart".into();
                connections
                    .routes
                    .retain(|route| links.contains_key(&route.link_id));
                connections.routes.iter_mut().for_each(not_heard);
            }
            if vehicle == before {
                continue;
            }
            vehicle.version += 1;
            self.commit_as(data_plane(), Change::VehicleUpserted { vehicle });
        }
        self.cameras_unheard();
        self.gimbals_unheard();
        // Who UXDuck is on MAVLink, before anything is sent.
        self.links.gcs_system_id(self.state.settings.gcs_system_id);
        self.links
            .connection_order(&self.state.settings.connection_order);
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
