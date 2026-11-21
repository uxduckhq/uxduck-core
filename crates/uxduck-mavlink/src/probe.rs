//! Looking at a serial device for a vehicle (ADR-0018 F5, F6, S44): listen
//! before writing, at the speeds radios use, accept only frames whose
//! checksum holds, write a ground-station heartbeat only to a known flight
//! controller that stays silent, and close the device when done. UXDuck
//! never sets DTR itself, but opening a serial port raises DTR and RTS on
//! Linux and macOS for any program, and closing it lowers them when the
//! port hangs up on close, so a board that resets on DTR can reset when it
//! is looked at.

use std::io::Write;
use std::time::{Duration, Instant};

use crate::decode::{Message, decode};
use crate::framing::split;

/// A vehicle's heartbeat, heard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Heard {
    pub system_id: u8,
    pub component_id: u8,
    pub vehicle_type: String,
    pub autopilot: String,
    pub armed: bool,
    /// From `AUTOPILOT_VERSION`, when it answered: its UID, for the
    /// vehicle UXDuck knows it as.
    pub uid: Option<(u64, [u8; 18])>,
}

/// The first vehicle heartbeat in some bytes: not a ground station, and a
/// whole frame whose checksum holds.
#[must_use]
pub fn heartbeat_in(bytes: &[u8]) -> Option<Heard> {
    split(bytes).frames.into_iter().find_map(|frame| {
        let decoded = decode(frame)?;
        match decoded.message {
            Message::Heartbeat {
                mav_type,
                autopilot,
                base_mode,
                ..
            } if crate::autopilot::flight_controller(
                &mav_type,
                &autopilot,
                decoded.component_id,
            ) =>
            {
                Some(Heard {
                    system_id: decoded.system_id,
                    component_id: decoded.component_id,
                    vehicle_type: mav_type,
                    autopilot,
                    armed: base_mode & 0x80 != 0,
                    uid: None,
                })
            }
            _ => None,
        }
    })
}

/// A UID in some bytes, from `AUTOPILOT_VERSION`.
#[must_use]
pub fn uid_in(bytes: &[u8]) -> Option<(u64, [u8; 18])> {
    split(bytes)
        .frames
        .into_iter()
        .find_map(|frame| match decode(frame)?.message {
            Message::AutopilotVersion { uid, uid2, .. } => Some((uid, uid2)),
            _ => None,
        })
}

/// How to look at a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    /// A flight controller on USB: the speed does not matter; one try, and
    /// one ground-station heartbeat if it stays silent.
    KnownBoard,
    /// A USB-serial adapter: maybe a radio, at one of the usual speeds.
    Adapter,
}

/// The speeds radios use, most common first.
pub const ADAPTER_BAUDS: [u32; 4] = [57_600, 115_200, 921_600, 460_800];

/// What looking found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Heard {
        baud: u32,
        heard: Heard,
    },
    /// Opened at every speed; no valid MAVLink.
    NoMavlink,
    /// A known board that did not answer its heartbeat.
    SilentKnownBoard,
    /// Another program has it.
    Busy,
    /// Not allowed: why, and what to do, as a link that won't open says it.
    Permission(String),
    /// Anything else, as the system said it.
    Failed(String),
}

/// How long each listen lasts.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    pub listen: Duration,
    /// A known board's first silence before it gets a heartbeat.
    pub known_silence: Duration,
    pub after_heartbeat: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            listen: Duration::from_millis(1500),
            known_silence: Duration::from_secs(2),
            after_heartbeat: Duration::from_secs(1),
        }
    }
}

