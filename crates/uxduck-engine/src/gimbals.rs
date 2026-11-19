//! A vehicle's gimbals (ADR-0063 Y6, S406): what the MAVLink session
//! learns of each gimbal its manager answers for, kept so every window,
//! the agent and the CLI see the same ones, and who has each. UXDuck takes
//! a gimbal before it moves it and gives it back a while after, and a
//! person is asked first when another has it: the autopilot, the mission
//! being flown or another ground station.
//!
//! A controller's or the keyboard's gimbal inputs turn it by the same
//! rules (S407): it is taken once when an input first asks, kept while one
//! asks, and given back a while after the last lets go. An input cannot
//! be asked a question, so one that asks while another has the gimbal
//! takes nothing and sends nothing, and a notice says why.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use uxduck_contracts::dto::Empty;
use uxduck_contracts::dto::command::{Command as CommandDto, CommandSendParams};
use uxduck_contracts::dto::gimbal::{
    Gimbal as GimbalDto, GimbalCommandParams, GimbalListResult, GimbalWatchParams,
};
use uxduck_contracts::dto::state::{ChangeOp, EntityChange, EntityType};
use uxduck_contracts::dto::terrain::TerrainAtParams;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::command::{Command, Receipt, ReceiptStatus};
use uxduck_core::domain::gimbal::{Gimbal, GimbalAction, GimbalController, GimbalHolder};
use uxduck_core::domain::notification::{NotificationKind, Severity, Subject};
use uxduck_core::domain::vehicle::{Presence, Vehicle};
use uxduck_core::events::Change;
use uxduck_mavlink::gimbal as seen;
use uxduck_terrain::lookup::Ground;

use crate::commands::Raise;
use crate::plugins::daemon;
use crate::rpc::handlers::to_value;
use crate::runtime::{CommandOrder, GimbalRoute};
use crate::vehicles::data_plane;
use crate::{ConnectionId, Engine, upsert};

/// How long after the last movement UXDuck gives back a gimbal it took by
/// itself: long enough that a person turning it bit by bit keeps it, and
/// short enough that the next to ask is not kept waiting.
pub const GIVEN_BACK_AFTER_US: u64 = 15_000_000;

/// Mission steps that point a gimbal: looking at a place
/// (`DO_SET_ROI_LOCATION`, `DO_SET_ROI_WPNEXT_OFFSET`, `DO_SET_ROI`),
/// `DO_MOUNT_CONTROL` and `DO_GIMBAL_MANAGER_PITCHYAW`.
const POINTING_STEPS: [u16; 5] = [195, 196, 201, 205, 1000];
/// `DO_SET_ROI_NONE`: the mission lets the gimbal go.
const DO_SET_ROI_NONE: u16 = 197;

/// What the engine keeps of gimbals while it runs. None of it is stored.
#[derive(Debug, Default)]
pub(crate) struct Gimbals {
    /// Who each gimbal's manager says has it, by gimbal id. What people
    /// are shown is this, or the mission while it points the gimbal.
    said: BTreeMap<String, GimbalHolder>,
    /// The mission step a vehicle's gimbals were taken from, by vehicle
    /// id: the mission has them again at its next pointing step.
    taken: BTreeMap<String, u16>,
    /// The gimbals UXDuck took by itself before a movement, by gimbal id,
    /// and when the last movement was accepted or giving it back was last
    /// tried. One stays until it is given back, gone or another's.
    ours: BTreeMap<String, u64>,
    /// The receipts of the takes UXDuck sent by itself.
    takes: BTreeSet<String>,
    /// The vehicles whose gimbals each connection is looking at.
    watchers: BTreeMap<ConnectionId, BTreeSet<String>>,
    /// The controllers whose gimbal inputs ask for a turn now (S407), by
    /// controller id, each with the gimbal it asks for.
    sticks: BTreeMap<String, Stick>,
}

/// A controller whose gimbal inputs ask for a turn (S407).
#[derive(Debug)]
struct Stick {
    gimbal_id: String,
    gimbal: GimbalRoute,
    /// The receipt of the take sent for it and not answered yet. None
    /// once it turns the gimbal.
    take: Option<String>,
}

/// The condition of the notice that a controller's gimbal inputs sent
/// nothing because another has the gimbal.
fn stick_held_back(gimbal_id: &str) -> String {
    format!("gimbal-stick:{gimbal_id}")
}

impl Gimbals {
    fn watched(&self, vehicle_id: &str) -> bool {
        self.watchers
            .values()
            .any(|vehicles| vehicles.contains(vehicle_id))
    }
}

