//! The WHEP client against a real `MediaMTX` (ADR-0064 S383): playing into
//! the hub, NACKs without RTX, the answer's candidates kept on the
//! address's host, an ICE restart following the vehicle, a new session and
//! then RTSP when the restart fails, a pinned certificate, and a stall
//! that moves to the RTSP address kept set up (ADR-0057 D12 and D13). A
//! squeezed link keeps the WHEP session, and a dead one falls back to RTSP
//! on its host and comes back at a keyframe.
//!
//! The `MediaMTX` tests need `MediaMTX` 1.21 (`MEDIAMTX`, else `mediamtx` on the
//! path) and an `FFmpeg` with `libx264`, so they are ignored by default:
//!
//!     MEDIAMTX=/path/to/mediamtx cargo test -p uxduck --bin uxduck whep -- --ignored
//!
//! Each loopback address stands for one connection to the vehicle: `127.0.0.2`
//! for the radio and `127.0.0.3` for the internet. `MediaMTX` listens on
//! `127.0.0.1` and names both as its `WebRTC` hosts, and each connection is a
//! relay in this test, its UDP and TCP passed on to `MediaMTX`, that can lose
//! packets or be cut as a connection is lost.

use std::net::Ipv4Addr;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use uxduck_engine::video::AddressRoute;

use super::*;
use crate::daemon::engine_task::EngineHandle;
use crate::video::hub::{Hub, Item};
use crate::video::rtsp;

pub(in crate::video) const RADIO: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 2);
pub(in crate::video) const INTERNET: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 3);

#[test]
fn a_whep_address_is_taken_apart() {
    let pin = "ab".repeat(32);
    let a = address(&format!(
        "https://viewer:p%40ss@10.0.0.5:8889/front/whep#sha256={pin}"
    ))
    .unwrap();
    assert_eq!(a.url, "https://10.0.0.5:8889/front/whep");
    assert_eq!((a.host.as_str(), a.port), ("10.0.0.5", 8889));
    assert_eq!(a.user.as_deref(), Some("viewer"));
    assert_eq!(a.password.as_deref(), Some("p@ss"));
    assert_eq!(a.pin, Some([0xab; 32]));
    let colons = vec!["AB"; 32].join(":");
    assert_eq!(
        address(&format!("https://gw/front/whep#sha256={colons}"))
            .unwrap()
            .pin,
        Some([0xab; 32])
    );
    let plain = address("http://[::1]/front/whep").unwrap();
    assert_eq!(
        (plain.host.as_str(), plain.port, plain.pin),
        ("::1", 80, None)
    );
    assert_eq!(address("https://gw/front/whep").unwrap().port, 443);
    assert!(
        address(&format!("http://gw/front/whep#sha256={pin}"))
            .unwrap_err()
            .contains("https://")
    );
    assert!(address("https://gw/front/whep#sha256=abc").is_err());
    assert!(address("rtsp://gw/front").is_err());
}

#[test]
fn only_udp_candidates_on_the_addresss_host_are_kept() {
    let answer = "v=0\r\n\
        a=ice-ufrag:x\r\n\
        a=candidate:1 1 udp 2130706431 10.0.0.5 8189 typ host\r\n\
        a=candidate:2 1 udp 2130706431 100.64.0.7 8189 typ host\r\n\
        a=candidate:3 1 tcp 2130706431 10.0.0.5 8189 typ host tcptype passive\r\n\
        a=candidate:4 1 udp 2130706431 gw.local 8189 typ host\r\n\
        a=end-of-candidates\r\n";
    let (kept, count) = on_host(answer, &["10.0.0.5".parse().unwrap()]);
    assert_eq!(count, 1);
    assert!(kept.contains("10.0.0.5 8189 typ host\r\n"));
    assert!(!kept.contains("100.64.0.7"), "{kept}");
    assert!(!kept.contains("tcp"), "{kept}");
    assert!(!kept.contains("gw.local"), "{kept}");
    assert!(kept.contains("a=ice-ufrag:x\r\n") && kept.contains("a=end-of-candidates\r\n"));
    // A host none of whose candidates is offered keeps none.
    assert_eq!(on_host(answer, &["192.168.1.9".parse().unwrap()]).1, 0);
}

