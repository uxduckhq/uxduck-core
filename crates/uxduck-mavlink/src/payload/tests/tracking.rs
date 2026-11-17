//! The thermal source and tracking of the unit that has them (ADR-0063
//! Y5), by what the camera protocol's definitions say of each command and
//! message (mavlink.io: camera protocol version 2, and common.xml).

use mavio::dialects::ardupilotmega::enums::{
    CameraCapFlags, CameraTrackingMode, CameraTrackingStatusFlags, CameraTrackingTargetData,
};
use mavio::dialects::ardupilotmega::messages::{Attitude, GlobalPositionInt};

use super::*;
use crate::payload::{OnLink, Profile};

const STREAM: &str = "udp://127.0.0.1:15600";
const SET_CAMERA_SOURCE: u16 = 534;
const TRACK_POINT: u16 = 2004;
const TRACK_RECTANGLE: u16 = 2005;
const STOP_TRACKING: u16 = 2010;
/// The stream's flags: running, thermal, and a thermal range to ask for.
const RUNNING: u8 = 1;
const THERMAL: u8 = 2;
const THERMAL_RANGE_ENABLED: u8 = 4;

fn fitted(profile: Profile) -> OnLink {
    OnLink::new(VEHICLE, true, true)
        .expect("a payload")
        .fitted(profile)
        .streaming(STREAM)
}

fn ask(link: &mut OnLink, command: u16, values: &[f32]) -> Vec<Vec<u8>> {
    link.from_ground(&long(GCS, CAMERA, command, values, 0))
        .said
}

/// The command's result, whatever comes with it.
fn answered(link: &mut OnLink, command: u16, values: &[f32]) -> u8 {
    result(&ask(link, command, values), CAMERA, command)
}

/// The autopilot says where the vehicle is: metres north of home's place
/// and above it.
fn placed(link: &mut OnLink, north_m: f64, up_m: i32) {
    let home = over_home();
    #[allow(clippy::cast_possible_truncation, reason = "degrees times 1e7")]
    let position = GlobalPositionInt {
        lat: home.lat + (north_m / 111_320.0 * 1e7).round() as i32,
        lon: home.lon,
        alt: home.alt_mm - home.relative_alt_mm + up_m * 1000,
        relative_alt: up_m * 1000,
        ..GlobalPositionInt::default()
    };
    link.from_autopilot(&crate::sim::frame(AUTOPILOT.0, AUTOPILOT.1, 0, &position));
}

/// The autopilot's clock, which is the payload's, with the vehicle's
/// heading.
fn at(link: &mut OnLink, time_boot_ms: u32, yaw: f32) -> Vec<Vec<u8>> {
    let message = Attitude {
        time_boot_ms,
        yaw,
        ..Attitude::default()
    };
    link.from_autopilot(&crate::sim::frame(AUTOPILOT.0, AUTOPILOT.1, 0, &message))
}

/// Runs the clock from just after `from_ms` to `to_ms`, the nose north.
fn run_to(link: &mut OnLink, from_ms: u32, to_ms: u32) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    for ms in ((from_ms + 20)..=to_ms).step_by(20) {
        frames.extend(at(link, ms, 0.0));
    }
    frames
}

/// A unit 100 m over home that knows where it is, looking level and north.
fn flying() -> OnLink {
    let mut link = fitted(Profile::ThermalTracking);
    placed(&mut link, 0.0, 100);
    run_to(&mut link, 0, 100);
    link
}

fn image_status(
    link: &mut OnLink,
) -> mavio::dialects::ardupilotmega::messages::CameraTrackingImageStatus {
    let frames = ask(link, 512, &[275.0]);
    assert_eq!(result(&frames, CAMERA, 512), ACCEPTED);
    of!(frames, CameraTrackingImageStatus).remove(0)
}

fn geo_status(
    link: &mut OnLink,
) -> mavio::dialects::ardupilotmega::messages::CameraTrackingGeoStatus {
    let frames = ask(link, 512, &[276.0]);
    assert_eq!(result(&frames, CAMERA, 512), ACCEPTED);
    of!(frames, CameraTrackingGeoStatus).remove(0)
}

