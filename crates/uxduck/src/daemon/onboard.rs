//! `uxduck forward --onboard` (S295, ADR-0049): the computer it runs on is
//! on the vehicle, so once a second it sends, out of every forward, a
//! heartbeat as an onboard controller and `ONBOARD_COMPUTER_STATUS`, from
//! the vehicle's system id and one of the four onboard computer component
//! ids. The ground station shows them with the vehicle's telemetry.
//!
//! It also answers the ground station's `TIMESYNC` requests addressed to it
//! with this computer's wall clock (S356), so the ground station can tell
//! how far this computer's clock is from its own. It stamps each status
//! with that clock as it is sent, for a ground station that only reads
//! those.
//!
//! While nothing keeps this computer's clock, such as a Raspberry Pi with
//! no clock battery and no time server it can reach, it asks the ground
//! station that asks it for its clock the same way, and sets its own to it
//! when they are further apart than the answer's uncertainty (S373): stepped
//! when it is far off, and slewed when the kernel can do that before the next
//! answer, so it never jumps or runs backwards for a small error.

use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use uxduck_mavlink::computer::{self, COMPUTER_IDS};
use uxduck_mavlink::computer_clock::{OUTSTANDING_US, UNIX_SCALE_NS};
use uxduck_mavlink::decode::{Message, decode};

use crate::adapters::computer_status::Sampler;
use crate::adapters::system_clock::{self, Moved};

/// An id heard this recently is someone else's.
const HEARD_FOR_US: u64 = 10_000_000;
/// A ground station that asked this recently is still there to ask.
const GROUND_HEARD_FOR: Duration = Duration::from_micros(HEARD_FOR_US);
/// How long one of this computer's requests waits for its reply, as long as
/// the ground station's own wait.
const REPLY_WAITS: Duration = Duration::from_micros(OUTSTANDING_US);
/// How long after an answer it asks the ground station's clock again (S373):
/// one answer sets the clock, and one a minute finds the drift since, small
/// enough to slew away before the next.
const ASK_AGAIN_AFTER: Duration = Duration::from_secs(60);

/// A test seam, never set in use: milliseconds added to this computer's
/// clock where it is told to the ground station, so a test can run a
/// computer whose clock is wrong without setting the real one.
pub const CLOCK_OFFSET_ENV: &str = "UXDUCK_ONBOARD_CLOCK_OFFSET_MS";

/// A test seam, never set in use, beside [`CLOCK_OFFSET_ENV`]: nothing keeps
/// the clock as it is told, so it is set from the ground station's by
/// changing that offset (S373), never the real clock.
pub const CLOCK_UNKEPT_ENV: &str = "UXDUCK_ONBOARD_CLOCK_UNKEPT";

/// What `--onboard` and `--computer-id` asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Onboard {
    /// The component id to send as; the first free one when `None`.
    pub component_id: Option<u8>,
}

/// The ids this computer sends as and the onboard computers it has heard.
pub struct Computer {
    pinned: Option<u8>,
    sampler: Sampler,
    /// The component id this computer has, per vehicle system id.
    ids: HashMap<u8, u8>,
    /// Other onboard computers, `(system, component)`, last heard when.
    heard: HashMap<(u8, u8), u64>,
    sequence: u8,
    /// Every id was taken, already said once.
    full: BTreeSet<u8>,
    /// The system and component ids it sent its status as last.
    sending: Vec<(u8, u8)>,
    /// Added to this computer's clock where it is told ([`CLOCK_OFFSET_ENV`]).
    offset_ns: i64,
    /// Its clock is told wrong on purpose ([`CLOCK_OFFSET_ENV`]), so the real
    /// clock is never set.
    told_wrong: bool,
    /// Nothing keeps the clock as told ([`CLOCK_UNKEPT_ENV`]), so it is set
    /// by changing `offset_ns`.
    told_unkept: bool,
    /// The ground station that asks this computer's clock, by system and
    /// component, and when it last did (S373).
    ground: Option<((u8, u8), Instant)>,
    /// This computer's requests for the ground station's clock: each `ts1`,
    /// its clock as told when it went, and when it went.
    asked: Vec<(i64, Instant)>,
    /// Not before then does it ask again.
    ask_after: Option<Instant>,
    /// It said it could not set the clock.
    set_refused: bool,
}

