//! Real `ArduRover` regression: default steering versus the old unscaled profile.
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[expect(
    clippy::too_many_lines,
    reason = "one end-to-end comparison of two rover profiles"
)]
async fn a_rovers_default_profile_softens_fast_turns_and_keeps_full_low_speed_steering() {
    native_simulator("ardurover")
        .expect("required ArduRover SITL build missing; run scripts/sitl.sh rover");
    let pad_port = free_udp_port();
    let daemon = Daemon::start(&["--no-gamepads", "--fake-controller", &pad_port.to_string()]);
    let started =
        stdout_json(&daemon.cli(&["sim", "start", "--vehicle", "rover", "--frame", "rover"]));
    let deadline = Instant::now() + Duration::from_secs(90);
    while !vehicles(&daemon).iter().any(|v| v["presence"] == "online") {
        assert!(Instant::now() < deadline, "rover did not start");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let vehicle = vehicles(&daemon)[0]["id"].as_str().unwrap().to_owned();
    let parameter = |name: &str| {
        rpc(
            &daemon,
            "param/get",
            &json!({ "vehicleId": vehicle, "names": [name] }),
        )["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == name)
            .and_then(|p| p["value"].as_f64())
    };
    wait_for("the shared simulation profile", || {
        parameter("MANUAL_STR_EXPO").is_some()
    });
    assert!((parameter("MANUAL_STR_EXPO").unwrap() - 0.35).abs() < 0.001);
    assert_eq!(parameter("MANUAL_OPTIONS"), Some(1.0));
    assert_eq!(parameter("ACRO_TURN_RATE"), Some(60.0));
    // Frame defaults still supply the calibrated steering controller.
    assert_eq!(parameter("ATC_STR_RAT_FF"), Some(0.75));

    let pad = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let move_pad = |steer: f64, throttle: f64| {
        pad.send_to(
            json!({ "id": "profile-pad", "name": "Profile pad", "axes": {
                "right-stick-x": steer, "left-stick-y": throttle
            }})
            .to_string()
            .as_bytes(),
            ("127.0.0.1", pad_port),
        )
        .unwrap();
    };
    wait_for("the centred pad", || {
        move_pad(0.0, 0.0);
        !rpc(&daemon, "controller/inputs", &json!({}))["controllers"]
            .as_array()
            .unwrap()
            .is_empty()
    });
    let cookie = paired_cookie(&daemon);
    let mut ui = daemon.websocket(Some(&cookie)).await.unwrap();
    ws_call(&mut ui, 1, "initialize", operator_ui()).await;
    let claimed = ws_call(
        &mut ui,
        2,
        "controller/claim",
        json!({
            "id": "profile-pad", "mode": "pinned", "vehicleId": vehicle
        }),
    )
    .await;
    assert!(claimed.get("result").is_some(), "{claimed}");
    // Keep reading so websocket ping/pong and the operator's lease stay live
    // throughout the drive, just as the browser reads continuously.
    let ui_reader = tokio::spawn(async move { while ui.next().await.is_some() {} });
    let deadline = Instant::now() + Duration::from_secs(90);
    while vehicles(&daemon)[0]["armed"] != true {
        move_pad(0.0, 0.0);
        let _ = rpc(
            &daemon,
            "command/send",
            &json!({ "vehicleId": vehicle, "command": { "type": "arm" } }),
        );
        assert!(Instant::now() < deadline, "rover did not arm");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let command = |mode: &str| {
        rpc(
            &daemon,
            "command/send",
            &json!({
                "vehicleId": vehicle, "command": { "type": "set-mode", "mode": mode }
            }),
        )
    };
    assert_eq!(command("MANUAL")["status"], "accepted");
    let telemetry = |key: &str| {
        rpc(&daemon, "telemetry/get", &json!({ "id": vehicle }))["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["key"] == key)
            .and_then(|i| i["value"].as_f64())
            .unwrap()
    };
    let drive = |steer: f64, throttle: f64, seconds: u64| {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < deadline {
            move_pad(steer, throttle);
            std::thread::sleep(Duration::from_millis(80));
        }
    };
    let stop = || {
        let deadline = Instant::now() + Duration::from_secs(60);
        while telemetry("speed.ground_mps") > 0.5 {
            move_pad(0.0, 0.0);
            assert!(
                Instant::now() < deadline,
                "neutral did not stop the rover: {} m/s, PWM {}",
                telemetry("speed.ground_mps"),
                telemetry("servo.3")
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        drive(0.0, 0.0, 1);
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    while telemetry("speed.ground_mps") < 2.0 {
        move_pad(0.0, 0.8);
        assert!(
            Instant::now() < deadline,
            "rover did not reach speed: {}",
            telemetry("speed.ground_mps")
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let speed = telemetry("speed.ground_mps");
    assert!(
        speed >= 2.0,
        "rover never reached test speed: {speed} m/s, throttle PWM {}, vehicle {:?}, inputs {}",
        telemetry("servo.3"),
        vehicles(&daemon),
        rpc(&daemon, "controller/inputs", &json!({}))
    );
    drive(0.25, 0.8, 2);
    let gentle = (telemetry("servo.1") - 1500.0).abs();
    assert!(
        gentle > 3.0 && gentle < 65.0,
        "profile steering PWM deflection: {gentle}"
    );
    let set = |name: &str, value: f64| {
        let result = rpc(
            &daemon,
            "param/set",
            &json!({ "vehicleId": vehicle, "name": name, "value": value }),
        );
        assert_eq!(result["status"], "accepted", "{result}");
    };
    // The same stick and speed without either improvement reproduces the sharp turn.
    set("MANUAL_STR_EXPO", 0.0);
    set("MANUAL_OPTIONS", 0.0);
    drive(0.25, 0.8, 2);
    let original = (telemetry("servo.1") - 1500.0).abs();
    eprintln!("steering comparison: profile {gentle} PWM, original {original} PWM");
    assert!(
        original > gentle * 2.0,
        "steering did not soften: original {original}, profile {gentle}"
    );

    set("MANUAL_STR_EXPO", 0.35);
    set("MANUAL_OPTIONS", 1.0);
    stop();
    drive(1.0, 0.0, 1);
    assert!(
        telemetry("servo.1") > 1950.0,
        "full low-speed steering was lost"
    );
    drive(-1.0, 0.0, 1);
    assert!(telemetry("servo.1") < 1050.0, "left steering was lost");
    let deadline = Instant::now() + Duration::from_secs(60);
    while telemetry("speed.ground_mps") < 1.0 {
        move_pad(0.0, -0.6);
        assert!(Instant::now() < deadline, "reverse was lost");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(telemetry("servo.3") < 1450.0, "reverse throttle was lost");
    stop();

    // Saved settings must survive reloading defaults at the next start.
    set("MANUAL_STR_EXPO", 0.12);
    assert_eq!(command("HOLD")["status"], "accepted");
    let before = transfers_state(&daemon)["parameterSets"][0]["bootInstantUs"].clone();
    rpc(&daemon, "sim/restart", &json!({ "id": started["id"] }));
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let state = transfers_state(&daemon);
        if state["parameterSets"][0]["status"] == "synced"
            && state["parameterSets"][0]["bootInstantUs"] != before
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "restarted rover parameters did not sync"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!((parameter("MANUAL_STR_EXPO").unwrap() - 0.12).abs() < 0.001);
    assert!(
        daemon
            .cli(&["sim", "stop", started["id"].as_str().unwrap()])
            .status
            .success()
    );
    ui_reader.abort();
}
