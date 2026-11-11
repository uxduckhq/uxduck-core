//! Whether this computer's connection is metered (ADR-0046 B12): a phone's
//! hotspot, a cellular modem, or a network the person marked so. With
//! "Wait for unmetered networks" on, telemetry logs and video wait while it
//! is. Each system says so its own way: macOS's Network framework (an
//! expensive or constrained path, as Low Data Mode sets), Windows'
//! connection cost, and `NetworkManager`'s `Metered` on Linux. `None` when the
//! system cannot say, which waits for nothing.
//!
//! `UXDUCK_METERED` (`1` or `0`) stands in for the system, for tests.

/// Whether the connection is metered now, if the system says. May block
/// briefly on Linux: call it off the async threads.
pub fn metered() -> Option<bool> {
    if let Some(value) = std::env::var_os("UXDUCK_METERED") {
        return Some(value == "1");
    }
    platform::metered()
}

/// `NetworkManager`'s `NMMetered` as `busctl` prints it (`u 4`): yes and
/// guessed yes are metered, no and guessed no are not, unknown says nothing.
#[cfg(any(test, all(unix, not(target_os = "macos"))))]
fn read_busctl(out: &str) -> Option<bool> {
    match out.trim().strip_prefix("u ")?.trim().parse::<u32>().ok()? {
        1 | 3 => Some(true),
        2 | 4 => Some(false),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
#[allow(
    unsafe_code,
    reason = "the Network framework's path monitor is a C API with a block"
)]
mod platform {
    use std::ffi::{c_char, c_void};
    use std::sync::Once;
    use std::sync::atomic::{AtomicU8, Ordering};

    use block2::RcBlock;

    type Object = *mut c_void;

    #[link(name = "Network", kind = "framework")]
    unsafe extern "C" {
        fn nw_path_monitor_create() -> Object;
        fn nw_path_monitor_set_queue(monitor: Object, queue: Object);
        fn nw_path_monitor_set_update_handler(
            monitor: Object,
            handler: &block2::Block<dyn Fn(Object)>,
        );
        fn nw_path_monitor_start(monitor: Object);
        fn nw_path_is_expensive(path: Object) -> bool;
        fn nw_path_is_constrained(path: Object) -> bool;
    }

    unsafe extern "C" {
        fn dispatch_queue_create(label: *const c_char, attr: Object) -> Object;
    }

    /// 0 until the first path, then 1 metered or 2 not.
    static STATE: AtomicU8 = AtomicU8::new(0);
    static START: Once = Once::new();

    /// A path monitor started once, for as long as UXDuck runs, that keeps
    /// the latest path's answer.
    pub fn metered() -> Option<bool> {
        START.call_once(|| {
            let handler = RcBlock::new(|path: Object| {
                // SAFETY: the monitor passes a valid path for the call.
                let metered = unsafe { nw_path_is_expensive(path) || nw_path_is_constrained(path) };
                STATE.store(if metered { 1 } else { 2 }, Ordering::Relaxed);
            });
            // SAFETY: the monitor and queue are created here and never
            // released; the monitor copies the handler block.
            unsafe {
                let monitor = nw_path_monitor_create();
                let queue =
                    dispatch_queue_create(c"com.uxduck.metered".as_ptr(), std::ptr::null_mut());
                nw_path_monitor_set_queue(monitor, queue);
                nw_path_monitor_set_update_handler(monitor, &handler);
                nw_path_monitor_start(monitor);
            }
        });
        match STATE.load(Ordering::Relaxed) {
            1 => Some(true),
            2 => Some(false),
            _ => None,
        }
    }
}

#[cfg(windows)]
mod platform {
    use windows::Networking::Connectivity::{NetworkCostType, NetworkInformation};

    /// The internet connection's cost: fixed or variable (a data plan),
    /// roaming, or over its limit is metered.
    pub fn metered() -> Option<bool> {
        let profile = NetworkInformation::GetInternetConnectionProfile().ok()?;
        let cost = profile.GetConnectionCost().ok()?;
        let kind = cost.NetworkCostType().ok()?;
        Some(
            kind == NetworkCostType::Fixed
                || kind == NetworkCostType::Variable
                || cost.Roaming().unwrap_or(false)
                || cost.OverDataLimit().unwrap_or(false),
        )
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use std::process::{Command, Stdio};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// `NetworkManager` is asked at most this often.
    const ASK_EVERY: Duration = Duration::from_secs(30);

    static LAST: Mutex<Option<(Instant, Option<bool>)>> = Mutex::new(None);

    /// `NetworkManager`'s `Metered` for the primary connection, through
    /// `busctl`; a system without either says nothing.
    pub fn metered() -> Option<bool> {
        let mut last = LAST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((at, answer)) = *last
            && at.elapsed() < ASK_EVERY
        {
            return answer;
        }
        let answer = Command::new("busctl")
            .args([
                "--system",
                "--timeout=2",
                "get-property",
                "org.freedesktop.NetworkManager",
                "/org/freedesktop/NetworkManager",
                "org.freedesktop.NetworkManager",
                "Metered",
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|out| out.status.success())
            .and_then(|out| super::read_busctl(&String::from_utf8_lossy(&out.stdout)));
        *last = Some((Instant::now(), answer));
        answer
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    pub const fn metered() -> Option<bool> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn the_path_monitor_answers_soon_after_it_starts() {
        let started = std::time::Instant::now();
        while platform::metered().is_none() && started.elapsed() < std::time::Duration::from_secs(5)
        {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(platform::metered().is_some());
    }

    #[test]
    fn network_managers_metered_reads_as_people_mean_it() {
        assert_eq!(read_busctl("u 1\n"), Some(true));
        assert_eq!(read_busctl("u 3"), Some(true));
        assert_eq!(read_busctl("u 2\n"), Some(false));
        assert_eq!(read_busctl("u 4"), Some(false));
        assert_eq!(read_busctl("u 0"), None);
        assert_eq!(read_busctl(""), None);
        assert_eq!(read_busctl("s \"yes\""), None);
    }
}
