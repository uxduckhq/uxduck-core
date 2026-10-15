//! Parameters and missions (ADR-0004, ADR-0001 entity versions, ADR-0007
//! low-bandwidth restart). The data plane runs the protocols; the engine
//! decides when to download, checks writes against what the client last
//! saw, journals what the vehicle confirms, and answers the request.
//!
//! A parameter set and a mission belong to one boot of the autopilot. When
//! a vehicle is first identified, or restarts, the data plane reports its
//! boot instant; if it differs from the one the stored set was downloaded
//! in, both are downloaded again, with a "restarted, refreshing" notice
//! when it is a restart. A daemon restart with the vehicle still running
//! downloads nothing.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use uxduck_contracts::dto::link::IdParams;
use uxduck_contracts::dto::mission::{
    Mission as MissionDto, MissionUploadParams, MissionUploadResult,
};
use uxduck_contracts::dto::parameter::{
    ParamSetParams, ParamSetResult, Parameter as ParameterDto, ParameterSet as ParameterSetDto,
    WriteStatus,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::history::ParamSource;
use uxduck_core::domain::link::LinkPolicy;
use uxduck_core::domain::mission::{Mission, MissionItem};
use uxduck_core::domain::notification::{NotificationKind, Severity, Subject};
use uxduck_core::domain::parameter::{
    ParamKind, Parameter, ParameterSet, TransferStatus, parameter_id, same_value,
};
use uxduck_core::domain::vehicle::Presence;
use uxduck_core::events::{Actor, Change};

use crate::commands::Raise;
use crate::rpc::handlers::to_value;
use crate::runtime::TransferOrder;
use crate::{ConnectionId, Engine, upsert};
use uxduck_contracts::dto::state::{ChangeOp, EntityChange, EntityType};

/// Boot instants closer than this are the same boot: the estimate moves
/// with link delay.
const SAME_BOOT_US: u64 = 5_000_000;

/// One parameter as the vehicle reported it.
#[derive(Clone, Debug, PartialEq)]
pub struct Reported {
    pub name: String,
    pub value: f64,
    pub kind: ParamKind,
}

/// Why a transfer did not go through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    NotSent(String),
    /// The vehicle said no, in its words.
    Rejected(String),
    Timeout,
}

impl Failure {
    fn status(&self) -> WriteStatus {
        match self {
            Self::NotSent(_) => WriteStatus::NotSent,
            Self::Rejected(_) => WriteStatus::Rejected,
            Self::Timeout => WriteStatus::Timeout,
        }
    }

    pub(crate) fn detail(&self, attempts: u8) -> String {
        match self {
            Self::NotSent(why) | Self::Rejected(why) => why.clone(),
            Self::Timeout if attempts > 0 => format!("no answer after {attempts} tries"),
            Self::Timeout => "no answer".into(),
        }
    }
}

#[derive(Debug)]
struct Write {
    reply: Option<(ConnectionId, Value)>,
    actor: Actor,
    /// Who asked, and why, for the vehicle's history (S74).
    author: uxduck_core::domain::history::Author,
    reason: Option<String>,
    vehicle_id: String,
    name: String,
    requested: f64,
}

#[derive(Debug)]
struct Upload {
    reply: Option<(ConnectionId, Value)>,
    actor: Actor,
    vehicle_id: String,
    items: Vec<MissionItem>,
}

/// Requests waiting for the vehicle, and each vehicle's latest boot.
#[derive(Debug, Default)]
pub(crate) struct Pending {
    writes: BTreeMap<String, Write>,
    uploads: BTreeMap<String, Upload>,
    /// The boot instant the data plane last reported, by vehicle.
    pub(crate) boots: BTreeMap<String, u64>,
}

fn daemon() -> Actor {
    Actor {
        client_id: "daemon".into(),
        operator_id: None,
    }
}

