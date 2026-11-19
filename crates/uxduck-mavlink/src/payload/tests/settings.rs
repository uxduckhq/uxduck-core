//! The simulated camera's definition file and its own settings (S411),
//! against the guide's pages on the camera definition file, the extended
//! parameter protocol and MAVLink FTP.

use mavio::dialects::ardupilotmega::enums::{CameraMode, ParamAck};

use super::*;
use crate::camera_params::{Out, Value};
use crate::files::{Event, FtpClient, Op};
use crate::payload::OnLink;
use crate::payload::camera::settings::{DEFINITION, DEFINITION_URI, DEFINITION_VERSION, NAMES};

fn camera() -> Payload {
    Payload::new(VEHICLE).with_camera(None)
}

fn ask(payload: &mut Payload, out: &Out) -> Vec<Vec<u8>> {
    payload.receive(&crate::sim::param_ext_frame(GCS, CAMERA, 0, out))
}

fn set(payload: &mut Payload, name: &str, value: Value) -> Vec<Vec<u8>> {
    ask(
        payload,
        &Out::Set {
            name: name.into(),
            value,
        },
    )
}

/// The values among some frames: name, value, index and count.
fn values(frames: &[Vec<u8>]) -> Vec<(String, Value, u16, u16)> {
    of!(frames, ParamExtValue)
        .into_iter()
        .map(|m| {
            (
                crate::decode::param_name(&m.param_id),
                Value::read(m.param_type as u8, &m.param_value).expect("a value"),
                m.param_index,
                m.param_count,
            )
        })
        .collect()
}

/// The one acknowledgement of a write among some frames.
fn acked(frames: &[Vec<u8>]) -> (String, ParamAck, Value) {
    let acks = of!(frames, ParamExtAck);
    assert_eq!(acks.len(), 1, "one acknowledgement");
    let ack = &acks[0];
    (
        crate::decode::param_name(&ack.param_id),
        ack.param_result,
        Value::read(ack.param_type as u8, &ack.param_value).expect("a value"),
    )
}

fn value_of(payload: &mut Payload, name: &str) -> Value {
    let said = values(&ask(payload, &Out::Read(name.into())));
    assert_eq!(said.len(), 1, "{name}");
    said[0].1.clone()
}

fn information(payload: &mut Payload) -> (String, u16) {
    let frames = payload.receive(&long(GCS, CAMERA, 512, &[259.0], 0));
    let said = of!(frames, CameraInformation);
    let uri: Vec<u8> = said[0]
        .cam_definition_uri
        .iter()
        .copied()
        .take_while(|byte| *byte != 0)
        .collect();
    (
        String::from_utf8(uri).expect("an address"),
        said[0].cam_definition_version,
    )
}

/// Reads a file from the camera as UXDuck's FTP client does.
fn read_file(payload: &mut Payload, path: &str) -> Result<Vec<u8>, String> {
    let mut client = FtpClient::new();
    let mut output = client.file(
        "read",
        Op::Read {
            path: path.into(),
            from: 0,
        },
        0,
    );
    let mut bytes = Vec::new();
    for round in 0..10_000u64 {
        for event in std::mem::take(&mut output.events) {
            match event {
                Event::Data { bytes: more, .. } => bytes.extend(more),
                Event::Done { result, .. } => {
                    return result.map(|()| bytes).map_err(|why| why.to_string());
                }
                _ => {}
            }
        }
        let mut answers = Vec::new();
        for request in std::mem::take(&mut output.send) {
            let frame =
                crate::sim::ftp_frame(GCS, CAMERA, 0, crate::transfers::ftp::payload(&request));
            answers.extend(payload.receive(&frame));
        }
        assert!(!answers.is_empty(), "the camera answers");
        for answer in of!(answers, FileTransferProtocol) {
            assert_eq!((answer.target_system, answer.target_component), GCS);
            let answer = crate::transfers::ftp::read(&answer.payload);
            let more = client.answer(&answer, round, round);
            output.send.extend(more.send);
            output.events.extend(more.events);
        }
    }
    panic!("the read never ended");
}

