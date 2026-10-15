//! Answering terrain requests (S118, ADR-0026 T4) on `ArduCopter`'s SITL:
//! started with an empty terrain folder at ArduPilot's usual field, it asks
//! UXDuck for every group around it and holds them all, with the ground at
//! the height the served dataset gives.
use std::collections::{BTreeMap, HashMap};
use std::io::BufRead as _;

use uxduck_terrain::manifest::{MANIFEST, Manifest, SCHEMA, SIGNATURE, SourceFile};
use uxduck_terrain::publish::{ReleaseBuilder, TileSamples};
use uxduck_terrain::{Dataset, Tile};

use super::*;

/// A signed bucket with S36E149 flat at 584 m, as ALOS, and the public
/// key that checks it.
fn bucket() -> (HashMap<String, Vec<u8>>, String) {
    let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let mut files = HashMap::new();
    let mut builder = ReleaseBuilder::new(Dataset::Alos, "test", "2026-10-13", "test");
    let mut keep = |key: String, bytes: Vec<u8>| {
        files.insert(format!("/{key}"), bytes);
    };
    builder
        .add_tile(
            &TileSamples {
                tile: Tile::parse("S36E149").unwrap(),
                rows: 64,
                cols: 64,
                unit_dm: 10,
                samples: vec![584; 64 * 64],
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
        published_at: "2026-10-13T00:00:00Z".into(),
        current: BTreeMap::from([("alos".into(), "test".into())]),
        releases: vec![release],
        withdrawn: vec![],
    };
    let bytes = serde_json::to_vec(&manifest).unwrap();
    let signature = minisign::sign(None, &pair.sk, bytes.as_slice(), None, None)
        .unwrap()
        .into_string();
    files.insert(format!("/{MANIFEST}"), bytes);
    files.insert(format!("/{SIGNATURE}"), signature.into_bytes());
    (files, pair.pk.to_base64())
}

/// Serves the bucket over plain HTTP on a free port.
fn serve(files: HashMap<String, Vec<u8>>) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = std::io::BufReader::new(stream);
            let mut request = String::new();
            let _ = reader.read_line(&mut request);
            let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 2 {
                line.clear();
            }
            let stream = reader.get_mut();
            let _ = match files.get(&path) {
                Some(body) => stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .and_then(|()| stream.write_all(body)),
                None => stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                ),
            };
        }
    });
    port
}

fn item(telemetry: &Value, key: &str) -> Option<f64> {
    telemetry["items"]
        .as_array()?
        .iter()
        .find(|i| i["key"] == key)?["value"]
        .as_f64()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sitl_copter_with_no_terrain_gets_it_all_from_uxduck() {
    native_simulator("arducopter")
        .expect("required ArduCopter SITL build missing; run scripts/sitl.sh copter");
    let (files, key) = bucket();
    let port = serve(files);
    let url = format!("http://127.0.0.1:{port}");
    let daemon = Daemon::run_with_env(
        tempfile::tempdir().expect("temp dir"),
        &["daemon"],
        &["--no-gamepads"],
        &[("UXDUCK_TERRAIN_URL", &url), ("UXDUCK_TERRAIN_KEY", &key)],
    );
    stdout_json(&daemon.cli(&["sim", "start", "--vehicle", "multicopter", "--wipe"]));
    // 504 groups (nine blocks of 56) at 12 a second: under a minute.
    let deadline = Instant::now() + Duration::from_secs(150);
    let mut seen_pending = false;
    loop {
        let vehicle = vehicles(&daemon)
            .into_iter()
            .find(|v| v["presence"] == "online");
        if let Some(vehicle) = vehicle {
            let telemetry = rpc(&daemon, "telemetry/get", &json!({ "id": vehicle["id"] }));
            let pending = item(&telemetry, "terrain.pending");
            let loaded = item(&telemetry, "terrain.loaded");
            let ground = item(&telemetry, "terrain.altitude_msl_m");
            seen_pending |= pending.is_some_and(|p| p > 0.0);
            let done = pending == Some(0.0) && loaded.is_some_and(|l| l > 0.0);
            if done && ground.is_some_and(|g| (g - 584.0).abs() < 0.5) {
                assert!(seen_pending, "it started with no terrain");
                return;
            }
            assert!(
                Instant::now() < deadline,
                "terrain not loaded: pending {pending:?}, loaded {loaded:?}, ground {ground:?}"
            );
        }
        assert!(
            Instant::now() < deadline,
            "no vehicle: {:?}",
            vehicles(&daemon)
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
