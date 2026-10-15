//! Checking a vehicle's terrain (S119, ADR-0026 T5): `TERRAIN_CHECK` asks
//! the vehicle for its own height at a few points, and each answer is
//! compared with what the vehicle's dataset gives there on the vehicle's
//! own grid, interpolated as `AP_Terrain` interpolates. The same dataset
//! agrees within a metre after rounding, so 2 m apart is a difference.
//!
//! Checks run by themselves when a vehicle's terrain has loaded (once per
//! connection) and after a mission upload, at most once a minute, at home,
//! under the vehicle and along the mission's legs. A difference raises one
//! warning per vehicle, naming the other dataset when the vehicle matches
//! it. UXDuck never changes a vehicle's dataset by itself.

use std::collections::BTreeMap;

use serde_json::Value;
use uxduck_contracts::dto::terrain::{
    TerrainCheckParams, TerrainCheckPoint, TerrainCheckResult, TerrainStatusParams,
    TerrainStatusResult, TerrainVerdict,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::notification::{
    Action, NotificationAction, NotificationKind, Severity, Subject,
};
use uxduck_core::domain::parameter::parameter_id;
use uxduck_terrain::grid;
use uxduck_terrain::lookup::{Ground, nearest_metre};
use uxduck_terrain::{Dataset, Unavailable};

use crate::commands::Raise;
use crate::rpc::handlers::to_value;
use crate::runtime::TransferOrder;
use crate::{ConnectionId, Engine};

/// Apart by more than this is a difference: the same dataset agrees
/// within a metre after rounding.
pub const THRESHOLD_M: f64 = 2.0;
/// Checks that run by themselves, at most this often per vehicle.
const AUTO_EVERY_US: u64 = 60 * 1_000_000;
/// Points along a mission's legs, at most.
const LEG_POINTS: usize = 4;

/// Checks running and the last result of each vehicle. Not journaled.
#[derive(Debug, Default)]
pub(crate) struct Checks {
    running: Vec<Check>,
    last: BTreeMap<String, TerrainCheckResult>,
    auto_at: BTreeMap<String, u64>,
    /// Vehicles whose loaded terrain was checked on this connection, with
    /// when their terrain was last reported.
    loaded: BTreeMap<String, u64>,
}

#[derive(Debug)]
struct Check {
    vehicle_id: String,
    points: Vec<Point>,
    reply: Option<(ConnectionId, Value)>,
    /// Whether a difference raises the warning (not for a point from the
    /// map menu).
    warn: bool,
    spacing: u16,
}

#[derive(Debug)]
struct Point {
    lat: i32,
    lon: i32,
    /// `None` while waiting; then the vehicle's height, if it had one, or
    /// `Some(None)` when it had none, and `answered` false for no reply.
    #[allow(clippy::option_option, reason = "waiting, no terrain, or a height")]
    vehicle: Option<Option<f32>>,
    answered: bool,
}

#[allow(clippy::cast_possible_truncation, reason = "degrees in 1e-7")]
fn e7(deg: f64) -> i32 {
    (deg * 1e7).round() as i32
}

fn wire(d: Dataset) -> uxduck_contracts::dto::terrain::TerrainDataset {
    match d {
        Dataset::Alos => uxduck_contracts::dto::terrain::TerrainDataset::Alos,
        Dataset::Copernicus => uxduck_contracts::dto::terrain::TerrainDataset::Copernicus,
    }
}

fn other(d: Dataset) -> Dataset {
    match d {
        Dataset::Alos => Dataset::Copernicus,
        Dataset::Copernicus => Dataset::Alos,
    }
}

/// What a dataset gives at a point on the vehicle's grid.
enum Expected {
    Waiting,
    None,
    Height(f32),
}

pub(crate) fn handle(
    engine: &mut Engine,
    _connection: ConnectionId,
    request: uxduck_contracts::rpc::ClientRequest,
    granted: &[uxduck_core::domain::client::Capability],
) -> Result<Value, ApiError> {
    use uxduck_contracts::rpc::ClientRequest;
    use uxduck_core::domain::client::Capability;
    if !granted.contains(&Capability::Read) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            Capability::Read.denied_message(),
        ));
    }
    match request {
        ClientRequest::TerrainCheck(params) => check(engine, &params),
        ClientRequest::TerrainStatus(TerrainStatusParams { vehicle_id }) => {
            let id = engine
                .vehicle(&vehicle_id)
                .map(|v| v.id.clone())
                .ok_or_else(|| ApiError::new(ErrorKind::NotFound, "no such vehicle"))?;
            to_value(&TerrainStatusResult {
                dataset: wire(engine.terrain_dataset_of(&id)),
                chosen: engine
                    .state
                    .settings
                    .vehicle_terrain_datasets
                    .contains_key(&id),
                last_check: engine.terrain.checks.last.get(&id).cloned(),
                vehicle_id: id,
            })
        }
        _ => Err(ApiError::new(ErrorKind::Internal, "not a terrain method")),
    }
}

