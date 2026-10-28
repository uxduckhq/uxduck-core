//! A fake board's bootloader (S189), for tests and `uxduck dev fake-board`:
//! it answers the bootloader protocol ([`crate::bootloader`]) as a board
//! does, keeps a flash of its size, and can be told to misbehave (a wrong
//! CRC, silence from a step on). [`FakeBoard`] is a fake vehicle on a
//! serial port that restarts into this bootloader when asked
//! (`MAV_CMD_PREFLIGHT_REBOOT_SHUTDOWN` 3) and back into its firmware once
//! the bootloader is told to restart, reporting the version written.

use crate::bootloader::{
    CHIP_ERASE, EOC, FAILED, GET_CRC, GET_DEVICE, GET_SYNC, INFO_BL_REV, INFO_BOARD_ID,
    INFO_BOARD_REV, INFO_FLASH_SIZE, INSYNC, INVALID, OK, PROG_MULTI, PROG_MULTI_MAX, REBOOT,
    crc32,
};

/// The bootloader's side of the protocol.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each is how the fake behaves or what happened to it"
)]
#[derive(Clone, Debug)]
pub struct FakeBootloader {
    pub bootloader: u32,
    pub board_id: u32,
    pub board_rev: u32,
    /// The firmware's flash, erased as `0xFF`.
    pub flash: Vec<u8>,
    /// Where the next `PROG_MULTI` writes.
    address: usize,
    /// Bytes of a command not whole yet.
    pending: Vec<u8>,
    /// Reports a CRC one off, as a bad write would.
    pub wrong_crc: bool,
    /// Says nothing from this command on, as a board unplugged mid-way.
    pub silent_from: Option<u8>,
    silent: bool,
    /// Told to restart: the firmware runs.
    pub rebooted: bool,
    /// Erased at least once.
    pub erased: bool,
}

impl FakeBootloader {
    #[must_use]
    pub fn new(board_id: u32, flash_size: usize) -> Self {
        Self {
            bootloader: 5,
            board_id,
            board_rev: 0,
            flash: vec![0xFF; flash_size],
            address: 0,
            pending: Vec::new(),
            wrong_crc: false,
            silent_from: None,
            silent: false,
            rebooted: false,
            erased: false,
        }
    }

    /// What was written, without the erased bytes after it.
    #[must_use]
    pub fn written(&self) -> &[u8] {
        let end = self
            .flash
            .iter()
            .rposition(|b| *b != 0xFF)
            .map_or(0, |i| i + 1);
        &self.flash[..end]
    }

    /// Bytes from the port; the answers to every whole command in them.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(&command) = self.pending.first() {
            let length = match command {
                GET_SYNC | CHIP_ERASE | GET_CRC | REBOOT => 2,
                GET_DEVICE => 3,
                PROG_MULTI => match self.pending.get(1) {
                    Some(n) => usize::from(*n) + 3,
                    None => break,
                },
                // Not a command: anything else on the port, such as MAVLink
                // the firmware sent before it restarted. Skipped.
                _ => {
                    self.pending.remove(0);
                    continue;
                }
            };
            if self.pending.len() < length {
                break;
            }
            let whole: Vec<u8> = self.pending.drain(..length).collect();
            if self.silent_from == Some(command) {
                self.silent = true;
            }
            if self.silent {
                continue;
            }
            if whole.last() != Some(&EOC) {
                out.extend([INSYNC, INVALID]);
                continue;
            }
            self.answer(&whole, &mut out);
        }
        out
    }

    fn answer(&mut self, whole: &[u8], out: &mut Vec<u8>) {
        match whole[0] {
            GET_SYNC => out.extend([INSYNC, OK]),
            GET_DEVICE => {
                let value = match whole[1] {
                    INFO_BL_REV => self.bootloader,
                    INFO_BOARD_ID => self.board_id,
                    INFO_BOARD_REV => self.board_rev,
                    INFO_FLASH_SIZE => u32::try_from(self.flash.len()).unwrap_or(u32::MAX),
                    _ => {
                        out.extend([INSYNC, INVALID]);
                        return;
                    }
                };
                out.extend(value.to_le_bytes());
                out.extend([INSYNC, OK]);
            }
            CHIP_ERASE => {
                self.flash.fill(0xFF);
                self.address = 0;
                self.erased = true;
                out.extend([INSYNC, OK]);
            }
            PROG_MULTI => {
                let data = &whole[2..whole.len() - 1];
                if !data.len().is_multiple_of(4) || data.len() > PROG_MULTI_MAX {
                    out.extend([INSYNC, INVALID]);
                } else if self.address + data.len() > self.flash.len() {
                    out.extend([INSYNC, FAILED]);
                } else {
                    self.flash[self.address..self.address + data.len()].copy_from_slice(data);
                    self.address += data.len();
                    out.extend([INSYNC, OK]);
                }
            }
            GET_CRC => {
                let crc = crc32(0, &self.flash).wrapping_add(u32::from(self.wrong_crc));
                out.extend(crc.to_le_bytes());
                out.extend([INSYNC, OK]);
            }
            REBOOT => {
                self.rebooted = true;
                out.extend([INSYNC, OK]);
            }
            _ => out.extend([INSYNC, INVALID]),
        }
    }
}

/// What a fake board runs now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoardMode {
    Firmware,
    Bootloader,
}

/// A fake vehicle on a serial port, with its bootloader (S189).
pub struct FakeBoard {
    pub vehicle: crate::sim::FakeVehicle,
    pub bootloader: FakeBootloader,
    pub mode: BoardMode,
    /// Bytes of a `MAVLink` frame not whole yet.
    mavlink: Vec<u8>,
}

