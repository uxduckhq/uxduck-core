//! W7's rule against the estimates the cloud test recorded
//! (`experiments/webrtc-bitrate/results`, the columns this needs), and
//! closed against two models of the camera's encoder.

use std::time::{Duration, Instant};

use super::*;

/// The cloud test's camera: 2 Mbit/s at 50 pictures a second, one
/// keyframe a second.
const TEST_CAMERA: Settings = Settings {
    bitrate: 2_000_000,
    fps: 50.0,
    idr: 50,
};

/// The Plane's front camera on 2026-11-22: 1 Mbit/s at 30.
const PLANE: Settings = Settings {
    bitrate: 1_000_000,
    fps: 30.0,
    idr: 30,
};

struct Row {
    t: f64,
    estimate: u64,
    fps: f64,
}

fn rows(csv: &str) -> Vec<Row> {
    csv.lines()
        .skip(1)
        .map(|line| {
            let f: Vec<f64> = line.split(',').map(|v| v.parse().unwrap()).collect();
            Row {
                t: f[0],
                estimate: whole(f[1] * 1000.0),
                fps: f[3],
            }
        })
        .collect()
}

/// What the cloud test's scene needs at 50 pictures a second at the
/// encoder's coarsest: about 1 Mbit/s panning 2 px a picture (ADR-0064's
/// Context).
const SCENE_FLOOR: u64 = 1_000_000;

/// Each recorded second's estimate, then what the encoder made in it with
/// the settings the rule gave, from mediamtx-rpicamera 2.9.0's encoder
/// (`made`): the recorded rates answered the cloud test's own settings, so
/// they are compared, not fed back. The settings and target after each.
fn replay(csv: &str, floor: u64) -> Vec<(f64, u64, Settings)> {
    let start = Instant::now();
    let mut rule = Rule::new(TEST_CAMERA, floor);
    rows(csv)
        .iter()
        .map(|row| {
            rule.estimate(row.estimate, start + Duration::from_secs_f64(row.t));
            rule.second(made(rule.settings(), TEST_CAMERA, SCENE_FLOOR, false));
            (row.t, rule.target(), rule.settings())
        })
        .collect()
}

#[test]
fn the_recorded_estimates_keep_the_target_under_them_between_floor_and_ceiling() {
    for (csv, floor) in [
        (include_str!("adaptive-150k.csv"), 150_000),
        (include_str!("adaptive-low.csv"), 150_000),
        (include_str!("adaptive-250k.csv"), 250_000),
    ] {
        let recorded = rows(csv);
        for ((t, target, settings), row) in replay(csv, floor).into_iter().zip(&recorded) {
            assert!(
                (floor..=TEST_CAMERA.bitrate).contains(&target),
                "{t}: {target}"
            );
            assert!(settings.bitrate <= TEST_CAMERA.bitrate, "{t}: {settings:?}");
            assert!(settings.bitrate >= floor, "{t}: {settings:?}");
            // A fall is taken at once, so the target is never more than 5%
            // over its share of the estimate, unless at the floor.
            let share = float(row.estimate) * SHARE_OF_ESTIMATE;
            assert!(
                target == floor || float(target) <= share / (1.0 - SMALLEST_CHANGE) + 1.0,
                "{t}: target {target} for an estimate of {}",
                row.estimate
            );
            // One keyframe a second at every rate.
            assert_eq!(whole(settings.fps), settings.idr, "{t}: {settings:?}");
        }
    }
}