fn check(engine: &mut Engine, params: &TerrainCheckParams) -> Result<Value, ApiError> {
    let id = engine
        .vehicle(&params.vehicle_id)
        .map(|v| v.id.clone())
        .ok_or_else(|| ApiError::new(ErrorKind::NotFound, "no such vehicle"))?;
    let points = match (params.latitude_deg, params.longitude_deg) {
        (Some(lat), Some(lon)) => {
            if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "latitude must be within ±90° and longitude within ±180°",
                ));
            }
            vec![(lat, lon)]
        }
        (None, None) => engine.usual_points(&id),
        _ => {
            return Err(ApiError::new(
                ErrorKind::InvalidParams,
                "give both latitudeDeg and longitudeDeg, or neither",
            ));
        }
    };
    if points.is_empty() {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "the vehicle has no position or home yet",
        ));
    }
    let warn = params.latitude_deg.is_none();
    let reply = engine.replying_to.take();
    engine.start_terrain_check(&id, &points, reply, warn);
    Ok(Value::Null)
}

impl Engine {
    /// A vehicle's dataset: chosen for it, or the default (S119).
    #[must_use]
    pub(crate) fn terrain_dataset_of(&self, vehicle_id: &str) -> Dataset {
        let chosen = self
            .state
            .settings
            .vehicle_terrain_datasets
            .get(vehicle_id)
            .copied()
            .unwrap_or(self.state.settings.terrain_dataset);
        crate::terrain::setting(chosen)
    }

    /// A person chose a vehicle's dataset: the warning about the old one
    /// goes, and a fresh check says whether the new one agrees.
    pub(crate) fn terrain_dataset_chosen(&mut self, vehicle_id: &str) {
        let id = self
            .vehicle(vehicle_id)
            .map_or_else(|| vehicle_id.to_owned(), |v| v.id.clone());
        self.resolve(&format!("terrain:check:{id}"));
        let points = self.usual_points(&id);
        if !points.is_empty() {
            self.terrain
                .checks
                .auto_at
                .insert(id.clone(), self.clock.now_us());
            self.start_terrain_check(&id, &points, None, true);
        }
        self.terrain_flush();
    }

    /// Home and the point under the vehicle, where known.
    fn usual_points(&self, vehicle_id: &str) -> Vec<(f64, f64)> {
        let mut points = Vec::new();
        if let Some(telemetry) = self.telemetry(vehicle_id) {
            let item = |key: &str| {
                telemetry
                    .items
                    .iter()
                    .find(|i| i.key == key)
                    .and_then(|i| i.value)
            };
            if let (Some(lat), Some(lon)) = (item("home.latitude_deg"), item("home.longitude_deg"))
            {
                points.push((lat, lon));
            }
            if let Some(p) = telemetry.positions.iter().find(|p| p.source == "fused") {
                points.push((p.latitude_deg, p.longitude_deg));
            }
        }
        points.dedup_by(|a, b| (a.0 - b.0).abs() < 1e-7 && (a.1 - b.1).abs() < 1e-7);
        points
    }

    /// The vehicle's `TERRAIN_SPACING`, if its parameters came; else
    /// ArduPilot's default.
    #[allow(clippy::cast_possible_truncation, reason = "a spacing in metres")]
    fn terrain_spacing(&self, vehicle_id: &str) -> u16 {
        self.state
            .parameters
            .get(&parameter_id(vehicle_id, "TERRAIN_SPACING"))
            .and_then(|p| u16::try_from(p.value.round() as i64).ok())
            .filter(|s| *s > 0)
            .unwrap_or(100)
    }

