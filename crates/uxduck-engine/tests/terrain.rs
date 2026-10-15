//! `terrain/at` (S23, ADR-0026): answered from the default dataset or the
//! one asked for, once the host has fetched what it needs, and with the
//! reason when there is no height.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::float_cmp,
    clippy::needless_pass_by_value,
    clippy::option_option,
    clippy::case_sensitive_file_extension_comparisons,
    reason = "test helpers and scenarios"
)]

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde_json::{Value, json};
use uxduck_contracts::dto::initialize::DaemonCapabilities;
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::domain::telemetry::{PositionEstimate, TelemetryItem, Unit, VehicleTelemetry};
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::runtime::{
    Clock, CommandOrder, Forget, LinkPort, MemoryCredentials, NoAgent, NoSystem, Ports, Random,
    TransferOrder,
};
use uxduck_engine::terrain::TerrainPort;
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded};
use uxduck_mavlink::terrain::Block;
use uxduck_terrain::chunk::VOID;
use uxduck_terrain::grid;
use uxduck_terrain::manifest::{MANIFEST, Manifest, SCHEMA, SIGNATURE, SourceFile};
use uxduck_terrain::publish::{ReleaseBuilder, TileSamples};
use uxduck_terrain::{Arrived, Dataset, FetchError, Order, Tile};

/// The engine's clock, moved on by the test.
#[derive(Clone, Default)]
struct Tick(Arc<AtomicU64>);

