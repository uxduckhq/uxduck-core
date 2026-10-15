//! A vehicle's files over MAVLink FTP (S121) through the real daemon: the
//! fake vehicle answers on UDP as ArduPilot does, losing every seventh FTP
//! answer, while the CLI lists, reads and downloads, the copy resumes after
//! `kill -9`, and the browser saves it once over HTTP.
use super::*;

use std::sync::Mutex;
use uxduck_mavlink::fake_files::FakeFiles;
use uxduck_mavlink::sim::FakeVehicle;

/// A fake vehicle on its own thread: heartbeats twice a second and answers
/// whatever the daemon sends, until dropped.
struct Vehicle {
    stop: Arc<AtomicBool>,
    fake: Arc<Mutex<FakeVehicle>>,
}

impl Drop for Vehicle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn fly(port: u16, mut fake: FakeVehicle) -> Vehicle {
    fake.files.drop_every = Some(7);
    // Bursts of 50, so a lost answer costs a short wait, not 2000 more.
    fake.files.burst_max = 50;
    let fake = Arc::new(Mutex::new(fake));
    let stop = Arc::new(AtomicBool::new(false));
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let target = format!("127.0.0.1:{port}");
    let (thread_fake, thread_stop) = (fake.clone(), stop.clone());
    std::thread::spawn(move || {
        let mut beat: Option<Instant> = None;
        let mut buffer = [0u8; 2048];
        while !thread_stop.load(Ordering::SeqCst) {
            if beat.is_none_or(|at| at.elapsed() >= Duration::from_millis(500)) {
                beat = Some(Instant::now());
                let heartbeat = thread_fake.lock().unwrap().heartbeat();
                let _ = socket.send_to(&heartbeat, &target);
            }
            if let Ok((len, _)) = socket.recv_from(&mut buffer) {
                let replies = thread_fake.lock().unwrap().answer(&buffer[..len]);
                for reply in replies {
                    let _ = socket.send_to(&reply, &target);
                }
            }
        }
    });
    Vehicle { stop, fake }
}

fn udp_link(daemon: &Daemon) -> u16 {
    let port = free_udp_port();
    rpc(
        daemon,
        "link/add",
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } }),
    );
    port
}

/// A GET whose body is bytes: the response's head, and its body.
fn get_bytes(daemon: &Daemon, path: &str) -> (String, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", daemon.port)).unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        daemon.port
    )
    .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(response.len());
    let head = String::from_utf8_lossy(&response[..split]).into_owned();
    let body = response.get(split + 4..).unwrap_or_default().to_vec();
    (head, body)
}

fn transfer(daemon: &Daemon, id: &str) -> Value {
    let state = rpc(
        daemon,
        "state/subscribe",
        &json!({ "entities": ["file-transfer"] }),
    );
    state["snapshot"]["fileTransfers"]
        .as_array()
        .and_then(|all| all.iter().find(|t| t["id"] == id).cloned())
        .unwrap_or_default()
}

