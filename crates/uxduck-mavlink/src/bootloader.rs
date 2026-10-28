//! The serial bootloader that PX4 and `ArduPilot` boards start in (S189,
//! ADR-0037 V10): UXDuck installs firmware through it. A board's bootloader
//! runs for a few seconds after power-up, or until its firmware is
//! installed when the firmware restarted into it on purpose
//! (`MAV_CMD_PREFLIGHT_REBOOT_SHUTDOWN` 3), and speaks a small command
//! protocol over USB serial: every command ends with `EOC`, and every
//! answer ends with `INSYNC` and a status (`OK`, `FAILED` or `INVALID`).
//!
//! Written for UXDuck from the protocol's public description (the command
//! and reply bytes, `GET_DEVICE`'s fields, programming in chunks of at most
//! 252 bytes that are whole words, and the CRC-32 over the whole flash with
//! unwritten bytes as `0xFF`); no code was taken from `ArduPilot`'s or PX4's
//! uploaders. Installing never writes the bootloader itself, so a failed
//! install is recovered by installing again.
//!
//! [`Bootloader`] speaks it over any [`Port`]; [`install`] runs the stages
//! in order, and can be cancelled only until erasing starts.

use std::io::{ErrorKind, Read, Write};
use std::time::{Duration, Instant};

/// Sent before every status.
pub const INSYNC: u8 = 0x12;
/// Ends every command.
pub const EOC: u8 = 0x20;
pub const OK: u8 = 0x10;
pub const FAILED: u8 = 0x11;
pub const INVALID: u8 = 0x13;
/// Some old STM32F4 silicon cannot use the top 1 MB; said by `GET_SYNC`.
pub const BAD_SILICON_REV: u8 = 0x14;

pub const GET_SYNC: u8 = 0x21;
pub const GET_DEVICE: u8 = 0x22;
pub const CHIP_ERASE: u8 = 0x23;
pub const PROG_MULTI: u8 = 0x27;
pub const GET_CRC: u8 = 0x29;
pub const REBOOT: u8 = 0x30;

/// `GET_DEVICE`'s fields.
pub const INFO_BL_REV: u8 = 1;
pub const INFO_BOARD_ID: u8 = 2;
pub const INFO_BOARD_REV: u8 = 3;
pub const INFO_FLASH_SIZE: u8 = 4;

/// The most bytes one `PROG_MULTI` carries: under its one-byte length, and
/// whole 32-bit words.
pub const PROG_MULTI_MAX: usize = 252;

/// The bootloader revisions UXDuck speaks to: 3 added `GET_CRC`, which
/// verifying needs; 5 is current.
pub const BL_REV_MIN: u32 = 3;
pub const BL_REV_MAX: u32 = 5;

/// How long a short command may take.
const SHORT: Duration = Duration::from_millis(500);
/// Erasing a 2 MB flash takes up to about 20 s on an H7; some take longer.
pub const ERASE_TIMEOUT: Duration = Duration::from_secs(60);
/// The CRC is over the whole flash.
const CRC_TIMEOUT: Duration = Duration::from_secs(10);

/// A byte stream to a bootloader, with a read timeout the protocol sets per
/// step: a serial port, or a test's fake.
pub trait Port: Read + Write + Send {
    /// How long one read waits for a byte.
    ///
    /// # Errors
    /// When the port refuses the setting.
    fn set_timeout(&mut self, timeout: Duration) -> std::io::Result<()>;
    /// Drops what came in and was not read, such as `MAVLink` the firmware
    /// sent before it restarted.
    ///
    /// # Errors
    /// When the port cannot.
    fn discard_input(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(feature = "native")]
impl Port for Box<dyn serialport::SerialPort> {
    fn set_timeout(&mut self, timeout: Duration) -> std::io::Result<()> {
        serialport::SerialPort::set_timeout(self.as_mut(), timeout).map_err(std::io::Error::from)
    }
    fn discard_input(&mut self) -> std::io::Result<()> {
        self.clear(serialport::ClearBuffer::Input)
            .map_err(std::io::Error::from)
    }
}

/// What a bootloader says of its board (`GET_DEVICE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Board {
    /// The bootloader's protocol revision.
    pub bootloader: u32,
    /// The board's id, as firmware files and `ArduPilot`'s manifest name it
    /// (`APJ_BOARD_ID`): 140 for a Cube Orange.
    pub board_id: u32,
    pub board_rev: u32,
    /// Bytes of flash the firmware may use.
    pub flash_size: u32,
}

/// Which step a failure was in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Sync,
    Identify,
    Erase,
    Write,
    Verify,
    Reboot,
}