/// The ground station takes the unit's gimbal and points it.
fn pointed(link: &mut OnLink, pitch_deg: f32, yaw_deg: f32) {
    assert_eq!(answered(link, 1001, &[-2.0, -2.0, -1.0, -1.0]), ACCEPTED);
    assert_eq!(
        answered(link, 1000, &[pitch_deg, yaw_deg, f32::NAN, f32::NAN]),
        ACCEPTED
    );
}

// common.xml: CAMERA_CAP_FLAGS_HAS_TRACKING_POINT, _RECTANGLE and
// _GEO_STATUS say what a camera tracks, and _HAS_THERMAL_RANGE that it
// "supports absolute thermal range".
#[test]
fn the_unit_says_it_tracks_and_has_a_thermal_range_and_the_other_kinds_do_not() {
    let wanted = CameraCapFlags::HAS_TRACKING_POINT
        | CameraCapFlags::HAS_TRACKING_RECTANGLE
        | CameraCapFlags::HAS_TRACKING_GEO_STATUS
        | CameraCapFlags::HAS_THERMAL_RANGE;
    let flags = |link: &mut OnLink| {
        of!(ask(link, 512, &[259.0]), CameraInformation)
            .remove(0)
            .flags
    };
    let mut unit = fitted(Profile::ThermalTracking);
    assert!(flags(&mut unit).contains(wanted));
    // One component that is camera and gimbal manager, as the integrated
    // kind is.
    assert_eq!(unit.announce().len(), 1);
    assert_eq!(
        result(&ask(&mut unit, 512, &[280.0]), CAMERA, 512),
        ACCEPTED
    );

    for profile in [Profile::Standard, Profile::PhotoVideo, Profile::Integrated] {
        let mut link = fitted(profile);
        assert!(!flags(&mut link).intersects(wanted), "{profile:?}");
        for command in [
            SET_CAMERA_SOURCE,
            TRACK_POINT,
            TRACK_RECTANGLE,
            STOP_TRACKING,
        ] {
            assert_eq!(
                answered(&mut link, command, &[0.5, 0.5, 0.0, 0.0]),
                UNSUPPORTED,
                "{profile:?} {command}"
            );
        }
        // It has the command that asks for messages, and not these.
        for message in [275.0, 276.0, 277.0] {
            assert_eq!(answered(&mut link, 512, &[message]), DENIED);
            assert_eq!(answered(&mut link, 511, &[message, 100_000.0]), DENIED);
        }
    }
    // Without a gimbal there is no unit, and the camera alone tracks
    // nothing.
    let mut alone = OnLink::new(VEHICLE, true, false)
        .expect("a payload")
        .fitted(Profile::ThermalTracking)
        .streaming(STREAM);
    assert!(!flags(&mut alone).intersects(wanted));
}

// common.xml, MAV_CMD_SET_CAMERA_SOURCE: param2 is the primary
// CAMERA_SOURCE and param3 a secondary one, shown as picture in picture.
// VIDEO_STREAM_STATUS_FLAGS_THERMAL says a stream is thermal, and
// _THERMAL_RANGE_ENABLED that CAMERA_THERMAL_RANGE can be asked for.
#[test]
fn its_one_stream_shows_the_daylight_sensor_or_the_thermal_one() {
    let mut link = flying();
    let flags_of = |frames: &[Vec<u8>]| of!(frames, VideoStreamStatus).remove(0).flags.bits();
    assert_eq!(flags_of(&ask(&mut link, 512, &[270.0])), RUNNING);
    assert!(!link.view().expect("a view").thermal);
    // The range of a picture that is not thermal is not to be had.
    assert_eq!(answered(&mut link, 512, &[277.0]), DENIED);

    // To thermal: accepted, and its stream's status says so at once.
    let frames = ask(&mut link, SET_CAMERA_SOURCE, &[0.0, 2.0, 0.0]);
    assert_eq!(result(&frames, CAMERA, SET_CAMERA_SOURCE), ACCEPTED);
    assert_eq!(flags_of(&frames), RUNNING | THERMAL | THERMAL_RANGE_ENABLED);
    assert!(link.view().expect("a view").thermal);
    let named = of!(ask(&mut link, 512, &[269.0]), VideoStreamInformation).remove(0);
    assert_eq!(
        named.flags.bits(),
        RUNNING | THERMAL | THERMAL_RANGE_ENABLED
    );
    assert_eq!((named.stream_id, named.count), (1, 1), "still one stream");

    let frames = ask(&mut link, 512, &[277.0, 1.0]);
    assert_eq!(result(&frames, CAMERA, 512), ACCEPTED);
    let range = of!(frames, CameraThermalRange).remove(0);
    assert_eq!(range.stream_id, 1);
    assert_eq!(
        (range.min, range.max),
        crate::payload::camera::THERMAL_SCALE_C
    );
    assert!(range.max_point_x.is_nan() && range.min_point_y.is_nan());
    assert_eq!(answered(&mut link, 512, &[277.0, 2.0]), DENIED, "stream 2");

    // Named by its component id, and back to daylight: by RGB or by the
    // camera's own choice.
    assert_eq!(
        answered(&mut link, SET_CAMERA_SOURCE, &[100.0, 1.0]),
        ACCEPTED
    );
    assert_eq!(flags_of(&ask(&mut link, 512, &[270.0])), RUNNING);
    assert_eq!(
        answered(&mut link, SET_CAMERA_SOURCE, &[0.0, 2.0]),
        ACCEPTED
    );
    assert_eq!(
        answered(&mut link, SET_CAMERA_SOURCE, &[0.0, 0.0]),
        ACCEPTED
    );
    assert!(!link.view().expect("a view").thermal);

    // A source it has not, a picture in picture, and another camera.
    assert_eq!(answered(&mut link, SET_CAMERA_SOURCE, &[0.0, 3.0]), DENIED);
    assert_eq!(
        answered(&mut link, SET_CAMERA_SOURCE, &[0.0, 1.0, 2.0]),
        DENIED
    );
    assert_eq!(answered(&mut link, SET_CAMERA_SOURCE, &[5.0, 2.0]), DENIED);
    assert!(
        !link.view().expect("a view").thermal,
        "refused, so as it was"
    );
}

