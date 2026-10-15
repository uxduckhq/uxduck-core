//! The UDP video listener (ADR-0017 V3): one thread, every sender a source.
//! What arrives goes through that sender's [`Source`] to the hub; once a
//! second each sender is reported to the engine, and one silent for 3 s is
//! reported stopped.
//!
//! A sender is its address without its port, since a camera may send from
//! another port when it restarts. Two streams at once from one address
//! (two test pipelines on one computer) are told apart by the port each
//! sends from; a new port while a stream from that address has gone quiet
//! is that stream back.

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use uxduck_core::domain::video::{
    CameraMetadata, VideoContainer, VideoCounters, VideoReport, source_id, split_source_id,
};
use uxduck_media::source::{Output, Source, Status};
use uxduck_media::udp::Container;

use super::hub::{Hub, Item};
use crate::daemon::engine_task::EngineHandle;
use crate::ports::{Protocol, in_use_message};

const REPORT_EVERY: Duration = Duration::from_secs(1);
/// Silent this long, a source has stopped.
const STOPPED_AFTER: Duration = Duration::from_secs(3);
/// Silent this long, a new port from the same address is the same stream
/// restarted rather than a second one.
const QUIET_AFTER: Duration = Duration::from_secs(1);

struct Sender {
    source: Source,
    address: SocketAddr,
    heard: Instant,
    stopped: bool,
    /// At the last report: when, bytes and pictures, for the rates.
    reported: (Instant, u64, u64),
    /// When its KLV last went to the engine.
    klv_posted: Option<Instant>,
}

/// Binds the port, or says why it cannot.
///
/// # Errors
/// A message for people, such as which program holds the port.
pub fn bind(port: u16) -> Result<UdpSocket, String> {
    UdpSocket::bind(("0.0.0.0", port)).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AddrInUse {
            in_use_message(Protocol::Udp, port)
        } else {
            format!("UXDuck can't listen on UDP port {port}: {error}")
        }
    })
}

/// Runs the listener on its own thread until `stop` is set.
pub fn spawn(socket: UdpSocket, hub: Arc<Hub>, engine: EngineHandle, stop: Arc<AtomicBool>) {
    let port = socket.local_addr().map_or(0, |a| a.port());
    let _ = socket.set_read_timeout(Some(Duration::from_millis(250)));
    let spawned = std::thread::Builder::new()
        .name("video-udp".into())
        .spawn(move || run(&socket, port, &hub, &engine, &stop));
    if let Err(error) = spawned {
        tracing::error!(%error, "the video listener could not start");
    }
}

fn run(socket: &UdpSocket, port: u16, hub: &Hub, engine: &EngineHandle, stop: &AtomicBool) {
    let mut senders: HashMap<String, Sender> = HashMap::new();
    let mut by_address: HashMap<SocketAddr, String> = HashMap::new();
    let mut buffer = vec![0u8; 65_536];
    let mut last_tick = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        if let Ok((len, from)) = socket.recv_from(&mut buffer) {
            crate::wakes::count("video");
            let now = Instant::now();
            let id = if let Some(id) = by_address.get(&from) {
                id.clone()
            } else {
                let id = claim(&mut senders, port, from, now);
                by_address.retain(|_, known| *known != id);
                by_address.insert(from, id.clone());
                id
            };
            let sender = senders.entry(id.clone()).or_insert_with(|| Sender {
                source: Source::new(),
                address: from,
                heard: now,
                stopped: false,
                reported: (now, 0, 0),
                klv_posted: None,
            });
            if sender.stopped {
                sender.source.resume();
                sender.stopped = false;
                let c = sender.source.counters();
                sender.reported = (now, c.bytes, c.pictures);
            }
            sender.heard = now;
            sender.address = from;
            for output in sender.source.push(&buffer[..len]) {
                if let Some(metadata) = metadata_due(&output, &mut sender.klv_posted) {
                    let id = id.clone();
                    engine.post(move |e| e.video_klv(&id, &metadata));
                }
                hub.publish(&id, &item(output));
            }
        }
        if last_tick.elapsed() >= REPORT_EVERY {
            last_tick = Instant::now();
            tick(&mut senders, port, engine);
        }
    }
    // Stopped listening: its senders have stopped as far as UXDuck knows.
    for (id, sender) in senders {
        if !sender.stopped {
            engine.post(move |e| e.video_stopped(&id));
        }
    }
}

