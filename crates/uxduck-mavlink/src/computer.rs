//! Onboard computers (S295, ADR-0049): a companion such as a Raspberry Pi
//! says how it is doing with `ONBOARD_COMPUTER_STATUS`, from one of the four
//! component ids MAVLink keeps for onboard computers, beside a heartbeat as
//! `MAV_TYPE_ONBOARD_CONTROLLER`. `uxduck forward` sends both; any other
//! software that sends them is read the same way.

use mavio::dialects::ardupilotmega::enums::{MavAutopilot, MavModeFlag, MavState, MavType};
use mavio::dialects::ardupilotmega::messages::{Heartbeat, OnboardComputerStatus};

/// `MAV_COMP_ID_ONBOARD_COMPUTER` to `MAV_COMP_ID_ONBOARD_COMPUTER4`.
pub const COMPUTER_IDS: std::ops::RangeInclusive<u8> = 191..=194;

/// What one status says, in its own units, with what it left unused as
/// `None` or left out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ComputerStatus {
    pub uptime_ms: u32,
    /// Each core's use, in percent.
    pub cpu_cores_pct: Vec<u8>,
    pub board_degc: Option<i8>,
    pub core_degc: Vec<i8>,
    pub ram_used_mib: Option<u32>,
    pub ram_total_mib: Option<u32>,
    /// Summed over the storage it lists.
    pub storage_used_mib: Option<u32>,
    pub storage_total_mib: Option<u32>,
}

impl ComputerStatus {
    pub(crate) fn from_message(m: &OnboardComputerStatus) -> Self {
        let storage = |values: [u32; 4]| -> Option<u32> {
            let valid: Vec<u32> = values.into_iter().filter(|v| *v != u32::MAX).collect();
            (!valid.is_empty()).then(|| valid.iter().fold(0_u32, |a, v| a.saturating_add(*v)))
        };
        Self {
            uptime_ms: m.uptime,
            cpu_cores_pct: m.cpu_cores.into_iter().filter(|v| *v != u8::MAX).collect(),
            board_degc: (m.temperature_board != i8::MAX).then_some(m.temperature_board),
            core_degc: m
                .temperature_core
                .into_iter()
                .filter(|v| *v != i8::MAX)
                .collect(),
            ram_used_mib: (m.ram_usage != u32::MAX).then_some(m.ram_usage),
            ram_total_mib: (m.ram_total != u32::MAX).then_some(m.ram_total),
            storage_used_mib: storage(m.storage_usage),
            storage_total_mib: storage(m.storage_total),
        }
    }
}

/// The key prefix and name for a computer's items: `computer` for the first
/// onboard computer id, `computer_2` to `computer_4` for the others, as
/// batteries are numbered, and the component id for anything else that
/// sends a status. The browser keeps items by key alone, so two computers
/// never share one.
#[must_use]
pub fn group(component_id: u8) -> (String, String) {
    match component_id {
        191 => ("computer".to_owned(), "Companion computer".to_owned()),
        192..=194 => {
            let n = component_id - 190;
            (format!("computer_{n}"), format!("Companion computer {n}"))
        }
        other => (
            format!("computer_{other}"),
            format!("Companion computer {other}"),
        ),
    }
}

/// The first onboard computer id nobody else uses.
#[must_use]
pub fn free_id(taken: impl IntoIterator<Item = u8>) -> Option<u8> {
    let taken: Vec<u8> = taken.into_iter().collect();
    COMPUTER_IDS.into_iter().find(|id| !taken.contains(id))
}

/// An onboard computer's heartbeat: not an autopilot, so never a vehicle.
#[must_use]
pub fn heartbeat_frame((system_id, component_id, sequence): (u8, u8, u8)) -> Vec<u8> {
    let heartbeat = Heartbeat {
        type_: MavType::OnboardController,
        autopilot: MavAutopilot::Invalid,
        base_mode: MavModeFlag::empty(),
        custom_mode: 0,
        system_status: MavState::Active,
        mavlink_version: 3,
    };
    crate::sim::frame(system_id, component_id, sequence, &heartbeat)
}