fn controller((system_id, component_id): (u8, u8)) -> GimbalController {
    GimbalController {
        system_id,
        component_id,
    }
}

impl Engine {
    /// What the data plane now knows of a gimbal: found, held by another,
    /// turned to hold north, gone or back. A gimbal of a vehicle that is
    /// not known, such as one on a link just removed, is nobody's.
    pub fn gimbal_observed(&mut self, observed: &seen::Observation) {
        let Some(vehicle_id) = self.vehicle(&observed.vehicle_id).map(|v| v.id.clone()) else {
            return;
        };
        let id = Gimbal::id_of(&vehicle_id, observed.component_id, observed.device_id);
        self.gimbals.said.insert(id.clone(), observed.holder);
        let before = self.state.gimbals.get(&id);
        let mut gimbal = Gimbal {
            id,
            vehicle_id,
            component_id: observed.component_id,
            device_id: observed.device_id,
            device_component_id: observed.device_component_id,
            device_silent_since_us: observed.device_silent_since_us,
            present: observed.present,
            capabilities: observed.cap_flags,
            limits: observed.limits,
            primary: observed.primary.map(controller),
            secondary: observed.secondary.map(controller),
            holder: observed.holder,
            yaw_locked: observed.yaw_locked,
            retracted: observed.retracted,
            neutral: observed.neutral,
            failure_flags: observed.failure_flags,
            version: before.map_or(0, |b| b.version),
        };
        gimbal.holder = self.gimbal_holder(&gimbal);
        if before == Some(&gimbal) {
            return;
        }
        gimbal.version += 1;
        self.commit_as(data_plane(), Change::GimbalUpserted { gimbal });
    }

    /// A gimbal that was listed is none: its manager no longer answers
    /// for it.
    pub fn gimbal_removed(&mut self, vehicle_id: &str, component_id: u8, device_id: u8) {
        let vehicle_id = self
            .vehicle(vehicle_id)
            .map_or(vehicle_id, |v| v.id.as_str());
        let id = Gimbal::id_of(vehicle_id, component_id, device_id);
        self.gimbals.said.remove(&id);
        self.gimbals.ours.remove(&id);
        if self.state.gimbals.contains_key(&id) {
            self.commit_as(data_plane(), Change::GimbalRemoved { id });
        }
    }

    /// Nothing is heard until the data plane reports again: every gimbal
    /// starts a daemon run gone, with nothing said yet of who has it. A
    /// device's silence is what one run measured, so the next starts
    /// without it.
    pub(crate) fn gimbals_unheard(&mut self) {
        let heard: Vec<Gimbal> = self
            .state
            .gimbals
            .values()
            .filter(|gimbal| {
                gimbal.present
                    || gimbal.device_silent_since_us.is_some()
                    || gimbal.holder != GimbalHolder::Unknown
                    || gimbal.primary.is_some()
                    || gimbal.secondary.is_some()
            })
            .cloned()
            .collect();
        for mut gimbal in heard {
            gimbal.present = false;
            gimbal.device_silent_since_us = None;
            gimbal.holder = GimbalHolder::Unknown;
            gimbal.primary = None;
            gimbal.secondary = None;
            gimbal.version += 1;
            self.commit_as(data_plane(), Change::GimbalUpserted { gimbal });
        }
    }

    /// A vehicle's gimbal by what a command names: its manager's
    /// component and what the manager calls it. With neither, the
    /// vehicle's first that is there now.
    pub(crate) fn gimbal_of(
        &self,
        vehicle_id: &str,
        component_id: Option<u8>,
        device_id: Option<u8>,
    ) -> Option<&Gimbal> {
        let vehicle_id = self
            .vehicle(vehicle_id)
            .map_or(vehicle_id, |vehicle| vehicle.id.as_str());
        self.state
            .gimbals
            .values()
            .filter(|gimbal| gimbal.vehicle_id == vehicle_id && gimbal.present)
            .find(|gimbal| {
                component_id.is_none_or(|id| gimbal.component_id == id)
                    && device_id.is_none_or(|id| gimbal.device_id == id)
            })
    }

    /// A gimbal command that names no gimbal, for the vehicle's first.
    /// With none found it stays for the autopilot's own.
    pub(crate) fn gimbal_addressed(&self, vehicle_id: &str, command: Command) -> Command {
        let Command::Gimbal {
            component_id: None,
            device_id: None,
            action,
        } = command
        else {
            return command;
        };
        let first = self.gimbal_of(vehicle_id, None, None);
        Command::Gimbal {
            component_id: first.map(|gimbal| gimbal.component_id),
            device_id: first.map(|gimbal| gimbal.device_id),
            action,
        }
    }

