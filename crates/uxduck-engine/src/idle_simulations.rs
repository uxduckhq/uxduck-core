//! A reminder, never a timeout that stops work. The native tray can show it
//! after thirty minutes without a visible UI or another consumer.

use std::collections::{BTreeMap, BTreeSet};

use uxduck_contracts::dto::initialize::RunsIn;
use uxduck_contracts::dto::state::EntityType;
use uxduck_contracts::dto::tray::SimulationReminder;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::{Capability, ClientKind};
use uxduck_core::domain::command::ReceiptStatus;
use uxduck_core::domain::link::{Link, LinkKind, LinkStatus};
use uxduck_core::domain::video::RecordingState;

use crate::{ConnectionId, Engine};

const IDLE_US: u64 = 30 * 60 * 1_000_000;

#[derive(Default)]
pub(crate) struct IdleSimulations {
    since: BTreeMap<String, u64>,
    /// One notification per period away, including after dismissing it.
    reminded: bool,
    pub reminder: Option<SimulationReminder>,
    targets: BTreeSet<String>,
    /// A non-UI consumer keeps its scope until the connection ends.
    consumers: BTreeMap<ConnectionId, BTreeSet<String>>,
    all_consumers: BTreeSet<ConnectionId>,
    /// Since when no UXDuck window has been visible: vehicles' warnings
    /// raised since go to the system's notifications through the tray.
    pub unseen_since_us: Option<u64>,
}

impl Engine {
    fn has_visible_ui(&self) -> bool {
        self.connections.values().any(|connection| {
            connection.visible
                && connection.admitted.as_ref().is_some_and(|admitted| {
                    matches!(admitted.kind, ClientKind::OperatorUi | ClientKind::Shell)
                })
        })
    }

    /// The daemon's existing housekeeping clock calls this; visibility,
    /// connection and state changes also review it immediately.
    pub fn review_idle_simulations(&mut self) {
        // Retire program scopes even while a UI is visible or this is a
        // headless session, so short-lived CLI connections cannot accumulate.
        self.idle_simulations.consumers.retain(|connection, links| {
            links.retain(|id| self.state.links.contains_key(id));
            self.connections.contains_key(connection)
        });
        self.idle_simulations
            .all_consumers
            .retain(|connection| self.connections.contains_key(connection));
        let previous = self.idle_simulations.reminder.clone();
        let unseen = if self.has_visible_ui() {
            None
        } else {
            Some(
                self.idle_simulations
                    .unseen_since_us
                    .unwrap_or_else(|| self.clock.now_us()),
            )
        };
        let unseen_changed = unseen != self.idle_simulations.unseen_since_us;
        self.idle_simulations.unseen_since_us = unseen;
        if self.has_visible_ui()
            || self.config.capabilities.runs_in != RunsIn::Installed
            || !self.opened.desktop
            || !self.state.settings.show_in_menu_bar
        {
            self.idle_simulations.since.clear();
            self.idle_simulations.reminder = None;
            self.idle_simulations.targets.clear();
            self.idle_simulations.reminded = false;
        } else {
            let eligible = self.idle_simulation_candidates();
            let now = self.clock.now_us();
            self.idle_simulations
                .since
                .retain(|id, _| eligible.contains(id));
            for id in &eligible {
                self.idle_simulations.since.entry(id.clone()).or_insert(now);
            }
            // Withdraw a notice if one of its targets acquires another use.
            // Old native actions still carry an invalidated token.
            if !self.idle_simulations.targets.is_subset(&eligible) {
                self.idle_simulations.reminder = None;
                self.idle_simulations.targets.clear();
            }
            if !self.idle_simulations.reminded && !self.tray_watchers.is_empty() {
                let targets: BTreeSet<String> = self
                    .idle_simulations
                    .since
                    .iter()
                    .filter(|(_, since)| now.saturating_sub(**since) >= IDLE_US)
                    .map(|(id, _)| id.clone())
                    .collect();
                if !targets.is_empty() {
                    let count = targets.len();
                    self.idle_simulations.reminder = Some(SimulationReminder {
                        id: self.random.token(),
                        title: if count == 1 {
                            "A simulation is still running".into()
                        } else {
                            format!("{count} simulations are still running")
                        },
                        message: "No UXDuck window has been visible for 30 minutes. Stop these simulations to save CPU and battery, or keep them running.".into(),
                    });
                    self.idle_simulations.targets = targets;
                    self.idle_simulations.reminded = true;
                }
            }
        }
        if previous != self.idle_simulations.reminder || unseen_changed {
            self.push_tray();
        }
    }

    fn idle_simulation_candidates(&self) -> BTreeSet<String> {
        if !self.turns.is_empty() || !self.idle_simulations.all_consumers.is_empty() {
            return BTreeSet::new();
        }
        self.state
            .links
            .values()
            .filter(|link| {
                link.kind.is_simulator()
                    && matches!(link.status, LinkStatus::Configured | LinkStatus::Up)
                    && !link.keep_alive
                    && !self.simulation_in_use(link)
            })
            .map(|link| link.id.clone())
            .collect()
    }