fn same_boot(a: Option<u64>, b: u64) -> bool {
    a.is_some_and(|a| a.abs_diff(b) < SAME_BOOT_US)
}

fn refreshing(vehicle_id: &str) -> String {
    format!("refreshing:{vehicle_id}")
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    let require = |capability: Capability| {
        if granted.contains(&capability) {
            Ok(())
        } else {
            Err(ApiError::new(
                ErrorKind::CapabilityDenied,
                capability.denied_message(),
            ))
        }
    };
    match request {
        ClientRequest::ParamRefresh(IdParams { id }) => {
            require(Capability::Read)?;
            let id = engine.writable_vehicle(&id)?;
            engine.download_parameters(&id);
            to_value(&ParameterSetDto::from(&engine.parameter_set(&id)))
        }
        ClientRequest::MissionRefresh(IdParams { id }) => {
            require(Capability::Read)?;
            let id = engine.writable_vehicle(&id)?;
            engine.download_mission(&id);
            to_value(&MissionDto::from(&engine.mission(&id)))
        }
        ClientRequest::ParamSet(params) => {
            require(Capability::Config)?;
            set_parameter(engine, connection, params)
        }
        ClientRequest::MissionUpload(params) => {
            require(Capability::Control)?;
            upload_mission(engine, connection, params)
        }
        _ => Err(ApiError::new(ErrorKind::Internal, "not a transfer method")),
    }
}

fn set_parameter(
    engine: &mut Engine,
    connection: ConnectionId,
    params: ParamSetParams,
) -> Result<Value, ApiError> {
    let vehicle = engine.vehicle(&params.vehicle_id).cloned().ok_or_else(|| {
        ApiError::new(
            ErrorKind::NotFound,
            format!("no vehicle {}", params.vehicle_id),
        )
    })?;
    let current = engine
        .state
        .parameters
        .get(&parameter_id(&vehicle.id, &params.name))
        .cloned()
        .ok_or_else(|| {
            ApiError::new(
                ErrorKind::NotFound,
                format!(
                    "{} has no parameter {} (download its parameters first)",
                    vehicle.name, params.name
                ),
            )
        })?;
    let reason = params
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_owned);
    if reason.as_ref().is_some_and(|r| r.chars().count() > 200) {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "a reason is up to 200 characters",
        ));
    }
    if !params.value.is_finite() || (current.kind.is_integer() && params.value.fract() != 0.0) {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!("{} takes a whole number", params.name),
        ));
    }
    if let Some(expected) = params.expected_value
        && !same_value(expected, current.value)
    {
        return Err(ApiError::new(
            ErrorKind::VersionConflict,
            format!("{} is now {}, not {expected}", params.name, current.value),
        ));
    }
    let (units, reboot_required) = engine.param_facts(&vehicle.id, &params.name);
    let result = |status: WriteStatus, detail: Option<String>| ParamSetResult {
        status,
        detail,
        requested: params.value,
        parameter: ParameterDto::from(&current),
        units: units.clone(),
        reboot_required,
    };
    if let Some(why) = engine.not_writable(&vehicle.link_id) {
        return to_value(&result(WriteStatus::NotSent, Some(why)));
    }
    if vehicle.presence != Presence::Online && !params.confirm {
        let presence = format!("{:?}", vehicle.presence).to_lowercase();
        return Err(ApiError::new(
            ErrorKind::ConfirmationRequired,
            format!(
                "setting {} needs confirming: {} is {presence}",
                params.name, vehicle.name
            ),
        ));
    }
    let order_id = format!("pw{}", &engine.random.token()[..16]);
    let actor = engine.actor(connection);
    let author = engine.author(connection);
    engine.transfers.writes.insert(
        order_id.clone(),
        Write {
            reply: engine.replying_to.take(),
            actor,
            author,
            reason,
            vehicle_id: vehicle.id.clone(),
            name: params.name.clone(),
            requested: params.value,
        },
    );
    engine.links.transfer(TransferOrder::SetParameter {
        order_id,
        vehicle_id: vehicle.id,
        name: params.name,
        value: params.value,
        kind: current.kind,
        by_file: params.by_file,
    });
    Ok(Value::Null)
}