    /// Taking control, when a command moves a gimbal its manager does not
    /// say UXDuck has: sent first, as its own command, so the movement is
    /// not refused and the history says what was done.
    pub(crate) fn gimbal_take_first(
        &self,
        command: &Command,
        vehicle: &Vehicle,
    ) -> Option<Command> {
        let Command::Gimbal {
            component_id,
            device_id,
            action,
        } = command
        else {
            return None;
        };
        if !action.moves() {
            return None;
        }
        let gimbal = self.gimbal_of(&vehicle.id, *component_id, *device_id)?;
        (self.gimbals.said.get(&gimbal.id) != Some(&GimbalHolder::Uxduck)).then_some(
            Command::Gimbal {
                component_id: *component_id,
                device_id: *device_id,
                action: GimbalAction::TakeControl,
            },
        )
    }

    /// A take UXDuck sends by itself, by its receipt: the gimbal is given
    /// back a while after its last movement.
    pub(crate) fn gimbal_taken_by_itself(&mut self, receipt_id: &str) {
        self.gimbals.takes.insert(receipt_id.to_owned());
    }

    /// Who a command would take a gimbal from, in words for the question
    /// asked first. None when it takes it from no one.
    pub(crate) fn gimbal_held_elsewhere(
        &self,
        vehicle_id: &str,
        command: &Command,
    ) -> Option<String> {
        let Command::Gimbal {
            component_id,
            device_id,
            action,
        } = command
        else {
            return None;
        };
        if !action.takes() {
            return None;
        }
        let gimbal = self.gimbal_of(vehicle_id, *component_id, *device_id)?;
        let name = self
            .vehicle(vehicle_id)
            .map_or("the vehicle", |vehicle| vehicle.name.as_str());
        match gimbal.holder {
            GimbalHolder::Mission => Some(format!("the mission {name} is flying")),
            GimbalHolder::Autopilot => Some(format!("{name}'s autopilot")),
            GimbalHolder::Other => Some(match gimbal.primary {
                Some(who) => format!(
                    "another ground station or component (system {}, component {})",
                    who.system_id, who.component_id
                ),
                None => "another ground station or component".into(),
            }),
            GimbalHolder::Unknown | GimbalHolder::NoOne | GimbalHolder::Uxduck => None,
        }
    }

    /// The question before a command takes a gimbal from whoever has it:
    /// a person confirms it, and an agent's or a plugin's was put to a
    /// person and is running approved.
    pub(crate) fn gimbal_confirmed(
        &self,
        vehicle_id: &str,
        command: &Command,
        confirmed: bool,
    ) -> Result<(), ApiError> {
        if self.running_approved || confirmed {
            return Ok(());
        }
        let Some(holder) = self.gimbal_held_elsewhere(vehicle_id, command) else {
            return Ok(());
        };
        Err(ApiError::new(
            ErrorKind::ConfirmationRequired,
            format!(
                "{} needs confirming: {holder} has the gimbal, and this takes it",
                command.label()
            ),
        ))
    }

    /// Whether a request is a gimbal command that takes a gimbal from
    /// whoever has it: an agent's or a plugin's is put to a person, as a
    /// person's own is confirmed.
    pub(crate) fn gimbal_asks(&self, request: &ClientRequest) -> bool {
        let ClientRequest::CommandSend(params) = request else {
            return false;
        };
        let command = self.gimbal_addressed(&params.vehicle_id, params.command.clone().into());
        self.gimbal_held_elsewhere(&params.vehicle_id, &command)
            .is_some()
    }

    /// A gimbal command ended. An accepted take or movement takes the
    /// gimbal from the mission until its next pointing step, a take
    /// UXDuck sent by itself starts the time after which it gives the
    /// gimbal back, each movement starts that time again, and a take by
    /// hand or a giving back ends it.
    pub(crate) fn gimbal_command_done(&mut self, receipt: &Receipt) {
        let Command::Gimbal {
            component_id,
            device_id,
            action,
        } = &receipt.command
        else {
            return;
        };
        let by_itself = self.gimbals.takes.remove(&receipt.id);
        self.stick_take_answered(&receipt.id, receipt.status == ReceiptStatus::Accepted);
        if receipt.status != ReceiptStatus::Accepted {
            // A giving back the gimbal refused is not asked for again. One
            // that got no answer is, at its next time.
            if receipt.status == ReceiptStatus::Rejected
                && *action == GimbalAction::ReleaseControl
                && let Some(id) = self
                    .gimbal_of(&receipt.vehicle_id, *component_id, *device_id)
                    .map(|gimbal| gimbal.id.clone())
            {
                self.gimbals.ours.remove(&id);
            }
            return;
        }
        let now = self.clock.now_us();
        if let Some(id) = self
            .gimbal_of(&receipt.vehicle_id, *component_id, *device_id)
            .map(|gimbal| gimbal.id.clone())
        {
            match action {
                GimbalAction::TakeControl if by_itself => {
                    self.gimbals.ours.insert(id, now);
                }
                GimbalAction::TakeControl | GimbalAction::ReleaseControl => {
                    self.gimbals.ours.remove(&id);
                }
                _ => {
                    if let Some(at) = self.gimbals.ours.get_mut(&id) {
                        *at = now;
                    }
                }
            }
        }
        if action.takes()
            && let Some(step) = self.mission_points(&receipt.vehicle_id)
        {
            self.gimbals.taken.insert(receipt.vehicle_id.clone(), step);
            self.review_holders(Some(&receipt.vehicle_id));
        }
    }

