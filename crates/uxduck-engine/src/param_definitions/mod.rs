//! Parameter definitions (ADR-0021, S67): which set describes each
//! vehicle's parameters, chosen in P3's order from the catalogue and the
//! sets people added, and fetched when first needed (P4).
//!
//! Sets are large (thousands of definitions), so they are kept by the host
//! and in memory here, never in the journal: the journal holds only which
//! set each vehicle uses ([`VehicleDefinitions`]) and what people added
//! ([`OwnSet`]). The host fetches, verifies and stores through a
//! [`DefinitionsPort`] and answers through [`Engine::definitions_arrived`].

pub mod files;
pub(crate) mod queries;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use serde_json::{Value, json};
use uxduck_contracts::dto::param_definitions::{
    self as dto, CatalogueSet, DefinitionsAddParams, DefinitionsListResult, ParamDefinitionsResult,
};
use uxduck_contracts::dto::state::{ChangeOp, EntityChange, EntityType};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::pdef;
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::param_definitions::{
    Candidate, ChoosingFor, DefinitionSet, DefinitionSource, DefinitionsStatus, Firmware, Match,
    OwnSet, VehicleDefinitions, Version, choose,
};
use uxduck_core::events::{Actor, Change};

use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine, upsert};

/// What the engine asks the host for.
#[derive(Clone, Debug, PartialEq)]
pub enum DefinitionsOrder {
    /// The catalogue's manifest, fetched and verified; again when `refresh`.
    Catalogue {
        refresh: bool,
    },
    /// One of the catalogue's sets: from disk if kept, else downloaded,
    /// checked against its SHA-256 and kept.
    Set(Box<CatalogueSet>),
    /// A set a person added, from disk.
    Own {
        id: String,
    },
    /// Keep a set a person added.
    StoreOwn {
        id: String,
        set: Box<DefinitionSet>,
    },
    RemoveOwn {
        id: String,
    },
    /// Fetch a person's set from where their CI publishes it (S74).
    FetchOwn {
        id: String,
        url: String,
    },
}

/// What the host answers.
#[derive(Clone, Debug, PartialEq)]
pub enum DefinitionsArrived {
    Catalogue(Result<Vec<CatalogueSet>, String>),
    /// A catalogue set or a person's, by id.
    Set {
        id: String,
        result: Result<DefinitionSet, String>,
    },
    /// A person's set from its URL: the file's text, or why not (S74).
    Fetched {
        id: String,
        result: Result<String, String>,
    },
}

/// Where the host's fetching goes; it answers through
/// [`Engine::definitions_arrived`].
pub trait DefinitionsPort: Send {
    fn request(&mut self, order: DefinitionsOrder);
}

#[derive(Clone, Debug, Default)]
enum Catalogue {
    #[default]
    Unknown,
    Loading,
    Ready(Vec<CatalogueSet>),
    Failed(String),
}

/// Sets in memory and what has been asked for. Not journaled.
#[derive(Default)]
pub(crate) struct Definitions {
    port: Option<Box<dyn DefinitionsPort>>,
    catalogue: Catalogue,
    loaded: HashMap<String, Arc<DefinitionSet>>,
    asked: BTreeSet<String>,
    failed: BTreeMap<String, String>,
}

impl std::fmt::Debug for Definitions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Definitions")
            .field("catalogue", &self.catalogue)
            .field("loaded", &self.loaded.keys().collect::<Vec<_>>())
            .field("asked", &self.asked)
            .finish_non_exhaustive()
    }
}

fn actor() -> Actor {
    Actor {
        client_id: "param-definitions".into(),
        operator_id: None,
    }
}

/// The catalogue's sets as candidates, by id.
fn catalogue_candidates(sets: &[CatalogueSet]) -> Vec<Candidate> {
    sets.iter()
        .map(|s| Candidate {
            id: s.id.clone(),
            firmware: s.firmware.into(),
            version: Some(s.version.clone()),
            commit: Some(s.commit.clone()),
            vehicle_id: None,
            own: false,
        })
        .collect()
}

impl Engine {
    /// Where definition sets come from; without it, only names and values.
    pub fn set_definitions(&mut self, port: Box<dyn DefinitionsPort>) {
        self.definitions.port = Some(port);
        self.choose_all_definitions();
    }