#[test]
fn a_vehicles_files_are_listed_read_downloaded_resumed_and_saved() {
    let daemon = Daemon::start(&[]);
    let port = udp_link(&daemon);
    let vehicle = fly(port, FakeVehicle::new(5, 0x00F1_1E50));
    wait_for("the vehicle", || {
        vehicles(&daemon)
            .first()
            .is_some_and(|v| v["presence"] == "online")
    });
    let id = vehicles(&daemon)[0]["id"].as_str().unwrap().to_owned();

    let listing = rpc(
        &daemon,
        "files/list",
        &json!({ "vehicleId": id, "path": "/APM/LOGS" }),
    );
    let names: Vec<&str> = listing["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "00000001.BIN",
            "00000002.BIN",
            "00000003.BIN",
            "LASTLOG.TXT"
        ]
    );
    let uarts = rpc(
        &daemon,
        "files/read",
        &json!({ "vehicleId": id, "path": "@SYS/uarts.txt" }),
    );
    assert!(uarts["text"].as_str().unwrap().contains("SERIAL2"));

    let started = rpc(
        &daemon,
        "files/download",
        &json!({ "vehicleId": id, "path": "/APM/LOGS/00000003.BIN", "size": 60_000 }),
    );
    let download = started["id"].as_str().unwrap().to_owned();
    // Part way, UXDuck is killed; started again, it goes on from its copy.
    wait_for("some of the log", || {
        transfer(&daemon, &download)["done"].as_u64().unwrap_or(0) > 0
    });
    let daemon = daemon.kill_and_restart(&[]);
    wait_for("the download to finish", || {
        transfer(&daemon, &download)["state"] == "done"
    });
    let expected = FakeFiles::default().files["/APM/LOGS/00000003.BIN"].clone();
    let copy = daemon
        .dir()
        .join("vehicle-files")
        .join(&id)
        .join("APM/LOGS/00000003.BIN");
    assert_eq!(std::fs::read(&copy).unwrap(), expected);
    assert!(
        vehicle.fake.lock().unwrap().files.reads > 0,
        "gaps read again"
    );

    // Saved once, by the address files/save gives.
    let saved = rpc(&daemon, "files/save", &json!({ "id": download }));
    let url = saved["url"].as_str().unwrap();
    let (head, body) = get_bytes(&daemon, url);
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(
        head.contains("attachment; filename=\"00000003.BIN\""),
        "{head}"
    );
    assert_eq!(body, expected);
    let (head, _) = get_bytes(&daemon, url);
    assert!(head.starts_with("HTTP/1.1 401"), "once: {head}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parameters_written_together_go_up_as_one_file() {
    let daemon = Daemon::start(&[]);
    let port = udp_link(&daemon);
    let vehicle = fly(port, FakeVehicle::new(6, 0x00F1_1E51));
    wait_for("the vehicle", || {
        vehicles(&daemon)
            .first()
            .is_some_and(|v| v["presence"] == "online")
    });
    let id = vehicles(&daemon)[0]["id"].as_str().unwrap().to_owned();
    // The fake sends no telemetry, so no boot time: asked for once.
    rpc(&daemon, "param/refresh", &json!({ "id": id }));
    wait_for("its parameters", || {
        let state = rpc(
            &daemon,
            "state/subscribe",
            &json!({ "entities": ["parameter-set"] }),
        );
        state["snapshot"]["parameterSets"]
            .as_array()
            .is_some_and(|sets| sets.iter().any(|s| s["status"] == "synced"))
    });
    // Ten at once on one connection, as the staged bar writes them.
    let names = [
        "ANGLE_MAX",
        "BATT_CAPACITY",
        "BATT_LOW_VOLT",
        "FENCE_ALT_MAX",
        "FENCE_RADIUS",
        "LAND_SPEED",
        "PILOT_SPEED_UP",
        "RTL_ALT",
        "WPNAV_SPEED",
        "WPNAV_ACCEL",
    ];
    let cookie = paired_cookie(&daemon);
    let mut socket = daemon.websocket(Some(&cookie)).await.unwrap();
    let init = ws_call(&mut socket, 0, "initialize", operator_ui()).await;
    assert!(init.get("result").is_some(), "{init}");
    for (i, name) in names.iter().enumerate() {
        let frame = json!({ "jsonrpc": "2.0", "id": i + 1, "method": "param/set",
            "params": { "vehicleId": id, "name": name, "value": 123, "reason": "tuning", "byFile": true } });
        socket
            .send(Message::Text(frame.to_string().into()))
            .await
            .unwrap();
    }
    let mut answered = 0;
    while answered < names.len() {
        let message = tokio::time::timeout(Duration::from_secs(20), socket.next())
            .await
            .expect("answers in time");
        if let Some(Ok(Message::Text(text))) = message {
            let value: Value = serde_json::from_str(&text).unwrap();
            if value["id"].as_u64().is_some_and(|n| (1..=10).contains(&n)) {
                assert_eq!(value["result"]["status"], "accepted", "{value}");
                answered += 1;
            }
        }
    }
    let fake = vehicle.fake.lock().unwrap();
    assert_eq!(fake.files.param_uploads, 1, "all ten went up as one file");
    for name in names {
        let held = fake.parameters.iter().find(|(n, _, _)| *n == name).unwrap();
        assert!((held.1 - 123.0).abs() < f32::EPSILON, "{name}");
    }
}

