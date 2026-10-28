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
use uxduck_contracts::dto::plan::{PlanUploadResult, PlanUploadStatus};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::history::ParamSource;
use uxduck_core::domain::link::LinkPolicy;
use uxduck_core::domain::mission::{Mission, MissionItem, MissionOrigin};
use uxduck_core::domain::notification::{NotificationKind, Severity, Subject};
use uxduck_core::domain::parameter::{
    ParamKind, Parameter, ParameterSet, TransferProgress, TransferStatus, parameter_id, same_value,
};
use uxduck_core::domain::plan::{PlanRevision, PlanUpload};
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
    /// `MAV_MISSION_TYPE`: 0 the mission, 1 the fence, 2 rally points (S128).
    kind: u8,
}

/// Which transfer a progress report is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProgressKind {
    Parameters,
    MissionDownload,
    MissionUpload,
}

/// Progress is journaled at most this often while it runs.
const PROGRESS_EVERY_US: u64 = 250_000;

/// Requests waiting for the vehicle, and each vehicle's latest boot.
#[derive(Debug, Default)]
pub(crate) struct Pending {
    writes: BTreeMap<String, Write>,
    uploads: BTreeMap<String, Upload>,
    /// When each running transfer's progress was last journaled, by
    /// vehicle and kind, so a fast one is not an event per item.
    progress_at: BTreeMap<(String, ProgressKind), u64>,
    /// Plans going up (S127), by upload order, then `verify:<vehicle>`.
    pub(crate) plan_jobs: BTreeMap<String, PlanJob>,
    /// Vehicles whose next mission download goes on to the fence and rally
    /// points (S128): asked for by a person, or a plan's read-back.
    with_parts: std::collections::BTreeSet<String>,
    /// The boot instant the data plane last reported, by vehicle.
    pub(crate) boots: BTreeMap<String, u64>,
    /// Simulator links, by id and version, whose vehicle was sent its
    /// type's example mission (P12): once per start.
    pub(crate) examples_sent: std::collections::BTreeSet<String>,
    /// How the next mission recorded for each vehicle came: an upload
    /// UXDuck made, with who (S134).
    pub(crate) arriving: BTreeMap<
        String,
        (
            uxduck_core::domain::mission::MissionOrigin,
            uxduck_core::events::Actor,
        ),
    >,
}

impl Pending {
    pub(crate) fn uses_vehicle(&self, id: &str) -> bool {
        self.writes.values().any(|write| write.vehicle_id == id)
            || self.uploads.values().any(|upload| upload.vehicle_id == id)
    }
}

