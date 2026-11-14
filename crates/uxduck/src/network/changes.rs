//! Says when this computer's network changes: an interface came up or went
//! down, or an address was added or removed. A UDP client waiting to retry
//! then does so at once (`docs/plan/vehicle-connections.md`). One poll of the
//! interface list serves macOS, Windows and Linux alike, because if-addrs has
//! no change notifier on macOS.

use std::collections::HashSet;
use std::time::Duration;

use uxduck_mavlink::links::NetworkChanges;

/// How often the interface list is read. A waiting client retries within
/// this long of a change.
const EVERY: Duration = Duration::from_secs(1);

/// Announces every change for as long as the daemon runs.
pub fn watch(changes: NetworkChanges) {
    tokio::spawn(async move {
        let mut last = None;
        loop {
            // A failed read says nothing, so the last list stays.
            if let Ok(found) = if_addrs::get_if_addrs()
                && differs(&mut last, found.into_iter().collect::<HashSet<_>>())
            {
                tracing::info!(
                    "the computer's network changed, so UDP clients waiting to retry do so now"
                );
                changes.announce();
            }
            tokio::time::sleep(EVERY).await;
        }
    });
}

/// Whether this read differs from the last, which it replaces. The first
/// read only sets what later ones are compared with.
fn differs<T: PartialEq>(last: &mut Option<T>, now: T) -> bool {
    let changed = last.as_ref().is_some_and(|last| *last != now);
    *last = Some(now);
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_is_any_difference_from_the_last_read() {
        let mut last = None;
        let wifi = "en0 up 192.168.1.4";
        assert!(
            !differs(&mut last, vec![wifi]),
            "the first read only sets the baseline"
        );
        assert!(!differs(&mut last, vec![wifi]));
        assert!(
            differs(&mut last, vec![wifi, "en5 up 192.168.50.10"]),
            "an address was added"
        );
        assert!(!differs(&mut last, vec![wifi, "en5 up 192.168.50.10"]));
        assert!(
            differs(&mut last, vec![wifi, "en5 down 192.168.50.10"]),
            "an interface went down"
        );
        assert!(differs(&mut last, vec![wifi]), "an address was removed");
    }
}