impl Computer {
    pub fn new(onboard: Onboard) -> Self {
        Self {
            pinned: onboard.component_id,
            sampler: Sampler::default(),
            ids: HashMap::new(),
            heard: HashMap::new(),
            sequence: 0,
            full: BTreeSet::new(),
            sending: Vec::new(),
            offset_ns: std::env::var(CLOCK_OFFSET_ENV)
                .ok()
                .and_then(|ms| ms.trim().parse::<i64>().ok())
                .map_or(0, |ms| ms.saturating_mul(1_000_000)),
            told_wrong: std::env::var(CLOCK_OFFSET_ENV)
                .is_ok_and(|ms| ms.trim().parse::<i64>().is_ok()),
            told_unkept: std::env::var_os(CLOCK_UNKEPT_ENV).is_some(),
            ground: None,
            asked: Vec::new(),
            ask_after: None,
            set_refused: false,
        }
    }

    /// This computer's wall clock as it tells it, in nanoseconds since 1970.
    pub fn now_ns(&self) -> i64 {
        let ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
        ns.saturating_add(self.offset_ns)
    }

    /// Its replies to a `TIMESYNC` request addressed to it, from each id it
    /// sends as (S356): the asker gets this computer's clock. Anything
    /// else, and a request before it has sent its first status, has none.
    /// A router such as mavp2p passes a companion the vehicle's whole
    /// stream, so any other message is let go on its header alone.
    ///
    /// The ground station whose request it answers is the one it asks, and
    /// that ground station's answer to its own request may set its clock
    /// (S373).
    pub fn answer(&mut self, request: &[u8]) -> Vec<Vec<u8>> {
        let Some(header) = uxduck_mavlink::framing::header(request)
            .filter(|h| h.message_id == uxduck_mavlink::link_health::TIMESYNC)
        else {
            return Vec::new();
        };
        self.ground_answered(request);
        let now_ns = self.now_ns();
        let mut replies = Vec::new();
        for from in self.sending.clone() {
            if let Some(reply) = computer::timesync_reply(request, from, self.sequence, now_ns) {
                self.next();
                replies.push(reply);
            }
        }
        if !replies.is_empty() && header.component_id == uxduck_mavlink::GCS_COMPONENT_ID {
            self.ground = Some(((header.system_id, header.component_id), Instant::now()));
        }
        replies
    }

    /// Whether something keeps this computer's clock, so it is never set.
    fn kept(&self) -> bool {
        if self.told_wrong {
            !self.told_unkept
        } else {
            system_clock::kept()
        }
    }

    /// The ground station to ask for its clock now (S373): while nothing
    /// keeps this computer's, once a second until one answers, then once a
    /// minute.
    fn ground_to_ask(&mut self) -> Option<(u8, u8)> {
        let now = Instant::now();
        self.asked
            .retain(|(_, at)| now.duration_since(*at) < REPLY_WAITS);
        let (ground, heard) = self.ground?;
        if now.duration_since(heard) >= GROUND_HEARD_FOR
            || self.ask_after.is_some_and(|after| now < after)
            || self.kept()
        {
            return None;
        }
        Some(ground)
    }

    /// The ground station's answer to one of this computer's requests
    /// (S373): its wall clock in `tc1`, on the Unix scale, from the ground
    /// station asked, to this computer. The ground station's clock less this
    /// one's is `tc1` less this computer's clock halfway through the round
    /// trip, true to within half the round trip. When nothing keeps this
    /// computer's clock and they are further apart than that, it is set.
    fn ground_answered(&mut self, frame: &[u8]) {
        let Some(decoded) = decode(frame) else {
            return;
        };
        let Message::Timesync {
            tc1,
            ts1,
            target_system,
            target_component,
        } = decoded.message
        else {
            return;
        };
        let from = (decoded.system_id, decoded.component_id);
        if tc1 < UNIX_SCALE_NS
            || self.ground.map(|(ground, _)| ground) != Some(from)
            || !self.sending.contains(&(target_system, target_component))
        {
            return;
        }
        let Some(at) = self.asked.iter().position(|(t, _)| *t == ts1) else {
            return;
        };
        let (_, sent) = self.asked.remove(at);
        let round_trip_ns = i64::try_from(sent.elapsed().as_nanos()).unwrap_or(i64::MAX);
        let within_ns = round_trip_ns / 2;
        let by_ns = tc1.saturating_sub(ts1.saturating_add(within_ns));
        self.ask_after = Some(Instant::now() + ASK_AGAIN_AFTER);
        if by_ns.unsigned_abs() <= within_ns.unsigned_abs() || self.kept() {
            return;
        }
        let moved = if self.told_wrong {
            self.offset_ns = self.offset_ns.saturating_add(by_ns);
            Ok(if system_clock::slews(by_ns, ASK_AGAIN_AFTER) {
                Moved::Slewed
            } else {
                Moved::Stepped
            })
        } else {
            system_clock::set(by_ns, ASK_AGAIN_AFTER)
        };
        match moved {
            Err(error) => {
                if !self.set_refused {
                    self.set_refused = true;
                    tracing::warn!(
                        %error,
                        off_by_ms = by_ns / 1_000_000,
                        "nothing keeps this computer's clock and it is off the ground station's, but it could not be set. Its service needs CAP_SYS_TIME, such as AmbientCapabilities=CAP_SYS_TIME in its systemd unit."
                    );
                }
                return;
            }
            Ok(Moved::Stepped) => tracing::info!(
                moved_ms = by_ns / 1_000_000,
                within_ms = within_ns / 1_000_000,
                ground_system = from.0,
                "set this computer's clock to the ground station's, because nothing else keeps it"
            ),
            Ok(Moved::Slewed) => tracing::debug!(
                moving_us = by_ns / 1_000,
                within_us = within_ns / 1_000,
                ground_system = from.0,
                "slewing this computer's clock to the ground station's"
            ),
        }
        self.set_refused = false;
        self.asked.clear();
    }

