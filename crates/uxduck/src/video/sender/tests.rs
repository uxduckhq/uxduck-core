//! The sender against the daemon's own WHEP client (`whep.rs`), in-process:
//! a session played into the hub, its estimate setting the camera through a
//! fake `MediaMTX` API, an ICE restart over PATCH that moves the video to
//! another address in the same session, and the camera put back when the
//! viewer leaves. The pictures are a recorded test pattern, fed as the
//! RTSP reader would. With `MediaMTX` and `FFmpeg` (ignored by default, as
//! the WHEP client's tests are), the sender reads a real `MediaMTX` path.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use super::mediamtx::tests::{FRONT, Fake, rpi};
use super::*;
use crate::video::whep::tests::{INTERNET, Node, RADIO, camera};

/// 160×90 at 30 pictures a second, a keyframe first: one second of
/// `FFmpeg`'s test pattern from libx264, its headers before the keyframe.
const PATTERN: &[u8] = include_bytes!("test-pattern.h264");

/// The pattern's pictures: each slice with the units before it.
fn pictures() -> Vec<(Vec<u8>, bool)> {
    let mut out = Vec::new();
    let mut unit = Vec::new();
    let mut key = false;
    for nal in uxduck_media::nal::split_annex_b(PATTERN) {
        let kind = nal[0] & 0x1f;
        unit.extend_from_slice(&[0, 0, 0, 1]);
        unit.extend_from_slice(nal);
        key |= kind == 5;
        if kind == 1 || kind == 5 {
            out.push((std::mem::take(&mut unit), key));
            key = false;
        }
    }
    out
}

/// Feeds the pattern, over and over at 30 a second, while someone watches
/// `path`, as the RTSP reader would.
fn feed(running: &Running, path: &'static str) {
    feed_unless(running, path, Arc::default());
}

/// Feeds as `feed` does, sending nothing while `paused`.
fn feed_unless(running: &Running, path: &'static str, paused: Arc<AtomicBool>) {
    let mut wanted = running.wanted.clone();
    let to = running.pictures.clone();
    tokio::spawn(async move {
        let pictures = pictures();
        let mut rtp = 0u64;
        let mut timer = tokio::time::interval(Duration::from_micros(33_333));
        for (annex_b, key) in pictures.iter().cycle() {
            timer.tick().await;
            if !wanted.borrow_and_update().contains(path) {
                if wanted.changed().await.is_err() {
                    return;
                }
                continue;
            }
            rtp += 3000;
            if paused.load(Ordering::SeqCst) {
                continue;
            }
            let frame = Frame {
                annex_b: annex_b.clone(),
                rtp,
                captured: SystemTime::now(),
                key: *key,
            };
            if to.send((path.to_owned(), frame)).await.is_err() {
                return;
            }
        }
    });
}

async fn sender(fake: &Fake, state: &std::path::Path) -> Running {
    start(Options {
        api: Api::new(&fake.url).unwrap(),
        hosts: Hosts::Addresses(vec![RADIO.into(), INTERNET.into()]),
        whep: SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
        udp_port: 0,
        state_file: state.join("video-sender.json"),
        cameras: vec![CameraConfig {
            path: "cam".into(),
            floor_bitrate: FLOOR,
            follow: true,
        }],
        recording: watch::channel(BTreeMap::new()).1,
    })
    .await
    .unwrap()
}

fn whep(ip: Ipv4Addr, running: &Running) -> String {
    format!("http://{ip}:{}/cam/whep", running.http.port())
}