#[test]
fn the_frame_rate_steps_down_on_the_300_kbit_link_and_back_up_at_4_mbit() {
    let csv = include_str!("adaptive-150k.csv");
    let replayed = replay(csv, 150_000);
    let recorded = rows(csv);
    let lowest = |from: f64, to: f64, fps: &dyn Fn(usize) -> f64| {
        (0..replayed.len())
            .filter(|i| (from..to).contains(&replayed[*i].0))
            .map(fps)
            .fold(f64::INFINITY, f64::min)
    };
    // The link carried 300 kbit/s from 60 s to 90 s.
    let ours = lowest(65.0, 90.0, &|i| replayed[i].2.fps);
    let theirs = lowest(65.0, 90.0, &|i| recorded[i].fps);
    println!("lowest frame rate on the 300 kbit/s link: {ours}, the cloud test's {theirs}");
    assert!(ours <= 10.0, "{ours}");
    // Back at 4 Mbit/s from 120 s, the camera's own rate by the end.
    let last = replayed.last().unwrap();
    assert!(
        (last.2.fps - TEST_CAMERA.fps).abs() < f64::EPSILON,
        "{last:?}"
    );
    // Within one step of the cloud test's rate over the last 20 s of each
    // of the link's five steps.
    for phase in 0..5 {
        let from = f64::from(phase) * 30.0 + 10.0;
        let to = from + 20.0;
        let ours: Vec<f64> = replayed
            .iter()
            .filter(|r| (from..to).contains(&r.0))
            .map(|r| r.2.fps)
            .collect();
        let theirs: Vec<f64> = recorded
            .iter()
            .filter(|r| (from..to).contains(&r.t))
            .map(|r| r.fps)
            .collect();
        let step = |fps: f64| {
            DIVISORS
                .iter()
                .position(|d| (TEST_CAMERA.fps / f64::from(*d) - fps).abs() < 0.01)
                .unwrap()
        };
        let median = |v: &[f64]| {
            let mut v = v.to_vec();
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        let (ours, theirs) = (median(&ours), median(&theirs));
        println!("{from} s to {to} s: {ours} pictures a second, the cloud test's {theirs}");
        assert!(
            step(ours).abs_diff(step(theirs)) <= 1,
            "{from} s: {ours} against {theirs}"
        );
    }
}

#[test]
fn a_fall_is_at_once_a_rise_once_a_second_and_a_small_change_is_left() {
    let start = Instant::now();
    let mut rule = Rule::new(PLANE, 150_000);
    // 85% of 800 kbit/s.
    assert_eq!(rule.estimate(800_000, start).unwrap().bitrate, 680_000);
    // A fall of 3% is left alone, one of 10% taken at once.
    assert_eq!(rule.estimate(776_000, start), None);
    assert_eq!(rule.estimate(720_000, start).unwrap().bitrate, 612_000);
    // A rise waits a second from the last.
    let at = start + Duration::from_millis(100);
    assert_eq!(rule.estimate(900_000, at).unwrap().bitrate, 765_000);
    assert_eq!(
        rule.estimate(1_000_000, at + Duration::from_millis(500)),
        None
    );
    let later = at + Duration::from_secs(1);
    assert_eq!(rule.estimate(1_000_000, later).unwrap().bitrate, 850_000);
    // Never above the ceiling, nor under the floor.
    let much_later = later + Duration::from_secs(1);
    assert_eq!(
        rule.estimate(5_000_000, much_later).unwrap().bitrate,
        1_000_000
    );
    assert_eq!(rule.estimate(10_000, much_later).unwrap().bitrate, 150_000);
}

/// A camera whose encoder cannot go under a floor set by the scene (QP 42
/// in `OpenH264`), that floor falling with the frame rate. mediamtx-rpicamera
/// 2.9.0 leaves the encoder's frame rate alone, so it makes the bitrate
/// times the share of pictures that come. A helper that passes the frame
/// rate on makes the bitrate.
fn made(settings: Settings, own: Settings, scene_floor: u64, passes_rate: bool) -> u64 {
    let share = settings.fps / own.fps;
    let rate = if passes_rate {
        float(settings.bitrate)
    } else {
        float(settings.bitrate) * share
    };
    whole(rate.max(float(scene_floor) * share))
}

fn run(own: Settings, estimate: u64, scene_floor: u64, passes_rate: bool) -> (Rule, Vec<Settings>) {
    let start = Instant::now();
    let mut rule = Rule::new(own, 150_000);
    let mut seen = Vec::new();
    for second in 0..180 {
        let now = start + Duration::from_secs(second);
        rule.estimate(estimate, now);
        let made = made(rule.settings(), own, scene_floor, passes_rate);
        rule.second(made);
        seen.push(rule.settings());
    }
    (rule, seen)
}

#[test]
fn the_ratio_is_corrected_for_a_helper_that_does_change_the_frame_rate() {
    for passes_rate in [false, true] {
        let (rule, _) = run(TEST_CAMERA, 400_000, 1_000_000, passes_rate);
        let settings = rule.settings();
        let made = made(settings, TEST_CAMERA, 1_000_000, passes_rate);
        println!(
            "helper passes the rate: {passes_rate}, {settings:?}, made {made} for a target of {}",
            rule.target()
        );
        assert!(settings.fps < TEST_CAMERA.fps, "{settings:?}");
        let error = float(made) / float(rule.target()) - 1.0;
        assert!(error.abs() < 0.1, "made {made} for {}", rule.target());
    }
}

#[test]
fn the_plane_never_sends_above_its_ceiling_and_steps_to_a_fifth_at_its_floor() {
    // 100 kbit/s, under the floor, and a scene that needs 600 kbit/s at 30.
    let (rule, seen) = run(PLANE, 100_000, 600_000, false);
    assert!(seen.iter().all(|s| s.bitrate <= PLANE.bitrate), "{seen:?}");
    assert_eq!(rule.target(), 150_000);
    // A tenth would need 1.5 Mbit/s sent for 150 kbit/s made: over the
    // ceiling, so a fifth is the lowest.
    assert!(
        (rule.settings().fps - 6.0).abs() < f64::EPSILON,
        "{:?}",
        rule.settings()
    );
    assert_eq!(rule.settings().idr, 6);
}

/// The camera recording on the vehicle (ADR-0066) shares its sensor, so the
/// rule never steps below the recording's frame rate, says when it would,
/// and steps on down once the recording stops.
#[test]
fn a_camera_that_records_keeps_the_recordings_frame_rate_and_follows_with_the_bitrate() {
    let start = Instant::now();
    let mut rule = Rule::new(TEST_CAMERA, 150_000);
    let second = |rule: &mut Rule, at: u64| {
        rule.estimate(300_000, start + Duration::from_secs(at));
        rule.second(made(rule.settings(), TEST_CAMERA, SCENE_FLOOR, false));
        rule.settings()
    };
    assert_eq!(
        rule.hold(Some(25.0)).map(|s| s.fps),
        None,
        "nothing to change yet"
    );
    let mut lowest = f64::INFINITY;
    for at in 0..30 {
        let settings = second(&mut rule, at);
        lowest = lowest.min(settings.fps);
        assert!(settings.bitrate <= TEST_CAMERA.bitrate, "{settings:?}");
    }
    assert!(
        (lowest - 25.0).abs() < 0.01,
        "never under the recording's 25: {lowest}"
    );
    assert!(
        rule.held(),
        "says the connection carries less than 25 a second needs"
    );

    let raised = rule
        .hold(Some(50.0))
        .expect("back to the recording's 50 at once");
    assert!((raised.fps - 50.0).abs() < 0.01, "{raised:?}");
    for at in 30..40 {
        assert!((second(&mut rule, at).fps - 50.0).abs() < 0.01);
    }
    assert!(rule.held());
    assert!(
        rule.settings().bitrate <= 300_000,
        "the bitrate alone follows: {:?}",
        rule.settings()
    );

    rule.hold(None);
    assert!(!rule.held());
    let lowest = (40..70)
        .map(|at| second(&mut rule, at).fps)
        .fold(f64::INFINITY, f64::min);
    assert!(
        lowest <= 10.0,
        "steps down again once the recording stops: {lowest}"
    );
}