    fn simulation_in_use(&self, simulation: &Link) -> bool {
        let vehicles: BTreeSet<&str> = self
            .state
            .vehicles
            .values()
            .filter(|vehicle| vehicle.link_id == simulation.id)
            .flat_map(|vehicle| {
                std::iter::once(vehicle.id.as_str())
                    .chain(vehicle.aliases.iter().map(String::as_str))
            })
            .collect();
        let targets_vehicle = |id: &str| vehicles.contains(id);
        self.idle_simulations
            .consumers
            .values()
            .any(|links| links.contains(&simulation.id))
            || self.state.links.values().any(|forward| {
                matches!(forward.kind, LinkKind::UdpForward { .. })
                    && matches!(
                        forward.status,
                        LinkStatus::Configured
                            | LinkStatus::Up
                            | LinkStatus::Down
                            | LinkStatus::PortInUse
                    )
                    && forward.from_link.as_ref().map_or_else(
                        || {
                            forward.vehicle_filter.is_empty()
                                || forward.vehicle_filter.iter().any(|id| targets_vehicle(id))
                        },
                        |source| source == &simulation.id,
                    )
            })
            || self.state.video_recordings.values().any(|recording| {
                let source_vehicle = self
                    .state
                    .video_sources
                    .get(&recording.source_id)
                    .and_then(|source| source.vehicle_id.as_deref());
                recording.state == RecordingState::Recording
                    && ((recording.vehicle_id.is_none() && source_vehicle.is_none())
                        || recording.vehicle_id.as_deref().is_some_and(targets_vehicle)
                        || source_vehicle.is_some_and(targets_vehicle))
            })
            || self.state.controllers.values().any(|controller| {
                controller.claim.is_some()
                    && controller.target.as_deref().is_some_and(targets_vehicle)
            })
            || self.remote.leases.keys().any(|id| targets_vehicle(id))
            || vehicles.iter().any(|id| self.transfers.uses_vehicle(id))
            || self.state.receipts.values().any(|receipt| {
                receipt.status == ReceiptStatus::Sent && targets_vehicle(&receipt.vehicle_id)
            })
            || self.state.vehicles.values().any(|vehicle| {
                vehicle.link_id == simulation.id
                    && vehicle.armed
                    && vehicle.mode.as_deref().is_some_and(|mode| {
                        mode.eq_ignore_ascii_case("AUTO") || mode.eq_ignore_ascii_case("Mission")
                    })
            })
    }

    /// A program actually reading/controlling a vehicle owns that scope
    /// while connected. Merely connecting an MCP client does not count.
    pub(crate) fn note_simulation_consumer(
        &mut self,
        connection: ConnectionId,
        request: &ClientRequest,
    ) {
        let Some(admitted) = self
            .connections
            .get(&connection)
            .and_then(|c| c.admitted.as_ref())
        else {
            return;
        };
        if matches!(admitted.kind, ClientKind::OperatorUi | ClientKind::Shell)
            || self.tray_watchers.contains(&connection)
        {
            return;
        }
        match request {
            ClientRequest::TelemetrySubscribe(_)
            | ClientRequest::VehicleList(_)
            | ClientRequest::SimStart(_) => {
                self.idle_simulations.all_consumers.insert(connection);
            }
            ClientRequest::StateSubscribe(params)
                if params.entities.iter().any(|entity| {
                    matches!(
                        entity,
                        EntityType::Vehicle | EntityType::Mission | EntityType::ParameterSet
                    )
                }) =>
            {
                self.idle_simulations.all_consumers.insert(connection);
            }
            _ => {}
        }
        // Most vehicle methods share these fields, including diagnostics,
        // transfers and commands. Restrict the fast, frequent plugin path.
        if admitted.kind == ClientKind::Plugin
            && !matches!(
                request,
                ClientRequest::CommandSend(_)
                    | ClientRequest::TelemetryGet(_)
                    | ClientRequest::ParamSet(_)
                    | ClientRequest::MissionUpload(_)
            )
        {
            return;
        }
        if let Ok(value) = serde_json::to_value(request) {
            let params = &value["params"];
            if let Some(vehicle) = params["vehicleId"]
                .as_str()
                .or_else(|| params["id"].as_str())
                .and_then(|id| self.vehicle(id))
            {
                let link = vehicle.link_id.clone();
                self.idle_simulations
                    .consumers
                    .entry(connection)
                    .or_default()
                    .insert(link);
            }
        }
    }

    pub(crate) fn answer_simulation_reminder(
        &mut self,
        connection: ConnectionId,
        token: &str,
        stop: bool,
        granted: &[Capability],
    ) -> Result<(), ApiError> {
        self.review_idle_simulations();
        if self
            .idle_simulations
            .reminder
            .as_ref()
            .is_none_or(|reminder| reminder.id != token)
        {
            return Ok(());
        }
        let targets = std::mem::take(&mut self.idle_simulations.targets);
        self.idle_simulations.reminder = None;
        self.push_tray();
        if !stop {
            return Ok(());
        }
        // The token names a snapshot. Never stop new simulations, and
        // recheck every use even when a native notification was left open.
        let eligible = self.idle_simulation_candidates();
        let mut failures = Vec::new();
        for id in targets.intersection(&eligible) {
            if let Err(error) = crate::tray::stop_simulation(self, connection, id, granted) {
                failures.push(error.message);
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(ApiError::new(ErrorKind::Internal, failures.join(". ")))
        }
    }
}
