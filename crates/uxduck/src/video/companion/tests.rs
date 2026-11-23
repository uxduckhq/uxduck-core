//! The companion's file: the shape the app's vehicle page reads and writes,
//! its defaults and the errors a person can act on.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::*;

const PLANE: &str = r#"
[video]
interfaces = ["eth0", "tailscale0"]

[mavlink]
router = "udp:127.0.0.1:15010"
system_id = 1

[cameras.front]
follow = true
floor_bitrate = 150000
record_path = "front_record"
record = "armed"
"#;

fn error(text: &str) -> String {
    File::parse(text).unwrap_err()
}

#[test]
fn the_planes_file_turns_on_the_sender_and_the_recorder() {
    let file = File::parse(PLANE).unwrap();
    let video = file.video.as_ref().unwrap();
    assert_eq!(video.interfaces, ["eth0", "tailscale0"]);
    assert_eq!(video.mediamtx_api, "http://127.0.0.1:9997");
    assert_eq!(video.mediamtx_rtsp, "rtsp://127.0.0.1:8554");
    assert_eq!(video.whep, "0.0.0.0:8889".parse().unwrap());
    assert_eq!(video.udp_port, 8189);
    assert_eq!(
        file.sender_cameras(),
        [CameraConfig {
            path: "front".into(),
            floor_bitrate: 150_000,
            follow: true,
        }]
    );
    assert_eq!(
        file.recorder(),
        Some(onboard_recording::Config {
            mavlink: "udp:127.0.0.1:15010".into(),
            system_id: 1,
            mediamtx_api: "http://127.0.0.1:9997".into(),
            mediamtx_rtsp: "rtsp://127.0.0.1:8554".into(),
            paths: vec![PathConfig {
                path: "front_record".into(),
                camera: "front".into(),
                mode: Mode::Armed,
            }],
        })
    );
}

#[test]
fn what_a_file_leaves_out_has_a_default() {
    let file = File::parse(
        "[video]\ninterfaces = [\"eth0\"]\nmediamtx_api = \"http://user:pass@127.0.0.1:9998\"\n[cameras.front]\nrecord_path = \"front_record\"\n[cameras.down]\nfollow = false\n",
    )
    .unwrap();
    assert_eq!(file.mavlink, Mavlink::default());
    assert_eq!(
        (file.mavlink.router.as_str(), file.mavlink.system_id),
        ("udp:127.0.0.1:15010", 1)
    );
    let cameras = file.sender_cameras();
    assert_eq!(
        cameras
            .iter()
            .map(|c| (c.path.as_str(), c.follow, c.floor_bitrate))
            .collect::<Vec<_>>(),
        [("down", false, 150_000), ("front", true, 150_000)],
        "a camera follows unless it says not to"
    );
    let recorder = file.recorder().unwrap();
    assert_eq!(
        recorder.paths.len(),
        1,
        "a camera without record_path does not record"
    );
    assert_eq!(
        recorder.paths[0].mode,
        Mode::Armed,
        "record is armed when unset"
    );
    assert_eq!(recorder.mediamtx_api, "http://user:pass@127.0.0.1:9998");

    let recorder_alone =
        File::parse("[cameras.front]\nrecord_path = \"front_record\"\nrecord = \"always\"\n")
            .unwrap();
    assert!(recorder_alone.video.is_none());
    let recorder = recorder_alone.recorder().unwrap();
    assert_eq!(recorder.mediamtx_api, "http://127.0.0.1:9997");
    assert_eq!(recorder.paths[0].mode, Mode::Always);
    assert!(
        File::parse("[video]\ninterfaces = [\"eth0\"]\n[cameras.front]\n")
            .unwrap()
            .recorder()
            .is_none()
    );
}

