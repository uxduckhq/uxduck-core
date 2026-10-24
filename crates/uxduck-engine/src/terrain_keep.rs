//! Terrain kept offline, and for missions (S120, ADR-0026 T6).
//!
//! Areas are rectangles kept in the daemon's settings and fetched from the
//! default dataset into the host's store (the data folder, or the browser's
//! Origin Private File System for UXDuck Web); at start they are checked
//! again, which reads what is kept and fetches only what is missing.
//!
//! A mission's terrain is fetched when it is uploaded: every chunk within
//! 1 km of any leg, sampled along each leg, from the vehicle's dataset. A
//! mission flown above terrain waits for it before it goes, and says so.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use uxduck_contracts::dto::Empty;
use uxduck_contracts::dto::link::IdParams;
use uxduck_contracts::dto::terrain::{
    TerrainArea as AreaDto, TerrainAreaSize, TerrainAreaStatus, TerrainAreasResult, TerrainBounds,
    TerrainKeepAreaParams, TerrainKeeping,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::mission::MissionItem;
use uxduck_core::domain::settings::TerrainArea;
use uxduck_core::events::Change;
use uxduck_terrain::cover::{self, Bounds, LEG_BUFFER_M, MAX_CHUNKS};
use uxduck_terrain::{ChunkKey, Unavailable};

use crate::rpc::handlers::to_value;
use crate::runtime::TransferOrder;
use crate::{ConnectionId, Engine};

/// `MAV_FRAME_GLOBAL_TERRAIN_ALT` and its `_INT` twin.
const TERRAIN_FRAMES: [u8; 2] = [10, 11];

#[derive(Debug, Default)]
pub(crate) struct Keeping {
    /// Each area's job, by area id.
    areas: BTreeMap<String, u64>,
    /// `terrain/areaSize` requests waiting for cell indexes.
    sizes: Vec<(Reply, BTreeSet<ChunkKey>)>,
    /// Uploads waiting for their terrain.
    uploads: Vec<Upload>,
    /// Missions' terrain being fetched, by vehicle.
    missions: BTreeMap<String, u64>,
}

/// Where to answer a request later.
type Reply = Option<(ConnectionId, Value)>;

#[derive(Debug)]
struct Upload {
    order_id: String,
    vehicle_id: String,
    items: Vec<MissionItem>,
    job: u64,
}

fn bounds(b: &TerrainBounds) -> Result<Bounds, ApiError> {
    let bounds = Bounds {
        south: b.south_deg,
        west: b.west_deg,
        north: b.north_deg,
        east: b.east_deg,
    };
    if !bounds.valid() {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "an area runs south to north and west to east, within ±90° and ±180°",
        ));
    }
    if cover::chunks_in(bounds).len() > MAX_CHUNKS {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "the area is too large to keep. Keep a smaller one, a few degrees across at most",
        ));
    }
    Ok(bounds)
}

fn area_dto(a: &TerrainArea) -> AreaDto {
    AreaDto {
        id: a.id.clone(),
        name: a.name.clone(),
        south_deg: a.south_deg,
        west_deg: a.west_deg,
        north_deg: a.north_deg,
        east_deg: a.east_deg,
    }
}

fn area_bounds(a: &TerrainArea) -> Bounds {
    Bounds {
        south: a.south_deg,
        west: a.west_deg,
        north: a.north_deg,
        east: a.east_deg,
    }
}

fn mission_points(items: &[MissionItem]) -> Vec<(f64, f64)> {
    items
        .iter()
        .filter(|i| i.latitude_deg != 0.0 || i.longitude_deg != 0.0)
        .map(|i| (i.latitude_deg, i.longitude_deg))
        .collect()
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
        ClientRequest::TerrainAreas(Empty {}) => {
            require(Capability::Read)?;
            to_value(&engine.terrain_areas())
        }
        ClientRequest::TerrainAreaSize(wanted) => {
            require(Capability::Read)?;
            let keys = cover::chunks_in(bounds(&wanted)?);
            let reply = engine.replying_to.take();
            engine.terrain.keeping.sizes.push((reply, keys));
            engine.settle_keeping();
            engine.terrain_flush();
            Ok(Value::Null)
        }
        ClientRequest::TerrainKeepArea(TerrainKeepAreaParams {
            name,
            bounds: wanted,
        }) => {
            require(Capability::Config)?;
            let b = bounds(&wanted)?;
            let name = name.trim();
            if name.is_empty() {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "an area needs a name",
                ));
            }
            let area = TerrainArea {
                id: format!("ta{}", &engine.random.token()[..16]),
                name: name.to_owned(),
                south_deg: b.south,
                west_deg: b.west,
                north_deg: b.north,
                east_deg: b.east,
            };
            let mut settings = engine.state.settings.clone();
            settings.terrain_areas.push(area.clone());
            settings.version += 1;
            engine.commit(connection, Change::SettingsChanged { settings });
            engine.keep_area(&area);
            engine.terrain_flush();
            to_value(&area_dto(&area))
        }
        ClientRequest::TerrainRemoveArea(IdParams { id }) => {
            require(Capability::Config)?;
            let mut settings = engine.state.settings.clone();
            let before = settings.terrain_areas.len();
            settings.terrain_areas.retain(|a| a.id != id);
            if settings.terrain_areas.len() == before {
                return Err(ApiError::new(ErrorKind::NotFound, "no such area"));
            }
            settings.version += 1;
            engine.commit(connection, Change::SettingsChanged { settings });
            if let Some(job) = engine.terrain.keeping.areas.remove(&id)
                && let Some(cache) = &mut engine.terrain.cache
            {
                cache.forget_job(job);
            }
            to_value(&Empty {})
        }
        _ => Err(ApiError::new(ErrorKind::Internal, "not a terrain method")),
    }
}