/// How many of a message UXDuck has sent the vehicle, as the inspector
/// counts them.
#[cfg(unix)]
fn sent(daemon: &Daemon, id: &str, name: &str) -> u64 {
    rpc(daemon, "mavlink/inspect", &json!({ "vehicleId": id }))["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == name && m["direction"] == "sent")
        .and_then(|m| m["count"].as_u64())
        .unwrap_or(0)
}

#[cfg(unix)]
fn param_sets(daemon: &Daemon, id: &str) -> u64 {
    sent(daemon, id, "PARAM_SET")
}

/// Where a running simulation works: the folder its `sitl.log` is in, which
/// is its MAVLink FTP root.
#[cfg(unix)]
fn simulation_folder(root: &Path) -> Option<std::path::PathBuf> {
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        if path.join("sitl.log").exists() {
            return Some(path);
        }
        if path.is_dir()
            && let Some(found) = simulation_folder(&path)
        {
            return Some(found);
        }
    }
    None
}

/// Against `ArduCopter`'s own SITL (S121): its storage listed and a file
/// downloaded with the rate measured, its parameters by file, and ten
/// written together going up as one file that it applies. Prints the rates
/// for `review.md`. Needs `scripts/sitl.sh copter`; run with `--ignored`.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the ArduCopter SITL build"]
#[expect(
    clippy::too_many_lines,
    reason = "one end-to-end file-transfer and parameter flow"
)]
async fn arducopter_serves_its_files_and_takes_parameters_by_file() {
    native_simulator("arducopter")
        .expect("required ArduCopter SITL build missing; run scripts/sitl.sh copter");
    let daemon = Daemon::start(&[]);
    let started = Instant::now();
    let out = daemon.cli(&["sim", "start", "--vehicle", "multicopter"]);
    eprintln!(
        "sim start: {} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(90);
    while !vehicles(&daemon).iter().any(|v| v["presence"] == "online") {
        assert!(Instant::now() < deadline, "copter did not start");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let id = vehicles(&daemon)[0]["id"].as_str().unwrap().to_owned();
    let set = || {
        rpc(
            &daemon,
            "state/subscribe",
            &json!({ "entities": ["parameter-set", "parameter"] }),
        )
    };
    wait_for("its parameters", || {
        set()["snapshot"]["parameterSets"]
            .as_array()
            .is_some_and(|sets| sets.iter().any(|s| s["status"] == "synced"))
    });
    let state = set();
    let count = state["snapshot"]["parameters"]
        .as_array()
        .map_or(0, Vec::len);
    let with_defaults = state["snapshot"]["parameters"].as_array().map_or(0, |all| {
        all.iter().filter(|p| p.get("default").is_some()).count()
    });
    eprintln!(
        "parameters: {count}, {with_defaults} with defaults, {:.1} s from start",
        started.elapsed().as_secs_f64()
    );
    assert!(
        count > 500 && with_defaults == count,
        "by file, each with its default"
    );

    // A megabyte in its logs folder, to time.
    let folder = simulation_folder(daemon.dir()).expect("the simulation's folder");
    std::fs::create_dir_all(folder.join("logs")).unwrap();
    let big: Vec<u8> = (0..1_048_576_u32).map(|i| (i * 7 % 251) as u8).collect();
    std::fs::write(folder.join("logs/00000099.BIN"), &big).unwrap();
    let root = rpc(
        &daemon,
        "files/list",
        &json!({ "vehicleId": id, "path": "/" }),
    );
    eprintln!("root: {}", root["entries"]);
    let logs = rpc(
        &daemon,
        "files/list",
        &json!({ "vehicleId": id, "path": "logs" }),
    );
    assert!(
        logs["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "00000099.BIN" && e["size"] == 1_048_576),
        "{logs}"
    );
    let begun = Instant::now();
    let download = rpc(
        &daemon,
        "files/download",
        &json!({ "vehicleId": id, "path": "logs/00000099.BIN", "size": 1_048_576 }),
    );
    let download = download["id"].as_str().unwrap().to_owned();
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let state = rpc(
            &daemon,
            "state/subscribe",
            &json!({ "entities": ["file-transfer"] }),
        );
        let transfer = state["snapshot"]["fileTransfers"][0].clone();
        if transfer["state"] == "done" {
            break;
        }
        assert!(Instant::now() < deadline, "{transfer}");
        assert_ne!(transfer["state"], "failed", "{transfer}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let seconds = begun.elapsed().as_secs_f64();
    eprintln!(
        "1 MB downloaded over SITL's TCP link in {seconds:.1} s: {:.0} KB/s",
        1024.0 / seconds
    );
    let copy = daemon
        .dir()
        .join("vehicle-files")
        .join(&id)
        .join("logs/00000099.BIN");
    assert_eq!(std::fs::read(copy).unwrap(), big);
    let _ = download;
    // Sent to its root, then listed at once: there.
    let uploaded = rpc(
        &daemon,
        "files/upload",
        &json!({ "vehicleId": id, "path": "/hello.lua", "contentBase64": "cHJpbnQoMSkK" }),
    );
    let upload = uploaded["id"].as_str().unwrap().to_owned();
    loop {
        let state = rpc(
            &daemon,
            "state/subscribe",
            &json!({ "entities": ["file-transfer"] }),
        );
        let all = state["snapshot"]["fileTransfers"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let t = all
            .iter()
            .find(|t| t["id"] == upload.as_str())
            .cloned()
            .unwrap_or_default();
        if t["state"] == "done" {
            break;
        }
        assert_ne!(t["state"], "failed", "{t}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let root = rpc(
        &daemon,
        "files/list",
        &json!({ "vehicleId": id, "path": "/" }),
    );
    eprintln!("after upload: {}", root["entries"]);
    assert!(
        root["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "hello.lua"),
        "{root}"
    );
    let uarts = rpc(
        &daemon,
        "files/read",
        &json!({ "vehicleId": id, "path": "@SYS/uarts.txt" }),
    );
    eprintln!("@SYS/uarts.txt: {}", uarts["text"]);
    // Small files downloaded, twice each: whole every time.
    for path in ["/sitl.log", "@SYS/uarts.txt", "/sitl.log", "@SYS/uarts.txt"] {
        let listed = rpc(
            &daemon,
            "files/list",
            &json!({ "vehicleId": id, "path": if path.starts_with('@') { "@SYS" } else { "/" } }),
        );
        let size = listed["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| path.ends_with(e["name"].as_str().unwrap()))
            .and_then(|e| e["size"].as_u64());
        let started = rpc(
            &daemon,
            "files/download",
            &json!({ "vehicleId": id, "path": path, "size": size }),
        );
        let small = started["id"].as_str().unwrap().to_owned();
        let deadline = Instant::now() + Duration::from_secs(30);
        let done = loop {
            let state = rpc(
                &daemon,
                "state/subscribe",
                &json!({ "entities": ["file-transfer"] }),
            );
            let t = state["snapshot"]["fileTransfers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["id"] == small.as_str())
                .cloned()
                .unwrap();
            if t["state"] == "done" || t["state"] == "failed" {
                break t;
            }
            assert!(Instant::now() < deadline, "{t}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        eprintln!("download {path}: listed {size:?}, {done}");
        if done["done"].as_u64().unwrap() == 0 {
            for direction in ["sent", "received"] {
                let history = rpc(
                    &daemon,
                    "mavlink/history",
                    &json!({ "vehicleId": id, "direction": direction, "id": 110, "limit": 20 }),
                );
                for m in history["messages"].as_array().unwrap().iter().rev() {
                    let payload: Vec<u64> = m["fields"]["payload"]
                        .as_array()
                        .map(|a| a.iter().filter_map(serde_json::Value::as_u64).collect())
                        .unwrap_or_default();
                    if payload.len() > 12 {
                        let seq = payload[0] + payload[1] * 256;
                        let offset = payload[8]
                            + (payload[9] << 8)
                            + (payload[10] << 16)
                            + (payload[11] << 24);
                        eprintln!(
                            "{direction} at {} seq {seq} session {} op {} size {} req {} burst {} offset {offset} data0 {}",
                            m["atUs"],
                            payload[2],
                            payload[3],
                            payload[4],
                            payload[5],
                            payload[6],
                            payload[12]
                        );
                    } else {
                        eprintln!("{direction}: {m}");
                    }
                }
            }
        }
        assert!(done["done"].as_u64().unwrap() > 0, "{path}: {done}");
    }

    // Ten rate gains, 5 % up, written together on one connection: names
    // change between ArduPilot versions, so taken from what it has.
    let names: Vec<(String, f64)> = state["snapshot"]["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| {
            p["name"]
                .as_str()
                .is_some_and(|n| n.starts_with("ATC_RAT_") && n.ends_with("_P"))
                || p["name"]
                    .as_str()
                    .is_some_and(|n| n.starts_with("ATC_RAT_") && n.ends_with("_I"))
                || p["name"]
                    .as_str()
                    .is_some_and(|n| n.starts_with("ATC_RAT_") && n.ends_with("_D"))
        })
        .take(10)
        .map(|p| {
            let value = (p["value"].as_f64().unwrap() * 1.05 * 10_000.0).round() / 10_000.0;
            (p["name"].as_str().unwrap().to_owned(), value)
        })
        .collect();
    assert!(names.len() >= 8, "{names:?}");
    let param_sets_before = param_sets(&daemon, &id);
    let cookie = paired_cookie(&daemon);
    let mut socket = daemon.websocket(Some(&cookie)).await.unwrap();
    ws_call(&mut socket, 0, "initialize", operator_ui()).await;
    let begun = Instant::now();
    for (i, (name, value)) in names.iter().enumerate() {
        let frame = json!({ "jsonrpc": "2.0", "id": i + 1, "method": "param/set",
            "params": { "vehicleId": id, "name": name, "value": value, "byFile": true } });
        socket
            .send(Message::Text(frame.to_string().into()))
            .await
            .unwrap();
    }
    let mut answered = 0;
    while answered < names.len() {
        let message = tokio::time::timeout(Duration::from_secs(30), socket.next())
            .await
            .expect("answers in time");
        if let Some(Ok(Message::Text(text))) = message {
            let value: Value = serde_json::from_str(&text).unwrap();
            if value["id"]
                .as_u64()
                .is_some_and(|n| (1..=names.len() as u64).contains(&n))
            {
                assert_eq!(value["result"]["status"], "accepted", "{value}");
                answered += 1;
            }
        }
    }
    eprintln!(
        "ten parameters written and confirmed in {:.1} s",
        begun.elapsed().as_secs_f64()
    );
    // They went up as a file: no PARAM_SET was sent for them.
    let param_sets_after = param_sets(&daemon, &id);
    assert!(
        sent(&daemon, &id, "FILE_TRANSFER_PROTOCOL") > 0,
        "the inspector counts"
    );
    eprintln!("PARAM_SET sent: {param_sets_before} before the writes, {param_sets_after} after");
    assert_eq!(
        param_sets_after, param_sets_before,
        "the writes went up as a file"
    );
    // Downloaded again from the vehicle: it kept them.
    rpc(&daemon, "param/refresh", &json!({ "id": id }));
    tokio::time::sleep(Duration::from_secs(1)).await;
    wait_for("its parameters again", || {
        set()["snapshot"]["parameterSets"]
            .as_array()
            .is_some_and(|sets| sets.iter().any(|s| s["status"] == "synced"))
    });
    let got = rpc(
        &daemon,
        "param/get",
        &json!({ "vehicleId": id, "names": names.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>() }),
    );
    for (name, value) in &names {
        let held = got["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == name.as_str())
            .and_then(|p| p["value"].as_f64())
            .unwrap();
        assert!((held - value).abs() < 1e-4, "{name}: {held} not {value}");
    }
}