    pub(crate) fn start_terrain_check(
        &mut self,
        vehicle_id: &str,
        points: &[(f64, f64)],
        reply: Option<(ConnectionId, Value)>,
        warn: bool,
    ) {
        let points: Vec<Point> = points
            .iter()
            .map(|&(lat, lon)| Point {
                lat: e7(lat),
                lon: e7(lon),
                vehicle: None,
                answered: false,
            })
            .collect();
        for point in &points {
            self.links.transfer(TransferOrder::TerrainCheck {
                vehicle_id: vehicle_id.to_owned(),
                lat: point.lat,
                lon: point.lon,
            });
        }
        let spacing = self.terrain_spacing(vehicle_id);
        self.terrain.checks.running.push(Check {
            vehicle_id: vehicle_id.to_owned(),
            points,
            reply,
            warn,
            spacing,
        });
        self.settle_terrain_checks();
    }

    /// The vehicle answered a check at a point, or did not (S119).
    #[allow(clippy::option_option, reason = "no reply, no terrain, or a height")]
    pub fn terrain_checked(
        &mut self,
        vehicle_id: &str,
        lat: i32,
        lon: i32,
        height: Option<Option<f32>>,
    ) {
        let id = self
            .vehicle(vehicle_id)
            .map_or_else(|| vehicle_id.to_owned(), |v| v.id.clone());
        let point = self
            .terrain
            .checks
            .running
            .iter_mut()
            .filter(|c| c.vehicle_id == id)
            .flat_map(|c| c.points.iter_mut())
            .find(|p| p.vehicle.is_none() && p.lat == lat && p.lon == lon);
        if let Some(point) = point {
            point.answered = height.is_some();
            point.vehicle = Some(height.flatten());
        }
        self.settle_terrain_checks();
        self.terrain_flush();
    }

    /// Telemetry came: checks a vehicle's terrain once it has loaded.
    pub(crate) fn review_terrain(&mut self, vehicle_id: &str) {
        let Some(telemetry) = self.telemetry(vehicle_id) else {
            return;
        };
        let item = |key: &str| {
            telemetry
                .items
                .iter()
                .find(|i| i.key == key)
                .and_then(|i| i.value.map(|v| (v, i.time_us)))
        };
        let (Some((pending, at)), Some((loaded, _))) =
            (item("terrain.pending"), item("terrain.loaded"))
        else {
            return;
        };
        let id = vehicle_id.to_owned();
        // A gap in reports is a new connection or a reboot: check again.
        let seen = self.terrain.checks.loaded.get(&id).copied();
        if seen.is_some_and(|last| at.saturating_sub(last) > 10_000_000) {
            self.terrain.checks.loaded.remove(&id);
        }
        if let Some(last) = self.terrain.checks.loaded.get_mut(&id) {
            *last = at;
            return;
        }
        if pending > 0.0 || loaded <= 0.0 {
            return;
        }
        self.terrain.checks.loaded.insert(id.clone(), at);
        self.auto_terrain_check(&id, Vec::new());
    }

    /// After a mission upload: home, under the vehicle and along the legs.
    pub(crate) fn terrain_check_mission(&mut self, vehicle_id: &str) {
        let mission = self.mission(vehicle_id);
        let places: Vec<(f64, f64)> = mission
            .items
            .iter()
            .skip(1)
            .filter(|i| i.latitude_deg != 0.0 || i.longitude_deg != 0.0)
            .map(|i| (i.latitude_deg, i.longitude_deg))
            .collect();
        let mut legs: Vec<(f64, f64)> = places
            .windows(2)
            .map(|w| (f64::midpoint(w[0].0, w[1].0), f64::midpoint(w[0].1, w[1].1)))
            .collect();
        legs.truncate(LEG_POINTS);
        if legs.is_empty() {
            legs.extend(places.first());
        }
        self.auto_terrain_check(vehicle_id, legs);
    }

    fn auto_terrain_check(&mut self, vehicle_id: &str, extra: Vec<(f64, f64)>) {
        // With terrain off there is nothing to compare its answers with.
        if self.terrain.cache.is_none() {
            return;
        }
        let now = self.clock.now_us();
        if self
            .terrain
            .checks
            .auto_at
            .get(vehicle_id)
            .is_some_and(|at| now.saturating_sub(*at) < AUTO_EVERY_US)
        {
            return;
        }
        let mut points = self.usual_points(vehicle_id);
        points.extend(extra);
        if points.is_empty() {
            return;
        }
        self.terrain
            .checks
            .auto_at
            .insert(vehicle_id.to_owned(), now);
        self.start_terrain_check(vehicle_id, &points, None, true);
    }