fn upload_mission(
    engine: &mut Engine,
    connection: ConnectionId,
    params: MissionUploadParams,
) -> Result<Value, ApiError> {
    let vehicle = engine.vehicle(&params.vehicle_id).cloned().ok_or_else(|| {
        ApiError::new(
            ErrorKind::NotFound,
            format!("no vehicle {}", params.vehicle_id),
        )
    })?;
    let mut mission = engine.mission(&vehicle.id);
    if mission.version != params.expected_version {
        let mut error = ApiError::new(
            ErrorKind::VersionConflict,
            format!(
                "the mission on {} is at version {}, not {}",
                vehicle.name, mission.version, params.expected_version
            ),
        );
        error.data.current_version = Some(mission.version);
        return Err(error);
    }
    if mission.status == TransferStatus::Uploading {
        return Err(ApiError::new(
            ErrorKind::Busy,
            format!("an upload to {} is in progress", vehicle.name),
        ));
    }
    let items: Vec<MissionItem> = params.items.into_iter().map(Into::into).collect();
    let finite = items.iter().all(|i| {
        [
            i.param1,
            i.param2,
            i.param3,
            i.param4,
            i.latitude_deg,
            i.longitude_deg,
            i.altitude_m,
        ]
        .iter()
        .all(|v| v.is_finite())
    });
    if !finite || items.len() > usize::from(u16::MAX) {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "every value in a mission must be a number",
        ));
    }
    let why = engine.not_writable(&vehicle.link_id).or_else(|| {
        (vehicle.presence != Presence::Online).then(|| {
            format!(
                "{} is {}: a mission needs a conversation",
                vehicle.name,
                format!("{:?}", vehicle.presence).to_lowercase()
            )
        })
    });
    if let Some(why) = why {
        return to_value(&MissionUploadResult {
            status: WriteStatus::NotSent,
            detail: Some(why),
            mission: MissionDto::from(&mission),
        });
    }
    mission.status = TransferStatus::Uploading;
    mission.detail = None;
    engine.commit_as(daemon(), Change::MissionUpserted { mission });
    let order_id = format!("mu{}", &engine.random.token()[..16]);
    let actor = engine.actor(connection);
    engine.transfers.uploads.insert(
        order_id.clone(),
        Upload {
            reply: engine.replying_to.take(),
            actor,
            vehicle_id: vehicle.id.clone(),
            items: items.clone(),
        },
    );
    // Its terrain along the legs first (S120): a mission flown above
    // terrain waits for it.
    engine.upload_with_terrain(order_id, vehicle.id, items);
    Ok(Value::Null)
}

impl Engine {
    /// Writes a parameter on the engine's own account, without the
    /// parameter list having been downloaded: the simulation start's
    /// `SIM_SPEEDUP` and `TKOFF_ALT` (S33). Ends in `parameter_written`.
    pub(crate) fn write_parameter_as(
        &mut self,
        actor: Actor,
        vehicle_id: &str,
        name: &str,
        value: f64,
        kind: ParamKind,
    ) -> String {
        let order_id = format!("pw{}", &self.random.token()[..16]);
        self.transfers.writes.insert(
            order_id.clone(),
            Write {
                reply: None,
                actor,
                author: uxduck_core::domain::history::Author {
                    client_name: "UXDuck".into(),
                    ..Default::default()
                },
                reason: Some("starting the simulation".into()),
                vehicle_id: vehicle_id.to_owned(),
                name: name.to_owned(),
                requested: value,
            },
        );
        self.links.transfer(TransferOrder::SetParameter {
            order_id: order_id.clone(),
            vehicle_id: vehicle_id.to_owned(),
            name: name.to_owned(),
            value,
            kind,
            by_file: false,
        });
        order_id
    }

