//! The recorder's modes, its MAVLink camera and its disk, and the whole of
//! it against a fake `MediaMTX` and a fake MAVLink router.

use std::fs::File;
use std::io::Write as _;
use std::path::Path;
use std::time::SystemTime;

use uxduck_mavlink::decode::{Message, decode};
use uxduck_mavlink::sim::{armed_heartbeat_frame, command_long_frame};

use super::disk::Store;
#[cfg(unix)]
use super::disk::{go_duration, size};
use super::*;
use crate::video::sender::mediamtx::tests::Fake;

const GROUND: u8 = 255;

fn config(modes: &[Mode]) -> Config {
    Config {
        mavlink: "udp:127.0.0.1:15010".into(),
        system_id: 1,
        mediamtx_api: "http://127.0.0.1:9997".into(),
        mediamtx_rtsp: "rtsp://127.0.0.1:8554".into(),
        paths: modes
            .iter()
            .enumerate()
            .map(|(i, mode)| PathConfig {
                path: format!("cam{i}_record"),
                camera: format!("cam{i}"),
                mode: *mode,
            })
            .collect(),
    }
}

fn heartbeat(armed: bool) -> Vec<u8> {
    armed_heartbeat_frame(1, 1, 0, armed, 0)
}

fn command(component: u8, command: u16, params: [f32; 7]) -> Vec<u8> {
    command_long_frame(GROUND, 1, component, 0, command, params, 0)
}

fn start() -> Vec<u8> {
    command(CAMERA, VIDEO_START_CAPTURE, [0.0; 7])
}

fn stop() -> Vec<u8> {
    command(CAMERA, VIDEO_STOP_CAPTURE, [0.0; 7])
}

fn request(component: u8, message: u32, param2: f32) -> Vec<u8> {
    #[allow(clippy::cast_precision_loss, reason = "a message id")]
    let id = message as f32;
    command(
        component,
        REQUEST_MESSAGE,
        [id, param2, 0.0, 0.0, 0.0, 0.0, 0.0],
    )
}

fn on(recorder: &Recorder) -> Vec<bool> {
    recorder.wanted().map(|(_, on)| on).collect()
}

fn said(frames: &[Vec<u8>]) -> Vec<(u8, Message)> {
    frames
        .iter()
        .map(|frame| {
            let decoded = decode(frame).expect("a frame that decodes");
            (decoded.component_id, decoded.message)
        })
        .collect()
}

/// The `MAV_RESULT` of each acknowledgement, by command.
fn acks(frames: &[Vec<u8>]) -> Vec<(u16, u8)> {
    said(frames)
        .into_iter()
        .filter_map(|(_, m)| match m {
            Message::CommandAck {
                command, result, ..
            } => Some((command, result)),
            _ => None,
        })
        .collect()
}

fn video_status(frames: &[Vec<u8>]) -> Vec<u8> {
    said(frames)
        .into_iter()
        .filter_map(|(_, m)| match m {
            Message::CameraCaptureStatus(status) => Some(status.video_status),
            _ => None,
        })
        .collect()
}

