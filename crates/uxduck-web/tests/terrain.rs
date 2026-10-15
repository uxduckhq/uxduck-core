//! Terrain in the browser's daemon (S23, ADR-0026 T3): the worker fetches
//! and keeps the files in the Origin Private File System, the daemon checks
//! them, and a place fetched once answers offline.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::needless_pass_by_value,
    reason = "test scenarios"
)]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use uxduck_engine::runtime::{Clock, Random};
use uxduck_mavlink::sim::FakeVehicle;
use uxduck_terrain::manifest::{MANIFEST, Manifest, SCHEMA, SIGNATURE, SourceFile};
use uxduck_terrain::publish::{ReleaseBuilder, TileSamples};
use uxduck_terrain::{Dataset, Tile};
use uxduck_web::root::{Browser, OFFLINE, Output, Store};

/// The page's clock, moved on by the test.
#[derive(Clone)]
struct Fixed(Arc<AtomicU64>);

impl Default for Fixed {
    fn default() -> Self {
        Self(Arc::new(AtomicU64::new(1_700_000_000_000_000)))
    }
}

impl Clock for Fixed {
    fn now_us(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct Counter(u8);

impl Random for Counter {
    fn fill(&mut self, bytes: &mut [u8]) {
        for byte in bytes {
            self.0 = self.0.wrapping_add(1);
            *byte = self.0;
        }
    }
}

const URL: &str = "https://terrain.example";

/// A signed bucket with one 16 × 16 ALOS tile: 580 m plus the row.
fn bucket() -> (HashMap<String, Vec<u8>>, String) {
    bucket_at("N53W005", |i| 580 + i / 16)
}

fn bucket_at(tile: &str, height: impl Fn(i16) -> i16) -> (HashMap<String, Vec<u8>>, String) {
    let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let mut files = HashMap::new();
    let mut builder = ReleaseBuilder::new(Dataset::Alos, "2026-10-14", "2026-10-14", "test");
    let mut keep = |key: String, bytes: Vec<u8>| {
        files.insert(format!("{URL}/{key}"), bytes);
    };
    builder
        .add_tile(
            &TileSamples {
                tile: Tile::parse(tile).unwrap(),
                rows: 16,
                cols: 16,
                unit_dm: 10,
                samples: (0..256).map(height).collect(),
            },
            SourceFile {
                name: "test".into(),
                sha256: "0".repeat(64),
            },
            &mut keep,
        )
        .unwrap();
    let release = builder.finish(&mut keep);
    let manifest = Manifest {
        schema: SCHEMA,
        published_at: "2026-10-14T00:00:00Z".into(),
        current: BTreeMap::from([("alos".into(), "2026-10-14".into())]),
        releases: vec![release],
        withdrawn: vec![],
    };
    let bytes = serde_json::to_vec(&manifest).unwrap();
    let signature = minisign::sign(None, &pair.sk, bytes.as_slice(), None, None)
        .unwrap()
        .into_string();
    files.insert(format!("{URL}/{MANIFEST}"), bytes);
    files.insert(format!("{URL}/{SIGNATURE}"), signature.into_bytes());
    (files, pair.pk.to_base64())
}

/// The worker: the network, the Origin Private File System and the page.
struct Worker {
    network: HashMap<String, Vec<u8>>,
    online: bool,
    opfs: HashMap<String, Vec<u8>>,
    posts: Vec<Value>,
    /// Bytes for the simulator's serial port.
    written: Vec<Vec<u8>>,
    clock: Fixed,
}

impl Worker {
    fn handle(&mut self, browser: &mut Browser, outputs: Vec<Output>) {
        let mut queue = outputs;
        while !queue.is_empty() {
            let mut next = Vec::new();
            for output in queue {
                match output {
                    Output::Fetch { request, url } => {
                        let result = if self.online {
                            self.network
                                .get(&url)
                                .cloned()
                                .ok_or(format!("cannot fetch {url}: 404"))
                        } else {
                            Err(format!("{OFFLINE}cannot fetch {url}: Failed to fetch"))
                        };
                        next.extend(browser.fetched(request, result));
                    }
                    Output::Store(Store::Read { request, key }) => {
                        let bytes = self.opfs.get(&key).cloned();
                        next.extend(browser.stored(request, bytes));
                    }
                    Output::Store(Store::Write { key, bytes }) => {
                        self.opfs.insert(key, bytes);
                    }
                    Output::Store(Store::Remove { prefix }) => {
                        self.opfs.retain(|k, _| !k.starts_with(&prefix));
                    }
                    Output::Post { text, .. } => {
                        self.posts.push(serde_json::from_str(&text).unwrap());
                    }
                    Output::LinkWrite { bytes, .. } => self.written.push(bytes),
                    _ => {}
                }
            }
            queue = next;
        }
    }

    fn start(&mut self, key: &str) -> (Browser, u32) {
        let (mut browser, outputs) = Browser::new(
            "0.1.0",
            Box::new(self.clock.clone()),
            Box::new(self.clock.clone()),
            Box::new(Counter(0)),
            None,
            "",
        )
        .unwrap();
        self.handle(&mut browser, outputs);
        let outputs = browser.terrain_from(URL, Some(key.into()));
        self.handle(&mut browser, outputs);
        let (connection, outputs) = browser.connect();
        self.handle(&mut browser, outputs);
        self.call(
            &mut browser,
            connection,
            1,
            "initialize",
            json!({
                "clientInfo": { "name": "web", "version": "0.1.0", "kind": "operator-ui" },
                "capabilities": { "protocolVersion": 1 }
            }),
        );
        (browser, connection)
    }

    fn call(
        &mut self,
        browser: &mut Browser,
        connection: u32,
        id: i64,
        method: &str,
        params: Value,
    ) -> Value {
        let text = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let outputs = browser.receive(connection, &text.to_string());
        self.handle(browser, outputs);
        self.posts
            .iter()
            .rev()
            .find(|p| p["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("no answer to {method}"))
    }
}

#[test]
fn heights_are_fetched_kept_and_answered_offline() {
    let (network, key) = bucket();
    let mut worker = Worker {
        network,
        online: true,
        opfs: HashMap::new(),
        posts: vec![],
        written: vec![],
        clock: Fixed::default(),
    };
    let (mut browser, connection) = worker.start(&key);
    let at = json!({ "latitudeDeg": 54.0 - 2.5 / 16.0, "longitudeDeg": -4.5 });
    let reply = worker.call(&mut browser, connection, 2, "terrain/at", at.clone());
    assert_eq!(reply["result"]["heightM"], 582.5, "{reply}");
    assert!(worker.opfs.contains_key(MANIFEST));
    assert!(worker.opfs.contains_key("alos/2026-10-14/N53W005/14.uxdt"));

    // The next visit, offline: the same place answers from what was kept.
    worker.online = false;
    let (mut browser, connection) = worker.start(&key);
    let reply = worker.call(&mut browser, connection, 2, "terrain/at", at);
    assert_eq!(reply["result"]["heightM"], 582.5, "{reply}");
    let never = worker.call(
        &mut browser,
        connection,
        3,
        "terrain/at",
        json!({ "latitudeDeg": 53.1, "longitudeDeg": -4.9 }),
    );
    assert_eq!(never["result"]["missing"], "offline", "{never}");
}

#[test]
fn a_browser_simulation_gets_its_terrain_too() {
    // ArduPilot's usual field, flat at 584 m.
    let (network, key) = bucket_at("S36E149", |_| 584);
    let mut worker = Worker {
        network,
        online: true,
        opfs: HashMap::new(),
        posts: vec![],
        written: vec![],
        clock: Fixed::default(),
    };
    let (mut browser, connection) = worker.start(&key);
    let place = json!({
        "type": "simulator",
        "vehicle": "multicopter",
        "latitudeDeg": -35.363_261,
        "longitudeDeg": 149.165_23,
        "altitudeM": 584.0,
        "headingDeg": 353.0
    });
    let link = worker.call(
        &mut browser,
        connection,
        2,
        "link/add",
        json!({ "kind": place }),
    );
    let link_id = link["result"]["id"].as_str().unwrap().to_owned();
    let outputs = browser.link_status(&link_id, true, None);
    worker.handle(&mut browser, outputs);
    let mut vehicle = FakeVehicle::new(1, 0x1234_abcd);
    vehicle.terrain = Some(uxduck_mavlink::fake_terrain::FakeTerrain::default());
    vehicle.on_ground = true;
    let start_us = worker.clock.0.load(Ordering::SeqCst);
    // Ten seconds of the worker's beat, the vehicle talking at 10 Hz.
    for tick in 1..=100u64 {
        worker
            .clock
            .0
            .store(start_us + tick * 100_000, Ordering::SeqCst);
        #[allow(clippy::cast_precision_loss, reason = "seconds of a test")]
        let t_s = tick as f64 / 10.0;
        let mut frames = vec![vehicle.heartbeat()];
        frames.extend(vehicle.telemetry(t_s));
        for frame in frames {
            let outputs = browser.link_received(&link_id, &frame);
            worker.handle(&mut browser, outputs);
        }
        let outputs = browser.tick();
        worker.handle(&mut browser, outputs);
        for bytes in std::mem::take(&mut worker.written) {
            for reply in vehicle.answer(&bytes) {
                let outputs = browser.link_received(&link_id, &reply);
                worker.handle(&mut browser, outputs);
            }
        }
    }
    let terrain = vehicle.terrain.as_ref().unwrap();
    assert_eq!(
        terrain.counts(-353_632_610, 1_491_652_300).0,
        0,
        "every group came"
    );
    assert_eq!(terrain.height(-353_632_610, 1_491_652_300), Some(584.0));
}
