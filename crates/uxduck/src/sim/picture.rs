//! The picture of a simulation's camera (ADR-0063 Y7, S408).
//!
//! A simulated camera has no sensor. What it sees is a 3D scene, and only
//! an open UXDuck window can draw one. So for each simulation that carries
//! a camera the daemon keeps what the camera truly sees, which is the
//! payload's own place, attitude and field of view, asks one open window to
//! draw it while someone watches or records it, takes the pictures that
//! window encodes, and sends them on as the camera's stream: RTP over UDP
//! on loopback, to the port the camera announces. From there it is a
//! camera's stream like any other, through the listener.
//!
//! Windows reach it over a WebSocket, `/sim-camera`, opened with a token
//! from `sim/pictures`. Its messages are in [`wire`].
//!
//! With nobody watching or recording, nothing is drawn and nothing is sent.
//! With no window that can draw, the stream is silent, and the camera's
//! video says why ([`Why`]).

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::net::{Ipv4Addr, UdpSocket};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{Notify, mpsc};
use uxduck_mavlink::payload::View;
use uxduck_media::latency::unix_us_ntp;
use uxduck_media::nal::{Codec, split_annex_b};
use uxduck_media::rtcp::SenderReport;
use uxduck_media::rtp::Packetizer;

pub mod socket;
pub mod wire;

/// The UDP port the first simulation's camera sends its picture to. Each
/// instance adds one, so each camera's stream is a source of its own, and
/// the same one each time its simulation runs.
const FIRST_PORT: u16 = 15_600;

/// The RTP payload type of the picture: a dynamic one, as H.264 has.
const PAYLOAD_TYPE: u8 = 96;
/// A sender report goes before the first picture, and this often after.
const REPORT_EVERY: Duration = Duration::from_secs(1);
/// A token from `sim/pictures` opens its socket within this long.
const TOKEN_LASTS: Duration = Duration::from_secs(30);
/// With no picture for this long, a watched camera says why. Sooner, the
/// listener may still report the last second's pictures.
const QUIET_FOR: Duration = Duration::from_millis(1500);
/// A camera wanted for less than this says nothing yet. A window takes a
/// moment to offer to draw it, and a reason that is gone the next second
/// reads as a fault (seen in the Camera tab as a simulation started,
/// 2026-11-15).
const SETTLE_FOR: Duration = Duration::from_secs(3);

/// The first of the ports a camera takes when another program holds its
/// own: past every instance's.
const SPARE_PORT: u16 = FIRST_PORT + 256;
/// How many of those there are. The look for a free one has to end, and
/// when none of them can be had the camera keeps its own port.
const SPARE_PORTS: u16 = 64;

/// The port an instance's camera sends its picture to, by rule.
#[must_use]
pub fn port(instance: u8) -> u16 {
    FIRST_PORT + u16::from(instance)
}

/// The port an instance's camera sends its picture to now: its own, unless
/// another program holds that, and then a spare one.
///
/// Another UXDuck on this computer is such a program: one still listening
/// for the camera it simulated before. Nothing here would hear the picture
/// on that port, and the other UXDuck would (seen with a development and an
/// installed UXDuck side by side, 2026-11-15).
#[must_use]
pub fn port_for(instance: u8) -> u16 {
    let own = port(instance);
    let port = choose_port(own, crate::video::listener::holds, |port| {
        UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).is_ok()
    });
    if port != own {
        tracing::warn!(
            own,
            port,
            "another program holds a simulated camera's port: its picture goes to a spare one"
        );
    }
    port
}

/// `own` if this UXDuck listens on it already or it can be had, else the
/// first spare port of which that is true, else `own` after all.
fn choose_port(own: u16, ours: impl Fn(u16) -> bool, free: impl Fn(u16) -> bool) -> u16 {
    std::iter::once(own)
        .chain(SPARE_PORT..SPARE_PORT + SPARE_PORTS)
        .find(|port| ours(*port) || free(*port))
        .unwrap_or(own)
}

/// The address a camera announces for its stream.
#[must_use]
pub fn uri(port: u16) -> String {
    format!("udp://127.0.0.1:{port}")
}

/// The video source the listener makes of what is sent to `port` from here.
#[must_use]
pub fn source(port: u16) -> String {
    uxduck_core::domain::video::source_id(port, "127.0.0.1")
}