#[test]
fn a_file_that_cannot_work_says_why() {
    assert!(error("").contains("turns on no feature"));
    assert!(error("[cameras.front]\nfollow = true\n").contains("turns on no feature"));
    assert!(
        error("[cameras.front]\nrecord = \"always\"\n").contains("has record but no record_path")
    );
    assert!(error("[video]\ninterfaces = [\"eth0\"]\n").contains("[video] has no camera to send"));
    assert!(error("[video]\ninterfaces = []\n[cameras.front]\n").contains("names no interface"));
    assert!(
        error("[video]\ninterfaces = [\"eth0\"]\n[cameras.\"/front\"]\n")
            .contains("is not a MediaMTX path")
    );
    assert!(
        error("[video]\ninterfaces = [\"eth0\"]\n[cameras.front]\nfloor_bitrate = 0\n")
            .contains("floor_bitrate above 0")
    );
    assert!(
        error("[video]\ninterfaces = [\"eth0\"]\nudp_port = 0\n[cameras.front]\n")
            .contains("udp_port")
    );
    assert!(
        error("[video]\ninterfaces = [\"eth0\"]\n[cameras.front]\nfolow = true\n")
            .contains("does not read")
    );
    assert!(error("[recorder]\n").contains("does not read"));
    assert!(
        error("[cameras.front]\nrecord_path = \"front_record\"\nrecord = \"armd\"\n")
            .contains("does not read")
    );
    assert!(
        error("[cameras.front]\nrecord_path = \"r\"\n[cameras.down]\nrecord_path = \"r\"\n")
            .contains("two cameras have the record_path")
    );
    assert!(error("[cameras.front]\nrecord_path = \"front\"\n").contains("records its own path"));
    assert!(
        error("[mavlink]\nrouter = \"127.0.0.1:15010\"\n[cameras.front]\nrecord_path = \"front_record\"\n")
            .contains("udp:HOST:PORT")
    );
    let missing = File::read(&PathBuf::from("/nonexistent/companion.toml")).unwrap_err();
    assert!(
        missing.starts_with("cannot read /nonexistent/companion.toml"),
        "{missing}"
    );
}

#[test]
fn a_reload_applies_the_recording_modes_and_names_the_rest() {
    let file = File::parse(PLANE).unwrap();
    let modes = File::parse(&PLANE.replace("record = \"armed\"", "record = \"off\"")).unwrap();
    assert!(!file.beyond_modes(&modes));
    let unset = File::parse(&PLANE.replace("record = \"armed\"\n", "")).unwrap();
    assert!(!file.beyond_modes(&unset), "an unset mode is a mode too");
    let floor = File::parse(&PLANE.replace("150000", "200000")).unwrap();
    assert!(file.beyond_modes(&floor));
    let path = File::parse(&PLANE.replace("front_record", "front_rec")).unwrap();
    assert!(file.beyond_modes(&path));
    let router = File::parse(&PLANE.replace("15010", "15011")).unwrap();
    assert!(file.beyond_modes(&router));
}

#[test]
fn a_cameras_microphone_is_auto_unless_set_and_a_reload_applies_it() {
    let file = File::parse(PLANE).unwrap();
    assert_eq!(file.cameras["front"].microphone, None);
    assert_eq!(
        file.microphones(),
        BTreeMap::from([("front".to_owned(), Choice::Auto)])
    );
    for (line, choice) in [
        ("auto", Choice::Auto),
        ("none", Choice::None),
        ("usb:3-1.2", Choice::Device("usb:3-1.2".into())),
        ("card:Loopback", Choice::Device("card:Loopback".into())),
    ] {
        let chosen = File::parse(&format!("{PLANE}microphone = \"{line}\"\n")).unwrap();
        assert_eq!(chosen.microphones()["front"], choice);
        assert!(
            !file.beyond_modes(&chosen),
            "a changed microphone applies on a reload: {line}"
        );
    }
}

#[test]
fn a_microphone_that_is_not_auto_none_or_a_devices_id_says_what_it_takes() {
    for wrong in ["hw:1,0", "Auto", "usb:", "default"] {
        let said = error(&format!("{PLANE}microphone = \"{wrong}\"\n"));
        assert!(said.contains("does not read"), "{said}");
        assert!(
            said.contains(&format!(
                "microphone = \"{wrong}\" is not \"auto\", \"none\" or a device's id such as \"usb:1-1.2\" or \"card:Loopback\""
            )),
            "{said}"
        );
    }
    assert!(error(&format!("{PLANE}microphone = 2\n")).contains("does not read"));
}
