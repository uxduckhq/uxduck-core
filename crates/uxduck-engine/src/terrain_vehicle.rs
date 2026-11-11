//! A vehicle's terrain as UXDuck knows it (S310, ADR-0051 A1): per block of
//! its grid this session, the groups it asked for, those sent, and those
//! that cannot be answered; its own counts, spacing and dataset; and the
//! files its terrain folder holds, by MAVLink FTP listing. `terrain/vehicle`
//! answers it and `terrain/vehicleChanged` follows, at most twice a second,
//! to the connections that asked. Not journaled: a session's view, cleared
//! when the vehicle goes.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use uxduck_contracts::dto::plan::LatLon;
use uxduck_contracts::dto::terrain::{
    VehicleTerrainBlock, VehicleTerrainBlockState, VehicleTerrainFile, VehicleTerrainFiles,
    VehicleTerrainParams, VehicleTerrainResult,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ServerNotification;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::link::LinkKind;
use uxduck_core::domain::parameter::parameter_id;
use uxduck_mavlink::terrain::Block;
use uxduck_terrain::grid;

use crate::rpc::handlers::to_value;
use crate::runtime::{FileOp, TransferOrder};
use crate::vehicle_files::FileReport;
use crate::{ConnectionId, Engine};

/// `terrain/vehicleChanged` at most this often for a vehicle.
const PUSH_EVERY_US: u64 = 500_000;
/// ArduPilot's terrain folder on a board, and on a simulator (its
/// `HAL_BOARD_TERRAIN_DIRECTORY`).
const BOARD_FOLDER: &str = "/APM/TERRAIN";
const SIMULATOR_FOLDER: &str = "terrain";
/// `MAV_FRAME_GLOBAL_TERRAIN_ALT` and its `_INT` form.
const TERRAIN_FRAMES: [u8; 2] = [10, 11];

/// Every vehicle's view, and who hears of it.
#[derive(Debug, Default)]
pub(crate) struct Seen {
    vehicles: BTreeMap<String, VehicleTerrain>,
    /// Connections that asked, and for which vehicles.
    watchers: BTreeMap<ConnectionId, BTreeSet<String>>,
    /// File listings under way: job id to vehicle id.
    listings: BTreeMap<String, String>,
}

#[derive(Debug, Default)]
struct VehicleTerrain {
    /// In the order first asked.
    blocks: Vec<(Block, BlockSeen)>,
    /// It said it had groups pending, once this session.
    pending_seen: bool,
    files: Option<VehicleTerrainFiles>,
    /// What was last pushed, and when.
    pushed: Option<(u64, VehicleTerrainResult)>,
}

#[derive(Debug, Default)]
struct BlockSeen {
    /// Every group asked for this session.
    asked: u64,
    /// Those it still lacks: its latest mask, less what was sent since.
    wanted: u64,
    at_us: u64,
}

/// The vehicle's terrain folder.
pub(crate) fn terrain_folder(simulator: bool) -> &'static str {
    if simulator {
        SIMULATOR_FOLDER
    } else {
        BOARD_FOLDER
    }
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &VehicleTerrainParams,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    if !granted.contains(&Capability::Read) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            Capability::Read.denied_message(),
        ));
    }
    let id = engine
        .vehicle(&params.vehicle_id)
        .map(|v| v.id.clone())
        .ok_or_else(|| ApiError::new(ErrorKind::NotFound, "no such vehicle"))?;
    engine
        .terrain
        .seen
        .watchers
        .entry(connection)
        .or_default()
        .insert(id.clone());
    if params.list_files {
        engine.list_terrain_files(&id);
    }
    to_value(&engine.vehicle_terrain(&id))
}