impl std::fmt::Debug for FakeBoard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeBoard")
            .field("mode", &self.mode)
            .field("board_id", &self.bootloader.board_id)
            .finish_non_exhaustive()
    }
}

/// `MAV_CMD_PREFLIGHT_REBOOT_SHUTDOWN`.
const REBOOT_SHUTDOWN: u16 = 246;

impl FakeBoard {
    /// A board running `vehicle`, with a bootloader for `board_id` and
    /// `flash_size` bytes of flash.
    #[must_use]
    pub fn new(vehicle: crate::sim::FakeVehicle, board_id: u32, flash_size: usize) -> Self {
        Self {
            vehicle,
            bootloader: FakeBootloader::new(board_id, flash_size),
            mode: BoardMode::Firmware,
            mavlink: Vec::new(),
        }
    }

    /// Bytes from the port; what the board sends back.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        match self.mode {
            BoardMode::Bootloader => {
                let out = self.bootloader.feed(bytes);
                if self.bootloader.rebooted {
                    self.bootloader.rebooted = false;
                    self.start_firmware();
                }
                out
            }
            BoardMode::Firmware => {
                self.mavlink.extend_from_slice(bytes);
                let mut out = Vec::new();
                let mut frames = Vec::new();
                while let Some((frame, used)) = next_frame(&self.mavlink) {
                    self.mavlink.drain(..used);
                    frames.push(frame);
                }
                for frame in frames {
                    let to_bootloader = restart_into_bootloader(&frame);
                    for reply in self.vehicle.answer(&frame) {
                        out.extend(reply);
                    }
                    if to_bootloader {
                        self.mode = BoardMode::Bootloader;
                        self.mavlink.clear();
                    }
                }
                out
            }
        }
    }

    /// The firmware starts: with the version written, when the image says
    /// one (`UXDUCK-FAKE-FIRMWARE <major>.<minor>.<patch>` at its start),
    /// as `AUTOPILOT_VERSION` will report it.
    fn start_firmware(&mut self) {
        self.mode = BoardMode::Firmware;
        if let Some(version) = fake_firmware_version(self.bootloader.written()) {
            self.vehicle.flight_sw_version = version;
        }
        self.vehicle.reboot();
    }
}

/// The marker a fake firmware image starts with, then its version.
pub const FAKE_FIRMWARE: &str = "UXDUCK-FAKE-FIRMWARE ";

/// A fake image for `version` ("4.7.2"), `size` bytes long.
#[must_use]
pub fn fake_firmware_image(version: &str, size: usize) -> Vec<u8> {
    let mut image = format!("{FAKE_FIRMWARE}{version}\n").into_bytes();
    let mut byte = 0u8;
    while image.len() < size {
        image.push(byte);
        byte = byte.wrapping_add(37);
    }
    image
}

/// `AUTOPILOT_VERSION`'s `flight_sw_version` for a fake image's version:
/// major, minor and patch, released.
#[must_use]
pub fn fake_firmware_version(image: &[u8]) -> Option<u32> {
    let rest = image.strip_prefix(FAKE_FIRMWARE.as_bytes())?;
    let end = rest.iter().position(|b| *b == b'\n')?;
    let text = std::str::from_utf8(&rest[..end]).ok()?;
    let mut parts = text.split('.').map(|p| p.parse::<u8>().ok());
    let (major, minor, patch) = (parts.next()??, parts.next()??, parts.next()??);
    Some(u32::from_be_bytes([major, minor, patch, 255]))
}

/// The first whole `MAVLink` frame in `bytes`, and how many bytes it and
/// anything before it take.
fn next_frame(bytes: &[u8]) -> Option<(Vec<u8>, usize)> {
    let start = bytes.iter().position(|b| *b == 0xFD || *b == 0xFE)?;
    let length = crate::framing::frame_len(&bytes[start..])?;
    (bytes.len() >= start + length).then(|| (bytes[start..start + length].to_vec(), start + length))
}

/// A `COMMAND_LONG` asking to restart and stay in the bootloader.
fn restart_into_bootloader(frame: &[u8]) -> bool {
    matches!(
        crate::decode::decode(frame).map(|d| d.message),
        Some(crate::decode::Message::CommandLong { command, param1, .. })
            if command == REBOOT_SHUTDOWN && (param1 - 3.0).abs() < 0.5
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "deterministic tests")]

    use super::*;

    #[test]
    fn a_fake_image_carries_its_version() {
        let image = fake_firmware_image("4.7.2", 64);
        assert_eq!(image.len(), 64);
        assert_eq!(
            fake_firmware_version(&image),
            Some(u32::from_be_bytes([4, 7, 2, 255]))
        );
        assert_eq!(fake_firmware_version(b"something else"), None);
    }

    #[test]
    fn commands_split_across_reads_are_answered_once_whole() {
        let mut fake = FakeBootloader::new(140, 64);
        assert!(fake.feed(&[GET_DEVICE, INFO_BOARD_ID]).is_empty());
        let out = fake.feed(&[EOC, GET_SYNC]);
        assert_eq!(out, [140, 0, 0, 0, INSYNC, OK]);
        assert_eq!(fake.feed(&[EOC]), [INSYNC, OK]);
        // A word that is not whole is refused.
        assert_eq!(fake.feed(&[PROG_MULTI, 3, 1, 2, 3, EOC]), [INSYNC, INVALID]);
    }
}
