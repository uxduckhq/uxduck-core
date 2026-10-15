//! The VTOL's "Get started" guide (S117) flown end to end on `ArduPlane`'s
//! quadplane SITL, as Start flying starts it: in the air, each step ticked
//! by the engine from what was sent and what the vehicle then did.
use super::*;

/// The guide's attempt on the vehicle, read as the CLI reads it.
fn guide_attempt(daemon: &Daemon, vehicle: &str) -> Value {
    rpc(daemon, "practice/status", &json!({ "vehicleId": vehicle }))["attempt"].clone()
}

/// Waits up to `seconds` for the guide to reach a step, or to pass.
async fn until_step(daemon: &Daemon, vehicle: &str, step: u64, seconds: u64) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        let attempt = guide_attempt(daemon, vehicle);
        let passed = attempt["state"] == "passed";
        if attempt["step"].as_u64().unwrap_or(0) >= step || passed {
            return;
        }
        assert_eq!(attempt["state"], "running", "{attempt}");
        assert!(
            Instant::now() < deadline,
            "step {step} not reached: {attempt}, vehicle {:?}",
            vehicles(daemon)
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn send(daemon: &Daemon, vehicle: &str, command: &Value) {
    let receipt = rpc(
        daemon,
        "command/send",
        &json!({ "vehicleId": vehicle, "command": command, "confirm": true }),
    );
    assert_eq!(receipt["status"], "accepted", "{receipt}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_vtol_guide_is_flown_end_to_end_on_a_simulated_quadplane() {
    native_simulator("arduplane")
        .expect("required ArduPlane SITL build missing; run scripts/sitl.sh plane");
    let daemon = Daemon::start(&["--no-gamepads"]);
    stdout_json(&daemon.cli(&[
        "sim",
        "start",
        "--vehicle",
        "vtol",
        "--in-air",
        "60",
        "--speedup",
        "5",
    ]));
    // Flying: the take-off holds it in GUIDED at its height.
    let deadline = Instant::now() + Duration::from_secs(180);
    let vehicle = loop {
        let flying = vehicles(&daemon)
            .into_iter()
            .find(|v| v["presence"] == "online" && v["armed"] == true && v["mode"] == "GUIDED");
        if let Some(v) = flying {
            let id = v["id"].as_str().unwrap().to_owned();
            let height = rpc(&daemon, "telemetry/get", &json!({ "id": id }))["positions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|p| p["source"] == "fused")
                .flat_map(|p| p["altitudes"].as_array().cloned().unwrap_or_default())
                .find(|a| a["frame"] == "above-home")
                .and_then(|a| a["meters"].as_f64());
            if height.is_some_and(|h| h > 50.0) {
                break id;
            }
        }
        assert!(Instant::now() < deadline, "the VTOL never flew");
        tokio::time::sleep(Duration::from_millis(250)).await;
    };

    let started = rpc(&daemon, "practice/guide", &json!({ "vehicleId": vehicle }));
    assert_eq!(started["exerciseId"], "guide-vtol", "{started}");
    assert_eq!(started["state"], "running");
    assert_eq!(
        rpc(&daemon, "practice/guide", &json!({ "vehicleId": vehicle }))["id"],
        started["id"],
        "asked again, the same guide"
    );

    // 1. Fly here: 300 m east of where it started.
    send(
        &daemon,
        &vehicle,
        &json!({
            "type": "go-to", "latitudeDeg": -35.363_261, "longitudeDeg": 149.168_53,
            "altitudeM": 60.0, "altitudeFrame": "above-home"
        }),
    );
    until_step(&daemon, &vehicle, 1, 30).await;
    // 2. LOITER from the Flight mode menu.
    send(
        &daemon,
        &vehicle,
        &json!({ "type": "set-mode", "mode": "LOITER" }),
    );
    until_step(&daemon, &vehicle, 2, 30).await;
    // 3. Return to launch: the vehicle reports RTL.
    send(&daemon, &vehicle, &json!({ "type": "rtl" }));
    until_step(&daemon, &vehicle, 3, 30).await;
    // 4. Land: QLAND, down and disarmed.
    send(&daemon, &vehicle, &json!({ "type": "land" }));
    until_step(&daemon, &vehicle, 4, 180).await;
    let ended = guide_attempt(&daemon, &vehicle);
    assert_eq!(ended["state"], "passed", "{ended}");
    assert_eq!(ended["line"], "Done. It's back on the ground.");
    let landed = vehicles(&daemon)
        .into_iter()
        .find(|v| v["id"] == vehicle)
        .unwrap();
    assert_eq!(landed["mode"], "QLAND");
    assert_eq!(landed["armed"], false);
}

/// A parameter's value on the vehicle, as UXDuck last heard it.
fn parameter(daemon: &Daemon, vehicle: &str, name: &str) -> Option<f64> {
    rpc(
        daemon,
        "param/get",
        &json!({ "vehicleId": vehicle, "names": [name] }),
    )["parameters"]
        .as_array()
        .and_then(|p| p.first())
        .and_then(|p| p["value"].as_f64())
}

async fn until(daemon: &Daemon, what: &str, seconds: u64, done: impl Fn(&Daemon) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !done(daemon) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// An exercise leaves the simulated vehicle as it found it (maintainer,
/// 2026-10-13): the VTOL's "Fly there" makes Fly here hover for the
/// exercise, and given up, Fly here circles again as it does by default.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_vtol_exercise_puts_back_the_parameters_it_changed() {
    native_simulator("arduplane")
        .expect("required ArduPlane SITL build missing; run scripts/sitl.sh plane");
    let daemon = Daemon::start(&["--no-gamepads"]);
    stdout_json(&daemon.cli(&[
        "sim",
        "start",
        "--vehicle",
        "vtol",
        "--in-air",
        "60",
        "--speedup",
        "5",
    ]));
    let deadline = Instant::now() + Duration::from_secs(180);
    let vehicle = loop {
        let flying = vehicles(&daemon)
            .into_iter()
            .find(|v| v["presence"] == "online" && v["armed"] == true && v["mode"] == "GUIDED");
        if let Some(v) = flying {
            break v["id"].as_str().unwrap().to_owned();
        }
        assert!(Instant::now() < deadline, "the VTOL never flew");
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    until(&daemon, "its parameters", 120, |d| {
        parameter(d, &vehicle, "Q_GUIDED_MODE").is_some()
    })
    .await;
    let before = parameter(&daemon, &vehicle, "Q_GUIDED_MODE");
    assert_eq!(before, Some(0.0), "the default: Fly here circles");

    rpc(
        &daemon,
        "practice/start",
        &json!({ "exerciseId": "land-in-the-circle-vtol", "vehicleId": vehicle }),
    );
    until(&daemon, "the exercise to be set up", 240, |d| {
        parameter(d, &vehicle, "Q_GUIDED_MODE") == Some(1.0)
    })
    .await;
    rpc(&daemon, "practice/stop", &json!({ "vehicleId": vehicle }));
    until(&daemon, "Q_GUIDED_MODE back as it was", 60, |d| {
        parameter(d, &vehicle, "Q_GUIDED_MODE") == before
    })
    .await;
    assert_eq!(parameter(&daemon, &vehicle, "Q_RTL_MODE"), Some(0.0));
}

/// The VTOL's "Bring it home" with ArduPilot's own Return to launch
/// (maintainer, 2026-10-13): it circles over home, and Land brings it down.
/// Nothing makes Return to launch land it, as `Q_RTL_MODE` once did.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_vtols_bring_it_home_circles_home_then_lands_when_asked() {
    native_simulator("arduplane")
        .expect("required ArduPlane SITL build missing; run scripts/sitl.sh plane");
    let daemon = Daemon::start(&["--no-gamepads"]);
    stdout_json(&daemon.cli(&[
        "sim",
        "start",
        "--vehicle",
        "vtol",
        "--in-air",
        "60",
        "--speedup",
        "5",
    ]));
    let deadline = Instant::now() + Duration::from_secs(180);
    let vehicle = loop {
        let flying = vehicles(&daemon)
            .into_iter()
            .find(|v| v["presence"] == "online" && v["armed"] == true && v["mode"] == "GUIDED");
        if let Some(v) = flying {
            break v["id"].as_str().unwrap().to_owned();
        }
        assert!(Instant::now() < deadline, "the VTOL never flew");
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    rpc(
        &daemon,
        "practice/start",
        &json!({ "exerciseId": "bring-it-home-vtol", "vehicleId": vehicle }),
    );
    until(&daemon, "the exercise to start", 240, |d| {
        guide_attempt(d, &vehicle)["state"] == "running"
    })
    .await;
    assert_eq!(parameter(&daemon, &vehicle, "Q_RTL_MODE"), Some(0.0));

    // 1. Return to launch.
    send(&daemon, &vehicle, &json!({ "type": "rtl" }));
    until_step(&daemon, &vehicle, 1, 30).await;
    // 2. Over home, circling: still in RTL, never landing by itself.
    until_step(&daemon, &vehicle, 2, 240).await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    let circling = vehicles(&daemon)
        .into_iter()
        .find(|v| v["id"] == vehicle)
        .unwrap();
    assert_eq!(circling["mode"], "RTL", "{circling}");
    assert_eq!(circling["armed"], true);
    // 3. Land: down where it is, inside the Home ring.
    send(&daemon, &vehicle, &json!({ "type": "land" }));
    until_step(&daemon, &vehicle, 3, 240).await;
    let ended = guide_attempt(&daemon, &vehicle);
    assert_eq!(ended["state"], "passed", "{ended}");
}

/// The VTOL's "Fly there" with ArduPilot's own Go to (maintainer,
/// 2026-10-13): the plane circles each place it is sent, 60 m out, and the
/// rings are wide enough for the circle to count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_vtols_fly_there_counts_circling_each_ring() {
    native_simulator("arduplane")
        .expect("required ArduPlane SITL build missing; run scripts/sitl.sh plane");
    let daemon = Daemon::start(&["--no-gamepads"]);
    stdout_json(&daemon.cli(&[
        "sim",
        "start",
        "--vehicle",
        "vtol",
        "--in-air",
        "60",
        "--speedup",
        "5",
    ]));
    let deadline = Instant::now() + Duration::from_secs(180);
    let vehicle = loop {
        let flying = vehicles(&daemon)
            .into_iter()
            .find(|v| v["presence"] == "online" && v["armed"] == true && v["mode"] == "GUIDED");
        if let Some(v) = flying {
            break v["id"].as_str().unwrap().to_owned();
        }
        assert!(Instant::now() < deadline, "the VTOL never flew");
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    rpc(
        &daemon,
        "practice/start",
        &json!({ "exerciseId": "fly-there-vtol", "vehicleId": vehicle }),
    );
    until(&daemon, "the exercise to start", 240, |d| {
        guide_attempt(d, &vehicle)["state"] == "running"
    })
    .await;
    assert_eq!(parameter(&daemon, &vehicle, "Q_GUIDED_MODE"), Some(0.0));
    let rings = guide_attempt(&daemon, &vehicle)["features"]
        .as_array()
        .unwrap()
        .clone();
    for (step, ring) in rings.iter().enumerate() {
        send(
            &daemon,
            &vehicle,
            &json!({
                "type": "go-to",
                "latitudeDeg": ring["latitudeDeg"],
                "longitudeDeg": ring["longitudeDeg"],
                "altitudeM": 50.0, "altitudeFrame": "above-home"
            }),
        );
        until_step(&daemon, &vehicle, step as u64 + 1, 240).await;
    }
    let ended = guide_attempt(&daemon, &vehicle);
    assert_eq!(ended["state"], "passed", "{ended}");
    // It circled each ring rather than hovering over it.
    let flying = vehicles(&daemon)
        .into_iter()
        .find(|v| v["id"] == vehicle)
        .unwrap();
    assert_eq!(flying["mode"], "GUIDED");
}
