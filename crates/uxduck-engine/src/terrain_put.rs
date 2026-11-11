//! Areas written to vehicles' cards (S312, ADR-0051 A6 to A10).
//!
//! An enabled area goes onto a vehicle as ArduPilot's own terrain files:
//! every block of the vehicle's grid within one block of it, filled from the
//! vehicle's dataset (`uxduck_terrain::card`), written whole at its offset in
//! the degree's file by MAVLink FTP `OpenFileWO` and `WriteFile`, which never
//! truncate, so every other block in the file stays. A block counts as on
//! the vehicle only once the vehicle acknowledged every byte and closed the
//! file; those are kept in the settings (`terrain_on_vehicles`).
//!
//! When (A7): never while the vehicle is armed (a write under way stops and
//! goes on once it is disarmed), never to a vehicle with "Keep areas off
//! this vehicle", by themselves to a simulator, and after the person agrees
//! to anything else (`terrain/putArea`, or the notification's button), once
//! per vehicle and set of changes, with the size and the time it takes.
//!
//! Sharing the file with the firmware: `AP_Terrain` keeps its own handle on
//! the degree file it last used and writes a block back to it only after it
//! got that block's groups from a ground station (`GRID_CACHE_DIRTY`, then
//! `write_block`). UXDuck therefore never writes a block the vehicle asked
//! for this session, and drops one from a write the moment the vehicle asks
//! for it: the two never write the same bytes. A block the firmware reads
//! while UXDuck is half way through it fails its CRC, so the firmware takes
//! it as missing and asks for it, as it would without a card. Two handles
//! growing one file on a board's FAT card are not coordinated by the file
//! system, which is why real vehicles ask first on every link until a board
//! has been tested (`REAL_VEHICLES_AUTOMATIC`).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde_json::Value;
use uxduck_contracts::dto::terrain::{
    TerrainPutAreaParams, TerrainPutAsk, TerrainPutState, TerrainVerdict, VehicleCardBlock,
    VehicleCardBlockState, VehicleTerrainArea,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::link::LinkKind;
use uxduck_core::domain::notification::{
    Action, NotificationAction, NotificationKind, NotificationState, Severity, Subject,
};
use uxduck_core::domain::parameter::parameter_id;
use uxduck_core::domain::plan::LatLon;
use uxduck_core::domain::settings::{TerrainDataset as Setting, TerrainOnVehicle};
use uxduck_core::domain::vehicle::Presence;
use uxduck_core::events::Change;
use uxduck_mavlink::terrain::Block;
use uxduck_terrain::Dataset;
use uxduck_terrain::card::{self, BLOCK_BYTES, CardBlock, Degree, Missing, Skipped};
use uxduck_terrain::grid;

use crate::commands::Raise;
use crate::rpc::handlers::to_value;
use crate::runtime::{FileOp, TransferOrder};
use crate::transfers::Failure;
use crate::vehicle_files::FileReport;
use crate::{ConnectionId, Engine};

/// Real vehicles get areas by themselves only once writing to a board's SD
/// card has been tested on hardware (S312's maintainer step, ADR-0051's
/// consequences). Until then they ask first on every link, and only
/// simulators are written to by themselves. Flip this once the test is
/// recorded in `docs/plan/review/`: radios still ask first after it.
pub(crate) const REAL_VEHICLES_AUTOMATIC: bool = false;

/// Blocks in one FTP job: a vehicle that arms stops at most this many
/// blocks into a write.
const BLOCKS_PER_JOB: usize = 8;
/// Card blocks given to the map at most.
const CARD_BLOCKS_SHOWN: usize = 2000;
/// Points checked on the vehicle after an area is written (A9).
const CHECK_POINTS: usize = 4;
/// ArduPilot reads a block from its card only once asked about it, so a
/// check right after a write may find nothing there yet: it is checked
/// again this long after.
const RECHECK_US: u64 = 5_000_000;
/// The rate is measured over this long.
const RATE_WINDOW_US: u64 = 5_000_000;
/// Rough paces for the ask's estimate: FTP shares a radio with telemetry.
const NETWORK_BPS: u64 = 50_000;
const RADIO_BPS: u64 = 2_000;

const ASK: &str = "terrain:put:";

/// Every vehicle's writes this session. Not journaled: what the vehicles
/// acknowledged is in the settings.
#[derive(Debug, Default)]
pub(crate) struct Puts {
    vehicles: BTreeMap<String, VehiclePuts>,
    /// FTP jobs under way: job id to vehicle id.
    jobs: BTreeMap<String, String>,
}

#[derive(Debug, Default)]
struct VehiclePuts {
    /// By area id.
    passes: BTreeMap<String, Pass>,
    job: Option<Job>,
    /// The set of changes the person agreed to, and the one asked about.
    agreed: Option<u64>,
    asked: Option<u64>,
    /// The terrain folder was made once this session after a write found
    /// none.
    made_folder: bool,
    /// The points checked after a write, checked again once due if the
    /// vehicle had not read them yet (A9).
    recheck: Option<(u64, Vec<(f64, f64)>)>,
}

/// One area written to, or removed from, one vehicle this session.
#[derive(Debug)]
struct Pass {
    remove: bool,
    want: Want,
    /// Still to write, in file order.
    todo: VecDeque<CardBlock>,
    /// Acknowledged this pass.
    done: BTreeSet<CardBlock>,
    /// Already on the card from an earlier, interrupted write of the same.
    kept: BTreeSet<CardBlock>,
    skipped: Vec<Skipped>,
    total: u32,
    failed: Option<String>,
    /// Asked for by `terrain/putArea`.
    explicit: bool,
    samples: VecDeque<(u64, u64)>,
    bytes_done: u64,
    /// Its next blocks wait for heights still on their way.
    waiting: bool,
}

/// What an area's blocks are written from.
#[derive(Clone, Debug, PartialEq)]
struct Want {
    points: Vec<(f64, f64)>,
    spacing: u16,
    dataset: Dataset,
    release: Option<String>,
}

#[derive(Debug)]
struct Job {
    id: String,
    area_id: String,
    blocks: Vec<CardBlock>,
    sent: u64,
}

fn setting_of(d: Dataset) -> Setting {
    match d {
        Dataset::Alos => Setting::Alos,
        Dataset::Copernicus => Setting::Copernicus,
    }
}

fn key(degree: Degree) -> String {
    degree.file_name().trim_end_matches(".DAT").to_owned()
}

fn degree_of(key: &str) -> Option<Degree> {
    let (lat_sign, rest) = key.split_at_checked(1)?;
    let (lat, rest) = rest.split_at_checked(2)?;
    let (lon_sign, lon) = rest.split_at_checked(1)?;
    let lat: i8 = lat.parse().ok()?;
    let lon: i16 = lon.parse().ok()?;
    Some(Degree {
        lat: if lat_sign == "S" { -lat } else { lat },
        lon: if lon_sign == "W" { -lon } else { lon },
    })
}

/// The blocks a record holds.
fn record_blocks(record: &TerrainOnVehicle) -> BTreeSet<CardBlock> {
    record
        .blocks
        .iter()
        .filter_map(|(key, blocks)| degree_of(key).map(|degree| (degree, blocks)))
        .flat_map(|(degree, blocks)| {
            blocks
                .iter()
                .map(move |&(x, y)| CardBlock::at(degree, record.spacing_m, x, y))
        })
        .collect()
}

fn record_points(points: &[LatLon]) -> Vec<(f64, f64)> {
    points
        .iter()
        .map(|p| (p.latitude_deg, p.longitude_deg))
        .collect()
}

fn same_block(card: &CardBlock, block: &Block) -> bool {
    // ArduPilot's `TERRAIN_LATLON_EQUAL`: within a few 1e-7 degrees.
    card.spacing == block.spacing
        && (card.lat - block.lat).abs() <= 5
        && (card.lon - block.lon).abs() <= 5
}

fn fingerprint(wants: &[(String, Want)]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (id, want) in wants {
        id.hash(&mut hasher);
        for (lat, lon) in &want.points {
            lat.to_bits().hash(&mut hasher);
            lon.to_bits().hash(&mut hasher);
        }
        want.spacing.hash(&mut hasher);
        want.dataset.id().hash(&mut hasher);
        want.release.hash(&mut hasher);
    }
    hasher.finish()
}

/// Where the block's middle is, for checking it (A9).
fn centre(block: &CardBlock) -> (f64, f64) {
    let (lat, lon) = grid::block_point(
        i16::from(block.degree.lat),
        block.degree.lon,
        u32::from(block.grid_idx_x),
        u32::from(block.grid_idx_y),
        grid::BLOCK_X / 2,
        grid::BLOCK_Y / 2,
        block.spacing,
    );
    (f64::from(lat) * 1e-7, f64::from(lon) * 1e-7)
}

fn why_skipped(skipped: &[Skipped], dataset: Dataset) -> Option<String> {
    let first = skipped.first()?;
    let why = match &first.why {
        Missing::NoData => format!("{dataset} has no height there"),
        Missing::Unavailable(uxduck_terrain::Unavailable::Offline) => {
            "it is not kept on this computer and there is no connection".to_owned()
        }
        Missing::Unavailable(uxduck_terrain::Unavailable::Failed(why)) => why.clone(),
    };
    let n = skipped.len();
    Some(format!(
        "{n} {} left off: {why}",
        if n == 1 { "block" } else { "blocks" }
    ))
}

fn size_words(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss, reason = "a size, shown rounded")]
    let mb = bytes as f64 / 1_000_000.0;
    if mb >= 1.0 {
        format!("{mb:.1} MB")
    } else {
        format!("{} kB", bytes.div_ceil(1000))
    }
}

