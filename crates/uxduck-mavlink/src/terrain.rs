//! Answering a vehicle's terrain requests (S118, ADR-0026 T4): the heights
//! the engine found, sent as `TERRAIN_DATA` at a pace per vehicle.
//!
//! A vehicle asks for a block of its grid by its south-west corner and
//! spacing, with a mask of the 4 × 4 groups it lacks, and asks again every
//! few seconds for what is still missing. Each batch of answers for a block
//! comes with the latest mask: queued answers the vehicle no longer asks
//! for are dropped, so an older request's leftovers are never sent after a
//! newer one. The pace starts from QGroundControl's 12 a second
//! (about 0.7 kB/s) and is slower on serial radios; the engine chooses it.

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

/// At most this many answers go at once after a quiet spell.
const BURST: f64 = 2.0;

#[derive(Debug)]
struct Queue {
    per_second: f64,
    budget: f64,
    last_us: u64,
    blocks: VecDeque<(Block, VecDeque<Group>)>,
}

/// Answers waiting to go, by vehicle.
#[derive(Debug, Default)]
pub struct Outbox {
    queues: BTreeMap<String, Queue>,
}

impl Outbox {
    /// Queues a block's answers for a vehicle. Answers for the block not
    /// sent yet stay only if their group is in `mask`, the vehicle's latest
    /// request; a new answer for a group replaces a queued one.
    pub fn queue(
        &mut self,
        vehicle_id: &str,
        block: Block,
        groups: Vec<Group>,
        mask: u64,
        per_second: u8,
        now_us: u64,
    ) {
        let queue = self
            .queues
            .entry(vehicle_id.to_owned())
            .or_insert_with(|| Queue {
                per_second: 0.0,
                budget: 1.0,
                last_us: now_us,
                blocks: VecDeque::new(),
            });
        queue.per_second = f64::from(per_second.max(1));
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

    /// What may be sent now, oldest block first.
    pub fn due(&mut self, now_us: u64) -> Vec<(String, Block, Group)> {
        let mut due = Vec::new();
        for (vehicle_id, queue) in &mut self.queues {
            #[allow(clippy::cast_precision_loss, reason = "microseconds between ticks")]
            let elapsed_s = now_us.saturating_sub(queue.last_us) as f64 / 1e6;
            queue.last_us = now_us;
            queue.budget = (queue.budget + elapsed_s * queue.per_second).min(BURST);
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

    #[test]
    fn answers_go_at_the_pace_asked() {
        let mut outbox = Outbox::default();
        outbox.queue(
            "v",
            A,
            groups(&(0..24).collect::<Vec<_>>()),
            u64::MAX,
            12,
            0,
        );
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
    fn the_latest_request_decides_what_is_still_sent() {
        let mut outbox = Outbox::default();
        outbox.queue("v", A, groups(&[1, 2, 3, 4]), 0b11110, 12, 0);
        assert_eq!(outbox.due(0)[0].2.gridbit, 1);
        // The vehicle asked again: only 3 and 5 are still missing; 5's
        // answer comes now, 3's is queued already.
        outbox.queue("v", A, groups(&[5]), 0b10_1000, 12, 0);
        assert_eq!(outbox.waiting("v"), 2);
        let due: Vec<u8> = outbox.due(1_000_000).iter().map(|d| d.2.gridbit).collect();
        assert_eq!(due, [3, 5]);
        assert!(outbox.is_empty());
        // A request with nothing new to send still drops what it no longer wants.
        outbox.queue("v", A, groups(&[6, 7]), u64::MAX, 12, 2_000_000);
        outbox.queue("v", A, Vec::new(), 1 << 7, 12, 2_000_000);
        assert_eq!(outbox.waiting("v"), 1);
    }

    #[test]
    fn a_quiet_spell_does_not_store_up_a_flood() {
        let mut outbox = Outbox::default();
        outbox.queue("v", A, groups(&(0..10).collect::<Vec<_>>()), u64::MAX, 4, 0);
        assert_eq!(outbox.due(0).len(), 1);
        // A minute without a tick (a busy link): two, not 240.
        assert_eq!(outbox.due(60_000_000).len(), 2, "at most a burst of two");
    }
}
