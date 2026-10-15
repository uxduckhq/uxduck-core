//! The cache against a fake bucket and store (ADR-0026 T2, T3): signed
//! manifests, checksums, withdrawn releases and answering offline.

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
#![allow(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "the bucket's own names"
)]

use std::collections::{BTreeMap, HashMap};

use uxduck_terrain::chunk::VOID;
use uxduck_terrain::lookup::Ground;
use uxduck_terrain::manifest::{MANIFEST, Manifest, SCHEMA, SIGNATURE, SourceFile, Withdrawn};
use uxduck_terrain::publish::{ReleaseBuilder, TileSamples};
use uxduck_terrain::{Arrived, Dataset, FetchError, Order, Terrain, Tile, Unavailable};

const HOUR_US: u64 = 3600 * 1_000_000;

/// The bucket online, and the host's store.
struct World {
    pair: minisign::KeyPair,
    bucket: HashMap<String, Vec<u8>>,
    store: HashMap<String, Vec<u8>>,
    online: bool,
    fetched: Vec<String>,
    now_us: u64,
}

impl World {
    fn new() -> Self {
        Self {
            pair: minisign::KeyPair::generate_unencrypted_keypair().unwrap(),
            bucket: HashMap::new(),
            store: HashMap::new(),
            online: true,
            fetched: Vec::new(),
            now_us: 1_000_000,
        }
    }

    fn terrain(&self) -> Terrain {
        Terrain::new(Some(self.pair.pk.to_base64()))
    }

    /// Publishes a release of a 16 × 16 tile whose heights are `base` plus
    /// the row, with a void at row 5 column 5; the tile to its south is sea.
    fn publish(&mut self, dataset: Dataset, id: &str, base: i16, published_at: &str) {
        let mut builder = ReleaseBuilder::new(dataset, id, published_at, "test");
        let tile = Tile::parse("N53W005").unwrap();
        let mut samples = Vec::new();
        for r in 0..16i16 {
            for c in 0..16i16 {
                samples.push(if (r, c) == (5, 5) { VOID } else { base + r });
            }
        }
        let samples = TileSamples {
            tile,
            rows: 16,
            cols: 16,
            unit_dm: 10,
            samples,
        };
        let bucket = &mut self.bucket;
        let mut keep = |key: String, bytes: Vec<u8>| {
            bucket.insert(key, bytes);
        };
        let source = SourceFile {
            name: "N53W005.hgt.zip".into(),
            sha256: "0".repeat(64),
        };
        builder.add_tile(&samples, source, &mut keep).unwrap();
        builder.add_sea_tile(Tile::parse("N52W005").unwrap());
        let release = builder.finish(&mut keep);
        let mut manifest = self.manifest();
        manifest.published_at = published_at.into();
        manifest
            .releases
            .retain(|r| !(r.dataset == dataset && r.id == id));
        manifest.releases.push(release);
        manifest.current.insert(dataset.id().into(), id.into());
        self.sign(&manifest);
    }

    fn manifest(&self) -> Manifest {
        self.bucket.get(MANIFEST).map_or_else(
            || Manifest {
                schema: SCHEMA,
                published_at: String::new(),
                current: BTreeMap::new(),
                releases: Vec::new(),
                withdrawn: Vec::new(),
            },
            |b| serde_json::from_slice(b).unwrap(),
        )
    }

    fn sign(&mut self, manifest: &Manifest) {
        let bytes = serde_json::to_vec(manifest).unwrap();
        let signature = minisign::sign(None, &self.pair.sk, bytes.as_slice(), None, None)
            .unwrap()
            .into_string();
        self.bucket.insert(MANIFEST.into(), bytes);
        self.bucket.insert(SIGNATURE.into(), signature.into_bytes());
    }