    /// A frame from a vehicle's link. Another computer with this one's id
    /// makes this one take another, unless it was pinned; whichever of two
    /// gives way is left to chance, so two that started together part.
    pub fn heard(&mut self, system_id: u8, component_id: u8, at_us: u64, coin: bool) {
        if !COMPUTER_IDS.contains(&component_id) {
            return;
        }
        self.heard.insert((system_id, component_id), at_us);
        if self.pinned.is_none() && self.ids.get(&system_id) == Some(&component_id) && coin {
            tracing::warn!(
                system_id,
                component_id,
                "another onboard computer sends as this one: taking another id"
            );
            self.ids.remove(&system_id);
        }
    }

    /// The id to send as for a vehicle, chosen once and kept.
    fn id_for(&mut self, system_id: u8, now_us: u64) -> Option<u8> {
        if let Some(id) = self.pinned.or_else(|| self.ids.get(&system_id).copied()) {
            return Some(id);
        }
        let taken = self
            .heard
            .iter()
            .filter(|((s, _), at)| *s == system_id && now_us.saturating_sub(**at) < HEARD_FOR_US)
            .map(|((_, c), _)| *c);
        let Some(id) = computer::free_id(taken) else {
            if self.full.insert(system_id) {
                tracing::warn!(
                    system_id,
                    "four onboard computers already on this vehicle: not sending this one's status; pin one with --computer-id"
                );
            }
            return None;
        };
        self.full.remove(&system_id);
        self.ids.insert(system_id, id);
        tracing::info!(
            system_id,
            component_id = id,
            "sending this computer's status"
        );
        Some(id)
    }

    /// This second's heartbeat and status for each vehicle system id.
    pub fn frames(&mut self, systems: impl IntoIterator<Item = u8>, now_us: u64) -> Vec<Vec<u8>> {
        let ids: Vec<(u8, u8)> = systems
            .into_iter()
            .collect::<BTreeSet<u8>>()
            .into_iter()
            .filter_map(|system| Some((system, self.id_for(system, now_us)?)))
            .collect();
        self.sending.clone_from(&ids);
        if ids.is_empty() {
            return Vec::new();
        }
        let mut status = self.sampler.sample();
        // Stamped once sampled, as it goes, for a ground station that reads
        // this computer's clock from it.
        status.time_usec = u64::try_from(self.now_ns() / 1000).unwrap_or(0);
        let mut frames = Vec::new();
        for &(system, component) in &ids {
            frames.push(computer::heartbeat_frame((system, component, self.next())));
            frames.push(computer::status_frame(
                (system, component, self.next()),
                &status,
            ));
        }
        if let Some(ground) = self.ground_to_ask() {
            let ts1 = self.now_ns();
            for (system, component) in ids {
                frames.push(uxduck_mavlink::sim::timesync_frame(
                    (system, component, self.next()),
                    (0, ts1),
                    ground,
                ));
            }
            self.asked.push((ts1, Instant::now()));
        }
        frames
    }

