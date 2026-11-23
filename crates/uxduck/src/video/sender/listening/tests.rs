//! The step a viewer's estimate gives, its hysteresis, the floor, the
//! picture's share less the sound, and one stream across a change of step
//! and a new run of the capture process.

use super::*;

#[test]
fn a_viewer_gets_the_highest_step_that_costs_at_most_15_percent_of_its_estimate() {
    assert_eq!(
        STEPS.map(|s| s.wire()),
        [55_200, 47_200, 27_600, 19_733],
        "about 55, 47, 28 and 20 kbit/s on the wire"
    );
    assert_eq!(allowed(2_000_000), 0);
    assert_eq!(allowed(368_000), 0, "32 kbit/s from about 370");
    assert_eq!(allowed(367_000), 1);
    assert_eq!(allowed(315_000), 1, "24 kbit/s from about 315");
    assert_eq!(allowed(314_000), 2);
    assert_eq!(allowed(184_000), 2, "16 kbit/s from about 190");
    assert_eq!(allowed(183_000), 3);
    assert_eq!(allowed(0), 3, "never below 12 kbit/s and never dropped");
}

#[test]
fn at_the_150_kbit_floor_sound_is_the_lowest_step_and_about_13_percent() {
    let step = allowed(150_000);
    assert_eq!(step, 3);
    assert_eq!(STEPS[step].bitrate, 12_000);
    let share = f64::from(STEPS[step].wire()) / 150_000.0;
    assert!((0.13..0.135).contains(&share), "{share}");
    assert_eq!(
        for_the_picture(150_000, Some(step)),
        130_267,
        "the rule hears what the sound leaves"
    );
}

#[test]
fn the_rule_is_given_the_estimate_less_the_steps_wire_cost_only_while_someone_listens() {
    assert_eq!(for_the_picture(1_000_000, None), 1_000_000);
    assert_eq!(for_the_picture(1_000_000, Some(0)), 944_800);
    assert_eq!(for_the_picture(400_000, Some(1)), 352_800);
    assert_eq!(for_the_picture(10_000, Some(3)), 0);
}

#[test]
fn a_step_down_is_at_once_and_a_step_up_after_the_estimate_allowed_it_for_5_s() {
    let start = Instant::now();
    let at = |s: u64| start + Duration::from_secs(s);
    let mut listener = Listener::default();
    assert_eq!(listener.step(), 3, "the lowest before any estimate");
    listener.start(Some(1_000_000), at(0));
    assert_eq!(
        listener.step(),
        0,
        "an unmute takes what the estimate allows at once"
    );
    listener.estimate(Some(200_000), at(1));
    assert_eq!(listener.step(), 2, "down at once");
    listener.estimate(Some(1_000_000), at(2));
    listener.estimate(Some(1_000_000), at(6));
    assert_eq!(listener.step(), 2, "up only after 5 s");
    listener.estimate(Some(1_000_000), at(7));
    assert_eq!(listener.step(), 0);
    listener.estimate(Some(1_000_000), at(8));
    listener.estimate(Some(300_000), at(9));
    assert_eq!(listener.step(), 2);
    listener.estimate(Some(1_000_000), at(10));
    listener.estimate(Some(300_000), at(12));
    listener.estimate(Some(1_000_000), at(13));
    listener.estimate(Some(1_000_000), at(17));
    assert_eq!(listener.step(), 2, "a dip starts the 5 s again");
    listener.estimate(Some(1_000_000), at(18));
    assert_eq!(listener.step(), 0);
}

/// The packets of each step of one run, in the order they arrive: each
/// when its sound has ended.
fn run(ssrc: u32, from_ms: u32, to_ms: u32, start: Instant) -> Vec<(usize, u32, u32, Instant)> {
    let mut packets = Vec::new();
    for (step, encode) in STEPS.iter().enumerate() {
        let mut ms = from_ms;
        while ms + encode.ms <= to_ms {
            let end = start + Duration::from_millis(u64::from(ms + encode.ms));
            packets.push((step, ssrc, ms * 48, end));
            ms += encode.ms;
        }
    }
    packets.sort_by_key(|(step, _, _, end)| (*end, *step));
    packets
}

