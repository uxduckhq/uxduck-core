//! The seabed for simulated submarines (ADR-0043 U9): heights from GMRT,
//! Lamont's Global Multi-Resolution Topography (CC BY 4.0), whose grid
//! service answers any area at about 60 m where ships have surveyed and
//! GEBCO's elsewhere, land included.
//!
//! As the cache does, nothing here does I/O: a tile is read from the
//! host's store, else fetched from GMRT by its full address and kept
//! (its licence allows it), through the same [`Order`]s and
//! [`Arrived`]s. A tile is a sixteenth of a degree square.

use std::collections::HashMap;
use std::sync::Arc;

use crate::cache::{Arrived, Order, RETRY_US, Unavailable};

/// GMRT's grid service.
pub const GMRT_URL: &str = "https://www.gmrt.org/services/GridServer";
/// Kept tiles live under this prefix in the host's store.
pub const PREFIX: &str = "seabed/gmrt/";
/// Credit for the heights, wherever they are shown.
pub const CREDIT: &str =
    "Seabed: GMRT (Ryan et al., 2009), Lamont-Doherty Earth Observatory, CC BY 4.0";
/// Tiles per degree.
const PER_DEGREE: f64 = 16.0;
/// A tile is at most this large; GMRT's answer for one is about 100 kB.
const MAX_BYTES: usize = 4 * 1024 * 1024;

/// One tile's grid, as GMRT's ESRI ASCII gives it: rows from the north.
#[derive(Debug, Clone, PartialEq)]
pub struct Grid {
    cols: u32,
    rows: u32,
    /// The south-west corner of the south-west cell, in degrees.
    west: f64,
    south: f64,
    cell: f64,
    values: Vec<Option<f64>>,
}

impl Grid {
    /// Reads an ESRI ASCII grid.
    ///
    /// # Errors
    ///
    /// What is wrong with it, when it is not a grid of at least 2 × 2.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_BYTES {
            return Err("the grid is too large".into());
        }
        let text = std::str::from_utf8(bytes).map_err(|_| "the grid is not text".to_owned())?;
        let mut words = text.split_ascii_whitespace();
        let mut header = HashMap::new();
        for _ in 0..6 {
            let name = words.next().ok_or("the grid's header is short")?;
            let value = words.next().ok_or("the grid's header is short")?;
            header.insert(name.to_ascii_lowercase(), value);
        }
        let word = |name: &str| {
            header
                .get(name)
                .copied()
                .ok_or_else(|| format!("the grid has no {name}"))
        };
        let count = |name: &str| -> Result<u32, String> {
            word(name)?
                .parse()
                .map_err(|_| format!("the grid's {name} is not a count"))
        };
        let number = |name: &str| -> Result<f64, String> {
            word(name)?
                .parse()
                .map_err(|_| format!("the grid's {name} is not a number"))
        };
        let (cols, rows, cell) = (count("ncols")?, count("nrows")?, number("cellsize")?);
        let nodata = word("nodata_value")?;
        let cells = usize::try_from(u64::from(cols) * u64::from(rows)).unwrap_or(usize::MAX);
        if cols < 2
            || rows < 2
            || cells > 1_000_000
            || cell.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater)
        {
            return Err("the grid's size is not usable".into());
        }
        let mut values = Vec::with_capacity(cells);
        for text in words.by_ref().take(cells) {
            let value: f64 = text.parse().map_err(|_| "a grid value is not a number")?;
            values.push((text != nodata && value.is_finite()).then_some(value));
        }
        if values.len() != cells {
            return Err("the grid ends early".into());
        }
        Ok(Self {
            cols,
            rows,
            west: number("xllcorner")?,
            south: number("yllcorner")?,
            cell,
            values,
        })
    }

    /// The height at a point, bilinear between the cells' centres;
    /// `None` off the grid or next to a cell with no value.
    #[must_use]
    pub fn height(&self, lat: f64, lon: f64) -> Option<f64> {
        // Cell centres, counted from the south-west.
        let across = (lon - self.west) / self.cell - 0.5;
        let up = (lat - self.south) / self.cell - 0.5;
        let last_col = f64::from(self.cols - 1);
        let last_row = f64::from(self.rows - 1);
        if !(-0.5..=last_col + 0.5).contains(&across) || !(-0.5..=last_row + 0.5).contains(&up) {
            return None;
        }
        let across = across.clamp(0.0, last_col);
        let up = up.clamp(0.0, last_row);
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped to the grid above"
        )]
        let (col, row) = (
            (across.floor() as u32).min(self.cols - 2),
            (up.floor() as u32).min(self.rows - 2),
        );
        let (u, v) = (across - f64::from(col), up - f64::from(row));
        // Row r counted from the south is row rows-1-r from the north.
        let at = |c: u32, r: u32| {
            let index = u64::from(self.rows - 1 - r) * u64::from(self.cols) + u64::from(c);
            usize::try_from(index)
                .ok()
                .and_then(|i| self.values.get(i).copied().flatten())
        };
        let (sw, se) = (at(col, row)?, at(col + 1, row)?);
        let (nw, ne) = (at(col, row + 1)?, at(col + 1, row + 1)?);
        Some((sw * (1.0 - u) + se * u) * (1.0 - v) + (nw * (1.0 - u) + ne * u) * v)
    }
}