// common.xml, CAMERA_THERMAL_RANGE: asked for with
// MAV_CMD_SET_MESSAGE_INTERVAL "when the associated VIDEO_STREAM_STATUS
// flag ... THERMAL_RANGE_ENABLED is set".
#[test]
fn the_thermal_range_comes_at_its_interval_while_the_picture_is_thermal() {
    let mut link = flying();
    assert_eq!(answered(&mut link, 511, &[277.0, 1_000_000.0]), ACCEPTED);
    let daylight = run_to(&mut link, 100, 3100);
    assert!(of!(daylight, CameraThermalRange).is_empty());
    assert_eq!(
        answered(&mut link, SET_CAMERA_SOURCE, &[0.0, 2.0]),
        ACCEPTED
    );
    let thermal = run_to(&mut link, 3100, 6100);
    assert_eq!(of!(thermal, CameraThermalRange).len(), 3);
    assert_eq!(
        answered(&mut link, SET_CAMERA_SOURCE, &[0.0, 1.0]),
        ACCEPTED
    );
    let after = run_to(&mut link, 6100, 9100);
    assert!(of!(after, CameraThermalRange).is_empty());
}

// common.xml, MAV_CMD_CAMERA_TRACK_POINT: x and y from 0 at the left and
// top to 1, and CAMERA_TRACKING_IMAGE_STATUS and _GEO_STATUS say where the
// target is in the picture and on the ground.
#[test]
fn it_tracks_the_ground_under_a_point_and_keeps_its_gimbal_on_it() {
    let mut link = flying();
    let idle = image_status(&mut link);
    assert_eq!(idle.tracking_status, CameraTrackingStatusFlags::Idle);
    assert_eq!(idle.tracking_mode, CameraTrackingMode::None);
    assert!(idle.point_x.is_nan() && idle.rec_top_x.is_nan());

    // Level, 100.5 m up and 44.8 degrees across: the bottom of the picture
    // is 13 degrees below the horizon, and nine tenths of the way down it
    // is 10.5 degrees below, where the ground is 542 m ahead.
    assert_eq!(
        answered(&mut link, TRACK_POINT, &[0.5, 0.9, 0.0, 0.0]),
        ACCEPTED
    );
    let taken = image_status(&mut link);
    assert_eq!(taken.tracking_status, CameraTrackingStatusFlags::Active);
    assert_eq!(taken.tracking_mode, CameraTrackingMode::Point);
    assert_eq!(taken.target_data, CameraTrackingTargetData::IN_STATUS);
    assert!(near(taken.point_x, 0.5) && near(taken.point_y, 0.9));
    assert!(taken.rec_top_x.is_nan() && taken.rec_bottom_y.is_nan());

    let home = over_home();
    let geo = geo_status(&mut link);
    assert_eq!(geo.tracking_status, CameraTrackingStatusFlags::Active);
    let north_m = f64::from(geo.lat - home.lat) / 1e7 * 111_320.0;
    assert!((north_m - 542.0).abs() < 5.0, "{north_m}");
    assert_eq!(geo.lon, home.lon);
    assert!(near(geo.alt, 584.0), "home's height: {}", geo.alt);
    assert!((geo.dist - 551.0).abs() < 5.0, "{}", geo.dist);
    assert!(geo.h_acc.is_nan() && geo.hdg.is_nan());

    // The unit turns its own gimbal to it, whoever is in control, and the
    // target comes to the middle of the picture.
    assert!(link.payload().gimbal.as_ref().expect("a gimbal").follows());
    run_to(&mut link, 100, 2100);
    let held = image_status(&mut link);
    assert!(
        near(held.point_x, 0.5) && near(held.point_y, 0.5),
        "{held:?}"
    );
    let attitude = link
        .payload()
        .gimbal
        .as_ref()
        .expect("a gimbal")
        .attitude_from_north();
    assert!(near(attitude[1], -10.5f32.to_radians()), "{attitude:?}");

    // The vehicle flies 300 m on and turns east: the place on the ground
    // is the same, and the gimbal is still on it.
    placed(&mut link, 300.0, 100);
    for ms in (2120..=5100).step_by(20) {
        at(&mut link, ms, std::f32::consts::FRAC_PI_2);
    }
    let moved = geo_status(&mut link);
    assert_eq!((moved.lat, moved.lon), (geo.lat, geo.lon));
    assert!((moved.dist - 262.0).abs() < 5.0, "{}", moved.dist);
    let held = image_status(&mut link);
    assert_eq!(held.tracking_status, CameraTrackingStatusFlags::Active);
    assert!(
        near(held.point_x, 0.5) && near(held.point_y, 0.5),
        "{held:?}"
    );

    // Stopped: idle, and the gimbal stays as it points.
    assert_eq!(answered(&mut link, STOP_TRACKING, &[0.0]), ACCEPTED);
    assert_eq!(
        image_status(&mut link).tracking_status,
        CameraTrackingStatusFlags::Idle
    );
    assert!(!link.payload().gimbal.as_ref().expect("a gimbal").follows());
    let before = link
        .payload()
        .gimbal
        .as_ref()
        .expect("a gimbal")
        .attitude_from_north();
    for ms in (5120..=6100).step_by(20) {
        at(&mut link, ms, std::f32::consts::FRAC_PI_2);
    }
    let after = link
        .payload()
        .gimbal
        .as_ref()
        .expect("a gimbal")
        .attitude_from_north();
    assert!(near(before[1], after[1]) && near(before[2], after[2]));
    // Stopping what is not tracked is no error.
    assert_eq!(answered(&mut link, STOP_TRACKING, &[100.0]), ACCEPTED);
}