/// What the viewer was sent: each packet's step, its time on the run and
/// on the viewer's stream.
fn listen(
    listener: &mut Listener,
    packets: &[(usize, u32, u32, Instant)],
    estimates: &[(Instant, u64)],
) -> Vec<(usize, u32, u64)> {
    let mut sent = Vec::new();
    for (step, ssrc, timestamp, at) in packets {
        for (when, estimate) in estimates {
            if when <= at {
                listener.estimate(Some(*estimate), *at);
            }
        }
        if let Some(out) = listener.packet(*step, *ssrc, *timestamp, *at) {
            sent.push((*step, *timestamp, out));
        }
    }
    sent
}

/// Each packet starts where the last ended, on the viewer's stream.
fn gapless(sent: &[(usize, u32, u64)]) {
    for pair in sent.windows(2) {
        let (step, _, out) = pair[0];
        assert_eq!(
            pair[1].2,
            out + u64::from(STEPS[step].samples()),
            "{pair:?} in {sent:?}"
        );
    }
}

#[test]
fn a_change_of_step_goes_on_at_the_same_times_with_no_gap_or_overlap() {
    let start = Instant::now();
    let packets = run(1, 0, 2000, start);
    let mut listener = Listener::default();
    listener.start(Some(1_000_000), start);
    let down = start + Duration::from_millis(510);
    let sent = listen(&mut listener, &packets, &[(down, 150_000)]);
    gapless(&sent);
    assert_eq!(sent.first().map(|s| (s.0, s.2)), Some((0, 0)));
    assert_eq!(sent.last().map(|s| s.0), Some(3), "moved to 12 kbit/s");
    let switch = sent.iter().find(|s| s.0 == 3).unwrap();
    assert_eq!(
        u64::from(switch.1),
        switch.2,
        "the run's timeline and the viewer's are one"
    );

    // And back up, which waits 5 s.
    let later = run(1, 2000, 9000, start);
    let up = start + Duration::from_millis(2100);
    let sent = listen(&mut listener, &later, &[(up, 1_000_000)]);
    gapless(&sent);
    assert_eq!(sent.last().map(|s| s.0), Some(0));
    let first_up = sent.iter().find(|s| s.0 == 0).unwrap();
    assert!(first_up.1 >= (2100 + 5000) * 48, "{first_up:?}");
}

#[test]
fn a_new_run_goes_on_after_the_time_that_passed_and_never_back() {
    let start = Instant::now();
    let mut listener = Listener::default();
    listener.start(Some(150_000), start);
    let first = listen(&mut listener, &run(1, 0, 600, start), &[]);
    gapless(&first);
    let end = first.last().unwrap().2 + u64::from(STEPS[3].samples());
    // The process ended and started again 1 s later: its timeline starts at
    // 0 again.
    let again = start + Duration::from_millis(1600);
    let second = listen(&mut listener, &run(2, 0, 600, again), &[]);
    gapless(&second);
    let resumed = second.first().unwrap().2;
    assert!(
        (end + 48_000 - 960..=end + 48_000 + 960).contains(&resumed),
        "{end} then {resumed}"
    );
}

#[test]
fn unmuting_again_goes_on_from_the_last_time() {
    let start = Instant::now();
    let packets = run(1, 0, 3000, start);
    let mut listener = Listener::default();
    listener.start(Some(1_000_000), start);
    let first = listen(&mut listener, &packets[..20], &[]);
    let last = *first.last().unwrap();
    listener.start(Some(1_000_000), start + Duration::from_secs(2));
    let later: Vec<_> = packets
        .iter()
        .copied()
        .filter(|p| p.3 >= start + Duration::from_secs(2))
        .collect();
    let second = listen(&mut listener, &later, &[]);
    assert_eq!(
        second[0].2 - last.2,
        u64::from(second[0].1 - last.1),
        "the time muted passes on the viewer's stream too"
    );
}

#[test]
fn a_short_first_packet_from_opusenc_is_followed_by_the_rest_of_the_run() {
    let start = Instant::now();
    let mut listener = Listener::default();
    listener.start(Some(1_000_000), start);
    let times = [110_u32, 758, 1718, 2678];
    let out: Vec<_> = times
        .iter()
        .enumerate()
        .map(|(i, &timestamp)| {
            let at = start + Duration::from_millis(20 * i as u64);
            listener.packet(0, 7, timestamp, at)
        })
        .collect();
    assert_eq!(out, [Some(0), Some(648), Some(1608), Some(2568)]);
    assert_eq!(listener.packet(0, 7, 2678, start), None);
}
