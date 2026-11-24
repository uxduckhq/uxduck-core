//! The WHEP client's sound (ADR-0067 M8, M9, M10) against a fake sender
//! in this process: an axum WHEP server whose session is a str0m peer
//! sending `FFmpeg`'s recorded H.264 test pattern and Opus packets. As
//! UXDuck's sender it answers `Server: UXDuck/…` and the session's sound
//! PATCH and GET, and sends sound only after a PATCH turns it on. As
//! UXDuck's sender older than sound it refuses both, takes no Opus and
//! sends its pictures on whichever stream the offer named last, as that
//! sender does. As `MediaMTX` it answers neither and sends sound whenever
//! the offer has it.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use str0m::change::SdpOffer;
use str0m::format::Codec as RtcCodec;
use str0m::media::{Frequency, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc, RtcConfig};
use tokio::net::UdpSocket;
use uxduck_media::audio::AudioConfig;
use uxduck_media::tsrecord::TsRecorder;

use super::tests::test_engine;
use super::{address, remember, sender_answers};
use crate::video::hub::{Hub, Item};
use crate::video::rtsp;

/// 160×90 at 30 pictures a second, a keyframe first: one second of
/// `FFmpeg`'s test pattern from libx264.
const PATTERN: &[u8] = include_bytes!("../sender/test-pattern.h264");

/// The pattern's pictures: each slice with the units before it.
fn pictures() -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut unit = Vec::new();
    for nal in uxduck_media::nal::split_annex_b(PATTERN) {
        unit.extend_from_slice(&[0, 0, 0, 1]);
        unit.extend_from_slice(nal);
        if matches!(nal[0] & 0x1f, 1 | 5) {
            out.push(std::mem::take(&mut unit));
        }
    }
    out
}

struct Peer {
    id: String,
    rtc: Rtc,
    video: Option<Mid>,
    audio: Option<Mid>,
    connected: bool,
}

/// The fake sender's state, shared by its HTTP routes and its media.
struct Fake {
    uxduck: bool,
    /// UXDuck's sender from before sound (ADR-0067).
    older: bool,
    /// Each request: `POST audio` for an offer with sound, `POST`,
    /// `PATCH true`, `PATCH false`, `GET` and `DELETE`.
    requests: Vec<String>,
    session: Option<Peer>,
    /// The session asked for its sound (UXDuck's sender).
    sound_on: bool,
    /// The sound's state when it is not sent, as UXDuck's sender says it.
    state: &'static str,
    sessions: u32,
}

type Shared = Arc<Mutex<Fake>>;

struct Sender {
    fake: Shared,
    url: String,
}

impl Sender {
    async fn start(uxduck: bool) -> Self {
        Self::with(uxduck, false).await
    }

    async fn older() -> Self {
        Self::with(true, true).await
    }

    async fn with(uxduck: bool, older: bool) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let media = socket.local_addr().unwrap();
        let fake: Shared = Arc::new(Mutex::new(Fake {
            uxduck,
            older,
            requests: Vec::new(),
            session: None,
            sound_on: false,
            state: "available",
            sessions: 0,
        }));
        let app = axum::Router::new()
            .route("/cam/whep", axum::routing::post(offer))
            .route(
                "/cam/whep/{id}",
                axum::routing::get(read).patch(patch).delete(delete),
            )
            .with_state((fake.clone(), media));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await });
        tokio::spawn(drive(fake.clone(), socket));
        let url = format!("http://127.0.0.1:{port}/cam/whep");
        // What a sender said of its sound is remembered by address for as
        // long as these tests' process runs, and another test's sender may
        // have had this port.
        remember(&address(&url).unwrap(), None);
        Self { fake, url }
    }

    fn requests(&self) -> Vec<String> {
        self.fake.lock().unwrap().requests.clone()
    }

    fn count(&self, request: &str) -> usize {
        self.requests().iter().filter(|r| *r == request).count()
    }

    fn offers(&self) -> Vec<String> {
        self.requests()
            .into_iter()
            .filter(|r| r.starts_with("POST"))
            .collect()
    }
}

type Routed = State<(Shared, SocketAddr)>;