// Camera definition file: "located at the URI specified in the message's
// cam_definition_uri field". MAVLink FTP: "mftp:///camera.xml" is a file on
// the component that names it.
#[test]
fn the_camera_names_its_definition_file_and_serves_it_over_ftp() {
    let mut payload = camera();
    assert_eq!(
        information(&mut payload),
        (DEFINITION_URI.to_owned(), DEFINITION_VERSION)
    );
    assert_eq!(
        read_file(&mut payload, "/camera.xml"),
        Ok(DEFINITION.as_bytes().to_vec())
    );
    // A relative path names the same file, and no other file is there.
    assert_eq!(
        read_file(&mut payload, "camera.xml"),
        Ok(DEFINITION.as_bytes().to_vec())
    );
    assert!(read_file(&mut payload, "/other.xml").is_err());
    // Every setting the file names is one the camera has.
    for name in NAMES {
        assert!(
            DEFINITION.contains(&format!("<parameter name=\"{name}\"")),
            "{name}"
        );
    }
    assert_eq!(DEFINITION.matches("<parameter name=").count(), NAMES.len());
}

// S404: a camera that only sends video has nothing to set. A camera told
// to name no file has no settings either.
#[test]
fn a_camera_that_names_no_definition_has_no_settings() {
    let mut video_only = camera();
    video_only.camera.as_mut().expect("a camera").video_only = true;
    let mut unnamed = camera();
    unnamed.camera.as_mut().expect("a camera").definition_uri = None;
    for payload in [&mut video_only, &mut unnamed] {
        assert_eq!(information(payload), (String::new(), 0));
        assert!(ask(payload, &Out::List).is_empty());
        assert!(ask(payload, &Out::Read("CAM_ISO".into())).is_empty());
        assert!(set(payload, "CAM_PHOTOFMT", Value::Uint32(1)).is_empty());
    }
}

// Extended parameter protocol: "Read All Parameters", "Read Single
// Parameter" by name or by index, and "Write Parameters".
#[test]
fn its_settings_are_listed_read_and_written_as_extended_parameters() {
    let mut payload = camera();
    let listed = values(&ask(&mut payload, &Out::List));
    assert_eq!(listed.len(), NAMES.len());
    for (index, (name, _, at, count)) in listed.iter().enumerate() {
        assert_eq!(name, NAMES[index]);
        assert_eq!(
            (usize::from(*at), usize::from(*count)),
            (index, NAMES.len())
        );
    }
    assert_eq!(listed[3].1, Value::Uint32(400));
    assert_eq!(listed[4].1, Value::Real32(0.002));
    assert_eq!(value_of(&mut payload, "CAM_PHOTOFMT"), Value::Uint32(0));
    let by_index = values(&ask(&mut payload, &Out::ReadIndex(7)));
    assert_eq!(by_index[0].0, "CAM_PHOTOFMT");
    // One it does not have is not answered.
    assert!(ask(&mut payload, &Out::Read("CAM_NOPE".into())).is_empty());
    assert!(ask(&mut payload, &Out::ReadIndex(9)).is_empty());

    // A write is answered with the value the camera now has.
    let frames = set(&mut payload, "CAM_PHOTOFMT", Value::Uint32(2));
    assert_eq!(sender(&frames[0]), CAMERA);
    assert_eq!(
        acked(&frames),
        ("CAM_PHOTOFMT".into(), ParamAck::Accepted, Value::Uint32(2))
    );
    assert_eq!(value_of(&mut payload, "CAM_PHOTOFMT"), Value::Uint32(2));
    // "If the parameter was unknown or of an unsupported type":
    // PARAM_ACK_VALUE_UNSUPPORTED. So is a value it does not list.
    for (name, value) in [
        ("CAM_NOPE", Value::Uint32(1)),
        ("CAM_PHOTOFMT", Value::Real32(1.0)),
        ("CAM_PHOTOFMT", Value::Uint32(9)),
    ] {
        assert_eq!(
            acked(&set(&mut payload, name, value)).1,
            ParamAck::ValueUnsupported,
            "{name}"
        );
    }
    assert_eq!(value_of(&mut payload, "CAM_PHOTOFMT"), Value::Uint32(2));
    // It counts its photos itself: "failed for another reason", with the
    // value it has.
    assert_eq!(
        acked(&set(&mut payload, "CAM_SHUTTERCNT", Value::Uint32(7))),
        ("CAM_SHUTTERCNT".into(), ParamAck::Failed, Value::Uint32(0))
    );
    payload.receive(&long(GCS, CAMERA, 2000, &[0.0, 0.0, 1.0, 1.0], 0));
    assert_eq!(value_of(&mut payload, "CAM_SHUTTERCNT"), Value::Uint32(1));
}

