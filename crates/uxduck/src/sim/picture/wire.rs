//! The messages of `/sim-camera` (S408), all binary, numbers big-endian.
//! The page's half is `apps/web/src/features/simCamera/protocol.ts`, and a
//! change here is a change there.
//!
//! To a window:
//!
//! - `DRAWS`: the cameras this window draws now, each named by the UDP port
//!   its picture is sent to. A count in a byte, then each port in two.
//!   None is a message too: it stops drawing.
//! - `VIEW`: what a camera it draws sees. The port, latitude and longitude
//!   in degrees as 64-bit floats, then as 32-bit floats its height above sea
//!   level and above home in metres, its attitude as a quaternion (w, x, y,
//!   z) from north, east and down, and the field of view across the picture
//!   in degrees. Then, only when one is set, flags in a byte: 1 the picture
//!   is the camera's thermal one, drawn in false colour. So a daylight
//!   picture's view is the 47 bytes it was before there were flags.
//!
//! From a window:
//!
//! - `ABLE`: what it can do. Its encoder (0 none, 1 software, 2 hardware),
//!   then flags: 1 it is visible, 2 it keeps drawing while hidden.
//! - `PICTURE`: one encoded picture of a camera it draws. The port, flags
//!   (1 a key picture), when it was drawn in microseconds since 1970, then
//!   the picture as an H.264 Annex B access unit.

use uxduck_mavlink::payload::View;

use super::{Able, Encoder};

pub const DRAWS: u8 = 1;
pub const VIEW: u8 = 2;
pub const ABLE: u8 = 16;
pub const PICTURE: u8 = 17;

/// A view's flag: the picture is the camera's thermal one.
const THERMAL: u8 = 1;

const VISIBLE: u8 = 1;
const BACKGROUND: u8 = 2;

/// The cameras a window draws now.
///
/// A window is told of at most 255, since their count is one byte: far
/// more than a computer simulates.
#[must_use]
pub fn draws(ports: &[u16]) -> Vec<u8> {
    let ports = &ports[..ports.len().min(255)];
    let mut message = Vec::with_capacity(2 + ports.len() * 2);
    #[allow(clippy::cast_possible_truncation, reason = "255 at most")]
    message.extend_from_slice(&[DRAWS, ports.len() as u8]);
    for port in ports {
        message.extend_from_slice(&port.to_be_bytes());
    }
    message
}

/// What a camera sees.
#[must_use]
pub fn view(port: u16, view: &View) -> Vec<u8> {
    let mut message = Vec::with_capacity(48);
    message.push(VIEW);
    message.extend_from_slice(&port.to_be_bytes());
    message.extend_from_slice(&(f64::from(view.place.lat) / 1e7).to_be_bytes());
    message.extend_from_slice(&(f64::from(view.place.lon) / 1e7).to_be_bytes());
    #[allow(
        clippy::cast_precision_loss,
        reason = "millimetres to metres, for drawing"
    )]
    for metres in [
        view.place.alt_mm as f32 / 1000.0,
        view.place.relative_alt_mm as f32 / 1000.0,
    ] {
        message.extend_from_slice(&metres.to_be_bytes());
    }
    for part in view.attitude {
        message.extend_from_slice(&part.to_be_bytes());
    }
    message.extend_from_slice(&view.hfov_deg.to_be_bytes());
    if view.thermal {
        message.push(THERMAL);
    }
    message
}

/// One encoded picture from the window that draws its camera.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Picture<'a> {
    pub port: u16,
    pub key: bool,
    /// When it was drawn: microseconds since 1970 on this computer's clock.
    pub captured_us: u64,
    /// An H.264 Annex B access unit.
    pub data: &'a [u8],
}

/// What a window says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FromWindow<'a> {
    Able(Able),
    Picture(Picture<'a>),
}