async fn offer(State((fake, media)): Routed, body: String) -> Response {
    let mut f = fake.lock().unwrap();
    let with_sound = body.contains("m=audio");
    f.requests
        .push(if with_sound { "POST audio" } else { "POST" }.into());
    let offer = SdpOffer::from_sdp_string(&body).unwrap();
    // Its sound reported on each second, as the pictures are, rather than
    // str0m's 5 s.
    let mut rtc = RtcConfig::new()
        .clear_codecs()
        .enable_h264(true)
        .enable_opus(!f.older, false)
        .set_rtcp_report_interval_audio(Duration::from_secs(1))
        .build(Instant::now());
    rtc.add_local_candidate(Candidate::host(media, "udp").unwrap());
    let answer = rtc.sdp_api().accept_offer(offer).unwrap().to_sdp_string();
    f.sessions += 1;
    let id = format!("s{}", f.sessions);
    f.session = Some(Peer {
        id: id.clone(),
        rtc,
        video: None,
        audio: None,
        connected: false,
    });
    f.sound_on = false;
    let server = if f.uxduck { "UXDuck/test" } else { "mediamtx" };
    (
        StatusCode::CREATED,
        [
            (header::LOCATION, format!("/cam/whep/{id}")),
            (header::SERVER, server.to_owned()),
            (header::CONTENT_TYPE, "application/sdp".to_owned()),
        ],
        answer,
    )
        .into_response()
}

