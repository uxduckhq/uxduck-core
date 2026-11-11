//! `uxduck forward --onboard` (S295, ADR-0049): the computer it runs on is
//! on the vehicle, so once a second it sends, out of every forward, a
//! heartbeat as an onboard controller and `ONBOARD_COMPUTER_STATUS`, from
//! the vehicle's system id and one of the four onboard computer component
//! ids. The ground station shows them with the vehicle's telemetry.

use std::collections::{BTreeSet, HashMap};

use uxduck_mavlink::computer::{self, COMPUTER_IDS};

use crate::adapters::computer_status::Sampler;

/// An id heard this recently is someone else's.
const HEARD_FOR_US: u64 = 10_000_000;

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
        }
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
        if ids.is_empty() {
            return Vec::new();
        }
        let status = self.sampler.sample();
        let mut frames = Vec::new();
        for (system, component) in ids {
            frames.push(computer::heartbeat_frame((system, component, self.next())));
            frames.push(computer::status_frame(
                (system, component, self.next()),
                now_us,
                &status,
            ));
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
    fn with_four_computers_heard_a_fifth_sends_nothing() {
        let mut computer = Computer::new(Onboard::default());
        for id in COMPUTER_IDS {
            computer.heard(1, id, 0, true);
        }
        assert!(computer.frames([1], 1).is_empty());
        assert_eq!(senders(&computer.frames([1], HEARD_FOR_US + 1))[0].1, 191);
    }
}
