//! Waiting for native gamepad input, independently of gilrs's platform backend.

use std::time::Duration;

// Fewer than 20 retries/second for a source that keeps returning immediately
// (S24), without imposing the old one-second delay on the next live input.
const RETRY_BACKOFF: Duration = Duration::from_millis(60);

/// A blocking gilrs read can return None after draining a Linux input fd,
/// before returning to epoll. Retry that boundary immediately. Consecutive
/// empty reads can also come from ignored device traffic, so keep their
/// backoff bounded while avoiding a busy loop if the source fails.
pub(super) fn next<T>(mut read: impl FnMut() -> Option<T>, mut pause: impl FnMut(Duration)) -> T {
    let mut retry = false;
    loop {
        if let Some(event) = read() {
            return event;
        }
        if retry {
            pause(RETRY_BACKOFF);
        }
        retry = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draining_linux_input_batches_does_not_delay_fresh_sticks() {
        // gilrs-core's Linux backend keeps the fd in `to_check` after an
        // event. The following read drains SYN_REPORT/EAGAIN, removes the
        // fd and returns None; only the next call waits in epoll again.
        let mut input = [
            Some(0.0),
            None,
            Some(0.4),
            None,
            Some(-0.6),
            None,
            Some(0.0),
        ]
        .into_iter();
        let mut pauses = Vec::new();
        let mut read = || input.next().expect("unexpected extra backend read");
        for expected in [0.0, 0.4, -0.6, 0.0] {
            let actual: f64 = next(&mut read, |delay| pauses.push(delay));
            assert!((actual - expected).abs() < f64::EPSILON);
        }
        assert!(pauses.is_empty(), "normal input batches slept: {pauses:?}");
    }

    #[test]
    fn repeated_empty_reads_back_off_briefly_and_recover_without_delay() {
        let mut input = [None, None, None, Some(1), None, Some(2)].into_iter();
        let mut pauses = Vec::new();
        let mut read = || input.next().expect("unexpected extra backend read");
        assert_eq!(next(&mut read, |delay| pauses.push(delay)), 1);
        assert_eq!(pauses, [Duration::from_millis(60); 2]);
        pauses.clear();
        assert_eq!(next(&mut read, |delay| pauses.push(delay)), 2);
        assert!(pauses.is_empty(), "recovery retained a failure backoff");
    }

    #[test]
    fn queued_button_and_disconnect_events_are_not_discarded() {
        let mut input = [Some("press"), Some("release"), Some("disconnect")].into_iter();
        for expected in ["press", "release", "disconnect"] {
            assert_eq!(
                next(
                    || input.next().expect("unexpected extra backend read"),
                    |_| panic!("pending events must not sleep"),
                ),
                expected,
            );
        }
    }
}