pub(in crate::video) fn test_engine() -> EngineHandle {
    use crate::adapters::system::{OsRandom, SystemClock};
    use uxduck_engine::runtime::{
        Forget, MemoryCredentials, NoAgent, NoLinks, NoPlugins, NoSystem, Ports,
    };
    use uxduck_engine::{Engine, EngineConfig, HostAbout, Loaded};
    EngineHandle::spawn(Engine::new(
        EngineConfig {
            daemon_version: "0.1.1".into(),
            capabilities: uxduck_contracts::dto::initialize::DaemonCapabilities::default(),
            ui_url: "http://127.0.0.1:48091".into(),
            checkpoint_every: 1000,
            models: vec![],
            agent_provider: "fake".into(),
            about: HostAbout::default(),
        },
        Box::new(SystemClock),
        Box::new(OsRandom),
        Ports {
            persistence: Box::new(Forget),
            agent: Box::new(NoAgent),
            credentials: Box::new(MemoryCredentials::default()),
            links: Box::new(NoLinks),
            system: Box::new(NoSystem::default()),
            plugins: Box::new(NoPlugins),
        },
        Loaded::default(),
    ))
}

fn free_tcp() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn free_udp() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// `MediaMTX` with `WebRTC` on, and `FFmpeg` publishing a test picture to it over
/// RTSP: 640×360 at 30 pictures a second, a keyframe each second.
pub(in crate::video) struct Node {
    _dir: tempfile::TempDir,
    server: Child,
    publisher: Child,
    http: u16,
    udp: u16,
    pub(in crate::video) rtsp: u16,
    pub(in crate::video) api: u16,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.publisher.kill();
        let _ = self.server.kill();
        let _ = self.publisher.wait();
        let _ = self.server.wait();
    }
}