/// How a window encodes pictures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Encoder {
    None,
    Software,
    Hardware,
}

/// What a window can do, as it says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Able {
    pub encoder: Encoder,
    /// It is shown: a browser draws nothing in a hidden tab.
    pub visible: bool,
    /// It draws while hidden, as the desktop app's windows do.
    pub background: bool,
}

impl Able {
    fn draws_now(self) -> bool {
        self.encoder != Encoder::None && (self.visible || self.background)
    }
}

/// Why no window draws a camera's picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    NoWindow,
    NoEncoder,
    Hidden,
}

impl Why {
    /// As the camera's video says it, to a person.
    #[must_use]
    pub fn says(self) -> &'static str {
        match self {
            Self::NoWindow => {
                "No UXDuck window is open on this computer to draw this simulated camera's picture. Open UXDuck there."
            }
            Self::NoEncoder => {
                "No open UXDuck window can encode this simulated camera's picture. Open UXDuck Desktop on this computer."
            }
            Self::Hidden => {
                "The browser tab that draws this simulated camera's picture is hidden. Show it, or open UXDuck Desktop, which draws in the background."
            }
        }
    }
}

/// Which window draws a camera: the one that draws it already while it
/// still can, so the picture does not jump between windows, else one that
/// draws while hidden before one that does not, a hardware encoder before
/// a software one, and the window open longest.
fn choose(drawing: Option<u64>, windows: &BTreeMap<u64, Window>) -> Result<u64, Why> {
    let able = |id: &u64| windows.get(id).and_then(|window| window.able);
    if let Some(id) = drawing
        && able(&id).is_some_and(Able::draws_now)
    {
        return Ok(id);
    }
    let said: Vec<(u64, Able)> = windows
        .iter()
        .filter_map(|(id, window)| Some((*id, window.able?)))
        .collect();
    said.iter()
        .filter(|(_, able)| able.draws_now())
        .max_by_key(|(id, able)| (able.background, able.encoder, std::cmp::Reverse(*id)))
        .map(|(id, _)| *id)
        .ok_or_else(|| {
            if said.is_empty() {
                Why::NoWindow
            } else if said.iter().all(|(_, able)| able.encoder == Encoder::None) {
                Why::NoEncoder
            } else {
                Why::Hidden
            }
        })
}

struct Window {
    to: mpsc::UnboundedSender<Vec<u8>>,
    /// None until it has said.
    able: Option<Able>,
    /// The cameras it was last told it draws.
    draws: Vec<u16>,
}

struct Camera {
    /// Tells this simulation's camera from one that had its port before.
    ticket: u64,
    view: Option<View>,
    drawer: Option<u64>,
    packetizer: Packetizer,
    /// RTP packets and bytes of payload sent, for the sender reports.
    packets: u32,
    octets: u32,
    reported: Option<Instant>,
    pictured: Option<Instant>,
    /// Since when its picture has been wanted, if it is.
    wanted_since: Option<Instant>,
}

#[derive(Default)]
struct Inner {
    cameras: BTreeMap<u16, Camera>,
    windows: BTreeMap<u64, Window>,
    next: u64,
    /// Tokens from `sim/pictures`, and when each lapses.
    tokens: HashMap<String, Instant>,
    /// Where each camera's picture is sent from. Kept while the daemon
    /// runs, so the listener hears one sender whatever comes and goes.
    senders: HashMap<u16, UdpSocket>,
}

/// Every simulated camera's picture on this computer.
#[derive(Default)]
pub struct Pictures {
    inner: Mutex<Inner>,
    /// A camera came: the review has work.
    carried: Notify,
}

/// The daemon's own.
pub fn pictures() -> &'static Pictures {
    static PICTURES: OnceLock<Pictures> = OnceLock::new();
    PICTURES.get_or_init(Pictures::default)
}

fn unix_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_micros()).unwrap_or(u64::MAX)
        })
}

/// A time on this computer's clock on RTP's 90 kHz clock, which wraps.
#[allow(clippy::cast_possible_truncation, reason = "RTP's clock wraps")]
const fn rtp_time(unix_us: u64) -> u32 {
    (unix_us as u128 * 9 / 100) as u32
}