    /// Does what the cache asks until it asks nothing more.
    fn pump(&mut self, terrain: &mut Terrain) {
        loop {
            let orders = terrain.take_orders();
            if orders.is_empty() {
                return;
            }
            for order in orders {
                match order {
                    Order::Read { key } => {
                        let bytes = self.store.get(&key).cloned();
                        terrain.arrived(Arrived::Read { key, bytes }, self.now_us);
                    }
                    Order::Fetch { key } => {
                        self.fetched.push(key.clone());
                        let result = if self.online {
                            self.bucket.get(&key).cloned().ok_or(FetchError {
                                offline: false,
                                message: "404 Not Found".into(),
                            })
                        } else {
                            Err(FetchError {
                                offline: true,
                                message: "no network".into(),
                            })
                        };
                        terrain.arrived(Arrived::Fetched { key, result }, self.now_us);
                    }
                    Order::Write { key, bytes } => {
                        self.store.insert(key, bytes);
                    }
                    Order::Remove { prefix } => self.store.retain(|k, _| !k.starts_with(&prefix)),
                }
            }
        }
    }

    /// Asks until the cache answers.
    fn at(
        &mut self,
        terrain: &mut Terrain,
        dataset: Dataset,
        lat: f64,
        lon: f64,
    ) -> uxduck_terrain::Answer {
        for _ in 0..10 {
            if let Some(answer) = terrain.at(dataset, lat, lon, self.now_us) {
                return answer;
            }
            self.pump(terrain);
        }
        panic!("no answer for {lat}, {lon}");
    }
}

/// Row 2.5 of the 16 × 16 tile, mid column.
const LAT: f64 = 54.0 - 2.5 / 16.0;
const LON: f64 = -4.5;

#[test]
fn a_height_is_fetched_kept_and_answered_offline() {
    let mut world = World::new();
    world.publish(Dataset::Alos, "2026-10-14", 580, "2026-10-14T00:00:00Z");
    let mut terrain = world.terrain();
    terrain.start(world.now_us);
    world.pump(&mut terrain);
    let answer = world.at(&mut terrain, Dataset::Alos, LAT, LON);
    assert_eq!(answer.release.as_deref(), Some("2026-10-14"));
    assert_eq!(answer.ground, Ok(Ground::Height(582.5)));
    assert!(
        world
            .store
            .contains_key("alos/2026-10-14/cells/N50W005.json")
    );
    assert!(world.store.contains_key("alos/2026-10-14/N53W005/14.uxdt"));

    // A new start offline: everything comes from the store.
    world.online = false;
    let mut offline = world.terrain();
    offline.start(world.now_us);
    world.pump(&mut offline);
    let answer = world.at(&mut offline, Dataset::Alos, LAT, LON);
    assert_eq!(answer.ground, Ok(Ground::Height(582.5)));
    // A chunk never fetched is not available offline.
    let answer = world.at(&mut offline, Dataset::Alos, 53.1, -4.9);
    assert_eq!(answer.ground, Err(Unavailable::Offline));
}

#[test]
fn voids_are_no_data_and_the_sea_is_zero() {
    let mut world = World::new();
    world.publish(Dataset::Alos, "2026-10-14", 580, "2026-10-14T00:00:00Z");
    let mut terrain = world.terrain();
    terrain.start(world.now_us);
    let void = world.at(
        &mut terrain,
        Dataset::Alos,
        54.0 - 5.0 / 16.0,
        -5.0 + 5.0 / 16.0,
    );
    assert_eq!(void.ground, Ok(Ground::NoData));
    let sea = world.at(&mut terrain, Dataset::Alos, 52.5, -4.5);
    assert_eq!(sea.ground, Ok(Ground::Sea));
    // Next door, nothing was published: no data, never a height.
    let none = world.at(&mut terrain, Dataset::Alos, 53.5, -3.5);
    assert_eq!(none.ground, Ok(Ground::NoData));
}

#[test]
fn each_dataset_answers_from_its_own_release() {
    let mut world = World::new();
    world.publish(Dataset::Alos, "2026-10-14", 580, "2026-10-14T00:00:00Z");
    world.publish(Dataset::Copernicus, "2024_1", 584, "2026-10-14T01:00:00Z");
    let mut terrain = world.terrain();
    terrain.start(world.now_us);
    let alos = world.at(&mut terrain, Dataset::Alos, LAT, LON);
    let copernicus = world.at(&mut terrain, Dataset::Copernicus, LAT, LON);
    assert_eq!(alos.ground, Ok(Ground::Height(582.5)));
    assert_eq!(copernicus.ground, Ok(Ground::Height(586.5)));
    assert_eq!(copernicus.release.as_deref(), Some("2024_1"));
}