/// Which source a datagram from a new address and port is: a quiet stream
/// from the same address, restarted; else the address's own source if free;
/// else a second source for the port it sends from.
fn claim(
    senders: &mut HashMap<String, Sender>,
    port: u16,
    from: SocketAddr,
    now: Instant,
) -> String {
    let address = from.to_string();
    let base = source_id(port, &address);
    let split = format!("{base}-from-");
    let mut quiet: Vec<&String> = senders
        .iter()
        .filter(|(id, s)| {
            (**id == base || id.starts_with(&split)) && now.duration_since(s.heard) >= QUIET_AFTER
        })
        .map(|(id, _)| id)
        .collect();
    quiet.sort_by_key(|id| **id != base);
    if let Some(id) = quiet.first().map(|id| (*id).clone())
        && let Some(sender) = senders.get_mut(&id)
    {
        sender.source.restart();
        sender.stopped = false;
        let c = sender.source.counters();
        sender.reported = (now, c.bytes, c.pictures);
        return id;
    }
    if senders.contains_key(&base) {
        split_source_id(port, &address)
    } else {
        base
    }
}

/// KLV's values go to the engine at most this often.
const KLV_EVERY: Duration = Duration::from_millis(200);

/// A KLV packet's values, if they are due to go to the engine.
pub(super) fn metadata_due(
    output: &Output,
    posted: &mut Option<Instant>,
) -> Option<CameraMetadata> {
    let Output::Klv {
        values: Some(v), ..
    } = output
    else {
        return None;
    };
    if posted.is_some_and(|at| at.elapsed() < KLV_EVERY) {
        return None;
    }
    *posted = Some(Instant::now());
    Some(CameraMetadata {
        platform_designation: v.platform_designation.clone(),
        image_source: v.image_source.clone(),
        sensor_latitude_deg: v.sensor_latitude_deg,
        sensor_longitude_deg: v.sensor_longitude_deg,
        sensor_altitude_m: v.sensor_altitude_m,
        platform_heading_deg: v.platform_heading_deg,
        horizontal_fov_deg: v.horizontal_fov_deg,
        vertical_fov_deg: v.vertical_fov_deg,
        sensor_azimuth_deg: v.sensor_azimuth_deg,
        sensor_elevation_deg: v.sensor_elevation_deg,
        slant_range_m: v.slant_range_m,
        frame_latitude_deg: v.frame_latitude_deg,
        frame_longitude_deg: v.frame_longitude_deg,
        frame_elevation_m: v.frame_elevation_m,
        corners: v.corners,
    })
}

/// An item for the hub from what a source put out.
pub(super) fn item(output: Output) -> Item {
    match output {
        Output::Unit(unit) => Item::Unit(unit),
        Output::Klv {
            timestamp_us, data, ..
        } => Item::Klv {
            timestamp_us,
            data: data.into(),
        },
    }
}

fn tick(senders: &mut HashMap<String, Sender>, port: u16, engine: &EngineHandle) {
    let now = Instant::now();
    for (id, sender) in senders.iter_mut() {
        if sender.stopped {
            continue;
        }
        if now.duration_since(sender.heard) >= STOPPED_AFTER {
            sender.stopped = true;
            let id = id.clone();
            engine.post(move |e| e.video_stopped(&id));
            continue;
        }
        let status = sender.source.status();
        let (at, bytes, pictures) = sender.reported;
        let elapsed = now.duration_since(at).as_secs_f64().max(0.001);
        sender.reported = (now, status.counters.bytes, status.counters.pictures);
        let mut report = report(port, &sender.address, &status, elapsed, bytes, pictures);
        // A second stream from the address says which it is.
        if report.source_id() != *id {
            report.id = Some(id.clone());
        }
        engine.post(move |e| e.video_observed(report));
    }
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "rates over a second, well within range"
)]
pub(super) fn report(
    port: u16,
    address: &SocketAddr,
    status: &Status,
    elapsed: f64,
    bytes_before: u64,
    pictures_before: u64,
) -> VideoReport {
    let c = &status.counters;
    let bitrate = (c.bytes.saturating_sub(bytes_before) * 8) as f64 / elapsed;
    let rate = c.pictures.saturating_sub(pictures_before) as f64 / elapsed;
    VideoReport {
        port,
        sender: address.to_string(),
        container: match status.container {
            Some(Container::RtpTs) => VideoContainer::RtpTs,
            Some(Container::RtpVideo) => VideoContainer::Rtp,
            Some(Container::RtpH264) => VideoContainer::RtpH264,
            Some(Container::RtpH265) => VideoContainer::RtpH265,
            Some(Container::RawTs) | None => VideoContainer::Ts,
        },
        codec: status.codec.clone(),
        width: status.width,
        height: status.height,
        frame_rate: (rate > 0.0).then(|| (rate * 10.0).round() / 10.0),
        unsupported: status.unsupported.clone(),
        klv: status.klv,
        id: None,
        note: None,
        counters: VideoCounters {
            bitrate_bps: bitrate as u64,
            datagrams: c.datagrams,
            pictures: c.pictures,
            keyframes: c.keyframes,
            lost: c.lost,
            damaged: c.damaged,
            pes_length_mismatches: c.pes_length_mismatches,
            pes_length_max_excess: c.pes_length_max_excess,
            nals_dropped: c.nals_dropped,
            klv: c.klv,
            klv_bad: c.klv_bad,
        },
    }
}
