//! On-demand Core and owned native simulator sampling. No background timer.
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use sysinfo::{CpuRefreshKind, Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use uxduck_contracts::dto::settings::{CoreProcessUsage, SimulationProcessUsage};

#[derive(Clone, Copy)]
struct ProcessSample {
    started: u64,
    cpu_ms: u64,
    memory: u64,
}

#[derive(Default)]
struct Sampler {
    system: System,
    previous: Option<Instant>,
    samples: HashMap<Pid, ProcessSample>,
    guards: HashSet<Pid>,
    reading: Option<CoreProcessUsage>,
}

/// Follow parent identities from registered guard roots, never executable names.
fn descendants(parents: &HashMap<Pid, Option<Pid>>, roots: &HashSet<Pid>) -> HashSet<Pid> {
    let mut owned = roots.clone();
    loop {
        let before = owned.len();
        for (pid, parent) in parents {
            if parent.is_some_and(|parent| owned.contains(&parent)) {
                owned.insert(*pid);
            }
        }
        if owned.len() == before {
            return owned;
        }
    }
}

fn usage(
    pids: &HashSet<Pid>,
    samples: &HashMap<Pid, ProcessSample>,
    previous: &HashMap<Pid, ProcessSample>,
    elapsed: Option<u32>,
) -> (u64, Option<u32>) {
    let memory = pids
        .iter()
        .filter_map(|pid| samples.get(pid))
        .map(|sample| sample.memory)
        .sum();
    // No simulators really is zero, including the first sample.
    if pids.is_empty() {
        return (memory, Some(0));
    }
    let Some(elapsed) = elapsed else {
        return (memory, None);
    };
    let cpu_ms = pids.iter().try_fold(0_u64, |total, pid| {
        let current = samples.get(pid)?;
        let old = previous.get(pid)?;
        (old.started == current.started)
            .then(|| total.saturating_add(current.cpu_ms.saturating_sub(old.cpu_ms)))
    });
    let cpu = cpu_ms.map(|cpu| {
        u32::try_from(cpu.saturating_mul(100) / u64::from(elapsed.max(1))).unwrap_or(u32::MAX)
    });
    (memory, cpu)
}

impl Sampler {
    fn read(&mut self, guard_pids: &[u32]) -> Option<CoreProcessUsage> {
        let now = Instant::now();
        let guards: HashSet<_> = guard_pids.iter().copied().map(Pid::from_u32).collect();
        if self
            .previous
            .is_some_and(|at| now.duration_since(at) < Duration::from_secs(2))
            && self.guards == guards
        {
            return self.reading.clone();
        }
        if self.system.cpus().is_empty() {
            self.system.refresh_cpu_list(CpuRefreshKind::nothing());
        }
        let core_pid = Pid::from_u32(std::process::id());
        // Process-tree discovery is needed only while this installation owns simulators.
        // Do not refresh command lines, names, paths or other processes' CPU/memory.
        let mut simulator_pids = HashSet::new();
        let mut live_guards = HashSet::new();
        if !guards.is_empty() {
            self.system.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing().without_tasks(),
            );
            let parents: HashMap<_, _> = self
                .system
                .processes()
                .iter()
                .map(|(pid, process)| (*pid, process.parent()))
                .collect();
            // Reject a stale root whose PID belongs to another parent's process.
            live_guards = guards
                .iter()
                .filter(|pid| parents.get(pid) == Some(&Some(core_pid)))
                .copied()
                .collect();
            simulator_pids = descendants(&parents, &live_guards);
        }
        let measured: Vec<_> = std::iter::once(core_pid)
            .chain(simulator_pids.iter().copied())
            .collect();
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&measured),
            true,
            ProcessRefreshKind::nothing()
                .without_tasks()
                .with_cpu()
                .with_memory(),
        );
        // Include every owned PID in `usage`: an unreadable process withholds CPU
        // and memory totals instead of making a partial total look complete.
        let samples: HashMap<_, _> = measured
            .iter()
            .filter_map(|pid| {
                let process = self.system.process(*pid)?;
                Some((
                    *pid,
                    ProcessSample {
                        started: process.start_time(),
                        cpu_ms: process.accumulated_cpu_time(),
                        memory: process.memory(),
                    },
                ))
            })
            .collect();
        let core = samples.get(&core_pid)?;
        let elapsed = self
            .previous
            .map(|at| u32::try_from(now.duration_since(at).as_millis()).unwrap_or(u32::MAX));
        let (_, cpu_percent) = usage(&HashSet::from([core_pid]), &samples, &self.samples, elapsed);
        let (resident_bytes, simulation_cpu) =
            usage(&simulator_pids, &samples, &self.samples, elapsed);
        let complete = live_guards.len() == guards.len()
            && simulator_pids.iter().all(|pid| samples.contains_key(pid));
        let reading = CoreProcessUsage {
            resident_bytes: core.memory,
            cpu_core_count: u32::try_from(self.system.cpus().len())
                .ok()
                .filter(|count| *count > 0),
            cpu_percent,
            cpu_window_ms: elapsed,
            simulations: complete.then_some(SimulationProcessUsage {
                count: u32::try_from(live_guards.len()).unwrap_or(u32::MAX),
                resident_bytes,
                cpu_percent: simulation_cpu,
                cpu_window_ms: elapsed,
            }),
        };
        self.previous = Some(now);
        self.samples = samples;
        self.guards = guards;
        self.reading = Some(reading.clone());
        Some(reading)
    }
}