    /// Uploads a mission on the engine's own account: a simulation
    /// snapshot's (S34). Ends in `mission_uploaded`.
    pub(crate) fn upload_mission_as(
        &mut self,
        actor: Actor,
        vehicle_id: &str,
        items: Vec<MissionItem>,
    ) -> String {
        let mut mission = self.mission(vehicle_id);
        mission.status = TransferStatus::Uploading;
        mission.detail = None;
        self.commit_as(actor.clone(), Change::MissionUpserted { mission });
        let order_id = format!("mu{}", &self.random.token()[..16]);
        self.transfers.uploads.insert(
            order_id.clone(),
            Upload {
                reply: None,
                actor,
                vehicle_id: vehicle_id.to_owned(),
                items: items.clone(),
            },
        );
        self.links.transfer(TransferOrder::UploadMission {
            order_id: order_id.clone(),
            vehicle_id: vehicle_id.to_owned(),
            items,
        });
        order_id
    }
}

impl Engine {
    /// The vehicle's id, if its link may carry requests.
    fn writable_vehicle(&self, id: &str) -> Result<String, ApiError> {
        let vehicle = self
            .vehicle(id)
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no vehicle {id}")))?;
        if let Some(why) = self.not_writable(&vehicle.link_id) {
            return Err(ApiError::new(ErrorKind::InvalidValue, why));
        }
        Ok(vehicle.id.clone())
    }

    /// Why nothing may be sent on this link, if so.
    pub(crate) fn not_writable(&self, link_id: &str) -> Option<String> {
        match self.state.links.get(link_id) {
            None => Some("its link is gone".into()),
            Some(link) if link.policy == LinkPolicy::Read => {
                Some(format!("{} is read-only", link.name))
            }
            Some(_) => None,
        }
    }

    pub(crate) fn parameter_set(&self, vehicle_id: &str) -> ParameterSet {
        self.state
            .parameter_sets
            .get(vehicle_id)
            .cloned()
            .unwrap_or_else(|| ParameterSet {
                vehicle_id: vehicle_id.to_owned(),
                status: TransferStatus::Unknown,
                detail: None,
                count: 0,
                boot_instant_us: None,
                synced_at_us: None,
            })
    }

    pub(crate) fn mission(&self, vehicle_id: &str) -> Mission {
        self.state
            .missions
            .get(vehicle_id)
            .cloned()
            .unwrap_or_else(|| Mission {
                vehicle_id: vehicle_id.to_owned(),
                items: Vec::new(),
                status: TransferStatus::Unknown,
                detail: None,
                boot_instant_us: None,
                synced_at_us: None,
                version: 0,
            })
    }

    fn download_parameters(&mut self, vehicle_id: &str) {
        let mut set = self.parameter_set(vehicle_id);
        set.status = TransferStatus::Downloading;
        set.detail = None;
        self.commit_as(daemon(), Change::ParameterSetUpserted { set });
        self.links.transfer(TransferOrder::DownloadParameters {
            vehicle_id: vehicle_id.to_owned(),
        });
    }

    fn download_mission(&mut self, vehicle_id: &str) {
        let mut mission = self.mission(vehicle_id);
        if mission.status == TransferStatus::Uploading {
            return;
        }
        mission.status = TransferStatus::Downloading;
        mission.detail = None;
        self.commit_as(daemon(), Change::MissionUpserted { mission });
        self.links.transfer(TransferOrder::DownloadMission {
            vehicle_id: vehicle_id.to_owned(),
        });
    }

    pub(crate) fn vehicle_name(&self, vehicle_id: &str) -> String {
        self.vehicle(vehicle_id)
            .map_or_else(|| "The vehicle".to_owned(), |v| v.name.clone())
    }