    /// What a dataset gives at a point on a vehicle's grid: the four grid
    /// points around it, as UXDuck would send them, interpolated as
    /// `AP_Terrain` interpolates.
    fn expected(&mut self, dataset: Dataset, lat: i32, lon: i32, spacing: u16) -> Expected {
        let now = self.clock.now_us();
        let Some(cache) = &mut self.terrain.cache else {
            return Expected::None;
        };
        let info = grid::grid_info(lat, lon, spacing);
        let mut heights = [0f32; 4];
        let corners = [(0, 0), (0, 1), (1, 0), (1, 1)];
        let mut waiting = false;
        for (i, (dx, dy)) in corners.into_iter().enumerate() {
            let (a, b) = grid::offset(
                info.grid_lat,
                info.grid_lon,
                f64::from(u16::from(info.idx_x) + dx) * f64::from(spacing),
                f64::from(u16::from(info.idx_y) + dy) * f64::from(spacing),
            );
            match cache
                .at(dataset, f64::from(a) * 1e-7, f64::from(b) * 1e-7, now)
                .map(|answer| answer.ground)
            {
                None => waiting = true,
                Some(Ok(Ground::Height(h))) => heights[i] = f32::from(nearest_metre(h)),
                Some(Ok(Ground::Sea)) => heights[i] = 0.0,
                Some(Ok(Ground::NoData) | Err(Unavailable::Offline | Unavailable::Failed(_))) => {
                    return Expected::None;
                }
            }
        }
        if waiting {
            Expected::Waiting
        } else {
            Expected::Height(grid::interpolate(&info, heights))
        }
    }

    /// Finishes every check whose answers are all in and whose datasets'
    /// heights are here.
    pub(crate) fn settle_terrain_checks(&mut self) {
        let mut done = Vec::new();
        for index in 0..self.terrain.checks.running.len() {
            let check = &self.terrain.checks.running[index];
            if check.points.iter().any(|p| p.vehicle.is_none()) {
                continue;
            }
            let vehicle_id = check.vehicle_id.clone();
            let spacing = check.spacing;
            let wanted: Vec<(i32, i32, Option<f32>, bool)> = check
                .points
                .iter()
                .map(|p| (p.lat, p.lon, p.vehicle.flatten(), p.answered))
                .collect();
            let dataset = self.terrain_dataset_of(&vehicle_id);
            let mut points = Vec::new();
            let mut waiting = false;
            for (lat, lon, vehicle, answered) in wanted {
                let mut heights = [None, None];
                for (slot, d) in [dataset, other(dataset)].into_iter().enumerate() {
                    match self.expected(d, lat, lon, spacing) {
                        Expected::Waiting => waiting = true,
                        Expected::None => {}
                        Expected::Height(h) => heights[slot] = Some(f64::from(h)),
                    }
                }
                points.push(TerrainCheckPoint {
                    latitude_deg: f64::from(lat) * 1e-7,
                    longitude_deg: f64::from(lon) * 1e-7,
                    vehicle_m: vehicle.map(f64::from),
                    expected_m: heights[0],
                    other_m: heights[1],
                    answered,
                });
            }
            if !waiting {
                done.push((
                    index,
                    verdict(&vehicle_id, wire(dataset), points, self.clock.now_us()),
                ));
            }
        }
        for (index, result) in done.into_iter().rev() {
            let check = self.terrain.checks.running.remove(index);
            self.terrain
                .checks
                .last
                .insert(check.vehicle_id.clone(), result.clone());
            if let Some((connection, request_id)) = check.reply {
                let frame = match to_value(&result) {
                    Ok(value) => {
                        serde_json::json!({ "jsonrpc": "2.0", "id": request_id, "result": value })
                    }
                    Err(error) => crate::rpc::router::error(&request_id, &error),
                };
                self.send(connection, &frame);
            }
            if check.warn {
                self.warn_terrain(&check.vehicle_id, &result);
            }
        }
    }