impl Node {
    /// With TLS, its certificate and key in PEM.
    #[allow(
        clippy::too_many_lines,
        reason = "one fixture: its configuration, MediaMTX and the publisher"
    )]
    pub(in crate::video) async fn start(tls: Option<(&str, &str)>) -> Self {
        let mediamtx = std::env::var("MEDIAMTX").unwrap_or_else(|_| "mediamtx".into());
        let dir = tempfile::tempdir().unwrap();
        let (http, udp, rtsp, api) = (free_tcp(), free_udp(), free_tcp(), free_tcp());
        let encryption = match tls {
            Some((cert, key)) => {
                std::fs::write(dir.path().join("server.crt"), cert).unwrap();
                std::fs::write(dir.path().join("server.key"), key).unwrap();
                "webrtcEncryption: yes\nwebrtcServerCert: server.crt\nwebrtcServerKey: server.key\n"
            }
            None => "webrtcEncryption: no\n",
        };
        std::fs::write(
            dir.path().join("mediamtx.yml"),
            format!(
                "logLevel: warn\n\
                 api: yes\n\
                 apiAddress: 127.0.0.1:{api}\n\
                 rtsp: yes\n\
                 rtspAddress: 127.0.0.1:{rtsp}\n\
                 rtspTransports: [tcp]\n\
                 rtmp: no\n\
                 hls: no\n\
                 srt: no\n\
                 moq: no\n\
                 webrtc: yes\n\
                 webrtcAddress: 127.0.0.1:{http}\n\
                 webrtcLocalUDPAddress: 127.0.0.1:{udp}\n\
                 webrtcLocalTCPAddress: ''\n\
                 webrtcIPsFromInterfaces: no\n\
                 webrtcAdditionalHosts: [{RADIO}, {INTERNET}]\n\
                 {encryption}\
                 authInternalUsers:\n\
                 \x20 - user: any\n\
                 \x20   pass:\n\
                 \x20   ips: []\n\
                 \x20   permissions:\n\
                 \x20     - action: publish\n\
                 \x20     - action: read\n\
                 \x20     - action: api\n\
                 paths:\n\
                 \x20 cam: {{}}\n\
                 \x20 all_others: {{}}\n"
            ),
        )
        .unwrap();
        // In its own folder: MediaMTX writes files into the one it runs in.
        let server = Command::new(&mediamtx)
            .arg("mediamtx.yml")
            .current_dir(dir.path())
            .stdout(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("these tests need MediaMTX ({mediamtx}): {e}"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::net::TcpStream::connect(("127.0.0.1", rtsp)).is_err() {
            assert!(Instant::now() < deadline, "MediaMTX did not start");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let publisher = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-re",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=640x360:rate=30",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-tune",
                "zerolatency",
                "-profile:v",
                "baseline",
                "-bf",
                "0",
                "-g",
                "30",
                "-b:v",
                "1500k",
                "-f",
                "rtsp",
                "-rtsp_transport",
                "tcp",
            ])
            .arg(format!("rtsp://127.0.0.1:{rtsp}/cam"))
            .stdout(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("these tests need FFmpeg with libx264: {e}"));
        let node = Self {
            _dir: dir,
            server,
            publisher,
            http,
            udp,
            rtsp,
            api,
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let ready = reqwest::get(format!("http://127.0.0.1:{api}/v3/paths/get/cam"))
                .await
                .ok()
                .filter(|r| r.status().is_success());
            if let Some(response) = ready
                && response
                    .text()
                    .await
                    .unwrap_or_default()
                    .contains("\"ready\":true")
            {
                return node;
            }
            assert!(
                Instant::now() < deadline,
                "the test picture never reached MediaMTX"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// The `WebRTC` sessions `MediaMTX` holds now.
    async fn sessions(&self) -> usize {
        self.listed("webrtcsessions").await
    }

    /// The RTSP sessions `MediaMTX` holds now, the publisher's included.
    async fn rtsp_sessions(&self) -> usize {
        self.listed("rtspsessions").await
    }

    async fn listed(&self, what: &str) -> usize {
        let listed: serde_json::Value =
            reqwest::get(format!("http://127.0.0.1:{}/v3/{what}/list", self.api))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
        listed["items"].as_array().map_or(0, Vec::len)
    }
}

/// One connection to the vehicle, as a relay: its own loopback address,
/// `MediaMTX`'s `WebRTC` port on it passed on over UDP, and its WHEP and RTSP
/// ports over TCP.
#[derive(Clone, Default)]
struct Connection {
    cut: Arc<AtomicBool>,
    /// Every this-many datagrams from `MediaMTX` over 300 bytes is lost, none at 0.
    lose_every: Arc<AtomicUsize>,
    /// Answers a PATCH with 405, as a WHEP server without ICE restart does.
    no_patch: Arc<AtomicBool>,
    datagrams: Arc<AtomicUsize>,
    /// The HTTP requests' methods, in order.
    requests: Arc<Mutex<Vec<String>>>,
    /// RTSP connections asked for, those a cut drops included.
    rtsp_connections: Arc<AtomicUsize>,
    /// WebRTC alone is down: its UDP is dropped and its HTTP refused, as
    /// when the WHEP sender restarts, while RTSP passes.
    whep_down: Arc<AtomicBool>,
    /// The bits a second `MediaMTX`'s UDP may send through, the rest dropped
    /// as a squeezed link drops them. No cap at 0.
    rate_bps: Arc<AtomicUsize>,
}

impl Connection {
    async fn open(ip: Ipv4Addr, node: &Node, whep: bool) -> Self {
        let connection = Self::default();
        let outside = UdpSocket::bind((ip, node.udp)).await.unwrap();
        let inside = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server = std::net::SocketAddr::from(([127, 0, 0, 1], node.udp));
        let relay = connection.clone();
        tokio::spawn(async move {
            let (mut up, mut down) = (vec![0u8; 2048], vec![0u8; 2048]);
            let mut client = None;
            let mut from_server = 0usize;
            // A token bucket for `rate_bps`, holding at most 100 ms of it.
            let (mut tokens, mut filled) = (0.0f64, Instant::now());
            loop {
                tokio::select! {
                    Ok((n, from)) = outside.recv_from(&mut up) => {
                        if relay.cut.load(Ordering::SeqCst) || relay.whep_down.load(Ordering::SeqCst) {
                            continue;
                        }
                        client = Some(from);
                        relay.datagrams.fetch_add(1, Ordering::SeqCst);
                        let _ = inside.send_to(&up[..n], server).await;
                    }
                    Ok((n, _)) = inside.recv_from(&mut down) => {
                        if relay.cut.load(Ordering::SeqCst) || relay.whep_down.load(Ordering::SeqCst) {
                            continue;
                        }
                        #[allow(clippy::cast_precision_loss, reason = "bytes a second")]
                        let rate = relay.rate_bps.load(Ordering::SeqCst) as f64 / 8.0;
                        if rate > 0.0 {
                            let now = Instant::now();
                            tokens = (tokens + (now - filled).as_secs_f64() * rate).min(rate / 10.0);
                            filled = now;
                            #[allow(clippy::cast_precision_loss, reason = "a datagram's size")]
                            let size = n as f64;
                            if tokens < size {
                                continue;
                            }
                            tokens -= size;
                        }
                        relay.datagrams.fetch_add(1, Ordering::SeqCst);
                        let every = relay.lose_every.load(Ordering::SeqCst);
                        if n > 300 {
                            from_server += 1;
                            if every > 0 && from_server.is_multiple_of(every) {
                                continue;
                            }
                        }
                        if let Some(client) = client {
                            let _ = outside.send_to(&down[..n], client).await;
                        }
                    }
                }
            }
        });
        if whep {
            connection.pass_tcp(ip, node.http, true).await;
        }
        connection.pass_tcp(ip, node.rtsp, false).await;
        connection
    }

    async fn pass_tcp(&self, ip: Ipv4Addr, port: u16, http: bool) {
        let listener = TcpListener::bind((ip, port)).await.unwrap();
        let relay = self.clone();
        tokio::spawn(async move {
            while let Ok((mut client, _)) = listener.accept().await {
                if !http {
                    relay.rtsp_connections.fetch_add(1, Ordering::SeqCst);
                }
                if relay.cut.load(Ordering::SeqCst)
                    || (http && relay.whep_down.load(Ordering::SeqCst))
                {
                    continue;
                }
                let relay = relay.clone();
                tokio::spawn(async move {
                    let Ok(mut server) = TcpStream::connect(("127.0.0.1", port)).await else {
                        return;
                    };
                    let (mut from_client, to_client) = client.split();
                    let to_client = tokio::sync::Mutex::new(to_client);
                    let (mut from_server, mut to_server) = server.split();
                    let upstream = async {
                        let mut buf = vec![0u8; 16384];
                        while let Ok(n) = from_client.read(&mut buf).await {
                            if n == 0
                                || relay.cut.load(Ordering::SeqCst)
                                || (http && relay.whep_down.load(Ordering::SeqCst))
                            {
                                break;
                            }
                            if http {
                                let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                                for method in ["POST", "PATCH", "DELETE", "OPTIONS"] {
                                    if text.lines().any(|l| l.starts_with(&format!("{method} /"))) {
                                        relay.requests.lock().unwrap().push(method.to_owned());
                                    }
                                }
                                if relay.no_patch.load(Ordering::SeqCst)
                                    && text.starts_with("PATCH ")
                                {
                                    let _ = to_client
                                        .lock()
                                        .await
                                        .write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                                        .await;
                                    break;
                                }
                            }
                            if to_server.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    };
                    let downstream = async {
                        let mut buf = vec![0u8; 16384];
                        while let Ok(n) = from_server.read(&mut buf).await {
                            if n == 0 || relay.cut.load(Ordering::SeqCst) {
                                break;
                            }
                            if to_client.lock().await.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    };
                    tokio::select! {
                        () = upstream => {}
                        () = downstream => {}
                    }
                });
            }
        });
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

/// The pictures a camera's worker put into the hub, as a recording takes
/// them: when each came and whether it was a keyframe.
#[derive(Clone, Default)]
pub(in crate::video) struct Pictures(Arc<Mutex<Vec<(Instant, bool, u64)>>>);

impl Pictures {
    fn recording(hub: &Hub, source_id: &str) -> Self {
        let pictures = Self::default();
        let (to, from) = std::sync::mpsc::channel();
        hub.record("test", source_id, to);
        let kept = pictures.clone();
        std::thread::spawn(move || {
            while let Ok(item) = from.recv() {
                if let Item::Unit(unit) = item {
                    kept.0
                        .lock()
                        .unwrap()
                        .push((Instant::now(), unit.key, unit.timestamp_us));
                }
            }
        });
        pictures
    }

    pub(in crate::video) fn since(&self, at: Instant) -> Vec<(Instant, bool, u64)> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .copied()
            .filter(|(when, _, _)| *when >= at)
            .collect()
    }
}

pub(in crate::video) struct Camera {
    pub(in crate::video) worker: rtsp::Worker,
    pub(in crate::video) pictures: Pictures,
    _dir: tempfile::TempDir,
}

pub(in crate::video) fn camera(urls: &[String]) -> Camera {
    let dir = tempfile::tempdir().unwrap();
    let hub = Arc::new(Hub::new(dir.path().to_owned()));
    let pictures = Pictures::recording(&hub, "cam");
    let worker = rtsp::spawn(
        &tokio::runtime::Handle::current(),
        "cam".into(),
        urls.to_vec(),
        1,
        None,
        hub,
        test_engine(),
    );
    Camera {
        worker,
        pictures,
        _dir: dir,
    }
}

fn whep(ip: Ipv4Addr, node: &Node) -> String {
    format!("http://{ip}:{}/cam/whep", node.http)
}

fn rtsp_url(ip: Ipv4Addr, node: &Node) -> String {
    format!("rtsp://{ip}:{}/cam", node.rtsp)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MediaMTX and FFmpeg: MEDIAMTX=… cargo test -p uxduck --bin uxduck whep -- --ignored"]
async fn a_whep_camera_plays_its_pictures_into_the_hub_and_deletes_its_session() {
    let node = Node::start(None).await;
    let radio = Connection::open(RADIO, &node, true).await;
    let internet = Connection::open(INTERNET, &node, true).await;
    let camera = camera(&[whep(RADIO, &node)]);
    tokio::time::sleep(Duration::from_secs(3)).await;
    let from = Instant::now();
    tokio::time::sleep(Duration::from_secs(5)).await;
    let pictures = camera.pictures.since(from);
    let keyframes = pictures.iter().filter(|(_, key, _)| *key).count();
    assert!(pictures.len() >= 140, "{} pictures in 5 s", pictures.len());
    assert!((4..=6).contains(&keyframes), "{keyframes} keyframes in 5 s");
    assert_eq!(radio.requests(), ["POST"]);
    assert_eq!(node.sessions().await, 1);
    // The answer named both connections' addresses: the internet's, not
    // the camera address's host, was never tried.
    assert_eq!(internet.datagrams.load(Ordering::SeqCst), 0);
    camera.worker.task.abort();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(radio.requests(), ["POST", "DELETE"]);
    assert_eq!(node.sessions().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MediaMTX and FFmpeg: MEDIAMTX=… cargo test -p uxduck --bin uxduck whep -- --ignored"]
async fn nacks_repair_a_lossy_connection_whose_sender_offers_no_rtx() {
    let node = Node::start(None).await;
    let radio = Connection::open(RADIO, &node, true).await;
    // About 2% of the video lost: one packet in 50.
    radio.lose_every.store(50, Ordering::SeqCst);
    let camera = camera(&[whep(RADIO, &node)]);
    tokio::time::sleep(Duration::from_secs(3)).await;
    let from = Instant::now();
    tokio::time::sleep(Duration::from_secs(10)).await;
    let pictures = camera.pictures.since(from).len();
    println!("{pictures} pictures in 10 s with one packet in 50 lost");
    assert!(pictures >= 285, "{pictures} pictures in 10 s");
}

/// The gap from the cut to the first picture and the first keyframe that
/// were not already on their way: newer than every picture before the cut.
fn gap(pictures: &Pictures, cut: Instant) -> (Duration, Duration) {
    let all = pictures.0.lock().unwrap().clone();
    let last_before = all
        .iter()
        .filter(|(at, _, _)| *at < cut)
        .map(|(_, _, pts)| *pts)
        .max()
        .unwrap_or_default();
    let after: Vec<_> = all
        .iter()
        .filter(|(at, _, pts)| *at >= cut && *pts > last_before)
        .collect();
    let first = after.first().map_or(Duration::MAX, |(at, _, _)| *at - cut);
    let key = after
        .iter()
        .find(|(_, key, _)| *key)
        .map_or(Duration::MAX, |(at, _, _)| *at - cut);
    (first, key)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MediaMTX and FFmpeg: MEDIAMTX=… cargo test -p uxduck --bin uxduck whep -- --ignored"]
async fn a_move_to_another_connection_restarts_ice_in_the_same_session() {
    let node = Node::start(None).await;
    let radio = Connection::open(RADIO, &node, true).await;
    let internet = Connection::open(INTERNET, &node, true).await;
    let camera = camera(&[
        whep(RADIO, &node),
        rtsp_url(RADIO, &node),
        whep(INTERNET, &node),
        rtsp_url(INTERNET, &node),
    ]);
    tokio::time::sleep(Duration::from_secs(4)).await;
    let before = camera
        .pictures
        .since(Instant::now().checked_sub(Duration::from_secs(1)).unwrap());
    let last_before = before.last().map(|(_, _, t)| *t).unwrap_or_default();
    // The radio is lost and the vehicle moves to the internet at once.
    radio.cut.store(true, Ordering::SeqCst);
    let cut = Instant::now();
    camera.worker.steer(2);
    tokio::time::sleep(Duration::from_secs(5)).await;
    let (first, key) = gap(&camera.pictures, cut);
    println!(
        "ICE restart: first picture {} ms and first keyframe {} ms after the cut",
        first.as_millis(),
        key.as_millis()
    );
    assert!(first < Duration::from_secs(3), "{first:?}");
    let after = camera.pictures.since(cut + Duration::from_secs(2));
    assert!(
        after.len() >= 80,
        "{} pictures in the 3 s after",
        after.len()
    );
    // One session: its pictures' times go on from before the cut.
    let first_after = camera
        .pictures
        .since(cut)
        .first()
        .map(|(_, _, t)| *t)
        .unwrap_or_default();
    assert!(
        first_after > last_before && first_after - last_before < 3_000_000,
        "{last_before} then {first_after}"
    );
    assert_eq!(radio.requests(), ["POST"]);
    assert_eq!(internet.requests(), ["PATCH"]);
    assert_eq!(node.sessions().await, 1);
    assert_eq!(internet.rtsp_connections.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MediaMTX and FFmpeg: MEDIAMTX=… cargo test -p uxduck --bin uxduck whep -- --ignored"]
async fn a_refused_ice_restart_opens_a_new_session_at_the_new_address() {
    let node = Node::start(None).await;
    let radio = Connection::open(RADIO, &node, true).await;
    let internet = Connection::open(INTERNET, &node, true).await;
    internet.no_patch.store(true, Ordering::SeqCst);
    let camera = camera(&[
        whep(RADIO, &node),
        rtsp_url(RADIO, &node),
        whep(INTERNET, &node),
        rtsp_url(INTERNET, &node),
    ]);
    tokio::time::sleep(Duration::from_secs(4)).await;
    radio.cut.store(true, Ordering::SeqCst);
    let cut = Instant::now();
    camera.worker.steer(2);
    tokio::time::sleep(Duration::from_secs(5)).await;
    let (first, key) = gap(&camera.pictures, cut);
    println!(
        "a new session: first picture {} ms and first keyframe {} ms after the cut",
        first.as_millis(),
        key.as_millis()
    );
    assert!(first < Duration::from_secs(3), "{first:?}");
    assert_eq!(internet.requests(), ["PATCH", "POST"]);
    assert_eq!(internet.rtsp_connections.load(Ordering::SeqCst), 0);
    // Straight there, not through the addresses between.
    assert_eq!(radio.rtsp_connections.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MediaMTX and FFmpeg: MEDIAMTX=… cargo test -p uxduck --bin uxduck whep -- --ignored"]
async fn a_move_to_an_address_whep_does_not_answer_falls_back_to_rtsp() {
    let node = Node::start(None).await;
    let radio = Connection::open(RADIO, &node, true).await;
    // Only RTSP passes on the internet's connection.
    let internet = Connection::open(INTERNET, &node, false).await;
    let camera = camera(&[
        whep(RADIO, &node),
        rtsp_url(RADIO, &node),
        whep(INTERNET, &node),
        rtsp_url(INTERNET, &node),
    ]);
    tokio::time::sleep(Duration::from_secs(4)).await;
    radio.cut.store(true, Ordering::SeqCst);
    let cut = Instant::now();
    camera.worker.steer(2);
    tokio::time::sleep(Duration::from_secs(6)).await;
    let (first, _) = gap(&camera.pictures, cut);
    println!(
        "RTSP after WHEP failed: first picture {} ms after the cut",
        first.as_millis()
    );
    assert!(first < Duration::from_secs(4), "{first:?}");
    assert_eq!(internet.rtsp_connections.load(Ordering::SeqCst), 1);
    assert!(camera.pictures.since(cut + Duration::from_secs(4)).len() >= 40);
}

/// A WHEP camera whose pictures stop for 2 s moves to the RTSP address
/// kept set up on another connection its vehicle is heard on, by PLAY
/// alone. The WHEP address on that connection is never the one kept set up
/// (ADR-0057 D12 and D13, ADR-0064 W14).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MediaMTX and FFmpeg: MEDIAMTX=… cargo test -p uxduck --bin uxduck whep -- --ignored"]
async fn a_whep_camera_that_stalls_moves_to_the_rtsp_address_kept_set_up() {
    let node = Node::start(None).await;
    let radio = Connection::open(RADIO, &node, true).await;
    let internet = Connection::open(INTERNET, &node, true).await;
    let camera = camera(&[
        whep(RADIO, &node),
        whep(INTERNET, &node),
        rtsp_url(INTERNET, &node),
    ]);
    let route = |link: &str, in_use: bool| {
        Some(AddressRoute {
            link_id: link.into(),
            up: true,
            in_use,
            returns: false,
        })
    };
    camera.worker.route(&[
        route("radio", true),
        route("internet", false),
        route("internet", false),
    ]);
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(internet.rtsp_connections.load(Ordering::SeqCst), 1);
    assert!(internet.requests().is_empty(), "{:?}", internet.requests());
    // The radio is lost and the vehicle has not moved yet.
    radio.cut.store(true, Ordering::SeqCst);
    let cut = Instant::now();
    tokio::time::sleep(Duration::from_secs(6)).await;
    let (first, key) = gap(&camera.pictures, cut);
    println!(
        "a stall to RTSP kept set up: first picture {} ms and first keyframe {} ms after the cut",
        first.as_millis(),
        key.as_millis()
    );
    assert!(
        (Duration::from_secs(2)..Duration::from_secs(4)).contains(&first),
        "{first:?}"
    );
    assert!(camera.pictures.since(cut + Duration::from_secs(4)).len() >= 40);
    // The session kept set up played: no other connection, and no WHEP.
    assert_eq!(internet.rtsp_connections.load(Ordering::SeqCst), 1);
    assert!(internet.requests().is_empty(), "{:?}", internet.requests());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MediaMTX and FFmpeg: MEDIAMTX=… cargo test -p uxduck --bin uxduck whep -- --ignored"]
async fn a_pinned_certificate_is_refused_once_it_changes() {
    let made = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let other = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let pin = |cert: &rcgen::Certificate| hex(&Sha256::digest(cert.der().as_ref()));
    let node = Node::start(Some((&made.cert.pem(), &made.signing_key.serialize_pem()))).await;
    let _radio = Connection::open(RADIO, &node, true).await;
    let at = format!("https://{RADIO}:{}/cam/whep", node.http);
    probe(&format!("{at}#sha256={}", pin(&made.cert)))
        .await
        .unwrap();
    let changed = probe(&format!("{at}#sha256={}", pin(&other.cert)))
        .await
        .unwrap_err();
    assert!(changed.contains("certificate changed"), "{changed}");
    // Without a pin, the system's certificates do not know it.
    assert!(probe(&at).await.is_err());
    // The pinned address plays.
    let camera = camera(&[format!("{at}#sha256={}", pin(&made.cert))]);
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(
        camera
            .pictures
            .since(Instant::now().checked_sub(Duration::from_secs(2)).unwrap())
            .len()
            >= 50
    );
}

#[test]
fn the_senders_note_on_this_camera_is_found_in_its_status() {
    let floor = "The connection carries about 120 kbit/s, under this camera's lowest 150 kbit/s, so its picture runs late.";
    let status = serde_json::json!({
        "cameras": [
            { "path": "rear", "note": null },
            { "path": "front", "note": floor },
        ],
        "addresses": [],
    });
    assert_eq!(note_for(&status, "front").as_deref(), Some(floor));
    assert_eq!(note_for(&status, "rear"), None);
    assert_eq!(note_for(&status, "side"), None);
    assert_eq!(
        note_for(&serde_json::json!("Not a WHEP address."), "front"),
        None
    );
    assert!(uxduck_sender(Some("UXDuck/0.20.0")));
    assert!(!uxduck_sender(Some("mediamtx")));
    assert!(!uxduck_sender(None));
}

#[test]
fn a_slow_picture_says_what_arrives() {
    assert_eq!(
        late(120_400),
        "The connection carries about 120 kbit/s, too little for this camera's pictures, so its picture runs late."
    );
    assert_eq!(
        late(0),
        "The connection carries too little for this camera's pictures, so its picture runs late."
    );
}

#[test]
fn a_keyframe_is_a_picture_with_an_idr_slice() {
    assert!(keyframe(&[
        0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 1, 0, 0, 0, 1, 0x65, 0x88
    ]));
    assert!(!keyframe(&[0, 0, 0, 1, 0x41, 0x9a]));
}

/// The vehicle uses the connection the camera's two addresses are on, as
/// the Plane uses Tailscale with the camera's WHEP and RTSP addresses there.
fn on_the_vehicles_connection(camera: &Camera) {
    let route = Some(AddressRoute {
        link_id: "tailscale".into(),
        up: true,
        in_use: true,
        returns: false,
    });
    camera.worker.route(&[route.clone(), route]);
}

/// The longest wait between two pictures after `since`.
fn longest_gap(pictures: &Pictures, since: Instant) -> Duration {
    pictures
        .since(since)
        .windows(2)
        .map(|pair| pair[1].0 - pair[0].0)
        .max()
        .unwrap_or(Duration::MAX)
}

async fn until(what: &str, within: Duration, done: impl Fn() -> bool) -> Instant {
    let deadline = Instant::now() + within;
    while !done() {
        assert!(Instant::now() < deadline, "{what} within {within:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Instant::now()
}

/// The link squeezed under what the pictures need, as the Plane's 4G was
/// under its sender's 150 kbit/s floor: the pictures stop or come late
/// while ICE holds and the sender's packets still arrive. The camera keeps
/// its WHEP session and never asks RTSP on the same connection, which
/// would share the squeeze and run later still, and the pictures come back
/// in the same session when the link does.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MediaMTX and FFmpeg: MEDIAMTX=… cargo test -p uxduck --bin uxduck whep -- --ignored"]
async fn a_whep_camera_on_a_squeezed_link_keeps_its_session_and_never_moves_to_rtsp() {
    let node = Node::start(None).await;
    let link = Connection::open(RADIO, &node, true).await;
    let camera = camera(&[whep(RADIO, &node), rtsp_url(RADIO, &node)]);
    on_the_vehicles_connection(&camera);
    tokio::time::sleep(Duration::from_secs(4)).await;
    // The test picture sends about 1.5 Mbit/s.
    link.rate_bps.store(120_000, Ordering::SeqCst);
    let squeezed = Instant::now();
    tokio::time::sleep(Duration::from_secs(14)).await;
    println!(
        "{} pictures in the 12 s of the squeeze after its first 2 s, the longest wait {:?}",
        camera
            .pictures
            .since(squeezed + Duration::from_secs(2))
            .len(),
        longest_gap(&camera.pictures, squeezed)
    );
    assert_eq!(link.rtsp_connections.load(Ordering::SeqCst), 0);
    assert_eq!(link.requests(), ["POST"]);
    assert_eq!(node.sessions().await, 1);
    link.rate_bps.store(0, Ordering::SeqCst);
    let lifted = Instant::now();
    tokio::time::sleep(Duration::from_secs(6)).await;
    let after = camera.pictures.since(lifted + Duration::from_secs(3)).len();
    println!("{after} pictures in the 3 s from 3 s after the link came back");
    assert!(after >= 60, "{after} pictures in 3 s");
    assert_eq!(link.requests(), ["POST"]);
    assert_eq!(link.rtsp_connections.load(Ordering::SeqCst), 0);
}

/// The WHEP sender stops, as when it restarted for 6 s on the Plane: the
/// dead session falls back to RTSP on the same host and connection. When
/// WHEP answers again, the camera goes back to it after the 30 s wait and
/// two probes 15 s apart, though the vehicle never moves, opening the WHEP
/// session beside RTSP and switching at its first keyframe, so the picture
/// is never empty. The test picture comes every 33 ms, and a new session's
/// first keyframe after RTSP closed came about 300 ms later on loopback, so
/// a gap under 150 ms is a switch with the session already playing.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MediaMTX and FFmpeg: MEDIAMTX=… cargo test -p uxduck --bin uxduck whep -- --ignored"]
async fn a_dead_whep_session_falls_back_to_rtsp_on_its_host_and_comes_back_at_a_keyframe() {
    let node = Node::start(None).await;
    let link = Connection::open(RADIO, &node, true).await;
    let camera = camera(&[whep(RADIO, &node), rtsp_url(RADIO, &node)]);
    on_the_vehicles_connection(&camera);
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(link.requests(), ["POST"]);
    link.whep_down.store(true, Ordering::SeqCst);
    let down = Instant::now();
    let fell = until("RTSP on the same host", Duration::from_secs(10), || {
        link.rtsp_connections.load(Ordering::SeqCst) == 1
    })
    .await;
    println!(
        "fell back to RTSP {} ms after WHEP stopped",
        (fell - down).as_millis()
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(camera.pictures.since(fell + Duration::from_secs(1)).len() >= 40);
    link.whep_down.store(false, Ordering::SeqCst);
    let posted = until("a new WHEP session", Duration::from_secs(55), || {
        link.requests().iter().filter(|m| *m == "POST").count() == 2
    })
    .await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    println!(
        "back on WHEP {} s after the fall back, the requests {:?}, the longest wait between pictures from 2 s after the fall back {:?}",
        (posted - fell).as_secs(),
        link.requests(),
        longest_gap(&camera.pictures, fell + Duration::from_secs(2))
    );
    assert!(
        posted - fell >= Duration::from_secs(44),
        "{:?}",
        posted - fell
    );
    assert!(
        longest_gap(&camera.pictures, fell + Duration::from_secs(2)) < Duration::from_millis(150),
        "the picture went empty while it went back"
    );
    assert!(camera.pictures.since(posted + Duration::from_secs(2)).len() >= 50);
    // One WebRTC session, and the RTSP reader gone: only the publisher's.
    assert_eq!(node.sessions().await, 1);
    assert_eq!(node.rtsp_sessions().await, 1);
    assert_eq!(link.rtsp_connections.load(Ordering::SeqCst), 1);
}