#[test]
fn armed_mode_records_from_arming_to_disarming_and_a_ground_stop_lasts_until_the_next_arm() {
    let mut recorder = Recorder::new(&config(&[Mode::Armed]), 0);
    assert_eq!(on(&recorder), [false]);
    assert!(
        recorder.heard(&heartbeat(false), 100).is_empty(),
        "disarmed at first is no change"
    );
    assert_eq!(on(&recorder), [false]);

    let armed = recorder.heard(&heartbeat(true), 200);
    assert_eq!(on(&recorder), [true]);
    assert_eq!(video_status(&armed), [1], "the change is said at once");
    assert!(recorder.heard(&heartbeat(true), 300).is_empty());

    let stopped = recorder.heard(&stop(), 400);
    assert_eq!(acks(&stopped), [(VIDEO_STOP_CAPTURE, ACCEPTED)]);
    assert_eq!(video_status(&stopped), [0]);
    recorder.heard(&heartbeat(true), 500);
    assert_eq!(
        on(&recorder),
        [false],
        "stopped by the ground while still armed"
    );

    recorder.heard(&heartbeat(false), 600);
    assert_eq!(on(&recorder), [false]);
    recorder.heard(&heartbeat(true), 700);
    assert_eq!(on(&recorder), [true], "the next arm records again");
    recorder.heard(&heartbeat(false), 800);
    assert_eq!(on(&recorder), [false]);

    let started = recorder.heard(&start(), 900);
    assert_eq!(acks(&started), [(VIDEO_START_CAPTURE, ACCEPTED)]);
    assert_eq!(
        on(&recorder),
        [true],
        "a ground Start records while disarmed"
    );
    recorder.heard(&heartbeat(true), 1000);
    recorder.heard(&heartbeat(false), 1100);
    assert_eq!(
        on(&recorder),
        [true],
        "the ground's recording outlasts a disarm"
    );
    recorder.heard(&stop(), 1200);
    assert_eq!(on(&recorder), [false], "and ends at the ground's Stop");

    recorder.heard(&heartbeat(true), 1300);
    assert_eq!(on(&recorder), [true]);
    recorder.heard(&start(), 1400);
    recorder.heard(&heartbeat(false), 1500);
    assert_eq!(
        on(&recorder),
        [true],
        "a Start while armed makes it the ground's recording"
    );
    recorder.heard(&stop(), 1600);
    assert_eq!(on(&recorder), [false]);
}

#[test]
fn start_and_stop_are_acknowledged_in_any_state_and_only_on_the_cameras_own_component() {
    let mut recorder = Recorder::new(&config(&[Mode::Off]), 0);
    assert_eq!(
        acks(&recorder.heard(&stop(), 0)),
        [(VIDEO_STOP_CAPTURE, ACCEPTED)],
        "already stopped"
    );
    assert_eq!(
        acks(&recorder.heard(&start(), 0)),
        [(VIDEO_START_CAPTURE, ACCEPTED)]
    );
    assert_eq!(
        acks(&recorder.heard(&start(), 0)),
        [(VIDEO_START_CAPTURE, ACCEPTED)],
        "already recording"
    );
    assert_eq!(on(&recorder), [true]);
    assert!(
        recorder
            .heard(&command(0, VIDEO_STOP_CAPTURE, [0.0; 7]), 0)
            .is_empty()
    );
    assert_eq!(
        on(&recorder),
        [true],
        "a Stop to every component is not this camera's"
    );
    recorder.heard(&stop(), 0);
    assert!(
        recorder
            .heard(&command(0, VIDEO_START_CAPTURE, [0.0; 7]), 0)
            .is_empty()
    );
    assert_eq!(on(&recorder), [false], "nor is a Start");
}

#[test]
fn a_reload_applies_a_changed_mode_and_keeps_the_grounds_recording() {
    let mut recorder = Recorder::new(&config(&[Mode::Armed, Mode::Armed]), 0);
    recorder.heard(&heartbeat(false), 0);
    recorder.heard(&command(CAMERA + 1, VIDEO_START_CAPTURE, [0.0; 7]), 0);
    assert_eq!(on(&recorder), [false, true]);

    let said = recorder.reconfigure(&config(&[Mode::Always, Mode::Off]), 100);
    assert_eq!(
        on(&recorder),
        [true, true],
        "always records at once and the ground's recording goes on"
    );
    assert_eq!(video_status(&said), [1]);
    recorder.heard(&command(CAMERA + 1, VIDEO_STOP_CAPTURE, [0.0; 7]), 200);
    assert_eq!(on(&recorder), [true, false]);

    let said = recorder.reconfigure(&config(&[Mode::Off, Mode::Off]), 300);
    assert_eq!(
        (on(&recorder), video_status(&said)),
        (vec![false, false], vec![0])
    );
    recorder.heard(&heartbeat(true), 400);
    recorder.reconfigure(&config(&[Mode::Armed, Mode::Off]), 500);
    assert_eq!(
        on(&recorder),
        [true, false],
        "armed while the vehicle is armed records"
    );

    let mut moved = config(&[Mode::Armed, Mode::Always]);
    moved.paths[1].path = "elsewhere".into();
    assert!(recorder.reconfigure(&moved, 600).is_empty());
    assert_eq!(
        on(&recorder),
        [true, false],
        "a path that changed waits for a restart"
    );
}

