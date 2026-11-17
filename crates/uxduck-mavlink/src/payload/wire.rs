//! What the payload reads from a frame, and the acknowledgement and the
//! small values it writes back.

use mavio::dialects::ardupilotmega::Ardupilotmega;
use mavio::io::StdIoReader;
use mavio::protocol::Versionless;
use mavio::{Frame, Receiver};

use crate::framing::Check;

/// `MAV_RESULT`, as a command's acknowledgement carries it.
pub const ACCEPTED: u8 = 0;
/// Valid, and not possible now.
pub const TEMPORARILY_REJECTED: u8 = 1;
/// A command it has, with values it cannot take.
pub const DENIED: u8 = 2;
/// A command it does not have.
pub const UNSUPPORTED: u8 = 3;
/// Valid, tried, and it did not work.
pub const FAILED: u8 = 4;
/// Taken, and still being carried out. Its end follows.
pub const IN_PROGRESS: u8 = 5;

/// `MAV_CMD_SET_MESSAGE_INTERVAL` and `MAV_CMD_REQUEST_MESSAGE`, which
/// every component of the payload answers.
pub const SET_MESSAGE_INTERVAL: u16 = 511;
pub const REQUEST_MESSAGE: u16 = 512;

const COMMAND_INT: u32 = 75;
const COMMAND_LONG: u32 = 76;
const COMMAND_ACK: u32 = 77;

/// One command, from `COMMAND_LONG` or `COMMAND_INT`. It is read from the
/// frame's bytes, so a `MAV_CMD` newer than mavio's dialect still arrives.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Command {
    /// The system and component that sent it.
    pub from: (u8, u8),
    /// The system and component it is for. Component 0 is every component.
    pub to: (u8, u8),
    pub command: u16,
    /// Param1 to param7. A `COMMAND_INT`'s x and y are here as numbers too.
    pub params: [f32; 7],
    /// A `COMMAND_INT`'s `MAV_FRAME`, x and y as they were sent.
    pub place: Option<(u8, i32, i32)>,
    /// How many times a `COMMAND_LONG` was sent before this one.
    pub confirmation: u8,
}

/// The command in a whole frame, if it is one with a good checksum.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "x and y are also kept as they were sent"
)]
pub fn command(frame: &[u8]) -> Option<Command> {
    let header = crate::framing::header(frame)?;
    if !matches!(header.message_id, COMMAND_INT | COMMAND_LONG)
        || crate::decode::check(frame) != Check::Valid
    {
        return None;
    }
    // MAVLink 2 leaves trailing zeros out.
    let mut bytes = [0u8; 35];
    let payload = crate::framing::payload(frame)?;
    let length = payload.len().min(bytes.len());
    bytes[..length].copy_from_slice(&payload[..length]);
    let four = |at: usize| [bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]];
    let float = |at: usize| f32::from_le_bytes(four(at));
    let from = (header.system_id, header.component_id);
    let to = (bytes[30], bytes[31]);
    let command = u16::from_le_bytes([bytes[28], bytes[29]]);
    if header.message_id == COMMAND_LONG {
        return Some(Command {
            from,
            to,
            command,
            params: [0, 4, 8, 12, 16, 20, 24].map(float),
            place: None,
            confirmation: bytes[32],
        });
    }
    let (x, y) = (i32::from_le_bytes(four(16)), i32::from_le_bytes(four(20)));
    Some(Command {
        from,
        to,
        command,
        params: [
            float(0),
            float(4),
            float(8),
            float(12),
            x as f32,
            y as f32,
            float(24),
        ],
        place: Some((bytes[32], x, y)),
        confirmation: 0,
    })
}