    /// The step of the mission a vehicle is flying that points its
    /// gimbals now: its latest pointing step up to the current one. None
    /// when it flies no mission, or the mission has let the gimbals go.
    /// An ArduPilot says nothing of this in its manager's status (S406),
    /// so it is read from the mission and the mode. An ArduPilot names
    /// the waypoint it flies to as its current step, and has by then run
    /// the steps between the waypoint before and that one (seen on
    /// `ArduCopter` 4.8), so every step up to the current one has begun.
    fn mission_points(&self, vehicle_id: &str) -> Option<u16> {
        let vehicle = self.vehicle(vehicle_id)?;
        if !vehicle.armed || vehicle.presence != Presence::Online {
            return None;
        }
        let mode = vehicle.mode.as_deref()?;
        if !(mode.eq_ignore_ascii_case("AUTO") || mode.eq_ignore_ascii_case("Mission")) {
            return None;
        }
        let mission = self.state.missions.get(&vehicle.id)?;
        let current = self.telemetry_value(&vehicle.id, "mission.current")?;
        if !current.is_finite() || current < 0.0 {
            return None;
        }
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a mission item number"
        )]
        let current = current as usize;
        let (step, item) = mission
            .items
            .iter()
            .enumerate()
            .take(current.saturating_add(1))
            .rev()
            .find(|(_, item)| {
                POINTING_STEPS.contains(&item.command) || item.command == DO_SET_ROI_NONE
            })?;
        if item.command == DO_SET_ROI_NONE {
            return None;
        }
        u16::try_from(step).ok()
    }

    /// Who has a gimbal, as people are told: the mission while it points
    /// a gimbal the autopilot manages and nobody took it since, and else
    /// who the manager says. A gimbal that manages itself says so itself
    /// when the autopilot takes it.
    fn gimbal_holder(&self, gimbal: &Gimbal) -> GimbalHolder {
        let said = self
            .gimbals
            .said
            .get(&gimbal.id)
            .copied()
            .unwrap_or(gimbal.holder);
        if !gimbal.present || gimbal.device_component_id == Some(gimbal.component_id) {
            return said;
        }
        match self.mission_points(&gimbal.vehicle_id) {
            Some(step) if self.gimbals.taken.get(&gimbal.vehicle_id) != Some(&step) => {
                GimbalHolder::Mission
            }
            _ => said,
        }
    }

    /// Says again who has each gimbal, of one vehicle or of all, where
    /// the mission took one or let it go.
    fn review_holders(&mut self, vehicle_id: Option<&str>) {
        let changed: Vec<Gimbal> = self
            .state
            .gimbals
            .values()
            .filter(|gimbal| vehicle_id.is_none_or(|id| gimbal.vehicle_id == id))
            .filter_map(|gimbal| {
                let holder = self.gimbal_holder(gimbal);
                (holder != gimbal.holder).then(|| Gimbal {
                    holder,
                    version: gimbal.version + 1,
                    ..gimbal.clone()
                })
            })
            .collect();
        for gimbal in changed {
            self.commit_as(data_plane(), Change::GimbalUpserted { gimbal });
        }
    }

    /// Once a second: who has each gimbal as the mission goes on, and the
    /// gimbals UXDuck took by itself given back
    /// [`GIVEN_BACK_AFTER_US`] after their last movement.
    pub fn review_gimbals(&mut self, now_us: u64) {
        if self.state.gimbals.is_empty() {
            return;
        }
        let taken: Vec<String> = self.gimbals.taken.keys().cloned().collect();
        for vehicle_id in taken {
            if self.mission_points(&vehicle_id) != self.gimbals.taken.get(&vehicle_id).copied() {
                self.gimbals.taken.remove(&vehicle_id);
            }
        }
        self.review_holders(None);
        self.review_sticks(now_us);
        let due: Vec<String> = self
            .gimbals
            .ours
            .iter()
            .filter(|(_, at)| now_us.saturating_sub(**at) >= GIVEN_BACK_AFTER_US)
            .map(|(id, _)| id.clone())
            .collect();
        for id in due {
            self.give_back(&id, now_us);
        }
    }

    /// Gives back a gimbal UXDuck took by itself, as a command of the
    /// daemon's own with its receipt. A gimbal that is gone, or that its
    /// manager says another has by now or no one, is no longer UXDuck's
    /// to give back. One that cannot be told now is tried again
    /// [`GIVEN_BACK_AFTER_US`] later, so it is given back in the end:
    /// while it or its vehicle is not heard, on a link nothing may be sent
    /// on such as a paused simulation's, until its manager says again who
    /// has it, and while one of its commands is still to be answered. So
    /// is a giving back that gets no answer.
    fn give_back(&mut self, id: &str, now_us: u64) {
        let Some(gimbal) = self.state.gimbals.get(id) else {
            self.gimbals.ours.remove(id);
            return;
        };
        match self.gimbals.said.get(id) {
            Some(GimbalHolder::Uxduck | GimbalHolder::Unknown) | None => {}
            Some(
                GimbalHolder::NoOne
                | GimbalHolder::Autopilot
                | GimbalHolder::Mission
                | GimbalHolder::Other,
            ) => {
                self.gimbals.ours.remove(id);
                return;
            }
        }
        // Tried again at its next time, whatever happens now.
        self.gimbals.ours.insert(id.to_owned(), now_us);
        if !gimbal.present || self.gimbals.said.get(id) != Some(&GimbalHolder::Uxduck) {
            return;
        }
        let Some(vehicle) = self.state.vehicles.get(&gimbal.vehicle_id) else {
            return;
        };
        if vehicle.presence != Presence::Online || self.not_writable(&vehicle.link_id).is_some() {
            return;
        }
        let waiting = self.state.receipts.values().any(|receipt| {
            receipt.status == ReceiptStatus::Sent
                && receipt.vehicle_id == vehicle.id
                && matches!(receipt.command, Command::Gimbal { .. })
        });
        if waiting {
            return;
        }
        let command = Command::Gimbal {
            component_id: Some(gimbal.component_id),
            device_id: Some(gimbal.device_id),
            action: GimbalAction::ReleaseControl,
        };
        let receipt = Receipt {
            id: format!("rc{}", &self.random.token()[..16]),
            vehicle_id: vehicle.id.clone(),
            command: command.clone(),
            status: ReceiptStatus::Sent,
            detail: None,
            reason: None,
            preflight: Vec::new(),
            preflight_override: None,
            sent_without_recent_heartbeat: false,
            forced: false,
            attempts: 0,
            client_id: daemon().client_id,
            operator_id: None,
            sent_by: None,
            remote: None,
            progress: None,
            created_at_us: now_us,
            completed_at_us: None,
            version: 1,
        };
        let order = CommandOrder {
            receipt_id: receipt.id.clone(),
            vehicle_id: vehicle.id.clone(),
            command,
            force: false,
        };
        self.commit_as(daemon(), Change::ReceiptUpserted { receipt });
        self.links.command(order);
    }

    /// A connection went away: it no longer looks at any gimbal.
    pub(crate) fn gimbals_unwatched_by(&mut self, connection: ConnectionId) {
        let Some(vehicles) = self.gimbals.watchers.remove(&connection) else {
            return;
        };
        for vehicle_id in vehicles {
            if !self.gimbals.watched(&vehicle_id) {
                self.links.watch_gimbals(&vehicle_id, false);
            }
        }
    }

    /// What follows a change, for the gimbals: they go with their vehicle
    /// and are gone while it is offline, and the mission has them or lets
    /// them go as the vehicle's mode and its mission change.
    pub(crate) fn gimbals_after(&mut self, change: &Change) {
        match change {
            Change::GimbalUpserted { gimbal } => {
                let gimbal = gimbal.clone();
                self.sticks_after(&gimbal);
            }
            Change::GimbalRemoved { id } => {
                let id = id.clone();
                self.sticks_lose(&id);
                self.resolve(&stick_held_back(&id));
            }
            _ => {}
        }
        if self.state.gimbals.is_empty() {
            return;
        }
        match change {
            Change::VehicleRemoved { id } => {
                let gone: Vec<String> = self
                    .state
                    .gimbals
                    .values()
                    .filter(|gimbal| gimbal.vehicle_id == *id)
                    .map(|gimbal| gimbal.id.clone())
                    .collect();
                for id in gone {
                    self.gimbals.said.remove(&id);
                    self.gimbals.ours.remove(&id);
                    self.commit_as(data_plane(), Change::GimbalRemoved { id });
                }
                self.gimbals.taken.remove(id);
            }
            Change::VehicleUpserted { vehicle } if vehicle.presence == Presence::Offline => {
                let heard: Vec<Gimbal> = self
                    .state
                    .gimbals
                    .values()
                    .filter(|gimbal| gimbal.vehicle_id == vehicle.id && gimbal.present)
                    .cloned()
                    .collect();
                for mut gimbal in heard {
                    gimbal.present = false;
                    gimbal.version += 1;
                    self.commit_as(data_plane(), Change::GimbalUpserted { gimbal });
                }
            }
            Change::VehicleUpserted { vehicle } => self.review_holders(Some(&vehicle.id)),
            Change::MissionUpserted { mission } => {
                let vehicle_id = mission.vehicle_id.clone();
                self.review_holders(Some(&vehicle_id));
            }
            _ => {}
        }
    }
}