#[test]
fn always_records_from_the_start_and_off_waits_for_the_ground() {
    let mut recorder = Recorder::new(&config(&[Mode::Always, Mode::Off]), 0);
    assert_eq!(on(&recorder), [true, false]);
    recorder.heard(&heartbeat(true), 100);
    assert_eq!(on(&recorder), [true, false], "arming changes neither");
    recorder.heard(&heartbeat(false), 200);
    assert_eq!(on(&recorder), [true, false], "nor does disarming");

    recorder.heard(&stop(), 300);
    recorder.heard(&heartbeat(true), 400);
    recorder.heard(&heartbeat(false), 500);
    recorder.heard(&heartbeat(true), 600);
    assert_eq!(on(&recorder), [false, false], "a Stop holds until a Start");
    recorder.heard(&start(), 700);
    assert_eq!(on(&recorder), [true, false]);

    let second = CAMERA + 1;
    let started = recorder.heard(&command(second, VIDEO_START_CAPTURE, [0.0; 7]), 800);
    assert_eq!(
        said(&started)[0].0,
        second,
        "the second path is the second camera"
    );
    assert_eq!(
        on(&recorder),
        [true, true],
        "off records when the ground asks"
    );
}

#[test]
fn a_recorder_that_starts_again_mid_flight_records_at_the_first_heartbeat() {
    let mut recorder = Recorder::new(&config(&[Mode::Armed]), 0);
    recorder.heard(&heartbeat(true), 100);
    assert_eq!(on(&recorder), [true]);
    drop(recorder);
    let mut again = Recorder::new(&config(&[Mode::Armed]), 0);
    assert_eq!(on(&again), [false]);
    again.heard(&heartbeat(true), 50);
    assert_eq!(on(&again), [true], "armed when it is heard is an arm");
}

#[test]
fn another_systems_flight_controller_and_commands_are_not_this_vehicles() {
    let mut recorder = Recorder::new(&config(&[Mode::Armed]), 0);
    assert!(
        recorder
            .heard(&armed_heartbeat_frame(2, 1, 0, true, 0), 0)
            .is_empty()
    );
    assert!(
        recorder
            .heard(
                &command_long_frame(GROUND, 2, CAMERA, 0, VIDEO_START_CAPTURE, [0.0; 7], 0),
                0
            )
            .is_empty()
    );
    assert_eq!(on(&recorder), [false]);
}

#[test]
fn the_camera_says_what_it_is_its_storage_and_its_capture_status() {
    let mut recorder = Recorder::new(&config(&[Mode::Armed]), 0);
    let space = Space {
        total: 64 * 1_048_576 * 1024,
        free: 10 * 1_048_576 * 1024,
    };
    recorder.space(0, space, true);

    let ticked = said(&recorder.tick(1000));
    assert!(
        matches!(&ticked[..], [(CAMERA, Message::Heartbeat { mav_type, .. })] if mav_type == "Camera")
    );

    let info = recorder.heard(&request(CAMERA, CAMERA_INFORMATION, 0.0), 1000);
    assert_eq!(acks(&info), [(REQUEST_MESSAGE, ACCEPTED)]);
    let Some((_, Message::CameraInformation(info))) = said(&info).pop() else {
        panic!("no CAMERA_INFORMATION");
    };
    assert_eq!(
        (info.vendor.as_str(), info.model.as_str()),
        ("UXDuck companion", "cam0")
    );
    assert_eq!(info.flags, 1, "CAMERA_CAP_FLAGS_CAPTURE_VIDEO alone");

    let storage = recorder.heard(&request(CAMERA, STORAGE_INFORMATION, 0.0), 1000);
    let Some((_, Message::StorageInformation(storage))) = said(&storage).pop() else {
        panic!("no STORAGE_INFORMATION");
    };
    assert!((storage.total_mib - 65_536.0).abs() < 0.5);
    assert!((storage.available_mib - 10_240.0).abs() < 0.5);
    assert!((storage.used_mib - 55_296.0).abs() < 0.5);

    recorder.heard(&heartbeat(true), 2000);
    let status = recorder.heard(&request(0, CAMERA_CAPTURE_STATUS, 0.0), 3500);
    assert_eq!(
        acks(&status),
        [(REQUEST_MESSAGE, ACCEPTED)],
        "every component is asked"
    );
    let Some((_, Message::CameraCaptureStatus(status))) = said(&status).pop() else {
        panic!("no CAMERA_CAPTURE_STATUS");
    };
    assert_eq!((status.video_status, status.recording_time_ms), (1, 1500));
    assert!((status.available_mib - 10_240.0).abs() < 0.5);
    assert_eq!(
        video_status(&recorder.tick(4000)),
        [1],
        "once a second while recording"
    );

    assert_eq!(
        acks(&recorder.heard(&request(CAMERA, 269, 0.0), 0)),
        [(REQUEST_MESSAGE, DENIED)]
    );
    assert!(
        recorder.heard(&request(0, 269, 0.0), 0).is_empty(),
        "another component's to answer"
    );
    assert_eq!(
        acks(&recorder.heard(&command(CAMERA, 203, [0.0; 7]), 0)),
        [(203, UNSUPPORTED)]
    );
    assert!(recorder.heard(&command(0, 203, [0.0; 7]), 0).is_empty());
}

