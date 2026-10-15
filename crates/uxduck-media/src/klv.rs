//! Enough MISB ST 0601 (UAS Datalink Local Set) to find it, check its
//! checksum and read a few values (ADR-0017 V2; S41 reads the rest).
//! Synchronous KLV puts a five-byte metadata AU cell header first; the
//! experiment's camera fills it with `0xFF`, claiming 65,286 bytes, so the
//! key is searched for instead of trusting the header.

use std::collections::BTreeMap;

const UAS_LS_KEY: [u8; 16] = [
    0x06, 0x0e, 0x2b, 0x34, 0x02, 0x0b, 0x01, 0x01, 0x0e, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00,
];

#[derive(Clone, Debug, PartialEq)]
pub struct Klv {
    pub checksum_ok: bool,
    /// Tag number to raw value.
    pub tags: BTreeMap<u32, Vec<u8>>,
    pub values: Values,
}

/// What a local set says, in degrees, metres and microseconds (MISB ST 0601
/// tags 2 to 89). A field the packet lacks, or marks as an error, is `None`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Values {
    /// Microseconds since 1970 (Precision Time Stamp, tag 2).
    pub timestamp_us: Option<u64>,
    pub mission_id: Option<String>,
    pub platform_heading_deg: Option<f64>,
    pub platform_pitch_deg: Option<f64>,
    pub platform_roll_deg: Option<f64>,
    /// Such as `MQ-9` (tag 10): evidence for which vehicle it is.
    pub platform_designation: Option<String>,
    pub image_source: Option<String>,
    pub sensor_latitude_deg: Option<f64>,
    pub sensor_longitude_deg: Option<f64>,
    pub sensor_altitude_m: Option<f64>,
    pub horizontal_fov_deg: Option<f64>,
    pub vertical_fov_deg: Option<f64>,
    /// The sensor's pointing relative to the platform.
    pub sensor_azimuth_deg: Option<f64>,
    pub sensor_elevation_deg: Option<f64>,
    pub sensor_roll_deg: Option<f64>,
    pub slant_range_m: Option<f64>,
    /// Where the middle of the picture is on the ground.
    pub frame_latitude_deg: Option<f64>,
    pub frame_longitude_deg: Option<f64>,
    pub frame_elevation_m: Option<f64>,
    /// The picture's corners on the ground, as (latitude, longitude):
    /// whole (tags 82 to 89), or as offsets from the frame centre (26 to 33).
    pub corners: Option<[(f64, f64); 4]>,
}

/// Where the local set's key starts, within the first 64 bytes.
#[must_use]
pub fn find_uas_local_set(data: &[u8]) -> Option<usize> {
    (0..64.min(data.len())).find(|&i| data.get(i..i + UAS_LS_KEY.len()) == Some(&UAS_LS_KEY[..]))
}

fn ber_length(data: &[u8], at: usize) -> Option<(usize, usize)> {
    let first = *data.get(at)?;
    if first < 0x80 {
        return Some((usize::from(first), 1));
    }
    let count = usize::from(first & 0x7f);
    let mut length = 0usize;
    for i in 1..=count {
        length = length.checked_mul(256)? + usize::from(*data.get(at + i)?);
    }
    Some((length, 1 + count))
}

fn ber_oid(data: &[u8], at: usize) -> Option<(u32, usize)> {
    let mut value = 0u32;
    let mut size = 0;
    loop {
        let byte = *data.get(at + size)?;
        size += 1;
        value = value * 128 + u32::from(byte & 0x7f);
        if byte & 0x80 == 0 || size >= 4 {
            return Some((value, size));
        }
    }
}

/// The local set in a KLV PES payload, if there is one.
#[must_use]
pub fn parse(payload: &[u8]) -> Option<Klv> {
    let start = find_uas_local_set(payload)?;
    let (length, length_size) = ber_length(payload, start + 16)?;
    let value_start = start + 16 + length_size;
    let end = payload.len().min(value_start.saturating_add(length));
    let mut tags = BTreeMap::new();
    let mut checksum_at = None;
    let mut i = value_start;
    while i < end {
        let Some((tag, tag_size)) = ber_oid(payload, i) else {
            break;
        };
        let Some((size, size_bytes)) = ber_length(payload, i + tag_size) else {
            break;
        };
        let from = i + tag_size + size_bytes;
        let value = payload
            .get(from..(from + size).min(payload.len()))
            .unwrap_or(&[]);
        if tag == 1 {
            checksum_at = Some(from);
        }
        tags.insert(tag, value.to_vec());
        i = from + size;
    }
    // The checksum sums 16-bit words from the key up to its own length byte.
    let checksum_ok = checksum_at.is_some_and(|at| {
        at + 2 <= payload.len() && {
            let mut sum: u16 = 0;
            for (k, &byte) in payload[start..at].iter().enumerate() {
                let shift = if (k + 1) % 2 == 1 { 8 } else { 0 };
                sum = sum.wrapping_add(u16::from(byte) << shift);
            }
            sum == (u16::from(payload[at]) << 8 | u16::from(payload[at + 1]))
        }
    });
    let values = values(&tags);
    Some(Klv {
        checksum_ok,
        tags,
        values,
    })
}

/// MISB's scaled integers: unsigned to `0..range`, offset by `low`.
fn scaled_unsigned(v: &[u8], range: f64, low: f64) -> f64 {
    let max = (1u128 << (8 * v.len())) - 1;
    #[allow(clippy::cast_precision_loss, reason = "MISB's integers fit f64")]
    let (n, max) = (unsigned(v) as f64, max as f64);
    n / max * range + low
}