/// A block's four corners, as ArduPilot places its outer points
/// (`calculate_grid_info`): from the whole degree's south-west corner,
/// `(grid_idx_x * 24 + x)` spacings north and `(grid_idx_y * 28 + y)` east,
/// with `x` 0 or 27 and `y` 0 or 31. Measured from the block's own corner
/// instead, they drift by tens of metres at 53°N.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a block's index from its degree, at most a few hundred"
)]
pub(crate) fn corners(block: Block) -> Vec<LatLon> {
    let spacing = f64::from(block.spacing);
    let info = grid::grid_info(block.lat, block.lon, block.spacing);
    let (degree_lat, degree_lon) = (
        i32::from(info.lat_degrees) * 10_000_000,
        i32::from(info.lon_degrees) * 10_000_000,
    );
    let (north, east) = grid::distance_ne(degree_lat, degree_lon, block.lat, block.lon);
    let step_x = f64::from(grid::SPACING_X) * spacing;
    let step_y = f64::from(grid::SPACING_Y) * spacing;
    let first_x = (f64::from(north) / step_x).round().max(0.0) as u32;
    let first_y = (f64::from(east) / step_y).round().max(0.0) as u32;
    let x = |point: u8| f64::from(first_x) * step_x + f64::from(point) * spacing;
    let y = |point: u8| f64::from(first_y) * step_y + f64::from(point) * spacing;
    let (top, right) = (grid::BLOCK_X - 1, grid::BLOCK_Y - 1);
    [
        (x(0), y(0)),
        (x(top), y(0)),
        (x(top), y(right)),
        (x(0), y(right)),
    ]
    .into_iter()
    .map(|(n, e)| {
        let (lat, lon) = grid::offset(degree_lat, degree_lon, n, e);
        LatLon {
            latitude_deg: f64::from(lat) * 1e-7,
            longitude_deg: f64::from(lon) * 1e-7,
        }
    })
    .collect()
}

#[allow(clippy::cast_possible_truncation, reason = "at most 56 groups")]
fn count(mask: u64) -> u8 {
    mask.count_ones() as u8
}

impl Engine {
    /// The vehicle asked for a block's groups (S118): its latest mask is
    /// what it still lacks.
    pub(crate) fn terrain_vehicle_asked(&mut self, vehicle_id: &str, block: Block, mask: u64) {
        let now = self.clock.now_us();
        let view = self
            .terrain
            .seen
            .vehicles
            .entry(vehicle_id.to_owned())
            .or_default();
        let index = view
            .blocks
            .iter()
            .position(|(b, _)| *b == block)
            .unwrap_or_else(|| {
                view.blocks.push((block, BlockSeen::default()));
                view.blocks.len() - 1
            });
        let asked = &mut view.blocks[index].1;
        asked.asked |= mask;
        asked.wanted = mask;
        asked.at_us = now;
    }

    /// Groups of a block went to the vehicle (S310). Nothing goes on a
    /// read-only link, whatever the session queued.
    pub fn terrain_sent(&mut self, vehicle_id: &str, block: Block, mask: u64) {
        let writable = self
            .vehicle(vehicle_id)
            .is_some_and(|v| self.not_writable(&v.link_id).is_none());
        if !writable {
            return;
        }
        if let Some((_, asked)) = self
            .terrain
            .seen
            .vehicles
            .get_mut(vehicle_id)
            .and_then(|v| v.blocks.iter_mut().find(|(b, _)| *b == block))
        {
            asked.wanted &= !mask;
        }
        self.push_vehicle_terrain(vehicle_id);
    }

    /// Telemetry came: a vehicle that says it has terrain pending uses it.
    pub(crate) fn review_vehicle_terrain(&mut self, vehicle_id: &str) {
        let pending = self.terrain_count(vehicle_id, "terrain.pending");
        if pending.is_some_and(|p| p > 0) {
            self.terrain
                .seen
                .vehicles
                .entry(vehicle_id.to_owned())
                .or_default()
                .pending_seen = true;
        }
        self.review_terrain_puts(vehicle_id);
        self.push_vehicle_terrain(vehicle_id);
    }

    /// The blocks the vehicle asked for this session.
    pub(crate) fn terrain_asked_blocks(&self, vehicle_id: &str) -> Vec<Block> {
        self.terrain
            .seen
            .vehicles
            .get(vehicle_id)
            .map(|v| v.blocks.iter().map(|(b, _)| *b).collect())
            .unwrap_or_default()
    }

