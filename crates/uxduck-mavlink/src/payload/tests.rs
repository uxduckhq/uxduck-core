//! The payload against the sequence diagrams of the two guide pages
//! (mavlink.io: camera protocol version 2, gimbal protocol version 2), the
//! messages a link loses, and what S400 measured ArduPilot needs.

mod camera;
mod fake_vehicle;
mod faults;
mod gimbal;
mod link;
mod manager;
mod profiles;
mod settings;
mod tracking;

pub(super) use mavio::dialects::ardupilotmega::Ardupilotmega as M;

pub(super) use super::wire::{self, ACCEPTED, DENIED, FAILED, TEMPORARILY_REJECTED, UNSUPPORTED};
pub(super) use super::{ManagedBy, Payload, Place};

/// The ground station, the vehicle's system, and its components.
pub(super) const GCS: (u8, u8) = (255, 190);
pub(super) const VEHICLE: u8 = 1;
pub(super) const AUTOPILOT: (u8, u8) = (VEHICLE, 1);
pub(super) const CAMERA: (u8, u8) = (VEHICLE, 100);
pub(super) const GIMBAL: (u8, u8) = (VEHICLE, 154);

/// The messages of one kind among some frames, in order.
macro_rules! of {
    ($frames:expr, $kind:ident) => {
        $frames
            .iter()
            .filter_map(|frame| match wire::typed(frame) {
                Some((_, M::$kind(message))) => Some(message),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
}
pub(super) use of;

/// Up to seven parameters, the rest zero.
pub(super) fn params(values: &[f32]) -> [f32; 7] {
    let mut params = [0.0; 7];
    params[..values.len()].copy_from_slice(values);
    params
}

/// A `COMMAND_LONG` from anyone to anyone.
pub(super) fn long(
    from: (u8, u8),
    to: (u8, u8),
    command: u16,
    values: &[f32],
    confirmation: u8,
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(33);
    for param in params(values) {
        payload.extend_from_slice(&param.to_le_bytes());
    }
    payload.extend_from_slice(&command.to_le_bytes());
    payload.extend_from_slice(&[to.0, to.1, confirmation]);
    crate::custom::frame_from((from.0, from.1, 0), 76, &payload, 152)
}

/// A `COMMAND_INT`: its `MAV_FRAME`, param1 to param4, then x, y and z.
pub(super) fn int(
    from: (u8, u8),
    to: (u8, u8),
    command: u16,
    frame: u8,
    values: [f32; 4],
    (x, y, z): (i32, i32, f32),
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(35);
    for param in values {
        payload.extend_from_slice(&param.to_le_bytes());
    }
    payload.extend_from_slice(&x.to_le_bytes());
    payload.extend_from_slice(&y.to_le_bytes());
    payload.extend_from_slice(&z.to_le_bytes());
    payload.extend_from_slice(&command.to_le_bytes());
    payload.extend_from_slice(&[to.0, to.1, frame, 0, 0]);
    crate::custom::frame_from((from.0, from.1, 0), 75, &payload, 158)
}

/// Who sent a frame.
pub(super) fn sender(frame: &[u8]) -> (u8, u8) {
    let header = crate::framing::header(frame).expect("a frame");
    (header.system_id, header.component_id)
}

/// An acknowledgement: who sent it, the command, its result and who it is
/// for.
pub(super) type Ack = ((u8, u8), u16, u8, (u8, u8));

/// The acknowledgements among some frames.
pub(super) fn acks(frames: &[Vec<u8>]) -> Vec<Ack> {
    frames
        .iter()
        .filter(|frame| crate::framing::header(frame).is_some_and(|h| h.message_id == 77))
        .map(|frame| {
            let mut bytes = [0u8; 10];
            let payload = crate::framing::payload(frame).expect("a payload");
            bytes[..payload.len()].copy_from_slice(payload);
            (
                sender(frame),
                u16::from_le_bytes([bytes[0], bytes[1]]),
                bytes[2],
                (bytes[8], bytes[9]),
            )
        })
        .collect()
}

/// The one acknowledgement among some frames, as its result. It must come
/// first, before any message the command asked for.
pub(super) fn result(frames: &[Vec<u8>], from: (u8, u8), command: u16) -> u8 {
    let acks = acks(frames);
    assert_eq!(acks.len(), 1, "one acknowledgement");
    assert_eq!(sender(&frames[0]), from);
    assert_eq!(
        crate::framing::header(&frames[0]).map(|h| h.message_id),
        Some(77),
        "the acknowledgement comes first"
    );
    assert_eq!((acks[0].0, acks[0].1), (from, command));
    acks[0].2
}

/// Steps a payload every 20 ms from just after `from_ms` to `to_ms`.
pub(super) fn run(payload: &mut Payload, from_ms: u64, to_ms: u64) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    let mut now_ms = from_ms;
    while now_ms < to_ms {
        now_ms += 20;
        frames.extend(payload.step(now_ms));
    }
    frames
}

pub(super) fn near(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.02
}

/// The fake vehicle's home, 100 m up.
pub(super) fn over_home() -> Place {
    Place {
        lat: -353_632_610,
        lon: 1_491_652_300,
        alt_mm: 684_000,
        relative_alt_mm: 100_000,
    }
}