async fn status(running: &Running) -> serde_json::Value {
    reqwest::get(format!("http://127.0.0.1:{}/", running.http.port()))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Under this many bytes from an address is ICE checking a pair, not
/// video: one picture of the pattern is about 800 bytes.
const CHECKS_ONLY: u64 = 10_000;

/// Bytes the sender sent from each address, by its IP.
fn sent(status: &serde_json::Value, ip: Ipv4Addr) -> u64 {
    status["addresses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| {
            a["address"]
                .as_str()
                .unwrap()
                .starts_with(&format!("{ip}:"))
        })
        .unwrap()["sentBytes"]
        .as_u64()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_daemon_plays_the_sender_whose_estimate_sets_the_camera_until_it_leaves() {
    let fake = Fake::start(&[("cam", rpi(FRONT))]).await;
    let dir = tempfile::tempdir().unwrap();
    let running = sender(&fake, dir.path()).await;
    feed(&running, "cam");
    // The configured settings go back when the sender starts.
    assert_eq!(
        fake.wait_for(1, Duration::from_secs(3)).await,
        [("cam".to_owned(), FRONT)]
    );

    let camera = camera(&[whep(RADIO, &running)]);
    tokio::time::sleep(Duration::from_secs(3)).await;
    let from = Instant::now();
    tokio::time::sleep(Duration::from_secs(5)).await;
    let pictures = camera.pictures.since(from);
    let keyframes = pictures.iter().filter(|(_, key, _)| *key).count();
    println!(
        "{} pictures and {keyframes} keyframes in 5 s",
        pictures.len()
    );
    assert!(pictures.len() >= 130, "{} pictures in 5 s", pictures.len());
    assert!((4..=6).contains(&keyframes), "{keyframes} keyframes in 5 s");

    let now = status(&running).await;
    println!("{now:#}");
    let cam = &now["cameras"][0];
    assert_eq!(cam["viewers"], 1);
    assert!(cam["estimate"].as_u64().unwrap() > 0, "{cam}");
    // The estimate set the camera: 85% of it, never above its bitrate.
    let patches = fake.patches();
    let followed: Vec<&Settings> = patches.iter().skip(1).map(|(_, s)| s).collect();
    assert!(!followed.is_empty(), "{patches:?}");
    assert!(
        followed
            .iter()
            .all(|s| s.bitrate <= FRONT.bitrate && s.idr == 30),
        "{patches:?}"
    );
    assert_eq!(
        cam["following"]["bitrate"].as_u64(),
        followed.last().map(|s| s.bitrate),
        "{cam}"
    );
    let other = sent(&now, INTERNET);
    assert!(
        other < CHECKS_ONLY,
        "{other} bytes of video on the answer's other address"
    );

    // The viewer leaves: its session deleted, the camera put back.
    let count = fake.patches().len();
    camera.worker.task.abort();
    let patches = fake.wait_for(count + 1, Duration::from_secs(3)).await;
    assert_eq!(patches.last().unwrap(), &("cam".to_owned(), FRONT));
    assert_eq!(status(&running).await["cameras"][0]["viewers"], 0);
    assert!(
        running.wanted.borrow().is_empty(),
        "nothing read with nobody watching"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_patch_restarts_ice_and_moves_the_video_to_the_new_address_in_the_same_session() {
    let fake = Fake::start(&[("cam", rpi(FRONT))]).await;
    let dir = tempfile::tempdir().unwrap();
    let running = sender(&fake, dir.path()).await;
    feed(&running, "cam");
    let camera = camera(&[whep(RADIO, &running), whep(INTERNET, &running)]);
    tokio::time::sleep(Duration::from_secs(3)).await;
    let before = status(&running).await;
    assert!(sent(&before, RADIO) > 50_000);
    assert!(sent(&before, INTERNET) < CHECKS_ONLY);
    let last_before = camera
        .pictures
        .since(Instant::now().checked_sub(Duration::from_secs(1)).unwrap())
        .last()
        .map(|(_, _, t)| *t)
        .unwrap();

    // The vehicle moved to the internet: the daemon restarts ICE there.
    let moved = Instant::now();
    camera.worker.steer(1);
    tokio::time::sleep(Duration::from_secs(4)).await;
    let after = status(&running).await;
    println!("{after:#}");
    assert_eq!(after["cameras"][0]["viewers"], 1, "the same session");
    let radio_then = sent(&after, RADIO);
    let internet_then = sent(&after, INTERNET);
    assert!(
        internet_then > 50_000,
        "{internet_then} bytes on the internet"
    );
    let pictures = camera.pictures.since(moved + Duration::from_secs(2));
    assert!(
        pictures.len() >= 50,
        "{} pictures in the 2 s after",
        pictures.len()
    );
    // One session: its pictures' times go on from before the move.
    let first_after = pictures.first().map(|(_, _, t)| *t).unwrap();
    assert!(first_after > last_before && first_after - last_before < 5_000_000);
    // Nothing more on the radio's address: the old pair is gone.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let later = status(&running).await;
    assert!(
        sent(&later, RADIO) - radio_then < CHECKS_ONLY,
        "{} more bytes on the radio",
        sent(&later, RADIO) - radio_then
    );
    assert!(sent(&later, INTERNET) > internet_then);
}

/// The camera's pictures stop for 8 s while its WHEP session holds and the
/// sender still sends its reports, as when the link is under the sender's
/// floor and its picture runs late (ADR-0064 W7). The daemon keeps the
/// session and never asks the RTSP address after it on the same host, the
/// sender says why at the root of its WHEP port, and the pictures come back
/// in the same session.
#[tokio::test(flavor = "multi_thread")]
async fn a_whep_picture_that_runs_late_keeps_its_session_and_the_sender_says_why() {
    let fake = Fake::start(&[("cam", rpi(FRONT))]).await;
    let dir = tempfile::tempdir().unwrap();
    let running = start(Options {
        api: Api::new(&fake.url).unwrap(),
        hosts: Hosts::Addresses(vec![RADIO.into()]),
        whep: SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
        udp_port: 0,
        state_file: dir.path().join("video-sender.json"),
        // Above any estimate on loopback, so the sender's note is on.
        cameras: vec![CameraConfig {
            path: "cam".into(),
            floor_bitrate: 1_000_000_000,
            follow: true,
        }],
        recording: watch::channel(BTreeMap::new()).1,
    })
    .await
    .unwrap();
    let paused = Arc::new(AtomicBool::new(false));
    feed_unless(&running, "cam", paused.clone());
    let rtsp = std::net::TcpListener::bind((RADIO, 0)).unwrap();
    let rtsp_port = rtsp.local_addr().unwrap().port();
    let asked = Arc::new(AtomicUsize::new(0));
    let counted = asked.clone();
    std::thread::spawn(move || {
        for stream in rtsp.incoming() {
            counted.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    });
    let camera = camera(&[
        whep(RADIO, &running),
        format!("rtsp://{RADIO}:{rtsp_port}/cam"),
    ]);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        camera
            .pictures
            .since(Instant::now().checked_sub(Duration::from_secs(2)).unwrap())
            .len()
            >= 40
    );

    paused.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(8)).await;
    assert_eq!(asked.load(Ordering::SeqCst), 0, "RTSP was asked");
    assert_eq!(status(&running).await["cameras"][0]["viewers"], 1);
    let note = crate::video::whep::sender_note(
        &crate::video::whep::address(&whep(RADIO, &running)).unwrap(),
    )
    .await;
    println!("the sender says: {note:?}");
    assert!(
        note.as_deref()
            .is_some_and(|n| n
                .contains("under this camera's lowest 1000000 kbit/s, so its picture runs late.")),
        "{note:?}"
    );

    paused.store(false, Ordering::SeqCst);
    let resumed = Instant::now();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let after = camera
        .pictures
        .since(resumed + Duration::from_secs(1))
        .len();
    assert!(after >= 40, "{after} pictures in the 2 s after");
    assert_eq!(asked.load(Ordering::SeqCst), 0, "RTSP was asked");
    assert_eq!(
        status(&running).await["cameras"][0]["viewers"],
        1,
        "the same session"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn whep_requests_the_sender_cannot_take_are_refused_with_why() {
    let fake = Fake::start(&[("cam", rpi(FRONT))]).await;
    let dir = tempfile::tempdir().unwrap();
    let running = sender(&fake, dir.path()).await;
    let base = format!("http://127.0.0.1:{}", running.http.port());
    let client = reqwest::Client::new();
    let offer = |path: &str| {
        client
            .post(format!("{base}/{path}/whep"))
            .header("content-type", "application/sdp")
            .body("v=0\r\n")
    };
    let nowhere = offer("nowhere").send().await.unwrap();
    assert_eq!(nowhere.status(), 404);
    assert_eq!(
        nowhere.text().await.unwrap(),
        "There is no camera nowhere here."
    );
    assert_eq!(offer("cam").send().await.unwrap().status(), 400);
    let wrong_type = client
        .post(format!("{base}/cam/whep"))
        .body("v=0\r\n")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_type.status(), 415);
    let options = client
        .request(reqwest::Method::OPTIONS, format!("{base}/cam/whep"))
        .send()
        .await
        .unwrap();
    assert!(options.status().is_success());
    let patch = client
        .patch(format!("{base}/cam/whep/0123"))
        .header("content-type", "application/trickle-ice-sdpfrag")
        .body("a=ice-ufrag:x\r\n")
        .send()
        .await
        .unwrap();
    assert_eq!(patch.status(), 404);
    assert_eq!(
        client
            .delete(format!("{base}/cam/whep/0123"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}

#[test]
fn a_patch_takes_candidates_only_under_a_numeric_mid_and_restarts_the_offer() {
    let fragment = Fragment::read(
        "a=ice-ufrag:new\r\na=ice-pwd:secret\r\n\
         m=video 9 UDP/TLS/RTP/SAVPF 0\r\na=mid:0\r\na=candidate:1 1 udp 2130706431 10.0.0.2 5000 typ host\r\n\
         m=video 9 UDP/TLS/RTP/SAVPF 0\r\na=mid:abc\r\na=candidate:2 1 udp 2130706431 10.0.0.3 5000 typ host\r\n\
         m=video 9 UDP/TLS/RTP/SAVPF 0\r\na=mid:7\r\na=candidate:3 1 udp 2130706431 10.0.0.4 5000 typ host\r\n",
        1,
    );
    assert_eq!(fragment.ufrag.as_deref(), Some("new"));
    assert_eq!(fragment.pwd.as_deref(), Some("secret"));
    assert_eq!(
        fragment.candidates,
        ["candidate:1 1 udp 2130706431 10.0.0.2 5000 typ host"]
    );
    let offer = "v=0\r\na=group:BUNDLE x\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=mid:x\r\n\
                 a=ice-ufrag:old\r\na=ice-pwd:gone\r\na=candidate:9 1 udp 1 192.168.1.5 6000 typ host\r\n\
                 a=end-of-candidates\r\na=recvonly\r\n";
    let restarted = restart_offer(offer, "new", "secret", &fragment.candidates);
    assert_eq!(
        restarted,
        "v=0\r\na=group:BUNDLE x\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=mid:x\r\n\
         a=ice-ufrag:new\r\na=ice-pwd:secret\r\na=recvonly\r\n\
         a=candidate:1 1 udp 2130706431 10.0.0.2 5000 typ host\r\n"
    );
}

#[tokio::test]
async fn a_port_in_use_stops_the_start_and_says_why() {
    let fake = Fake::start(&[("cam", rpi(FRONT))]).await;
    let dir = tempfile::tempdir().unwrap();
    let taken = std::net::UdpSocket::bind((RADIO, 0)).unwrap();
    let error = start(Options {
        api: Api::new(&fake.url).unwrap(),
        hosts: Hosts::Addresses(vec![RADIO.into()]),
        whep: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        udp_port: taken.local_addr().unwrap().port(),
        state_file: dir.path().join("video-sender.json"),
        cameras: vec![CameraConfig {
            path: "cam".into(),
            floor_bitrate: FLOOR,
            follow: true,
        }],
        recording: watch::channel(BTreeMap::new()).1,
    })
    .await
    .err()
    .unwrap();
    assert!(
        error.contains("Is MediaMTX's own WebRTC still on?"),
        "{error}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MediaMTX and FFmpeg: MEDIAMTX=… cargo test -p uxduck --bin uxduck sender -- --ignored"]
async fn the_sender_reads_a_mediamtx_path_over_rtsp_and_the_daemon_plays_it() {
    let node = Node::start(None).await;
    let dir = tempfile::tempdir().unwrap();
    let running = start(Options {
        api: Api::new(&format!("http://127.0.0.1:{}", node.api)).unwrap(),
        hosts: Hosts::Addresses(vec![RADIO.into()]),
        whep: SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
        udp_port: 0,
        state_file: dir.path().join("video-sender.json"),
        cameras: vec![CameraConfig {
            path: "cam".into(),
            floor_bitrate: FLOOR,
            follow: true,
        }],
        recording: watch::channel(BTreeMap::new()).1,
    })
    .await
    .unwrap();
    tokio::spawn(read(
        format!("rtsp://127.0.0.1:{}", node.rtsp),
        running.wanted.clone(),
        running.pictures.clone(),
    ));
    let camera = camera(&[whep(RADIO, &running)]);
    tokio::time::sleep(Duration::from_secs(4)).await;
    let from = Instant::now();
    tokio::time::sleep(Duration::from_secs(5)).await;
    let pictures = camera.pictures.since(from);
    let keyframes = pictures.iter().filter(|(_, key, _)| *key).count();
    println!(
        "{} pictures and {keyframes} keyframes in 5 s from MediaMTX",
        pictures.len()
    );
    assert!(pictures.len() >= 140, "{} pictures in 5 s", pictures.len());
    assert!((4..=6).contains(&keyframes), "{keyframes} keyframes in 5 s");
    let now = status(&running).await;
    println!("{now:#}");
    // A publisher's path is not a Raspberry Pi camera: sent at its own rate.
    assert_eq!(now["cameras"][0]["source"], "publisher");
    assert!(now["cameras"][0]["following"].is_null());
    assert!(now["cameras"][0]["readBitrate"].as_u64().unwrap() > 500_000);
}