impl Engine {
    /// Explicitly clearing browser terrain also stops keeping its offline areas.
    pub fn clear_terrain_storage(&mut self) {
        let mut settings = self.state.settings.clone();
        if !settings.terrain_areas.is_empty() {
            settings.terrain_areas.clear();
            settings.version += 1;
            self.commit_as(actor(), Change::SettingsChanged { settings });
        }
        for (reply, _) in std::mem::take(&mut self.terrain.keeping.sizes) {
            if let Some((connection, id)) = reply {
                self.send(
                    connection,
                    &crate::rpc::router::error(
                        &id,
                        &ApiError::new(
                            ErrorKind::InvalidValue,
                            "Terrain storage was cleared. Try again.",
                        ),
                    ),
                );
            }
        }
        self.terrain.keeping = Keeping::default();
        self.terrain.problems.clear();
        self.terrain.missing.clear();
        if let Some(cache) = &mut self.terrain.cache {
            cache.reset(self.clock.now_us());
        }
        self.terrain_flush();
    }

    /// Fetches what is missing of an area into the store.
    fn keep_area(&mut self, area: &TerrainArea) {
        let dataset = crate::terrain::setting(self.state.settings.terrain_dataset);
        let now = self.clock.now_us();
        let Some(cache) = &mut self.terrain.cache else {
            return;
        };
        if let Some(old) = self.terrain.keeping.areas.remove(&area.id) {
            cache.forget_job(old);
        }
        let job = cache.keep(dataset, cover::chunks_in(area_bounds(area)), now);
        self.terrain.keeping.areas.insert(area.id.clone(), job);
    }

    /// At start: every kept area checked again, reading what is kept.
    pub(crate) fn resume_terrain_areas(&mut self) {
        for area in self.state.settings.terrain_areas.clone() {
            self.keep_area(&area);
        }
    }

    fn terrain_areas(&self) -> TerrainAreasResult {
        let cache = self.terrain.cache.as_ref();
        let areas = self
            .state
            .settings
            .terrain_areas
            .iter()
            .map(|area| {
                let status = self
                    .terrain
                    .keeping
                    .areas
                    .get(&area.id)
                    .and_then(|job| cache?.job(*job));
                let (state, detail) = match &status {
                    None => (
                        TerrainKeeping::Incomplete,
                        Some(crate::terrain::OFF.to_owned()),
                    ),
                    Some(s) if !s.finished => (TerrainKeeping::Fetching, None),
                    Some(s) if s.failed == 0 => (TerrainKeeping::Kept, None),
                    Some(s) => (
                        TerrainKeeping::Incomplete,
                        Some(match &s.why {
                            Some(Unavailable::Offline) => {
                                "not all of it could be fetched: there is no connection".to_owned()
                            }
                            Some(Unavailable::Failed(why)) => why.clone(),
                            None => "not all of it could be fetched".to_owned(),
                        }),
                    ),
                };
                TerrainAreaStatus {
                    area: area_dto(area),
                    state,
                    chunks: status
                        .as_ref()
                        .map_or(0, |s| u32::try_from(s.total).unwrap_or(u32::MAX)),
                    done: status
                        .as_ref()
                        .map_or(0, |s| u32::try_from(s.done).unwrap_or(u32::MAX)),
                    bytes: status.as_ref().map_or(0, |s| s.bytes),
                    detail,
                }
            })
            .collect();
        TerrainAreasResult {
            dataset: self.state.settings.terrain_dataset.into(),
            areas,
            manifest_checked_at_us: cache.and_then(uxduck_terrain::Terrain::checked_at_us),
        }
    }