#[test]
fn a_start_with_no_room_is_refused_and_said() {
    let mut recorder = Recorder::new(&config(&[Mode::Off]), 0);
    recorder.space(
        0,
        Space {
            total: 100,
            free: 0,
        },
        false,
    );
    let refused = recorder.heard(&start(), 0);
    assert_eq!(
        acks(&refused),
        [(VIDEO_START_CAPTURE, TEMPORARILY_REJECTED)]
    );
    assert!(said(&refused).iter().any(|(_, m)| matches!(m, Message::StatusText { text, .. } if text == "No room to record cam0, the disk is full")));
    assert_eq!(on(&recorder), [false]);
}

#[test]
fn a_configuration_that_cannot_work_says_why() {
    let mut seven = config(&[Mode::Armed; 7]);
    assert!(
        seven
            .check()
            .unwrap_err()
            .contains("camera components for 6")
    );
    seven.paths.truncate(2);
    seven.paths[1].path = seven.paths[0].path.clone();
    assert!(
        seven
            .check()
            .unwrap_err()
            .contains("two cameras have the record_path")
    );
    let mut own = config(&[Mode::Armed]);
    own.paths[0].path = own.paths[0].camera.clone();
    assert!(own.check().unwrap_err().contains("records its own path"));
    let mut empty = config(&[Mode::Armed]);
    empty.paths[0].path = String::new();
    assert!(empty.check().unwrap_err().contains("empty record_path"));
    let mut router = config(&[Mode::Armed]);
    router.mavlink = "127.0.0.1:15010".into();
    assert!(router.check().unwrap_err().contains("udp:HOST:PORT"));
    config(&[Mode::Armed]).check().unwrap();
}

fn record_conf(dir: &Path, bitrate: u64, segment: &str) -> serde_json::Value {
    serde_json::json!({
        "source": "rpiCamera",
        "rpiCameraSecondary": true,
        "rpiCameraBitrate": bitrate,
        "rpiCameraFPS": 30,
        "record": false,
        "recordPath": format!("{}/%path/%Y-%m-%d_%H-%M-%S-%f", dir.display()),
        "recordFormat": "fmp4",
        "recordSegmentDuration": segment,
    })
}