/// A tile by its south-west corner, in sixteenths of a degree.
type TileKey = (i32, i32);

#[allow(clippy::cast_possible_truncation, reason = "within ±2880")]
fn tile_of(lat: f64, lon: f64) -> TileKey {
    (
        (lat * PER_DEGREE).floor() as i32,
        (lon * PER_DEGREE).floor() as i32,
    )
}

fn key_of((lat, lon): TileKey) -> String {
    format!("{PREFIX}{lat}_{lon}.asc")
}

/// GMRT's address for a tile, a cell wider on each side so points at its
/// edge have neighbours.
fn url_of((lat, lon): TileKey) -> String {
    let margin = 0.002;
    let south = f64::from(lat) / PER_DEGREE - margin;
    let west = f64::from(lon) / PER_DEGREE - margin;
    let north = f64::from(lat + 1) / PER_DEGREE + margin;
    let east = f64::from(lon + 1) / PER_DEGREE + margin;
    format!(
        "{GMRT_URL}?north={north:.6}&south={south:.6}&east={east:.6}&west={west:.6}&layer=topo&format=esriascii&resolution=max"
    )
}

#[derive(Debug)]
enum Slot {
    Reading,
    Fetching,
    Ready(Arc<Grid>),
    Failed { why: Unavailable, at_us: u64 },
}

/// The seabed tiles one daemon holds, and what it asks its host for.
#[derive(Debug, Default)]
pub struct Seabed {
    tiles: HashMap<TileKey, Slot>,
    /// Fetches in flight, by address.
    fetching: HashMap<String, TileKey>,
    orders: Vec<Order>,
}

impl Seabed {
    /// The height at a point over mean sea level: `None` while its tile
    /// is being read or fetched (asked for here), `Err` when it cannot be had.
    pub fn at(&mut self, lat: f64, lon: f64, now_us: u64) -> Option<Result<f64, Unavailable>> {
        let tile = tile_of(lat, lon);
        match self.tiles.get(&tile) {
            Some(Slot::Ready(grid)) => Some(
                grid.height(lat, lon)
                    .ok_or_else(|| Unavailable::Failed("GMRT has no height there".into())),
            ),
            Some(Slot::Reading | Slot::Fetching) => None,
            Some(Slot::Failed { why, at_us }) if now_us.saturating_sub(*at_us) < RETRY_US => {
                Some(Err(why.clone()))
            }
            _ => {
                self.tiles.insert(tile, Slot::Reading);
                self.orders.push(Order::Read { key: key_of(tile) });
                None
            }
        }
    }

    /// What to ask the host for.
    pub fn take_orders(&mut self) -> Vec<Order> {
        std::mem::take(&mut self.orders)
    }

    /// The host's answer, if it is the seabed's; `true` when it was.
    pub fn arrived(&mut self, arrived: &Arrived, now_us: u64) -> bool {
        match arrived {
            Arrived::Read { key, bytes } if key.starts_with(PREFIX) => {
                let Some(tile) = tile_for_key(key) else {
                    return true;
                };
                if let Some(Ok(grid)) = bytes.as_deref().map(Grid::parse) {
                    self.tiles.insert(tile, Slot::Ready(Arc::new(grid)));
                } else {
                    // Not kept yet, or kept damaged: fetched afresh.
                    let url = url_of(tile);
                    self.fetching.insert(url.clone(), tile);
                    self.tiles.insert(tile, Slot::Fetching);
                    self.orders.push(Order::Fetch { key: url });
                }
                true
            }
            Arrived::Fetched { key, result } => {
                let Some(tile) = self.fetching.remove(key) else {
                    return false;
                };
                let slot = match result {
                    Ok(bytes) => match Grid::parse(bytes) {
                        Ok(grid) => {
                            self.orders.push(Order::Write {
                                key: key_of(tile),
                                bytes: bytes.clone(),
                            });
                            Slot::Ready(Arc::new(grid))
                        }
                        Err(why) => Slot::Failed {
                            why: Unavailable::Failed(format!("GMRT's answer is not a grid: {why}")),
                            at_us: now_us,
                        },
                    },
                    Err(error) => Slot::Failed {
                        why: if error.offline {
                            Unavailable::Offline
                        } else {
                            Unavailable::Failed(error.message.clone())
                        },
                        at_us: now_us,
                    },
                };
                self.tiles.insert(tile, slot);
                true
            }
            Arrived::Written { key, .. } => key.starts_with(PREFIX),
            Arrived::Read { .. } => false,
        }
    }
}

