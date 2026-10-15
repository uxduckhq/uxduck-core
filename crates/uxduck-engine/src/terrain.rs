//! Terrain heights (S23, ADR-0026 T3, T6): `terrain/at` over the
//! `uxduck-terrain` cache. The host keeps and fetches the files through a
//! [`TerrainPort`] and answers through [`Engine::terrain_arrived`]; a
//! request whose data is on its way is answered when it arrives.
//!
//! Vehicles' `TERRAIN_REQUEST`s are answered from the same cache (S118,
//! T4): each group of 16 points on the vehicle's own grid, rounded to the
//! nearest metre, once all 16 are here. A point with no height (offline,
//! no data, a file that did not check out) is never guessed: its group is
//! not sent, and a notification names the vehicle and the place.

use serde_json::Value;
use std::collections::BTreeMap;
use uxduck_contracts::dto::terrain::{
    TerrainAtParams, TerrainAtResult, TerrainDataset, TerrainMissing,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;

use uxduck_core::domain::client::Capability;
use uxduck_core::domain::link::LinkKind;
use uxduck_core::domain::notification::{NotificationKind, Severity, Subject};
use uxduck_core::domain::settings::TerrainDataset as Setting;
use uxduck_mavlink::terrain::{Block, Group};
use uxduck_terrain::grid;
use uxduck_terrain::lookup::Ground;
use uxduck_terrain::lookup::nearest_metre;
use uxduck_terrain::{Answer, Arrived, Dataset, Order, Terrain, Unavailable};

use crate::commands::Raise;
use crate::rpc::handlers::to_value;
use crate::runtime::TransferOrder;
use crate::{ConnectionId, Engine};

/// Where the cache's orders go.
pub trait TerrainPort: Send {
    fn order(&mut self, order: Order);
}

/// The cache, its host and the requests waiting for data. Not journaled:
/// the host keeps the files.
#[derive(Default)]
pub(crate) struct TerrainState {
    pub(crate) cache: Option<Terrain>,
    port: Option<Box<dyn TerrainPort>>,
    waiting: Vec<Waiting>,
    /// Vehicles' requests still being answered: the latest mask of each
    /// block, by vehicle (S118).
    blocks: BTreeMap<(String, Block), u64>,
    /// Checks of vehicles' own terrain (S119).
    pub(crate) checks: crate::terrain_check::Checks,
    /// Areas kept offline and missions' terrain (S120).
    pub(crate) keeping: crate::terrain_keep::Keeping,
}

/// Answers a second on a network link (QGroundControl's pace) and on a
/// serial radio, which a telemetry radio's 57600 baud shares with
/// everything else.
const NETWORK_PER_S: u8 = 12;
const SERIAL_PER_S: u8 = 4;

struct Waiting {
    reply: (ConnectionId, Value),
    params: TerrainAtParams,
    dataset: Dataset,
}

impl std::fmt::Debug for TerrainState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerrainState")
            .field("cache", &self.cache.is_some())
            .field("waiting", &self.waiting.len())
            .finish_non_exhaustive()
    }
}

/// Why a host answers no terrain at all.
pub const OFF: &str = "terrain heights are off in this UXDuck";

#[must_use]
pub fn dataset(d: TerrainDataset) -> Dataset {
    match d {
        TerrainDataset::Alos => Dataset::Alos,
        TerrainDataset::Copernicus => Dataset::Copernicus,
    }
}

fn wire(d: Dataset) -> TerrainDataset {
    match d {
        Dataset::Alos => TerrainDataset::Alos,
        Dataset::Copernicus => TerrainDataset::Copernicus,
    }
}

pub(crate) fn setting(d: Setting) -> Dataset {
    match d {
        Setting::Alos => Dataset::Alos,
        Setting::Copernicus => Dataset::Copernicus,
    }
}

/// The answer as the wire has it.
fn result(params: &TerrainAtParams, answer: Answer) -> TerrainAtResult {
    let mut result = TerrainAtResult {
        latitude_deg: params.latitude_deg,
        longitude_deg: params.longitude_deg,
        dataset: wire(answer.dataset),
        release: answer.release,
        height_m: None,
        geoid: answer.dataset.geoid().into(),
        sea: false,
        missing: None,
        detail: None,
    };
    match answer.ground {
        Ok(Ground::Height(h)) => result.height_m = Some((h * 10.0).round() / 10.0),
        Ok(Ground::Sea) => {
            result.height_m = Some(0.0);
            result.sea = true;
        }
        Ok(Ground::NoData) => {
            result.missing = Some(TerrainMissing::NoData);
            result.detail = Some(format!("{} has no height here", answer.dataset));
        }
        Err(Unavailable::Offline) => {
            result.missing = Some(TerrainMissing::Offline);
            result.detail = Some("Not available offline: this place was never fetched".into());
        }
        Err(Unavailable::Failed(why)) => {
            result.missing = Some(TerrainMissing::Unavailable);
            result.detail = Some(why);
        }
    }
    result
}

