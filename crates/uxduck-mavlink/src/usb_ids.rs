//! The known USB id table (ADR-0018 F2, S44): which vendor and product ids
//! are flight controllers, which are the same boards in their bootloaders,
//! and which are generic USB-serial adapters that may be a telemetry radio.
//! The system reports these ids without opening the device. The table is
//! small on purpose and grows by pull request from published board lists
//! and from finds; an unknown board is still found by "Look at every serial
//! port".

/// What an id says a device is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsbKind {
    /// A flight controller running its firmware: a strong match.
    FlightController,
    /// A flight controller in its bootloader: not MAVLink yet.
    Bootloader,
    /// A generic USB-serial adapter: maybe a telemetry radio, maybe
    /// anything else.
    Adapter,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KnownUsb {
    pub vendor_id: u16,
    pub product_id: u16,
    pub kind: UsbKind,
    /// What people call it.
    pub name: &'static str,
}

const fn known(vendor_id: u16, product_id: u16, kind: UsbKind, name: &'static str) -> KnownUsb {
    KnownUsb {
        vendor_id,
        product_id,
        kind,
        name,
    }
}

use UsbKind::{Adapter, Bootloader, FlightController};

/// The table. Ids that clones reuse are classed by what most people have.
pub const KNOWN: &[KnownUsb] = &[
    // ArduPilot's ChibiOS boards use pid.codes' ids unless the maker has its own.
    known(
        0x1209,
        0x5740,
        FlightController,
        "ArduPilot flight controller",
    ),
    known(0x1209, 0x5741, Bootloader, "ArduPilot bootloader"),
    // CubePilot.
    known(0x2dae, 0x1011, FlightController, "Cube Black"),
    known(0x2dae, 0x1016, FlightController, "Cube Orange"),
    known(0x2dae, 0x1058, FlightController, "Cube Orange+"),
    // 3D Robotics' ids, which PX4's FMU boards use.
    known(0x26ac, 0x0011, FlightController, "Pixhawk"),
    known(0x26ac, 0x0012, FlightController, "Pixracer"),
    known(0x26ac, 0x0032, FlightController, "Pixhawk 4"),
    // STM32's own: its bootloader, and the virtual serial port many boards
    // and other gadgets share.
    known(0x0483, 0xdf11, Bootloader, "STM32 bootloader"),
    known(0x0483, 0x5740, Adapter, "STM32 serial port"),
    // USB-serial chips, in telemetry radios (SiK, RFD) and much else.
    known(0x0403, 0x6001, Adapter, "FTDI serial adapter"),
    known(0x0403, 0x6015, Adapter, "FTDI serial adapter"),
    known(0x10c4, 0xea60, Adapter, "Silicon Labs serial adapter"),
    known(0x1a86, 0x7523, Adapter, "CH340 serial adapter"),
    known(0x067b, 0x2303, Adapter, "Prolific serial adapter"),
];

/// What the table knows about an id.
#[must_use]
pub fn lookup(vendor_id: u16, product_id: u16) -> Option<&'static KnownUsb> {
    KNOWN
        .iter()
        .find(|k| k.vendor_id == vendor_id && k.product_id == product_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_id_is_in_the_table_once() {
        let mut ids: Vec<(u16, u16)> = KNOWN.iter().map(|k| (k.vendor_id, k.product_id)).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(
            ids.len(),
            before,
            "an id is listed twice, perhaps as two kinds"
        );
        assert!(KNOWN.iter().all(|k| !k.name.is_empty()));
    }

    #[test]
    fn a_cube_orange_is_a_flight_controller_and_an_ftdi_chip_only_maybe_a_radio() {
        assert_eq!(
            lookup(0x2dae, 0x1016).map(|k| k.kind),
            Some(FlightController)
        );
        assert_eq!(lookup(0x0403, 0x6015).map(|k| k.kind), Some(Adapter));
        assert_eq!(lookup(0x1209, 0x5741).map(|k| k.kind), Some(Bootloader));
        assert_eq!(lookup(0xffff, 0xffff), None);
    }
}