#[test]
fn files_that_do_not_match_are_refused() {
    let mut world = World::new();
    world.publish(Dataset::Alos, "2026-10-14", 580, "2026-10-14T00:00:00Z");
    // The store holds a changed chunk: it is fetched again, not used.
    let key = "alos/2026-10-14/N53W005/14.uxdt".to_owned();
    let other = uxduck_terrain::chunk::Chunk::new(2, 2, 10, vec![1, 1, 1, 1]).unwrap();
    world.store.insert(key.clone(), other.encode());
    let mut terrain = world.terrain();
    terrain.start(world.now_us);
    let answer = world.at(&mut terrain, Dataset::Alos, LAT, LON);
    assert_eq!(answer.ground, Ok(Ground::Height(582.5)));
    assert!(world.fetched.contains(&key));

    // The bucket serves a changed chunk: refused, and a problem to show.
    world
        .bucket
        .insert("alos/2026-10-14/N53W005/77.uxdt".into(), other.encode());
    let answer = world.at(
        &mut terrain,
        Dataset::Alos,
        54.0 - 14.5 / 16.0,
        -5.0 + 14.5 / 16.0,
    );
    assert!(matches!(answer.ground, Err(Unavailable::Failed(ref why)) if why.contains("checksum")));
    let problems = terrain.take_problems();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(!world.store.contains_key("alos/2026-10-14/N53W005/77.uxdt"));
}

#[test]
fn an_unsigned_or_older_manifest_is_refused() {
    let mut world = World::new();
    world.publish(Dataset::Alos, "2026-10-14", 580, "2026-10-14T00:00:00Z");
    let mut terrain = Terrain::new(Some(
        minisign::KeyPair::generate_unencrypted_keypair()
            .unwrap()
            .pk
            .to_base64(),
    ));
    terrain.start(world.now_us);
    let answer = world.at(&mut terrain, Dataset::Alos, LAT, LON);
    assert!(
        matches!(answer.ground, Err(Unavailable::Failed(ref why)) if why.contains("signature"))
    );
    assert_eq!(terrain.take_problems().len(), 1);

    // A manifest older than one already seen is not used.
    let mut world = World::new();
    world.publish(Dataset::Alos, "2026-10-14", 580, "2026-10-14T00:00:00Z");
    let older = world.bucket.clone();
    world.publish(Dataset::Alos, "2026-10-15", 700, "2026-10-15T00:00:00Z");
    let mut terrain = world.terrain();
    terrain.start(world.now_us);
    world.pump(&mut terrain);
    let newer = terrain.manifest().unwrap().published_at.clone();
    assert_eq!(newer, "2026-10-15T00:00:00Z");
    world.bucket = older;
    world.now_us += 7 * HOUR_US;
    terrain.refresh_if_due(world.now_us);
    world.pump(&mut terrain);
    assert_eq!(terrain.manifest().unwrap().published_at, newer);
    assert!(
        terrain
            .take_problems()
            .iter()
            .any(|p| p.body.contains("before one already seen"))
    );
}

#[test]
fn withdrawn_releases_are_removed() {
    let mut world = World::new();
    world.publish(Dataset::Alos, "2026-01-01", 500, "2026-01-01T00:00:00Z");
    let mut terrain = world.terrain();
    terrain.start(world.now_us);
    world.at(&mut terrain, Dataset::Alos, LAT, LON);
    assert!(
        world
            .store
            .keys()
            .any(|k| k.starts_with("alos/2026-01-01/"))
    );

    world.publish(Dataset::Alos, "2026-10-14", 580, "2026-10-14T00:00:00Z");
    let mut manifest = world.manifest();
    manifest.withdrawn.push(Withdrawn {
        dataset: Dataset::Alos,
        id: "2026-01-01".into(),
        reason: "land as sea next to the coast".into(),
    });
    world.sign(&manifest);
    world.now_us += 7 * HOUR_US;
    terrain.refresh_if_due(world.now_us);
    world.pump(&mut terrain);
    let answer = world.at(&mut terrain, Dataset::Alos, LAT, LON);
    assert_eq!(answer.release.as_deref(), Some("2026-10-14"));
    assert_eq!(answer.ground, Ok(Ground::Height(582.5)));
    assert!(
        !world
            .store
            .keys()
            .any(|k| k.starts_with("alos/2026-01-01/"))
    );
}