/// A `COMMAND_ACK` that says a command is still being carried out, and
/// how far along it is in percent (ADR-0063 Y16).
#[must_use]
pub fn in_progress(from: (u8, u8, u8), to: (u8, u8), command: u16, percent: u8) -> Vec<u8> {
    let mut payload = Vec::with_capacity(10);
    payload.extend_from_slice(&command.to_le_bytes());
    payload.extend_from_slice(&[IN_PROGRESS, percent.min(100), 0, 0, 0, 0, to.0, to.1]);
    crate::custom::frame_from(from, COMMAND_ACK, &payload, 143)
}

/// A frame as mavio's message with who sent it, for the messages the
/// payload reads whole.
#[must_use]
pub fn typed(frame: &[u8]) -> Option<((u8, u8), Ardupilotmega)> {
    let mut receiver = Receiver::new::<Versionless>(StdIoReader::new(frame));
    let frame: Frame<Versionless> = receiver.recv().ok()?;
    frame.validate_checksum::<Ardupilotmega>().ok()?;
    let from = (frame.system_id(), frame.component_id());
    Some((from, frame.decode::<Ardupilotmega>().ok()?))
}

/// A name in a fixed field, cut to fit and padded with zeros.
#[must_use]
pub fn text<const N: usize>(value: &str) -> [u8; N] {
    let mut bytes = [0u8; N];
    for (to, from) in bytes.iter_mut().zip(value.bytes()) {
        *to = from;
    }
    bytes
}

/// A version as the information messages carry it: major in the lowest
/// byte, then minor, patch and development.
#[must_use]
pub fn version(major: u8, minor: u8, patch: u8) -> u32 {
    u32::from(major) | u32::from(minor) << 8 | u32::from(patch) << 16
}

/// An angle brought into -π to π.
#[must_use]
pub fn wrap(angle: f32) -> f32 {
    let turn = std::f32::consts::TAU;
    let wrapped = (angle + std::f32::consts::PI).rem_euclid(turn) - std::f32::consts::PI;
    // `rem_euclid` may round up to a whole turn.
    if wrapped >= std::f32::consts::PI {
        wrapped - turn
    } else {
        wrapped
    }
}

/// The quaternion (w, x, y, z) of a roll, a pitch and a yaw, in radians.
#[must_use]
pub fn quaternion([roll, pitch, yaw]: [f32; 3]) -> [f32; 4] {
    let (sr, cr) = (roll / 2.0).sin_cos();
    let (sp, cp) = (pitch / 2.0).sin_cos();
    let (sy, cy) = (yaw / 2.0).sin_cos();
    [
        cr * cp * cy + sr * sp * sy,
        sr * cp * cy - cr * sp * sy,
        cr * sp * cy + sr * cp * sy,
        cr * cp * sy - sr * sp * cy,
    ]
}

/// The roll, pitch and yaw of a quaternion (w, x, y, z), or none when a
/// part of it is NaN, which is how the gimbal messages leave it unset.
#[must_use]
pub fn angles([w, x, y, z]: [f32; 4]) -> Option<[f32; 3]> {
    if [w, x, y, z].iter().any(|part| part.is_nan()) {
        return None;
    }
    let pitch = (2.0 * (w * y - z * x)).clamp(-1.0, 1.0).asin();
    // Straight up or down, roll and yaw turn about the same line. All of
    // it is taken as yaw, so that a look straight down keeps its heading.
    if pitch.abs() > std::f32::consts::FRAC_PI_2 - 0.01 {
        let yaw = (2.0 * (w * z - x * y)).atan2(1.0 - 2.0 * (x * x + z * z));
        return Some([0.0, pitch, yaw]);
    }
    Some([
        (2.0 * (w * x + y * z)).atan2(1.0 - 2.0 * (x * x + y * y)),
        pitch,
        (2.0 * (w * z + x * y)).atan2(1.0 - 2.0 * (y * y + z * z)),
    ])
}

/// A number from a command's parameter: a count, an id or a flag. NaN and
/// anything below zero are 0.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a float to integer cast saturates, and NaN is 0"
)]
pub fn whole(param: f32) -> u32 {
    param.round() as u32
}