    /// The vehicle went: what was known of its terrain goes with it.
    pub(crate) fn terrain_vehicle_gone(&mut self, vehicle_id: &str) {
        self.terrain_puts_gone(vehicle_id);
        self.terrain.seen.vehicles.remove(vehicle_id);
        self.terrain.seen.listings.retain(|_, v| v != vehicle_id);
        for watched in self.terrain.seen.watchers.values_mut() {
            watched.remove(vehicle_id);
        }
        self.terrain.blocks.retain(|(v, _), _| v != vehicle_id);
        let before = self.terrain.missing.len();
        self.terrain.missing.retain(|(v, _), _| v != vehicle_id);
        if self.terrain.missing.len() != before {
            self.terrain_flush();
        }
    }

    pub(crate) fn terrain_watchers_disconnected(&mut self, connection: ConnectionId) {
        self.terrain.seen.watchers.remove(&connection);
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a count, checked"
    )]
    fn terrain_count(&self, vehicle_id: &str, key: &str) -> Option<u32> {
        self.telemetry(vehicle_id)?
            .items
            .iter()
            .find(|i| i.key == key)
            .and_then(|i| i.value)
            .filter(|v| v.is_finite() && *v >= 0.0)
            .map(|v| v.round().min(f64::from(u32::MAX)) as u32)
    }

    fn terrain_parameter(&self, vehicle_id: &str, name: &str) -> Option<f64> {
        self.state
            .parameters
            .get(&parameter_id(vehicle_id, name))
            .map(|p| p.value)
            .filter(|v| v.is_finite())
    }

    /// Whether a vehicle is heard through a telemetry radio: a serial link,
    /// or one whose radio reports `RADIO_STATUS`. A simulator never is.
    pub(crate) fn on_a_radio(&self, vehicle_id: &str) -> bool {
        let Some(link) = self
            .vehicle(vehicle_id)
            .and_then(|v| self.state.links.get(&v.link_id))
        else {
            return false;
        };
        if link.kind.is_simulator() {
            return false;
        }
        matches!(link.kind, LinkKind::Serial { .. })
            || self
                .telemetry(vehicle_id)
                .is_some_and(|t| t.items.iter().any(|i| i.key == "link.rssi_pct"))
    }

    /// A vehicle's view as `terrain/vehicle` answers it.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a spacing in metres, checked"
    )]
    pub(crate) fn vehicle_terrain(&self, vehicle_id: &str) -> VehicleTerrainResult {
        let view = self.terrain.seen.vehicles.get(vehicle_id);
        let blocks: Vec<VehicleTerrainBlock> = view
            .map(|v| v.blocks.as_slice())
            .unwrap_or_default()
            .iter()
            .map(|(block, asked)| {
                let detail = self
                    .terrain
                    .missing
                    .get(&(vehicle_id.to_owned(), *block))
                    .map(|m| m.why.clone());
                let done = asked.asked & !asked.wanted;
                let state = if detail.is_some() {
                    VehicleTerrainBlockState::Unavailable
                } else if asked.wanted == 0 {
                    VehicleTerrainBlockState::Sent
                } else if done != 0 {
                    VehicleTerrainBlockState::Sending
                } else {
                    VehicleTerrainBlockState::Asked
                };
                VehicleTerrainBlock {
                    corners: corners(*block),
                    spacing_m: block.spacing,
                    state,
                    groups: count(asked.asked),
                    groups_done: count(done),
                    detail,
                    asked_at_us: asked.at_us,
                }
            })
            .collect();
        let pending = self.terrain_count(vehicle_id, "terrain.pending");
        let put = self.terrain_put_view(vehicle_id);
        let above_terrain = self.state.missions.get(vehicle_id).is_some_and(|m| {
            m.items
                .iter()
                .any(|item| TERRAIN_FRAMES.contains(&item.frame))
        });
        VehicleTerrainResult {
            vehicle_id: vehicle_id.to_owned(),
            used: !blocks.is_empty()
                || view.is_some_and(|v| v.pending_seen)
                || pending.is_some_and(|p| p > 0)
                || above_terrain,
            dataset: crate::terrain::wire(self.terrain_dataset_of(vehicle_id)),
            chosen: self
                .state
                .settings
                .vehicle_terrain_datasets
                .contains_key(vehicle_id),
            spacing_m: self
                .terrain_parameter(vehicle_id, "TERRAIN_SPACING")
                .filter(|s| *s >= 1.0 && *s <= f64::from(u16::MAX))
                .map(|s| s.round() as u16),
            enabled: self
                .terrain_parameter(vehicle_id, "TERRAIN_ENABLE")
                .map(|v| v != 0.0),
            pending,
            loaded: self.terrain_count(vehicle_id, "terrain.loaded"),
            blocks,
            files: view.and_then(|v| v.files.clone()),
            last_check: self.terrain.checks.last.get(vehicle_id).cloned(),
            areas: put.areas,
            card_blocks: put.cards,
            card_blocks_omitted: put.omitted,
            areas_off: self
                .state
                .settings
                .vehicles_without_terrain_areas
                .contains(vehicle_id),
            areas_automatic: put.automatic,
            ask: put.ask,
        }
    }

    /// Tells the connections that asked of a vehicle's terrain once it
    /// changed, at most twice a second; a change held back goes with the
    /// next one, or the vehicle's next telemetry.
    pub(crate) fn push_vehicle_terrain(&mut self, vehicle_id: &str) {
        let watchers: Vec<ConnectionId> = self
            .terrain
            .seen
            .watchers
            .iter()
            .filter(|(_, vehicles)| vehicles.contains(vehicle_id))
            .map(|(id, _)| *id)
            .collect();
        if watchers.is_empty() {
            return;
        }
        let now = self.clock.now_us();
        if self
            .terrain
            .seen
            .vehicles
            .get(vehicle_id)
            .and_then(|v| v.pushed.as_ref())
            .is_some_and(|(at, _)| now.saturating_sub(*at) < PUSH_EVERY_US)
        {
            return;
        }
        let current = self.vehicle_terrain(vehicle_id);
        let view = self
            .terrain
            .seen
            .vehicles
            .entry(vehicle_id.to_owned())
            .or_default();
        if view
            .pushed
            .as_ref()
            .is_some_and(|(_, last)| *last == current)
        {
            return;
        }
        view.pushed = Some((now, current.clone()));
        let frame =
            crate::rpc::router::notification(&ServerNotification::TerrainVehicleChanged(current));
        for id in watchers {
            self.send(id, &frame);
        }
    }

    /// The same for every vehicle someone watches: after the cache or the
    /// answers changed.
    pub(crate) fn push_watched_vehicle_terrain(&mut self) {
        let watched: BTreeSet<String> = self
            .terrain
            .seen
            .watchers
            .values()
            .flatten()
            .cloned()
            .collect();
        for vehicle_id in watched {
            self.push_vehicle_terrain(&vehicle_id);
        }
    }

    /// Lists the vehicle's terrain folder (A1): a few hundred bytes, but
    /// never while it is armed on a radio, nor on a link that may not send.
    fn list_terrain_files(&mut self, vehicle_id: &str) {
        let Some(vehicle) = self.vehicle(vehicle_id) else {
            return;
        };
        let simulator = self
            .state
            .links
            .get(&vehicle.link_id)
            .is_some_and(|l| l.kind.is_simulator());
        let path = terrain_folder(simulator);
        let (name, armed, link_id) = (vehicle.name.clone(), vehicle.armed, vehicle.link_id.clone());
        let refused = self.not_writable(&link_id).or_else(|| {
            (armed && self.on_a_radio(vehicle_id)).then(|| {
                format!("Not listed while {name} is armed on a radio, which the flight needs")
            })
        });
        let listing = self.terrain.seen.listings.values().any(|v| v == vehicle_id);
        let view = self
            .terrain
            .seen
            .vehicles
            .entry(vehicle_id.to_owned())
            .or_default();
        let files = view.files.get_or_insert_with(|| VehicleTerrainFiles {
            path: path.to_owned(),
            files: Vec::new(),
            listed_at_us: None,
            listing: false,
            detail: None,
        });
        if let Some(why) = refused {
            files.detail = Some(why);
            return;
        }
        if listing {
            return;
        }
        files.listing = true;
        files.detail = None;
        let id = format!("tf{}", &self.random.token()[..16]);
        self.terrain
            .seen
            .listings
            .insert(id.clone(), vehicle_id.to_owned());
        self.links.transfer(TransferOrder::File {
            vehicle_id: vehicle_id.to_owned(),
            id,
            op: FileOp::List { path: path.into() },
        });
    }

    /// A terrain folder's listing came, or failed; `false` when the job is
    /// not one of these.
    pub(crate) fn terrain_files_reported(&mut self, report: &FileReport) -> bool {
        let id = match report {
            FileReport::Listed { id, .. } | FileReport::Done { id, .. } => id,
            FileReport::Opened { id, .. }
            | FileReport::Data { id, .. }
            | FileReport::Sent { id, .. } => {
                return self.terrain.seen.listings.contains_key(id);
            }
        };
        let Some(vehicle_id) = self.terrain.seen.listings.remove(id) else {
            return false;
        };
        let now = self.clock.now_us();
        let Some(files) = self
            .terrain
            .seen
            .vehicles
            .get_mut(&vehicle_id)
            .and_then(|v| v.files.as_mut())
        else {
            return true;
        };
        files.listing = false;
        match report {
            FileReport::Listed { entries, .. } => {
                files.files = entries
                    .iter()
                    .filter_map(|(name, size)| {
                        size.map(|size| VehicleTerrainFile {
                            name: name.clone(),
                            size,
                        })
                    })
                    .collect();
                files.files.sort_by(|a, b| a.name.cmp(&b.name));
                files.listed_at_us = Some(now);
                files.detail = None;
            }
            FileReport::Done {
                result: Err(failure),
                ..
            } => {
                let detail = failure.detail(0);
                if detail == "there is no such file or folder" {
                    // No terrain was ever written to this card.
                    files.files.clear();
                    files.listed_at_us = Some(now);
                    files.detail = None;
                } else {
                    files.detail = Some(format!("Could not list {}: {detail}", files.path));
                }
            }
            _ => {}
        }
        self.push_vehicle_terrain(&vehicle_id);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How many spacings north and east of its degree's corner ArduPilot's
    /// own `calculate_grid_info` puts a point.
    fn placed(at: &LatLon, spacing: u16) -> (f32, f32) {
        #[allow(clippy::cast_possible_truncation, reason = "1e-7 degrees")]
        let info = grid::grid_info(
            (at.latitude_deg * 1e7).round() as i32,
            (at.longitude_deg * 1e7).round() as i32,
            spacing,
        );
        #[allow(clippy::cast_precision_loss, reason = "a few hundred points")]
        let along = |block: u32, step: u8, point: u8, frac: f32| {
            (block * u32::from(step) + u32::from(point)) as f32 + frac
        };
        (
            along(info.grid_idx_x, grid::SPACING_X, info.idx_x, info.frac_x),
            along(info.grid_idx_y, grid::SPACING_Y, info.idx_y, info.frac_y),
        )
    }

    #[test]
    fn a_blocks_corners_fall_on_the_points_ardupilot_places_from_the_degree() {
        // Far into the degree at 53°N, where measuring from the block's own
        // corner drifts by tens of metres.
        for (lat, lon) in [(534_000_000, -46_000_000), (539_500_000, -40_500_000)] {
            let info = grid::grid_info(lat, lon, 100);
            let block = Block {
                lat: info.grid_lat,
                lon: info.grid_lon,
                spacing: 100,
            };
            #[allow(clippy::cast_precision_loss, reason = "a few hundred points")]
            let (x0, y0) = (
                (info.grid_idx_x * u32::from(grid::SPACING_X)) as f32,
                (info.grid_idx_y * u32::from(grid::SPACING_Y)) as f32,
            );
            let (top, right) = (f32::from(grid::BLOCK_X - 1), f32::from(grid::BLOCK_Y - 1));
            let want = [
                (x0, y0),
                (x0 + top, y0),
                (x0 + top, y0 + right),
                (x0, y0 + right),
            ];
            for (corner, (north, east)) in corners(block).iter().zip(want) {
                let (at_north, at_east) = placed(corner, 100);
                // Within 10 cm of the point; measured from the block's corner it is tens of metres off.
                assert!(
                    (at_north - north).abs() < 0.001 && (at_east - east).abs() < 0.001,
                    "{corner:?} is at {at_north}, {at_east} spacings, not {north}, {east}"
                );
            }
        }
    }
}
