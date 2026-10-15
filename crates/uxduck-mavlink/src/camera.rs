//! The MAVLink camera protocol's video streams (ADR-0017 V3, V5, S39): a
//! camera component that announces itself is asked once for
//! `VIDEO_STREAM_INFORMATION`, and again a few times until it answers.

use std::collections::BTreeMap;

/// `VIDEO_STREAM_INFORMATION`'s message id.
pub const VIDEO_STREAM_INFORMATION: u32 = 269;
const ATTEMPTS: u8 = 3;
const RETRY_US: u64 = 3_000_000;

/// A camera by `(link, peer, system id, component id)`.
pub type Key = (String, String, u8, u8);

#[derive(Clone, Copy, Debug, Default)]
struct Ask {
    attempts: u8,
    last_us: u64,
    answered: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Cameras {
    asks: BTreeMap<Key, Ask>,
}

impl Cameras {
    /// A camera's heartbeat.
    pub fn heard(&mut self, key: Key) {
        self.asks.entry(key).or_default();
    }

    pub fn answered(&mut self, key: &Key) {
        if let Some(ask) = self.asks.get_mut(key) {
            ask.answered = true;
        }
    }

    /// The cameras to ask now, among those whose vehicle is known.
    pub fn due(&mut self, now_us: u64, known: impl Fn(&Key) -> bool) -> Vec<Key> {
        let mut out = Vec::new();
        for (key, ask) in &mut self.asks {
            let waited = ask.attempts == 0 || now_us.saturating_sub(ask.last_us) >= RETRY_US;
            if !ask.answered && ask.attempts < ATTEMPTS && waited && known(key) {
                ask.attempts += 1;
                ask.last_us = now_us;
                out.push(key.clone());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_camera_is_asked_when_its_vehicle_is_known_and_again_until_it_answers() {
        let mut cameras = Cameras::default();
        let key: Key = ("l".into(), "p".into(), 1, 100);
        cameras.heard(key.clone());
        assert!(cameras.due(0, |_| false).is_empty(), "no vehicle yet");
        assert_eq!(cameras.due(0, |_| true), std::slice::from_ref(&key));
        assert!(cameras.due(1_000_000, |_| true).is_empty(), "too soon");
        assert_eq!(cameras.due(3_000_000, |_| true).len(), 1);
        assert_eq!(cameras.due(6_000_000, |_| true).len(), 1);
        assert!(
            cameras.due(9_000_000, |_| true).is_empty(),
            "three times at most"
        );
        let mut answered = Cameras::default();
        answered.heard(key.clone());
        answered.answered(&key);
        assert!(answered.due(0, |_| true).is_empty());
    }
}
