//! The system clock and the OS random source behind the engine's ports.

use std::time::{SystemTime, UNIX_EPOCH};

use uxduck_engine::runtime::{Clock, Random};

#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_us(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX)
            })
    }
}

#[derive(Debug, Default)]
pub struct OsRandom;

impl Random for OsRandom {
    fn fill(&mut self, bytes: &mut [u8]) {
        // The OS source only fails on platforms we do not build for; a
        // predictable token would be worse than stopping.
        if let Err(error) = getrandom::fill(bytes) {
            panic!("the operating system's random source failed: {error}");
        }
    }
}