fn time_words(seconds: u32) -> String {
    if seconds < 90 {
        format!("about {} seconds", seconds.max(1))
    } else {
        format!("about {} minutes", seconds.div_ceil(60))
    }
}

pub(crate) fn handle(
    engine: &mut Engine,
    _connection: ConnectionId,
    params: &TerrainPutAreaParams,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    if !granted.contains(&Capability::Config) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            Capability::Config.denied_message(),
        ));
    }
    let vehicle_id = engine
        .vehicle(&params.vehicle_id)
        .map(|v| v.id.clone())
        .ok_or_else(|| ApiError::new(ErrorKind::NotFound, "no such vehicle"))?;
    if engine.terrain.cache.is_none() {
        return Err(ApiError::new(ErrorKind::InvalidValue, crate::terrain::OFF));
    }
    if let Some(why) = engine
        .vehicle(&vehicle_id)
        .and_then(|v| engine.not_writable(&v.link_id))
    {
        return Err(ApiError::new(ErrorKind::InvalidValue, why));
    }
    let name = engine.vehicle_name(&vehicle_id);
    if params.remove {
        remove_from(engine, &vehicle_id, &name, params.area_id.as_ref())?;
    } else {
        put_on(engine, &vehicle_id, &name, params.area_id.as_ref())?;
    }
    engine.review_terrain_puts(&vehicle_id);
    engine.terrain_flush();
    to_value(&engine.vehicle_terrain(&vehicle_id))
}

/// "Remove from Alpha" (A10).
fn remove_from(
    engine: &mut Engine,
    vehicle_id: &str,
    name: &str,
    area_id: Option<&String>,
) -> Result<(), ApiError> {
    let Some(area_id) = area_id else {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "name the area to remove",
        ));
    };
    // A write of the same area under way stops first, keeping what
    // the vehicle acknowledged so that it is removed too.
    if engine
        .terrain
        .puts
        .vehicles
        .get(vehicle_id)
        .and_then(|p| p.job.as_ref())
        .is_some_and(|j| &j.area_id == area_id)
    {
        engine.stop_put_job(vehicle_id);
    }
    if engine
        .put_record(vehicle_id, area_id)
        .is_none_or(|r| r.removed)
    {
        return Err(ApiError::new(
            ErrorKind::NotFound,
            format!("UXDuck wrote nothing of that area to {name}"),
        ));
    }
    engine.start_removal(vehicle_id, area_id);
    Ok(())
}

/// Puts one area on the vehicle now, or agrees to every one it is asked
/// about (A7).
fn put_on(
    engine: &mut Engine,
    vehicle_id: &str,
    name: &str,
    area_id: Option<&String>,
) -> Result<(), ApiError> {
    if engine
        .state
        .settings
        .vehicles_without_terrain_areas
        .contains(vehicle_id)
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!(
                "Copying offline areas to {name} is off. Turn it on in its Terrain section first"
            ),
        ));
    }
    let wants = engine.wanted(vehicle_id);
    if let Some(area_id) = area_id {
        let Some(area) = engine
            .state
            .settings
            .terrain_areas
            .iter()
            .find(|a| &a.id == area_id)
        else {
            return Err(ApiError::new(ErrorKind::NotFound, "no such area"));
        };
        if !area.enabled {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "the area is disabled: enable it first",
            ));
        }
        if let Some((_, want)) = wants.iter().find(|(a, _)| a == area_id) {
            let record = engine.put_record(vehicle_id, area_id).cloned();
            let pass = engine
                .terrain
                .puts
                .vehicles
                .entry(vehicle_id.to_owned())
                .or_default()
                .passes
                .entry(area_id.clone())
                .or_insert_with(|| Pass::new(false, want.clone(), record.as_ref()));
            pass.explicit = true;
            pass.failed = None;
        }
    } else {
        let fp = fingerprint(&engine.needing(vehicle_id, &wants));
        let puts = engine
            .terrain
            .puts
            .vehicles
            .entry(vehicle_id.to_owned())
            .or_default();
        puts.agreed = Some(fp);
        for pass in puts.passes.values_mut() {
            pass.failed = None;
        }
    }
    Ok(())
}

