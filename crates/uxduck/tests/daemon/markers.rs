//! Markers (S177, ADR-0035) through the actual daemon and its CLI, kept
//! across a restart.

use super::*;

fn text(out: &Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn the_cli_adds_changes_and_removes_markers_and_a_restart_keeps_them() {
    let daemon = Daemon::start(&["--no-gamepads"]);
    assert_eq!(
        text(&daemon.cli(&["marker", "list"])).trim(),
        "No markers yet."
    );

    let added = text(&daemon.cli(&[
        "marker",
        "add",
        "--lat",
        "-35.36",
        "--lon",
        "149.16",
        "--look",
        "flag",
        "--name",
        "Launch site",
        "--heading",
        "270",
    ]));
    assert!(added.starts_with("Launch site (mk"), "{added}");
    assert!(
        added.contains("flag · -35.3600000, 149.1600000 · facing 270°"),
        "{added}"
    );

    let turned = text(&daemon.cli(&[
        "marker",
        "set",
        "launch site",
        "--no-heading",
        "--look",
        "boat",
    ]));
    assert!(turned.contains("boat"), "{turned}");
    assert!(!turned.contains("facing"), "{turned}");
    let missing = daemon.cli(&["marker", "remove", "Nowhere"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("no marker named Nowhere"));
    text(&daemon.cli(&["marker", "add", "--lat", "51.5", "--lon", "-0.12"]));

    // Writes reach the OS at the end of each batch; give it one.
    std::thread::sleep(Duration::from_millis(300));
    let daemon = daemon.kill_and_restart(&["--no-gamepads"]);
    let listed = rpc(&daemon, "marker/list", &json!({}));
    let markers = listed["markers"].as_array().unwrap();
    assert_eq!(markers.len(), 2);
    assert_eq!(markers[0]["name"], "Launch site");
    assert_eq!(markers[0]["look"], "boat");
    assert_eq!(markers[0]["version"], 2);
    assert!(markers[0].get("headingDeg").is_none());
    assert_eq!(markers[1]["name"], "Marker", "named after its look");

    let removed = text(&daemon.cli(&["marker", "remove", "Launch site"]));
    assert_eq!(removed.trim(), "Removed Launch site.");
    assert_eq!(
        rpc(&daemon, "marker/list", &json!({}))["markers"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

/// The sentence with its checksum, as a receiver sends it.
fn nmea(body: &str) -> String {
    format!("${body}*{:02X}\r\n", body.bytes().fold(0u8, |a, b| a ^ b))
}

#[test]
fn a_marker_follows_nmea_sent_over_udp_and_is_kept_where_last_seen() {
    let daemon = Daemon::start(&["--no-gamepads"]);
    let port = free_udp_port();
    let marker = rpc(
        &daemon,
        "marker/add",
        &json!({ "name": "Boat", "look": "boat", "latitudeDeg": 0.0, "longitudeDeg": 0.0,
                 "source": { "type": "udp", "port": port } }),
    );
    let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let datagram = format!(
        "{}{}",
        nmea("HEHDT,045.0,T"),
        nmea("GPGGA,120000,5030.000,N,00100.000,W,1,10,0.8,3.0,M,47.0,M,,")
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let live = loop {
        sender
            .send_to(datagram.as_bytes(), ("127.0.0.1", port))
            .unwrap();
        let listed = rpc(&daemon, "marker/list", &json!({}));
        let live = listed["markers"][0]["live"].clone();
        if live["state"] == "live" {
            break live;
        }
        assert!(Instant::now() < deadline, "no fix: {listed}");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(live["fix"]["latitudeDeg"], 50.5);
    assert_eq!(live["fix"]["longitudeDeg"], -1.0);
    assert_eq!(live["fix"]["headingDeg"], 45.0);
    assert_eq!(live["fix"]["satellites"], 10);

    let kept = rpc(
        &daemon,
        "marker/update",
        &json!({ "id": marker["id"], "expectedVersion": 1, "source": { "type": "manual" } }),
    );
    assert_eq!(kept["latitudeDeg"], 50.5);
    assert_eq!(kept["headingDeg"], 45.0);
}

#[cfg(unix)]
#[test]
fn a_marker_follows_a_gps_receiver_on_a_serial_port_and_says_when_it_is_unplugged() {
    use serialport::{SerialPort, TTYPort};

    // The pair is made after the daemon starts, so it can hang up.
    let daemon = Daemon::start(&["--no-gamepads"]);
    let (mut receiver, daemon_end) = TTYPort::pair().expect("a pseudo-terminal pair");
    let device = daemon_end.name().expect("a device path");
    rpc(
        &daemon,
        "marker/add",
        &json!({ "name": "Boat", "latitudeDeg": 0.0, "longitudeDeg": 0.0,
                 "source": { "type": "serial", "port": device } }),
    );
    let rmc = nmea("GPRMC,120000,A,5030.000,N,00100.000,W,6.0,123.0,211026,,");
    let live =
        |daemon: &Daemon| rpc(daemon, "marker/list", &json!({}))["markers"][0]["live"].clone();
    let deadline = Instant::now() + Duration::from_secs(15);
    let fix = loop {
        receiver.write_all(rmc.as_bytes()).unwrap();
        receiver.flush().unwrap();
        let now = live(&daemon);
        if now["state"] == "live" {
            break now["fix"].clone();
        }
        assert!(Instant::now() < deadline, "no fix: {now}");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(fix["latitudeDeg"], 50.5);
    assert_eq!(fix["headingDeg"], 123.0, "the course while moving");

    // Unplugged: failed, the last position kept.
    drop(receiver);
    drop(daemon_end);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let now = live(&daemon);
        if now["state"] == "failed" {
            assert_eq!(now["fix"]["latitudeDeg"], 50.5);
            break;
        }
        assert!(Instant::now() < deadline, "not failed: {now}");
        std::thread::sleep(Duration::from_millis(100));
    }
}