/// A window's message, or none for one that is not understood.
#[must_use]
pub fn read(message: &[u8]) -> Option<FromWindow<'_>> {
    match *message.first()? {
        ABLE => {
            let [_, encoder, flags] = *message else {
                return None;
            };
            Some(FromWindow::Able(Able {
                encoder: match encoder {
                    0 => Encoder::None,
                    1 => Encoder::Software,
                    2 => Encoder::Hardware,
                    _ => return None,
                },
                visible: flags & VISIBLE != 0,
                background: flags & BACKGROUND != 0,
            }))
        }
        PICTURE => {
            let (head, data) = message.split_at_checked(12)?;
            Some(FromWindow::Picture(Picture {
                port: u16::from_be_bytes([head[1], head[2]]),
                key: head[3] & 1 != 0,
                captured_us: u64::from_be_bytes(head[4..12].try_into().ok()?),
                data,
            }))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use uxduck_mavlink::payload::Place;

    use super::*;

    #[test]
    fn a_window_is_told_what_it_draws_and_what_each_camera_sees() {
        assert_eq!(draws(&[]), [DRAWS, 0]);
        assert_eq!(draws(&[15_600, 15_601]), [DRAWS, 2, 0x3c, 0xf0, 0x3c, 0xf1]);
        let seen = View {
            place: Place {
                lat: -353_632_610,
                lon: 1_491_652_300,
                alt_mm: 684_000,
                relative_alt_mm: 100_000,
            },
            attitude: [1.0, 0.0, 0.0, 0.0],
            hfov_deg: 44.8,
            thermal: false,
        };
        let message = view(15_600, &seen);
        assert_eq!(message.len(), 47);
        assert_eq!(message[..3], [VIEW, 0x3c, 0xf0]);
        let f64_at = |at: usize| f64::from_be_bytes(message[at..at + 8].try_into().unwrap());
        let f32_at = |at: usize| f32::from_be_bytes(message[at..at + 4].try_into().unwrap());
        assert!((f64_at(3) - -35.363_261).abs() < 1e-9);
        assert!((f64_at(11) - 149.165_23).abs() < 1e-9);
        assert!((f32_at(19) - 684.0).abs() < 1e-3);
        assert!((f32_at(23) - 100.0).abs() < 1e-3);
        assert!((f32_at(27) - 1.0).abs() < 1e-6);
        assert!((f32_at(43) - 44.8).abs() < 1e-4);
    }

    #[test]
    fn a_thermal_pictures_view_ends_with_its_flag_and_a_daylight_one_is_as_it_was() {
        let daylight = View {
            place: Place {
                lat: -353_632_610,
                lon: 1_491_652_300,
                alt_mm: 684_000,
                relative_alt_mm: 100_000,
            },
            attitude: [1.0, 0.0, 0.0, 0.0],
            hfov_deg: 44.8,
            thermal: false,
        };
        let plain = view(15_600, &daylight);
        let flagged = view(
            15_600,
            &View {
                thermal: true,
                ..daylight
            },
        );
        assert_eq!(plain.len(), 47);
        assert_eq!(flagged.len(), 48);
        assert_eq!(flagged[..47], plain[..]);
        assert_eq!(flagged[47], 1);
    }

    #[test]
    fn a_window_says_what_it_can_do_and_gives_its_pictures() {
        assert_eq!(
            read(&[ABLE, 2, 3]),
            Some(FromWindow::Able(Able {
                encoder: Encoder::Hardware,
                visible: true,
                background: true,
            }))
        );
        assert_eq!(
            read(&[ABLE, 0, 0]),
            Some(FromWindow::Able(Able {
                encoder: Encoder::None,
                visible: false,
                background: false,
            }))
        );
        let mut message = vec![PICTURE, 0x3c, 0xf0, 1];
        message.extend_from_slice(&1_800_000_000_123_456_u64.to_be_bytes());
        message.extend_from_slice(&[0, 0, 0, 1, 0x65, 9]);
        assert_eq!(
            read(&message),
            Some(FromWindow::Picture(Picture {
                port: 15_600,
                key: true,
                captured_us: 1_800_000_000_123_456,
                data: &[0, 0, 0, 1, 0x65, 9],
            }))
        );
        // Cut short, an encoder there is none of, a kind there is none of,
        // and nothing at all are not understood.
        for bad in [&message[..11], &[ABLE, 3, 0][..], &[ABLE, 1], &[9, 9], &[]] {
            assert_eq!(read(bad), None, "{bad:?}");
        }
    }
}