impl Pictures {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A simulation carries a camera whose picture goes to `port`. Returns
    /// a ticket to give back with [`Pictures::leave`].
    pub fn carry(&self, port: u16) -> u64 {
        let mut inner = self.lock();
        inner.next += 1;
        let ticket = inner.next;
        inner.cameras.insert(
            port,
            Camera {
                ticket,
                view: None,
                drawer: None,
                // "SC", then the port: the same stream each run.
                packetizer: Packetizer::new(
                    Codec::H264,
                    PAYLOAD_TYPE,
                    0x5343_0000 | u32::from(port),
                ),
                packets: 0,
                octets: 0,
                reported: None,
                pictured: None,
                wanted_since: None,
            },
        );
        drop(inner);
        self.carried.notify_one();
        ticket
    }

    /// The simulation's link closed. One started since on the same port
    /// stays.
    pub fn leave(&self, port: u16, ticket: u64) {
        let mut inner = self.lock();
        if inner
            .cameras
            .get(&port)
            .is_some_and(|camera| camera.ticket == ticket)
        {
            inner.cameras.remove(&port);
        }
    }

    /// What a camera sees now. Goes on to the window that draws it.
    pub fn sees(&self, port: u16, view: Option<View>) {
        let mut inner = self.lock();
        let Some(camera) = inner.cameras.get_mut(&port) else {
            return;
        };
        if camera.view == view {
            return;
        }
        camera.view = view;
        let drawer = camera.drawer;
        if let Some(view) = view
            && let Some(window) = drawer.and_then(|id| inner.windows.get(&id))
        {
            let _ = window.to.send(wire::view(port, &view));
        }
    }

    /// The ports of the cameras carried now.
    pub fn ports(&self) -> Vec<u16> {
        self.lock().cameras.keys().copied().collect()
    }

    /// Resolves once a camera is carried.
    pub async fn carried(&self) {
        self.carried.notified().await;
    }

    /// Lets one window open its socket with `token`, for a while.
    pub fn allow(&self, token: &str) {
        let mut inner = self.lock();
        let now = Instant::now();
        inner.tokens.retain(|_, lapses| *lapses > now);
        inner.tokens.insert(token.to_owned(), now + TOKEN_LASTS);
    }

    /// Whether `token` opens a socket: once, and before it lapses.
    pub fn admit(&self, token: &str) -> bool {
        self.lock()
            .tokens
            .remove(token)
            .is_some_and(|lapses| lapses > Instant::now())
    }

    /// A window's socket opened: its number, and what to send it.
    pub fn join(&self) -> (u64, mpsc::UnboundedReceiver<Vec<u8>>) {
        let (to, from) = mpsc::unbounded_channel();
        let mut inner = self.lock();
        inner.next += 1;
        let id = inner.next;
        inner.windows.insert(
            id,
            Window {
                to,
                able: None,
                draws: Vec::new(),
            },
        );
        tracing::debug!(window = id, "a window offers to draw simulated cameras");
        (id, from)
    }

    /// A window says what it can do.
    pub fn able(&self, window: u64, able: Able) {
        if let Some(window) = self.lock().windows.get_mut(&window) {
            window.able = Some(able);
        }
    }

    /// A window's socket closed.
    pub fn left(&self, window: u64) {
        self.lock().windows.remove(&window);
        tracing::debug!(
            window,
            "a window that offered to draw simulated cameras left"
        );
    }

    /// Who draws what, by which cameras are `wanted`, watched or recorded.
    /// Each window is told when what it draws changes. Returns the cameras
    /// wanted for a while that nobody draws and that have gone quiet, with
    /// why.
    pub fn review(&self, wanted: impl Fn(u16) -> bool) -> Vec<(u16, Why)> {
        self.review_at(Instant::now(), wanted)
    }

