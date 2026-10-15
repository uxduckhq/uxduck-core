//! The serial probe over pseudo-terminal pairs (S44, ADR-0018 F5): what it
//! hears, and that it writes nothing to a device until MAVLink came from it,
//! except one heartbeat to a silent known board.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "a test")]

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serialport::{SerialPort, TTYPort};
use uxduck_mavlink::probe::{Outcome, Plan, Timing, probe};
use uxduck_mavlink::sim::FakeVehicle;

fn quick() -> Timing {
    Timing {
        listen: Duration::from_millis(300),
        known_silence: Duration::from_millis(300),
        after_heartbeat: Duration::from_millis(400),
    }
}

/// Writes `bytes` from the device's side until stopped.
fn sending(mut end: TTYPort, bytes: impl Fn() -> Vec<u8> + Send + 'static) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    std::thread::spawn(move || {
        while !stopped.load(Ordering::Relaxed) {
            let _ = end.write_all(&bytes());
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    stop
}

/// What the device's side received within a while.
fn received(end: &mut TTYPort) -> Vec<u8> {
    end.set_timeout(Duration::from_millis(200)).unwrap();
    let mut all = Vec::new();
    let mut chunk = [0u8; 512];
    while let Ok(n) = end.read(&mut chunk) {
        if n == 0 {
            break;
        }
        all.extend_from_slice(&chunk[..n]);
    }
    all
}

#[test]
fn a_radio_sending_heartbeats_is_heard_at_the_first_speed() {
    let (vehicle_end, ours) = TTYPort::pair().unwrap();
    let path = ours.name().unwrap();
    let fake = std::sync::Mutex::new(FakeVehicle::new(7, 0xAB));
    let stop = sending(vehicle_end, move || fake.lock().unwrap().heartbeat());
    let outcome = probe(
        &path,
        Plan::Adapter,
        &quick(),
        uxduck_mavlink::GCS_SYSTEM_ID,
    );
    stop.store(true, Ordering::Relaxed);
    let Outcome::Heard { baud, heard } = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(baud, 57_600);
    assert_eq!(
        (heard.system_id, heard.vehicle_type.as_str()),
        (7, "Quadrotor")
    );
}

#[test]
fn nothing_is_written_to_a_silent_device_it_does_not_know() {
    let (mut vehicle_end, ours) = TTYPort::pair().unwrap();
    let path = ours.name().unwrap();
    assert_eq!(
        probe(
            &path,
            Plan::Adapter,
            &quick(),
            uxduck_mavlink::GCS_SYSTEM_ID
        ),
        Outcome::NoMavlink
    );
    assert!(
        received(&mut vehicle_end).is_empty(),
        "the probe wrote to it"
    );
}

#[test]
fn a_stream_that_is_not_mavlink_is_no_mavlink() {
    let (vehicle_end, ours) = TTYPort::pair().unwrap();
    let path = ours.name().unwrap();
    let stop = sending(vehicle_end, || {
        b"$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*47\r\n".to_vec()
    });
    assert_eq!(
        probe(
            &path,
            Plan::Adapter,
            &quick(),
            uxduck_mavlink::GCS_SYSTEM_ID
        ),
        Outcome::NoMavlink
    );
    stop.store(true, Ordering::Relaxed);
}

#[test]
fn a_silent_known_board_gets_one_heartbeat_and_one_that_waits_for_it_answers() {
    let (mut vehicle_end, ours) = TTYPort::pair().unwrap();
    let path = ours.name().unwrap();
    assert_eq!(
        probe(
            &path,
            Plan::KnownBoard,
            &quick(),
            uxduck_mavlink::GCS_SYSTEM_ID
        ),
        Outcome::SilentKnownBoard
    );
    let sent = received(&mut vehicle_end);
    let frames = uxduck_mavlink::framing::split(&sent).frames.len();
    assert_eq!(frames, 1, "one ground-station heartbeat");

    // A board that speaks once spoken to.
    let (mut vehicle_end, ours) = TTYPort::pair().unwrap();
    let path = ours.name().unwrap();
    let answering = std::thread::spawn(move || {
        vehicle_end.set_timeout(Duration::from_millis(100)).unwrap();
        let mut fake = FakeVehicle::new(9, 0xCD);
        let mut chunk = [0u8; 512];
        for _ in 0..30 {
            if let Ok(n) = vehicle_end.read(&mut chunk)
                && n > 0
            {
                for _ in 0..5 {
                    let _ = vehicle_end.write_all(&fake.heartbeat());
                    std::thread::sleep(Duration::from_millis(30));
                }
                return;
            }
        }
    });
    let outcome = probe(
        &path,
        Plan::KnownBoard,
        &quick(),
        uxduck_mavlink::GCS_SYSTEM_ID,
    );
    let _ = answering.join();
    assert!(matches!(outcome, Outcome::Heard { .. }), "{outcome:?}");
}