    /// The data plane knows when a vehicle's autopilot booted: at first
    /// sight, and after each restart it sees (`restarted`). A new boot means
    /// new parameters and mission; at first sight the stored boot decides,
    /// so a daemon restart downloads nothing.
    pub fn vehicle_booted(&mut self, vehicle_id: &str, boot_instant_us: u64, restarted: bool) {
        self.transfers
            .boots
            .insert(vehicle_id.to_owned(), boot_instant_us);
        let Some(vehicle) = self.vehicle(vehicle_id).cloned() else {
            return;
        };
        if self.not_writable(&vehicle.link_id).is_some() {
            return;
        }
        let set = self.parameter_set(&vehicle.id);
        let mission = self.mission(&vehicle.id);
        let params_current = !restarted
            && set.status == TransferStatus::Synced
            && same_boot(set.boot_instant_us, boot_instant_us);
        let mission_current = !restarted
            && mission.status == TransferStatus::Synced
            && same_boot(mission.boot_instant_us, boot_instant_us);
        let restarted = restarted
            || set
                .boot_instant_us
                .is_some_and(|known| !same_boot(Some(known), boot_instant_us));
        if restarted {
            self.raise(Raise {
                severity: Severity::Info,
                kind: NotificationKind::Persistent,
                subject: Subject::Vehicle {
                    id: vehicle.id.clone(),
                },
                title: format!("{} restarted", vehicle.name),
                body: Some("Downloading its parameters and mission again.".into()),
                actions: Vec::new(),
                condition: Some(refreshing(&vehicle.id)),
                report: None,
            });
        }
        if !params_current {
            self.download_parameters(&vehicle.id);
        }
        if !mission_current {
            self.download_mission(&vehicle.id);
        }
    }

    /// Clears the "refreshing" notice once neither download is running.
    fn refreshed(&mut self, vehicle_id: &str) {
        let busy = self.parameter_set(vehicle_id).status == TransferStatus::Downloading
            || self.mission(vehicle_id).status == TransferStatus::Downloading;
        if !busy {
            self.resolve(&refreshing(vehicle_id));
        }
    }

    fn failed(&mut self, vehicle_id: &str, what: &str, detail: &str) {
        let name = self.vehicle_name(vehicle_id);
        self.raise(Raise {
            severity: Severity::Warning,
            kind: NotificationKind::Transient,
            subject: Subject::Vehicle {
                id: vehicle_id.to_owned(),
            },
            title: format!("Could not download the {what} of {name}"),
            body: Some(detail.to_owned()),
            actions: Vec::new(),
            condition: None,
            report: Some("download-failed".into()),
        });
    }