impl Pass {
    fn new(remove: bool, want: Want, record: Option<&TerrainOnVehicle>) -> Self {
        Self {
            remove,
            want,
            todo: VecDeque::new(),
            done: BTreeSet::new(),
            kept: record
                .filter(|r| !r.complete)
                .map(record_blocks)
                .unwrap_or_default(),
            skipped: Vec::new(),
            total: 0,
            failed: None,
            explicit: false,
            samples: VecDeque::new(),
            bytes_done: 0,
            waiting: false,
        }
    }

    /// Planned, not yet: its blocks are worked out on its first job.
    fn planned(&self) -> bool {
        self.total > 0 || !self.todo.is_empty() || !self.done.is_empty()
    }

    fn rate(&self) -> Option<u64> {
        let (&(t0, b0), &(t1, b1)) = (self.samples.front()?, self.samples.back()?);
        let span = t1.saturating_sub(t0);
        (span > 0).then(|| b1.saturating_sub(b0) * 1_000_000 / span)
    }
}

impl Engine {
    /// The record of what UXDuck wrote of an area to a vehicle.
    fn put_record(&self, vehicle_id: &str, area_id: &str) -> Option<&TerrainOnVehicle> {
        self.state
            .settings
            .terrain_on_vehicles
            .iter()
            .find(|r| r.vehicle_id == vehicle_id && r.area_id == area_id)
    }

