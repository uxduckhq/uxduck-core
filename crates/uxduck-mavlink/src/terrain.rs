//! Answering a vehicle's terrain requests (S118, ADR-0026 T4): the heights
//! the engine found, sent as `TERRAIN_DATA` at a pace per vehicle.
//!
//! A vehicle asks for a block of its grid by its south-west corner and
//! spacing, with a mask of the 4 × 4 groups it lacks, and asks again every
//! few seconds for what is still missing. Each batch of answers for a block
//! comes with the latest mask: queued answers the vehicle no longer asks
//! for are dropped, so an older request's leftovers are never sent after a
//! newer one. A simulator's answers go at once. Elsewhere the pace starts
//! from QGroundControl's 12 a second (about 0.7 kB/s), slower on serial;
//! the engine chooses it, and a telemetry radio's `RADIO_STATUS` moves it
//! by how much room the radio has to send.

use std::collections::{BTreeMap, VecDeque};

/// A block of the vehicle's grid: its south-west corner, 1e-7 degrees, and
/// its spacing in metres.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Block {
    pub lat: i32,
    pub lon: i32,
    pub spacing: u16,
}

/// One `TERRAIN_DATA`: a group's bit and its 16 heights in whole metres,
/// `heights[x * 4 + y]` with `x` north.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Group {
    pub gridbit: u8,
    pub heights: [i16; 16],
}

/// How fast a vehicle's answers go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pace {
    /// All at once: a link that never leaves the computer, a simulator's.
    Free,
    /// This many a second until the link's radio reports its transmit
    /// buffer (`RADIO_STATUS.txbuf`), then as fast as that buffer keeps up.
    Paced(u8),
}

/// At most this many answers, or [`BURST_S`] of them at the pace, go at
/// once after a quiet spell.
const BURST: f64 = 2.0;
/// Seconds of answers a tick may send: a fast pace is kept between ticks.
const BURST_S: f64 = 0.25;
/// A radio's report counts this long; then the link's own pace is back.
const RADIO_FRESH_US: u64 = 5_000_000;
/// The pace a radio's room can earn, and the least it falls to: a block
/// a second at most, and never stopped by a radio that reports no room.
const RADIO_MAX_PER_S: f64 = 56.0;
const RADIO_MIN_PER_S: f64 = 2.0;

#[derive(Debug)]
struct Queue {
    link_id: String,
    pace: Pace,
    budget: f64,
    last_us: u64,
    blocks: VecDeque<(Block, VecDeque<Group>)>,
}

/// What a link's telemetry radio last said of its transmit buffer, and the
/// pace it has earned so far. Paced like ArduPilot paces its own streams
/// (`GCS_MAVLINK::handle_radio_status`): slower as the buffer fills, faster
/// while it has room.
#[derive(Debug)]
struct Radio {
    /// Free transmit buffer, percent.
    txbuf: u8,
    heard_us: u64,
    /// Not yet applied to the pace.
    unread: bool,
    per_second: Option<f64>,
}

impl Radio {
    /// The pace for answers on this link, from `base` when it is first
    /// used, moved by each report once.
    fn pace(&mut self, base: f64) -> f64 {
        let pace = self.per_second.get_or_insert(base);
        if std::mem::take(&mut self.unread) {
            *pace = match self.txbuf {
                0..20 => *pace * 0.5,
                20..50 => *pace * 0.8,
                90.. => *pace + 4.0,
                _ => *pace,
            }
            .clamp(RADIO_MIN_PER_S, RADIO_MAX_PER_S);
        }
        *pace
    }
}

/// Answers waiting to go, by vehicle.
#[derive(Debug, Default)]
pub struct Outbox {
    queues: BTreeMap<String, Queue>,
    /// Telemetry radios heard, by link.
    radios: BTreeMap<String, Radio>,
}

impl Outbox {
    /// Queues a block's answers for a vehicle on a link. Answers for the
    /// block not sent yet stay only if their group is in `mask`, the
    /// vehicle's latest request; a new answer for a group replaces a queued
    /// one.
    #[allow(clippy::too_many_arguments, reason = "one request's parts")]
    pub fn queue(
        &mut self,
        vehicle_id: &str,
        link_id: &str,
        block: Block,
        groups: Vec<Group>,
        mask: u64,
        pace: Pace,
        now_us: u64,
    ) {
        let queue = self
            .queues
            .entry(vehicle_id.to_owned())
            .or_insert_with(|| Queue {
                link_id: link_id.to_owned(),
                pace,
                budget: 1.0,
                last_us: now_us,
                blocks: VecDeque::new(),
            });
        link_id.clone_into(&mut queue.link_id);
        queue.pace = pace;
        let fresh: Vec<u8> = groups.iter().map(|g| g.gridbit).collect();
        if let Some((_, queued)) = queue.blocks.iter_mut().find(|(b, _)| *b == block) {
            queued.retain(|g| mask & (1 << g.gridbit) != 0 && !fresh.contains(&g.gridbit));
            queued.extend(groups);
        } else if !groups.is_empty() {
            queue.blocks.push_back((block, groups.into()));
        }
        queue.blocks.retain(|(_, g)| !g.is_empty());
        if queue.blocks.is_empty() {
            self.queues.remove(vehicle_id);
        }
    }