/// On Unix alone: `recordPath` is a folder on the camera computer, whose disk
/// the recorder reads on Unix only (`disk::space`), and Windows does not take
/// these paths as absolute.
#[cfg(unix)]
#[test]
fn the_store_is_the_paths_own_folder_and_room_for_one_segment() {
    let store = Store::of(
        "front_record",
        &record_conf(Path::new("/var/lib/uxduck/recordings"), 4_000_000, "5m"),
    )
    .unwrap();
    assert_eq!(
        store.dir,
        Path::new("/var/lib/uxduck/recordings/front_record")
    );
    assert_eq!(store.extension, "mp4");
    assert_eq!(store.reserve, 150_000_000, "4 Mbit/s for five minutes");

    let with = |path: &str| serde_json::json!({ "recordPath": path });
    for wrong in [
        "./recordings/%path/%Y",
        "/var/lib/%Y/%path/x",
        "/var/lib/uxduck/%Y-%m",
        "/var/../%path/%Y",
    ] {
        assert!(
            Store::of("front_record", &with(wrong))
                .unwrap_err()
                .contains("absolute folder"),
            "{wrong}"
        );
    }
    let other = Store::of("usb", &serde_json::json!({ "source": "rtsp://cam", "recordPath": "/r/%path/%Y", "recordFormat": "mpegts" })).unwrap();
    assert_eq!(
        (other.extension, other.reserve),
        ("ts", 0),
        "an unknown bitrate leaves the largest segment to stand in"
    );

    assert_eq!(go_duration("5m"), Some(Duration::from_mins(5)));
    assert_eq!(go_duration("1h30m0s"), Some(Duration::from_mins(90)));
    assert_eq!(go_duration("1.5h"), Some(Duration::from_mins(90)));
    assert_eq!(go_duration("500ms"), Some(Duration::from_millis(500)));
    assert_eq!(go_duration("0"), Some(Duration::ZERO));
    assert_eq!(go_duration("5x"), None);
    assert_eq!(go_duration("m"), None);
    assert_eq!(
        (
            size(450_000_000),
            size(5_005_529),
            size(9_687_040),
            size(1_234_000_000)
        ),
        (
            "450 MB".to_owned(),
            "5 MB".to_owned(),
            "10 MB".to_owned(),
            "1.2 GB".to_owned()
        )
    );
}

/// A segment of `bytes` last written `age` seconds before now.
fn segment(dir: &Path, name: &str, bytes: usize, age: u64) {
    std::fs::create_dir_all(dir).unwrap();
    let mut file = File::create(dir.join(name)).unwrap();
    file.write_all(&vec![0; bytes]).unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(age))
        .unwrap();
}

/// A disk of `capacity` bytes that holds only what is under `root`.
fn disk(root: &Path, capacity: u64) -> impl Fn(&Path) -> std::io::Result<Space> {
    let root = root.to_owned();
    move |_| {
        let mut used = 0;
        let mut folders = vec![root.clone()];
        while let Some(folder) = folders.pop() {
            for entry in std::fs::read_dir(folder)? {
                let meta = entry?.path();
                if meta.is_dir() {
                    folders.push(meta);
                } else {
                    used += std::fs::metadata(&meta)?.len();
                }
            }
        }
        Ok(Space {
            total: capacity,
            free: capacity.saturating_sub(used),
        })
    }
}

#[test]
fn a_full_disk_loses_the_oldest_earlier_segments_and_never_the_current_recordings() {
    let root = tempfile::tempdir().unwrap();
    // 1000 bytes a second for ten seconds: 10 000 bytes for the segment
    // being written.
    let store = Store::of("front_record", &record_conf(root.path(), 8_000, "10s")).unwrap();
    segment(&store.dir, "2026-11-20_10-00-00-000000.mp4", 6_000, 300);
    segment(&store.dir, "2026-11-20_10-05-00-000000.mp4", 6_000, 200);
    segment(&store.dir, "2026-11-21_09-00-00-000000.mp4", 6_000, 100);
    segment(&store.dir, "notes.txt", 10, 400);
    let earlier = store.earlier().unwrap();
    assert_eq!(earlier.len(), 3, "segments alone");
    segment(&store.dir, "2026-11-22_12-00-00-000000.mp4", 6_000, 0);

    let made = store
        .make_room(&earlier, disk(root.path(), 30_000))
        .unwrap();
    assert_eq!((made.deleted, made.files, made.short), (6_000, 1, false));
    assert!(made.space.free >= 10_000);
    assert!(
        !store.dir.join("2026-11-20_10-00-00-000000.mp4").exists(),
        "the oldest went"
    );
    assert!(
        store.dir.join("2026-11-20_10-05-00-000000.mp4").exists(),
        "no more than needed"
    );
    assert!(store.dir.join("notes.txt").exists(), "only segments");

    let made = store
        .make_room(&earlier, disk(root.path(), 15_000))
        .unwrap();
    assert_eq!(
        (made.files, made.short),
        (2, true),
        "every earlier one went and still short"
    );
    let left: Vec<_> = store
        .segments()
        .unwrap()
        .into_iter()
        .map(|s| s.path)
        .collect();
    assert_eq!(
        left,
        [store.dir.join("2026-11-22_12-00-00-000000.mp4")],
        "the current recording's kept"
    );
}