    /// A parameter download ended.
    pub fn parameters_downloaded(
        &mut self,
        vehicle_id: &str,
        result: Result<Vec<Reported>, Failure>,
        defaults: &std::collections::BTreeMap<String, f64>,
    ) {
        let Some(id) = self.vehicle(vehicle_id).map(|v| v.id.clone()) else {
            return;
        };
        let mut set = self.parameter_set(&id);
        // By the list no defaults come: those of this boot are kept (S74).
        let boot = self.transfers.boots.get(&id).copied();
        let same_boot = set.boot_instant_us.is_some() && set.boot_instant_us == boot;
        match result {
            Ok(reported) => {
                let parameters: Vec<Parameter> = reported
                    .into_iter()
                    .map(|r| {
                        let default = defaults.get(&r.name).copied().or_else(|| {
                            same_boot
                                .then(|| self.state.parameters.get(&parameter_id(&id, &r.name)))
                                .flatten()
                                .and_then(|p| p.default)
                        });
                        Parameter {
                            vehicle_id: id.clone(),
                            name: r.name,
                            value: r.value,
                            kind: r.kind,
                            default,
                        }
                    })
                    .collect();
                let prefix = format!("{id}/");
                let removed: Vec<String> = self
                    .state
                    .parameters
                    .range(prefix.clone()..)
                    .take_while(|(key, _)| key.starts_with(&prefix))
                    .map(|(_, p)| p.name.clone())
                    .filter(|name| !parameters.iter().any(|p| p.name == *name))
                    .collect();
                // What changed since the last download, such as on a restart.
                let changes: Vec<(String, f64, f64)> = parameters
                    .iter()
                    .filter_map(|p| {
                        let before = self.state.parameters.get(&p.id())?;
                        (!same_value(before.value, p.value))
                            .then(|| (p.name.clone(), before.value, p.value))
                    })
                    .collect();
                for (name, from, to) in changes {
                    self.record_param(
                        &id,
                        &name,
                        (Some(from), to),
                        ParamSource::Download,
                        None,
                        None,
                    );
                }
                set.status = TransferStatus::Synced;
                set.detail = None;
                set.count = u32::try_from(parameters.len()).unwrap_or(u32::MAX);
                set.boot_instant_us = self.transfers.boots.get(&id).copied();
                set.synced_at_us = Some(self.clock.now_us());
                self.commit_as(
                    daemon(),
                    Change::ParametersReplaced {
                        set,
                        parameters,
                        removed,
                    },
                );
            }
            Err(failure) => {
                let detail = failure.detail(0);
                set.status = TransferStatus::Failed;
                set.detail = Some(detail.clone());
                self.commit_as(daemon(), Change::ParameterSetUpserted { set });
                self.failed(&id, "parameters", &detail);
            }
        }
        self.refreshed(&id);
    }

    /// Records a change of value in the vehicle's history (S74), leaving
    /// out those that change by themselves or cannot be set: volatile and
    /// read-only parameters by the vehicle's descriptions, and those UXDuck
    /// knows without them (`unrecorded`). Changes within one sitting fold
    /// into one entry (`record_param_change`).
    fn record_param(
        &mut self,
        vehicle_id: &str,
        name: &str,
        (from, to): (Option<f64>, f64),
        source: ParamSource,
        reason: Option<String>,
        author: Option<uxduck_core::domain::history::Author>,
    ) {
        if uxduck_core::domain::history::unrecorded(name) {
            return;
        }
        let definitions = self.vehicle_definitions(vehicle_id);
        let vehicle_type = self.vehicle(vehicle_id).map(|v| v.vehicle_type.clone());
        let unwatched = definitions.as_deref().is_some_and(|set| {
            set.find(name, vehicle_type.as_deref() == Some("Helicopter"))
                .is_some_and(|d| d.volatile || d.read_only)
        });
        if unwatched {
            return;
        }
        self.record_param_change(vehicle_id, name, (from, to), source, reason, author);
    }

    /// A parameter's default as last downloaded, kept through changes of value.
    fn default_of(&self, vehicle_id: &str, name: &str) -> Option<f64> {
        self.state
            .parameters
            .get(&parameter_id(vehicle_id, name))
            .and_then(|p| p.default)
    }

    /// A parameter the vehicle reported outside a download: kept if it
    /// differs from what is known. Nobody in UXDuck asked for the change:
    /// another ground station made it if one was heard (`station`), and
    /// otherwise the vehicle did, as far as UXDuck can tell.
    pub fn parameter_reported(&mut self, vehicle_id: &str, reported: Reported, station: bool) {
        let Some(id) = self.vehicle(vehicle_id).map(|v| v.id.clone()) else {
            return;
        };
        let known = self
            .state
            .parameters
            .get(&parameter_id(&id, &reported.name))
            .is_some_and(|p| same_value(p.value, reported.value) && p.kind == reported.kind);
        if !known {
            let before = self
                .state
                .parameters
                .get(&parameter_id(&id, &reported.name))
                .map(|p| p.value);
            self.record_param(
                &id,
                &reported.name,
                (before, reported.value),
                if station {
                    ParamSource::OtherStation
                } else {
                    ParamSource::Vehicle
                },
                None,
                None,
            );
            let default = self.default_of(&id, &reported.name);
            self.commit_as(
                daemon(),
                Change::ParameterChanged {
                    parameter: Parameter {
                        vehicle_id: id,
                        name: reported.name,
                        value: reported.value,
                        kind: reported.kind,
                        default,
                    },
                },
            );
        }
    }