// A movement asked of the unit's gimbal manager is obeyed, as the gimbal
// protocol asks of a manager. The camera goes on tracking in the picture
// it then has, and says so when the target is outside it.
#[test]
fn a_gimbal_pointed_elsewhere_moves_the_target_in_the_picture_and_then_loses_it() {
    let mut link = flying();
    assert_eq!(answered(&mut link, TRACK_POINT, &[0.5, 0.9]), ACCEPTED);
    run_to(&mut link, 100, 2100);

    // Five degrees further down: the target is above the middle now.
    pointed(&mut link, -15.5, 0.0);
    assert!(!link.payload().gimbal.as_ref().expect("a gimbal").follows());
    run_to(&mut link, 2100, 4100);
    let above = image_status(&mut link);
    assert_eq!(above.tracking_status, CameraTrackingStatusFlags::Active);
    assert!(near(above.point_x, 0.5));
    assert!((0.25..0.35).contains(&above.point_y), "{}", above.point_y);

    // Straight down: the target is far outside the picture.
    pointed(&mut link, -90.0, 0.0);
    run_to(&mut link, 4100, 6100);
    let lost = image_status(&mut link);
    assert_eq!(lost.tracking_status, CameraTrackingStatusFlags::Error);
    assert_eq!(lost.tracking_mode, CameraTrackingMode::Point);
    assert!(lost.point_x.is_nan() && lost.point_y.is_nan());
    let geo = geo_status(&mut link);
    assert_eq!(geo.tracking_status, CameraTrackingStatusFlags::Error);
    assert!(geo.dist.is_nan());

    // Back on it: found again.
    pointed(&mut link, -10.5, 0.0);
    run_to(&mut link, 6100, 8100);
    let found = image_status(&mut link);
    assert_eq!(found.tracking_status, CameraTrackingStatusFlags::Active);
    assert!(near(found.point_y, 0.5), "{}", found.point_y);

    // A new target is followed again.
    assert_eq!(answered(&mut link, TRACK_POINT, &[0.4, 0.6]), ACCEPTED);
    assert!(link.payload().gimbal.as_ref().expect("a gimbal").follows());
}