// Camera definition file, "Exclusion Rules" and "Required Option Updates":
// what a mode rules out the camera refuses, and automatic exposure sets
// the ISO and the shutter speed itself.
// The seam for a camera whose list is longer than a ground station keeps:
// settings beyond its own, each a number that stays 0.
#[test]
fn a_camera_can_be_made_to_list_more_settings_than_its_file_names() {
    let mut payload = camera();
    payload.camera.as_mut().unwrap().more_settings = 1030;
    // A long list comes a few at a time, as a camera paces one.
    let mut frames = ask(&mut payload, &Out::List);
    assert_eq!(values(&frames).len(), 32);
    frames.extend(run(&mut payload, 0, 1000));
    let listed = values(&frames);
    assert_eq!(listed.len(), NAMES.len() + 1030);
    assert!(listed.iter().all(|(_, _, _, count)| *count == 1039));
    assert!(values(&run(&mut payload, 1000, 2000)).is_empty());
    assert_eq!(listed[8].0, "CAM_SHUTTERCNT");
    assert_eq!(listed[9], ("X9".to_owned(), Value::Uint8(0), 9, 1039));
    assert_eq!(listed[1038].0, "X1038");
    let by_index = values(&ask(&mut payload, &Out::ReadIndex(1038)));
    assert_eq!(by_index[0].0, "X1038");
    assert!(ask(&mut payload, &Out::ReadIndex(1039)).is_empty());
}

#[test]
fn a_setting_its_exposure_mode_rules_out_is_refused() {
    let mut payload = camera();
    assert_eq!(
        acked(&set(&mut payload, "CAM_ISO", Value::Uint32(800))),
        ("CAM_ISO".into(), ParamAck::Failed, Value::Uint32(400))
    );
    assert_eq!(
        acked(&set(&mut payload, "CAM_EV", Value::Real32(-1.5))).1,
        ParamAck::Accepted
    );
    // Not a step of a half, and outside its range.
    for ev in [0.25, 3.5] {
        assert_eq!(
            acked(&set(&mut payload, "CAM_EV", Value::Real32(ev))).1,
            ParamAck::ValueUnsupported
        );
    }
    assert_eq!(
        acked(&set(&mut payload, "CAM_EXPMODE", Value::Uint32(1))).1,
        ParamAck::Accepted
    );
    assert_eq!(
        acked(&set(&mut payload, "CAM_EV", Value::Real32(1.0))).1,
        ParamAck::Failed
    );
    assert_eq!(
        acked(&set(&mut payload, "CAM_ISO", Value::Uint32(800))).1,
        ParamAck::Accepted
    );
    assert_eq!(
        acked(&set(&mut payload, "CAM_SHUTTERSPD", Value::Real32(0.0005))).1,
        ParamAck::Accepted
    );
    assert_eq!(
        acked(&set(&mut payload, "CAM_ISO", Value::Uint32(123))).1,
        ParamAck::ValueUnsupported
    );
    // Back to automatic: the camera picks them again.
    set(&mut payload, "CAM_EXPMODE", Value::Uint32(0));
    assert_eq!(value_of(&mut payload, "CAM_ISO"), Value::Uint32(400));
    assert_eq!(
        value_of(&mut payload, "CAM_SHUTTERSPD"),
        Value::Real32(0.002)
    );
}

// "The parameter CAM_MODE must be part of the parameter list. It maps to
// the command MAV_CMD_SET_CAMERA_MODE."
#[test]
fn its_mode_is_one_setting_whichever_way_it_is_set() {
    let mut payload = camera();
    // A video's settings are not for a camera taking photos.
    assert_eq!(
        acked(&set(&mut payload, "CAM_VIDFPS", Value::Uint32(60))).1,
        ParamAck::Failed
    );
    let frames = set(&mut payload, "CAM_MODE", Value::Uint32(1));
    assert_eq!(acked(&frames).1, ParamAck::Accepted);
    assert_eq!(of!(frames, CameraSettings)[0].mode_id, CameraMode::Video);
    assert_eq!(
        acked(&set(&mut payload, "CAM_PHOTOFMT", Value::Uint32(1))).1,
        ParamAck::Failed
    );
    // The command changes the setting too.
    payload.receive(&long(GCS, CAMERA, 530, &[0.0, 0.0], 0));
    assert_eq!(value_of(&mut payload, "CAM_MODE"), Value::Uint32(0));
    assert_eq!(
        acked(&set(&mut payload, "CAM_MODE", Value::Uint32(2))).1,
        ParamAck::ValueUnsupported
    );
}