    fn ask_definitions(&mut self, order: DefinitionsOrder) -> bool {
        match self.definitions.port.as_mut() {
            Some(port) => {
                port.request(order);
                true
            }
            None => false,
        }
    }

    /// The host fetched something, or could not.
    pub fn definitions_arrived(&mut self, arrived: DefinitionsArrived) {
        match arrived {
            DefinitionsArrived::Catalogue(Ok(sets)) => {
                self.definitions.catalogue = Catalogue::Ready(sets);
            }
            DefinitionsArrived::Catalogue(Err(why)) => {
                self.definitions.catalogue = Catalogue::Failed(why);
            }
            DefinitionsArrived::Fetched { id, result } => self.own_fetched(&id, result),
            DefinitionsArrived::Set { id, result } => {
                self.definitions.asked.remove(&id);
                match result {
                    Ok(mut set) => {
                        set.sort();
                        self.definitions.failed.remove(&id);
                        self.definitions.loaded.insert(id, Arc::new(set));
                    }
                    Err(why) => {
                        self.definitions.failed.insert(id, why);
                    }
                }
            }
        }
        self.choose_all_definitions();
    }

    pub(crate) fn choose_all_definitions(&mut self) {
        let ids: Vec<String> = self.state.vehicles.keys().cloned().collect();
        for id in ids {
            self.choose_definitions(&id);
        }
    }

    /// Chooses a vehicle's set (P3), asks for it if it is not here, and
    /// journals the choice when it changed.
    pub(crate) fn choose_definitions(&mut self, vehicle_id: &str) {
        let Some(chosen) = self.definitions_for(vehicle_id) else {
            return;
        };
        if self.state.param_definitions.get(vehicle_id) != Some(&chosen) {
            self.commit_as(
                actor(),
                Change::ParamDefinitionsChosen {
                    definitions: chosen,
                },
            );
        }
    }

    fn definitions_for(&mut self, vehicle_id: &str) -> Option<VehicleDefinitions> {
        let vehicle = self.state.vehicles.get(vehicle_id)?.clone();
        let blank = |status, detail: Option<String>| VehicleDefinitions {
            vehicle_id: vehicle.id.clone(),
            status,
            set_id: None,
            title: None,
            matched: None,
            detail,
        };
        let Some(firmware) = Firmware::of(&vehicle.vehicle_type) else {
            // A ground station or a gimbal: nothing to describe.
            return None;
        };
        if !vehicle.autopilot.to_ascii_lowercase().contains("ardupilot") {
            return Some(blank(
                DefinitionsStatus::Missing,
                Some("Descriptions are for ArduPilot only for now.".into()),
            ));
        }
        if matches!(self.definitions.catalogue, Catalogue::Unknown) && vehicle.firmware.is_some() {
            if self.ask_definitions(DefinitionsOrder::Catalogue { refresh: false }) {
                self.definitions.catalogue = Catalogue::Loading;
            } else {
                self.definitions.catalogue =
                    Catalogue::Failed("this UXDuck cannot download descriptions".into());
            }
        }
        let mut candidates: Vec<Candidate> = self
            .state
            .own_sets
            .values()
            .map(OwnSet::candidate)
            .collect();
        if let Catalogue::Ready(sets) = &self.definitions.catalogue {
            candidates.extend(catalogue_candidates(sets));
        }
        let version = vehicle.firmware.as_deref().and_then(Version::parse);
        let running = vehicle.firmware.as_deref().map_or_else(
            || firmware.tag_prefix().to_owned(),
            |v| format!("{} {v}", firmware.tag_prefix()),
        );
        let Some((candidate, matched)) = choose(
            &candidates,
            &ChoosingFor {
                vehicle_id: &vehicle.id,
                firmware,
                version,
                commit: vehicle.firmware_commit.as_deref(),
            },
        ) else {
            let (status, detail) = self.none_fits(&running);
            return Some(blank(status, detail));
        };
        let candidate = candidate.clone();
        let title = self.state.own_sets.get(&candidate.id).map_or_else(
            || match &candidate.version {
                Some(v) => format!("{} {v}", firmware.tag_prefix()),
                None => firmware.tag_prefix().to_owned(),
            },
            |own| own.label.clone(),
        );
        let detail = match matched {
            Match::Approximate => Some(format!(
                "Descriptions are from {title}. This vehicle runs {}.",
                vehicle
                    .firmware
                    .as_deref()
                    .unwrap_or("a version it has not said")
            )),
            Match::Own => Some(format!("Descriptions from your set “{title}”.")),
            Match::Exact | Match::Commit => None,
        };
        let mut chosen = VehicleDefinitions {
            vehicle_id: vehicle.id.clone(),
            status: DefinitionsStatus::Ready,
            set_id: Some(candidate.id.clone()),
            title: Some(title),
            matched: Some(matched),
            detail,
        };
        self.fetch_chosen(&candidate, &mut chosen);
        Some(chosen)
    }