    /// A telemetry radio on a link reported its free transmit buffer.
    pub fn radio_status(&mut self, link_id: &str, txbuf: u8, now_us: u64) {
        let radio = self
            .radios
            .entry(link_id.to_owned())
            .or_insert_with(|| Radio {
                txbuf,
                heard_us: now_us,
                unread: false,
                per_second: None,
            });
        radio.txbuf = txbuf.min(100);
        radio.heard_us = now_us;
        radio.unread = true;
    }

    /// What may be sent now, oldest block first.
    pub fn due(&mut self, now_us: u64) -> Vec<(String, Block, Group)> {
        let mut due = Vec::new();
        for (vehicle_id, queue) in &mut self.queues {
            #[allow(clippy::cast_precision_loss, reason = "microseconds between ticks")]
            let elapsed_s = now_us.saturating_sub(queue.last_us) as f64 / 1e6;
            queue.last_us = now_us;
            let Pace::Paced(base) = queue.pace else {
                for (block, groups) in queue.blocks.drain(..) {
                    due.extend(groups.into_iter().map(|g| (vehicle_id.clone(), block, g)));
                }
                continue;
            };
            let base = f64::from(base.max(1));
            let per_second = match self.radios.get_mut(&queue.link_id) {
                Some(radio) if now_us.saturating_sub(radio.heard_us) <= RADIO_FRESH_US => {
                    radio.pace(base)
                }
                _ => base,
            };
            queue.budget =
                (queue.budget + elapsed_s * per_second).min(BURST.max(per_second * BURST_S));
            while queue.budget >= 1.0 {
                let Some((block, groups)) = queue.blocks.front_mut() else {
                    break;
                };
                let block = *block;
                if let Some(group) = groups.pop_front() {
                    due.push((vehicle_id.clone(), block, group));
                    queue.budget -= 1.0;
                }
                if groups.is_empty() {
                    queue.blocks.pop_front();
                }
            }
        }
        self.queues.retain(|_, q| !q.blocks.is_empty());
        due
    }