    /// Uploads a mission, fetching its terrain first: the upload waits for
    /// it when the mission is flown above terrain, and goes at once
    /// otherwise.
    pub(crate) fn upload_with_terrain(
        &mut self,
        order_id: String,
        vehicle_id: String,
        items: Vec<MissionItem>,
    ) {
        let dataset = self.terrain_dataset_of(&vehicle_id);
        let now = self.clock.now_us();
        let keys = cover::along_legs(&mission_points(&items), LEG_BUFFER_M);
        let above_terrain = items.iter().any(|i| TERRAIN_FRAMES.contains(&i.frame));
        let job = match &mut self.terrain.cache {
            Some(cache) if !keys.is_empty() => Some(cache.keep(dataset, keys, now)),
            _ => None,
        };
        let finished = job.is_some_and(|job| {
            self.terrain
                .cache
                .as_ref()
                .and_then(|c| c.job(job))
                .is_some_and(|s| s.finished)
        });
        match job {
            Some(job) if above_terrain && !finished => {
                let mut mission = self.mission(&vehicle_id);
                mission.detail = Some(
                    "Fetching the terrain along its legs first: it is flown above terrain".into(),
                );
                self.commit_as(actor(), Change::MissionUpserted { mission });
                self.terrain.keeping.uploads.push(Upload {
                    order_id,
                    vehicle_id,
                    items,
                    job,
                });
            }
            other => {
                if let Some(job) = other
                    && let Some(old) = self
                        .terrain
                        .keeping
                        .missions
                        .insert(vehicle_id.clone(), job)
                    && let Some(cache) = &mut self.terrain.cache
                {
                    cache.forget_job(old);
                }
                self.links.transfer(TransferOrder::UploadMission {
                    order_id,
                    vehicle_id,
                    items,
                });
            }
        }
        self.terrain_flush();
    }

    /// Answers sizes whose cells came, sends uploads whose terrain is in,
    /// and forgets missions' finished jobs.
    pub(crate) fn settle_keeping(&mut self) {
        let dataset = crate::terrain::setting(self.state.settings.terrain_dataset);
        let now = self.clock.now_us();
        let Some(cache) = &mut self.terrain.cache else {
            return;
        };
        let mut answered = Vec::new();
        self.terrain.keeping.sizes.retain_mut(|(reply, keys)| {
            match cache.size(dataset, keys, now) {
                None if cache.loading() || cache.manifest().is_some() => true,
                None => {
                    answered.push((
                        reply.take(),
                        Err(Unavailable::Failed(crate::terrain::OFF.into())),
                    ));
                    false
                }
                Some(result) => {
                    answered.push((reply.take(), result));
                    false
                }
            }
        });
        let mut ready = Vec::new();
        self.terrain.keeping.uploads.retain(|upload| {
            let done = cache.job(upload.job).is_none_or(|s| s.finished);
            if done {
                ready.push(upload.order_id.clone());
            }
            true
        });
        let finished: Vec<String> = self
            .terrain
            .keeping
            .missions
            .iter()
            .filter(|(_, job)| cache.job(**job).is_none_or(|s| s.finished))
            .map(|(vehicle, _)| vehicle.clone())
            .collect();
        for vehicle in finished {
            if let Some(job) = self.terrain.keeping.missions.remove(&vehicle) {
                cache.forget_job(job);
            }
        }
        for (reply, result) in answered {
            let Some((connection, request_id)) = reply else {
                continue;
            };
            let size = match result {
                Ok((chunks, bytes)) => TerrainAreaSize {
                    chunks: u32::try_from(chunks).unwrap_or(u32::MAX),
                    bytes,
                    detail: None,
                },
                Err(why) => TerrainAreaSize {
                    chunks: 0,
                    bytes: 0,
                    detail: Some(match why {
                        Unavailable::Offline => "Its size is not known offline".to_owned(),
                        Unavailable::Failed(why) => why,
                    }),
                },
            };
            let frame = serde_json::json!({ "jsonrpc": "2.0", "id": request_id, "result": size });
            self.send(connection, &frame);
        }
        for order_id in ready {
            let Some(at) = self
                .terrain
                .keeping
                .uploads
                .iter()
                .position(|u| u.order_id == order_id)
            else {
                continue;
            };
            let upload = self.terrain.keeping.uploads.remove(at);
            let status = self.terrain.cache.as_ref().and_then(|c| c.job(upload.job));
            if let Some(cache) = &mut self.terrain.cache {
                cache.forget_job(upload.job);
            }
            let mut mission = self.mission(&upload.vehicle_id);
            mission.detail = status.filter(|s| s.failed > 0).map(|s| {
                format!(
                    "Sent without some terrain along its legs: {} of {} pieces could not be fetched",
                    s.failed, s.total
                )
            });
            self.commit_as(actor(), Change::MissionUpserted { mission });
            self.links.transfer(TransferOrder::UploadMission {
                order_id: upload.order_id,
                vehicle_id: upload.vehicle_id,
                items: upload.items,
            });
        }
    }
}

pub(crate) fn actor() -> uxduck_core::events::Actor {
    uxduck_core::events::Actor {
        client_id: "terrain".into(),
        operator_id: None,
        identity: None,
    }
}