    fn next(&mut self) -> u8 {
        let sequence = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        sequence
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uxduck_mavlink::decode::{Message, decode};

    fn senders(frames: &[Vec<u8>]) -> Vec<(u8, u8, &'static str)> {
        frames
            .iter()
            .map(|f| {
                let d = decode(f).unwrap();
                let kind = match d.message {
                    Message::Heartbeat { .. } => "heartbeat",
                    Message::OnboardComputerStatus(_) => "status",
                    _ => "other",
                };
                (d.system_id, d.component_id, kind)
            })
            .collect()
    }

    #[test]
    fn it_sends_a_heartbeat_and_a_status_per_vehicle_from_the_first_free_id() {
        let mut computer = Computer::new(Onboard::default());
        assert!(computer.frames([], 0).is_empty());
        computer.heard(1, 191, 0, true);
        computer.heard(1, 1, 0, true);
        assert_eq!(
            senders(&computer.frames([1, 1], 1_000)),
            [(1, 192, "heartbeat"), (1, 192, "status")]
        );
        // Kept, even once the first computer is heard no more.
        assert_eq!(senders(&computer.frames([1], 60_000_000))[0].1, 192);
    }

    #[test]
    fn a_pinned_id_stays_and_a_chosen_one_gives_way() {
        let mut pinned = Computer::new(Onboard {
            component_id: Some(193),
        });
        pinned.heard(1, 193, 0, true);
        assert_eq!(senders(&pinned.frames([1], 0))[0].1, 193);

        let mut chosen = Computer::new(Onboard::default());
        assert_eq!(senders(&chosen.frames([1], 0))[0].1, 191);
        chosen.heard(1, 191, 1, false);
        assert_eq!(senders(&chosen.frames([1], 2))[0].1, 191);
        chosen.heard(1, 191, 3, true);
        assert_eq!(senders(&chosen.frames([1], 4))[0].1, 192);
    }

    #[test]
    fn it_answers_the_ground_stations_timesync_with_its_clock_and_stamps_its_status() {
        let mut computer = Computer::new(Onboard {
            component_id: Some(192),
        });
        computer.offset_ns = -712_800_000_000_000;
        let request = uxduck_mavlink::sim::timesync_frame((255, 190, 0), (0, 42), (1, 0));
        assert!(computer.answer(&request).is_empty(), "nothing sent as yet");
        let before = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let frames = computer.frames([1], 0);
        let after = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let Some(Message::OnboardComputerStatus(status)) = decode(&frames[1]).map(|d| d.message)
        else {
            panic!("a status");
        };
        let stamped_ms = i64::try_from(status.time_usec / 1000).unwrap();
        let told_ms =
            |at: std::time::Duration| i64::try_from(at.as_millis()).unwrap() - 712_800_000;
        assert!(
            (told_ms(before)..=told_ms(after)).contains(&stamped_ms),
            "its own clock, offset: {stamped_ms}"
        );
        let replies = computer.answer(&request);
        let answered_at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        assert_eq!(replies.len(), 1);
        let reply = decode(&replies[0]).unwrap();
        let Message::Timesync {
            tc1,
            ts1,
            target_system,
            target_component,
        } = reply.message
        else {
            panic!("a reply");
        };
        assert_eq!((reply.system_id, reply.component_id), (1, 192));
        assert_eq!((ts1, target_system, target_component), (42, 255, 190));
        assert!(
            (told_ms(after)..=told_ms(answered_at)).contains(&(tc1 / 1_000_000)),
            "{tc1}"
        );
        let sequence = |frame: &[u8]| uxduck_mavlink::framing::header(frame).unwrap().sequence;
        assert_eq!(sequence(&replies[0]), sequence(&frames[1]).wrapping_add(1));
        let elsewhere = uxduck_mavlink::sim::timesync_frame((255, 190, 1), (0, 42), (2, 0));
        assert!(computer.answer(&elsewhere).is_empty(), "another vehicle's");
        let autopilot = uxduck_mavlink::sim::timesync_frame((255, 190, 2), (0, 42), (1, 1));
        assert!(computer.answer(&autopilot).is_empty(), "the autopilot's");
        // A router such as mavp2p passes a companion the vehicle's whole
        // stream: none of it is answered, the autopilot's own reply to the
        // same request included.
        let answered = uxduck_mavlink::sim::timesync_frame((1, 1, 3), (7_000, 42), (255, 190));
        assert!(
            computer.answer(&answered).is_empty(),
            "the autopilot's reply"
        );
        assert!(computer.answer(&frames[0]).is_empty(), "a heartbeat");
        assert_eq!(
            computer.answer(&request).len(),
            1,
            "a sequence number is spent only on a reply"
        );
    }

    /// Buggy's camera Pi (2026-11-15): no clock battery and no time server
    /// it can reach, so it lost every hour the vehicle was off and ran 10
    /// days 5 h behind.
    #[test]
    fn a_clock_nothing_keeps_is_set_to_the_ground_stations() {
        let wall_ns = || {
            i64::try_from(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
            )
            .unwrap()
        };
        let asks = |frames: &[Vec<u8>]| {
            frames
                .iter()
                .filter_map(|f| {
                    let d = decode(f)?;
                    match d.message {
                        Message::Timesync {
                            tc1: 0,
                            ts1,
                            target_system,
                            target_component,
                        } => Some((
                            d.system_id,
                            d.component_id,
                            ts1,
                            target_system,
                            target_component,
                        )),
                        _ => None,
                    }
                })
                .collect::<Vec<_>>()
        };
        let mut computer = Computer::new(Onboard {
            component_id: Some(191),
        });
        computer.told_wrong = true;
        computer.told_unkept = true;
        computer.offset_ns = -883_493_000_000_000;
        assert!(
            asks(&computer.frames([1], 0)).is_empty(),
            "no ground station has asked"
        );

        let request = uxduck_mavlink::sim::timesync_frame((255, 190, 0), (0, 42), (1, 0));
        assert_eq!(computer.answer(&request).len(), 1);
        let asked = asks(&computer.frames([1], 1));
        let [(1, 191, ts1, 255, 190)] = asked[..] else {
            panic!("one request to the ground station that asked: {asked:?}");
        };
        let told = computer.now_ns();
        assert!((told - wall_ns() + 883_493_000_000_000).abs() < 1_000_000_000);

        let autopilot =
            uxduck_mavlink::sim::timesync_frame((1, 1, 0), (7_000_000_000, ts1), (1, 191));
        computer.answer(&autopilot);
        let other = uxduck_mavlink::sim::timesync_frame((254, 190, 0), (wall_ns(), ts1), (1, 191));
        computer.answer(&other);
        let elsewhere =
            uxduck_mavlink::sim::timesync_frame((255, 190, 0), (wall_ns(), ts1), (1, 192));
        computer.answer(&elsewhere);
        assert_eq!(
            computer.offset_ns, -883_493_000_000_000,
            "not the autopilot's time since boot, another ground station's or one to another computer"
        );

        let answered =
            uxduck_mavlink::sim::timesync_frame((255, 190, 1), (wall_ns(), ts1), (1, 191));
        assert!(
            computer.answer(&answered).is_empty(),
            "an answer is not asked back"
        );
        assert!(
            (computer.now_ns() - wall_ns()).abs() < 1_000_000_000,
            "set to the ground station's: {} ms off",
            (computer.now_ns() - wall_ns()) / 1_000_000
        );
        assert!(
            asks(&computer.frames([1], 2)).is_empty(),
            "answered: asked again in a minute"
        );
    }

    #[test]
    fn a_clock_within_the_answers_uncertainty_is_left_alone() {
        let mut computer = Computer::new(Onboard {
            component_id: Some(192),
        });
        computer.told_wrong = true;
        computer.told_unkept = true;
        let request = uxduck_mavlink::sim::timesync_frame((255, 190, 0), (0, 42), (1, 0));
        computer.frames([1], 0);
        computer.answer(&request);
        let frames = computer.frames([1], 1);
        let Some(Message::Timesync { ts1, .. }) =
            frames.last().and_then(|f| decode(f)).map(|d| d.message)
        else {
            panic!("a request");
        };
        std::thread::sleep(Duration::from_millis(20));
        let same =
            uxduck_mavlink::sim::timesync_frame((255, 190, 1), (computer.now_ns(), ts1), (1, 192));
        computer.answer(&same);
        assert_eq!(
            computer.offset_ns, 0,
            "the same clock, within half the round trip"
        );
    }

    #[test]
    fn with_four_computers_heard_a_fifth_sends_nothing() {
        let mut computer = Computer::new(Onboard::default());
        for id in COMPUTER_IDS {
            computer.heard(1, id, 0, true);
        }
        assert!(computer.frames([1], 1).is_empty());
        assert_eq!(senders(&computer.frames([1], HEARD_FOR_US + 1))[0].1, 191);
    }
}