    /// Why no set fits, while the catalogue is where it is.
    fn none_fits(&self, running: &str) -> (DefinitionsStatus, Option<String>) {
        match &self.definitions.catalogue {
            Catalogue::Unknown => (
                DefinitionsStatus::Loading,
                Some("Waiting for the vehicle to say its version.".into()),
            ),
            Catalogue::Loading => (DefinitionsStatus::Loading, None),
            Catalogue::Failed(why) => (
                DefinitionsStatus::Missing,
                Some(format!(
                    "No descriptions: {why}. Names and values only. Add your own in Settings > Parameters."
                )),
            ),
            Catalogue::Ready(_) => (
                DefinitionsStatus::Missing,
                Some(format!(
                    "No descriptions for {running} yet. Names and values only."
                )),
            ),
        }
    }

    /// Ready if the chosen set is here; otherwise asks for it once, or says
    /// why it could not be had.
    fn fetch_chosen(&mut self, candidate: &Candidate, chosen: &mut VehicleDefinitions) {
        if self.definitions.loaded.contains_key(&candidate.id) {
            return;
        }
        if let Some(why) = self.definitions.failed.get(&candidate.id) {
            chosen.status = DefinitionsStatus::Failed;
            chosen.detail = Some(format!(
                "Descriptions could not be loaded: {why}. Names and values only."
            ));
            return;
        }
        chosen.status = DefinitionsStatus::Loading;
        if !self.definitions.asked.insert(candidate.id.clone()) {
            return;
        }
        let order = if candidate.own {
            Some(DefinitionsOrder::Own {
                id: candidate.id.clone(),
            })
        } else if let Catalogue::Ready(sets) = &self.definitions.catalogue {
            sets.iter()
                .find(|s| s.id == candidate.id)
                .map(|set| DefinitionsOrder::Set(Box::new(set.clone())))
        } else {
            None
        };
        if let Some(order) = order {
            self.ask_definitions(order);
        }
    }

    /// The set describing this vehicle, when it is here.
    #[must_use]
    pub fn vehicle_definitions(&self, vehicle_id: &str) -> Option<Arc<DefinitionSet>> {
        let chosen = self.state.param_definitions.get(vehicle_id)?;
        if chosen.status != DefinitionsStatus::Ready {
            return None;
        }
        self.definitions
            .loaded
            .get(chosen.set_id.as_deref()?)
            .cloned()
    }

    /// A parameter's units, and whether a new value needs a reboot, from the
    /// vehicle's set (S68).
    pub(crate) fn param_facts(&self, vehicle_id: &str, name: &str) -> (Option<String>, bool) {
        let helicopter = self
            .vehicle(vehicle_id)
            .is_some_and(|v| v.vehicle_type == "Helicopter");
        self.vehicle_definitions(vehicle_id)
            .and_then(|set| {
                set.find(name, helicopter)
                    .map(|d| (d.units.clone(), d.reboot_required))
            })
            .unwrap_or_default()
    }