// Extended parameter protocol: "For long-running write operations drone
// may initially respond with PARAM_ACK_IN_PROGRESS", and "PARAM_EXT_ACK
// should be resent when the operation completes". Camera definition file,
// "Option Range Limit": the largest video is not recorded 60 times a
// second.
#[test]
fn a_videos_size_takes_a_moment_and_cannot_change_while_it_records() {
    let mut payload = camera();
    run(&mut payload, 0, 1000);
    set(&mut payload, "CAM_MODE", Value::Uint32(1));
    assert_eq!(
        acked(&set(&mut payload, "CAM_VIDFPS", Value::Uint32(60))).1,
        ParamAck::Accepted
    );
    let working = (
        "CAM_VIDRES".to_owned(),
        ParamAck::InProgress,
        Value::Uint32(1),
    );
    assert_eq!(
        acked(&set(&mut payload, "CAM_VIDRES", Value::Uint32(0))),
        working
    );
    // The same write again, as when its answer was lost, is one change.
    assert_eq!(
        acked(&set(&mut payload, "CAM_VIDRES", Value::Uint32(0))),
        working
    );
    assert_eq!(
        acked(&set(&mut payload, "CAM_VIDRES", Value::Uint32(2))).1,
        ParamAck::Failed
    );
    assert!(of!(run(&mut payload, 1000, 1900), ParamExtAck).is_empty());
    let done = run(&mut payload, 1900, 2100);
    assert_eq!(
        acked(&done),
        ("CAM_VIDRES".into(), ParamAck::Accepted, Value::Uint32(0))
    );
    // It dropped the rate the largest video cannot have, and refuses it.
    assert_eq!(value_of(&mut payload, "CAM_VIDFPS"), Value::Uint32(30));
    assert_eq!(
        acked(&set(&mut payload, "CAM_VIDFPS", Value::Uint32(60))).1,
        ParamAck::ValueUnsupported
    );
    // Not while it records.
    payload.receive(&long(GCS, CAMERA, 2500, &[0.0, 0.0], 0));
    for (name, value) in [("CAM_VIDRES", 2), ("CAM_VIDFPS", 24)] {
        assert_eq!(
            acked(&set(&mut payload, name, Value::Uint32(value))).1,
            ParamAck::Failed,
            "{name}"
        );
    }
    payload.receive(&long(GCS, CAMERA, 2501, &[0.0], 0));
    assert_eq!(
        acked(&set(&mut payload, "CAM_VIDFPS", Value::Uint32(24))).1,
        ParamAck::Accepted
    );
}

/// The room the camera says is left on its card, in MiB.
fn room(payload: &mut Payload) -> f32 {
    let frames = payload.receive(&long(GCS, CAMERA, 512, &[261.0, 1.0], 0));
    of!(frames, StorageInformation)[0].available_capacity
}

#[test]
fn a_photos_format_and_a_videos_size_change_how_fast_the_card_fills() {
    let mut payload = camera();
    run(&mut payload, 0, 1000);
    let photo = |payload: &mut Payload, number: f32| {
        let before = room(payload);
        payload.receive(&long(GCS, CAMERA, 2000, &[0.0, 0.0, 1.0, number], 0));
        before - room(payload)
    };
    assert!(near(photo(&mut payload, 1.0), 8.0));
    set(&mut payload, "CAM_PHOTOFMT", Value::Uint32(1));
    assert!(near(photo(&mut payload, 2.0), 24.0));
    set(&mut payload, "CAM_PHOTOFMT", Value::Uint32(2));
    assert!(near(photo(&mut payload, 3.0), 32.0));

    let second = |payload: &mut Payload, from_ms: u64| {
        payload.receive(&long(GCS, CAMERA, 2500, &[0.0, 0.0], 0));
        let before = room(payload);
        run(payload, from_ms, from_ms + 1000);
        let used = before - room(payload);
        payload.receive(&long(GCS, CAMERA, 2501, &[0.0], 0));
        used
    };
    assert!(near(second(&mut payload, 1000), 1.5));
    set(&mut payload, "CAM_MODE", Value::Uint32(1));
    set(&mut payload, "CAM_VIDFPS", Value::Uint32(60));
    assert!(near(second(&mut payload, 2000), 3.0));
    set(&mut payload, "CAM_VIDRES", Value::Uint32(2));
    run(&mut payload, 3000, 4100);
    assert_eq!(value_of(&mut payload, "CAM_VIDRES"), Value::Uint32(2));
    assert!(near(second(&mut payload, 4100), 1.5));

    // MAV_CMD_RESET_CAMERA_SETTINGS puts them back as it was made.
    payload.receive(&long(GCS, CAMERA, 529, &[1.0], 0));
    for (name, value) in [("CAM_PHOTOFMT", 0), ("CAM_VIDRES", 1), ("CAM_VIDFPS", 30)] {
        assert_eq!(value_of(&mut payload, name), Value::Uint32(value), "{name}");
    }
}