/// `ONBOARD_COMPUTER_STATUS` from `component_id`; its `type` is the
/// computer's place among the onboard ids (0 for the first).
#[must_use]
pub fn status_frame(
    (system_id, component_id, sequence): (u8, u8, u8),
    time_usec: u64,
    status: &ComputerStatus,
) -> Vec<u8> {
    fn fill<T: Copy, const N: usize>(values: &[T], unused: T) -> [T; N] {
        let mut out = [unused; N];
        for (slot, value) in out.iter_mut().zip(values) {
            *slot = *value;
        }
        out
    }
    let used: Vec<u32> = status.storage_used_mib.into_iter().collect();
    let total: Vec<u32> = status.storage_total_mib.into_iter().collect();
    let message = OnboardComputerStatus {
        time_usec,
        uptime: status.uptime_ms,
        type_: component_id.saturating_sub(*COMPUTER_IDS.start()),
        cpu_cores: fill(&status.cpu_cores_pct, u8::MAX),
        cpu_combined: [u8::MAX; 10],
        gpu_cores: [u8::MAX; 4],
        gpu_combined: [u8::MAX; 10],
        temperature_board: status.board_degc.unwrap_or(i8::MAX),
        temperature_core: fill(&status.core_degc, i8::MAX),
        fan_speed: [i16::MAX; 4],
        ram_usage: status.ram_used_mib.unwrap_or(u32::MAX),
        ram_total: status.ram_total_mib.unwrap_or(u32::MAX),
        storage_type: [u32::MAX; 4],
        storage_usage: fill(&used, u32::MAX),
        storage_total: fill(&total, u32::MAX),
        link_type: [0; 6],
        link_tx_rate: [u32::MAX; 6],
        link_rx_rate: [u32::MAX; 6],
        link_tx_max: [u32::MAX; 6],
        link_rx_max: [u32::MAX; 6],
    };
    crate::sim::frame(system_id, component_id, sequence, &message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::{Message, decode};

    #[test]
    fn a_status_survives_the_wire_with_its_unused_fields_left_out() {
        let status = ComputerStatus {
            uptime_ms: 61_000,
            cpu_cores_pct: vec![12, 40, 7, 1],
            board_degc: None,
            core_degc: vec![63],
            ram_used_mib: Some(1_024),
            ram_total_mib: Some(4_096),
            storage_used_mib: Some(9_000),
            storage_total_mib: Some(30_000),
        };
        let decoded = decode(&status_frame((1, 192, 7), 5, &status)).unwrap();
        assert_eq!((decoded.system_id, decoded.component_id), (1, 192));
        assert_eq!(
            decoded.message,
            Message::OnboardComputerStatus(Box::new(status))
        );
    }

    #[test]
    fn its_heartbeat_is_an_onboard_controller_and_not_an_autopilot() {
        let decoded = decode(&heartbeat_frame((1, 191, 0))).unwrap();
        let Message::Heartbeat {
            mav_type,
            autopilot,
            ..
        } = decoded.message
        else {
            panic!("not a heartbeat");
        };
        assert_eq!(
            (mav_type.as_str(), autopilot.as_str()),
            ("OnboardController", "Invalid")
        );
    }

    #[test]
    fn computers_take_the_first_free_id_and_are_numbered_by_it() {
        assert_eq!(free_id([]), Some(191));
        assert_eq!(free_id([191, 193]), Some(192));
        assert_eq!(free_id(COMPUTER_IDS), None);
        assert_eq!(group(191).0, "computer");
        assert_eq!(
            group(193),
            ("computer_3".to_owned(), "Companion computer 3".to_owned())
        );
        assert_eq!(group(100).0, "computer_100");
    }
}