#[test]
fn there_is_room_to_record_when_earlier_recordings_can_make_it() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::of("front_record", &record_conf(root.path(), 8_000, "10s")).unwrap();
    let full = |free| Space {
        total: 100_000,
        free,
    };
    assert!(
        !store.room(full(9_999), None).unwrap(),
        "nothing to delete and too little free"
    );
    assert!(store.room(full(10_000), None).unwrap());
    segment(&store.dir, "2026-11-20_10-00-00-000000.mp4", 6_000, 100);
    assert!(
        store.room(full(4_000), None).unwrap(),
        "before a recording every segment may go"
    );
    let none = std::collections::HashSet::new();
    assert!(
        !store.room(full(4_000), Some(&none)).unwrap(),
        "a recording's own segments may not"
    );
}

/// A long `STATUSTEXT` put back together, as the ground does.
fn whole_text(frames: &[Vec<u8>]) -> Vec<(u8, String)> {
    let mut whole: Vec<(u8, String)> = Vec::new();
    let mut open = false;
    for (_, message) in said(frames) {
        let Message::StatusText {
            severity,
            text,
            id,
            full,
            ..
        } = message
        else {
            continue;
        };
        if open {
            whole.last_mut().unwrap().1.push_str(&text);
        } else {
            whole.push((severity, text));
        }
        open = id != 0 && full;
    }
    whole
}

#[tokio::test]
async fn deleting_earlier_recordings_and_a_disk_nearly_full_are_warnings_said_whole() {
    let tmp = tempfile::tempdir().unwrap();
    let conf = record_conf(tmp.path(), 8_000_000, "10s");
    let fake = Fake::start(&[("front_record", conf.clone())]).await;
    let api = Api::new(&fake.url).unwrap();
    let store = Store::of("front_record", &conf).unwrap();
    // Two earlier segments of 6 MB, then a recording that needs 10 MB.
    segment(&store.dir, "2026-11-20_10-00-00-000000.mp4", 6_000_000, 300);
    segment(&store.dir, "2026-11-20_10-05-00-000000.mp4", 6_000_000, 200);
    let mut kept = Kept::default();
    let roomy = disk(tmp.path(), 100_000_000);
    let (_, said) = keep(&api, "front_record", true, &mut kept, true, &roomy, None)
        .await
        .unwrap();
    assert!(said.unwrap().2.is_empty(), "room enough, nothing to say");
    segment(&store.dir, "2026-11-22_12-00-00-000000.mp4", 6_000_000, 0);

    let full = disk(tmp.path(), 24_000_000);
    let (_, said) = keep(&api, "front_record", true, &mut kept, true, &full, None)
        .await
        .unwrap();
    let (_, _, notes) = said.unwrap();
    let [(severity, words)] = &notes[..] else {
        panic!("one note: {notes:?}");
    };
    let (severity, words) = (*severity, words.clone());
    assert_eq!(severity, WARNING);
    assert_eq!(
        words,
        "Deleted 6 MB of earlier recordings to keep recording, 12 MB free"
    );
    let mut recorder = Recorder::new(&config(&[Mode::Armed]), 0);
    assert_eq!(
        whole_text(&recorder.say(0, severity, &words)),
        [(WARNING, words.clone())],
        "the ground hears it whole, though it is longer than one STATUSTEXT"
    );

    let fuller = disk(tmp.path(), 14_000_000);
    let (_, said) = keep(&api, "front_record", true, &mut kept, true, &fuller, None)
        .await
        .unwrap();
    let (_, room, notes) = said.unwrap();
    assert_eq!(
        notes,
        [
            (
                WARNING,
                "Deleted 6 MB of earlier recordings to keep recording, 8 MB free".to_owned()
            ),
            (
                WARNING,
                "Disk nearly full, 8 MB free and only this recording left".to_owned()
            ),
        ],
        "both are said, the deletion and that nothing is left to delete"
    );
    assert!(!room);
    assert_eq!(
        store.segments().unwrap().len(),
        1,
        "the current recording is never deleted"
    );
}