fn open(path: &str, baud: u32) -> Result<Box<dyn serialport::SerialPort>, Outcome> {
    let try_open = |baud| {
        serialport::new(path, baud)
            .timeout(Duration::from_millis(100))
            .preserve_dtr_on_open()
            .open()
    };
    let result = match try_open(baud) {
        // A pseudo-terminal has no line speed (macOS: "Not a typewriter").
        Err(error) if error.description.contains("typewriter") => try_open(0),
        other => other,
    };
    result.map_err(|error| {
        let text = error.description.to_lowercase();
        if text.contains("busy") || text.contains("in use") || text.contains("exclusive") {
            Outcome::Busy
        } else if text.contains("permission") || text.contains("denied") {
            if cfg!(windows) && text.contains("denied") {
                Outcome::Busy
            } else {
                Outcome::Permission(crate::serial_access::denied(path))
            }
        } else {
            Outcome::Failed(error.description)
        }
    })
}

/// Reads for `long`, until a heartbeat arrives.
fn listen(port: &mut dyn serialport::SerialPort, long: Duration) -> (Option<Heard>, Vec<u8>) {
    let until = Instant::now() + long;
    let mut heard = Vec::new();
    let mut chunk = [0u8; 1024];
    while Instant::now() < until {
        match port.read(&mut chunk) {
            Ok(0) => std::thread::sleep(Duration::from_millis(10)),
            Ok(n) => {
                heard.extend_from_slice(&chunk[..n]);
                if let Some(found) = heartbeat_in(&heard) {
                    return (Some(found), heard);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
    (None, heard)
}

/// Asks a vehicle that has spoken for its UID, briefly.
fn ask_uid(
    port: &mut dyn serialport::SerialPort,
    heard: &Heard,
    timing: &Timing,
    gcs: u8,
) -> Option<(u64, [u8; 18])> {
    let ask =
        crate::sim::request_autopilot_version_frame(gcs, heard.system_id, heard.component_id, 1);
    port.write_all(&ask).ok()?;
    let until = Instant::now() + timing.after_heartbeat;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 1024];
    while Instant::now() < until {
        if let Ok(n) = port.read(&mut chunk) {
            bytes.extend_from_slice(&chunk[..n]);
            if let Some(uid) = uid_in(&bytes) {
                return Some(uid);
            }
        }
    }
    None
}

/// Looks at one device as its plan says, and closes it, speaking as ground
/// station `gcs` when it has to speak.
#[must_use]
pub fn probe(path: &str, plan: Plan, timing: &Timing, gcs: u8) -> Outcome {
    let bauds: &[u32] = match plan {
        Plan::KnownBoard => &[115_200],
        Plan::Adapter => &ADAPTER_BAUDS,
    };
    for &baud in bauds {
        let mut port = match open(path, baud) {
            Ok(port) => port,
            Err(outcome) => return outcome,
        };
        let first = if plan == Plan::KnownBoard {
            timing.known_silence
        } else {
            timing.listen
        };
        let (mut heard, _) = listen(port.as_mut(), first);
        if heard.is_none() && plan == Plan::KnownBoard {
            // Some autopilots wait for a ground station to speak first.
            let hello = crate::sim::gcs_heartbeat_frame(gcs, crate::GCS_COMPONENT_ID, 0);
            if port.write_all(&hello).is_ok() {
                heard = listen(port.as_mut(), timing.after_heartbeat).0;
            }
            if heard.is_none() {
                return Outcome::SilentKnownBoard;
            }
        }
        if let Some(mut heard) = heard {
            heard.uid = ask_uid(port.as_mut(), &heard, timing, gcs);
            return Outcome::Heard { baud, heard };
        }
    }
    Outcome::NoMavlink
}

/// The rates baud detection tries, most used first: every rate the add-link
/// form's Baud rate list offers, so what it finds can be chosen there. A
/// rate outside the list is one only the person knows, so it is never
/// guessed.
pub const DETECT_BAUDS: [u32; 7] = [57_600, 115_200, 460_800, 921_600, 230_400, 9_600, 4_800];

/// How long detection listens at each rate. MAVLink asks every component to
/// send a heartbeat at least once a second, so this holds two whole ones
/// even from a vehicle that streams nothing else.
pub const DETECT_LISTEN: Duration = Duration::from_millis(2500);

/// Checksum-verified frames that name a rate. At a wrong rate a stream
/// arrives as noise, and noise passes a frame's 16-bit checksum by chance
/// about once in 65536 tries, so one such frame in a listen can happen by
/// chance and two do not.
pub const DETECT_FRAMES: usize = 2;

/// What baud detection heard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Detected {
    /// Whole MAVLink frames whose checksums hold, at this rate.
    Found { baud: u32 },
    /// It listened at each of these rates and heard no MAVLink.
    NotFound { tried: Vec<u32> },
    /// It was asked to stop, and let the device go.
    Stopped,
    /// It could not open the device, in the words a probe's outcome uses.
    Unavailable(Outcome),
    /// The device stopped answering once open: reading it failed, as when
    /// it is unplugged, or it could not be moved to any rate.
    Gone,
}

/// A device detection listens to: it can be read and moved to another
/// rate, and has no way to be written to.
pub trait Retune: std::io::Read {
    /// Reads at `baud` from now on, dropping what arrived before.
    ///
    /// # Errors
    /// When the device cannot run at that rate.
    fn retune(&mut self, baud: u32) -> std::io::Result<()>;
}

/// Listens to an open device at each rate in turn for `listen`, saying
/// which through `listening`, and names the first rate where
/// [`DETECT_FRAMES`] whole frames arrive whose checksums hold, `CRC_EXTRA`
/// included. A frame of a message no dialect describes cannot be checked,
/// so it does not count. A rate the device cannot run at is skipped, and
/// only the rates listened at are named when none is found. It stops as
/// soon as `stop` is set.
pub fn detect_baud_with(
    device: &mut impl Retune,
    bauds: &[u32],
    listen: Duration,
    stop: &std::sync::atomic::AtomicBool,
    mut listening: impl FnMut(u32),
) -> Detected {
    use crate::framing::{Check, Reassembler};
    use std::io::ErrorKind;
    use std::sync::atomic::Ordering;
    let mut tried = Vec::new();
    for &baud in bauds {
        if stop.load(Ordering::Relaxed) {
            return Detected::Stopped;
        }
        if device.retune(baud).is_err() {
            continue;
        }
        listening(baud);
        tried.push(baud);
        let mut frames = Reassembler::default();
        let mut valid = 0;
        let until = Instant::now() + listen;
        let mut chunk = [0u8; 1024];
        while Instant::now() < until && valid < DETECT_FRAMES {
            if stop.load(Ordering::Relaxed) {
                return Detected::Stopped;
            }
            match device.read(&mut chunk) {
                Ok(0) => std::thread::sleep(Duration::from_millis(10)),
                Ok(n) => {
                    valid += frames
                        .push(&chunk[..n], |frame| match crate::decode::check(frame) {
                            Check::Valid => Check::Valid,
                            Check::Invalid | Check::Unknown => Check::Invalid,
                        })
                        .len();
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::TimedOut | ErrorKind::Interrupted | ErrorKind::WouldBlock
                    ) => {}
                Err(_) => return Detected::Gone,
            }
        }
        if valid >= DETECT_FRAMES {
            return Detected::Found { baud };
        }
    }
    if tried.is_empty() {
        return Detected::Gone;
    }
    Detected::NotFound { tried }
}

/// How long detection waits for a device another look holds: a probe keeps
/// one for at most a listen at each adapter speed and a question after.
fn detect_wait() -> Duration {
    let timing = Timing::default();
    timing.listen * u32::try_from(ADAPTER_BAUDS.len()).unwrap_or(u32::MAX) + timing.after_heartbeat
}

/// A serial device opened once for every rate detection tries. Fields drop
/// in order: the port closes first, then its settings are put back as they
/// were found and the last handle closes.
struct Listened {
    port: Box<dyn serialport::SerialPort>,
    #[cfg(unix)]
    _settings: Option<PutBack>,
}

impl std::io::Read for Listened {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.port.read(buf)
    }
}

impl Retune for Listened {
    fn retune(&mut self, baud: u32) -> std::io::Result<()> {
        match self.port.set_baud_rate(baud) {
            // A pseudo-terminal has no line speed, so every rate is the same.
            Err(error) if error.description.contains("typewriter") => {}
            other => other?,
        }
        self.port.clear(serialport::ClearBuffer::Input)?;
        Ok(())
    }
}

/// A device's terminal settings as detection found them, through a handle
/// of its own opened before the port. Dropped as it is, it changes nothing:
/// only [`PutBack`] writes them back.
#[cfg(unix)]
struct Settings {
    handle: std::fs::File,
    found: nix::sys::termios::Termios,
}

#[cfg(unix)]
impl Settings {
    fn keep(path: &str) -> Option<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        // Without waiting for a carrier, as the port itself opens.
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(nix::libc::O_NOCTTY | nix::libc::O_NONBLOCK)
            .open(path)
            .ok()?;
        let found = nix::sys::termios::tcgetattr(&handle).ok()?;
        Some(Self { handle, found })
    }
}