#[test]
fn without_a_key_or_a_manifest_it_says_why() {
    let world = World::new();
    let mut terrain = Terrain::new(None);
    terrain.start(world.now_us);
    let answer = terrain.at(Dataset::Alos, LAT, LON, world.now_us).unwrap();
    assert!(matches!(answer.ground, Err(Unavailable::Failed(_))));

    let mut world = World::new();
    world.online = false;
    let mut terrain = world.terrain();
    terrain.start(world.now_us);
    let answer = world.at(&mut terrain, Dataset::Alos, LAT, LON);
    assert_eq!(answer.ground, Err(Unavailable::Offline));
    assert!(
        terrain.take_problems().is_empty(),
        "offline is not a problem"
    );
}

#[test]
fn an_area_is_kept_whole_and_answers_offline_afterwards() {
    use uxduck_terrain::cover::{Bounds, chunks_in};
    let mut world = World::new();
    world.publish(Dataset::Alos, "2026-10-14", 580, "2026-10-14T00:00:00Z");
    let mut terrain = world.terrain();
    terrain.start(world.now_us);
    world.pump(&mut terrain);
    // The whole tile, and a bit of the sea south of it.
    let area = chunks_in(Bounds {
        south: 52.9,
        west: -5.0,
        north: 54.0,
        east: -4.0,
    });
    let size = loop {
        match terrain.size(Dataset::Alos, &area, world.now_us) {
            Some(size) => break size.unwrap(),
            None => world.pump(&mut terrain),
        }
    };
    assert_eq!(size.0, 64, "the tile's chunks; the sea has none");
    assert!(size.1 > 0);
    let job = terrain.keep(Dataset::Alos, area, world.now_us);
    world.pump(&mut terrain);
    let status = terrain.job(job).unwrap();
    assert!(status.finished, "{status:?}");
    assert_eq!((status.total, status.done, status.failed), (64, 64, 0));
    assert_eq!(status.bytes, size.1);
    let kept = world
        .store
        .keys()
        .filter(|k| k.starts_with("alos/2026-10-14/N53W005/"))
        .count();
    assert_eq!(kept, 64);

    // Kept again: read back, nothing fetched.
    world.fetched.clear();
    let again = terrain.keep(
        Dataset::Alos,
        chunks_in(Bounds {
            south: 53.5,
            west: -4.6,
            north: 53.6,
            east: -4.5,
        }),
        world.now_us,
    );
    world.pump(&mut terrain);
    assert!(terrain.job(again).unwrap().finished);
    assert!(
        world.fetched.iter().all(|k| !k.ends_with(".uxdt")),
        "{:?}",
        world.fetched
    );

    // Offline after a restart: anywhere in the area answers.
    world.online = false;
    let mut offline = world.terrain();
    offline.start(world.now_us);
    world.pump(&mut offline);
    let answer = world.at(&mut offline, Dataset::Alos, 53.1, -4.9);
    assert!(matches!(answer.ground, Ok(Ground::Height(_))), "{answer:?}");
}

#[test]
fn keeping_offline_fails_and_says_why() {
    use uxduck_terrain::cover::{Bounds, chunks_in};
    let mut world = World::new();
    world.publish(Dataset::Alos, "2026-10-14", 580, "2026-10-14T00:00:00Z");
    let mut terrain = world.terrain();
    terrain.start(world.now_us);
    let area = chunks_in(Bounds {
        south: 53.5,
        west: -4.6,
        north: 53.6,
        east: -4.5,
    });
    world.pump(&mut terrain);
    // The cell came; then the network went.
    let size = loop {
        match terrain.size(Dataset::Alos, &area, world.now_us) {
            Some(size) => break size.unwrap(),
            None => world.pump(&mut terrain),
        }
    };
    world.online = false;
    let job = terrain.keep(Dataset::Alos, area, world.now_us);
    world.pump(&mut terrain);
    let status = terrain.job(job).unwrap();
    assert!(status.finished);
    assert_eq!(status.failed, size.0);
    assert_eq!(status.why, Some(Unavailable::Offline));
}