pub fn snapshot(guard_pids: &[u32]) -> Option<CoreProcessUsage> {
    static SAMPLER: OnceLock<Mutex<Sampler>> = OnceLock::new();
    SAMPLER
        .get_or_init(|| Mutex::new(Sampler::default()))
        .lock()
        .ok()?
        .read(guard_pids)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn first_sample_has_memory_but_no_invented_cpu_and_immediate_reads_are_cached() {
        let mut sampler = Sampler::default();
        let first = sampler.read(&[]).expect("native self process");
        assert!(first.resident_bytes > 0);
        assert!(first.cpu_core_count.is_some_and(|count| count > 0));
        assert_eq!(first.cpu_percent, None);
        assert_eq!(first.cpu_window_ms, None);
        let simulations = first
            .simulations
            .as_ref()
            .expect("native simulator support");
        assert_eq!(simulations.count, 0);
        assert_eq!(simulations.resident_bytes, 0);
        assert_eq!(simulations.cpu_percent, Some(0));
        assert_eq!(sampler.read(&[]), Some(first));
    }
    #[test]
    fn later_sample_reports_the_actual_interval_without_dividing_by_core_count() {
        let mut sampler = Sampler::default();
        sampler.read(&[]).expect("native self process");
        sampler.previous = Instant::now().checked_sub(Duration::from_secs(3));
        let next = sampler.read(&[]).expect("second self sample");
        assert!(next.cpu_percent.is_some());
        assert!(next.cpu_window_ms.is_some_and(|ms| ms >= 3000));
    }
    #[test]
    fn owns_only_registered_guard_descendants_not_neighboring_apps_or_the_core() {
        let pid = Pid::from_u32;
        let parents = HashMap::from([
            (pid(1), None),
            (pid(2), Some(pid(1))),
            (pid(3), Some(pid(2))),
            (pid(4), Some(pid(3))),
            (pid(5), Some(pid(1))),
            (pid(6), Some(pid(5))),
        ]);
        assert_eq!(
            descendants(&parents, &HashSet::from([pid(2)])),
            HashSet::from([pid(2), pid(3), pid(4)])
        );
    }
    #[test]
    fn sums_process_deltas_and_resident_memory_and_discards_exited_processes() {
        let pid = Pid::from_u32;
        let before = HashMap::from([
            (
                pid(1),
                ProcessSample {
                    started: 10,
                    cpu_ms: 100,
                    memory: 20,
                },
            ),
            (
                pid(2),
                ProcessSample {
                    started: 11,
                    cpu_ms: 200,
                    memory: 30,
                },
            ),
        ]);
        let after = HashMap::from([
            (
                pid(1),
                ProcessSample {
                    started: 10,
                    cpu_ms: 500,
                    memory: 25,
                },
            ),
            (
                pid(2),
                ProcessSample {
                    started: 11,
                    cpu_ms: 1000,
                    memory: 35,
                },
            ),
        ]);
        assert_eq!(
            usage(
                &HashSet::from([pid(1), pid(2)]),
                &after,
                &before,
                Some(2000)
            ),
            (60, Some(60))
        );
        assert_eq!(
            usage(&HashSet::from([pid(2)]), &after, &before, Some(2000)),
            (35, Some(40))
        );
        assert_eq!(
            usage(&HashSet::new(), &after, &before, Some(2000)),
            (0, Some(0))
        );
    }
    #[test]
    fn newly_started_or_reused_pids_need_a_new_cpu_baseline() {
        let pid = Pid::from_u32(1);
        let before = HashMap::from([(
            pid,
            ProcessSample {
                started: 10,
                cpu_ms: 1000,
                memory: 20,
            },
        )]);
        let after = HashMap::from([(
            pid,
            ProcessSample {
                started: 20,
                cpu_ms: 50,
                memory: 30,
            },
        )]);
        let pids = HashSet::from([pid]);
        assert_eq!(usage(&pids, &after, &before, Some(2000)), (30, None));
        assert_eq!(
            usage(&pids, &after, &HashMap::new(), Some(2000)),
            (30, None)
        );
    }

    #[cfg(unix)]
    #[test]
    fn samples_a_real_owned_process_and_removes_it_without_counting_a_neighbor() {
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let owned = Child(
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("owned helper"),
        );
        let neighbor = Child(
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("unrelated helper"),
        );
        let mut sampler = Sampler::default();
        let first = sampler.read(&[owned.0.id()]).expect("process sample");
        let sim = first.simulations.expect("owned process is readable");
        assert_eq!(sim.count, 1);
        assert!(sim.resident_bytes > 0);
        assert_eq!(sim.cpu_percent, None);
        assert!(
            !sampler
                .samples
                .contains_key(&Pid::from_u32(neighbor.0.id()))
        );
        sampler.previous = Instant::now().checked_sub(Duration::from_secs(3));
        assert!(
            sampler
                .read(&[owned.0.id()])
                .unwrap()
                .simulations
                .unwrap()
                .cpu_percent
                .is_some()
        );
        drop(owned);
        let stopped = sampler.read(&[]).unwrap().simulations.unwrap();
        assert_eq!(stopped.count, 0);
        assert_eq!(stopped.cpu_percent, Some(0));
        assert_eq!(stopped.resident_bytes, 0);
    }
}