// common.xml, MAV_CMD_CAMERA_TRACK_RECTANGLE: the top left corner, then
// the bottom right.
#[test]
fn a_box_is_tracked_by_its_middle_and_is_larger_when_the_lens_is_longer() {
    let mut link = flying();
    assert_eq!(
        answered(&mut link, TRACK_RECTANGLE, &[0.4, 0.8, 0.6, 1.0, 0.0]),
        ACCEPTED
    );
    let taken = image_status(&mut link);
    assert_eq!(taken.tracking_mode, CameraTrackingMode::Rectangle);
    assert!(taken.point_x.is_nan() && taken.radius.is_nan());
    assert!(near(taken.rec_top_x, 0.4) && near(taken.rec_bottom_x, 0.6));
    assert!(near(taken.rec_top_y, 0.8) && near(taken.rec_bottom_y, 1.0));

    run_to(&mut link, 100, 2100);
    let held = image_status(&mut link);
    let middle = (
        f32::midpoint(held.rec_top_x, held.rec_bottom_x),
        f32::midpoint(held.rec_top_y, held.rec_bottom_y),
    );
    assert!(near(middle.0, 0.5) && near(middle.1, 0.5), "{held:?}");
    let wide = held.rec_bottom_x - held.rec_top_x;
    assert!(near(wide, 0.2), "{wide}");

    // Zoomed to the middle of its range, 48 mm for 16: three times as
    // large, and cut to the picture where it is larger than that.
    assert_eq!(answered(&mut link, 531, &[2.0, 50.0]), ACCEPTED);
    run_to(&mut link, 2100, 2200);
    let zoomed = image_status(&mut link);
    assert!(
        near(zoomed.rec_bottom_x - zoomed.rec_top_x, 0.6),
        "{zoomed:?}"
    );
    assert_eq!(answered(&mut link, 531, &[2.0, 100.0]), ACCEPTED);
    run_to(&mut link, 2200, 2300);
    let filled = image_status(&mut link);
    assert!(near(filled.rec_top_x, 0.0) && near(filled.rec_bottom_x, 1.0));
}

// Guide, camera protocol, "Camera tracking": the ground station sets the
// interval of the two status messages, and common.xml says each is "sent
// while in active tracking".
#[test]
fn the_status_is_sent_while_it_tracks_and_once_more_when_it_stops() {
    let mut link = flying();
    assert_eq!(answered(&mut link, 511, &[275.0, 100_000.0]), ACCEPTED);
    assert_eq!(answered(&mut link, 511, &[276.0, 1_000_000.0]), ACCEPTED);
    let idle = run_to(&mut link, 100, 2100);
    assert!(of!(idle, CameraTrackingImageStatus).is_empty());
    assert!(of!(idle, CameraTrackingGeoStatus).is_empty());

    assert_eq!(answered(&mut link, TRACK_POINT, &[0.5, 0.9]), ACCEPTED);
    let tracking = run_to(&mut link, 2100, 4100);
    let images = of!(tracking, CameraTrackingImageStatus);
    assert!((19..=21).contains(&images.len()), "{}", images.len());
    assert!(
        images
            .iter()
            .all(|status| status.tracking_status == CameraTrackingStatusFlags::Active)
    );
    assert_eq!(of!(tracking, CameraTrackingGeoStatus).len(), 2);

    let stopped = ask(&mut link, STOP_TRACKING, &[0.0]);
    assert_eq!(result(&stopped, CAMERA, STOP_TRACKING), ACCEPTED);
    let last = of!(stopped, CameraTrackingImageStatus);
    assert_eq!(last.len(), 1);
    assert_eq!(last[0].tracking_status, CameraTrackingStatusFlags::Idle);
    assert_eq!(of!(stopped, CameraTrackingGeoStatus).len(), 1);
    let after = run_to(&mut link, 4100, 6100);
    assert!(of!(after, CameraTrackingImageStatus).is_empty());

    // Stopped with a negative interval, as any message is.
    assert_eq!(answered(&mut link, TRACK_POINT, &[0.5, 0.9]), ACCEPTED);
    assert_eq!(answered(&mut link, 511, &[275.0, -1.0]), ACCEPTED);
    let silent = run_to(&mut link, 6100, 8100);
    assert!(of!(silent, CameraTrackingImageStatus).is_empty());
    assert_eq!(of!(silent, CameraTrackingGeoStatus).len(), 2);
}