impl Step {
    #[must_use]
    pub const fn words(self) -> &'static str {
        match self {
            Self::Sync => "finding the bootloader",
            Self::Identify => "reading the board",
            Self::Erase => "erasing",
            Self::Write => "writing",
            Self::Verify => "verifying",
            Self::Reboot => "restarting",
        }
    }
}

/// Why an install stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Nothing answered in that step.
    Timeout(Step),
    /// The bootloader answered `FAILED` or `INVALID`.
    Refused(Step),
    /// Something other than the protocol came back.
    Garbled(Step),
    /// Too old (no `GET_CRC`) or newer than UXDuck knows.
    Unsupported { bootloader: u32 },
    /// The file is for another board.
    BoardMismatch { file: u32, board: u32 },
    /// The image does not fit the board's flash.
    TooLarge { image: u32, flash: u32 },
    /// What was written is not what the file holds.
    Crc { expected: u32, reported: u32 },
    /// Cancelled before erasing.
    Cancelled,
    /// The port failed.
    Io(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout(step) => {
                write!(f, "the bootloader stopped answering while {}", step.words())
            }
            Self::Refused(step) => write!(f, "the bootloader refused while {}", step.words()),
            Self::Garbled(step) => write!(
                f,
                "the bootloader's answer made no sense while {}",
                step.words()
            ),
            Self::Unsupported { bootloader } => {
                write!(
                    f,
                    "its bootloader (revision {bootloader}) is one UXDuck does not install through"
                )
            }
            Self::BoardMismatch { file, board } => {
                write!(f, "the file is for board {file}, and this board is {board}")
            }
            Self::TooLarge { image, flash } => {
                write!(
                    f,
                    "the firmware is {image} bytes, and the board has {flash}"
                )
            }
            Self::Crc { expected, reported } => write!(
                f,
                "what was written does not match the file (CRC {reported:08x}, expected {expected:08x})"
            ),
            Self::Cancelled => write!(f, "cancelled"),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for Error {}

/// A stage of the install, for progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Erasing,
    /// With bytes written of all.
    Writing,
    Verifying,
    Rebooting,
}

/// The bootloader's own CRC-32: the reflected polynomial `0xEDB88320`,
/// from `state` and without the usual inversions, so a sum can be carried
/// on over more bytes.
#[must_use]
pub fn crc32(mut state: u32, bytes: &[u8]) -> u32 {
    for byte in bytes {
        state ^= u32::from(*byte);
        for _ in 0..8 {
            state = if state & 1 == 1 {
                (state >> 1) ^ 0xEDB8_8320
            } else {
                state >> 1
            };
        }
    }
    state
}

/// The image as written: padded with `0xFF` to whole words.
#[must_use]
pub fn padded(image: &[u8]) -> Vec<u8> {
    let mut bytes = image.to_vec();
    while !bytes.len().is_multiple_of(4) {
        bytes.push(0xFF);
    }
    bytes
}

/// The CRC the bootloader reports once `image` is written to a flash of
/// `flash_size` bytes: the image, then erased bytes to the end.
#[must_use]
pub fn flash_crc(image: &[u8], flash_size: u32) -> u32 {
    let image = padded(image);
    let mut state = crc32(0, &image);
    let rest = (flash_size as usize).saturating_sub(image.len());
    let erased = [0xFFu8; 256];
    let mut left = rest;
    while left > 0 {
        let n = left.min(erased.len());
        state = crc32(state, &erased[..n]);
        left -= n;
    }
    state
}

/// Whether a file's image may be installed on a board: the same board id,
/// it fits, and a bootloader UXDuck speaks to.
///
/// # Errors
/// What stands in the way.
pub fn check(board: &Board, file_board_id: u32, image_len: usize) -> Result<(), Error> {
    if !(BL_REV_MIN..=BL_REV_MAX).contains(&board.bootloader) {
        return Err(Error::Unsupported {
            bootloader: board.bootloader,
        });
    }
    if file_board_id != board.board_id {
        return Err(Error::BoardMismatch {
            file: file_board_id,
            board: board.board_id,
        });
    }
    let image = u32::try_from(padded_len(image_len)).unwrap_or(u32::MAX);
    if image > board.flash_size {
        return Err(Error::TooLarge {
            image,
            flash: board.flash_size,
        });
    }
    Ok(())
}

const fn padded_len(len: usize) -> usize {
    len.div_ceil(4) * 4
}

/// A board's bootloader on a port.
pub struct Bootloader<P: Port> {
    port: P,
}

impl<P: Port> std::fmt::Debug for Bootloader<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bootloader").finish_non_exhaustive()
    }
}

impl<P: Port> Bootloader<P> {
    pub const fn new(port: P) -> Self {
        Self { port }
    }