/// A controller's or the keyboard's gimbal inputs (S407).
impl Engine {
    /// A routed controller's gimbal inputs began to ask for a turn. The
    /// vehicle's first gimbal is turned, as a gimbal command that names
    /// none goes to the first. One UXDuck has is turned at once. One no
    /// one has is taken first, as the claiming client and through the
    /// ordinary command path, as before a movement: the take is UXDuck's
    /// own, so the gimbal is given back a while after the inputs let go.
    /// One another has is not taken, since an input cannot be asked the
    /// question a person is: nothing is sent, and a notice says why.
    pub fn controller_gimbal_asked(&mut self, id: &str) {
        self.gimbals.sticks.remove(id);
        let Some((controller, connection, target)) = self.controller_sends(id) else {
            return;
        };
        let Some(gimbal) = self.gimbal_of(&target, None, None).cloned() else {
            return;
        };
        let take = Command::Gimbal {
            component_id: Some(gimbal.component_id),
            device_id: Some(gimbal.device_id),
            action: GimbalAction::TakeControl,
        };
        let condition = stick_held_back(&gimbal.id);
        // Said again at each push that does nothing.
        self.resolve(&condition);
        if let Some(holder) = self.gimbal_held_elsewhere(&target, &take) {
            let name = self
                .vehicle(&target)
                .map_or_else(|| "the vehicle".to_owned(), |vehicle| vehicle.name.clone());
            self.raise(Raise {
                severity: Severity::Warning,
                kind: NotificationKind::Transient,
                subject: Subject::Vehicle {
                    id: gimbal.vehicle_id.clone(),
                },
                title: format!("{} did not turn {name}'s gimbal", controller.name),
                body: Some(format!(
                    "Nothing was sent, because {holder} has the gimbal. Press Take control in the Camera tab or in the Cockpit's Controls, then push again."
                )),
                actions: Vec::new(),
                condition: Some(condition),
                report: None,
            });
            return;
        }
        let route = GimbalRoute {
            vehicle_id: gimbal.vehicle_id.clone(),
            component_id: gimbal.component_id,
            device_id: gimbal.device_id,
        };
        if self.gimbals.said.get(&gimbal.id) == Some(&GimbalHolder::Uxduck) {
            let now = self.clock.now_us();
            if let Some(at) = self.gimbals.ours.get_mut(&gimbal.id) {
                *at = now;
            }
            self.links.steer_gimbal(id, Some(route.clone()));
            self.gimbals.sticks.insert(
                id.to_owned(),
                Stick {
                    gimbal_id: gimbal.id,
                    gimbal: route,
                    take: None,
                },
            );
            return;
        }
        let Some(receipt_id) = self.send_for_controller(&controller, connection, target, &take)
        else {
            return;
        };
        self.gimbal_taken_by_itself(&receipt_id);
        self.gimbals.sticks.insert(
            id.to_owned(),
            Stick {
                gimbal_id: gimbal.id,
                gimbal: route,
                take: Some(receipt_id),
            },
        );
    }

