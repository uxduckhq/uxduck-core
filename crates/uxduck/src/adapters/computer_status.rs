//! This computer's own health for `uxduck forward --onboard` (S295,
//! ADR-0049): its cores' use and temperatures, memory, the storage it runs
//! from and its uptime, read once a second when the status is sent. A
//! reading the computer does not offer is left out, never made up.

use std::path::Path;

use sysinfo::{Components, Disks, System};
use uxduck_mavlink::computer::ComputerStatus;

const MIB: u64 = 1024 * 1024;

/// Holds the previous CPU reading, without which use cannot be told.
pub struct Sampler {
    system: System,
    disks: Disks,
    components: Components,
}

impl Default for Sampler {
    fn default() -> Self {
        let mut system = System::new();
        system.refresh_cpu_usage();
        Self {
            system,
            disks: Disks::new_with_refreshed_list(),
            components: Components::new_with_refreshed_list(),
        }
    }
}

impl Sampler {
    pub fn sample(&mut self) -> ComputerStatus {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        self.disks.refresh(true);
        self.components.refresh(true);
        let cores: Vec<f32> = self
            .system
            .cpus()
            .iter()
            .map(sysinfo::Cpu::cpu_usage)
            .collect();
        let temperatures: Vec<(&str, f32)> = self
            .components
            .iter()
            .filter_map(|c| Some((c.label(), c.temperature()?)))
            .collect();
        let disks: Vec<(&Path, u64, u64)> = self
            .disks
            .iter()
            .map(|d| (d.mount_point(), d.total_space(), d.available_space()))
            .collect();
        status_of(
            &cores,
            &temperatures,
            (self.system.used_memory(), self.system.total_memory()),
            &disks,
            System::uptime(),
        )
    }
}

/// What a sample says. The cores' temperatures are the components named as
/// a processor's (`cpu_thermal` on a Raspberry Pi), or every one when none
/// is; the storage is the root file system's.
fn status_of(
    cores: &[f32],
    temperatures: &[(&str, f32)],
    (memory_used, memory_total): (u64, u64),
    disks: &[(&Path, u64, u64)],
    uptime_s: u64,
) -> ComputerStatus {
    let cpu = |label: &str| {
        let label = label.to_lowercase();
        ["cpu", "core", "soc", "package", "tdie", "tctl"]
            .iter()
            .any(|name| label.contains(name))
    };
    let degrees = |only_cpu: bool| -> Vec<i8> {
        temperatures
            .iter()
            .filter(|(label, t)| t.is_finite() && (!only_cpu || cpu(label)))
            .map(|(_, t)| clamp_i8(*t))
            .collect()
    };
    let mut core_degc = degrees(true);
    if core_degc.is_empty() {
        core_degc = degrees(false);
    }
    core_degc.truncate(8);
    let root = disks.iter().find(|(mount, ..)| *mount == Path::new("/"));
    let mib = |bytes: u64| u32::try_from(bytes / MIB).unwrap_or(u32::MAX - 1);
    ComputerStatus {
        uptime_ms: u32::try_from(uptime_s.saturating_mul(1000)).unwrap_or(u32::MAX),
        cpu_cores_pct: cores.iter().take(8).map(|p| clamp_pct(*p)).collect(),
        board_degc: None,
        core_degc,
        ram_used_mib: (memory_total > 0).then(|| mib(memory_used)),
        ram_total_mib: (memory_total > 0).then(|| mib(memory_total)),
        storage_used_mib: root.map(|(_, total, free)| mib(total.saturating_sub(*free))),
        storage_total_mib: root.map(|(_, total, _)| mib(*total)),
    }
}

/// MAVLink keeps `i8::MAX` for "not given".
#[allow(clippy::cast_possible_truncation)]
fn clamp_i8(degrees: f32) -> i8 {
    degrees.round().clamp(-128.0, 126.0) as i8
}

/// MAVLink keeps `u8::MAX` for "not given".
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn clamp_pct(pct: f32) -> u8 {
    if pct.is_finite() {
        pct.round().clamp(0.0, 100.0) as u8
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_raspberry_pi_says_its_cpu_temperature_memory_and_root_storage() {
        let status = status_of(
            &[12.4, 99.6, 0.0, 50.0],
            &[("cpu_thermal temp1", 61.7), ("rp1_adc temp1", 48.0)],
            (1_024 * MIB, 4_096 * MIB),
            &[
                (Path::new("/boot/firmware"), 512 * MIB, 400 * MIB),
                (Path::new("/"), 30_000 * MIB, 21_000 * MIB),
            ],
            90,
        );
        assert_eq!(
            status,
            ComputerStatus {
                uptime_ms: 90_000,
                cpu_cores_pct: vec![12, 100, 0, 50],
                board_degc: None,
                core_degc: vec![62],
                ram_used_mib: Some(1_024),
                ram_total_mib: Some(4_096),
                storage_used_mib: Some(9_000),
                storage_total_mib: Some(30_000),
            }
        );
    }

    #[test]
    fn without_a_named_processor_every_temperature_counts_and_nothing_is_made_up() {
        let status = status_of(&[], &[("acpitz", 40.2), ("nvme", f32::NAN)], (0, 0), &[], 0);
        assert_eq!(status.core_degc, vec![40]);
        assert!(status.cpu_cores_pct.is_empty());
        assert_eq!((status.ram_used_mib, status.ram_total_mib), (None, None));
        assert_eq!(
            (status.storage_used_mib, status.storage_total_mib),
            (None, None)
        );
    }

    #[test]
    fn the_sampler_reads_this_computer() {
        let status = Sampler::default().sample();
        assert!(status.ram_total_mib.is_some_and(|m| m > 0));
        assert!(!status.cpu_cores_pct.is_empty());
    }
}