    /// One warning per vehicle while its terrain differs; it goes when a
    /// check agrees.
    fn warn_terrain(&mut self, vehicle_id: &str, result: &TerrainCheckResult) {
        let condition = format!("terrain:check:{vehicle_id}");
        let name = self
            .state
            .vehicles
            .get(vehicle_id)
            .map_or_else(|| vehicle_id.to_owned(), |v| v.name.clone());
        let ours = crate::terrain::dataset(result.dataset);
        match result.verdict {
            TerrainVerdict::Agrees => self.resolve(&condition),
            TerrainVerdict::MatchesOther => {
                let theirs = other(ours);
                self.raise(Raise {
                    severity: Severity::Warning,
                    kind: NotificationKind::Persistent,
                    subject: Subject::Vehicle {
                        id: vehicle_id.to_owned(),
                    },
                    title: format!("{name}'s terrain matches {theirs}, not {ours}"),
                    body: Some(format!(
                        "Its heights agree with {theirs} within {THRESHOLD_M} m and differ from {ours} by {:.0} m. Use {theirs} for {name} so UXDuck answers and checks it the same way.",
                        result.difference_m.unwrap_or_default()
                    )),
                    actions: vec![NotificationAction {
                        label: format!("Use {theirs} for {name}"),
                        action: Action::UseTerrainDataset {
                            vehicle_id: vehicle_id.to_owned(),
                            dataset: match theirs {
                                Dataset::Alos => uxduck_core::domain::settings::TerrainDataset::Alos,
                                Dataset::Copernicus => {
                                    uxduck_core::domain::settings::TerrainDataset::Copernicus
                                }
                            },
                        },
                    }],
                    condition: Some(condition),
                    report: None,
                });
            }
            TerrainVerdict::Differs => {
                self.raise(Raise {
                    severity: Severity::Warning,
                    kind: NotificationKind::Persistent,
                    subject: Subject::Vehicle {
                        id: vehicle_id.to_owned(),
                    },
                    title: format!(
                        "{name}'s terrain differs from {ours} by {:.0} m",
                        result.difference_m.unwrap_or_default()
                    ),
                    body: Some(format!(
                        "It may be old or from another source. To replace it, delete the APM/TERRAIN folder in {name}'s Files tab (or on its SD card) and restart {name}: it then asks UXDuck for terrain again."
                    )),
                    actions: Vec::new(),
                    condition: Some(condition),
                    report: None,
                });
            }
            TerrainVerdict::NoTerrain | TerrainVerdict::NoReply | TerrainVerdict::Unknown => {}
        }
    }
}

/// The verdict over a check's points.
fn verdict(
    vehicle_id: &str,
    dataset: uxduck_contracts::dto::terrain::TerrainDataset,
    points: Vec<TerrainCheckPoint>,
    now_us: u64,
) -> TerrainCheckResult {
    let compared: Vec<&TerrainCheckPoint> = points
        .iter()
        .filter(|p| p.vehicle_m.is_some() && p.expected_m.is_some())
        .collect();
    let largest = |of: fn(&TerrainCheckPoint) -> Option<f64>| {
        compared
            .iter()
            .filter_map(|p| Some((p.vehicle_m? - of(p)?).abs()))
            .fold(None, |most: Option<f64>, d| {
                Some(most.map_or(d, |m| m.max(d)))
            })
    };
    let difference = largest(|p| p.expected_m);
    let verdict = if points.iter().all(|p| !p.answered) {
        TerrainVerdict::NoReply
    } else if points.iter().all(|p| p.answered && p.vehicle_m.is_none()) {
        TerrainVerdict::NoTerrain
    } else if compared.is_empty() {
        TerrainVerdict::Unknown
    } else if difference.is_some_and(|d| d <= THRESHOLD_M) {
        TerrainVerdict::Agrees
    } else if compared.iter().all(|p| p.other_m.is_some())
        && largest(|p| p.other_m).is_some_and(|d| d <= THRESHOLD_M)
    {
        TerrainVerdict::MatchesOther
    } else {
        TerrainVerdict::Differs
    };
    TerrainCheckResult {
        vehicle_id: vehicle_id.to_owned(),
        dataset,
        verdict,
        difference_m: difference.map(|d| (d * 10.0).round() / 10.0),
        points,
        checked_at_us: now_us,
    }
}
