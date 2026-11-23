//! A camera that a computer on the vehicle presents for its recordings
//! (ADR-0066): its heartbeat, what it says of itself, its storage and its
//! capture status, and the commands it reads. Frames only, so the companion
//! that keeps the recording's state is tested without a link.

use mavio::dialects::ardupilotmega::enums::{
    CameraCapFlags, MavAutopilot, MavModeFlag, MavSeverity, MavState, MavType, StorageStatus,
    StorageType, StorageUsageFlag,
};
use mavio::dialects::ardupilotmega::messages::{
    CameraCaptureStatus, CameraInformation, Heartbeat, Statustext, StorageInformation,
};

pub use crate::payload::wire::{
    ACCEPTED, Command, DENIED, TEMPORARILY_REJECTED, UNSUPPORTED, command, text,
};

/// `MAV_COMP_ID_CAMERA`, the first camera's component. `MAV_COMP_ID_CAMERA2` to 6 follow it.
pub const CAMERA: u8 = 100;
/// The camera components there are, `MAV_COMP_ID_CAMERA` to `MAV_COMP_ID_CAMERA6`.
pub const CAMERAS: u8 = 6;

pub const REQUEST_MESSAGE: u16 = crate::payload::wire::REQUEST_MESSAGE;
pub const VIDEO_START_CAPTURE: u16 = 2500;
pub const VIDEO_STOP_CAPTURE: u16 = 2501;

pub const CAMERA_INFORMATION: u32 = 259;
pub const STORAGE_INFORMATION: u32 = 261;
pub const CAMERA_CAPTURE_STATUS: u32 = 262;

/// `MAV_SEVERITY` for a `STATUSTEXT`.
pub const WARNING: u8 = 4;
pub const NOTICE: u8 = 5;
pub const INFO: u8 = 6;

/// Space on the recording's filesystem, in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Space {
    pub total: u64,
    pub free: u64,
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "MiB as the message's float, far inside its precision"
)]
fn mib(bytes: u64) -> f32 {
    (bytes as f64 / 1_048_576.0) as f32
}

/// Whether the autopilot of `system` is armed, from its heartbeat: component
/// 1 and an autopilot of any kind.
#[must_use]
pub fn armed(frame: &[u8], system: u8) -> Option<bool> {
    let decoded = crate::decode::decode(frame)?;
    if (decoded.system_id, decoded.component_id) != (system, 1) {
        return None;
    }
    match decoded.message {
        crate::decode::Message::Heartbeat {
            autopilot,
            base_mode,
            ..
        } if autopilot != "Invalid" => Some(base_mode & MavModeFlag::SAFETY_ARMED.bits() != 0),
        _ => None,
    }
}

/// The system id of a flight controller's heartbeat, any system's.
#[must_use]
pub fn autopilot(frame: &[u8]) -> Option<u8> {
    let decoded = crate::decode::decode(frame)?;
    match decoded.message {
        crate::decode::Message::Heartbeat { autopilot, .. }
            if decoded.component_id == 1 && autopilot != "Invalid" =>
        {
            Some(decoded.system_id)
        }
        _ => None,
    }
}

/// One camera component's frames, numbered in order.
#[derive(Clone, Debug)]
pub struct Camera {
    pub system: u8,
    pub component: u8,
    sequence: u8,
    /// The `id` of the last long `STATUSTEXT`, never 0.
    text_id: u16,
}

impl Camera {
    #[must_use]
    pub fn new(system: u8, component: u8) -> Self {
        Self {
            system,
            component,
            sequence: 0,
            text_id: 0,
        }
    }

    fn send(&mut self, message: &dyn mavio::Message) -> Vec<u8> {
        let frame = crate::sim::frame(self.system, self.component, self.sequence, message);
        self.sequence = self.sequence.wrapping_add(1);
        frame
    }

    pub fn heartbeat(&mut self) -> Vec<u8> {
        self.send(&Heartbeat {
            type_: MavType::Camera,
            autopilot: MavAutopilot::Invalid,
            base_mode: MavModeFlag::empty(),
            custom_mode: 0,
            system_status: MavState::Active,
            mavlink_version: 3,
        })
    }

    /// A camera that records video and nothing else, named by its vendor
    /// and model.
    pub fn information(&mut self, time_boot_ms: u32, vendor: &str, model: &str) -> Vec<u8> {
        self.send(&CameraInformation {
            time_boot_ms,
            vendor_name: text(vendor),
            model_name: text(model),
            focal_length: f32::NAN,
            sensor_size_h: f32::NAN,
            sensor_size_v: f32::NAN,
            flags: CameraCapFlags::CAPTURE_VIDEO,
            ..CameraInformation::default()
        })
    }