    fn review_at(&self, now: Instant, wanted: impl Fn(u16) -> bool) -> Vec<(u16, Why)> {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let mut silent = Vec::new();
        for (port, camera) in &mut inner.cameras {
            if !wanted(*port) {
                camera.wanted_since = None;
                if camera.drawer.take().is_some() {
                    tracing::info!(port, "a simulated camera's picture is no longer wanted");
                }
                continue;
            }
            let wanted_since = *camera.wanted_since.get_or_insert(now);
            match choose(camera.drawer, &inner.windows) {
                Ok(window) => {
                    if camera.drawer != Some(window) {
                        camera.drawer = Some(window);
                        // Another window's clock: the next picture is dated anew.
                        camera.reported = None;
                        let encoder = inner
                            .windows
                            .get(&window)
                            .and_then(|window| window.able)
                            .map_or(Encoder::None, |able| able.encoder);
                        tracing::info!(
                            port,
                            window,
                            ?encoder,
                            "a window draws a simulated camera's picture"
                        );
                    }
                }
                Err(why) => {
                    if camera.drawer.take().is_some() {
                        tracing::info!(
                            port,
                            why = why.says(),
                            "no window draws a simulated camera"
                        );
                    }
                    let settled = now.duration_since(wanted_since) >= SETTLE_FOR;
                    let quiet = camera
                        .pictured
                        .is_none_or(|at| now.duration_since(at) >= QUIET_FOR);
                    if settled && quiet {
                        silent.push((*port, why));
                    }
                }
            }
        }
        for (id, window) in &mut inner.windows {
            let draws: Vec<u16> = inner
                .cameras
                .iter()
                .filter(|(_, camera)| camera.drawer == Some(*id))
                .map(|(port, _)| *port)
                .collect();
            if draws == window.draws {
                continue;
            }
            let _ = window.to.send(wire::draws(&draws));
            // What each camera new to it sees, at once.
            for port in draws.iter().filter(|port| !window.draws.contains(port)) {
                if let Some(view) = inner.cameras.get(port).and_then(|camera| camera.view) {
                    let _ = window.to.send(wire::view(*port, &view));
                }
            }
            window.draws = draws;
        }
        silent
    }

    /// One picture from a window, sent on as RTP if that window draws its
    /// camera. A sender report goes first, and again every second, with
    /// when the picture was drawn: the stream's clock.
    pub fn picture(&self, window: u64, picture: &wire::Picture<'_>) {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let Some(camera) = inner.cameras.get_mut(&picture.port) else {
            return;
        };
        if camera.drawer != Some(window) {
            return;
        }
        let Some(sender) = sender(&mut inner.senders, picture.port) else {
            return;
        };
        let to = (Ipv4Addr::LOCALHOST, picture.port);
        let now = Instant::now();
        let timestamp = rtp_time(picture.captured_us);
        if camera
            .reported
            .is_none_or(|at| now.duration_since(at) >= REPORT_EVERY)
        {
            camera.reported = Some(now);
            let report = SenderReport {
                ssrc: camera.packetizer.ssrc,
                ntp: unix_us_ntp(picture.captured_us),
                rtp: timestamp,
            };
            let _ = sender.send_to(&report.packet(camera.packets, camera.octets), to);
        }
        let nals = split_annex_b(picture.data);
        for datagram in camera.packetizer.picture(&nals, timestamp) {
            camera.packets = camera.packets.wrapping_add(1);
            #[allow(clippy::cast_possible_truncation, reason = "a datagram's payload")]
            let payload = datagram.len().saturating_sub(12) as u32;
            camera.octets = camera.octets.wrapping_add(payload);
            let _ = sender.send_to(&datagram, to);
        }
        camera.pictured = Some(now);
    }

    /// One datagram to a camera's port with no picture in it, so the
    /// listener there finds the stream and it can be watched: a sender
    /// report of a stream that has sent nothing.
    pub fn hello(&self, port: u16) {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let Some(camera) = inner.cameras.get(&port) else {
            return;
        };
        let Some(sender) = sender(&mut inner.senders, port) else {
            return;
        };
        let now = unix_us();
        let report = SenderReport {
            ssrc: camera.packetizer.ssrc,
            ntp: unix_us_ntp(now),
            rtp: rtp_time(now),
        };
        let _ = sender.send_to(&report.packet(0, 0), (Ipv4Addr::LOCALHOST, port));
    }
}