    /// A controller's gimbal inputs ask for nothing any more: all let go,
    /// or its route ended. The data plane has told the gimbal to stop.
    /// The time after which UXDuck gives the gimbal back starts now.
    pub fn controller_gimbal_let_go(&mut self, id: &str) {
        let Some(stick) = self.gimbals.sticks.remove(id) else {
            return;
        };
        let now = self.clock.now_us();
        if stick.take.is_none()
            && let Some(at) = self.gimbals.ours.get_mut(&stick.gimbal_id)
        {
            *at = now;
        }
    }

    /// The take sent for a controller's gimbal inputs was answered: the
    /// controller turns the gimbal if the take was accepted and it still
    /// asks, and nothing if not, which the take's receipt says.
    fn stick_take_answered(&mut self, receipt_id: &str, accepted: bool) {
        let Some(id) = self
            .gimbals
            .sticks
            .iter()
            .find(|(_, stick)| stick.take.as_deref() == Some(receipt_id))
            .map(|(id, _)| id.clone())
        else {
            return;
        };
        if !accepted || self.controller_sends(&id).is_none() {
            self.gimbals.sticks.remove(&id);
            return;
        }
        if let Some(stick) = self.gimbals.sticks.get_mut(&id) {
            stick.take = None;
            let gimbal = stick.gimbal.clone();
            self.links.steer_gimbal(&id, Some(gimbal));
        }
    }

