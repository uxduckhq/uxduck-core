//! How often each daemon task wakes (S24): every timer and loop calls
//! [`count`] with its name, and `daemon/wakeups` reports the totals, so an
//! idle daemon that is not idle can be traced to the task responsible.
//! Channel and socket waits that only wake for real work are not counted,
//! apart from the engine, whose count is the work arriving.

use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex};

use uxduck_engine::runtime::Clock as _;

use crate::adapters::system::SystemClock;

struct Counts {
    since_us: u64,
    tasks: BTreeMap<&'static str, u64>,
}

static COUNTS: LazyLock<Mutex<Counts>> = LazyLock::new(|| {
    Mutex::new(Counts {
        since_us: SystemClock.now_us(),
        tasks: BTreeMap::new(),
    })
});

/// One wakeup of the named task.
pub fn count(task: &'static str) {
    if let Ok(mut counts) = COUNTS.lock() {
        *counts.tasks.entry(task).or_default() += 1;
    }
}

/// When counting began, and each task's wakeups since.
pub fn snapshot() -> (u64, Vec<(String, u64)>) {
    COUNTS.lock().map_or_else(
        |_| (0, Vec::new()),
        |counts| {
            (
                counts.since_us,
                counts
                    .tasks
                    .iter()
                    .map(|(name, count)| ((*name).to_owned(), *count))
                    .collect(),
            )
        },
    )
}