// S400: what names the camera is kept from the autopilot, which would
// answer it itself. Each component has files of its own, so a file request
// is the camera's only when it names the camera.
#[test]
fn what_names_the_cameras_files_or_settings_is_kept_from_the_autopilot() {
    let mut link = OnLink::new(VEHICLE, true, true).expect("a payload");
    let open = crate::transfers::FtpRequest {
        seq: 1,
        session: 0,
        opcode: crate::transfers::ftp::OPEN_FILE_RO,
        size: 11,
        offset: 0,
        data: b"/camera.xml".to_vec(),
    };
    let file = |to| crate::sim::ftp_frame(GCS, to, 0, crate::transfers::ftp::payload(&open));
    let heard = link.from_ground(&file(CAMERA));
    assert!(heard.kept);
    assert_eq!(of!(heard.said, FileTransferProtocol).len(), 1);
    assert_eq!(sender(&heard.said[0]), CAMERA);
    // The autopilot's files, every component's and the gimbal's, which has
    // none.
    for to in [AUTOPILOT, (VEHICLE, 0), GIMBAL] {
        let heard = link.from_ground(&file(to));
        assert_eq!(heard.kept, to == GIMBAL, "{to:?}");
        assert!(heard.said.is_empty(), "{to:?}");
    }
    let list = |to| crate::sim::param_ext_frame(GCS, to, 0, &Out::List);
    let heard = link.from_ground(&list(CAMERA));
    assert!(heard.kept);
    assert_eq!(values(&heard.said).len(), NAMES.len());
    // "Broadcast addresses may be used": the camera answers, and the frame
    // goes on to the autopilot too.
    let heard = link.from_ground(&list((VEHICLE, 0)));
    assert!(!heard.kept);
    assert_eq!(values(&heard.said).len(), NAMES.len());
    let heard = link.from_ground(&list(AUTOPILOT));
    assert_eq!((heard.kept, heard.said.len()), (false, 0));
    let heard = link.from_ground(&list((2, 100)));
    assert_eq!((heard.kept, heard.said.len()), (false, 0));
}

// ADR-0063 Y16: a camera that takes photos in one mode and records in the
// other keeps its mode while it records, whichever way the mode is asked
// for.
#[test]
fn a_camera_bound_to_its_mode_keeps_it_while_it_records_by_its_setting_too() {
    let mut payload = camera();
    payload.camera.as_mut().expect("a camera").mode_bound = true;
    assert_eq!(
        acked(&set(&mut payload, "CAM_MODE", Value::Uint32(1))).1,
        ParamAck::Accepted
    );
    // It records in video mode, and its mode then stays.
    payload.receive(&long(GCS, CAMERA, 2500, &[0.0, 0.0], 0));
    assert_eq!(
        acked(&set(&mut payload, "CAM_MODE", Value::Uint32(0))).1,
        ParamAck::Failed
    );
    assert_eq!(value_of(&mut payload, "CAM_MODE"), Value::Uint32(1));
    // Asked for the mode it is in, it has nothing to refuse.
    assert_eq!(
        acked(&set(&mut payload, "CAM_MODE", Value::Uint32(1))).1,
        ParamAck::Accepted
    );
    payload.receive(&long(GCS, CAMERA, 2501, &[0.0], 0));
    assert_eq!(
        acked(&set(&mut payload, "CAM_MODE", Value::Uint32(0))).1,
        ParamAck::Accepted
    );
}
