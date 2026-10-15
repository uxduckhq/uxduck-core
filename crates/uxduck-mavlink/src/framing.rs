//! Finds MAVLink frames in bytes without decoding them, so each frame's
//! exact bytes can be recorded and forwarded (byte-preserving, ADR-0004).

/// MAVLink 1 start byte.
pub const STX_V1: u8 = 0xFE;
/// MAVLink 2 start byte.
pub const STX_V2: u8 = 0xFD;

/// What a datagram held.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Split<'a> {
    /// Each frame's exact bytes, in order.
    pub frames: Vec<&'a [u8]>,
    /// Bytes that were not part of a complete frame.
    pub junk: usize,
}

/// The length of the frame starting at `bytes[0]`, if the header says so.
#[must_use]
pub fn frame_len(bytes: &[u8]) -> Option<usize> {
    match *bytes.first()? {
        STX_V1 => Some(6 + usize::from(*bytes.get(1)?) + 2),
        STX_V2 => {
            let signed = bytes.get(2)? & 0x01 != 0;
            Some(10 + usize::from(*bytes.get(1)?) + 2 + if signed { 13 } else { 0 })
        }
        _ => None,
    }
}

/// Splits bytes into whole frames. A frame cut short at the end counts as
/// junk; UDP datagrams carry whole frames, so this does not buffer.
#[must_use]
pub fn split(bytes: &[u8]) -> Split<'_> {
    let mut split = Split::default();
    let mut at = 0;
    while at < bytes.len() {
        match frame_len(&bytes[at..]) {
            Some(len) if at + len <= bytes.len() => {
                split.frames.push(&bytes[at..at + len]);
                at += len;
            }
            Some(_) => {
                split.junk += bytes.len() - at;
                break;
            }
            None => {
                split.junk += 1;
                at += 1;
            }
        }
    }
    split
}

/// The routing fields of a frame's header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub sequence: u8,
    pub system_id: u8,
    pub component_id: u8,
    pub message_id: u32,
}

/// Reads the header of a whole frame.
#[must_use]
pub fn header(frame: &[u8]) -> Option<Header> {
    match *frame.first()? {
        STX_V1 => Some(Header {
            sequence: *frame.get(2)?,
            system_id: *frame.get(3)?,
            component_id: *frame.get(4)?,
            message_id: u32::from(*frame.get(5)?),
        }),
        STX_V2 => Some(Header {
            sequence: *frame.get(4)?,
            system_id: *frame.get(5)?,
            component_id: *frame.get(6)?,
            message_id: u32::from(*frame.get(7)?)
                | u32::from(*frame.get(8)?) << 8
                | u32::from(*frame.get(9)?) << 16,
        }),
        _ => None,
    }
}

/// A whole frame's payload, as sent: MAVLink 2 drops trailing zeros.
#[must_use]
pub fn payload(frame: &[u8]) -> Option<&[u8]> {
    let (start, len) = match *frame.first()? {
        STX_V1 => (6, usize::from(*frame.get(1)?)),
        STX_V2 => (10, usize::from(*frame.get(1)?)),
        _ => return None,
    };
    frame.get(start..start + len)
}

/// What a candidate frame's checksum says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    Valid,
    Invalid,
    /// A message the dialect does not know, so its checksum cannot be checked.
    Unknown,
}

/// Frames from a byte stream (serial, TCP), where frames arrive in pieces and
/// noise can look like a start byte. Keeps what is left over for the next
/// read, and resynchronises one byte at a time past anything that is not a
/// frame.
#[derive(Debug, Default)]
pub struct Reassembler {
    buffer: Vec<u8>,
    /// Bytes skipped while resynchronising.
    pub junk: u64,
}

/// A stream that never frames is dropped past this many buffered bytes.
const MAX_BUFFER: usize = 64 * 1024;

impl Reassembler {
    /// Adds bytes and returns every whole frame now available. `check` tells
    /// a real frame from noise; a frame of a message the dialect does not
    /// know is accepted when another frame starts right after it.
    pub fn push(&mut self, bytes: &[u8], check: impl Fn(&[u8]) -> Check) -> Vec<Vec<u8>> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();
        let mut at = 0;
        while at < self.buffer.len() {
            let rest = &self.buffer[at..];
            let Some(len) = frame_len(rest) else {
                if rest.len() < 3 && matches!(rest[0], STX_V1 | STX_V2) {
                    break; // A start byte waiting for its length.
                }
                at += 1;
                self.junk += 1;
                continue;
            };
            if rest.len() < len {
                break; // Wait for the rest of the frame.
            }
            let accept = match check(&rest[..len]) {
                Check::Valid => true,
                Check::Invalid => false,
                Check::Unknown => rest
                    .get(len)
                    .is_none_or(|next| matches!(*next, STX_V1 | STX_V2)),
            };
            if accept {
                frames.push(rest[..len].to_vec());
                at += len;
            } else {
                at += 1;
                self.junk += 1;
            }
        }
        self.buffer.drain(..at);
        if self.buffer.len() > MAX_BUFFER {
            self.junk += self.buffer.len() as u64;
            self.buffer.clear();
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim;

    #[test]
    fn finds_every_frame_and_counts_junk() {
        let a = sim::heartbeat_frame(1, 1, 0);
        let b = sim::heartbeat_frame(2, 1, 1);
        let mut datagram = vec![0x00, 0x42];
        datagram.extend_from_slice(&a);
        datagram.extend_from_slice(&b);
        datagram.extend_from_slice(&a[..5]);
        let split = split(&datagram);
        assert_eq!(split.frames, [a.as_slice(), b.as_slice()]);
        assert_eq!(split.junk, 2 + 5);
    }

    #[test]
    fn headers_carry_the_routing_fields() {
        let frame = sim::heartbeat_frame(7, 3, 42);
        assert_eq!(
            header(&frame),
            Some(Header {
                sequence: 42,
                system_id: 7,
                component_id: 3,
                message_id: 0
            })
        );
    }

    #[test]
    fn a_stream_in_pieces_with_noise_gives_the_same_frames() {
        let a = sim::heartbeat_frame(1, 1, 0);
        let b = sim::autopilot_version_frame(1, 1, 1, 0xAB, 0x0405_01ff);
        // Noise that includes a false start byte announcing a long frame.
        let mut stream = vec![0x00, STX_V2, 0x10, 0x00];
        stream.extend_from_slice(&a);
        stream.extend_from_slice(&b);
        stream.extend_from_slice(&a);
        let mut reassembler = Reassembler::default();
        let mut frames = Vec::new();
        for piece in stream.chunks(7) {
            frames.extend(reassembler.push(piece, crate::decode::check));
        }
        assert_eq!(frames, [a.clone(), b, a]);
        assert_eq!(reassembler.junk, 4);
    }

    #[test]
    fn a_v1_frame_has_its_own_length() {
        let mut frame = vec![STX_V1, 3, 0, 1, 1, 0, 9, 9, 9, 0xAB, 0xCD];
        assert_eq!(frame_len(&frame), Some(11));
        frame.push(0);
        assert_eq!(split(&frame).frames.len(), 1);
    }
}

#[cfg(test)]
mod payload_tests {
    #[test]
    fn a_payload_is_found_in_both_versions() {
        let frame = crate::sim::command_ack_frame(1, 1, (255, 190), 0, 2000, 0);
        let payload = super::payload(&frame).unwrap();
        assert_eq!(&payload[..3], &[0xD0, 0x07, 0], "command 2000, accepted");
    }
}