    /// A parameter write ended: journal what the vehicle now reports, as
    /// the client that asked, and answer it.
    pub fn parameter_written(
        &mut self,
        order_id: &str,
        result: Result<Reported, Failure>,
        attempts: u8,
    ) {
        let Some(write) = self.transfers.writes.remove(order_id) else {
            return;
        };
        let key = parameter_id(&write.vehicle_id, &write.name);
        let (status, detail) = match result {
            Ok(reported) => {
                let changed = self
                    .state
                    .parameters
                    .get(&key)
                    .is_none_or(|p| !same_value(p.value, reported.value));
                let accepted = same_value(reported.value, write.requested);
                let detail = (!accepted).then(|| format!("the vehicle kept {}", reported.value));
                if changed {
                    let before = self.state.parameters.get(&key).map(|p| p.value);
                    let source = if write.author.client_kind.as_deref() == Some("agent") {
                        ParamSource::Agent
                    } else {
                        ParamSource::Person
                    };
                    self.record_param(
                        &write.vehicle_id,
                        &reported.name,
                        (before, reported.value),
                        source,
                        write.reason.clone(),
                        Some(write.author.clone()),
                    );
                    let default = self.default_of(&write.vehicle_id, &reported.name);
                    self.commit_as(
                        write.actor.clone(),
                        Change::ParameterChanged {
                            parameter: Parameter {
                                vehicle_id: write.vehicle_id.clone(),
                                name: reported.name,
                                value: reported.value,
                                kind: reported.kind,
                                default,
                            },
                        },
                    );
                }
                let status = if accepted {
                    WriteStatus::Accepted
                } else {
                    WriteStatus::Rejected
                };
                (status, detail)
            }
            Err(failure) => (failure.status(), Some(failure.detail(attempts))),
        };
        if self.is_launch_order(order_id) {
            self.launch_order_done(order_id, status == WriteStatus::Accepted, detail);
            return;
        }
        let Some(parameter) = self.state.parameters.get(&key).cloned() else {
            return;
        };
        if status != WriteStatus::Accepted {
            let name = self.vehicle_name(&write.vehicle_id);
            self.raise(Raise {
                severity: Severity::Warning,
                kind: NotificationKind::Transient,
                subject: Subject::Vehicle {
                    id: write.vehicle_id.clone(),
                },
                title: format!("{name} didn't accept the new value for {}", write.name),
                body: detail.clone(),
                actions: Vec::new(),
                condition: None,
                report: Some("parameter-refused".into()),
            });
        }
        let (units, reboot_required) = self.param_facts(&write.vehicle_id, &write.name);
        let result = ParamSetResult {
            status,
            detail,
            requested: write.requested,
            parameter: ParameterDto::from(&parameter),
            units,
            reboot_required,
        };
        if let Some((connection, request_id)) = write.reply {
            let frame = json!({ "jsonrpc": "2.0", "id": request_id, "result": result });
            self.send(connection, &frame);
        }
    }

    /// A mission download ended. The version moves only if the items did.
    pub fn mission_downloaded(
        &mut self,
        vehicle_id: &str,
        result: Result<Vec<MissionItem>, Failure>,
    ) {
        let Some(id) = self.vehicle(vehicle_id).map(|v| v.id.clone()) else {
            return;
        };
        let mut mission = self.mission(&id);
        match result {
            Ok(items) => {
                if items != mission.items {
                    mission.items = items;
                    mission.version += 1;
                }
                mission.status = TransferStatus::Synced;
                mission.detail = None;
                mission.boot_instant_us = self.transfers.boots.get(&id).copied();
                mission.synced_at_us = Some(self.clock.now_us());
                self.commit_as(daemon(), Change::MissionUpserted { mission });
            }
            Err(failure) => {
                let detail = failure.detail(0);
                mission.status = TransferStatus::Failed;
                mission.detail = Some(detail.clone());
                self.commit_as(daemon(), Change::MissionUpserted { mission });
                self.failed(&id, "mission", &detail);
            }
        }
        self.refreshed(&id);
    }