/// The socket a camera's picture is sent from, made the first time.
fn sender(senders: &mut HashMap<u16, UdpSocket>, port: u16) -> Option<&UdpSocket> {
    match senders.entry(port) {
        Entry::Occupied(made) => Some(&*made.into_mut()),
        Entry::Vacant(free) => match UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)) {
            Ok(socket) => Some(&*free.insert(socket)),
            Err(error) => {
                tracing::warn!(%error, port, "a simulated camera's picture cannot be sent");
                None
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use uxduck_mavlink::payload::Place;
    use uxduck_media::rtcp::sender_report;
    use uxduck_media::rtp::{self, Depacketizer};

    use super::*;

    const DESKTOP: Able = Able {
        encoder: Encoder::Software,
        visible: false,
        background: true,
    };
    const TAB: Able = Able {
        encoder: Encoder::Hardware,
        visible: true,
        background: false,
    };

    fn view(hfov_deg: f32) -> View {
        View {
            place: Place {
                lat: -353_632_610,
                lon: 1_491_652_300,
                alt_mm: 684_000,
                relative_alt_mm: 100_000,
            },
            attitude: [1.0, 0.0, 0.0, 0.0],
            hfov_deg,
            thermal: false,
        }
    }

    fn said(from: &mut mpsc::UnboundedReceiver<Vec<u8>>) -> Vec<Vec<u8>> {
        std::iter::from_fn(|| from.try_recv().ok()).collect()
    }

    // ADR-0063 Y7: drawn by one window, only while watched or recorded.
    #[test]
    fn one_window_draws_a_camera_and_only_while_its_picture_is_wanted() {
        let pictures = Pictures::default();
        let ticket = pictures.carry(15_600);
        pictures.sees(15_600, Some(view(44.8)));
        let (first, mut to_first) = pictures.join();
        let (second, mut to_second) = pictures.join();
        pictures.able(first, TAB);
        pictures.able(second, TAB);
        // Nobody watches: nobody draws, and nothing is said to anyone.
        assert!(pictures.review(|_| false).is_empty());
        assert!(said(&mut to_first).is_empty());
        // Watched: the window open longest draws, and is told what the
        // camera sees at once. The other is told nothing.
        assert!(pictures.review(|_| true).is_empty());
        assert_eq!(
            said(&mut to_first),
            [wire::draws(&[15_600]), wire::view(15_600, &view(44.8))]
        );
        assert!(said(&mut to_second).is_empty());
        // What the camera sees goes on to the window that draws it, when
        // it changes.
        pictures.sees(15_600, Some(view(44.8)));
        pictures.sees(15_600, Some(view(9.4)));
        assert_eq!(said(&mut to_first), [wire::view(15_600, &view(9.4))]);
        assert!(said(&mut to_second).is_empty());
        // It stays with that window, review after review.
        assert!(pictures.review(|_| true).is_empty());
        assert!(said(&mut to_first).is_empty());
        // That window closes: the other draws.
        pictures.left(first);
        assert!(pictures.review(|_| true).is_empty());
        assert_eq!(
            said(&mut to_second),
            [wire::draws(&[15_600]), wire::view(15_600, &view(9.4))]
        );
        // Nobody watches any more: it is told to stop.
        assert!(pictures.review(|_| false).is_empty());
        assert_eq!(said(&mut to_second), [wire::draws(&[])]);
        // The simulation stops while it draws: it is told to stop, too.
        pictures.review(|_| true);
        said(&mut to_second);
        pictures.leave(15_600, ticket);
        assert!(pictures.review(|_| true).is_empty());
        assert_eq!(said(&mut to_second), [wire::draws(&[])]);
    }

    // ADR-0063 Y7: with no window drawing, the stream is silent and the
    // camera's video says why.
    #[test]
    fn a_watched_camera_nobody_can_draw_says_why() {
        let pictures = Pictures::default();
        pictures.carry(15_600);
        let start = Instant::now();
        let after = |settles: u32| start + SETTLE_FOR * settles;
        // Just watched, a window may be about to offer: nothing is said yet.
        assert!(pictures.review_at(after(0), |_| true).is_empty());
        assert!(
            pictures
                .review_at(start + SETTLE_FOR / 2, |_| true)
                .is_empty()
        );
        assert_eq!(
            pictures.review_at(after(1), |_| true),
            [(15_600, Why::NoWindow)]
        );
        // Unwatched, there is nothing to say, and watched again it waits again.
        assert!(pictures.review_at(after(1), |_| false).is_empty());
        assert!(pictures.review_at(after(1), |_| true).is_empty());
        let now = after(2);
        assert_eq!(pictures.review_at(now, |_| true), [(15_600, Why::NoWindow)]);
        // A window that has not yet said what it can do is not counted.
        let (window, mut to) = pictures.join();
        assert_eq!(pictures.review_at(now, |_| true), [(15_600, Why::NoWindow)]);
        pictures.able(
            window,
            Able {
                encoder: Encoder::None,
                ..TAB
            },
        );
        assert_eq!(
            pictures.review_at(now, |_| true),
            [(15_600, Why::NoEncoder)]
        );
        // A browser's tab draws while it is shown, and stops while hidden.
        pictures.able(window, TAB);
        assert!(pictures.review_at(now, |_| true).is_empty());
        assert_eq!(said(&mut to), [wire::draws(&[15_600])]);
        pictures.able(
            window,
            Able {
                visible: false,
                ..TAB
            },
        );
        assert_eq!(pictures.review_at(now, |_| true), [(15_600, Why::Hidden)]);
        assert_eq!(said(&mut to), [wire::draws(&[])]);
        // The desktop app's window draws hidden or not.
        pictures.able(window, DESKTOP);
        assert!(pictures.review_at(now, |_| true).is_empty());
        assert_eq!(said(&mut to), [wire::draws(&[15_600])]);
        for why in [Why::NoWindow, Why::NoEncoder, Why::Hidden] {
            assert!(!why.says().contains(';'), "{}", why.says());
        }
    }

    #[test]
    fn the_desktop_app_draws_before_a_browser_and_hardware_before_software() {
        let window = |able| Window {
            to: mpsc::unbounded_channel().0,
            able: Some(able),
            draws: Vec::new(),
        };
        let software = Able {
            encoder: Encoder::Software,
            ..TAB
        };
        let mut windows = BTreeMap::new();
        windows.insert(1, window(software));
        windows.insert(2, window(TAB));
        windows.insert(3, window(TAB));
        assert_eq!(choose(None, &windows), Ok(2));
        windows.insert(4, window(DESKTOP));
        assert_eq!(choose(None, &windows), Ok(4));
        // The window drawing keeps drawing while it can, whoever else came.
        assert_eq!(choose(Some(1), &windows), Ok(1));
        windows.insert(
            1,
            window(Able {
                visible: false,
                ..software
            }),
        );
        assert_eq!(choose(Some(1), &windows), Ok(4));
        assert_eq!(choose(Some(9), &windows), Ok(4));
    }

    // ADR-0063 Y7: the pictures go out as RTP over UDP on loopback, to the
    // port the camera announces, with a clock.
    #[test]
    fn a_picture_goes_on_as_rtp_with_a_sender_report_that_dates_it() {
        let listener = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("a port");
        listener
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("a timeout");
        let port = listener.local_addr().expect("an address").port();
        let pictures = Pictures::default();
        pictures.carry(port);
        let (window, _to) = pictures.join();
        let (other, _to_other) = pictures.join();
        pictures.able(window, TAB);
        pictures.able(other, TAB);
        let sps: &[u8] = &[0x67, 0x42, 0xe0, 0x1f, 0x8c, 0x8d, 0x40];
        let pps: &[u8] = &[0x68, 0xce, 0x3c, 0x80];
        let idr: Vec<u8> = std::iter::once(0x65)
            .chain((0..3000u32).map(|n| (n % 251) as u8 + 1))
            .collect();
        let unit = [&[0, 0, 0, 1][..], sps, &[0, 0, 0, 1], pps, &[0, 0, 1], &idr].concat();
        let captured_us = 1_800_000_000_250_000;
        let drawn = wire::Picture {
            port,
            key: true,
            captured_us,
            data: &unit,
        };
        // Before anyone watches, a picture is not sent: nobody draws.
        pictures.picture(window, &drawn);
        pictures.review(|_| true);
        // And one from a window that does not draw this camera is dropped.
        pictures.picture(other, &drawn);
        pictures.picture(window, &drawn);
        let mut buffer = [0u8; 2048];
        let mut next = || {
            let (len, _) = listener.recv_from(&mut buffer).expect("a datagram");
            buffer[..len].to_vec()
        };
        // The clock first: when the picture was drawn, and its RTP time.
        let report = sender_report(&next()).expect("a sender report");
        assert_eq!(
            uxduck_media::latency::ntp_unix_us(report.ntp),
            Some(captured_us)
        );
        // The next picture's time by it, to RTP's tick of 11 millionths.
        let next_us =
            uxduck_media::latency::rtp_capture_us(&report, rtp_time(captured_us + 33_333))
                .expect("a time");
        assert!(next_us.abs_diff(captured_us + 33_333) < 12, "{next_us}");
        // Then the picture: its parameter sets alone, the picture in
        // fragments below 1200 bytes, the marker on the last.
        let mut depacketizer = Depacketizer::new(Codec::H264);
        let mut got = Vec::new();
        let mut markers = Vec::new();
        for _ in 0..5 {
            let datagram = next();
            assert!(datagram.len() <= 12 + 1200);
            let (header, payload) = rtp::parse(&datagram).expect("RTP");
            assert_eq!(header.payload_type, PAYLOAD_TYPE);
            assert_eq!(header.timestamp, report.rtp);
            markers.push(header.marker);
            got.extend(depacketizer.push(&header, payload));
        }
        assert_eq!(markers, [false, false, false, false, true]);
        assert_eq!(got.len(), 1);
        assert_eq!(split_annex_b(&got[0].annex_b), [sps, pps, &idr[..]]);
        // The next picture a moment later has no report before it.
        pictures.picture(
            window,
            &wire::Picture {
                captured_us: captured_us + 33_333,
                key: false,
                ..drawn
            },
        );
        assert!(sender_report(&next()).is_none());
    }

    // The stream can be watched only once the listener has heard it, and it
    // sends nothing until it is watched: one datagram with no picture in it
    // makes it known.
    #[test]
    fn a_camera_makes_itself_known_to_the_listener_with_no_picture() {
        let listener = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("a port");
        listener
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("a timeout");
        let port = listener.local_addr().expect("an address").port();
        let pictures = Pictures::default();
        pictures.hello(port);
        pictures.carry(port);
        pictures.hello(port);
        let mut buffer = [0u8; 64];
        let (len, _) = listener.recv_from(&mut buffer).expect("a datagram");
        // RTP's version to the listener, which tells streams by it, and a
        // sender report to a source that reads it.
        assert!(uxduck_media::udp::classify(&buffer[..len]).is_ok());
        assert!(sender_report(&buffer[..len]).is_some());
        // Only the one: none went out before the camera was carried.
        listener
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("a timeout");
        assert!(listener.recv_from(&mut buffer).is_err());
    }

    // Seen with two UXDucks on one computer: the second's camera had no
    // video at all, and its picture went to the first.
    #[test]
    fn a_camera_whose_port_another_program_holds_takes_a_spare_one() {
        let nobody = |_: u16| false;
        // Its own port, free or already listened on by this UXDuck.
        assert_eq!(choose_port(15_600, nobody, |_| true), 15_600);
        assert_eq!(
            choose_port(15_601, |port| port == 15_601, |_| false),
            15_601
        );
        // Held by another program: the first spare port that can be had,
        // past every instance's own.
        assert_eq!(choose_port(15_600, nobody, |port| port != 15_600), 15_856);
        assert_eq!(
            choose_port(15_600, |port| port == 15_857, |port| port > 15_857),
            15_857
        );
        assert!(u16::from(u8::MAX) + FIRST_PORT < SPARE_PORT);
        // None to be had: its own, where at least its video says it waits.
        assert_eq!(choose_port(15_600, nobody, |_| false), 15_600);
    }

    #[test]
    fn a_token_opens_one_socket() {
        let pictures = Pictures::default();
        assert!(!pictures.admit("sp1"));
        pictures.allow("sp1");
        assert!(pictures.admit("sp1"));
        assert!(!pictures.admit("sp1"));
    }

    #[test]
    fn a_simulation_started_again_on_the_port_outlives_the_one_before() {
        let pictures = Pictures::default();
        let before = pictures.carry(15_600);
        let after = pictures.carry(15_600);
        pictures.leave(15_600, before);
        assert_eq!(pictures.ports(), [15_600]);
        pictures.leave(15_600, after);
        assert!(pictures.ports().is_empty());
        assert_eq!((port(0), port(3)), (15_600, 15_603));
        assert_eq!(uri(15_600), "udp://127.0.0.1:15600");
        assert_eq!(source(15_600), "udp-15600-127.0.0.1");
    }
}