/// Settings put back when dropped, made only once detection's own open
/// succeeded. Had the open failed, another program may hold the port, and
/// writing the settings found before would undo its own.
#[cfg(unix)]
struct PutBack(Settings);

#[cfg(unix)]
impl Drop for PutBack {
    fn drop(&mut self) {
        let _ = nix::sys::termios::tcsetattr(
            &self.0.handle,
            nix::sys::termios::SetArg::TCSANOW,
            &self.0.found,
        );
    }
}

/// Detects a serial device's rate by listening only, at each of `bauds`
/// for `listen` (the daemon's are [`DETECT_BAUDS`] and [`DETECT_LISTEN`]).
/// The device is opened once for every rate and closed once, with its
/// settings put back as they were found. Opening a serial port raises DTR
/// and RTS on Linux and macOS, for any program, and the last close lowers
/// them again when the port hangs up on close (HUPCL, the usual setting),
/// so a board that resets on DTR resets once. On Windows DTR is left as the
/// driver has it. A device a search for vehicles is looking at is waited
/// for, and `stop` ends it early.
#[must_use]
pub fn detect_baud(
    path: &str,
    bauds: &[u32],
    listen: Duration,
    stop: &std::sync::atomic::AtomicBool,
    listening: impl FnMut(u32),
) -> Detected {
    use std::sync::atomic::Ordering;
    let Some(&first) = bauds.first() else {
        return Detected::NotFound { tried: Vec::new() };
    };
    let until = Instant::now() + detect_wait();
    let mut device = loop {
        #[cfg(unix)]
        let found = Settings::keep(path);
        match open(path, first) {
            Ok(port) => {
                break Listened {
                    port,
                    #[cfg(unix)]
                    _settings: found.map(PutBack),
                };
            }
            Err(_) if stop.load(Ordering::Relaxed) => return Detected::Stopped,
            Err(Outcome::Busy) if Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(outcome) => return Detected::Unavailable(outcome),
        }
    };
    detect_baud_with(&mut device, bauds, listen, stop, listening)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::sim::{FakeVehicle, gcs_heartbeat_frame};

    #[test]
    fn a_vehicle_heartbeat_is_heard_and_a_ground_stations_is_not() {
        let mut fake = FakeVehicle::new(3, 0xBEEF);
        let heard = heartbeat_in(&fake.heartbeat()).unwrap();
        assert_eq!(
            (heard.system_id, heard.vehicle_type.as_str()),
            (3, "Quadrotor")
        );
        assert_eq!(heartbeat_in(&gcs_heartbeat_frame(255, 190, 0)), None);
        assert_eq!(
            heartbeat_in(b"$GPGGA,123519,4807.038,N,01131.000,E*47\r\n"),
            None
        );
    }

    const QUICK: Duration = Duration::from_millis(20);

    /// What a UART reading at `heard_at` makes of `bytes` sent at
    /// `sent_at`: 8N1 with the line high at rest, a start bit found on its
    /// falling edge at sixteen times the rate, and each bit read at its
    /// middle, as a USB-serial adapter does.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "a simulated wire"
    )]
    fn on_the_wire(bytes: &[u8], sent_at: u32, heard_at: u32) -> Vec<u8> {
        let bits: Vec<bool> = [true, true]
            .into_iter()
            .chain(bytes.iter().flat_map(|&byte| {
                std::iter::once(false)
                    .chain((0..8).map(move |i| byte >> i & 1 == 1))
                    .chain(std::iter::once(true))
            }))
            .collect();
        let sent = f64::from(sent_at);
        let level = |t: f64| bits.get((t * sent) as usize).copied().unwrap_or(true);
        let end = bits.len() as f64 / sent;
        let bit = 1.0 / f64::from(heard_at);
        let mut heard = Vec::new();
        let mut t = 0.0;
        while t < end {
            while t < end && !level(t) {
                t += bit / 16.0;
            }
            while t < end && level(t) {
                t += bit / 16.0;
            }
            if t >= end {
                break;
            }
            if level(t + bit / 2.0) {
                continue;
            }
            let byte = (0..8)
                .filter(|&i| level(t + bit * (1.5 + f64::from(i))))
                .fold(0u8, |byte, i| byte | 1 << i);
            heard.push(byte);
            t += bit * 9.5;
        }
        heard
    }

    /// Bytes that look like nothing, the same every run.
    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                (state >> 56) as u8
            })
            .collect()
    }

    fn heartbeats(count: usize) -> Vec<u8> {
        let mut fake = FakeVehicle::new(1, 0xFEED);
        (0..count).flat_map(|_| fake.heartbeat()).collect()
    }

    /// A device sending `stream` at `sent_at`, read at whatever rate it is
    /// tuned to, and the rates it was tuned to.
    struct Wire {
        stream: Vec<u8>,
        sent_at: u32,
        heard: std::io::Cursor<Vec<u8>>,
        tuned: Vec<u32>,
        cannot: Vec<u32>,
        unplugged: bool,
    }

    impl Wire {
        fn new(stream: &[u8], sent_at: u32) -> Self {
            Self {
                stream: stream.to_vec(),
                sent_at,
                heard: std::io::Cursor::new(Vec::new()),
                tuned: Vec::new(),
                cannot: Vec::new(),
                unplugged: false,
            }
        }
    }

    impl std::io::Read for Wire {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.unplugged {
                return Err(std::io::Error::other("Device not configured"));
            }
            self.heard.read(buf)
        }
    }

    impl Retune for Wire {
        fn retune(&mut self, baud: u32) -> std::io::Result<()> {
            if self.cannot.contains(&baud) {
                return Err(std::io::Error::other("not a rate this adapter runs at"));
            }
            self.tuned.push(baud);
            self.heard = std::io::Cursor::new(on_the_wire(&self.stream, self.sent_at, baud));
            Ok(())
        }
    }

    /// Detects over a device that sends `stream` at `sent_at`, with the
    /// rates it was tuned to.
    fn detect_on(stream: &[u8], sent_at: u32) -> (Detected, Vec<u32>) {
        let mut wire = Wire::new(stream, sent_at);
        let detected = detect_baud_with(
            &mut wire,
            &DETECT_BAUDS,
            QUICK,
            &AtomicBool::new(false),
            |_| {},
        );
        (detected, wire.tuned)
    }

    /// Nothing heard at any of the daemon's rates.
    fn not_found() -> Detected {
        Detected::NotFound {
            tried: DETECT_BAUDS.to_vec(),
        }
    }

    #[test]
    fn a_uart_at_the_senders_rate_reads_what_was_sent() {
        let stream = [heartbeats(2), noise(64, 1)].concat();
        for baud in DETECT_BAUDS {
            assert_eq!(on_the_wire(&stream, baud, baud), stream, "{baud}");
            assert_ne!(on_the_wire(&stream, baud, baud * 2), stream, "{baud}");
        }
    }

    #[test]
    fn each_rate_is_found_from_a_vehicle_sending_at_it_after_garbage_at_the_others() {
        let stream = heartbeats(5);
        for sent_at in DETECT_BAUDS {
            let (detected, tuned) = detect_on(&stream, sent_at);
            assert_eq!(detected, Detected::Found { baud: sent_at });
            let tried = DETECT_BAUDS.iter().position(|&b| b == sent_at).unwrap() + 1;
            assert_eq!(tuned, DETECT_BAUDS[..tried], "stops at the rate it found");
        }
    }

    #[test]
    fn noise_between_frames_still_finds_the_rate() {
        let mut stream = noise(300, 2);
        let mut fake = FakeVehicle::new(2, 0xD00D);
        for seed in 3..8 {
            stream.extend(fake.heartbeat());
            stream.extend([0xFE, 0x09, 0xFD]);
            stream.extend(noise(97, seed));
        }
        assert_eq!(
            detect_on(&stream, 115_200).0,
            Detected::Found { baud: 115_200 }
        );
    }

    #[test]
    fn noise_text_and_silence_find_nothing_at_any_rate() {
        let text =
            b"$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*47\r\n".repeat(40);
        for stream in [noise(20_000, 9), text, Vec::new()] {
            let (detected, tuned) = detect_on(&stream, 57_600);
            assert_eq!(
                detected,
                Detected::NotFound {
                    tried: DETECT_BAUDS.to_vec()
                }
            );
            assert_eq!(tuned, DETECT_BAUDS, "every rate was tried");
        }
    }

    #[test]
    fn one_frame_or_frames_that_fail_their_checksum_are_not_enough() {
        let one = [noise(500, 10), heartbeats(1), noise(500, 11)].concat();
        assert_eq!(detect_on(&one, 57_600).0, not_found());

        let mut fake = FakeVehicle::new(1, 0xFEED);
        let broken: Vec<u8> = (0..5)
            .flat_map(|_| {
                let mut frame = fake.heartbeat();
                *frame.last_mut().unwrap() ^= 0x5A;
                frame
            })
            .collect();
        assert_eq!(detect_on(&broken, 57_600).0, not_found());
    }

    #[test]
    fn frames_of_a_message_no_dialect_knows_cannot_be_checked_so_do_not_count() {
        let unknown: Vec<u8> = (0..5)
            .flat_map(|_| crate::custom::frame(50_000, &[1, 2, 3, 4], 0x42))
            .collect();
        assert_eq!(detect_on(&unknown, 57_600).0, not_found());
    }

    #[test]
    fn it_says_each_rate_as_it_listens_and_skips_a_rate_the_device_cannot_run_at() {
        let mut wire = Wire::new(&heartbeats(5), 460_800);
        wire.cannot = vec![115_200];
        let mut said = Vec::new();
        let detected = detect_baud_with(
            &mut wire,
            &DETECT_BAUDS,
            QUICK,
            &AtomicBool::new(false),
            |baud| said.push(baud),
        );
        assert_eq!(detected, Detected::Found { baud: 460_800 });
        assert_eq!(said, [57_600, 460_800], "115200 was never listened at");
        assert_eq!(wire.tuned, said, "one device, tuned from rate to rate");
    }

    #[test]
    fn nothing_heard_names_only_the_rates_it_listened_at() {
        let mut wire = Wire::new(&[], 57_600);
        wire.cannot = vec![115_200, 921_600];
        let detected = detect_baud_with(
            &mut wire,
            &DETECT_BAUDS,
            QUICK,
            &AtomicBool::new(false),
            |_| {},
        );
        assert_eq!(
            detected,
            Detected::NotFound {
                tried: vec![57_600, 460_800, 230_400, 9_600, 4_800]
            }
        );
    }

    #[test]
    fn a_device_that_takes_no_rate_or_goes_away_is_gone_not_silent() {
        let mut stuck = Wire::new(&heartbeats(5), 57_600);
        stuck.cannot = DETECT_BAUDS.to_vec();
        let mut said = Vec::new();
        let detected = detect_baud_with(
            &mut stuck,
            &DETECT_BAUDS,
            QUICK,
            &AtomicBool::new(false),
            |b| {
                said.push(b);
            },
        );
        assert_eq!(detected, Detected::Gone);
        assert!(said.is_empty(), "it listened at nothing");

        let mut unplugged = Wire::new(&heartbeats(5), 57_600);
        unplugged.unplugged = true;
        let detected = detect_baud_with(
            &mut unplugged,
            &DETECT_BAUDS,
            QUICK,
            &AtomicBool::new(false),
            |_| {},
        );
        assert_eq!(detected, Detected::Gone);
        assert_eq!(unplugged.tuned, [57_600], "no rate after it went away");
    }

    #[test]
    fn it_stops_when_asked_and_lets_the_device_go() {
        let stop = AtomicBool::new(false);
        let mut wire = Wire::new(&[], 57_600);
        let detected = detect_baud_with(&mut wire, &DETECT_BAUDS, QUICK, &stop, |baud| {
            if baud == 115_200 {
                stop.store(true, Ordering::Relaxed);
            }
        });
        assert_eq!(detected, Detected::Stopped);
        assert_eq!(
            wire.tuned,
            [57_600, 115_200],
            "no rate after it was asked to stop"
        );
    }

    /// A pseudo-terminal set cooked, and its path. The handle returned
    /// stays open, as a terminal's own would: closing a pseudo-terminal's
    /// last handle resets its settings on macOS, which would hide a change.
    #[cfg(unix)]
    fn cooked_port() -> (serialport::TTYPort, std::fs::File, String) {
        use nix::sys::termios::{LocalFlags, SetArg, tcgetattr, tcsetattr};
        use serialport::SerialPort;
        let (vehicle_end, ours) = serialport::TTYPort::pair().unwrap();
        let path = ours.name().unwrap();
        let handle = reopen(&path);
        let mut cooked = tcgetattr(&handle).unwrap();
        cooked.local_flags |= LocalFlags::ICANON | LocalFlags::ECHO;
        tcsetattr(&handle, SetArg::TCSANOW, &cooked).unwrap();
        (vehicle_end, handle, path)
    }

    #[cfg(unix)]
    fn reopen(path: &str) -> std::fs::File {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(nix::libc::O_NOCTTY | nix::libc::O_NONBLOCK)
            .open(path)
            .unwrap()
    }

    /// Makes the port raw, as opening it for a link or for detection does.
    #[cfg(unix)]
    fn make_raw(path: &str) {
        use nix::sys::termios::{LocalFlags, SetArg, cfmakeraw, tcgetattr, tcsetattr};
        let handle = reopen(path);
        let mut raw = tcgetattr(&handle).unwrap();
        cfmakeraw(&mut raw);
        tcsetattr(&handle, SetArg::TCSANOW, &raw).unwrap();
        assert!(
            !tcgetattr(&handle)
                .unwrap()
                .local_flags
                .contains(LocalFlags::ICANON)
        );
    }

    #[cfg(unix)]
    fn is_cooked(path: &str) -> bool {
        use nix::sys::termios::{LocalFlags, tcgetattr};
        tcgetattr(reopen(path))
            .unwrap()
            .local_flags
            .contains(LocalFlags::ICANON | LocalFlags::ECHO)
    }

    #[cfg(unix)]
    #[test]
    fn the_settings_a_port_had_are_put_back_when_detection_lets_it_go() {
        let (_vehicle_end, _held, path) = cooked_port();
        let settings = PutBack(Settings::keep(&path).unwrap());
        make_raw(&path);
        drop(settings);
        assert!(is_cooked(&path));
    }

    #[cfg(unix)]
    #[test]
    fn settings_found_before_an_open_that_failed_never_undo_another_programs() {
        let (_vehicle_end, _held, path) = cooked_port();
        let found = Settings::keep(&path).unwrap();
        // A link opened the port between the look and detection's own open,
        // which then failed: the link's raw mode stays.
        make_raw(&path);
        drop(found);
        assert!(!is_cooked(&path));
    }
}