    /// A mission upload ended: on success the mission is the uploaded one,
    /// at the next version, journaled as the client that asked.
    pub fn mission_uploaded(&mut self, order_id: &str, result: Result<(), Failure>) {
        let Some(upload) = self.transfers.uploads.remove(order_id) else {
            return;
        };
        let mut mission = self.mission(&upload.vehicle_id);
        if result.is_ok() {
            self.practice_mission_uploaded(&upload.vehicle_id, &upload.actor.client_id);
        }
        let (status, detail, actor) = match result {
            Ok(()) => {
                mission.items = upload.items;
                mission.version += 1;
                mission.status = TransferStatus::Synced;
                mission.detail = None;
                mission.boot_instant_us = self.transfers.boots.get(&upload.vehicle_id).copied();
                mission.synced_at_us = Some(self.clock.now_us());
                (WriteStatus::Accepted, None, upload.actor)
            }
            Err(failure) => {
                let detail = failure.detail(0);
                // What the vehicle holds after a failed upload is unknown.
                mission.status = TransferStatus::Failed;
                mission.detail = Some(format!("upload failed: {detail}"));
                (failure.status(), Some(detail), daemon())
            }
        };
        self.commit_as(
            actor,
            Change::MissionUpserted {
                mission: mission.clone(),
            },
        );
        if status == WriteStatus::Accepted {
            // Its terrain along the new legs (S119), once it is the mission.
            self.terrain_check_mission(&upload.vehicle_id);
        }
        let result = MissionUploadResult {
            status,
            detail,
            mission: MissionDto::from(&mission),
        };
        if let Some((connection, request_id)) = upload.reply {
            let frame = json!({ "jsonrpc": "2.0", "id": request_id, "result": result });
            self.send(connection, &frame);
        }
        if self.is_launch_order(order_id) {
            let accepted = result.status == WriteStatus::Accepted;
            self.launch_order_done(order_id, accepted, result.detail);
        }
    }
}

/// The wire form of a parameter or mission change.
pub(crate) fn entity_changes(change: &Change) -> Vec<EntityChange> {
    match change {
        Change::ParameterSetUpserted { set } => vec![EntityChange {
            parameter_set: Some(set.into()),
            ..upsert(EntityType::ParameterSet, &set.vehicle_id)
        }],
        Change::ParametersReplaced {
            set,
            parameters,
            removed,
        } => {
            let mut changes: Vec<EntityChange> = removed
                .iter()
                .map(|name| EntityChange {
                    op: ChangeOp::Remove,
                    ..upsert(
                        EntityType::Parameter,
                        &uxduck_core::domain::parameter::parameter_id(&set.vehicle_id, name),
                    )
                })
                .collect();
            changes.extend(parameters.iter().map(|parameter| EntityChange {
                parameter: Some(parameter.into()),
                ..upsert(EntityType::Parameter, &parameter.id())
            }));
            changes.push(EntityChange {
                parameter_set: Some(set.into()),
                ..upsert(EntityType::ParameterSet, &set.vehicle_id)
            });
            changes
        }
        Change::ParameterChanged { parameter } => vec![EntityChange {
            parameter: Some(parameter.into()),
            ..upsert(EntityType::Parameter, &parameter.id())
        }],
        Change::MissionUpserted { mission } => vec![EntityChange {
            mission: Some(mission.into()),
            ..upsert(EntityType::Mission, &mission.vehicle_id)
        }],
        _ => Vec::new(),
    }
}