/// The tile a kept file holds, by its key.
fn tile_for_key(key: &str) -> Option<TileKey> {
    let name = key.strip_prefix(PREFIX)?.strip_suffix(".asc")?;
    let (lat, lon) = name.split_once('_')?;
    Some((lat.parse().ok()?, lon.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::FetchError;

    /// A 3 × 3 grid from 25.68° N, 80.12° W, cells 0.01° wide.
    const GRID: &str = "ncols 3\nnrows 3\nxllcorner -80.12\nyllcorner 25.68\ncellsize 0.01\nnodata_value -2147483648\n-10 -11 -12\n-20 -21 -22\n-30 -31 -2147483648\n";

    /// Within a millimetre: the degrees do not land exactly on cell centres.
    fn near(height: Option<f64>, want: f64) -> bool {
        height.is_some_and(|h| (h - want).abs() < 1e-3)
    }

    #[test]
    fn reads_rows_from_the_north_and_interpolates() {
        let grid = Grid::parse(GRID.as_bytes()).unwrap();
        // The south-west cell's centre is the last row's first value.
        assert!(near(grid.height(25.685, -80.115), -30.0));
        assert!(near(grid.height(25.705, -80.115), -10.0));
        // Halfway between the north row's first two centres.
        assert!(near(grid.height(25.705, -80.11), -10.5));
        // Next to the cell with no value.
        assert_eq!(grid.height(25.685, -80.1), None);
        assert_eq!(grid.height(26.0, -80.115), None);
    }

    #[test]
    fn refuses_what_is_not_a_grid() {
        assert!(Grid::parse(b"<html>busy</html>").is_err());
        assert!(
            Grid::parse(
                b"ncols 3\nnrows 3\nxllcorner 0\nyllcorner 0\ncellsize 1\nnodata_value 0\n1 2"
            )
            .is_err()
        );
    }

    #[test]
    fn reads_a_kept_tile_else_fetches_and_keeps_it() {
        let mut seabed = Seabed::default();
        assert_eq!(seabed.at(25.7, -80.105, 0), None);
        let orders = seabed.take_orders();
        let [Order::Read { key }] = &orders[..] else {
            panic!("{orders:?}")
        };
        assert_eq!(key, "seabed/gmrt/411_-1282.asc");
        assert!(seabed.arrived(
            &Arrived::Read {
                key: key.clone(),
                bytes: None
            },
            0
        ));
        let orders = seabed.take_orders();
        let [Order::Fetch { key: url }] = &orders[..] else {
            panic!("{orders:?}")
        };
        assert!(
            url.starts_with("https://www.gmrt.org/services/GridServer?north=25.75"),
            "{url}"
        );
        assert!(url.contains("format=esriascii"), "{url}");
        assert_eq!(seabed.at(25.7, -80.105, 0), None);
        assert!(seabed.arrived(
            &Arrived::Fetched {
                key: url.clone(),
                result: Ok(GRID.as_bytes().to_vec())
            },
            0,
        ));
        assert!(
            matches!(&seabed.take_orders()[..], [Order::Write { key: kept, .. }] if kept == key)
        );
        assert!(near(
            seabed.at(25.695, -80.105, 0).and_then(Result::ok),
            -21.0
        ));
    }

    #[test]
    fn offline_is_said_and_tried_again_later() {
        let mut seabed = Seabed::default();
        seabed.at(25.7, -80.105, 0);
        let key = match &seabed.take_orders()[..] {
            [Order::Read { key }] => key.clone(),
            other => panic!("{other:?}"),
        };
        seabed.arrived(&Arrived::Read { key, bytes: None }, 0);
        let url = match &seabed.take_orders()[..] {
            [Order::Fetch { key }] => key.clone(),
            other => panic!("{other:?}"),
        };
        let offline = FetchError {
            offline: true,
            message: "no network".into(),
        };
        seabed.arrived(
            &Arrived::Fetched {
                key: url,
                result: Err(offline),
            },
            0,
        );
        assert_eq!(seabed.at(25.7, -80.105, 1), Some(Err(Unavailable::Offline)));
        assert_eq!(seabed.at(25.7, -80.105, RETRY_US + 1), None);
    }

    #[test]
    fn leaves_other_files_to_the_cache() {
        let mut seabed = Seabed::default();
        assert!(!seabed.arrived(
            &Arrived::Read {
                key: "manifest.json".into(),
                bytes: None
            },
            0
        ));
        let other = Arrived::Fetched {
            key: "alos/x.uxdt".into(),
            result: Ok(Vec::new()),
        };
        assert!(!seabed.arrived(&other, 0));
    }
}