    /// Whether anything is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queues.is_empty()
    }

    /// How many answers wait for a vehicle.
    #[must_use]
    pub fn waiting(&self, vehicle_id: &str) -> usize {
        self.queues
            .get(vehicle_id)
            .map_or(0, |q| q.blocks.iter().map(|(_, g)| g.len()).sum())
    }

    /// Drops what waits for a vehicle that went.
    pub fn forget(&mut self, vehicle_id: &str) {
        self.queues.remove(vehicle_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Block = Block {
        lat: 530_646_790,
        lon: -40_798_165,
        spacing: 100,
    };

    fn groups(bits: &[u8]) -> Vec<Group> {
        bits.iter()
            .map(|&gridbit| Group {
                gridbit,
                heights: [i16::from(gridbit); 16],
            })
            .collect()
    }

    fn all() -> Vec<Group> {
        groups(&(0..56).collect::<Vec<_>>())
    }

    #[test]
    fn answers_go_at_the_pace_asked() {
        let mut outbox = Outbox::default();
        let first = groups(&(0..24).collect::<Vec<_>>());
        outbox.queue("v", "l", A, first, u64::MAX, Pace::Paced(12), 0);
        // One at once, then twelve a second.
        assert_eq!(outbox.due(0).len(), 1);
        let mut sent = 1;
        for tick in 1..=10 {
            sent += outbox.due(tick * 100_000).len();
        }
        assert_eq!(sent, 13, "one, then twelve in a second");
        assert_eq!(outbox.waiting("v"), 11);
    }

    #[test]
    fn a_simulators_answers_go_at_once() {
        let mut outbox = Outbox::default();
        outbox.queue("v", "l", A, all(), u64::MAX, Pace::Free, 0);
        assert_eq!(outbox.due(0).len(), 56);
        assert!(outbox.is_empty());
    }

    #[test]
    fn the_latest_request_decides_what_is_still_sent() {
        let mut outbox = Outbox::default();
        let pace = Pace::Paced(12);
        outbox.queue("v", "l", A, groups(&[1, 2, 3, 4]), 0b11110, pace, 0);
        assert_eq!(outbox.due(0)[0].2.gridbit, 1);
        // The vehicle asked again: only 3 and 5 are still missing; 5's
        // answer comes now, 3's is queued already.
        outbox.queue("v", "l", A, groups(&[5]), 0b10_1000, pace, 0);
        assert_eq!(outbox.waiting("v"), 2);
        let due: Vec<u8> = outbox.due(1_000_000).iter().map(|d| d.2.gridbit).collect();
        assert_eq!(due, [3, 5]);
        assert!(outbox.is_empty());
        // A request with nothing new to send still drops what it no longer wants.
        outbox.queue("v", "l", A, groups(&[6, 7]), u64::MAX, pace, 2_000_000);
        outbox.queue("v", "l", A, Vec::new(), 1 << 7, pace, 2_000_000);
        assert_eq!(outbox.waiting("v"), 1);
    }

    /// Answers sent over `seconds` of 100 ms ticks from `from_us`, the
    /// radio reporting `txbuf` once a second.
    fn over(outbox: &mut Outbox, from_us: u64, seconds: u64, txbuf: u8) -> usize {
        let mut sent = 0;
        for tick in 1..=seconds * 10 {
            let now = from_us + tick * 100_000;
            if tick % 10 == 0 {
                outbox.radio_status("l", txbuf, now);
            }
            sent += outbox.due(now).len();
        }
        sent
    }

    #[test]
    fn a_radio_with_room_earns_a_faster_pace_and_a_full_one_slows_it() {
        let mut outbox = Outbox::default();
        for i in 0..40 {
            let block = Block {
                lat: A.lat + i * 10_000,
                ..A
            };
            outbox.queue("v", "l", block, all(), u64::MAX, Pace::Paced(12), 0);
        }
        outbox.due(0);
        // Room to spare: four more a second each report, up to a block a
        // second.
        let roomy = over(&mut outbox, 0, 20, 100);
        assert!(roomy > 12 * 20 + 200, "{roomy}");
        let fast = over(&mut outbox, 20_000_000, 2, 100);
        assert!((100..=114).contains(&fast), "{fast} in 2 s at 56 a second");
        // A filling buffer halves it each report, to no less than two.
        let full = over(&mut outbox, 22_000_000, 10, 5);
        let crawl = over(&mut outbox, 32_000_000, 2, 5);
        assert!(full < 150, "{full}");
        assert!(crawl <= 5, "{crawl} in 2 s at two a second");
    }

    #[test]
    fn a_radio_gone_quiet_leaves_the_links_own_pace() {
        let mut outbox = Outbox::default();
        outbox.queue("v", "l", A, all(), u64::MAX, Pace::Paced(12), 0);
        outbox.radio_status("l", 5, 0);
        outbox.due(0);
        // Five seconds after its last report the radio no longer counts.
        let sent = (1..=70)
            .map(|t| (t, outbox.due(t * 100_000).len()))
            .filter(|(t, _)| *t > 60)
            .map(|(_, n)| n)
            .sum::<usize>();
        assert!((11..=13).contains(&sent), "{sent} in a second at twelve");
    }

    #[test]
    fn another_links_radio_does_not_pace_it() {
        let mut outbox = Outbox::default();
        outbox.queue("v", "l", A, all(), u64::MAX, Pace::Paced(12), 0);
        outbox.queue("w", "m", A, all(), u64::MAX, Pace::Paced(12), 0);
        outbox.due(0);
        outbox.radio_status("m", 0, 0);
        let mut by_vehicle = BTreeMap::<String, usize>::new();
        for tick in 1..=10 {
            for (vehicle, _, _) in outbox.due(tick * 100_000) {
                *by_vehicle.entry(vehicle).or_default() += 1;
            }
        }
        assert_eq!(by_vehicle["v"], 12);
        assert!(by_vehicle["w"] <= 7, "{by_vehicle:?}");
    }

    #[test]
    fn a_fast_pace_is_kept_between_ticks() {
        let mut outbox = Outbox::default();
        outbox.queue("v", "l", A, all(), u64::MAX, Pace::Paced(56), 0);
        // The data plane's ticks are 100 ms apart: a whole block in a
        // second, not two answers a tick.
        let mut sent = outbox.due(0).len();
        for tick in 1..=10 {
            sent += outbox.due(tick * 100_000).len();
        }
        assert_eq!(sent, 56);
        // A minute without a tick still sends a quarter second's worth.
        outbox.queue("v", "l", A, all(), u64::MAX, Pace::Paced(56), 2_000_000);
        assert_eq!(outbox.due(62_000_000).len(), 14);
    }

    #[test]
    fn a_quiet_spell_does_not_store_up_a_flood() {
        let mut outbox = Outbox::default();
        let ten = groups(&(0..10).collect::<Vec<_>>());
        outbox.queue("v", "l", A, ten, u64::MAX, Pace::Paced(4), 0);
        assert_eq!(outbox.due(0).len(), 1);
        // A minute without a tick (a busy link): two, not 240.
        assert_eq!(outbox.due(60_000_000).len(), 2, "at most a burst of two");
    }
}