    /// Once a second: a gimbal a controller turns is not given back while
    /// it turns, and a controller that sends nothing any more, such as one
    /// whose input stopped, turns nothing.
    fn review_sticks(&mut self, now_us: u64) {
        let gone: Vec<String> = self
            .gimbals
            .sticks
            .keys()
            .filter(|id| self.controller_sends(id).is_none())
            .cloned()
            .collect();
        for id in gone {
            self.gimbals.sticks.remove(&id);
            self.links.steer_gimbal(&id, None);
        }
        let turning: Vec<String> = self
            .gimbals
            .sticks
            .values()
            .filter(|stick| stick.take.is_none())
            .map(|stick| stick.gimbal_id.clone())
            .collect();
        for gimbal_id in turning {
            if let Some(at) = self.gimbals.ours.get_mut(&gimbal_id) {
                *at = now_us;
            }
        }
    }

    /// What a gimbal now is, for the controllers that turn it: one gone,
    /// or another's by now, is no longer turned, and the notice that
    /// another has it is over once no other has.
    fn sticks_after(&mut self, gimbal: &Gimbal) {
        if !gimbal.present || gimbal.holder.held_elsewhere() {
            self.sticks_lose(&gimbal.id);
        }
        if !gimbal.present || !gimbal.holder.held_elsewhere() {
            self.resolve(&stick_held_back(&gimbal.id));
        }
    }

    /// No controller turns this gimbal any more. Each that did tells it
    /// to stop, which is the last it is told, and turns nothing until its
    /// inputs are let go and pushed again.
    fn sticks_lose(&mut self, gimbal_id: &str) {
        let lost: Vec<String> = self
            .gimbals
            .sticks
            .iter()
            .filter(|(_, stick)| stick.gimbal_id == gimbal_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in lost {
            self.gimbals.sticks.remove(&id);
            self.links.steer_gimbal(&id, None);
        }
    }

    /// Before a gimbal command asked for by a person, an agent or a
    /// plugin goes: the controllers that turn its gimbal stop first, so
    /// the command is the last word and the gimbal does not go on turning
    /// once it is given up. The data plane sends the stop before the
    /// command, in the order it is told.
    pub(crate) fn sticks_before(&mut self, vehicle_id: &str, command: &Command) {
        let Command::Gimbal {
            component_id,
            device_id,
            ..
        } = command
        else {
            return;
        };
        let Some(gimbal_id) = self
            .gimbal_of(vehicle_id, *component_id, *device_id)
            .map(|gimbal| gimbal.id.clone())
        else {
            return;
        };
        let turning: Vec<String> = self
            .gimbals
            .sticks
            .iter()
            .filter(|(_, stick)| stick.gimbal_id == gimbal_id && stick.take.is_none())
            .map(|(id, _)| id.clone())
            .collect();
        for id in turning {
            self.gimbals.sticks.remove(&id);
            self.links.steer_gimbal(&id, None);
        }
    }
}

/// A place to look at with no height is looked at on the ground: at
/// UXDuck's terrain height there, or where UXDuck has none, at the height
/// of the vehicle's home. A vehicle that has not said where its home is
/// still says how high it is above sea level and above home, and the
/// difference is its home's height.
pub(crate) fn look_at_the_ground(
    engine: &mut Engine,
    mut params: CommandSendParams,
) -> Result<CommandSendParams, ApiError> {
    let Command::Gimbal {
        component_id,
        device_id,
        action:
            GimbalAction::LookAt {
                latitude_deg,
                longitude_deg,
                altitude_amsl_m: None,
            },
    } = Command::from(params.command.clone())
    else {
        return Ok(params);
    };
    let dataset = engine.terrain_dataset_of(&params.vehicle_id);
    let ground = engine
        .terrain_answer(
            dataset,
            &TerrainAtParams {
                latitude_deg,
                longitude_deg,
                dataset: None,
            },
        )
        .and_then(|answer| match answer.ground {
            Ok(Ground::Height(height)) => Some(height),
            Ok(Ground::Sea) => Some(0.0),
            _ => None,
        });
    engine.terrain_flush();
    let said = |key: &str| {
        let id = &engine.vehicle(&params.vehicle_id)?.id;
        engine
            .telemetry_value(id, key)
            .filter(|height| height.is_finite())
    };
    let home = said("home.altitude_msl_m")
        .or_else(|| Some(said("altitude.amsl_m")? - said("altitude.relative_m")?));
    let Some(altitude_amsl_m) = ground.or(home) else {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "UXDuck has no ground height at that place yet, and the vehicle has not said how high its home is. Try again in a moment, or give the height to look at",
        ));
    };
    params.command = (&Command::Gimbal {
        component_id,
        device_id,
        action: GimbalAction::LookAt {
            latitude_deg,
            longitude_deg,
            altitude_amsl_m: Some(altitude_amsl_m),
        },
    })
        .into();
    Ok(params)
}