/// Signed to `-range..range`; the most negative value means an error.
fn scaled_signed(v: &[u8], range: f64) -> Option<f64> {
    let bits = 8 * v.len();
    let n = signed(v);
    if n == -(1i64 << (bits - 1)) {
        return None;
    }
    #[allow(clippy::cast_precision_loss, reason = "MISB's integers fit f64")]
    let max = ((1i64 << (bits - 1)) - 1) as f64;
    #[allow(clippy::cast_precision_loss, reason = "MISB's integers fit f64")]
    Some(n as f64 / max * range)
}

fn unsigned(v: &[u8]) -> u64 {
    v.iter().fold(0u64, |n, &b| (n << 8) | u64::from(b))
}

fn signed(v: &[u8]) -> i64 {
    let u = unsigned(v);
    if v.first().is_some_and(|b| b & 0x80 != 0) {
        u.cast_signed() - (1i64 << (8 * v.len()))
    } else {
        u.cast_signed()
    }
}

fn values(tags: &BTreeMap<u32, Vec<u8>>) -> Values {
    let text = |v: &[u8]| String::from_utf8_lossy(v).trim_end_matches('\0').to_owned();
    let get = |tag: u32, size: usize| {
        tags.get(&tag)
            .filter(|v| v.len() == size)
            .map(Vec::as_slice)
    };
    let angle = |tag: u32| get(tag, 2).map(|v| scaled_unsigned(v, 180.0, 0.0));
    let altitude = |tag: u32| get(tag, 2).map(|v| scaled_unsigned(v, 19_900.0, -900.0));
    let latitude = |tag: u32| get(tag, 4).and_then(|v| scaled_signed(v, 90.0));
    let longitude = |tag: u32| get(tag, 4).and_then(|v| scaled_signed(v, 180.0));
    let offset = |tag: u32| get(tag, 2).and_then(|v| scaled_signed(v, 0.075));
    let frame = (latitude(23), longitude(24));
    let whole = [(82, 83), (84, 85), (86, 87), (88, 89)]
        .map(|(lat, lon)| latitude(lat).zip(longitude(lon)));
    let offsets = [(26, 27), (28, 29), (30, 31), (32, 33)].map(|(lat, lon)| {
        let (Some(flat), Some(flon)) = frame else {
            return None;
        };
        offset(lat)
            .zip(offset(lon))
            .map(|(a, b)| (flat + a, flon + b))
    });
    let all = |c: [Option<(f64, f64)>; 4]| {
        c.iter()
            .all(Option::is_some)
            .then(|| c.map(Option::unwrap_or_default))
    };
    Values {
        timestamp_us: get(2, 8).map(unsigned),
        mission_id: tags.get(&3).map(|v| text(v)),
        platform_heading_deg: get(5, 2).map(|v| scaled_unsigned(v, 360.0, 0.0)),
        platform_pitch_deg: get(6, 2).and_then(|v| scaled_signed(v, 20.0)),
        platform_roll_deg: get(7, 2).and_then(|v| scaled_signed(v, 50.0)),
        platform_designation: tags.get(&10).map(|v| text(v)),
        image_source: tags.get(&11).map(|v| text(v)),
        sensor_latitude_deg: latitude(13),
        sensor_longitude_deg: longitude(14),
        sensor_altitude_m: altitude(15),
        horizontal_fov_deg: angle(16),
        vertical_fov_deg: angle(17),
        sensor_azimuth_deg: get(18, 4).map(|v| scaled_unsigned(v, 360.0, 0.0)),
        sensor_elevation_deg: get(19, 4).and_then(|v| scaled_signed(v, 180.0)),
        sensor_roll_deg: get(20, 4).map(|v| scaled_unsigned(v, 360.0, 0.0)),
        slant_range_m: get(21, 4).map(|v| scaled_unsigned(v, 5_000_000.0, 0.0)),
        frame_latitude_deg: frame.0,
        frame_longitude_deg: frame.1,
        frame_elevation_m: altitude(25),
        corners: all(whole).or_else(|| all(offsets)),
    }
}

/// A local set with these tags and a correct checksum, as a camera writes
/// it: for tests and `uxduck dev` streams.
#[must_use]
pub fn encode(tags: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (tag, value) in tags {
        body.push(*tag);
        #[allow(clippy::cast_possible_truncation, reason = "short values")]
        body.push(value.len() as u8);
        body.extend_from_slice(value);
    }
    // The checksum's own tag and length, then its two bytes.
    body.extend_from_slice(&[1, 2]);
    let mut out = UAS_LS_KEY.to_vec();
    let length = body.len() + 2;
    if length < 0x80 {
        #[allow(clippy::cast_possible_truncation, reason = "below 128")]
        out.push(length as u8);
    } else {
        #[allow(clippy::cast_possible_truncation, reason = "a local set is short")]
        out.extend_from_slice(&[0x82, (length >> 8) as u8, length as u8]);
    }
    out.extend_from_slice(&body);
    let mut sum: u16 = 0;
    for (k, &byte) in out.iter().enumerate() {
        let shift = if (k + 1) % 2 == 1 { 8 } else { 0 };
        sum = sum.wrapping_add(u16::from(byte) << shift);
    }
    out.extend_from_slice(&sum.to_be_bytes());
    out
}

/// A degree value as MISB's signed integer of `bytes` bytes over `range`.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "MISB's integers"
)]
pub fn to_signed(value: f64, range: f64, bytes: usize) -> Vec<u8> {
    let max = ((1i64 << (8 * bytes - 1)) - 1) as f64;
    let n = (value / range * max).round() as i64;
    n.to_be_bytes()[8 - bytes..].to_vec()
}
