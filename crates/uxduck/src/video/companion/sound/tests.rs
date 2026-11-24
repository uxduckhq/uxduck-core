//! The sound manager with a fake `gst-launch-1.0` that sends Opus RTP to the
//! ports it is given, on the shared fake microphones: a device found and
//! its process started, one plugged in and pulled out, a choice changed, a
//! device that refuses, `GStreamer` missing, and two cameras on one device.
//! The real pipeline's words are checked here and run in the cloud check.
//!
//! The fake is a script, so the tests that start it are for Unix alone, and
//! so are the helpers that only those tests call.

use std::time::Duration;

use super::*;
#[cfg(unix)]
use crate::video::companion::microphones::tests::case;

/// A fake `gst-launch-1.0` in `dir`: it writes its words to `args.log`
/// beside it, then for a capture pipeline sends RTP to each `port=` at its
/// `frame-size=`, with its `ssrc=` and timestamps from 0, and for a joining
/// pipeline waits until it is stopped. Its payload is `payload_of(dir)`, so
/// a test tells its own fake's packets from another test's that reach a
/// port it was given after that test let it go. With `refuse`, it says
/// what ALSA says of a busy device and ends.
#[cfg(unix)]
pub(in crate::video) fn fake_gst(dir: &Path, refuse: bool) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let script = format!(
        r#"#!/usr/bin/env python3
import os, socket, struct, sys, time
args = sys.argv[1:]
payload = bytes([0xFC, 0x01, 0x02]) + os.path.basename(os.path.dirname(os.path.abspath(__file__))).encode()
with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "args.log"), "a") as log:
    log.write(" ".join(args) + "\n")
if {refuse}:
    sys.stderr.write("Setting pipeline to PAUSED ...\n")
    sys.stderr.write("ERROR: from element /GstPipeline:pipeline0/GstAlsaSrc:alsasrc0: Could not open audio device for recording. Device is being used by another application.\n")
    sys.exit(1)
if "rtspclientsink" in args:
    while True:
        time.sleep(1)
encodes, frame, ssrc = [], 20, 1
for word in args:
    key, _, value = word.partition("=")
    if key == "frame-size":
        frame = int(value)
    elif key == "ssrc":
        ssrc = int(value)
    elif key == "port":
        encodes.append((int(value), frame))
out = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
start, sent, seq = time.monotonic(), [0] * len(encodes), 0
while True:
    now = time.monotonic() - start
    for i, (port, ms) in enumerate(encodes):
        while (sent[i] + 1) * ms / 1000 <= now:
            header = struct.pack(">BBHII", 0x80, 96, seq & 0xFFFF, (sent[i] * ms * 48) & 0xFFFFFFFF, ssrc)
            out.sendto(header + payload, ("127.0.0.1", port))
            sent[i] += 1
            seq += 1
    time.sleep(0.005)
"#,
        refuse = if refuse { "True" } else { "False" }
    );
    let path = dir.join("gst-launch-1.0");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// What the fake in `dir` sends in each packet: an Opus TOC byte and two
/// more, then the name of `dir`.
#[cfg(unix)]
pub(in crate::video) fn payload_of(dir: &Path) -> Vec<u8> {
    let mut payload = vec![0xFC, 0x01, 0x02];
    payload.extend_from_slice(dir.file_name().unwrap().as_encoded_bytes());
    payload
}

/// The words of the first `count` runs of the fake, waiting up to 3 s for
/// them to start.
#[cfg(unix)]
pub(in crate::video) async fn runs_until(dir: &Path, count: usize) -> Vec<String> {
    let until = tokio::time::Instant::now() + Duration::from_secs(3);
    while runs(dir).len() < count && tokio::time::Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    runs(dir)
}