    /// The vehicle's `TERRAIN_SPACING`, once its parameters came.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a spacing in metres, checked"
    )]
    fn put_spacing(&self, vehicle_id: &str) -> Option<u16> {
        self.state
            .parameters
            .get(&parameter_id(vehicle_id, "TERRAIN_SPACING"))
            .map(|p| p.value)
            .filter(|s| s.is_finite() && *s >= 1.0 && *s <= f64::from(u16::MAX))
            .map(|s| s.round() as u16)
    }

    /// The release a dataset answers from now, once the manifest is known.
    fn current_release(&self, dataset: Dataset) -> Option<String> {
        self.terrain
            .cache
            .as_ref()?
            .manifest()?
            .current(dataset)
            .map(|r| r.id.clone())
    }

    /// Every enabled area as it would be written to this vehicle now.
    fn wanted(&self, vehicle_id: &str) -> Vec<(String, Want)> {
        let Some(spacing) = self.put_spacing(vehicle_id) else {
            return Vec::new();
        };
        let dataset = self.terrain_dataset_of(vehicle_id);
        let release = self.current_release(dataset);
        self.state
            .settings
            .terrain_areas
            .iter()
            .filter(|a| a.enabled)
            .map(|a| {
                (
                    a.id.clone(),
                    Want {
                        points: record_points(&a.points),
                        spacing,
                        dataset,
                        release: release.clone(),
                    },
                )
            })
            .collect()
    }

    /// Whether a record holds this area whole, as it would be written now.
    /// A release not known yet (no manifest) does not make it stale.
    fn record_holds(record: &TerrainOnVehicle, want: &Want) -> bool {
        record.complete
            && record_points(&record.points) == want.points
            && record.spacing_m == want.spacing
            && record.dataset == setting_of(want.dataset)
            && (want.release.is_none() || record.release == want.release)
    }

    /// Of the wanted areas, those not on the card as they would be written.
    fn needing(&self, vehicle_id: &str, wants: &[(String, Want)]) -> Vec<(String, Want)> {
        wants
            .iter()
            .filter(|(id, want)| match self.put_record(vehicle_id, id) {
                None => true,
                // Removed by the person: only once they put it there again.
                Some(r) if r.removed => self
                    .terrain
                    .puts
                    .vehicles
                    .get(vehicle_id)
                    .and_then(|p| p.passes.get(id))
                    .is_some_and(|p| p.explicit && !p.remove),
                Some(r) => !Self::record_holds(r, want),
            })
            .cloned()
            .collect()
    }

    /// Whether areas go to this vehicle without asking (A7).
    fn puts_automatic(&self, vehicle_id: &str) -> bool {
        let simulator = self
            .vehicle(vehicle_id)
            .and_then(|v| self.state.links.get(&v.link_id))
            .is_some_and(|l| l.kind.is_simulator());
        simulator || (REAL_VEHICLES_AUTOMATIC && !self.on_a_radio(vehicle_id))
    }

    /// Why nothing can be written to the vehicle now, if anything stops it.
    fn put_blocked(&self, vehicle_id: &str) -> Option<String> {
        let vehicle = self.vehicle(vehicle_id)?;
        let name = vehicle.name.clone();
        if self.terrain.cache.is_none() {
            return Some(crate::terrain::OFF.to_owned());
        }
        if vehicle.presence != Presence::Online {
            return Some(format!("{name} is not connected"));
        }
        if let Some(why) = self.not_writable(&vehicle.link_id) {
            return Some(why);
        }
        if self
            .state
            .settings
            .vehicles_without_terrain_areas
            .contains(vehicle_id)
        {
            return Some(format!("Copying offline areas to {name} is off"));
        }
        if self.put_spacing(vehicle_id).is_none() {
            return Some(format!("{name}'s parameters have not come yet"));
        }
        if self
            .state
            .parameters
            .get(&parameter_id(vehicle_id, "TERRAIN_ENABLE"))
            .is_some_and(|p| p.value == 0.0)
        {
            return Some(format!("{name}'s terrain is off (TERRAIN_ENABLE is 0)"));
        }
        if vehicle.armed {
            return Some(format!(
                "{name} is armed: it goes on once {name} is disarmed"
            ));
        }
        None
    }

    /// Checks a written area's points again once due, when the first check
    /// found the vehicle had not read them from its card yet.
    fn recheck_written(&mut self, vehicle_id: &str) {
        let now = self.clock.now_us();
        let Some(puts) = self.terrain.puts.vehicles.get_mut(vehicle_id) else {
            return;
        };
        if puts.recheck.as_ref().is_none_or(|(due, _)| now < *due) {
            return;
        }
        let Some((_, points)) = puts.recheck.take() else {
            return;
        };
        if self
            .terrain
            .checks
            .last
            .get(vehicle_id)
            .is_some_and(|c| c.verdict == TerrainVerdict::NoTerrain)
        {
            self.start_terrain_check(vehicle_id, &points, None, true);
        }
    }

    /// Withdraws the ask once it no longer applies.
    fn drop_ask(&mut self, vehicle_id: &str) {
        let condition = format!("{ASK}{vehicle_id}");
        if self.state.notifications.values().any(|n| {
            n.condition.as_deref() == Some(condition.as_str())
                && n.state != NotificationState::Resolved
        }) {
            self.resolve(&condition);
        }
    }

    /// After settings changed: every vehicle.
    pub(crate) fn review_all_terrain_puts(&mut self) {
        let ids: Vec<String> = self.state.vehicles.keys().cloned().collect();
        for id in ids {
            self.review_terrain_puts(&id);
        }
    }

    /// Stops a running write: what was acknowledged is kept and the rest
    /// goes back in line.
    fn stop_put_job(&mut self, vehicle_id: &str) {
        let Some(job) = self
            .terrain
            .puts
            .vehicles
            .get_mut(vehicle_id)
            .and_then(|p| p.job.take())
        else {
            return;
        };
        self.terrain.puts.jobs.remove(&job.id);
        self.links.transfer(TransferOrder::CancelFile {
            vehicle_id: vehicle_id.to_owned(),
            id: job.id.clone(),
        });
        if let Some(pass) = self
            .terrain
            .puts
            .vehicles
            .get_mut(vehicle_id)
            .and_then(|p| p.passes.get_mut(&job.area_id))
        {
            for block in job.blocks.into_iter().rev() {
                pass.todo.push_front(block);
            }
        }
        self.keep_written(vehicle_id, &job.area_id);
        self.push_vehicle_terrain(vehicle_id);
    }

    /// A pass whose area changed, was disabled or deleted stops, unless it
    /// removes what was written.
    fn drop_stale_passes(&mut self, vehicle_id: &str, needing: &[(String, Want)]) {
        let puts = self
            .terrain
            .puts
            .vehicles
            .entry(vehicle_id.to_owned())
            .or_default();
        let job_area = puts.job.as_ref().map(|j| j.area_id.clone());
        let stale: Vec<String> = puts
            .passes
            .iter()
            .filter(|(id, pass)| {
                !pass.remove
                    && pass.failed.is_none()
                    && !needing.iter().any(|(n, w)| n == *id && *w == pass.want)
            })
            .map(|(id, _)| id.clone())
            .collect();
        if job_area.as_ref().is_some_and(|a| stale.contains(a)) {
            self.stop_put_job(vehicle_id);
        }
        let Some(puts) = self.terrain.puts.vehicles.get_mut(vehicle_id) else {
            return;
        };
        for id in &stale {
            puts.passes.remove(id);
        }
    }

    /// Starts what can be written to a vehicle now, stops what may not go
    /// on, and asks first where it must (A7).
    pub(crate) fn review_terrain_puts(&mut self, vehicle_id: &str) {
        if self.vehicle(vehicle_id).is_none() || self.is_simulated_submarine(vehicle_id) {
            return;
        }
        self.recheck_written(vehicle_id);
        if let Some(why) = self.put_blocked(vehicle_id) {
            self.stop_put_job(vehicle_id);
            if !why.contains("is armed") {
                self.drop_ask(vehicle_id);
                if let Some(puts) = self.terrain.puts.vehicles.get_mut(vehicle_id) {
                    puts.asked = None;
                }
            }
            return;
        }
        let wants = self.wanted(vehicle_id);
        let needing = self.needing(vehicle_id, &wants);
        let automatic = self.puts_automatic(vehicle_id);
        let fp = fingerprint(&needing);
        self.drop_stale_passes(vehicle_id, &needing);
        let Some(puts) = self.terrain.puts.vehicles.get_mut(vehicle_id) else {
            return;
        };
        let agreed = automatic || puts.agreed == Some(fp);
        let explicit: BTreeSet<String> = puts
            .passes
            .iter()
            .filter(|(_, p)| p.explicit)
            .map(|(id, _)| id.clone())
            .collect();
        let ask = !agreed && needing.iter().any(|(id, _)| !explicit.contains(id));
        if ask {
            if puts.asked != Some(fp) {
                puts.asked = Some(fp);
                self.ask_to_put(vehicle_id, &needing);
            }
        } else {
            puts.asked = None;
            self.drop_ask(vehicle_id);
        }
        let Some(puts) = self.terrain.puts.vehicles.get(vehicle_id) else {
            return;
        };
        if puts.job.is_some() {
            return;
        }
        // Removals first, then each area in turn.
        let next = puts
            .passes
            .iter()
            .find(|(_, p)| p.remove && p.failed.is_none())
            .map(|(id, _)| id.clone())
            .or_else(|| {
                needing
                    .iter()
                    .find(|(id, _)| {
                        (agreed || explicit.contains(id))
                            && puts.passes.get(id).is_none_or(|p| p.failed.is_none())
                    })
                    .map(|(id, _)| id.clone())
            });
        let Some(area_id) = next else {
            return;
        };
        if let Some((_, want)) = needing.iter().find(|(id, _)| *id == area_id)
            && !puts.passes.contains_key(&area_id)
        {
            let record = self.put_record(vehicle_id, &area_id).cloned();
            let pass = Pass::new(false, want.clone(), record.as_ref());
            if let Some(puts) = self.terrain.puts.vehicles.get_mut(vehicle_id) {
                puts.passes.insert(area_id.clone(), pass);
            }
        }
        if self.terrain.puts.vehicles[vehicle_id]
            .passes
            .get(&area_id)
            .is_some_and(|p| p.waiting)
        {
            return;
        }
        if self.terrain.puts.vehicles[vehicle_id]
            .passes
            .get(&area_id)
            .is_some_and(|p| !p.remove && !p.planned())
        {
            self.plan_pass(vehicle_id, &area_id);
        }
        self.next_job(vehicle_id, &area_id);
    }

    /// The blocks a new pass writes: the area's, less those the vehicle
    /// asked for this session (it writes those itself) and those an
    /// interrupted write of the same already put there.
    fn plan_pass(&mut self, vehicle_id: &str, area_id: &str) {
        let asked = self.terrain_asked_blocks(vehicle_id);
        let Some(pass) = self
            .terrain
            .puts
            .vehicles
            .get_mut(vehicle_id)
            .and_then(|p| p.passes.get_mut(area_id))
        else {
            return;
        };
        let blocks: Vec<CardBlock> = card::blocks_covering(&pass.want.points, pass.want.spacing)
            .into_values()
            .flatten()
            .filter(|b| !asked.iter().any(|a| same_block(b, a)))
            .collect();
        pass.total = u32::try_from(blocks.len()).unwrap_or(u32::MAX);
        pass.todo = blocks
            .into_iter()
            .filter(|b| !pass.kept.contains(b))
            .collect();
    }

    /// Fills the next blocks of one file and writes them, or finishes the
    /// pass once nothing is left.
    fn next_job(&mut self, vehicle_id: &str, area_id: &str) {
        let now = self.clock.now_us();
        let dataset = self.terrain_dataset_of(vehicle_id);
        loop {
            let Some(pass) = self
                .terrain
                .puts
                .vehicles
                .get_mut(vehicle_id)
                .and_then(|p| p.passes.get_mut(area_id))
            else {
                return;
            };
            let Some(first) = pass.todo.front().copied() else {
                self.finish_pass(vehicle_id, area_id);
                return;
            };
            let batch: Vec<CardBlock> = pass
                .todo
                .iter()
                .take_while(|b| b.degree == first.degree)
                .take(BLOCKS_PER_JOB)
                .copied()
                .collect();
            let ready: Vec<(CardBlock, Vec<u8>)> = if pass.remove {
                batch.iter().map(|b| (*b, vec![0u8; BLOCK_BYTES])).collect()
            } else {
                let Some(cache) = self.terrain.cache.as_mut() else {
                    return;
                };
                let filled = card::fill(batch.iter().copied(), |lat, lon| {
                    cache.at(dataset, lat, lon, now).map(|a| a.ground)
                });
                let Some(pass) = self
                    .terrain
                    .puts
                    .vehicles
                    .get_mut(vehicle_id)
                    .and_then(|p| p.passes.get_mut(area_id))
                else {
                    return;
                };
                for skipped in filled.skipped {
                    pass.todo.retain(|b| *b != skipped.block);
                    pass.skipped.push(skipped);
                }
                if filled.blocks.is_empty() {
                    if filled.waiting.is_empty() {
                        // All of these were skipped: on to the next ones.
                        continue;
                    }
                    // Their heights are on their way: `terrain_arrived` again.
                    pass.waiting = true;
                    self.terrain_flush();
                    self.push_vehicle_terrain(vehicle_id);
                    return;
                }
                filled.blocks
            };
            self.start_put_job(vehicle_id, area_id, first.degree, ready);
            return;
        }
    }

    fn start_put_job(
        &mut self,
        vehicle_id: &str,
        area_id: &str,
        degree: Degree,
        ready: Vec<(CardBlock, Vec<u8>)>,
    ) {
        let now = self.clock.now_us();
        let simulator = self
            .vehicle(vehicle_id)
            .and_then(|v| self.state.links.get(&v.link_id))
            .is_some_and(|l| l.kind.is_simulator());
        let folder = crate::terrain_vehicle::terrain_folder(simulator);
        let path = format!("{folder}/{}", degree.file_name());
        let blocks: Vec<CardBlock> = ready.iter().map(|(b, _)| *b).collect();
        let pieces: Vec<(u64, Vec<u8>)> = ready
            .into_iter()
            .map(|(b, bytes)| (b.offset(), bytes))
            .collect();
        let id = format!("tw{}", &self.random.token()[..16]);
        let Some(puts) = self.terrain.puts.vehicles.get_mut(vehicle_id) else {
            return;
        };
        if let Some(pass) = puts.passes.get_mut(area_id) {
            pass.todo.retain(|b| !blocks.contains(b));
            if pass.samples.is_empty() {
                pass.samples.push_back((now, pass.bytes_done));
            }
        }
        puts.job = Some(Job {
            id: id.clone(),
            area_id: area_id.to_owned(),
            blocks,
            sent: 0,
        });
        self.terrain
            .puts
            .jobs
            .insert(id.clone(), vehicle_id.to_owned());
        self.links.transfer(TransferOrder::File {
            vehicle_id: vehicle_id.to_owned(),
            id,
            op: FileOp::Patch { path, pieces },
        });
        self.push_vehicle_terrain(vehicle_id);
    }

    /// A pass wrote or removed everything it had: kept in the settings,
    /// and a written area checked on the vehicle (A9).
    fn finish_pass(&mut self, vehicle_id: &str, area_id: &str) {
        let Some(pass) = self
            .terrain
            .puts
            .vehicles
            .get_mut(vehicle_id)
            .and_then(|p| p.passes.remove(area_id))
        else {
            return;
        };
        let mut settings = self.state.settings.clone();
        settings
            .terrain_on_vehicles
            .retain(|r| !(r.vehicle_id == vehicle_id && r.area_id == area_id));
        let mut check = Vec::new();
        if pass.remove {
            let mut record = self.written_record(vehicle_id, area_id, &pass, false);
            record.blocks.clear();
            record.skipped = 0;
            record.removed = true;
            settings.terrain_on_vehicles.push(record);
        } else {
            settings
                .terrain_on_vehicles
                .push(self.written_record(vehicle_id, area_id, &pass, true));
            // A few blocks spread across what was written.
            let done: Vec<&CardBlock> = pass.done.iter().collect();
            if !done.is_empty() {
                let mut picked: Vec<usize> = (0..CHECK_POINTS)
                    .map(|i| i * (done.len() - 1) / (CHECK_POINTS - 1))
                    .collect();
                picked.dedup();
                check = picked.into_iter().map(|i| centre(done[i])).collect();
            }
        }
        settings.version += 1;
        self.commit_as(
            crate::terrain_keep::actor(),
            Change::SettingsChanged { settings },
        );
        if !check.is_empty() {
            self.start_terrain_check(vehicle_id, &check, None, true);
            let due = self.clock.now_us() + RECHECK_US;
            if let Some(puts) = self.terrain.puts.vehicles.get_mut(vehicle_id) {
                puts.recheck = Some((due, check));
            }
        }
        self.push_vehicle_terrain(vehicle_id);
        self.review_terrain_puts(vehicle_id);
    }

    /// The record of what a pass wrote, with what an earlier interrupted
    /// write of the same put there.
    fn written_record(
        &self,
        vehicle_id: &str,
        area_id: &str,
        pass: &Pass,
        complete: bool,
    ) -> TerrainOnVehicle {
        let mut blocks: BTreeMap<String, Vec<(u16, u16)>> = BTreeMap::new();
        for block in pass.kept.iter().chain(&pass.done) {
            let list = blocks.entry(key(block.degree)).or_default();
            if !list.contains(&(block.grid_idx_x, block.grid_idx_y)) {
                list.push((block.grid_idx_x, block.grid_idx_y));
            }
        }
        TerrainOnVehicle {
            vehicle_id: vehicle_id.to_owned(),
            area_id: area_id.to_owned(),
            points: pass
                .want
                .points
                .iter()
                .map(|&(latitude_deg, longitude_deg)| LatLon {
                    latitude_deg,
                    longitude_deg,
                })
                .collect(),
            spacing_m: pass.want.spacing,
            dataset: setting_of(pass.want.dataset),
            release: pass.want.release.clone(),
            blocks,
            skipped: u32::try_from(pass.skipped.len()).unwrap_or(u32::MAX),
            complete,
            written_at_us: self.clock.now_us(),
            removed: false,
        }
    }

    /// A pass stopped part way: what the vehicle acknowledged is kept, so
    /// it is shown, can be removed, and is not written again.
    fn keep_written(&mut self, vehicle_id: &str, area_id: &str) {
        let Some(pass) = self
            .terrain
            .puts
            .vehicles
            .get(vehicle_id)
            .and_then(|p| p.passes.get(area_id))
        else {
            return;
        };
        if pass.done.is_empty() {
            return;
        }
        let mut settings = self.state.settings.clone();
        if pass.remove {
            if let Some(record) = settings
                .terrain_on_vehicles
                .iter_mut()
                .find(|r| r.vehicle_id == vehicle_id && r.area_id == area_id)
            {
                for block in &pass.done {
                    if let Some(list) = record.blocks.get_mut(&key(block.degree)) {
                        list.retain(|b| *b != (block.grid_idx_x, block.grid_idx_y));
                    }
                }
                record.blocks.retain(|_, list| !list.is_empty());
                record.complete = false;
                record.removed = true;
            }
        } else {
            let record = self.written_record(vehicle_id, area_id, pass, false);
            settings
                .terrain_on_vehicles
                .retain(|r| !(r.vehicle_id == vehicle_id && r.area_id == area_id));
            settings.terrain_on_vehicles.push(record);
        }
        settings.version += 1;
        self.commit_as(
            crate::terrain_keep::actor(),
            Change::SettingsChanged { settings },
        );
        if let Some(pass) = self
            .terrain
            .puts
            .vehicles
            .get_mut(vehicle_id)
            .and_then(|p| p.passes.get_mut(area_id))
        {
            let done = std::mem::take(&mut pass.done);
            if !pass.remove {
                pass.kept.extend(done);
            }
        }
    }

    /// "Remove from Alpha" (A10): empty blocks over UXDuck's own, less
    /// those another of its areas on the vehicle still holds. A block with
    /// no groups is one ArduPilot reads as missing (`read_block`).
    fn start_removal(&mut self, vehicle_id: &str, area_id: &str) {
        let Some(record) = self.put_record(vehicle_id, area_id).cloned() else {
            return;
        };
        let others: BTreeSet<CardBlock> = self
            .state
            .settings
            .terrain_on_vehicles
            .iter()
            .filter(|r| r.vehicle_id == vehicle_id && r.area_id != record.area_id)
            .flat_map(record_blocks)
            .collect();
        let todo: VecDeque<CardBlock> = record_blocks(&record)
            .into_iter()
            .filter(|b| !others.contains(b))
            .collect();
        let want = Want {
            points: record_points(&record.points),
            spacing: record.spacing_m,
            dataset: crate::terrain::setting(record.dataset),
            release: record.release.clone(),
        };
        let mut pass = Pass::new(true, want, None);
        pass.total = u32::try_from(todo.len()).unwrap_or(u32::MAX);
        pass.todo = todo;
        pass.explicit = true;
        self.terrain
            .puts
            .vehicles
            .entry(vehicle_id.to_owned())
            .or_default()
            .passes
            .insert(record.area_id.clone(), pass);
    }

    /// The vehicle asked for a block (S118): it is answered live and the
    /// firmware writes it to its card itself, so UXDuck no longer writes it.
    pub(crate) fn terrain_put_asked(&mut self, vehicle_id: &str, block: Block) {
        if let Some(puts) = self.terrain.puts.vehicles.get_mut(vehicle_id) {
            for pass in puts.passes.values_mut().filter(|p| !p.remove) {
                pass.todo.retain(|b| !same_block(b, &block));
            }
        }
    }

    /// The vehicle was deleted, as a simulated one is when its simulation
    /// ends: its writes and what UXDuck knew of its card go with it.
    pub(crate) fn terrain_puts_gone(&mut self, vehicle_id: &str) {
        if self
            .state
            .settings
            .terrain_on_vehicles
            .iter()
            .any(|r| r.vehicle_id == vehicle_id)
        {
            let mut settings = self.state.settings.clone();
            settings
                .terrain_on_vehicles
                .retain(|r| r.vehicle_id != vehicle_id);
            settings.vehicles_without_terrain_areas.remove(vehicle_id);
            settings.version += 1;
            self.commit_as(
                crate::terrain_keep::actor(),
                Change::SettingsChanged { settings },
            );
        }
        self.terrain.puts.vehicles.remove(vehicle_id);
        self.terrain.puts.jobs.retain(|_, v| v != vehicle_id);
        self.drop_ask(vehicle_id);
    }

    /// Heights came: writes that waited for them go on.
    pub(crate) fn resume_terrain_puts(&mut self) {
        for puts in self.terrain.puts.vehicles.values_mut() {
            for pass in puts.passes.values_mut() {
                pass.waiting = false;
            }
        }
        let waiting: Vec<String> = self
            .terrain
            .puts
            .vehicles
            .iter()
            .filter(|(_, p)| p.job.is_none() && !p.passes.is_empty())
            .map(|(id, _)| id.clone())
            .collect();
        for id in waiting {
            self.review_terrain_puts(&id);
        }
    }

    /// An FTP job of a write reported; `false` when it is not one.
    pub(crate) fn terrain_put_reported(&mut self, report: &FileReport) -> bool {
        let id = match report {
            FileReport::Listed { id, .. }
            | FileReport::Opened { id, .. }
            | FileReport::Data { id, .. }
            | FileReport::Sent { id, .. }
            | FileReport::Done { id, .. } => id.clone(),
        };
        let Some(vehicle_id) = self.terrain.puts.jobs.get(&id).cloned() else {
            return false;
        };
        let now = self.clock.now_us();
        if id.starts_with("tm") {
            // The terrain folder was made, or was there: write again.
            if matches!(report, FileReport::Done { .. }) {
                self.terrain.puts.jobs.remove(&id);
                self.review_terrain_puts(&vehicle_id);
            }
            return true;
        }
        match report {
            FileReport::Sent { done, .. } => {
                if let Some(puts) = self.terrain.puts.vehicles.get_mut(&vehicle_id)
                    && let Some(job) = puts.job.as_mut()
                {
                    let more = done.saturating_sub(job.sent);
                    job.sent = *done;
                    if let Some(pass) = puts.passes.get_mut(&job.area_id) {
                        pass.bytes_done += more;
                        pass.samples.push_back((now, pass.bytes_done));
                        while pass.samples.len() > 2
                            && pass
                                .samples
                                .front()
                                .is_some_and(|(t, _)| now.saturating_sub(*t) > RATE_WINDOW_US)
                        {
                            pass.samples.pop_front();
                        }
                    }
                }
                self.push_vehicle_terrain(&vehicle_id);
            }
            FileReport::Done { result, .. } => {
                self.terrain.puts.jobs.remove(&id);
                let Some(job) = self
                    .terrain
                    .puts
                    .vehicles
                    .get_mut(&vehicle_id)
                    .and_then(|p| p.job.take_if(|j| j.id == id))
                else {
                    return true;
                };
                self.put_job_done(&vehicle_id, job, result.clone());
            }
            _ => {}
        }
        true
    }

    fn put_job_done(&mut self, vehicle_id: &str, job: Job, result: Result<(), Failure>) {
        let missing_folder = matches!(
            &result,
            Err(Failure::Rejected(why)) if why == "there is no such file or folder"
        );
        let restarted = matches!(
            &result,
            Err(Failure::Rejected(why)) if why == "the vehicle restarted"
        );
        let simulator = self
            .vehicle(vehicle_id)
            .and_then(|v| self.state.links.get(&v.link_id))
            .is_some_and(|l| l.kind.is_simulator());
        let Some(puts) = self.terrain.puts.vehicles.get_mut(vehicle_id) else {
            return;
        };
        let make_folder = missing_folder && !puts.made_folder;
        if make_folder {
            puts.made_folder = true;
        }
        let Some(pass) = puts.passes.get_mut(&job.area_id) else {
            return;
        };
        match result {
            Ok(()) => pass.done.extend(job.blocks),
            Err(failure) => {
                for block in job.blocks.into_iter().rev() {
                    pass.todo.push_front(block);
                }
                if make_folder {
                    let id = format!("tm{}", &self.random.token()[..16]);
                    self.terrain
                        .puts
                        .jobs
                        .insert(id.clone(), vehicle_id.to_owned());
                    self.links.transfer(TransferOrder::File {
                        vehicle_id: vehicle_id.to_owned(),
                        id,
                        op: FileOp::MakeDir {
                            path: crate::terrain_vehicle::terrain_folder(simulator).to_owned(),
                        },
                    });
                    return;
                }
                if !restarted {
                    pass.failed = Some(failure.detail(0));
                    self.keep_written(vehicle_id, &job.area_id);
                }
            }
        }
        self.push_vehicle_terrain(vehicle_id);
        self.review_terrain_puts(vehicle_id);
    }

    /// Asks the person before putting areas on a vehicle that asks first
    /// (A7), with the size and the time at the link's pace.
    fn ask_to_put(&mut self, vehicle_id: &str, needing: &[(String, Want)]) {
        let Some(ask) = self.put_ask(vehicle_id, needing) else {
            return;
        };
        let name = self.vehicle_name(vehicle_id);
        let areas = if ask.areas == 1 {
            "1 area".to_owned()
        } else {
            format!("{} areas", ask.areas)
        };
        let over = if ask.radio { " over its radio" } else { "" };
        let why = if ask.radio {
            format!("{name} is on a telemetry radio, so UXDuck asks before writing to its card.")
        } else {
            "UXDuck writes terrain to a real vehicle's card only once you agree, until writing to a board's card has been tested.".to_owned()
        };
        self.raise_or_update(Raise {
            severity: Severity::Info,
            kind: NotificationKind::Persistent,
            subject: Subject::Vehicle {
                id: vehicle_id.to_owned(),
            },
            title: format!(
                "Put {areas} on {name}? {}, {}{over}",
                size_words(ask.bytes),
                time_words(ask.seconds)
            ),
            body: Some(format!(
                "{why} {name} then reads its terrain there from its card instead of asking for it, so a mission there is ready almost at once. Nothing is written while it is armed."
            )),
            actions: vec![NotificationAction {
                label: format!("Put them on {name}"),
                action: Action::PutTerrainAreas {
                    vehicle_id: vehicle_id.to_owned(),
                },
            }],
            condition: Some(format!("{ASK}{vehicle_id}")),
            report: None,
        });
    }

    /// What putting the areas still needed would take.
    fn put_ask(&self, vehicle_id: &str, needing: &[(String, Want)]) -> Option<TerrainPutAsk> {
        if needing.is_empty() {
            return None;
        }
        let blocks: usize = needing
            .iter()
            .map(|(_, want)| {
                card::blocks_covering(&want.points, want.spacing)
                    .values()
                    .map(Vec::len)
                    .sum::<usize>()
            })
            .sum();
        let bytes = blocks as u64 * BLOCK_BYTES as u64;
        let radio = self.on_a_radio(vehicle_id);
        let link = self
            .vehicle(vehicle_id)
            .and_then(|v| self.state.links.get(&v.link_id));
        let pace = match link.map(|l| &l.kind) {
            // Half of what the baud carries: telemetry shares it.
            Some(LinkKind::Serial { baud, .. }) => (u64::from(*baud) / 20).max(500),
            _ if radio => RADIO_BPS,
            _ => NETWORK_BPS,
        };
        Some(TerrainPutAsk {
            areas: u32::try_from(needing.len()).unwrap_or(u32::MAX),
            bytes,
            seconds: u32::try_from(bytes.div_ceil(pace)).unwrap_or(u32::MAX),
            radio,
        })
    }

    /// How each area stands on a vehicle, its card blocks for the map, and
    /// what the person is asked, for `terrain/vehicle`.
    #[allow(clippy::too_many_lines, reason = "each state in turn")]
    pub(crate) fn terrain_put_view(&self, vehicle_id: &str) -> PutView {
        let name = self.vehicle_name(vehicle_id);
        let blocked = self.put_blocked(vehicle_id);
        let wants = self.wanted(vehicle_id);
        let needing = self.needing(vehicle_id, &wants);
        let automatic = self.puts_automatic(vehicle_id);
        let puts = self.terrain.puts.vehicles.get(vehicle_id);
        let agreed = automatic || puts.is_some_and(|p| p.agreed == Some(fingerprint(&needing)));
        let dataset = self.terrain_dataset_of(vehicle_id);
        let current = self.current_release(dataset);
        let mut view = PutView {
            automatic,
            ..PutView::default()
        };
        let mut ids: Vec<String> = wants.iter().map(|(id, _)| id.clone()).collect();
        for record in self
            .state
            .settings
            .terrain_on_vehicles
            .iter()
            .filter(|r| r.vehicle_id == vehicle_id)
        {
            if !ids.contains(&record.area_id) {
                ids.push(record.area_id.clone());
            }
        }
        for id in puts.iter().flat_map(|p| p.passes.keys()) {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        for id in ids {
            let record = self.put_record(vehicle_id, &id);
            let pass = puts.and_then(|p| p.passes.get(&id));
            let want = wants.iter().find(|(a, _)| *a == id).map(|(_, w)| w);
            let job = puts
                .and_then(|p| p.job.as_ref())
                .filter(|j| j.area_id == id);
            let mut area = VehicleTerrainArea {
                area_id: id.clone(),
                name: self
                    .state
                    .settings
                    .terrain_areas
                    .iter()
                    .find(|a| a.id == id)
                    .map(|a| a.name.clone()),
                vehicle_id: vehicle_id.to_owned(),
                vehicle_name: name.clone(),
                state: TerrainPutState::Waiting,
                blocks: 0,
                written: 0,
                skipped: 0,
                bytes: 0,
                rate_bps: None,
                written_at_us: record.filter(|r| r.complete).map(|r| r.written_at_us),
                detail: None,
            };
            if let Some(pass) = pass {
                let written: BTreeSet<&CardBlock> = pass.kept.iter().chain(&pass.done).collect();
                area.blocks = pass.total;
                area.written = u32::try_from(if pass.remove {
                    pass.done.len()
                } else {
                    written.len()
                })
                .unwrap_or(u32::MAX);
                area.skipped = u32::try_from(pass.skipped.len()).unwrap_or(u32::MAX);
                area.bytes = u64::from(pass.total) * BLOCK_BYTES as u64;
                area.rate_bps = job.and_then(|_| pass.rate());
                area.state = if pass.remove {
                    TerrainPutState::Removing
                } else if pass.failed.is_some() {
                    TerrainPutState::Failed
                } else if job.is_some() {
                    TerrainPutState::Writing
                } else {
                    TerrainPutState::Waiting
                };
                area.detail = pass
                    .failed
                    .clone()
                    .or_else(|| blocked.clone())
                    .or_else(|| why_skipped(&pass.skipped, pass.want.dataset))
                    .or_else(|| {
                        (job.is_none() && pass.planned() && !pass.todo.is_empty())
                            .then(|| "Getting its heights ready".to_owned())
                    });
                let writing: BTreeSet<&CardBlock> =
                    job.map(|j| j.blocks.iter().collect()).unwrap_or_default();
                if pass.remove {
                    for block in &pass.todo {
                        view.card(block, VehicleCardBlockState::Written, &id);
                    }
                } else {
                    for block in &written {
                        view.card(block, VehicleCardBlockState::Written, &id);
                    }
                    for block in &writing {
                        view.card(block, VehicleCardBlockState::Writing, &id);
                    }
                    for block in &pass.todo {
                        view.card(block, VehicleCardBlockState::Waiting, &id);
                    }
                    for skipped in &pass.skipped {
                        view.card(&skipped.block, VehicleCardBlockState::Skipped, &id);
                    }
                }
            } else if record.is_some_and(|r| r.removed) {
                area.detail = Some(format!(
                    "Removed from {name}: it goes there again only once you put it there"
                ));
            } else if let Some(record) = record {
                let on_card = record_blocks(record);
                area.written = u32::try_from(on_card.len()).unwrap_or(u32::MAX);
                area.skipped = record.skipped;
                area.blocks = area.written + record.skipped;
                let stale = record.dataset != setting_of(dataset)
                    || current
                        .as_ref()
                        .is_some_and(|c| record.release.as_ref() != Some(c));
                let moved = want.is_some_and(|w| {
                    record_points(&record.points) != w.points || record.spacing_m != w.spacing
                });
                area.state = if !record.complete || moved {
                    TerrainPutState::Waiting
                } else if stale && want.is_some() {
                    TerrainPutState::Stale
                } else {
                    TerrainPutState::Written
                };
                area.detail = match area.state {
                    TerrainPutState::Stale => Some(format!(
                        "Written from {}{}. {name} now uses {dataset}{}",
                        crate::terrain::setting(record.dataset),
                        record
                            .release
                            .as_ref()
                            .map_or_else(String::new, |r| format!(" {r}")),
                        current
                            .as_ref()
                            .map_or_else(String::new, |r| format!(" {r}")),
                    )),
                    TerrainPutState::Written if record.skipped > 0 => Some(format!(
                        "{} {} left off: a height was missing",
                        record.skipped,
                        if record.skipped == 1 {
                            "block"
                        } else {
                            "blocks"
                        }
                    )),
                    _ => None,
                };
                for block in &on_card {
                    view.card(block, VehicleCardBlockState::Written, &id);
                }
            } else if want.is_none() {
                continue;
            }
            if area.state == TerrainPutState::Waiting && area.detail.is_none() {
                area.detail = blocked.clone().or_else(|| {
                    (!agreed && !pass.is_some_and(|p| p.explicit))
                        .then(|| format!("Waiting for you to agree to put it on {name}"))
                });
            }
            if let Some(want) = want
                && pass.is_none_or(|p| !p.planned())
                && area.state == TerrainPutState::Waiting
            {
                area.blocks = u32::try_from(
                    card::blocks_covering(&want.points, want.spacing)
                        .values()
                        .map(Vec::len)
                        .sum::<usize>(),
                )
                .unwrap_or(u32::MAX);
                area.bytes = u64::from(area.blocks) * BLOCK_BYTES as u64;
            }
            view.areas.push(area);
        }
        view.ask = (!agreed && blocked.as_deref().is_none_or(|b| b.contains("is armed")))
            .then(|| self.put_ask(vehicle_id, &needing))
            .flatten();
        view
    }

    /// The vehicles an area is on or going to, for `terrain/areas`.
    pub(crate) fn terrain_area_vehicles(&self, area_id: &str) -> Vec<VehicleTerrainArea> {
        let mut ids: BTreeSet<String> = self
            .state
            .settings
            .terrain_on_vehicles
            .iter()
            .filter(|r| r.area_id == area_id)
            .map(|r| r.vehicle_id.clone())
            .collect();
        for (id, puts) in &self.terrain.puts.vehicles {
            if puts.passes.contains_key(area_id) {
                ids.insert(id.clone());
            }
        }
        for vehicle in self.state.vehicles.values() {
            if vehicle.presence == Presence::Online
                && self.put_spacing(&vehicle.id).is_some()
                && !self.is_simulated_submarine(&vehicle.id)
            {
                ids.insert(vehicle.id.clone());
            }
        }
        ids.retain(|id| self.state.vehicles.contains_key(id));
        ids.iter()
            .flat_map(|id| self.terrain_put_view(id).areas)
            .filter(|a| a.area_id == area_id)
            .collect()
    }
}