/// `gimbal/command` as the `command/send` it is: a gimbal command for the
/// gimbal's vehicle, so its checks, its approval and its receipt are a
/// command's.
pub(crate) fn as_command(
    engine: &Engine,
    params: &GimbalCommandParams,
) -> Result<ClientRequest, ApiError> {
    let gimbal = engine.state.gimbals.get(&params.gimbal_id).ok_or_else(|| {
        ApiError::new(
            ErrorKind::NotFound,
            format!("no gimbal {}", params.gimbal_id),
        )
    })?;
    Ok(ClientRequest::CommandSend(CommandSendParams {
        vehicle_id: gimbal.vehicle_id.clone(),
        command: CommandDto::Gimbal {
            component_id: Some(gimbal.component_id),
            device_id: Some(gimbal.device_id),
            action: params.action,
        },
        confirm: params.confirm,
        force: false,
        override_preflight: Vec::new(),
    }))
}

/// Every gimbal of every vehicle, by id.
pub(crate) fn listed(engine: &Engine) -> Vec<GimbalDto> {
    engine.state.gimbals.values().map(Into::into).collect()
}

fn readable(granted: &[Capability]) -> Result<(), ApiError> {
    if granted.contains(&Capability::Read) {
        return Ok(());
    }
    Err(ApiError::new(
        ErrorKind::CapabilityDenied,
        Capability::Read.denied_message(),
    ))
}

/// `gimbal/list`, for anyone who may read.
pub(crate) fn list(engine: &Engine, granted: &[Capability]) -> Result<Value, ApiError> {
    readable(granted)?;
    to_value(&GimbalListResult {
        gimbals: listed(engine),
    })
}

/// `gimbal/watch`: a connection starts or stops looking at where a
/// vehicle's gimbals point. The data plane is told when the first starts
/// and the last stops, so the gimbals say it ten times a second only
/// while someone looks.
pub(crate) fn watch(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &GimbalWatchParams,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    readable(granted)?;
    let vehicle_id = match engine.vehicle(&params.vehicle_id) {
        Some(vehicle) => vehicle.id.clone(),
        // A vehicle that is gone is no longer looked at.
        None if !params.watch => params.vehicle_id.clone(),
        None => {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                format!("no vehicle {}", params.vehicle_id),
            ));
        }
    };
    let before = engine.gimbals.watched(&vehicle_id);
    if params.watch {
        engine
            .gimbals
            .watchers
            .entry(connection)
            .or_default()
            .insert(vehicle_id.clone());
    } else if let Some(vehicles) = engine.gimbals.watchers.get_mut(&connection) {
        vehicles.remove(&vehicle_id);
        if vehicles.is_empty() {
            engine.gimbals.watchers.remove(&connection);
        }
    }
    let now = engine.gimbals.watched(&vehicle_id);
    if now != before {
        engine.links.watch_gimbals(&vehicle_id, now);
    }
    to_value(&Empty {})
}

/// The wire form of a gimbal change.
pub(crate) fn entity_changes(change: &Change) -> Vec<EntityChange> {
    match change {
        Change::GimbalUpserted { gimbal } => vec![EntityChange {
            gimbal: Some(gimbal.into()),
            ..upsert(EntityType::Gimbal, &gimbal.id)
        }],
        Change::GimbalRemoved { id } => vec![EntityChange {
            op: ChangeOp::Remove,
            ..upsert(EntityType::Gimbal, id)
        }],
        _ => Vec::new(),
    }
}