impl Clock for Tick {
    fn now_us(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct Counter(u64);

impl Random for Counter {
    fn fill(&mut self, bytes: &mut [u8]) {
        for byte in bytes {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            *byte = (self.0 >> 56) as u8;
        }
    }
}

/// What the engine asked the host for.
#[derive(Clone, Default)]
struct Asked(Arc<Mutex<Vec<Order>>>);

impl TerrainPort for Asked {
    fn order(&mut self, order: Order) {
        self.0.lock().unwrap().push(order);
    }
}

/// What the engine asked the data plane to send: terrain answers (S118).
#[derive(Clone, Default)]
struct Sent(Arc<Mutex<Vec<TransferOrder>>>);

impl LinkPort for Sent {
    fn open(&mut self, _: &uxduck_core::domain::link::Link) {}
    fn close(&mut self, _: &str) {}
    fn command(&mut self, _: CommandOrder) {}
    fn transfer(&mut self, order: TransferOrder) {
        self.0.lock().unwrap().push(order);
    }
}

impl Sent {
    /// The points the engine asked a vehicle to check (S119).
    fn checks(&self) -> Vec<(i32, i32)> {
        let mut checks = Vec::new();
        self.0.lock().unwrap().retain(|order| match order {
            TransferOrder::TerrainCheck { lat, lon, .. } => {
                checks.push((*lat, *lon));
                false
            }
            _ => true,
        });
        checks
    }

    /// The groups sent for a block, by bit, and the masks given.
    fn take(&self) -> (Vec<(u8, [i16; 16])>, Vec<u64>) {
        let mut groups = Vec::new();
        let mut masks = Vec::new();
        for order in std::mem::take(&mut *self.0.lock().unwrap()) {
            if let TransferOrder::TerrainData {
                groups: g, mask, ..
            } = order
            {
                masks.push(mask);
                groups.extend(g.into_iter().map(|g| (g.gridbit, g.heights)));
            }
        }
        (groups, masks)
    }
}

/// A bucket with a hill at N53W005: 16 × 16 samples, ALOS the row number
/// plus 580, Copernicus 4 m higher, and one void.
fn bucket() -> (HashMap<String, Vec<u8>>, String) {
    let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let mut files = HashMap::new();
    let mut manifest = Manifest {
        schema: SCHEMA,
        published_at: "2026-10-14T00:00:00Z".into(),
        current: BTreeMap::new(),
        releases: Vec::new(),
        withdrawn: Vec::new(),
    };
    for (dataset, base) in [(Dataset::Alos, 580), (Dataset::Copernicus, 584)] {
        let mut builder = ReleaseBuilder::new(dataset, "2026-10-14", "2026-10-14", "test");
        let samples: Vec<i16> = (0..256)
            .map(|i| if i == 5 * 16 + 5 { VOID } else { base + i / 16 })
            .collect();
        let mut keep = |key: String, bytes: Vec<u8>| {
            files.insert(key, bytes);
        };
        builder
            .add_tile(
                &TileSamples {
                    tile: Tile::parse("N53W005").unwrap(),
                    rows: 16,
                    cols: 16,
                    unit_dm: 10,
                    samples,
                },
                SourceFile {
                    name: "test".into(),
                    sha256: "0".repeat(64),
                },
                &mut keep,
            )
            .unwrap();
        builder.add_sea_tile(Tile::parse("N52W005").unwrap());
        manifest.releases.push(builder.finish(&mut keep));
        manifest
            .current
            .insert(dataset.id().into(), "2026-10-14".into());
    }
    let bytes = serde_json::to_vec(&manifest).unwrap();
    let signature = minisign::sign(None, &pair.sk, bytes.as_slice(), None, None)
        .unwrap()
        .into_string();
    files.insert(MANIFEST.into(), bytes);
    files.insert(SIGNATURE.into(), signature.into_bytes());
    (files, pair.pk.to_base64())
}

struct World {
    clock: Tick,
    sent: Sent,
    engine: Engine,
    asked: Asked,
    bucket: HashMap<String, Vec<u8>>,
    /// What the host keeps (S120).
    store: HashMap<String, Vec<u8>>,
    online: bool,
    ui: (ConnectionId, UnboundedReceiver<String>),
    next_id: i64,
}

impl World {
    fn new() -> Self {
        let (bucket, key) = bucket();
        let clock = Tick::default();
        let sent = Sent::default();
        let config = EngineConfig {
            daemon_version: "0.1.0".into(),
            capabilities: DaemonCapabilities::default(),
            ui_url: "http://127.0.0.1:47800/".into(),
            checkpoint_every: 1000,
            models: vec![],
            agent_provider: "fake".into(),
            about: uxduck_engine::HostAbout::default(),
        };
        let ports = Ports {
            persistence: Box::new(Forget),
            agent: Box::new(NoAgent),
            credentials: Box::new(MemoryCredentials::default()),
            links: Box::new(sent.clone()),
            system: Box::new(NoSystem::default()),
            plugins: Box::new(uxduck_engine::runtime::NoPlugins),
        };
        let mut engine = Engine::new(
            config,
            Box::new(clock.clone()),
            Box::new(Counter(0)),
            ports,
            Loaded::default(),
        );
        let asked = Asked::default();
        engine.set_terrain(Box::new(asked.clone()), Some(key));
        let (outbox, inbox) = unbounded();
        let id = engine.connect(Transport::Socket, Trust::LocalUser, outbox);
        let mut world = Self {
            clock,
            sent,
            engine,
            asked,
            bucket,
            store: HashMap::new(),
            online: true,
            ui: (id, inbox),
            next_id: 0,
        };
        let init = world.call(
            "initialize",
            json!({
                "clientInfo": { "name": "operator-ui", "version": "0.1.0", "kind": "operator-ui" },
                "capabilities": { "protocolVersion": 1 }
            }),
        );
        assert!(init.get("result").is_some(), "{init}");
        world
    }

    /// Answers the engine's orders, with no store: every read is empty.
    fn host(&mut self) {
        loop {
            let orders = std::mem::take(&mut *self.asked.0.lock().unwrap());
            if orders.is_empty() {
                return;
            }
            for order in orders {
                match order {
                    Order::Read { key } => {
                        let bytes = self.store.get(&key).cloned();
                        self.engine.terrain_arrived(Arrived::Read { key, bytes });
                    }
                    Order::Fetch { key } => {
                        let result = if self.online {
                            self.bucket.get(&key).cloned().ok_or(FetchError {
                                offline: false,
                                message: "404".into(),
                            })
                        } else {
                            Err(FetchError {
                                offline: true,
                                message: "offline".into(),
                            })
                        };
                        self.engine
                            .terrain_arrived(Arrived::Fetched { key, result });
                    }
                    Order::Write { key, bytes } => {
                        self.store.insert(key, bytes);
                    }
                    Order::Remove { .. } => {}
                }
            }
        }
    }

    fn send(&mut self, method: &str, params: &Value) -> i64 {
        self.next_id += 1;
        let frame =
            json!({ "jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params });
        self.engine.receive(self.ui.0, &frame.to_string());
        self.next_id
    }

    fn reply(&mut self, id: i64) -> Option<Value> {
        while let Ok(text) = self.ui.1.try_recv() {
            let frame: Value = serde_json::from_str(&text).unwrap();
            if frame["id"] == id {
                return Some(frame);
            }
        }
        None
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, &params);
        self.reply(id)
            .unwrap_or_else(|| panic!("no reply to {method}"))
    }

    /// Any deferred call, answered once the host has done its part.
    fn at_with(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, &params);
        self.host();
        self.reply(id).expect("answered")
    }

    /// `terrain/at`, answered once the host has done its part.
    fn at(&mut self, params: Value) -> Value {
        let id = self.send("terrain/at", &params);
        self.host();
        self.reply(id).expect("terrain/at answered")
    }
}

#[test]
fn the_ground_is_answered_once_fetched() {
    let mut world = World::new();
    // Row 2.5 of 16: 582.5 m above EGM96.
    let reply = world.at(json!({ "latitudeDeg": 54.0 - 2.5 / 16.0, "longitudeDeg": -4.5 }));
    let result = &reply["result"];
    assert_eq!(result["heightM"], 582.5, "{reply}");
    assert_eq!(result["dataset"], "alos");
    assert_eq!(result["release"], "2026-10-14");
    assert_eq!(result["geoid"], "EGM96");
}

#[test]
fn the_default_dataset_comes_from_settings() {
    let mut world = World::new();
    let at = json!({ "latitudeDeg": 54.0 - 2.5 / 16.0, "longitudeDeg": -4.5 });
    let asked = world.at(
        json!({ "latitudeDeg": 54.0 - 2.5 / 16.0, "longitudeDeg": -4.5, "dataset": "copernicus" }),
    );
    assert_eq!(asked["result"]["heightM"], 586.5, "{asked}");

    let settings = world.call(
        "settings/update",
        json!({ "expectedVersion": 0, "terrainDataset": "copernicus" }),
    );
    assert_eq!(
        settings["result"]["terrainDataset"], "copernicus",
        "{settings}"
    );
    let reply = world.at(at);
    assert_eq!(reply["result"]["dataset"], "copernicus");
    assert_eq!(reply["result"]["heightM"], 586.5);
    assert_eq!(reply["result"]["geoid"], "EGM2008");
}

#[test]
fn no_height_says_why() {
    let mut world = World::new();
    let void =
        world.at(json!({ "latitudeDeg": 54.0 - 5.0 / 16.0, "longitudeDeg": -5.0 + 5.0 / 16.0 }));
    assert_eq!(void["result"]["missing"], "noData", "{void}");
    assert!(void["result"].get("heightM").is_none());
    let sea = world.at(json!({ "latitudeDeg": 52.5, "longitudeDeg": -4.5 }));
    assert_eq!(sea["result"]["heightM"], 0.0);
    assert_eq!(sea["result"]["sea"], true);

    world.online = false;
    let offline = world.at(json!({ "latitudeDeg": 53.1, "longitudeDeg": -4.9 }));
    assert_eq!(offline["result"]["missing"], "offline", "{offline}");

    let bad = world.call(
        "terrain/at",
        json!({ "latitudeDeg": 91.0, "longitudeDeg": 0.0 }),
    );
    assert_eq!(bad["error"]["data"]["kind"], "invalid-value", "{bad}");
}

#[test]
fn a_manifest_that_could_not_be_fetched_is_fetched_again() {
    let mut world = World::new();
    world.online = false;
    let at = json!({ "latitudeDeg": 54.0 - 2.5 / 16.0, "longitudeDeg": -4.5 });
    let offline = world.at(at.clone());
    assert_eq!(offline["result"]["missing"], "offline", "{offline}");
    // Back online, and half a minute on: the next question fetches it.
    world.online = true;
    world.clock.0.fetch_add(31_000_000, Ordering::SeqCst);
    let first = world.at(at.clone());
    assert_eq!(first["result"]["heightM"], 582.5, "{first}");
}

/// A block of a vehicle's grid inside the fixture's tile, at 100 m.
fn block() -> Block {
    let info = grid::grid_info(535_000_000, -45_000_000, 100);
    Block {
        lat: info.grid_lat,
        lon: info.grid_lon,
        spacing: 100,
    }
}

#[test]
fn a_vehicle_is_answered_with_its_own_grid_rounded_to_the_metre() {
    let mut world = World::new();
    let block = block();
    world.engine.terrain_requested("veh-1", block, 0b101);
    world.host();
    let (groups, masks) = world.sent.take();
    assert_eq!(masks.last(), Some(&0b101));
    let bits: Vec<u8> = groups.iter().map(|g| g.0).collect();
    assert_eq!(bits, [0, 2]);
    // Each point's height is the dataset's at that point, rounded.
    for (gridbit, heights) in groups {
        for (i, (lat, lon)) in grid::group_points(block.lat, block.lon, 100, gridbit)
            .into_iter()
            .enumerate()
        {
            let at = world.at(json!({
                "latitudeDeg": f64::from(lat) * 1e-7,
                "longitudeDeg": f64::from(lon) * 1e-7,
            }));
            let want = at["result"]["heightM"].as_f64().unwrap().round();
            assert!(
                (f64::from(heights[i]) - want).abs() < 1.0,
                "{gridbit}/{i}: {} {want}",
                heights[i]
            );
        }
    }
    // Nothing is left to answer.
    world.engine.terrain_arrived(uxduck_terrain::Arrived::Read {
        key: "nothing".into(),
        bytes: None,
    });
    assert!(world.sent.take().0.is_empty());
}

#[test]
fn only_the_latest_request_for_a_block_is_answered() {
    let mut world = World::new();
    let block = block();
    // Two requests before any data: the second, smaller mask wins.
    world.engine.terrain_requested("veh-1", block, 0b1111);
    world.engine.terrain_requested("veh-1", block, 0b1000);
    world.host();
    let (groups, masks) = world.sent.take();
    let bits: Vec<u8> = groups.iter().map(|g| g.0).collect();
    assert_eq!(bits, [3]);
    assert!(masks.iter().all(|m| *m == 0b1111 || *m == 0b1000));
    assert_eq!(masks.last(), Some(&0b1000));
}

#[test]
fn no_terrain_sends_nothing_and_says_so_once() {
    let mut world = World::new();
    world.online = false;
    world.engine.terrain_requested("veh-1", block(), 0b11);
    world.host();
    world.engine.terrain_requested("veh-1", block(), 0b11);
    world.host();
    assert!(world.sent.take().0.is_empty(), "never a guessed height");
    let notes = world.call("state/subscribe", json!({ "entities": ["notification"] }));
    let titles: Vec<String> = notes["result"]["snapshot"]["notifications"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["title"].as_str().unwrap_or_default().to_owned())
        .filter(|t| t.starts_with("No terrain for"))
        .collect();
    assert_eq!(titles.len(), 1, "{notes}");
    assert!(
        titles[0].contains("veh-1") && titles[0].contains("53.4"),
        "{}",
        titles[0]
    );
}

/// Row 2.5 of the fixture's tile, mid column.
const SPOT: (f64, f64) = (54.0 - 2.5 / 16.0, -4.5);

impl World {
    /// A vehicle heard on a link, called Alpha.
    fn vehicle(&mut self) -> String {
        self.engine.vehicle_observed(Observation {
            id: "veh-1".into(),
            aliases: Vec::new(),
            link_id: "lk".into(),
            peer: "127.0.0.1:5760".into(),
            system_id: 1,
            component_ids: vec![1],
            autopilot: "Ardupilotmega".into(),
            vehicle_type: "Quadrotor".into(),
            uid: None,
            firmware: None,
            firmware_commit: None,
            presence: Presence::Online,
            last_heartbeat_us: 5,
            armed: false,
            mode: None,
        });
        "veh-1".into()
    }

    /// `terrain/check` at a point, the vehicle answering `height` there.
    fn check(&mut self, vehicle: &str, height: Option<Option<f32>>) -> Value {
        let id = self.send(
            "terrain/check",
            &json!({ "vehicleId": vehicle, "latitudeDeg": SPOT.0, "longitudeDeg": SPOT.1 }),
        );
        let asked = self.sent.checks();
        assert_eq!(asked.len(), 1);
        self.engine
            .terrain_checked(vehicle, asked[0].0, asked[0].1, height);
        self.host();
        self.reply(id).expect("terrain/check answered")["result"].clone()
    }

    fn warnings(&mut self) -> Vec<Value> {
        let notes = self.call("state/subscribe", json!({ "entities": ["notification"] }));
        notes["result"]["snapshot"]["notifications"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| {
                n["state"] == "active" && n["title"].as_str().is_some_and(|t| t.contains("terrain"))
            })
            .cloned()
            .collect()
    }
}

#[test]
fn a_vehicle_holding_its_own_dataset_agrees() {
    let mut world = World::new();
    let vehicle = world.vehicle();
    let first = world.check(&vehicle, Some(Some(0.0)));
    let expected = first["points"][0]["expectedM"].as_f64().unwrap();
    assert!((expected - 582.5).abs() < 1.0, "{first}");
    #[allow(clippy::cast_possible_truncation, reason = "a height")]
    let agrees = world.check(&vehicle, Some(Some(expected as f32 + 0.6)));
    assert_eq!(agrees["verdict"], "agrees", "{agrees}");
    assert_eq!(agrees["dataset"], "alos");
    // A point from the map menu never warns.
    assert!(world.warnings().is_empty());
}

#[test]
fn a_vehicle_holding_the_other_dataset_is_offered_it_and_keeps_it_once_chosen() {
    let mut world = World::new();
    let vehicle = world.vehicle();
    let expected = world.check(&vehicle, Some(Some(0.0)))["points"][0]["otherM"]
        .as_f64()
        .unwrap();
    // Without a point: home and under the vehicle, which warn.
    world.engine.telemetry_observed(VehicleTelemetry {
        vehicle_id: vehicle.clone(),
        positions: vec![PositionEstimate {
            source: "fused".into(),
            component_id: 1,
            latitude_deg: SPOT.0,
            longitude_deg: SPOT.1,
            altitudes: Vec::new(),
            heading_deg: None,
            course_deg: None,
            ground_speed_mps: None,
            climb_mps: None,
            horizontal_accuracy_m: None,
            vertical_accuracy_m: None,
            time_us: 10,
        }],
        items: Vec::new(),
    });
    let id = world.send("terrain/check", &json!({ "vehicleId": vehicle }));
    let asked = world.sent.checks();
    assert_eq!(asked.len(), 1, "under the vehicle; no home yet");
    #[allow(clippy::cast_possible_truncation, reason = "a height")]
    world.engine.terrain_checked(
        &vehicle,
        asked[0].0,
        asked[0].1,
        Some(Some(expected as f32)),
    );
    world.host();
    let result = world.reply(id).unwrap()["result"].clone();
    assert_eq!(result["verdict"], "matchesOther", "{result}");
    let warnings = world.warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(
        warnings[0]["title"],
        "Alpha's terrain matches Copernicus, not ALOS"
    );
    let action = &warnings[0]["actions"][0];
    assert_eq!(action["label"], "Use Copernicus for Alpha");
    assert_eq!(action["action"]["type"], "use-terrain-dataset");

    // The button: the vehicle's own dataset.
    let settings = world.call("state/subscribe", json!({ "entities": ["settings"] }));
    let version = settings["result"]["snapshot"]["settings"]["version"].clone();
    let updated = world.call(
        "settings/update",
        json!({ "expectedVersion": version, "vehicleTerrain": { "vehicleId": vehicle, "dataset": "copernicus" } }),
    );
    assert_eq!(
        updated["result"]["vehicleTerrainDatasets"][0]["dataset"], "copernicus",
        "{updated}"
    );
    // The warning goes at once, and a fresh check agrees.
    assert!(world.warnings().is_empty());
    let asked = world.sent.checks();
    assert_eq!(asked.len(), 1, "checked again");
    #[allow(clippy::cast_possible_truncation, reason = "a height")]
    world.engine.terrain_checked(
        &vehicle,
        asked[0].0,
        asked[0].1,
        Some(Some(expected as f32)),
    );
    world.host();
    let status = world.call("terrain/status", json!({ "vehicleId": vehicle }));
    assert_eq!(status["result"]["dataset"], "copernicus");
    assert_eq!(status["result"]["chosen"], true);
    assert_eq!(
        status["result"]["lastCheck"]["verdict"], "agrees",
        "{status}"
    );
    assert!(world.warnings().is_empty());
    // Answering it uses its dataset too.
    world.engine.terrain_requested(&vehicle, block(), 1);
    world.host();
    let (groups, _) = world.sent.take();
    assert_eq!(groups.len(), 1);
}

#[test]
fn old_or_foreign_terrain_differs_and_says_how_to_replace_it() {
    let mut world = World::new();
    let vehicle = world.vehicle();
    let expected = world.check(&vehicle, Some(Some(0.0)))["points"][0]["expectedM"]
        .as_f64()
        .unwrap();
    #[allow(clippy::cast_possible_truncation, reason = "a height")]
    let differs = world.check(&vehicle, Some(Some(expected as f32 + 20.0)));
    assert_eq!(differs["verdict"], "differs", "{differs}");
    assert_eq!(differs["differenceM"], 20.0);
    let silent = world.check(&vehicle, None);
    assert_eq!(silent["verdict"], "noReply");
    let none = world.check(&vehicle, Some(None));
    assert_eq!(none["verdict"], "noTerrain");
}

#[test]
fn loaded_terrain_is_checked_by_itself_once() {
    let mut world = World::new();
    let vehicle = world.vehicle();
    let item = |key: &str, value: f64, time_us: u64| TelemetryItem {
        key: key.into(),
        label: key.into(),
        unit: Unit::Count,
        value: Some(value),
        text: None,
        source: "terrain".into(),
        component_id: 1,
        time_us,
    };
    let report = |pending: f64, time_us: u64| VehicleTelemetry {
        vehicle_id: "veh-1".into(),
        positions: Vec::new(),
        items: vec![
            item("terrain.pending", pending, time_us),
            item("terrain.loaded", 56.0, time_us),
            item("home.latitude_deg", SPOT.0, time_us),
            item("home.longitude_deg", SPOT.1, time_us),
        ],
    };
    world.engine.telemetry_observed(report(12.0, 1_000_000));
    assert!(world.sent.checks().is_empty(), "not while some is pending");
    world.engine.telemetry_observed(report(0.0, 2_000_000));
    assert_eq!(world.sent.checks().len(), 1, "home");
    world.engine.telemetry_observed(report(0.0, 3_000_000));
    assert!(world.sent.checks().is_empty(), "once");
    let _ = vehicle;
}

#[test]
fn an_area_is_sized_kept_and_listed() {
    let mut world = World::new();
    let tile = json!({ "southDeg": 53.0, "westDeg": -5.0, "northDeg": 54.0, "eastDeg": -4.0 });
    let size = world.at_with("terrain/areaSize", tile.clone());
    assert_eq!(size["result"]["chunks"], 64, "{size}");
    assert!(size["result"]["bytes"].as_u64().unwrap() > 0);

    let mut keep = tile.clone();
    keep["name"] = json!("Snowdonia");
    let area = world.call("terrain/keepArea", keep);
    let id = area["result"]["id"].as_str().unwrap().to_owned();
    world.host();
    let areas = world.call("terrain/areas", json!({}));
    let listed = &areas["result"]["areas"][0];
    assert_eq!(listed["area"]["name"], "Snowdonia");
    assert_eq!(listed["state"], "kept", "{areas}");
    assert_eq!(listed["done"], 64);
    assert!(areas["result"]["manifestCheckedAtUs"].as_u64().is_some());
    assert_eq!(
        world.store.keys().filter(|k| k.ends_with(".uxdt")).count(),
        64
    );

    let too_big = world.call(
        "terrain/keepArea",
        json!({ "name": "Europe", "southDeg": 35.0, "westDeg": -10.0, "northDeg": 60.0, "eastDeg": 30.0 }),
    );
    assert_eq!(too_big["error"]["data"]["kind"], "invalid-value");

    let removed = world.call("terrain/removeArea", json!({ "id": id }));
    assert!(removed.get("result").is_some(), "{removed}");
    let areas = world.call("terrain/areas", json!({}));
    assert_eq!(areas["result"]["areas"].as_array().unwrap().len(), 0);
}

#[test]
fn a_mission_flown_above_terrain_waits_for_its_terrain() {
    let mut world = World::new();
    let link = world.call(
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": 47_999 } }),
    );
    let link_id = link["result"]["id"].as_str().unwrap().to_owned();
    world.engine.vehicle_observed(Observation {
        id: "veh-1".into(),
        aliases: Vec::new(),
        link_id,
        peer: "127.0.0.1:5760".into(),
        system_id: 1,
        component_ids: vec![1],
        autopilot: "Ardupilotmega".into(),
        vehicle_type: "Quadrotor".into(),
        uid: None,
        firmware: None,
        firmware_commit: None,
        presence: Presence::Online,
        last_heartbeat_us: 5,
        armed: false,
        mode: None,
    });
    let item = |lat: f64, lon: f64, frame: u8| {
        json!({
            "command": 16, "frame": frame, "param1": 0.0, "param2": 0.0, "param3": 0.0, "param4": 0.0,
            "latitudeDeg": lat, "longitudeDeg": lon, "altitudeM": 60.0, "autocontinue": true
        })
    };
    // Home, then a 20 km leg above terrain across the fixture's tile.
    let items = json!([
        item(53.5, -4.9, 0),
        item(53.5, -4.9, 10),
        item(53.5, -4.6, 10)
    ]);
    let mut version = 0;
    let id = loop {
        let id = world.send(
            "mission/upload",
            &json!({ "vehicleId": "veh-1", "expectedVersion": version, "items": items }),
        );
        match world.reply(id) {
            Some(reply) if reply["error"]["data"]["kind"] == "version-conflict" => {
                version = reply["error"]["data"]["currentVersion"].as_u64().unwrap();
            }
            Some(reply) => panic!("{reply}"),
            None => break id,
        }
    };
    let uploads = |world: &World| {
        world
            .sent
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|o| matches!(o, TransferOrder::UploadMission { .. }))
            .count()
    };
    assert_eq!(uploads(&world), 0, "it waits for its terrain");
    assert!(
        world.engine.state().missions["veh-1"]
            .detail
            .as_deref()
            .is_some_and(|d| d.contains("terrain")),
    );
    world.host();
    assert_eq!(uploads(&world), 1, "then it goes");
    // Chunks between the waypoints were kept, not only at them.
    let kept: Vec<&String> = world
        .store
        .keys()
        .filter(|k| k.ends_with(".uxdt"))
        .collect();
    assert!(kept.len() >= 6, "{kept:?}");
    let _ = id;
}