/// A vehicle's areas as `terrain/vehicle` gives them.
#[derive(Debug, Default)]
pub(crate) struct PutView {
    pub(crate) areas: Vec<VehicleTerrainArea>,
    pub(crate) cards: Vec<VehicleCardBlock>,
    pub(crate) omitted: u32,
    pub(crate) ask: Option<TerrainPutAsk>,
    pub(crate) automatic: bool,
}

impl PutView {
    fn card(&mut self, block: &CardBlock, state: VehicleCardBlockState, area: &str) {
        if self.cards.len() >= CARD_BLOCKS_SHOWN {
            self.omitted += 1;
            return;
        }
        self.cards.push(VehicleCardBlock {
            corners: crate::terrain_vehicle::corners(Block {
                lat: block.lat,
                lon: block.lon,
                spacing: block.spacing,
            }),
            state,
            area_id: area.to_owned(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_degree_reads_back_from_its_key() {
        for degree in [
            Degree { lat: -36, lon: 149 },
            Degree { lat: 53, lon: -5 },
            Degree { lat: 0, lon: 0 },
            Degree {
                lat: -17,
                lon: -180,
            },
        ] {
            assert_eq!(degree_of(&key(degree)), Some(degree), "{}", key(degree));
        }
    }

    #[test]
    fn sizes_and_times_read_plainly() {
        assert_eq!(size_words(1_835_008), "1.8 MB");
        assert_eq!(size_words(196_608), "197 kB");
        assert_eq!(time_words(360), "about 6 minutes");
        assert_eq!(time_words(4), "about 4 seconds");
    }
}