#[test]
fn a_target_that_cannot_be_taken_is_refused_with_why() {
    // Before it knows where it is: not now.
    let mut link = fitted(Profile::ThermalTracking);
    run_to(&mut link, 0, 100);
    assert_eq!(
        answered(&mut link, TRACK_POINT, &[0.5, 0.9]),
        TEMPORARILY_REJECTED
    );

    let mut link = flying();
    // The sky has no ground under it: tried, and it did not work.
    assert_eq!(answered(&mut link, TRACK_POINT, &[0.5, 0.1]), FAILED);
    assert_eq!(
        answered(&mut link, TRACK_RECTANGLE, &[0.4, 0.0, 0.6, 0.2]),
        FAILED
    );
    // Values that are no place in a picture, or no box.
    for values in [
        [1.5, 0.5, 0.0, 0.0],
        [0.5, -0.1, 0.0, 0.0],
        [0.5, 0.9, 2.0, 0.0],
    ] {
        assert_eq!(
            answered(&mut link, TRACK_POINT, &values),
            DENIED,
            "{values:?}"
        );
    }
    assert_eq!(answered(&mut link, TRACK_POINT, &[f32::NAN, 0.5]), DENIED);
    for values in [
        [0.6, 0.8, 0.4, 1.0],
        [0.4, 0.9, 0.6, 0.9],
        [0.4, 0.8, 0.6, 1.2],
    ] {
        assert_eq!(
            answered(&mut link, TRACK_RECTANGLE, &values),
            DENIED,
            "{values:?}"
        );
    }
    // Another camera's.
    assert_eq!(
        answered(&mut link, TRACK_POINT, &[0.5, 0.9, 0.0, 3.0]),
        DENIED
    );
    assert_eq!(answered(&mut link, STOP_TRACKING, &[3.0]), DENIED);
    assert_eq!(
        image_status(&mut link).tracking_status,
        CameraTrackingStatusFlags::Idle,
        "nothing was taken"
    );

    // On the ground it sees from its lens, half a metre up, so the ground
    // ahead of it can be tracked there too.
    let mut landed = fitted(Profile::ThermalTracking);
    placed(&mut landed, 0.0, 0);
    run_to(&mut landed, 0, 100);
    assert_eq!(answered(&mut landed, TRACK_POINT, &[0.5, 0.9]), ACCEPTED);
    let geo = geo_status(&mut landed);
    assert!((geo.dist - 2.75).abs() < 0.2, "{}", geo.dist);
}

// A camera with no power says nothing of what it tracked, and tracks it
// still when it has power again.
#[test]
fn a_camera_that_goes_silent_while_it_tracks_says_nothing_until_it_is_back() {
    let mut link = flying();
    assert_eq!(answered(&mut link, 511, &[275.0, 100_000.0]), ACCEPTED);
    assert_eq!(answered(&mut link, TRACK_POINT, &[0.5, 0.9]), ACCEPTED);
    link.set_faults(crate::payload::Faults {
        camera_silent: true,
        ..Default::default()
    });
    let silent = run_to(&mut link, 100, 2100);
    assert!(silent.is_empty(), "{}", silent.len());
    assert!(ask(&mut link, 512, &[275.0]).is_empty());
    link.set_faults(crate::payload::Faults::default());
    let back = run_to(&mut link, 2100, 3100);
    assert!(!of!(back, CameraTrackingImageStatus).is_empty());
}