fn daemon() -> Actor {
    Actor {
        client_id: "daemon".into(),
        operator_id: None,
        identity: None,
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
        ClientRequest::MissionHistory(IdParams { id }) => {
            require(Capability::Read)?;
            let id = engine
                .vehicle(&id)
                .map(|v| v.id.clone())
                .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no vehicle {id}")))?;
            to_value(&uxduck_contracts::dto::mission::MissionHistoryResult {
                records: engine
                    .mission_history(&id)
                    .into_iter()
                    .map(Into::into)
                    .collect(),
            })
        }
        ClientRequest::MissionRestore(params) => {
            require(Capability::Control)?;
            restore(engine, connection, params)
        }
        ClientRequest::MissionRefresh(IdParams { id }) => {
            require(Capability::Read)?;
            let id = engine.writable_vehicle(&id)?;
            // Asked for: the fence and rally points come too (S128).
            if engine
                .vehicle(&id)
                .is_none_or(|vehicle| vehicle.vehicle_type != "Airship")
            {
                engine.transfers.with_parts.insert(id.clone());
            }
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
        ClientRequest::TransferCancel(params) => {
            require(Capability::Control)?;
            let id = engine.writable_vehicle(&params.vehicle_id)?;
            let kinds: &[ProgressKind] = match params.what {
                uxduck_contracts::dto::mission::TransferWhat::Parameters => {
                    &[ProgressKind::Parameters]
                }
                uxduck_contracts::dto::mission::TransferWhat::Mission => {
                    &[ProgressKind::MissionUpload, ProgressKind::MissionDownload]
                }
            };
            for kind in kinds {
                engine.links.transfer(TransferOrder::CancelTransfer {
                    vehicle_id: id.clone(),
                    kind: *kind,
                });
            }
            Ok(json!({}))
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
            kind: 0,
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
        self.write_parameter_for(
            actor,
            uxduck_core::domain::history::Author {
                client_name: "UXDuck".into(),
                ..Default::default()
            },
            "starting the simulation",
            (vehicle_id, name, value),
            kind,
        )
    }

    /// Writes a parameter for someone, with a reason, as a person's write
    /// is kept (S74): the radio calibration's values (S187). Ends in
    /// `parameter_written`, answering no one.
    pub(crate) fn write_parameter_for(
        &mut self,
        actor: Actor,
        author: uxduck_core::domain::history::Author,
        reason: &str,
        (vehicle_id, name, value): (&str, &str, f64),
        kind: ParamKind,
    ) -> String {
        let order_id = format!("pw{}", &self.random.token()[..16]);
        self.transfers.writes.insert(
            order_id.clone(),
            Write {
                reply: None,
                actor,
                author,
                reason: Some(reason.to_owned()),
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
                kind: 0,
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
    pub(crate) fn writable_vehicle(&self, id: &str) -> Result<String, ApiError> {
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
            Some(link) if link.paused => Some("the simulation is paused. Resume it first".into()),
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
                progress: None,
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
                progress: None,
                fence: Vec::new(),
                rally: Vec::new(),
            })
    }

    pub(crate) fn download_parameters(&mut self, vehicle_id: &str) {
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
            self.setup_restarted(&vehicle.id);
            self.preflight_restarted(&vehicle.id);
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
                set.progress = None;
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
                set.progress = None;
                set.detail = Some(detail.clone());
                self.commit_as(daemon(), Change::ParameterSetUpserted { set });
                self.failed(&id, "parameters", &detail);
            }
        }
        self.refreshed(&id);
    }

    /// How far a transfer has come (S127): kept on the parameter set or
    /// the mission while it runs, at most four times a second unless it
    /// asks again or finishes.
    pub fn transfer_progress(
        &mut self,
        vehicle_id: &str,
        kind: ProgressKind,
        done: u32,
        total: Option<u32>,
        attempt: u8,
    ) {
        let Some(id) = self.vehicle(vehicle_id).map(|v| v.id.clone()) else {
            return;
        };
        let now = self.clock.now_us();
        let last = self.transfers.progress_at.get(&(id.clone(), kind)).copied();
        let before = match kind {
            ProgressKind::Parameters => self.parameter_set(&id).progress,
            _ => self.mission(&id).progress,
        };
        let new_try = before.is_none_or(|p| p.attempt != attempt);
        let finished = total.is_some_and(|t| done >= t);
        if !new_try
            && !finished
            && last.is_some_and(|at| now.saturating_sub(at) < PROGRESS_EVERY_US)
        {
            return;
        }
        self.transfers.progress_at.insert((id.clone(), kind), now);
        // It last moved when more arrived; a new try alone is not progress.
        let at_us = match before {
            Some(p) if p.done == done => p.at_us,
            _ => now,
        };
        let progress = Some(TransferProgress {
            done,
            total,
            attempt,
            at_us,
            upload: kind == ProgressKind::MissionUpload,
        });
        match kind {
            ProgressKind::Parameters => {
                let mut set = self.parameter_set(&id);
                if set.status != TransferStatus::Downloading {
                    return;
                }
                set.progress = progress;
                self.commit_as(daemon(), Change::ParameterSetUpserted { set });
            }
            ProgressKind::MissionDownload | ProgressKind::MissionUpload => {
                let mut mission = self.mission(&id);
                if !matches!(
                    mission.status,
                    TransferStatus::Downloading | TransferStatus::Uploading
                ) {
                    return;
                }
                mission.progress = progress;
                self.commit_as(daemon(), Change::MissionUpserted { mission });
            }
        }
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
        // One of the radio calibration's values (S187).
        self.radio_written(order_id, status == WriteStatus::Accepted);
        // One of the airspeed ratio's (S190).
        self.airspeed_written(order_id, status == WriteStatus::Accepted);
        // A change that takes effect on restarting makes a restart due (S192).
        let asked = write.reply.is_some() && status == WriteStatus::Accepted;
        self.preflight_written(&write.vehicle_id, &write.name, asked);
        if write.reply.is_some() {
            self.track_task(
                &write.vehicle_id,
                uxduck_contracts::dto::analytics::EngagementTask::ParameterWritten,
                status == WriteStatus::Accepted,
            );
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
        mission.progress = None;
        match result {
            Ok(items) => {
                if items != mission.items {
                    mission.items = items;
                    mission.version += 1;
                }
                // The fence and rally points next, when they were asked for.
                if self.transfers.with_parts.contains(&id) {
                    self.commit_as(daemon(), Change::MissionUpserted { mission });
                    self.links.transfer(TransferOrder::DownloadMissionPart {
                        vehicle_id: id,
                        kind: 1,
                    });
                    return;
                }
                mission.status = TransferStatus::Synced;
                mission.detail = None;
                mission.boot_instant_us = self.transfers.boots.get(&id).copied();
                mission.synced_at_us = Some(self.clock.now_us());
                let empty = mission.items.len() <= 1;
                self.commit_as(daemon(), Change::MissionUpserted { mission });
                self.record_mission(&id);
                // A new simulated vehicle gets its type's example (P12).
                if empty {
                    self.send_example_mission(&id);
                }
            }
            Err(failure) => {
                let detail = failure.detail(0);
                self.transfers.with_parts.remove(&id);
                mission.status = TransferStatus::Failed;
                mission.detail = Some(detail.clone());
                self.commit_as(daemon(), Change::MissionUpserted { mission });
                self.failed(&id, "mission", &detail);
            }
        }
        self.refreshed(&id);
        let ok = self.mission(&id).status == TransferStatus::Synced;
        self.plan_mission_read(&id, ok);
    }

    /// A fence (1) or rally points (2) download ended (S128). One the
    /// autopilot cannot give (an older ArduPilot) is left as it was known;
    /// the mission is synced either way once rally points are done.
    pub fn mission_part_downloaded(
        &mut self,
        vehicle_id: &str,
        kind: u8,
        result: Result<Vec<MissionItem>, Failure>,
    ) {
        let Some(id) = self.vehicle(vehicle_id).map(|v| v.id.clone()) else {
            return;
        };
        let mut mission = self.mission(&id);
        mission.progress = None;
        if let Ok(items) = result {
            let part = if kind == 1 {
                &mut mission.fence
            } else {
                &mut mission.rally
            };
            if *part != items {
                *part = items;
                mission.version += 1;
            }
        }
        if kind == 1 {
            self.commit_as(daemon(), Change::MissionUpserted { mission });
            self.links.transfer(TransferOrder::DownloadMissionPart {
                vehicle_id: id,
                kind: 2,
            });
            return;
        }
        self.transfers.with_parts.remove(&id);
        mission.status = TransferStatus::Synced;
        mission.detail = None;
        mission.boot_instant_us = self.transfers.boots.get(&id).copied();
        mission.synced_at_us = Some(self.clock.now_us());
        self.commit_as(daemon(), Change::MissionUpserted { mission });
        self.record_mission(&id);
        self.refreshed(&id);
        self.plan_mission_read(&id, true);
    }

    /// A mission upload ended: on success the mission is the uploaded one,
    /// at the next version, journaled as the client that asked.
    pub fn mission_uploaded(&mut self, order_id: &str, result: Result<(), Failure>) {
        let Some(upload) = self.transfers.uploads.remove(order_id) else {
            return;
        };
        let outcome = result.clone();
        let mut mission = self.mission(&upload.vehicle_id);
        mission.progress = None;
        if result.is_ok() {
            self.practice_mission_uploaded(&upload.vehicle_id, &upload.actor.client_id);
        }
        // A plan's mission goes on to its fence and rally points (S128).
        let more = self
            .transfers
            .plan_jobs
            .get(order_id)
            .is_some_and(|job| !job.parts.is_empty());
        // How the mission came, for its record (S134): a plan's is recorded
        // once read back, any other upload now.
        let plain = !self.transfers.plan_jobs.contains_key(order_id);
        if plain && upload.reply.is_some() && upload.kind == 0 {
            self.track_task(
                &upload.vehicle_id,
                uxduck_contracts::dto::analytics::EngagementTask::MissionUploaded,
                result.is_ok(),
            );
        }
        if result.is_ok() && upload.kind == 0 {
            let origin = match self.transfers.plan_jobs.get(order_id) {
                Some(_) if upload.actor.client_id == crate::launch::ACTOR => MissionOrigin::Example,
                Some(job) => MissionOrigin::Uploaded {
                    plan_id: (!job.plan_id.is_empty()).then(|| job.plan_id.clone()),
                    plan_name: self
                        .state
                        .plans
                        .get(&job.plan_id)
                        .map(|p| p.body.name.clone()),
                    verified: None,
                },
                None => MissionOrigin::Uploaded {
                    plan_id: None,
                    plan_name: None,
                    verified: None,
                },
            };
            self.mission_arriving(&upload.vehicle_id, origin, upload.actor.clone());
        }
        let kind = upload.kind;
        let (status, detail, actor) = match result {
            Ok(()) => {
                match upload.kind {
                    1 => mission.fence = upload.items,
                    2 => mission.rally = upload.items,
                    _ => mission.items = upload.items,
                }
                mission.version += 1;
                mission.status = if more {
                    TransferStatus::Uploading
                } else {
                    TransferStatus::Synced
                };
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
            if plain && kind == 0 {
                self.record_mission(&upload.vehicle_id);
            }
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
        // A plan's upload goes on to be read back (S127).
        self.plan_mission_uploaded(order_id, &outcome);
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

/// A plan going up to a vehicle (S127, ADR-0029 P7): sent, then read back
/// and compared before `plan/upload` answers.
#[derive(Debug)]
pub(crate) struct PlanJob {
    pub(crate) reply: Option<(ConnectionId, Value)>,
    /// What goes up after the mission (S128): the fence, then rally points.
    pub(crate) parts: Vec<(u8, Vec<MissionItem>)>,
    /// As sent, for the read-back: the fence and rally points.
    pub(crate) fence: Vec<MissionItem>,
    pub(crate) rally: Vec<MissionItem>,
    /// Fence settings written after the fence: name, value, kind.
    pub(crate) fence_settings: Vec<(String, f64, ParamKind)>,
    pub(crate) plan_id: String,
    pub(crate) plan_version: u64,
    pub(crate) vehicle_id: String,
    /// As sent: compared with what is read back.
    pub(crate) items: Vec<MissionItem>,
    /// Item 0 is home, which the autopilot keeps as its own.
    pub(crate) home_first: bool,
}

impl Engine {
    /// Uploads a mission for a plan, answering only once it has been read
    /// back (`plan_mission_uploaded`, then `plan_mission_read`).
    pub(crate) fn upload_plan_mission(
        &mut self,
        connection: ConnectionId,
        job: PlanJob,
    ) -> Result<(), ApiError> {
        let actor = self.actor(connection);
        self.upload_plan_mission_as(actor, job)
    }

    /// As `upload_plan_mission`, recorded as `actor`: a simulated vehicle's
    /// example mission goes up as the simulation start's (P12).
    pub(crate) fn upload_plan_mission_as(
        &mut self,
        actor: Actor,
        job: PlanJob,
    ) -> Result<(), ApiError> {
        let vehicle = self.vehicle(&job.vehicle_id).cloned().ok_or_else(|| {
            ApiError::new(
                ErrorKind::NotFound,
                format!("no vehicle {}", job.vehicle_id),
            )
        })?;
        let mut mission = self.mission(&vehicle.id);
        if matches!(mission.status, TransferStatus::Uploading) {
            return Err(ApiError::new(
                ErrorKind::Busy,
                format!("an upload to {} is in progress", vehicle.name),
            ));
        }
        if let Some(why) = self.not_writable(&vehicle.link_id).or_else(|| {
            (vehicle.presence != Presence::Online).then(|| {
                format!(
                    "{} is not answering: a mission needs a conversation",
                    vehicle.name
                )
            })
        }) {
            return Err(ApiError::new(ErrorKind::InvalidValue, why));
        }
        mission.status = TransferStatus::Uploading;
        mission.detail = None;
        mission.progress = None;
        self.commit_as(daemon(), Change::MissionUpserted { mission });
        let order_id = format!("mu{}", &self.random.token()[..16]);
        self.transfers.uploads.insert(
            order_id.clone(),
            Upload {
                reply: None,
                actor,
                vehicle_id: vehicle.id.clone(),
                items: job.items.clone(),
                kind: 0,
            },
        );
        let items = job.items.clone();
        self.transfers.plan_jobs.insert(order_id.clone(), job);
        self.upload_with_terrain(order_id, vehicle.id, items);
        Ok(())
    }

    /// The upload of a plan's mission ended: accepted, it is read back
    /// before the answer; otherwise the answer goes now and the mission is
    /// read back anyway, so the tab shows what the vehicle holds.
    fn plan_mission_uploaded(&mut self, order_id: &str, result: &Result<(), Failure>) {
        let Some(mut job) = self.transfers.plan_jobs.remove(order_id) else {
            return;
        };
        let vehicle_id = job.vehicle_id.clone();
        match result {
            Ok(()) if !job.parts.is_empty() => {
                // The next part: the fence, then rally points.
                let (kind, items) = job.parts.remove(0);
                let next = format!("mu{}", &self.random.token()[..16]);
                self.transfers.uploads.insert(
                    next.clone(),
                    Upload {
                        reply: None,
                        actor: daemon(),
                        vehicle_id: vehicle_id.clone(),
                        items: items.clone(),
                        kind,
                    },
                );
                self.transfers.plan_jobs.insert(next.clone(), job);
                self.links.transfer(TransferOrder::UploadMissionPart {
                    order_id: next,
                    vehicle_id,
                    items,
                    kind,
                });
                return;
            }
            Ok(()) => {
                // Fence settings after the fence, then everything read back.
                for (name, value, kind) in std::mem::take(&mut job.fence_settings) {
                    self.write_parameter_as(daemon(), &vehicle_id, &name, value, kind);
                }
                if self
                    .vehicle(&vehicle_id)
                    .is_none_or(|vehicle| vehicle.vehicle_type != "Airship")
                {
                    self.transfers.with_parts.insert(vehicle_id.clone());
                }
                self.transfers
                    .plan_jobs
                    .insert(format!("verify:{vehicle_id}"), job);
            }
            Err(failure) => {
                let cancelled = failure.detail(0) == "cancelled";
                let status = if cancelled {
                    PlanUploadStatus::Cancelled
                } else {
                    PlanUploadStatus::Failed
                };
                let detail = (!cancelled).then(|| failure.detail(0));
                self.answer_plan_job(job, status, detail, Vec::new());
            }
        }
        self.download_mission(&vehicle_id);
    }

    /// The mission was read back: a plan just uploaded is compared with it.
    fn plan_mission_read(&mut self, vehicle_id: &str, ok: bool) {
        let Some(job) = self
            .transfers
            .plan_jobs
            .remove(&format!("verify:{vehicle_id}"))
        else {
            return;
        };
        if !ok {
            let detail = self.mission(vehicle_id).detail;
            self.answer_plan_job(
                job,
                PlanUploadStatus::Different,
                Some(format!(
                    "it was accepted, but couldn't be read back to check: {}",
                    detail.unwrap_or_else(|| "no answer".into())
                )),
                Vec::new(),
            );
            return;
        }
        let read = self.mission(vehicle_id);
        let (mut same, mut notes) = compare_missions(&job.items, &read.items, job.home_first);
        for (name, sent, back) in [
            ("fence", &job.fence, &read.fence),
            ("rally points", &job.rally, &read.rally),
        ] {
            // A fence item's frame means nothing (it has no height):
            // ArduPilot gives each back as `MAV_FRAME_GLOBAL`, whatever it
            // was sent as.
            let frameless = |items: &[MissionItem]| -> Vec<MissionItem> {
                items
                    .iter()
                    .map(|item| MissionItem {
                        frame: 0,
                        ..item.clone()
                    })
                    .collect()
            };
            let (part_same, part_notes) = if name == "fence" {
                compare_missions(&frameless(sent), &frameless(back), false)
            } else {
                compare_missions(sent, back, false)
            };
            same &= part_same;
            notes.extend(part_notes.into_iter().map(|note| format!("{name}: {note}")));
        }
        self.mission_record_verified(vehicle_id, same);
        if let Some(mut plan) = self.state.plans.get(&job.plan_id).cloned() {
            let mission_version = self.mission(vehicle_id).version;
            plan.uploads.retain(|u| u.vehicle_id != vehicle_id);
            plan.uploads.push(PlanUpload {
                vehicle_id: vehicle_id.to_owned(),
                plan_version: job.plan_version,
                at_us: self.clock.now_us(),
                verified: same,
                mission_version,
            });
            // The record of where it went is not a change to undo.
            plan.version += 1;
            self.commit_as(
                daemon(),
                Change::PlanSaved {
                    plan,
                    revision: PlanRevision::None,
                },
            );
        }
        let status = if same {
            PlanUploadStatus::Verified
        } else {
            PlanUploadStatus::Different
        };
        self.answer_plan_job(job, status, None, notes);
    }

    fn answer_plan_job(
        &mut self,
        job: PlanJob,
        status: PlanUploadStatus,
        detail: Option<String>,
        notes: Vec<String>,
    ) {
        let Some((connection, request_id)) = job.reply else {
            return;
        };
        self.track_task(
            &job.vehicle_id,
            uxduck_contracts::dto::analytics::EngagementTask::PlanVerified,
            status == PlanUploadStatus::Verified,
        );
        let mission = MissionDto::from(&self.mission(&job.vehicle_id));
        // A mission from history (S134) has no plan.
        let result = match self.state.plans.get(&job.plan_id) {
            Some(plan) => json!(PlanUploadResult {
                status,
                detail,
                notes,
                plan: plan.into(),
                mission,
            }),
            None => json!(uxduck_contracts::dto::mission::MissionRestoreResult {
                status,
                detail,
                notes,
                mission,
            }),
        };
        let frame = json!({ "jsonrpc": "2.0", "id": request_id, "result": result });
        self.send(connection, &frame);
    }
}

/// Whether what was read back is what was sent, allowing for what the
/// autopilot keeps: positions to its 1e-7°, heights to the centimetre, and
/// parameters as it stores them (noted, not refused). Item 0 is home on
/// ArduPilot and is the vehicle's own.
#[must_use]
pub fn compare_missions(
    sent: &[MissionItem],
    read: &[MissionItem],
    home_first: bool,
) -> (bool, Vec<String>) {
    let skip = usize::from(home_first);
    if sent.len() != read.len() {
        return (
            false,
            vec![format!(
                "{} items were sent and {} read back",
                sent.len().saturating_sub(skip),
                read.len().saturating_sub(skip)
            )],
        );
    }
    let frame_of = |frame: u8| match frame {
        5 => 0,
        6 => 3,
        11 => 10,
        other => other,
    };
    let close = |x: f64, y: f64, tolerance: f64| (x - y).abs() <= tolerance;
    let mut same = true;
    let mut notes = Vec::new();
    for (index, (a, b)) in sent.iter().zip(read).enumerate().skip(skip) {
        let number = index + 1 - skip;
        if a.command != b.command
            || (a.command != 20 && frame_of(a.frame) != frame_of(b.frame))
            || !close(a.latitude_deg, b.latitude_deg, 2e-7)
            || !close(a.longitude_deg, b.longitude_deg, 2e-7)
            || !close(a.altitude_m, b.altitude_m, 0.02)
        {
            same = false;
            notes.push(format!("item {number} came back different"));
            continue;
        }
        let sent_params = [a.param1, a.param2, a.param3, a.param4];
        let read_params = [b.param1, b.param2, b.param3, b.param4];
        for (p, (x, y)) in sent_params.into_iter().zip(read_params).enumerate() {
            let unchanged = (x.is_nan() && y.is_nan()) || close(x, y, 1e-4 * x.abs().max(1.0));
            if !unchanged {
                notes.push(format!(
                    "item {number} keeps parameter {} as {y} (sent {x})",
                    p + 1
                ));
            }
        }
    }
    (same, notes)
}

/// Puts a mission from a vehicle's history back (S134), as a plan's goes:
/// the mission, then on ArduPilot its fence and rally points, then read
/// back, answered with `MissionRestoreResult`.
fn restore(
    engine: &mut Engine,
    connection: ConnectionId,
    uxduck_contracts::dto::mission::MissionRestoreParams {
        record_id,
        vehicle_id,
    }: uxduck_contracts::dto::mission::MissionRestoreParams,
) -> Result<Value, ApiError> {
    let record = engine
        .state
        .mission_records
        .get(&record_id)
        .cloned()
        .ok_or_else(|| {
            ApiError::new(
                ErrorKind::NotFound,
                format!("no mission {record_id} in any vehicle's history"),
            )
        })?;
    let vehicle_id =
        engine.writable_vehicle(vehicle_id.as_deref().unwrap_or(&record.vehicle_id))?;
    let ardupilot = engine
        .vehicle(&vehicle_id)
        .is_none_or(|v| v.autopilot.to_lowercase().contains("ardupilot"));
    let mut items = record.items.clone();
    if !ardupilot {
        // Another autopilot's mission put on a PX4 one: its frames as PX4 takes them.
        for item in &mut items {
            item.frame = crate::plans::px4_frame(item.command, item.frame);
        }
    }
    if ardupilot {
        // Item 0 is home, which the autopilot keeps as its own.
        let home = engine
            .mission(&vehicle_id)
            .items
            .first()
            .cloned()
            .unwrap_or(MissionItem {
                command: 16,
                frame: 0,
                param1: 0.0,
                param2: 0.0,
                param3: 0.0,
                param4: 0.0,
                latitude_deg: 0.0,
                longitude_deg: 0.0,
                altitude_m: 0.0,
                autocontinue: true,
            });
        items.insert(0, home);
    }
    // The same three parts on PX4 (P14).
    let (fence, rally) = (record.fence.clone(), record.rally.clone());
    let blimp = engine
        .vehicle(&vehicle_id)
        .is_some_and(|vehicle| vehicle.vehicle_type == "Airship");
    if blimp && (!fence.is_empty() || !rally.is_empty()) {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "Blimp history restore does not support fence or rally transfers",
        ));
    }
    let job = PlanJob {
        reply: engine.replying_to.take(),
        parts: if blimp {
            Vec::new()
        } else {
            vec![(1, fence.clone()), (2, rally.clone())]
        },
        fence,
        rally,
        fence_settings: Vec::new(),
        plan_id: String::new(),
        plan_version: 0,
        vehicle_id,
        items,
        home_first: ardupilot,
    };
    engine.upload_plan_mission(connection, job)?;
    Ok(Value::Null)
}