#[tokio::test]
async fn record_follows_arming_through_a_restart_of_mediamtx() {
    let root = tempfile::tempdir().unwrap();
    let fake = Fake::start(&[("cam0_record", record_conf(root.path(), 4_000_000, "5m"))]).await;
    let router = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = config(&[Mode::Armed]);
    config.mavlink = format!("udp:{}", router.local_addr().unwrap());
    config.mediamtx_api = fake.url.clone();
    config.check().unwrap();
    let (recording_tx, mut recording_fps) = tokio::sync::watch::channel(BTreeMap::new());
    tokio::spawn(run(
        config.clone(),
        tokio::sync::watch::channel(config).1,
        recording_tx,
        Sounds {
            status: tokio::sync::watch::channel(sound::Status::default()).1,
            gst: PathBuf::from("/nonexistent/gst-launch-1.0"),
        },
    ));

    let mut buffer = vec![0; 65_536];
    let hear = async |router: &UdpSocket, buffer: &mut Vec<u8>| {
        let (length, from) = tokio::time::timeout(Duration::from_secs(5), router.recv_from(buffer))
            .await
            .unwrap()
            .unwrap();
        (
            said(
                &split(&buffer[..length])
                    .frames
                    .iter()
                    .map(|f| f.to_vec())
                    .collect::<Vec<_>>(),
            ),
            from,
        )
    };
    let (first, recorder) = hear(&router, &mut buffer).await;
    assert!(
        matches!(&first[..], [(CAMERA, Message::Heartbeat { mav_type, .. })] if mav_type == "Camera")
    );
    router.send_to(&heartbeat(false), recorder).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        fake.records().is_empty(),
        "disarmed and not recording is what MediaMTX has"
    );

    router.send_to(&heartbeat(true), recorder).await.unwrap();
    assert_eq!(
        fake.wait_for_records(1, Duration::from_secs(3)).await,
        [("cam0_record".to_owned(), true)]
    );
    let mut recording = false;
    for _ in 0..5 {
        let (frames, _) = hear(&router, &mut buffer).await;
        recording |= frames
            .iter()
            .any(|(_, m)| matches!(m, Message::CameraCaptureStatus(s) if s.video_status == 1));
    }
    assert!(recording, "the ground hears it record");
    tokio::time::timeout(
        Duration::from_secs(3),
        recording_fps.wait_for(|r| r.get("cam0") == Some(&30.0)),
    )
    .await
    .expect("the sender hears the recording's frame rate")
    .unwrap();

    fake.restart();
    assert_eq!(
        fake.path("cam0_record")["record"],
        false,
        "a restart forgets the API's change"
    );
    let records = fake.wait_for_records(2, Duration::from_secs(3)).await;
    assert_eq!(
        records.last(),
        Some(&("cam0_record".to_owned(), true)),
        "set again after the restart"
    );

    router.send_to(&heartbeat(false), recorder).await.unwrap();
    let records = fake.wait_for_records(3, Duration::from_secs(3)).await;
    assert_eq!(records.last(), Some(&("cam0_record".to_owned(), false)));
    assert_eq!(fake.path("cam0_record")["record"], false);
    tokio::time::timeout(
        Duration::from_secs(3),
        recording_fps.wait_for(BTreeMap::is_empty),
    )
    .await
    .expect("the frame rate is the sender's own again")
    .unwrap();
}

fn sound_status(
    state: sound::State,
    recording: Option<u16>,
    reason: Option<&str>,
) -> sound::Status {
    sound::Status {
        microphones: Vec::new(),
        cameras: BTreeMap::from([(
            "cam0".to_owned(),
            sound::CameraSound {
                state,
                microphone: None,
                reason: reason.map(str::to_owned),
                recording,
            },
        )]),
    }
}