/// The words of each run of the fake so far.
#[cfg(unix)]
pub(in crate::video) fn runs(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("args.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[cfg(unix)]
fn environment(root: &Path, gst: PathBuf) -> Environment {
    Environment {
        root: root.to_owned(),
        source: None,
        gst,
    }
}

#[cfg(unix)]
fn choices(cameras: &[(&str, Choice)]) -> watch::Sender<BTreeMap<String, Choice>> {
    watch::channel(
        cameras
            .iter()
            .map(|(path, choice)| ((*path).to_owned(), choice.clone()))
            .collect(),
    )
    .0
}

/// Waits up to `within` for the status to pass `test`.
#[cfg(unix)]
pub(in crate::video) async fn until(
    status: &mut watch::Receiver<Status>,
    within: Duration,
    test: impl Fn(&Status) -> bool,
) -> Status {
    let reached = tokio::time::timeout(within, status.wait_for(|s| test(s)))
        .await
        .is_ok();
    let now = status.borrow().clone();
    assert!(reached, "not reached in {within:?}: {now:#?}");
    now
}

#[cfg(unix)]
fn state(status: &Status, path: &str) -> State {
    status.cameras[path].state
}

/// The packets that came in `over`, by step.
#[cfg(unix)]
async fn packets(feed: &mut Feed, over: Duration) -> Vec<Packet> {
    let mut got = Vec::new();
    let until = tokio::time::Instant::now() + over;
    while let Ok(Some(packet)) = tokio::time::timeout_at(until, feed.packets.recv()).await {
        got.push(packet);
    }
    got
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_microphone_starts_its_cameras_process_whose_steps_reach_the_sender() {
    let helper = tempfile::tempdir().unwrap();
    let gst = fake_gst(helper.path(), false);
    let tx = choices(&[("front", Choice::Auto)]);
    let root = case("one-usb");
    let mut running = start(environment(root.path(), gst), tx.subscribe())
        .await
        .unwrap();
    let status = until(&mut running.status, Duration::from_secs(3), |s| {
        s.cameras
            .get("front")
            .is_some_and(|c| c.state == State::Available)
    })
    .await;
    let front = &status.cameras["front"];
    assert_eq!(front.microphone.as_ref().unwrap().id, "usb:3-1.2");
    assert_eq!(front.reason, None);
    assert!(
        front.recording.is_some(),
        "its recording port, for the recorder"
    );
    assert_eq!(status.microphones.len(), 1);

    let payload = payload_of(helper.path());
    let got: Vec<Packet> = packets(&mut running.feed, Duration::from_millis(700))
        .await
        .into_iter()
        .filter(|p| p.payload == payload)
        .collect();
    for (step, encode) in STEPS.iter().enumerate() {
        let mine: Vec<&Packet> = got.iter().filter(|p| p.step == step).collect();
        assert!(!mine.is_empty(), "no packets of step {step}: {got:?}");
        assert!(mine.iter().all(|p| p.path == "front" && p.ssrc == 1));
        assert!(
            mine.windows(2)
                .all(|w| w[1].timestamp - w[0].timestamp == encode.samples()),
            "step {step}'s timestamps"
        );
    }
    let words = runs_until(helper.path(), 1).await;
    assert_eq!(words.len(), 1);
    assert!(
        words[0].starts_with("alsasrc device=\"hw:CARD=Device,DEV=0\" ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=1 ! tee name=t"),
        "{}",
        words[0]
    );
    assert!(
        words[0].ends_with(&format!(
            "opusenc audio-type=generic bitrate=64000 frame-size=20 bandwidth=fullband ! rtpopuspay pt=96 ssrc=1 timestamp-offset=0 ! udpsink host=127.0.0.1 port={} sync=false async=false",
            front.recording.unwrap()
        )),
        "{}",
        words[0]
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_microphone_plugged_in_starts_the_sound_and_pulled_out_stops_it() {
    let root = tempfile::tempdir().unwrap();
    crate::video::companion::microphones::tests::copy_case("none", root.path());
    let helper = tempfile::tempdir().unwrap();
    let gst = fake_gst(helper.path(), false);
    let tx = choices(&[("front", Choice::Auto)]);
    let mut running = start(environment(root.path(), gst), tx.subscribe())
        .await
        .unwrap();
    assert_eq!(
        state(&running.status.borrow(), "front"),
        State::NoMicrophone
    );
    assert!(
        runs(helper.path()).is_empty(),
        "no process without a microphone"
    );

    // The loopback case's card, plugged in as card 2.
    let asound = root.path().join("proc/asound");
    let cards = std::fs::read_to_string(asound.join("cards")).unwrap();
    std::fs::write(
        asound.join("cards"),
        format!("{cards} 2 [Loopback       ]: Loopback - Loopback PCM\n                      Loopback 1\n"),
    )
    .unwrap();
    std::fs::create_dir_all(asound.join("card2/pcm0c")).unwrap();
    let status = until(&mut running.status, Duration::from_secs(3), |s| {
        state(s, "front") == State::Available
    })
    .await;
    assert_eq!(
        status.cameras["front"].microphone.as_ref().unwrap().id,
        "card:Loopback"
    );
    assert!(
        !packets(&mut running.feed, Duration::from_millis(500))
            .await
            .is_empty()
    );

    std::fs::write(asound.join("cards"), cards).unwrap();
    std::fs::remove_dir_all(asound.join("card2")).unwrap();
    until(&mut running.status, Duration::from_secs(3), |s| {
        state(s, "front") == State::NoMicrophone
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    while running.feed.packets.try_recv().is_ok() {}
    assert!(
        packets(&mut running.feed, Duration::from_millis(500))
            .await
            .is_empty(),
        "the process stopped"
    );
    assert_eq!(runs(helper.path()).len(), 1);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_reloaded_choice_applies_at_once_and_never_takes_another_device() {
    let helper = tempfile::tempdir().unwrap();
    let gst = fake_gst(helper.path(), false);
    let tx = choices(&[("front", Choice::Auto)]);
    let root = case("several");
    let mut running = start(environment(root.path(), gst), tx.subscribe())
        .await
        .unwrap();
    let status = until(&mut running.status, Duration::from_secs(3), |s| {
        state(s, "front") == State::Available
    })
    .await;
    assert_eq!(
        status.cameras["front"].microphone.as_ref().unwrap().id,
        "usb:3-1.2",
        "the first USB one"
    );
    assert_eq!(runs_until(helper.path(), 1).await.len(), 1);

    tx.send_replace(BTreeMap::from([("front".to_owned(), Choice::None)]));
    let status = until(&mut running.status, Duration::from_secs(2), |s| {
        state(s, "front") == State::Off
    })
    .await;
    assert_eq!(
        status.cameras["front"].recording, None,
        "no recording with sound"
    );

    tx.send_replace(BTreeMap::from([(
        "front".to_owned(),
        Choice::Device("usb:1-2".into()),
    )]));
    let status = until(&mut running.status, Duration::from_secs(2), |s| {
        state(s, "front") == State::Available
    })
    .await;
    assert_eq!(
        status.cameras["front"].microphone.as_ref().unwrap().name,
        "Blue Snowball"
    );
    assert!(runs_until(helper.path(), 2).await[1].contains("hw:CARD=Snowball,DEV=0"));

    tx.send_replace(BTreeMap::from([(
        "front".to_owned(),
        Choice::Device("usb:9-9".into()),
    )]));
    let status = until(&mut running.status, Duration::from_secs(2), |s| {
        state(s, "front") == State::Unplugged
    })
    .await;
    assert_eq!(status.cameras["front"].microphone, None);
    assert_eq!(
        status.cameras["front"].reason.as_deref(),
        Some("The microphone usb:9-9 is not plugged in.")
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_device_that_refuses_is_said_and_tried_again_with_a_growing_wait() {
    let helper = tempfile::tempdir().unwrap();
    let gst = fake_gst(helper.path(), true);
    let tx = choices(&[("front", Choice::Auto)]);
    let root = case("one-usb");
    let mut running = start(environment(root.path(), gst), tx.subscribe())
        .await
        .unwrap();
    let status = until(&mut running.status, Duration::from_secs(3), |s| {
        state(s, "front") == State::Failed
    })
    .await;
    assert_eq!(
        status.cameras["front"].reason.as_deref(),
        Some(
            "Could not open audio device for recording. Device is being used by another application."
        )
    );
    assert_eq!(
        status.cameras["front"].microphone.as_ref().unwrap().id,
        "usb:3-1.2"
    );
    tokio::time::sleep(Duration::from_millis(4500)).await;
    // Started at once, then each time it ended after a wait of 1 s, then
    // 2 s, then 4 s, checked once a second: never once a second.
    let started = runs(helper.path()).len();
    assert!(
        (2..=4).contains(&started),
        "{started} starts in about 5.5 s"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn without_gstreamer_a_camera_that_wants_sound_fails_loudly_and_one_set_to_none_is_off() {
    let tx = choices(&[("down", Choice::None), ("front", Choice::Auto)]);
    let root = case("one-usb");
    let running = start(
        environment(root.path(), PathBuf::from("/nonexistent/gst-launch-1.0")),
        tx.subscribe(),
    )
    .await
    .unwrap();
    let status = running.status.borrow().clone();
    assert_eq!(state(&status, "down"), State::Off);
    let front = &status.cameras["front"];
    assert_eq!(front.state, State::Failed);
    assert_eq!(
        front.reason.as_deref(),
        Some(
            "GStreamer is not installed on the camera computer (/nonexistent/gst-launch-1.0 cannot be run). Install gstreamer1.0-tools, gstreamer1.0-alsa, gstreamer1.0-plugins-base, gstreamer1.0-plugins-good, gstreamer1.0-rtsp."
        )
    );
    assert_eq!(front.recording, None, "the record path records as before");
    assert!(!front.reason.as_deref().unwrap().contains(';'));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn two_cameras_on_one_microphone_the_second_is_refused_and_says_so() {
    let helper = tempfile::tempdir().unwrap();
    let gst = fake_gst(helper.path(), false);
    let tx = choices(&[("down", Choice::Auto), ("front", Choice::Auto)]);
    let root = case("one-usb");
    let mut running = start(environment(root.path(), gst), tx.subscribe())
        .await
        .unwrap();
    let status = until(&mut running.status, Duration::from_secs(3), |s| {
        state(s, "down") == State::Available
    })
    .await;
    assert_eq!(state(&status, "front"), State::Failed);
    assert_eq!(
        status.cameras["front"].reason.as_deref(),
        Some("USB PnP Sound Device is used by down, and one microphone serves one camera.")
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        runs(helper.path()).len(),
        1,
        "one process, the first camera's"
    );
}

#[test]
fn the_pipeline_encodes_each_step_and_the_recording_as_the_adr_says() {
    let words = pipeline(
        "audiotestsrc is-live=true wave=sine freq=440",
        [5001, 5002, 5003, 5004],
        5005,
        7,
    )
    .join(" ");
    assert!(words.starts_with("audiotestsrc is-live=true wave=sine freq=440 ! audioconvert"));
    for (bitrate, ms, band, port) in [
        (32_000, 20, "fullband", 5001),
        (24_000, 20, "fullband", 5002),
        (16_000, 40, "auto", 5003),
        (12_000, 60, "auto", 5004),
        (64_000, 20, "fullband", 5005),
    ] {
        let encode = format!(
            "t. ! queue ! opusenc audio-type=generic bitrate={bitrate} frame-size={ms} bandwidth={band} ! rtpopuspay pt=96 ssrc=7 timestamp-offset=0 ! udpsink host=127.0.0.1 port={port} sync=false async=false"
        );
        assert!(words.contains(&encode), "{encode} in {words}");
    }
    assert_eq!(words.matches("opusenc").count(), 5);
    assert_eq!(
        joining(
            "rtsp://127.0.0.1:8554/",
            "front_record",
            "front_record_sound",
            5005
        )
        .join(" "),
        "rtspclientsink name=s location=rtsp://127.0.0.1:8554/front_record_sound protocols=tcp \
         rtspsrc location=rtsp://127.0.0.1:8554/front_record latency=0 protocols=tcp ! rtph264depay ! s. \
         udpsrc address=127.0.0.1 port=5005 caps=application/x-rtp,media=audio,clock-rate=48000,encoding-name=OPUS,payload=96 \
         ! rtpjitterbuffer latency=60 ! rtpopusdepay ! s."
    );
}

#[test]
fn rtp_gives_the_ssrc_timestamp_and_payload_past_csrcs_extensions_and_padding() {
    let plain = [0x80, 96, 0, 1, 0, 0, 3, 0xC0, 0, 0, 0, 7, 0xFC, 1, 2];
    assert_eq!(rtp(&plain), Some((7, 960, &[0xFC, 1, 2][..])));
    let mut extended = vec![0xB1, 96, 0, 1, 0, 0, 0, 1, 0, 0, 0, 9, 0, 0, 0, 5];
    extended.extend([0xBE, 0xDE, 0, 1, 0x10, 0xAA, 0, 0]);
    extended.extend([0xFC, 0, 2]);
    assert_eq!(rtp(&extended), Some((9, 1, &[0xFC][..])));
    assert_eq!(rtp(&[0x40, 96, 0, 1]), None, "not RTP");
    assert_eq!(rtp(&plain[..10]), None);
}

#[test]
fn a_gstreamer_error_is_said_in_its_own_words() {
    assert_eq!(
        gst_error("ERROR: from element /GstPipeline:pipeline0/GstAlsaSrc:alsasrc0: Could not open audio device for recording. Device is being used by another application."),
        Some("Could not open audio device for recording. Device is being used by another application.".into())
    );
    assert_eq!(
        gst_error("ERROR: pipeline could not be constructed: no element \"rtspclientsink\"."),
        Some("pipeline could not be constructed: no element \"rtspclientsink\".".into())
    );
    assert_eq!(gst_error("Setting pipeline to PLAYING ..."), None);
}

#[test]
fn the_wait_doubles_to_30_s_and_starts_again_after_a_good_run() {
    let now = Instant::now();
    let mut backoff = Backoff::default();
    assert!(backoff.due(now));
    let mut waits = Vec::new();
    for _ in 0..7 {
        backoff.ended(Duration::ZERO, now);
        waits.push(backoff.until.unwrap() - now);
    }
    assert_eq!(waits, [1, 2, 4, 8, 16, 30, 30].map(Duration::from_secs));
    assert!(!backoff.due(now));
    backoff.ended(Duration::from_secs(60), now);
    assert_eq!(backoff.until.unwrap() - now, Duration::from_secs(1));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs GStreamer: sudo apt-get install the PACKAGES, then cargo test -p uxduck --bin uxduck sound -- --ignored"]
async fn the_real_pipeline_sends_each_encode_as_opus_rtp_on_its_own_port() {
    let gst = locate(Path::new("gst-launch-1.0")).expect("gst-launch-1.0 on PATH");
    let mut sockets = Vec::new();
    for _ in 0..=STEPS.len() {
        sockets.push(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    }
    let port = |i: usize| sockets[i].local_addr().unwrap().port();
    let words = pipeline(
        "audiotestsrc is-live=true wave=sine freq=440",
        [port(0), port(1), port(2), port(3)],
        port(4),
        42,
    );
    let _helper = Helper::spawn(&gst, &words).unwrap();
    let encodes: Vec<Encode> = STEPS.iter().copied().chain([RECORDING]).collect();
    let mut buffer = vec![0_u8; 2048];
    for (socket, encode) in sockets.iter().zip(&encodes) {
        let mut times = Vec::new();
        let mut bytes = 0;
        let until = tokio::time::Instant::now() + Duration::from_secs(3);
        while times.len() < 20 {
            let (length, _) = tokio::time::timeout_at(until, socket.recv_from(&mut buffer))
                .await
                .expect("RTP within 3 s")
                .unwrap();
            let (ssrc, timestamp, payload) = rtp(&buffer[..length]).unwrap();
            assert_eq!(ssrc, 42);
            assert_eq!(buffer[1] & 0x7f, 96);
            assert!(!payload.is_empty());
            bytes += payload.len();
            times.push(timestamp);
        }
        assert!(
            times
                .windows(2)
                .skip(1)
                .all(|w| w[1] - w[0] == encode.samples()),
            "{encode:?}: {times:?}"
        );
        println!(
            "{} kbit/s in {} ms: {} bytes of Opus in 20 packets, first at {}",
            encode.bitrate / 1000,
            encode.ms,
            bytes,
            times[0]
        );
    }
}