    fn add_own_set(&mut self, params: &DefinitionsAddParams) -> Result<OwnSet, ApiError> {
        if let Some(id) = &params.vehicle_id
            && !self.state.vehicles.contains_key(id)
        {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                format!("no vehicle {id}"),
            ));
        }
        let given = Given::from(params);
        let id = format!("own-{}", &self.random.token()[..12]);
        if let Some(url) = params.url.as_deref().map(str::trim) {
            return self.add_own_url(id, url, &given);
        }
        let (set, own) = Self::own_from_text(&params.text, &given, id, None, self.clock.now_us())?;
        self.keep_own(set, own.clone())?;
        Ok(own)
    }

    /// A set from where a CI publishes it: kept as soon as it is fetched,
    /// and fetched again every day and on a refresh (S74).
    fn add_own_url(&mut self, id: String, url: &str, given: &Given) -> Result<OwnSet, ApiError> {
        let local = url.starts_with("http://127.0.0.1") || url.starts_with("http://localhost");
        if url.len() > 2048 || !(url.starts_with("https://") || local) {
            return Err(invalid("a set's URL starts with https://"));
        }
        let firmware = given
            .firmware
            .ok_or_else(|| invalid("say which firmware the set at this URL documents"))?;
        let host = url.split('/').nth(2).unwrap_or(url);
        let label = given
            .label
            .clone()
            .unwrap_or_else(|| format!("{} from {host}", firmware.tag_prefix()));
        if label.chars().count() > 64 {
            return Err(invalid("a label has at most 64 characters"));
        }
        let own = OwnSet {
            id: id.clone(),
            label,
            firmware,
            version: given.version.clone(),
            commit: given.commit.clone(),
            vehicle_id: given.vehicle_id.clone(),
            parameter_count: 0,
            added_at_us: self.clock.now_us(),
            url: Some(url.to_owned()),
            fetched_at_us: None,
        };
        if !self.ask_definitions(DefinitionsOrder::FetchOwn {
            id,
            url: url.to_owned(),
        }) {
            return Err(invalid("this UXDuck cannot fetch definition sets"));
        }
        self.commit_as(actor(), Change::OwnSetUpserted { set: own.clone() });
        Ok(own)
    }

    /// A set with a URL that is not here yet, such as one from another
    /// computer: fetched (S74).
    pub(crate) fn fetch_own_if_missing(&mut self, own: &OwnSet) {
        if let Some(url) = &own.url
            && !self.definitions.loaded.contains_key(&own.id)
        {
            self.ask_definitions(DefinitionsOrder::FetchOwn {
                id: own.id.clone(),
                url: url.clone(),
            });
        }
    }

    /// Fetches every set that has a URL again (S74): on a refresh, daily,
    /// and for one that came from another computer.
    pub fn refresh_own_sets(&mut self) {
        let fetch: Vec<(String, String)> = self
            .state
            .own_sets
            .values()
            .filter_map(|own| own.url.clone().map(|url| (own.id.clone(), url)))
            .collect();
        for (id, url) in fetch {
            self.ask_definitions(DefinitionsOrder::FetchOwn { id, url });
        }
    }

    /// A set's URL answered: read and kept like a file, or a notice why not.
    fn own_fetched(&mut self, id: &str, result: Result<String, String>) {
        let Some(record) = self.state.own_sets.get(id).cloned() else {
            return;
        };
        let condition = format!("own-set:{id}");
        let read = result.and_then(|text| {
            Self::own_from_text(
                &text,
                &Given::from(&record),
                record.id.clone(),
                record.url.clone(),
                record.added_at_us,
            )
            .map_err(|e| e.message)
        });
        match read {
            Ok((set, mut own)) => {
                own.fetched_at_us = Some(self.clock.now_us());
                let _ = self.keep_own(set, own);
                self.resolve(&condition);
            }
            Err(why) => self.raise(crate::commands::Raise {
                severity: uxduck_core::domain::notification::Severity::Warning,
                kind: uxduck_core::domain::notification::NotificationKind::Persistent,
                subject: uxduck_core::domain::notification::Subject::System,
                title: format!(
                    "Couldn't download the parameter descriptions for {}",
                    record.label
                ),
                body: Some(why),
                actions: Vec::new(),
                condition: Some(condition),
                report: Some("parameter-descriptions".into()),
            }),
        }
    }

    /// A set read from a file's text, with what was given winning over what
    /// the file says.
    fn own_from_text(
        text: &str,
        given: &Given,
        id: String,
        url: Option<String>,
        added_at_us: u64,
    ) -> Result<(DefinitionSet, OwnSet), ApiError> {
        let read = pdef::read(text).map_err(invalid)?;
        let (from_file, mut parameters, mut version, mut commit) = match read {
            pdef::Read::Pdef {
                firmware,
                parameters,
            } => (firmware, parameters, None, None),
            pdef::Read::Set(set) => (Some(set.firmware), set.parameters, set.version, set.commit),
        };
        let firmware = given
            .firmware
            .or(from_file)
            .ok_or_else(|| invalid("the file does not say which firmware it documents. Give it"))?;
        if let Some(given) = given.version.clone().filter(|v| !v.trim().is_empty()) {
            version = Some(given.trim().to_owned());
        }
        if let Some(given) = &given.commit {
            let given = given.trim().to_ascii_lowercase();
            if given.len() < 7 || !given.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(invalid("a commit is at least 7 hex characters"));
            }
            commit = Some(given);
        }
        let label = given
            .label
            .clone()
            .map(|l| l.trim().to_owned())
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| {
                let what = version
                    .clone()
                    .or_else(|| commit.as_ref().map(|c| c.chars().take(8).collect()))
                    .unwrap_or_else(|| "own".into());
                format!("{} {what}", firmware.tag_prefix())
            });
        if label.chars().count() > 64 {
            return Err(invalid("a label has at most 64 characters"));
        }
        parameters.sort_by(|a, b| a.name.cmp(&b.name));
        let set = DefinitionSet {
            source: DefinitionSource::Own {
                label: label.clone(),
            },
            firmware,
            version: version.clone(),
            commit: commit.clone(),
            parameters,
        };
        let own = OwnSet {
            id,
            label,
            firmware,
            version,
            commit,
            vehicle_id: given.vehicle_id.clone(),
            parameter_count: u32::try_from(set.parameters.len()).unwrap_or(u32::MAX),
            added_at_us,
            url,
            fetched_at_us: None,
        };
        Ok((set, own))
    }

    /// Keeps a set and its record, and chooses again.
    fn keep_own(&mut self, set: DefinitionSet, own: OwnSet) -> Result<(), ApiError> {
        if !self.ask_definitions(DefinitionsOrder::StoreOwn {
            id: own.id.clone(),
            set: Box::new(set.clone()),
        }) {
            return Err(invalid("this UXDuck cannot keep definition sets"));
        }
        self.definitions
            .loaded
            .insert(own.id.clone(), Arc::new(set));
        self.definitions.failed.remove(&own.id);
        self.commit_as(actor(), Change::OwnSetUpserted { set: own });
        self.choose_all_definitions();
        Ok(())
    }
}

