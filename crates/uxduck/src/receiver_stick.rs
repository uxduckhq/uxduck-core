//! A USB receiver stick on this computer (S173, ADR-0034): a 1090 MHz
//! RTL-SDR stick, read by a decoder UXDuck starts, readsb or dump1090-fa,
//! with only its SBS-1 output on, on a free local port. The link reads that
//! port as it reads a receiver on the network (S172). UXDuck does not ship
//! the decoder (readsb is GPL, ADR-0028); it finds the one installed, and
//! says how to install one when there is none.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};

/// Said when no decoder is installed.
pub const NO_DECODER: &str = "UXDuck needs readsb to read a USB receiver stick. On a Mac: brew install readsb. On Linux: install readsb or dump1090-fa from your distribution or FlightAware. Then turn the receiver off and on.";

/// The decoders UXDuck knows how to start, by file name.
const DECODERS: &[&str] = &["readsb", "dump1090-fa", "dump1090"];

/// Where a decoder may be beyond `PATH`: Homebrew's, on either Mac.
const ALSO: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"];

/// A running decoder; stopped when dropped.
#[derive(Debug)]
pub struct Decoder {
    child: Child,
    /// Its SBS-1 port on 127.0.0.1.
    pub port: u16,
    log: PathBuf,
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The decoder to start: `UXDUCK_ADSB_DECODER`, else the first known one on
/// `PATH` or in Homebrew's folders.
#[must_use]
pub fn find_decoder() -> Option<PathBuf> {
    if let Some(chosen) = std::env::var_os("UXDUCK_ADSB_DECODER") {
        return Some(PathBuf::from(chosen));
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<PathBuf> = std::env::split_paths(&path)
        .chain(ALSO.iter().map(PathBuf::from))
        .collect();
    DECODERS.iter().find_map(|name| {
        dirs.iter()
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

/// Starts the decoder on the first stick, its SBS-1 output on a free port
/// and every other output off, its own output in `log`.
///
/// # Errors
/// When there is no decoder, no free port, or it cannot be started.
pub fn launch(log: &Path) -> Result<Decoder, String> {
    let decoder = find_decoder().ok_or_else(|| NO_DECODER.to_owned())?;
    let port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .map_err(|e| format!("no free port for the receiver stick: {e}"))?;
    let output = std::fs::File::create(log).map_err(|e| format!("{}: {e}", log.display()))?;
    let errors = output.try_clone().map_err(|e| e.to_string())?;
    let child = Command::new(&decoder)
        .args(arguments(&decoder, port))
        .stdin(Stdio::null())
        .stdout(output)
        .stderr(errors)
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", decoder.display()))?;
    tracing::info!(decoder = %decoder.display(), port, "receiver stick decoder started");
    Ok(Decoder {
        child,
        port,
        log: log.to_owned(),
    })
}

/// The decoder's arguments: the stick, SBS-1 on `port`, nothing else served.
fn arguments(decoder: &Path, port: u16) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if decoder.file_name().is_some_and(|n| n == "readsb") {
        args.extend(["--device-type", "rtlsdr"].map(str::to_owned));
    }
    args.extend(
        [
            "--net",
            "--net-ri-port",
            "0",
            "--net-ro-port",
            "0",
            "--net-bi-port",
            "0",
            "--net-bo-port",
            "0",
            "--quiet",
            "--net-sbs-port",
        ]
        .map(str::to_owned),
    );
    args.push(port.to_string());
    args
}

impl Decoder {
    /// How it ended, if it has, with its last line of output, or a word on
    /// the stick when it said nothing.
    pub fn exited(&mut self) -> Option<String> {
        let status = self.child.try_wait().ok().flatten()?;
        Some(stopped_detail(status, &self.log))
    }
}

fn stopped_detail(status: ExitStatus, log: &Path) -> String {
    let last = std::fs::read_to_string(log).ok().and_then(|text| {
        text.lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(str::to_owned)
    });
    match (last, stick_plugged_in()) {
        (_, Some(false)) => "No receiver stick is plugged in".to_owned(),
        (Some(line), _) => format!("The decoder stopped: {}", line.trim()),
        (None, _) => format!("The decoder stopped ({status})"),
    }
}

/// RTL-SDR sticks by USB vendor and product: Realtek's RTL2832U and
/// RTL2838 reference designs, which most sticks are.
const STICKS: &[(u16, u16)] = &[(0x0BDA, 0x2832), (0x0BDA, 0x2838)];

/// Whether a stick is plugged in, when this system can say: from sysfs on
/// Linux, `ioreg` on macOS; `None` elsewhere.
#[must_use]
pub fn stick_plugged_in() -> Option<bool> {
    if cfg!(target_os = "linux") {
        let devices = std::fs::read_dir("/sys/bus/usb/devices").ok()?;
        let hex = |p: PathBuf| {
            std::fs::read_to_string(p)
                .ok()
                .and_then(|t| u16::from_str_radix(t.trim(), 16).ok())
        };
        Some(devices.flatten().any(|d| {
            let (vendor, product) = (
                hex(d.path().join("idVendor")),
                hex(d.path().join("idProduct")),
            );
            matches!((vendor, product), (Some(v), Some(p)) if STICKS.contains(&(v, p)))
        }))
    } else if cfg!(target_os = "macos") {
        let listed = Command::new("/usr/sbin/ioreg")
            .args(["-p", "IOUSB", "-l", "-w0"])
            .output()
            .ok()?;
        Some(sticks_in_ioreg(&String::from_utf8_lossy(&listed.stdout)))
    } else {
        None
    }
}

/// Whether `ioreg`'s listing holds a stick: a device's `idVendor` and
/// `idProduct`, in decimal, one after the other.
fn sticks_in_ioreg(listing: &str) -> bool {
    let mut vendor = None;
    for line in listing.lines() {
        let number = |key: &str| {
            line.split_once(&format!("\"{key}\" = "))
                .and_then(|(_, n)| n.trim().parse::<u16>().ok())
        };
        if let Some(v) = number("idVendor") {
            vendor = Some(v);
        }
        if let (Some(p), Some(v)) = (number("idProduct"), vendor)
            && STICKS.contains(&(v, p))
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readsb_gets_the_stick_and_only_sbs() {
        let args = arguments(Path::new("/opt/homebrew/bin/readsb"), 30_103).join(" ");
        assert!(args.starts_with("--device-type rtlsdr --net "));
        assert!(args.ends_with("--net-sbs-port 30103"));
        assert!(args.contains("--net-bo-port 0"));
        let dump1090 = arguments(Path::new("/usr/bin/dump1090-fa"), 30_103).join(" ");
        assert!(!dump1090.contains("--device-type"));
    }

    #[test]
    fn a_stick_is_found_in_ioreg_by_its_ids() {
        let with =
            "  |   \"idProduct\" = 10296\n  |   \"idVendor\" = 3034\n  |   \"idProduct\" = 10296\n";
        assert!(sticks_in_ioreg(with));
        let without = "  |   \"idVendor\" = 1452\n  |   \"idProduct\" = 10296\n";
        assert!(!sticks_in_ioreg(without));
    }

    #[cfg(unix)]
    #[test]
    fn a_decoder_that_stops_says_why() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("readsb");
        std::fs::write(
            &script,
            "#!/bin/sh\necho 'No supported RTLSDR devices found.' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let log = dir.path().join("decoder.log");
        let mut child = Command::new(&script)
            .stdout(std::fs::File::create(&log).unwrap())
            .stderr(std::fs::File::options().append(true).open(&log).unwrap())
            .spawn()
            .unwrap();
        let status = child.wait().unwrap();
        let detail = stopped_detail(status, &log);
        assert!(
            detail == "The decoder stopped: No supported RTLSDR devices found."
                || detail == "No receiver stick is plugged in",
            "{detail}"
        );
    }
}