    pub fn storage(&mut self, time_boot_ms: u32, space: Space, name: &str) -> Vec<u8> {
        self.send(&StorageInformation {
            time_boot_ms,
            storage_id: 1,
            storage_count: 1,
            status: StorageStatus::Ready,
            total_capacity: mib(space.total),
            used_capacity: mib(space.total.saturating_sub(space.free)),
            available_capacity: mib(space.free),
            read_speed: f32::NAN,
            write_speed: f32::NAN,
            type_: StorageType::Other,
            name: text(name),
            storage_usage: StorageUsageFlag::SET | StorageUsageFlag::VIDEO,
        })
    }

    pub fn capture_status(
        &mut self,
        time_boot_ms: u32,
        recording_time_ms: Option<u32>,
        space: Space,
    ) -> Vec<u8> {
        self.send(&CameraCaptureStatus {
            time_boot_ms,
            image_status: 0,
            video_status: u8::from(recording_time_ms.is_some()),
            image_interval: 0.0,
            recording_time_ms: recording_time_ms.unwrap_or(0),
            available_capacity: mib(space.free),
            image_count: 0,
            camera_device_id: 0,
        })
    }

    pub fn ack(&mut self, command: &Command, result: u8) -> Vec<u8> {
        let frame = crate::sim::command_ack_frame(
            self.system,
            self.component,
            command.from,
            self.sequence,
            command.command,
            result,
        );
        self.sequence = self.sequence.wrapping_add(1);
        frame
    }

    /// A `STATUSTEXT`: one message, or for a text longer than its 50 bytes,
    /// 50-byte parts sharing an `id` and numbered by `chunk_seq`, as
    /// `MAVLink` 2 sends a long text, so nothing is cut. The last part is
    /// shorter than 50 bytes, an empty one when the text fills its parts, so
    /// the receiver knows it has the whole text.
    pub fn status_text(&mut self, severity: u8, said: &str) -> Vec<Vec<u8>> {
        const PART: usize = 50;
        let severity = MavSeverity::try_from(severity).unwrap_or(MavSeverity::Info);
        let bytes = said.as_bytes();
        if bytes.len() <= PART {
            return vec![self.send(&Statustext {
                severity,
                text: text(said),
                id: 0,
                chunk_seq: 0,
            })];
        }
        self.text_id = self.text_id.checked_add(1).unwrap_or(1);
        let id = self.text_id;
        let mut parts: Vec<&[u8]> = bytes.chunks(PART).collect();
        if bytes.len().is_multiple_of(PART) {
            parts.push(&[]);
        }
        parts
            .into_iter()
            .zip(0u8..)
            .map(|(part, chunk_seq)| {
                let mut text = [0u8; PART];
                text[..part.len()].copy_from_slice(part);
                self.send(&Statustext {
                    severity,
                    text,
                    id,
                    chunk_seq,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::{Message, decode};

    fn texts(frames: &[Vec<u8>]) -> Vec<(u8, String, u16, u8)> {
        frames
            .iter()
            .map(
                |frame| match decode(frame).expect("a frame that decodes").message {
                    Message::StatusText {
                        severity,
                        text,
                        id,
                        chunk_seq,
                        ..
                    } => (severity, text, id, chunk_seq),
                    other => panic!("not a STATUSTEXT: {other:?}"),
                },
            )
            .collect()
    }

    #[test]
    fn a_long_status_text_goes_whole_in_numbered_parts() {
        let mut camera = Camera::new(1, CAMERA);
        let said = "Deleted 450 MB of earlier recordings to keep recording, 160 MB free";
        let parts = texts(&camera.status_text(WARNING, said));
        assert_eq!(
            parts,
            vec![
                (WARNING, said[..50].to_owned(), 1, 0),
                (WARNING, said[50..].to_owned(), 1, 1),
            ]
        );
        assert_eq!(
            texts(&camera.status_text(WARNING, "Disk nearly full")),
            vec![(WARNING, "Disk nearly full".to_owned(), 0, 0)]
        );
        let fifty = "x".repeat(100);
        let parts = texts(&camera.status_text(INFO, &fifty));
        assert_eq!(
            parts.len(),
            3,
            "an empty last part ends a text that fills its parts"
        );
        assert!(parts.iter().all(|(_, _, id, _)| *id == 2));
        assert_eq!(parts[2].1, "");
    }
}