/// What a person said about a set, or what its record says when fetched again.
struct Given {
    label: Option<String>,
    firmware: Option<Firmware>,
    version: Option<String>,
    commit: Option<String>,
    vehicle_id: Option<String>,
}

impl From<&DefinitionsAddParams> for Given {
    fn from(params: &DefinitionsAddParams) -> Self {
        Self {
            label: params
                .label
                .as_ref()
                .map(|l| l.trim().to_owned())
                .filter(|l| !l.is_empty()),
            firmware: params.firmware.map(Firmware::from),
            version: params.version.clone(),
            commit: params.commit.clone(),
            vehicle_id: params.vehicle_id.clone(),
        }
    }
}

impl From<&OwnSet> for Given {
    fn from(own: &OwnSet) -> Self {
        Self {
            label: Some(own.label.clone()),
            firmware: Some(own.firmware),
            version: own.version.clone(),
            commit: own.commit.clone(),
            vehicle_id: own.vehicle_id.clone(),
        }
    }
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::InvalidValue, message)
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    let _ = connection;
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
        ClientRequest::ParamDefinitions(params) => {
            require(Capability::Read)?;
            to_value(&definitions_of(engine, &params.vehicle_id)?)
        }
        ClientRequest::ParamDefinitionsList(params) => {
            require(Capability::Read)?;
            to_value(&list(engine, params.refresh))
        }
        ClientRequest::ParamDefinitionsAdd(params) => {
            require(Capability::Config)?;
            let own = engine.add_own_set(&params)?;
            to_value(&dto::OwnSet::from(&own))
        }
        ClientRequest::ParamDefinitionsRemove(params) => {
            require(Capability::Config)?;
            if !engine.state.own_sets.contains_key(&params.id) {
                return Err(ApiError::new(
                    ErrorKind::NotFound,
                    format!("no definition set {}", params.id),
                ));
            }
            engine.ask_definitions(DefinitionsOrder::RemoveOwn {
                id: params.id.clone(),
            });
            engine.definitions.loaded.remove(&params.id);
            engine.definitions.failed.remove(&params.id);
            engine.commit_as(actor(), Change::OwnSetRemoved { id: params.id });
            engine.choose_all_definitions();
            Ok(json!({}))
        }
        _ => Err(ApiError::new(
            ErrorKind::Internal,
            "not a parameter definitions method",
        )),
    }
}

