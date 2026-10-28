//! `uxduck dev fake-board` (S189): a fake vehicle on a serial port of its
//! own, a pseudo-terminal, that restarts into a fake bootloader when asked
//! (`MAV_CMD_PREFLIGHT_REBOOT_SHUTDOWN` 3) and back into its firmware once
//! it is told to, reporting the version written. Add a serial link to the
//! path it prints, then install firmware on it as on a real board.

#[cfg(unix)]
use std::io::{Read, Write};
use std::path::PathBuf;
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use uxduck_mavlink::fake_bootloader::{BoardMode, FakeBoard, fake_firmware_image};

use super::client::CliError;

/// How the fake board starts.
#[derive(Debug)]
#[cfg_attr(
    not(unix),
    allow(dead_code, reason = "the fake board runs on a pseudo-terminal")
)]
pub struct BoardOptions {
    pub system_id: u8,
    pub uid: u64,
    pub flight_sw_version: u32,
    pub board_id: u32,
    pub flash_kb: u32,
    /// Starts in its bootloader, as a board with no firmware.
    pub in_bootloader: bool,
    /// Writes a fake firmware file for this board first: its path, the
    /// version in it and its size in KB.
    pub write_apj: Option<(PathBuf, String, u32)>,
    /// Takes this long for each chunk written, and a hundred times it to
    /// erase, as a real board does, so each stage can be seen.
    pub pace_ms: u64,
}

/// A pseudo-terminal is a Unix device, so the fake board is for macOS and
/// Linux; Windows says so rather than not building.
#[cfg(not(unix))]
#[allow(
    clippy::needless_pass_by_value,
    reason = "the same signature as on Unix"
)]
pub fn run(_options: BoardOptions) -> Result<(), CliError> {
    Err(CliError::Usage(
        "uxduck dev fake-board needs a pseudo-terminal, so it runs on macOS and Linux".into(),
    ))
}

#[cfg(unix)]
#[allow(
    clippy::needless_pass_by_value,
    reason = "it runs until stopped, owning them"
)]
pub fn run(options: BoardOptions) -> Result<(), CliError> {
    use serialport::SerialPort as _;
    if let Some((path, version, size_kb)) = &options.write_apj {
        let image = fake_firmware_image(version, usize::try_from(*size_kb).unwrap_or(64) * 1024);
        let file = uxduck_mavlink::firmware::write_file(options.board_id, &image, "arducopter");
        std::fs::write(path, file)
            .map_err(|e| CliError::Usage(format!("{}: {e}", path.display())))?;
        eprintln!(
            "wrote {} for board {} with version {version}",
            path.display(),
            options.board_id
        );
    }
    let (mut master, slave) = serialport::TTYPort::pair()
        .map_err(|e| CliError::Usage(format!("cannot make a serial port: {e}")))?;
    let path = slave
        .name()
        .ok_or_else(|| CliError::Usage("the serial port has no name".into()))?;
    // The other end stays open here, so the port lives while UXDuck closes
    // and opens it.
    let _slave = slave;
    master
        .set_timeout(Duration::from_millis(20))
        .map_err(|e| CliError::Usage(e.to_string()))?;
    let mut vehicle = uxduck_mavlink::sim::FakeVehicle::new(options.system_id, options.uid);
    vehicle.flight_sw_version = options.flight_sw_version;
    let mut board = FakeBoard::new(
        vehicle,
        options.board_id,
        usize::try_from(options.flash_kb).unwrap_or(0) * 1024,
    );
    if options.in_bootloader {
        board.mode = BoardMode::Bootloader;
    }
    println!("{path}");
    eprintln!(
        "fake board {} on {path}, in its {}; Ctrl-C to stop",
        options.board_id,
        if options.in_bootloader {
            "bootloader"
        } else {
            "firmware"
        }
    );
    let started = Instant::now();
    let mut beat = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    let mut tick = Instant::now();
    let mut buffer = [0u8; 4096];
    let mut mode = board.mode;
    loop {
        let mut out: Vec<Vec<u8>> = Vec::new();
        if board.mode == BoardMode::Firmware {
            if beat.elapsed() >= Duration::from_secs(1) {
                beat = Instant::now();
                out.push(board.vehicle.heartbeat());
            }
            if tick.elapsed() >= Duration::from_millis(200) {
                tick = Instant::now();
                out.extend(board.vehicle.telemetry(started.elapsed().as_secs_f64()));
            }
        }
        for frame in out {
            // Nobody reading: dropped, as a USB port with nothing open.
            let _ = master.write_all(&frame);
        }
        match master.read(&mut buffer) {
            Ok(n) if n > 0 => {
                if options.pace_ms > 0 && board.mode == BoardMode::Bootloader {
                    let chunks =
                        u64::from(buffer[..n].contains(&uxduck_mavlink::bootloader::PROG_MULTI));
                    let erase =
                        buffer[..n].first() == Some(&uxduck_mavlink::bootloader::CHIP_ERASE);
                    let wait = if erase {
                        options.pace_ms * 100
                    } else {
                        options.pace_ms * chunks
                    };
                    std::thread::sleep(Duration::from_millis(wait));
                }
                let answer = board.feed(&buffer[..n]);
                if !answer.is_empty() {
                    let _ = master.write_all(&answer);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {}
            // Nothing, or macOS's EIO while nothing has the other end open.
            Ok(_) | Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
        if board.mode != mode {
            mode = board.mode;
            eprintln!(
                "{}",
                match mode {
                    BoardMode::Bootloader => "restarted into its bootloader",
                    BoardMode::Firmware => "restarted into its firmware",
                }
            );
        }
    }
}