    pub fn into_inner(self) -> P {
        self.port
    }

    fn send(&mut self, bytes: &[u8], step: Step) -> Result<(), Error> {
        self.port
            .write_all(bytes)
            .and_then(|()| self.port.flush())
            .map_err(|e| io(&e, step))
    }

    /// One byte, waiting until `deadline`.
    fn byte(&mut self, deadline: Instant, step: Step) -> Result<u8, Error> {
        let mut one = [0u8; 1];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::Timeout(step));
            }
            self.port
                .set_timeout(left.min(Duration::from_millis(250)))
                .map_err(|e| io(&e, step))?;
            match self.port.read(&mut one) {
                Ok(1) => return Ok(one[0]),
                Ok(_) => {}
                Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(io(&e, step)),
            }
        }
    }

    /// `INSYNC` and a status, which must be `OK`.
    fn status(&mut self, timeout: Duration, step: Step) -> Result<(), Error> {
        let deadline = Instant::now() + timeout;
        let insync = self.byte(deadline, step)?;
        if insync != INSYNC {
            return Err(Error::Garbled(step));
        }
        match self.byte(deadline, step)? {
            OK => Ok(()),
            FAILED | INVALID | BAD_SILICON_REV => Err(Error::Refused(step)),
            _ => Err(Error::Garbled(step)),
        }
    }

    fn word(&mut self, timeout: Duration, step: Step) -> Result<u32, Error> {
        let deadline = Instant::now() + timeout;
        let mut bytes = [0u8; 4];
        for byte in &mut bytes {
            *byte = self.byte(deadline, step)?;
        }
        Ok(u32::from_le_bytes(bytes))
    }

    /// Finds the bootloader's attention: anything waiting is dropped, then
    /// `GET_SYNC` until it answers, at most `tries` times.
    ///
    /// # Errors
    /// When it never answers in sync.
    pub fn sync(&mut self, tries: u32) -> Result<(), Error> {
        let mut last = Error::Timeout(Step::Sync);
        for _ in 0..tries.max(1) {
            let _ = self.port.discard_input();
            self.send(&[GET_SYNC, EOC], Step::Sync)?;
            match self.status(SHORT, Step::Sync) {
                Ok(()) => return Ok(()),
                Err(error @ Error::Io(_)) => return Err(error),
                Err(error) => last = error,
            }
        }
        // A garbled answer is something on the port that is not a
        // bootloader: say it did not answer.
        Err(match last {
            Error::Garbled(_) => Error::Timeout(Step::Sync),
            other => other,
        })
    }

    fn device(&mut self, field: u8) -> Result<u32, Error> {
        self.send(&[GET_DEVICE, field, EOC], Step::Identify)?;
        let value = self.word(SHORT, Step::Identify)?;
        self.status(SHORT, Step::Identify)?;
        Ok(value)
    }

    /// The board, from `GET_DEVICE`.
    ///
    /// # Errors
    /// When the bootloader does not say.
    pub fn identify(&mut self) -> Result<Board, Error> {
        Ok(Board {
            bootloader: self.device(INFO_BL_REV)?,
            board_id: self.device(INFO_BOARD_ID)?,
            board_rev: self.device(INFO_BOARD_REV)?,
            flash_size: self.device(INFO_FLASH_SIZE)?,
        })
    }

    /// Erases the firmware's flash; the bootloader answers when done.
    ///
    /// # Errors
    /// When it refuses or takes longer than `timeout`.
    pub fn erase(&mut self, timeout: Duration) -> Result<(), Error> {
        self.send(&[CHIP_ERASE, EOC], Step::Erase)?;
        self.status(timeout, Step::Erase)
    }

    /// Writes the image from the start of the flash, in chunks, saying how
    /// far it is after each.
    ///
    /// # Errors
    /// When a chunk is refused or not answered.
    pub fn program(
        &mut self,
        image: &[u8],
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<(), Error> {
        let image = padded(image);
        let total = image.len() as u64;
        let mut done = 0u64;
        for chunk in image.chunks(PROG_MULTI_MAX) {
            let mut command = Vec::with_capacity(chunk.len() + 3);
            command.push(PROG_MULTI);
            command.push(u8::try_from(chunk.len()).unwrap_or(u8::MAX));
            command.extend_from_slice(chunk);
            command.push(EOC);
            self.send(&command, Step::Write)?;
            self.status(SHORT, Step::Write)?;
            done += chunk.len() as u64;
            progress(done, total);
        }
        Ok(())
    }

    /// Asks for the CRC of the whole flash and compares it with the file's.
    ///
    /// # Errors
    /// When they differ, or it does not answer.
    pub fn verify(&mut self, image: &[u8], flash_size: u32) -> Result<(), Error> {
        self.send(&[GET_CRC, EOC], Step::Verify)?;
        let reported = self.word(CRC_TIMEOUT, Step::Verify)?;
        self.status(SHORT, Step::Verify)?;
        let expected = flash_crc(image, flash_size);
        if reported == expected {
            Ok(())
        } else {
            Err(Error::Crc { expected, reported })
        }
    }

    /// Starts the firmware. The board may restart before it answers, so
    /// no answer is not a failure.
    ///
    /// # Errors
    /// When the port fails.
    pub fn reboot(&mut self) -> Result<(), Error> {
        self.send(&[REBOOT, EOC], Step::Reboot)?;
        match self.status(SHORT, Step::Reboot) {
            Ok(()) | Err(Error::Timeout(_) | Error::Garbled(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

fn io(error: &std::io::Error, step: Step) -> Error {
    Error::Io(format!("the port failed while {}: {error}", step.words()))
}

/// Installs an image on an identified board: checks it, erases, writes,
/// verifies and restarts it, saying each stage as it starts and the bytes
/// written. `cancelled` is asked once, just before erasing: after that the
/// install runs to its end, since a half-erased board has no firmware.
///
/// # Errors
/// Why it stopped. A board refused before erasing is restarted, so its old
/// firmware runs again.
pub fn install<P: Port>(
    bootloader: &mut Bootloader<P>,
    board: &Board,
    file_board_id: u32,
    image: &[u8],
    progress: &mut dyn FnMut(Stage, u64, u64),
    cancelled: &dyn Fn() -> bool,
) -> Result<(), Error> {
    install_within(
        bootloader,
        board,
        (file_board_id, image),
        ERASE_TIMEOUT,
        progress,
        cancelled,
    )
}

/// [`install`], erasing within `erase_timeout`.
///
/// # Errors
/// As [`install`].
pub fn install_within<P: Port>(
    bootloader: &mut Bootloader<P>,
    board: &Board,
    (file_board_id, image): (u32, &[u8]),
    erase_timeout: Duration,
    progress: &mut dyn FnMut(Stage, u64, u64),
    cancelled: &dyn Fn() -> bool,
) -> Result<(), Error> {
    let before_erasing = check(board, file_board_id, image.len()).and_then(|()| {
        if cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    });
    if let Err(error) = before_erasing {
        let _ = bootloader.reboot();
        return Err(error);
    }
    progress(Stage::Erasing, 0, 0);
    bootloader.erase(erase_timeout)?;
    let total = padded_len(image.len()) as u64;
    progress(Stage::Writing, 0, total);
    bootloader.program(image, &mut |done, total| {
        progress(Stage::Writing, done, total);
    })?;
    progress(Stage::Verifying, 0, 0);
    bootloader.verify(image, board.flash_size)?;
    progress(Stage::Rebooting, 0, 0);
    bootloader.reboot()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "deterministic tests")]

    use super::*;

    #[test]
    fn the_crc_is_the_reflected_polynomial_without_inversions() {
        // Without the usual inversions: zero in, zero out over zeroes.
        assert_eq!(crc32(0, &[0, 0, 0, 0]), 0);
        // The standard CRC-32 of "123456789" is cbf43926: the same sum with
        // the state inverted on the way in and out.
        let standard = !crc32(0xFFFF_FFFF, b"123456789");
        assert_eq!(standard, 0xCBF4_3926);
        // Carried on in pieces, the same.
        assert_eq!(crc32(crc32(0, b"1234"), b"56789"), crc32(0, b"123456789"));
    }

    #[test]
    fn the_flash_crc_counts_erased_bytes_to_the_end() {
        let image = [1u8, 2, 3];
        let mut flash = vec![0xFFu8; 64];
        flash[..3].copy_from_slice(&image);
        assert_eq!(flash_crc(&image, 64), crc32(0, &flash));
        assert_eq!(padded(&image), vec![1, 2, 3, 0xFF]);
    }

    #[test]
    fn a_file_is_checked_against_the_board() {
        let board = Board {
            bootloader: 5,
            board_id: 140,
            board_rev: 0,
            flash_size: 1024,
        };
        assert_eq!(check(&board, 140, 1024), Ok(()));
        assert_eq!(
            check(&board, 9, 10),
            Err(Error::BoardMismatch {
                file: 9,
                board: 140
            })
        );
        assert_eq!(
            check(&board, 140, 1025),
            Err(Error::TooLarge {
                image: 1028,
                flash: 1024
            })
        );
        let old = Board {
            bootloader: 2,
            ..board
        };
        assert_eq!(
            check(&old, 140, 10),
            Err(Error::Unsupported { bootloader: 2 })
        );
    }
}