pub(crate) fn handle(
    engine: &mut Engine,
    _connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    let ClientRequest::TerrainAt(params) = request else {
        return Err(ApiError::new(ErrorKind::Internal, "not a terrain method"));
    };
    if !granted.contains(&Capability::Read) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            Capability::Read.denied_message(),
        ));
    }
    if !(-90.0..=90.0).contains(&params.latitude_deg)
        || !(-180.0..=180.0).contains(&params.longitude_deg)
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "latitude must be within ±90° and longitude within ±180°",
        ));
    }
    let dataset = params.dataset.map_or_else(
        || setting(engine.state.settings.terrain_dataset),
        self::dataset,
    );
    if let Some(answer) = engine.terrain_answer(dataset, &params) {
        // Asking may have started a refresh of the manifest.
        engine.terrain_flush();
        return to_value(&result(&params, answer));
    }
    // Answered when what it needs arrives.
    if let Some(reply) = engine.replying_to.take() {
        engine.terrain.waiting.push(Waiting {
            reply,
            params,
            dataset,
        });
    }
    engine.terrain_flush();
    Ok(Value::Null)
}

impl Engine {
    /// Where terrain files are kept and fetched from, and the key the
    /// manifest must be signed with. Reads the kept manifest and fetches
    /// the bucket's at once.
    pub fn set_terrain(&mut self, port: Box<dyn TerrainPort>, public_key: Option<String>) {
        let mut cache = Terrain::new(public_key);
        cache.start(self.clock.now_us());
        self.terrain.cache = Some(cache);
        self.terrain.port = Some(port);
        self.resume_terrain_areas();
        self.terrain_flush();
    }

    /// A vehicle asked for the groups of a block it lacks (S118). Its
    /// latest mask replaces any older one for the block.
    pub fn terrain_requested(&mut self, vehicle_id: &str, block: Block, mask: u64) {
        let mask = mask & grid::ALL_GROUPS;
        let key = (vehicle_id.to_owned(), block);
        if mask == 0 {
            self.terrain.blocks.remove(&key);
        } else {
            self.terrain.blocks.insert(key.clone(), mask);
        }
        // Queued answers it no longer asks for are dropped.
        self.links.transfer(TransferOrder::TerrainData {
            vehicle_id: vehicle_id.to_owned(),
            block,
            groups: Vec::new(),
            mask,
            per_second: self.terrain_pace(vehicle_id),
        });
        self.answer_vehicles(Some(&key));
        self.terrain_flush();
    }

    /// Answers a second for a vehicle, by the link it is on.
    fn terrain_pace(&self, vehicle_id: &str) -> u8 {
        let serial = self
            .state
            .vehicles
            .get(vehicle_id)
            .and_then(|v| self.state.links.get(&v.link_id))
            .is_some_and(|l| matches!(l.kind, LinkKind::Serial { .. }));
        if serial { SERIAL_PER_S } else { NETWORK_PER_S }
    }