/// The definitions of the parameters a vehicle reports, in its frame's
/// variant.
fn definitions_of(engine: &Engine, vehicle_id: &str) -> Result<ParamDefinitionsResult, ApiError> {
    let vehicle = engine
        .vehicle(vehicle_id)
        .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no vehicle {vehicle_id}")))?;
    let helicopter = vehicle.vehicle_type == "Helicopter";
    let definitions = engine
        .vehicle_definitions(&vehicle.id)
        .map(|set| {
            let prefix = format!("{}/", vehicle.id);
            engine
                .state
                .parameters
                .range(prefix.clone()..)
                .take_while(|(id, _)| id.starts_with(&prefix))
                .filter_map(|(_, p)| set.find(&p.name, helicopter))
                .map(Into::into)
                .collect()
        })
        .unwrap_or_default();
    Ok(ParamDefinitionsResult {
        status: engine
            .state
            .param_definitions
            .get(&vehicle.id)
            .map(Into::into),
        definitions,
    })
}

/// People's own sets, newest first, and the catalogue's state; fetching it
/// again first when `refresh`.
fn list(engine: &mut Engine, refresh: bool) -> DefinitionsListResult {
    if refresh
        && !matches!(engine.definitions.catalogue, Catalogue::Loading)
        && engine.ask_definitions(DefinitionsOrder::Catalogue { refresh: true })
    {
        engine.definitions.catalogue = Catalogue::Loading;
    }
    if refresh {
        engine.refresh_own_sets();
    }
    let (catalogue, catalogue_detail, catalogue_sets) = match (
        &engine.definitions.catalogue,
        engine.definitions.port.is_some(),
    ) {
        (_, false) => (
            dto::CatalogueState::Unavailable,
            Some("this UXDuck cannot download descriptions".to_owned()),
            0,
        ),
        (Catalogue::Unknown, true) => (dto::CatalogueState::Unknown, None, 0),
        (Catalogue::Loading, true) => (dto::CatalogueState::Loading, None, 0),
        (Catalogue::Ready(sets), true) => (
            dto::CatalogueState::Ready,
            None,
            u32::try_from(sets.len()).unwrap_or(u32::MAX),
        ),
        (Catalogue::Failed(why), true) => (dto::CatalogueState::Failed, Some(why.clone()), 0),
    };
    let mut own: Vec<&OwnSet> = engine.state.own_sets.values().collect();
    own.sort_by_key(|s| std::cmp::Reverse(s.added_at_us));
    DefinitionsListResult {
        own: own.into_iter().map(Into::into).collect(),
        catalogue,
        catalogue_detail,
        catalogue_sets,
    }
}

/// The wire form of a definitions change. A person's own sets are read with
/// `param/definitions/list`, not pushed.
pub(crate) fn entity_changes(change: &Change) -> Vec<EntityChange> {
    match change {
        Change::ParamDefinitionsChosen { definitions } => vec![EntityChange {
            param_definitions: Some(definitions.into()),
            ..upsert(EntityType::ParamDefinitions, &definitions.vehicle_id)
        }],
        _ => Vec::new(),
    }
}

/// A removed vehicle, and its definitions with it.
pub(crate) fn removed_with_vehicle(id: &str) -> Vec<EntityChange> {
    [EntityType::Vehicle, EntityType::ParamDefinitions]
        .into_iter()
        .map(|entity| EntityChange {
            op: ChangeOp::Remove,
            ..upsert(entity, id)
        })
        .collect()
}
