//! Telemetry logs in the de facto tlog format: each frame's exact bytes,
//! prefixed with the receive time as 8 big-endian bytes of microseconds
//! since the Unix epoch. Mission Planner and pymavlink read these.

use std::io::Write;

/// Appends one frame.
///
/// # Errors
/// When the writer fails.
pub fn record(out: &mut impl Write, received_at_us: u64, frame: &[u8]) -> std::io::Result<()> {
    out.write_all(&received_at_us.to_be_bytes())?;
    out.write_all(frame)
}

/// Reads records back, for replay and tests. Stops at the first record that
/// is not a whole frame.
#[must_use]
pub fn read(bytes: &[u8]) -> Vec<(u64, &[u8])> {
    let mut records = Vec::new();
    let mut at = 0;
    while at + 8 < bytes.len() {
        let mut stamp = [0u8; 8];
        stamp.copy_from_slice(&bytes[at..at + 8]);
        let start = at + 8;
        let Some(len) = crate::framing::frame_len(&bytes[start..]) else {
            break;
        };
        if start + len > bytes.len() {
            break;
        }
        records.push((u64::from_be_bytes(stamp), &bytes[start..start + len]));
        at = start + len;
    }
    records
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim;

    #[test]
    fn frames_come_back_byte_for_byte() {
        let a = sim::heartbeat_frame(1, 1, 0);
        let b = sim::autopilot_version_frame(1, 1, 1, 42, 0);
        let mut log = Vec::new();
        record(&mut log, 1_000, &a).unwrap_or_default();
        record(&mut log, 2_000, &b).unwrap_or_default();
        assert_eq!(read(&log), [(1_000, a.as_slice()), (2_000, b.as_slice())]);
    }
}