    /// Answers what can be answered of the vehicles' requests: one block,
    /// or every block after data arrived.
    fn answer_vehicles(&mut self, only: Option<&(String, Block)>) {
        let now = self.clock.now_us();
        let keys: Vec<(String, Block)> = match only {
            Some(key) => vec![key.clone()],
            None => self.terrain.blocks.keys().cloned().collect(),
        };
        for key in keys {
            let Some(&mask) = self.terrain.blocks.get(&key) else {
                continue;
            };
            // Each vehicle from its own dataset (S119).
            let dataset = self.terrain_dataset_of(&key.0);
            let Some(cache) = &mut self.terrain.cache else {
                return;
            };
            let (vehicle_id, block) = &key;
            let mut ready = Vec::new();
            let mut left = 0u64;
            let mut missing: Option<(String, (f64, f64))> = None;
            for gridbit in (0..56u8).filter(|bit| mask & (1 << bit) != 0) {
                let mut heights = [0i16; 16];
                let mut waiting = false;
                let mut why = None;
                for (i, (lat, lon)) in
                    grid::group_points(block.lat, block.lon, block.spacing, gridbit)
                        .into_iter()
                        .enumerate()
                {
                    let (lat, lon) = (f64::from(lat) * 1e-7, f64::from(lon) * 1e-7);
                    match cache.at(dataset, lat, lon, now).map(|a| a.ground) {
                        None => waiting = true,
                        Some(Ok(Ground::Height(h))) => heights[i] = nearest_metre(h),
                        Some(Ok(Ground::Sea)) => heights[i] = 0,
                        Some(Ok(Ground::NoData)) => {
                            why.get_or_insert((
                                format!("{dataset} has no height there"),
                                (lat, lon),
                            ));
                        }
                        Some(Err(Unavailable::Offline)) => {
                            why.get_or_insert((
                                "it is not kept on this computer and there is no connection"
                                    .to_owned(),
                                (lat, lon),
                            ));
                        }
                        Some(Err(Unavailable::Failed(detail))) => {
                            why.get_or_insert((detail, (lat, lon)));
                        }
                    }
                }
                if let Some(why) = why {
                    // Never a guessed height: the vehicle asks again.
                    missing.get_or_insert(why);
                } else if waiting {
                    left |= 1 << gridbit;
                } else {
                    ready.push(Group { gridbit, heights });
                }
            }
            if left == 0 {
                self.terrain.blocks.remove(&key);
            } else {
                self.terrain.blocks.insert(key.clone(), left);
            }
            let answered = !ready.is_empty();
            if answered {
                self.links.transfer(TransferOrder::TerrainData {
                    vehicle_id: vehicle_id.clone(),
                    block: *block,
                    groups: ready,
                    mask,
                    per_second: self.terrain_pace(vehicle_id),
                });
            }
            let condition = format!("terrain:vehicle:{vehicle_id}");
            match missing {
                Some((why, (lat, lon))) => {
                    let name = self
                        .state
                        .vehicles
                        .get(vehicle_id)
                        .map_or_else(|| vehicle_id.clone(), |v| v.name.clone());
                    self.raise(Raise {
                        severity: Severity::Warning,
                        kind: NotificationKind::Persistent,
                        subject: Subject::Vehicle {
                            id: vehicle_id.clone(),
                        },
                        title: format!("No terrain for {name} near {lat:.4}, {lon:.4}"),
                        body: Some(format!(
                            "UXDuck sent {name} no terrain there: {why}. It will not fly terrain-relative there until it has it."
                        )),
                        actions: Vec::new(),
                        condition: Some(condition),
                        report: None,
                    });
                }
                None if answered && left == 0 => self.resolve(&condition),
                None => {}
            }
        }
    }

    /// The host read or fetched what the cache asked for.
    pub fn terrain_arrived(&mut self, arrived: Arrived) {
        let now = self.clock.now_us();
        if let Some(cache) = &mut self.terrain.cache {
            cache.arrived(arrived, now);
        }
        let waiting = std::mem::take(&mut self.terrain.waiting);
        for w in waiting {
            match self.terrain_answer(w.dataset, &w.params) {
                Some(answer) => {
                    let (connection, request_id) = w.reply;
                    let frame = match to_value(&result(&w.params, answer)) {
                        Ok(value) => {
                            serde_json::json!({ "jsonrpc": "2.0", "id": request_id, "result": value })
                        }
                        Err(error) => crate::rpc::router::error(&request_id, &error),
                    };
                    self.send(connection, &frame);
                }
                None => self.terrain.waiting.push(w),
            }
        }
        self.answer_vehicles(None);
        self.settle_terrain_checks();
        self.settle_keeping();
        self.terrain_flush();
    }

    /// The ground at a point, or `None` while its data is on its way.
    fn terrain_answer(&mut self, dataset: Dataset, params: &TerrainAtParams) -> Option<Answer> {
        let now = self.clock.now_us();
        let Some(cache) = &mut self.terrain.cache else {
            return Some(Answer {
                dataset,
                release: None,
                ground: Err(Unavailable::Failed(OFF.into())),
            });
        };
        cache.at(dataset, params.latitude_deg, params.longitude_deg, now)
    }

    /// Hands the cache's orders to the host and raises its problems.
    pub(crate) fn terrain_flush(&mut self) {
        let Some(cache) = &mut self.terrain.cache else {
            return;
        };
        let orders = cache.take_orders();
        let problems = cache.take_problems();
        if let Some(port) = &mut self.terrain.port {
            for order in orders {
                port.order(order);
            }
        }
        for problem in problems {
            self.raise(Raise {
                severity: Severity::Warning,
                kind: NotificationKind::Persistent,
                subject: Subject::System,
                title: problem.title,
                body: Some(problem.body),
                actions: Vec::new(),
                condition: Some(problem.condition),
                report: Some("terrain".into()),
            });
        }
    }
}