/// Waits up to 5 s for the record changes to end with `last`.
#[cfg(unix)]
async fn records_end_with(fake: &Fake, last: &[(&str, bool)]) -> Vec<(String, bool)> {
    let last: Vec<(String, bool)> = last.iter().map(|(p, on)| ((*p).to_owned(), *on)).collect();
    let until = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let records = fake.records();
        if records.ends_with(&last) {
            return records;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "{records:?} does not end with {last:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn with_sound_the_recording_moves_to_the_sound_path_once_it_is_ready_and_never_waits_for_it()
{
    let root = tempfile::tempdir().unwrap();
    let conf = record_conf(root.path(), 4_000_000, "5m");
    let fake = Fake::start(&[("cam0_record", conf.clone())]).await;
    let router = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = config(&[Mode::Always]);
    config.mavlink = format!("udp:{}", router.local_addr().unwrap());
    config.mediamtx_api = fake.url.clone();
    let helper = tempfile::tempdir().unwrap();
    let gst = sound::tests::fake_gst(helper.path(), false);
    let (status_tx, status) =
        tokio::sync::watch::channel(sound_status(sound::State::Available, Some(5005), None));
    tokio::spawn(run(
        config.clone(),
        tokio::sync::watch::channel(config).1,
        tokio::sync::watch::channel(BTreeMap::new()).0,
        Sounds { status, gst },
    ));

    // Until the joined path is ready, the record path records the picture.
    records_end_with(&fake, &[("cam0_record", true)]).await;
    assert_eq!(fake.added(), ["cam0_record_sound"]);
    let added = fake.path("cam0_record_sound");
    assert_eq!(
        added["recordPath"], conf["recordPath"],
        "the record path's own folder pattern"
    );
    assert_eq!(added["recordFormat"], "fmp4");
    assert_eq!(added["recordSegmentDuration"], "5m");
    let joins = sound::tests::runs_until(helper.path(), 1).await;
    assert_eq!(joins.len(), 1);
    assert_eq!(
        joins[0],
        sound::joining(
            "rtsp://127.0.0.1:8554",
            "cam0_record",
            "cam0_record_sound",
            5005
        )
        .join(" ")
    );

    // Ready: the sound path records first, then the picture alone stops.
    fake.publish("cam0_record_sound", true);
    records_end_with(
        &fake,
        &[("cam0_record_sound", true), ("cam0_record", false)],
    )
    .await;

    // A restart of MediaMTX forgets the added path: it is added again, and
    // the picture records alone until the joined path is ready again.
    fake.restart();
    records_end_with(&fake, &[("cam0_record", true)]).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(fake.added(), ["cam0_record_sound", "cam0_record_sound"]);
    assert!(fake.has("cam0_record_sound"));
    fake.publish("cam0_record_sound", true);
    records_end_with(
        &fake,
        &[("cam0_record_sound", true), ("cam0_record", false)],
    )
    .await;

    // Set to none: the joining stops and the picture records alone.
    status_tx.send_replace(sound_status(sound::State::Off, None, None));
    records_end_with(
        &fake,
        &[("cam0_record_sound", false), ("cam0_record", true)],
    )
    .await;
    assert_eq!(fake.path("cam0_record_sound")["record"], false);
    assert_eq!(
        sound::tests::runs(helper.path()).len(),
        1,
        "never started again"
    );
}

#[test]
fn why_a_camera_has_no_sound_is_said_to_the_ground_once_as_a_warning() {
    let config = config(&[Mode::Armed]);
    let mut recorder = Recorder::new(&config, 0);
    let mut said = BTreeMap::new();
    let failed = sound_status(
        sound::State::Failed,
        None,
        Some("GStreamer is not installed on the camera computer."),
    );
    assert_eq!(
        whole_text(&say_sound(&mut recorder, &config, &failed, &mut said)),
        [(
            WARNING,
            "No sound from cam0. GStreamer is not installed on the camera computer.".to_owned()
        )]
    );
    assert!(
        say_sound(&mut recorder, &config, &failed, &mut said).is_empty(),
        "once"
    );
    let fine = sound_status(sound::State::NoMicrophone, Some(5005), None);
    assert!(say_sound(&mut recorder, &config, &fine, &mut said).is_empty());
    assert_eq!(
        whole_text(&say_sound(&mut recorder, &config, &failed, &mut said)).len(),
        1,
        "again once it came back"
    );
}
