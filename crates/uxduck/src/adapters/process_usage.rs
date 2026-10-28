//! On-demand sampling of this core only. No process discovery or background timer.
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use uxduck_contracts::dto::settings::CoreProcessUsage;

#[derive(Default)]
struct Sampler {
    system: System,
    previous: Option<(Instant, u64)>,
    reading: Option<CoreProcessUsage>,
}

impl Sampler {
    fn read(&mut self) -> Option<CoreProcessUsage> {
        let now = Instant::now();
        if self
            .previous
            .is_some_and(|(at, _)| now.duration_since(at) < Duration::from_secs(2))
        {
            return self.reading.clone();
        }
        let pid = Pid::from_u32(std::process::id());
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        let process = self.system.process(pid)?;
        let total = process.accumulated_cpu_time();
        let (cpu_percent, cpu_window_ms) = self.previous.map_or((None, None), |(at, previous)| {
            let elapsed = u32::try_from(now.duration_since(at).as_millis()).unwrap_or(u32::MAX);
            let percent =
                total.saturating_sub(previous).saturating_mul(100) / u64::from(elapsed.max(1));
            (
                Some(u32::try_from(percent).unwrap_or(u32::MAX)),
                Some(elapsed),
            )
        });
        let reading = CoreProcessUsage {
            resident_bytes: process.memory(),
            cpu_percent,
            cpu_window_ms,
        };
        self.previous = Some((now, total));
        self.reading = Some(reading.clone());
        Some(reading)
    }
}

pub fn snapshot() -> Option<CoreProcessUsage> {
    static SAMPLER: OnceLock<Mutex<Sampler>> = OnceLock::new();
    SAMPLER
        .get_or_init(|| Mutex::new(Sampler::default()))
        .lock()
        .ok()?
        .read()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn first_sample_has_memory_but_no_invented_cpu_and_immediate_reads_are_cached() {
        let mut sampler = Sampler::default();
        let first = sampler.read().expect("native self process");
        assert!(first.resident_bytes > 0);
        assert_eq!(first.cpu_percent, None);
        assert_eq!(first.cpu_window_ms, None);
        assert_eq!(sampler.read(), Some(first));
    }
    #[test]
    fn later_sample_reports_the_actual_interval_without_sleeping_or_dividing_by_core_count() {
        let mut sampler = Sampler::default();
        sampler.read().expect("native self process");
        let (_, total) = sampler.previous.expect("first baseline");
        let earlier = Instant::now()
            .checked_sub(Duration::from_secs(3))
            .expect("three seconds after the clock's start");
        sampler.previous = Some((earlier, total));
        let next = sampler.read().expect("second self sample");
        assert!(next.cpu_percent.is_some());
        assert!(next.cpu_window_ms.is_some_and(|ms| ms >= 3000));
    }
}