/// The session's sound as UXDuck's sender answers it (ADR-0067 M9).
fn said(f: &Fake) -> Response {
    let state = if f.sound_on && f.state == "available" {
        "sending"
    } else {
        f.state
    };
    let microphone = (state == "available" || state == "sending").then(|| {
        serde_json::json!({"id": "usb:3-1.2", "name": "USB PnP Sound Device", "card": "Device", "usb": true})
    });
    let body = serde_json::json!({"sound": {
        "state": state,
        "on": f.sound_on,
        "microphone": microphone,
        "codec": microphone.as_ref().map(|_| "Opus, 48 kHz, mono"),
        "bitrate": (state == "sending").then_some(24_000),
        "reason": null,
    }});
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

async fn read(State((fake, _)): Routed, Path(_id): Path<String>) -> Response {
    let mut f = fake.lock().unwrap();
    if !f.uxduck {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    f.requests.push("GET".into());
    if f.older {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    said(&f)
}

async fn patch(State((fake, _)): Routed, Path(_id): Path<String>, body: String) -> Response {
    let mut f = fake.lock().unwrap();
    if !f.uxduck {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    if f.older {
        f.requests.push("PATCH".into());
        return (
            StatusCode::BAD_REQUEST,
            "The ICE restart was not an SDP fragment.",
        )
            .into_response();
    }
    let asked: serde_json::Value = serde_json::from_str(&body).unwrap();
    let on = asked["sound"].as_bool().unwrap();
    f.requests.push(format!("PATCH {on}"));
    f.sound_on = on;
    said(&f)
}

async fn delete(State((fake, _)): Routed, Path(id): Path<String>) -> Response {
    let mut f = fake.lock().unwrap();
    f.requests.push("DELETE".into());
    if f.session.as_ref().is_some_and(|s| s.id == id) {
        f.session = None;
    }
    StatusCode::OK.into_response()
}

/// The session's media: a picture every 33 ms and an Opus packet every
/// 20 ms, numbered in its second and third bytes, dated on one wall clock
/// so the two streams' sender reports line them up.
async fn drive(fake: Shared, socket: UdpSocket) {
    let pictures = pictures();
    let local = socket.local_addr().unwrap();
    let start = Instant::now();
    let mut video_tick = tokio::time::interval(Duration::from_micros(33_333));
    let mut audio_tick = tokio::time::interval(Duration::from_millis(20));
    let (mut n, mut m) = (0u64, 0u64);
    let mut buf = vec![0u8; 2048];
    loop {
        let mut sends = Vec::new();
        let timeout = {
            let mut f = fake.lock().unwrap();
            let older = f.older;
            match f.session.as_mut() {
                None => Instant::now() + Duration::from_millis(20),
                Some(peer) => loop {
                    match peer.rtc.poll_output() {
                        Ok(Output::Timeout(at)) => break at,
                        Ok(Output::Transmit(t)) => sends.push((t.destination, t.contents.to_vec())),
                        Ok(Output::Event(Event::Connected)) => peer.connected = true,
                        Ok(Output::Event(Event::MediaAdded(added))) => match added.kind {
                            _ if older => peer.video = Some(added.mid),
                            MediaKind::Video => peer.video = Some(added.mid),
                            MediaKind::Audio => peer.audio = Some(added.mid),
                        },
                        Ok(Output::Event(_)) => {}
                        Err(_) => break Instant::now() + Duration::from_millis(20),
                    }
                },
            }
        };
        for (to, bytes) in sends {
            let _ = socket.send_to(&bytes, to).await;
        }
        let wake = tokio::time::Instant::from_std(timeout.max(Instant::now()));
        tokio::select! {
            Ok((len, from)) = socket.recv_from(&mut buf) => {
                let mut f = fake.lock().unwrap();
                if let Some(peer) = f.session.as_mut()
                    && let Ok(contents) = buf[..len].try_into()
                {
                    let _ = peer.rtc.handle_input(Input::Receive(
                        Instant::now(),
                        Receive { proto: Protocol::Udp, source: from, destination: local, contents },
                    ));
                }
            }
            () = tokio::time::sleep_until(wake) => {
                if let Some(peer) = fake.lock().unwrap().session.as_mut() {
                    let _ = peer.rtc.handle_input(Input::Timeout(Instant::now()));
                }
            }
            _ = video_tick.tick() => {
                let picture = pictures[usize::try_from(n).unwrap() % pictures.len()].clone();
                let wallclock = start + Duration::from_micros(n * 33_333);
                let time = MediaTime::from_90khz(n * 3000);
                n += 1;
                let mut f = fake.lock().unwrap();
                if let Some(peer) = f.session.as_mut().filter(|p| p.connected)
                    && let Some(mid) = peer.video
                    && let Some(writer) = peer.rtc.writer(mid)
                {
                    let pt = writer
                        .payload_params()
                        .find(|p| {
                            p.spec().codec == RtcCodec::H264
                                && p.spec().format.packetization_mode == Some(1)
                        })
                        .map(str0m::format::PayloadParams::pt);
                    if let Some(pt) = pt {
                        let _ = writer.write(pt, wallclock, time, picture);
                    }
                }
            }
            _ = audio_tick.tick() => {
                let wallclock = start + Duration::from_millis(m * 20);
                let time = MediaTime::new(m * 960, Frequency::FORTY_EIGHT_KHZ);
                let [.., hi, lo] = m.to_be_bytes();
                let packet = vec![0xFC, hi, lo];
                m += 1;
                let mut f = fake.lock().unwrap();
                let sends_sound = !f.uxduck || f.sound_on;
                if sends_sound
                    && let Some(peer) = f.session.as_mut().filter(|p| p.connected)
                    && let Some(mid) = peer.audio
                    && let Some(writer) = peer.rtc.writer(mid)
                {
                    let pt = writer
                        .payload_params()
                        .find(|p| p.spec().codec == RtcCodec::Opus)
                        .map(str0m::format::PayloadParams::pt);
                    if let Some(pt) = pt {
                        let _ = writer.write(pt, wallclock, time, packet);
                    }
                }
            }
        }
    }
}

/// What reached the hub for a socket or a recording: when, a picture
/// (`p`) or sound (`s`), its time, and the sound's number.
#[derive(Clone, Default)]
struct Heard(Arc<Mutex<Vec<Thing>>>);

/// One thing heard: when, a picture (`p`) or sound (`s`), its time, and
/// the sound's number.
type Thing = (Instant, char, u64, u64);

impl Heard {
    fn keep(&self, item: &Item) {
        let heard = match item {
            Item::Unit(unit) => ('p', unit.timestamp_us, 0),
            Item::Audio(sound) => {
                assert_eq!(
                    sound.config,
                    AudioConfig::Opus(uxduck_media::opus::OpusConfig { channels: 1 })
                );
                assert_eq!(sound.samples, 960);
                (
                    's',
                    sound.timestamp_us,
                    u64::from(u16::from_be_bytes([sound.data[1], sound.data[2]])),
                )
            }
            _ => return,
        };
        self.0
            .lock()
            .unwrap()
            .push((Instant::now(), heard.0, heard.1, heard.2));
    }

    fn since(&self, at: Instant, kind: char) -> Vec<(u64, u64)> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(when, k, _, _)| *when >= at && *k == kind)
            .map(|(_, _, t, n)| (*t, *n))
            .collect()
    }

    /// How far apart the latest sound and the latest picture are on the
    /// pictures' clock: both were sent up to the same moment.
    fn apart_us(&self) -> u64 {
        let all = self.0.lock().unwrap();
        let latest = |kind: char| {
            all.iter()
                .filter(|h| h.1 == kind)
                .map(|h| h.2)
                .max()
                .unwrap_or(0)
        };
        latest('p').abs_diff(latest('s'))
    }
}

/// A camera's worker playing `url`, and a socket watching it that does not
/// listen yet.
struct Watching {
    hub: Arc<Hub>,
    worker: rtsp::Worker,
    heard: Heard,
    _dir: tempfile::TempDir,
}

impl Watching {
    fn start(url: &str) -> Self {
        Self::with(url, false)
    }

    /// With the socket listening before the camera's worker starts.
    fn listening(url: &str) -> Self {
        Self::with(url, true)
    }

    fn with(url: &str, listening: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let hub = Arc::new(Hub::new(dir.path().to_owned()));
        hub.allow("t", "cam");
        let (_, watcher) = hub.open("t").unwrap();
        hub.listen("t", listening);
        let heard = Heard::default();
        let kept = heard.clone();
        tokio::spawn(async move {
            while let Some((item, _)) = watcher.next().await {
                kept.keep(&item);
            }
        });
        let worker = rtsp::spawn(
            &tokio::runtime::Handle::current(),
            "cam".into(),
            vec![url.to_owned()],
            1,
            None,
            hub.clone(),
            test_engine(),
        );
        Self {
            hub,
            worker,
            heard,
            _dir: dir,
        }
    }
}

async fn sleep_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// ADR-0067 M9: UXDuck's sender is offered sound in every session, which
/// starts silent, is asked for it with a PATCH while a window listens, and
/// is read with a GET every 2 s. The first session to a new address is
/// opened again with sound once its `Server` says it is UXDuck's sender,
/// and from then on the pictures never stop for sound: one session.
#[tokio::test(flavor = "multi_thread")]
async fn a_uxduck_sender_is_asked_for_sound_in_its_session_as_it_is_wanted() {
    let sender = Sender::start(true).await;
    let camera = Watching::start(&sender.url);
    sleep_ms(3000).await;
    assert_eq!(sender.offers(), ["POST", "POST audio"]);
    assert_eq!(sender.count("DELETE"), 1, "{:?}", sender.requests());
    assert_eq!(sender.count("PATCH true"), 0, "{:?}", sender.requests());
    assert!(sender.count("GET") >= 1, "{:?}", sender.requests());
    let from = Instant::now();
    assert!(
        camera
            .heard
            .since(from.checked_sub(Duration::from_secs(2)).unwrap(), 'p')
            .len()
            >= 40
    );
    assert!(
        camera
            .heard
            .since(from.checked_sub(Duration::from_secs(3)).unwrap(), 's')
            .is_empty()
    );

    camera.hub.listen("t", true);
    let on = Instant::now();
    sleep_ms(1500).await;
    assert_eq!(sender.count("PATCH true"), 1, "{:?}", sender.requests());
    let sound = camera.heard.since(on, 's');
    println!("{} sound frames in the 1.5 s after sound on", sound.len());
    assert!(sound.len() >= 30, "{} sound frames", sound.len());
    // 20 ms apart on the pictures' clock, as they were sent, but for one
    // step where the sender reports took over from arrival, which lined
    // the first sound up with the latest picture, at most a frame old.
    let steps: Vec<_> = sound
        .windows(2)
        .filter(|pair| pair[1].1 == pair[0].1 + 1)
        .filter(|pair| pair[1].0.abs_diff(pair[0].0 + 20_000) > 1_000)
        .collect();
    assert!(steps.len() <= 1, "{steps:?}");
    for pair in steps {
        assert!(pair[1].0.abs_diff(pair[0].0 + 20_000) < 100_000, "{pair:?}");
    }
    // Lined up by the sender reports before arrival could have.
    let apart = camera.heard.apart_us();
    println!("the latest sound is {apart} µs from the latest picture");
    assert!(apart < 100_000, "{apart} µs apart");

    camera.hub.listen("t", false);
    let off = Instant::now();
    sleep_ms(1500).await;
    assert_eq!(sender.count("PATCH false"), 1, "{:?}", sender.requests());
    assert!(
        camera
            .heard
            .since(off + Duration::from_millis(500), 's')
            .is_empty()
    );
    assert!(
        camera.heard.since(off, 'p').len() >= 30,
        "the pictures go on"
    );
    sleep_ms(1500).await;
    assert!(sender.count("GET") >= 3, "{:?}", sender.requests());
    assert_eq!(sender.offers(), ["POST", "POST audio"], "one session since");
    assert_eq!(sender_answers(&address(&sender.url).unwrap()), Some(true));
    camera.worker.task.abort();
}

/// ADR-0067 M10: another server is offered sound only while it is wanted,
/// its session opened again each time that changes.
#[tokio::test(flavor = "multi_thread")]
async fn a_mediamtx_session_is_opened_again_with_sound_while_it_is_wanted() {
    let sender = Sender::start(false).await;
    let camera = Watching::start(&sender.url);
    sleep_ms(3000).await;
    assert_eq!(sender.offers(), ["POST"]);
    camera.hub.listen("t", true);
    let on = Instant::now();
    sleep_ms(3000).await;
    assert_eq!(sender.offers(), ["POST", "POST audio"]);
    assert_eq!(sender.count("DELETE"), 1, "{:?}", sender.requests());
    assert_eq!(sender.count("PATCH true") + sender.count("GET"), 0);
    let sound = camera.heard.since(on, 's');
    assert!(sound.len() >= 40, "{} sound frames", sound.len());
    assert!(camera.heard.since(on + Duration::from_secs(2), 'p').len() >= 20);
    let apart = camera.heard.apart_us();
    assert!(apart < 100_000, "{apart} µs apart");
    camera.hub.listen("t", false);
    sleep_ms(3000).await;
    assert_eq!(sender.offers(), ["POST", "POST audio", "POST"]);
    assert_eq!(sender_answers(&address(&sender.url).unwrap()), None);
    camera.worker.task.abort();
}

/// ADR-0067 M10: an address UXDuck's sender answered is offered sound at
/// once, and one now answered by another server is opened again without
/// it, and forgotten.
#[tokio::test(flavor = "multi_thread")]
async fn an_address_uxducks_sender_answered_is_offered_sound_until_another_answers() {
    let sender = Sender::start(true).await;
    let first = Watching::start(&sender.url);
    sleep_ms(2000).await;
    first.worker.task.abort();
    sleep_ms(500).await;
    assert_eq!(sender_answers(&address(&sender.url).unwrap()), Some(true));
    sender.fake.lock().unwrap().uxduck = false;
    let second = Watching::start(&sender.url);
    sleep_ms(3000).await;
    assert_eq!(
        sender.offers(),
        ["POST", "POST audio", "POST audio", "POST"]
    );
    assert_eq!(sender_answers(&address(&sender.url).unwrap()), None);
    second.worker.task.abort();
}

/// ADR-0067 M10: UXDuck's sender from before sound refuses the session's
/// sound GET, so it is never offered sound, which it would send its
/// pictures in place of. Offered sound because it was wanted before
/// anything was known of the address, its session is opened again
/// without it.
#[tokio::test(flavor = "multi_thread")]
async fn uxducks_sender_older_than_sound_is_not_offered_it() {
    let sender = Sender::older().await;
    let camera = Watching::start(&sender.url);
    sleep_ms(2500).await;
    assert_eq!(sender.offers(), ["POST"]);
    assert_eq!(sender_answers(&address(&sender.url).unwrap()), Some(false));
    let gets = sender.count("GET");
    assert_eq!(gets, 1, "{:?}", sender.requests());
    camera.hub.listen("t", true);
    let on = Instant::now();
    sleep_ms(2500).await;
    assert_eq!(sender.offers(), ["POST"], "{:?}", sender.requests());
    assert_eq!(sender.count("GET") + sender.count("PATCH"), gets);
    assert!(
        camera.heard.since(on, 'p').len() >= 50,
        "the pictures go on"
    );
    assert!(camera.heard.since(on, 's').is_empty());
    camera.worker.task.abort();

    let wanted = Sender::older().await;
    let camera = Watching::listening(&wanted.url);
    sleep_ms(2500).await;
    assert_eq!(
        wanted.offers(),
        ["POST audio", "POST"],
        "{:?}",
        wanted.requests()
    );
    // Each session asks once, so a sender updated in place is found.
    assert_eq!(wanted.count("PATCH"), 2, "{:?}", wanted.requests());
    let late = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    assert!(
        camera.heard.since(late, 'p').len() >= 20,
        "the pictures came"
    );
    camera.worker.task.abort();
}

/// ADR-0067 M8: a recording wants the sound, so UXDuck's sender is asked
/// for it with nobody listening, and the recording's MPEG-TS has the Opus
/// beside the pictures (S149's writer), read back by UXDuck's own reader
/// and by `ffprobe` when it is installed.
#[tokio::test(flavor = "multi_thread")]
async fn a_recording_of_a_whep_camera_keeps_its_opus() {
    let sender = Sender::start(true).await;
    let dir = tempfile::tempdir().unwrap();
    let hub = Arc::new(Hub::new(dir.path().to_owned()));
    let (to, from) = std::sync::mpsc::channel();
    hub.record("recording", "cam", to);
    let worker = rtsp::spawn(
        &tokio::runtime::Handle::current(),
        "cam".into(),
        vec![sender.url.clone()],
        1,
        None,
        hub.clone(),
        test_engine(),
    );
    sleep_ms(4000).await;
    worker.task.abort();
    hub.stop_recording("recording");
    assert_eq!(sender.count("PATCH true"), 1, "{:?}", sender.requests());
    let mut recorder = TsRecorder::new();
    let mut ts = Vec::new();
    let (mut pictures, mut sounds) = (0, 0);
    while let Ok(item) = from.try_recv() {
        let written = match &item {
            Item::Unit(unit) => {
                pictures += 1;
                recorder.push_unit(unit)
            }
            Item::Audio(sound) => {
                sounds += 1;
                recorder.push_audio(sound)
            }
            _ => continue,
        };
        ts.extend_from_slice(&written.bytes);
    }
    assert!(
        pictures >= 60 && sounds >= 100,
        "{pictures} pictures, {sounds} sounds"
    );
    let mut source = uxduck_media::source::Source::new();
    let (mut first_picture, mut heard) = (None, Vec::new());
    for chunk in ts.chunks(188 * 7) {
        for output in source.push(chunk) {
            match output {
                uxduck_media::source::Output::Unit(unit) => {
                    first_picture.get_or_insert(unit.timestamp_us);
                }
                uxduck_media::source::Output::Audio(unit) => heard.push(unit),
                uxduck_media::source::Output::Klv { .. } => {}
            }
        }
    }
    assert_eq!(
        source.status().audio.map(|a| a.name),
        Some("Opus, 48 kHz, mono".to_owned())
    );
    assert!(heard.len() >= 100, "{} sound frames read back", heard.len());
    assert!(heard.iter().all(|u| u.data[0] == 0xFC));
    let file = dir.path().join("recording.ts");
    std::fs::write(&file, &ts).unwrap();
    match std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "csv=p=0",
        ])
        .arg(&file)
        .output()
    {
        Ok(probed) => {
            let codecs = String::from_utf8_lossy(&probed.stdout).into_owned();
            println!("ffprobe: {}", codecs.replace('\n', " "));
            assert!(
                codecs.contains("h264") && codecs.contains("opus"),
                "{codecs}"
            );
        }
        Err(e) => println!("ffprobe is not installed, so only UXDuck's reader read it: {e}"),
    }
}
