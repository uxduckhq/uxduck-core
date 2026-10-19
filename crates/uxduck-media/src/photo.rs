//! Photos from the video (ADR-0017 V19, S144): a JPEG the browser made of
//! the picture it drew, checked here and given EXIF before UXDuck keeps it.
//! The JPEG is never decoded: its header gives the size, and the EXIF goes
//! in as one more segment.

/// What a photo's EXIF says.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tags {
    /// When it was taken, in microseconds since the Unix epoch.
    pub taken_at_us: u64,
    /// The person's offset from UTC then, in minutes (+60 for UTC+1).
    pub utc_offset_minutes: i32,
    /// The camera's name, as the Camera tab shows it.
    pub camera: String,
    pub width: u32,
    pub height: u32,
    pub latitude_deg: Option<f64>,
    pub longitude_deg: Option<f64>,
    /// Above mean sea level.
    pub altitude_m: Option<f64>,
    /// Degrees from true north.
    pub heading_deg: Option<f64>,
}

/// A date and time without a zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Civil {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl Civil {
    /// The date and time `offset_minutes` from UTC at `us` since the epoch.
    #[must_use]
    pub fn at(us: u64, offset_minutes: i32) -> Self {
        let seconds =
            i64::try_from(us / 1_000_000).unwrap_or(i64::MAX) + i64::from(offset_minutes) * 60;
        let days = seconds.div_euclid(86_400);
        let of_day = seconds.rem_euclid(86_400);
        // Howard Hinnant's civil_from_days.
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + i64::from(month <= 2);
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "in range"
        )]
        Self {
            year,
            month: month as u32,
            day: day as u32,
            hour: (of_day / 3600) as u32,
            minute: (of_day % 3600 / 60) as u32,
            second: (of_day % 60) as u32,
        }
    }

    /// `2026:10:18 10:42:03`, as EXIF writes it.
    #[must_use]
    pub fn exif(&self) -> String {
        format!(
            "{:04}:{:02}:{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }

    /// `2026-10-18 10.42.03`, for a file name: no colons, which Finder and
    /// Explorer refuse or show as slashes.
    #[must_use]
    pub fn file_stem(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}.{:02}.{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

/// `+01:00`, as EXIF's offset tags write it.
#[must_use]
pub fn offset_text(minutes: i32) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    let m = minutes.unsigned_abs();
    format!("{sign}{:02}:{:02}", m / 60, m % 60)
}

/// A baseline or progressive JPEG's width and height, from its frame
/// header; `None` if it is not a JPEG.
#[must_use]
pub fn jpeg_size(jpeg: &[u8]) -> Option<(u32, u32)> {
    if !jpeg.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut at = 2;
    while at + 4 <= jpeg.len() {
        if jpeg[at] != 0xFF {
            return None;
        }
        let marker = jpeg[at + 1];
        // Fill bytes before a marker.
        if marker == 0xFF {
            at += 1;
            continue;
        }
        let length = usize::from(u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]));
        // SOF0 to SOF15, but not DHT (C4), JPG (C8) or DAC (CC).
        if (0xC0..=0xCF).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker) {
            let frame = jpeg.get(at + 4..at + 2 + length)?;
            let height = u32::from(u16::from_be_bytes([*frame.get(1)?, *frame.get(2)?]));
            let width = u32::from(u16::from_be_bytes([*frame.get(3)?, *frame.get(4)?]));
            return (width > 0 && height > 0).then_some((width, height));
        }
        // Start of scan before a frame header: not a JPEG we can read.
        if marker == 0xDA || length < 2 {
            return None;
        }
        at += 2 + length;
    }
    None
}

/// The JPEG with `tags` as its EXIF: one APP1 segment after the JFIF
/// header, any EXIF it had removed.
#[must_use]
pub fn with_exif(jpeg: &[u8], tags: &Tags) -> Vec<u8> {
    let tiff = tiff(tags);
    let mut app1 = Vec::with_capacity(tiff.len() + 10);
    app1.extend_from_slice(&[0xFF, 0xE1]);
    let length = u16::try_from(tiff.len() + 8).unwrap_or(u16::MAX);
    app1.extend_from_slice(&length.to_be_bytes());
    app1.extend_from_slice(b"Exif\0\0");
    app1.extend_from_slice(&tiff);

    let mut out = Vec::with_capacity(jpeg.len() + app1.len());
    out.extend_from_slice(&jpeg[..2]);
    let mut at = 2;
    let mut placed = false;
    // Keep APP0 (JFIF) first, drop any APP1 EXIF, then the rest unchanged.
    while at + 4 <= jpeg.len() && jpeg[at] == 0xFF {
        let marker = jpeg[at + 1];
        if !(0xE0..=0xEF).contains(&marker) {
            break;
        }
        let end = at + 2 + usize::from(u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]));
        if end > jpeg.len() {
            break;
        }
        let segment = &jpeg[at..end];
        if marker == 0xE0 {
            out.extend_from_slice(segment);
        } else if !(marker == 0xE1 && segment.get(4..10) == Some(b"Exif\0\0")) {
            if !placed {
                out.extend_from_slice(&app1);
                placed = true;
            }
            out.extend_from_slice(segment);
        }
        at = end;
    }
    if !placed {
        out.extend_from_slice(&app1);
    }
    out.extend_from_slice(&jpeg[at..]);
    out
}

const ASCII: u16 = 2;
const SHORT: u16 = 3;
const LONG: u16 = 4;
const RATIONAL: u16 = 5;
const BYTE: u16 = 1;

struct Entry {
    tag: u16,
    kind: u16,
    count: u32,
    data: Vec<u8>,
}

fn ascii(tag: u16, text: &str) -> Entry {
    let mut data: Vec<u8> = text.bytes().filter(|b| *b != 0).collect();
    data.push(0);
    Entry {
        tag,
        kind: ASCII,
        count: u32::try_from(data.len()).unwrap_or(u32::MAX),
        data,
    }
}

fn long(tag: u16, value: u32) -> Entry {
    Entry {
        tag,
        kind: LONG,
        count: 1,
        data: value.to_be_bytes().to_vec(),
    }
}

fn rationals(tag: u16, values: &[(u32, u32)]) -> Entry {
    let mut data = Vec::with_capacity(values.len() * 8);
    for (n, d) in values {
        data.extend_from_slice(&n.to_be_bytes());
        data.extend_from_slice(&d.to_be_bytes());
    }
    Entry {
        tag,
        kind: RATIONAL,
        count: u32::try_from(values.len()).unwrap_or(u32::MAX),
        data,
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "rounded, in range"
)]
fn hundredths(value: f64) -> (u32, u32) {
    (
        (value.abs() * 100.0).round().min(f64::from(u32::MAX)) as u32,
        100,
    )
}

/// Degrees, minutes and seconds to a ten-thousandth, as GPS tags write them.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "rounded, in range"
)]
fn dms(degrees: f64) -> [(u32, u32); 3] {
    let total = (degrees.abs() * 3600.0 * 10_000.0).round() as u64;
    let seconds = total % (60 * 10_000);
    let minutes = total / (60 * 10_000) % 60;
    let whole = total / (3600 * 10_000);
    [
        (whole as u32, 1),
        (minutes as u32, 1),
        (seconds as u32, 10_000),
    ]
}

/// An IFD and the values that do not fit in it, at `offset` in the TIFF.
fn ifd(mut entries: Vec<Entry>, offset: u32) -> Vec<u8> {
    entries.sort_by_key(|e| e.tag);
    let count = u32::try_from(entries.len()).unwrap_or(0);
    let mut data_at = offset + 2 + 12 * count + 4;
    let mut table = Vec::new();
    let mut data = Vec::new();
    table.extend_from_slice(&u16::try_from(count).unwrap_or(0).to_be_bytes());
    for entry in &entries {
        table.extend_from_slice(&entry.tag.to_be_bytes());
        table.extend_from_slice(&entry.kind.to_be_bytes());
        table.extend_from_slice(&entry.count.to_be_bytes());
        if entry.data.len() <= 4 {
            let mut inline = [0u8; 4];
            inline[..entry.data.len()].copy_from_slice(&entry.data);
            table.extend_from_slice(&inline);
        } else {
            table.extend_from_slice(&data_at.to_be_bytes());
            data.extend_from_slice(&entry.data);
            if entry.data.len() % 2 == 1 {
                data.push(0);
            }
            data_at = offset + 2 + 12 * count + 4 + u32::try_from(data.len()).unwrap_or(0);
        }
    }
    // No next IFD.
    table.extend_from_slice(&0u32.to_be_bytes());
    table.extend_from_slice(&data);
    table
}

/// The size an IFD of these entries takes.
fn ifd_len(entries: &[Entry]) -> u32 {
    let data: usize = entries
        .iter()
        .filter(|e| e.data.len() > 4)
        .map(|e| e.data.len() + e.data.len() % 2)
        .sum();
    u32::try_from(2 + 12 * entries.len() + 4 + data).unwrap_or(0)
}

/// A big-endian TIFF: IFD0, the EXIF IFD and, with a position, the GPS IFD.
fn tiff(tags: &Tags) -> Vec<u8> {
    let local = Civil::at(tags.taken_at_us, tags.utc_offset_minutes).exif();
    let offset = offset_text(tags.utc_offset_minutes);

    let exif = vec![
        ascii(0x9003, &local),
        ascii(0x9004, &local),
        ascii(0x9010, &offset),
        ascii(0x9011, &offset),
        long(0xA002, tags.width),
        long(0xA003, tags.height),
    ];

    let gps = match (tags.latitude_deg, tags.longitude_deg) {
        (Some(lat), Some(lon)) if lat.is_finite() && lon.is_finite() => {
            let utc = Civil::at(tags.taken_at_us, 0);
            let mut gps = vec![
                Entry {
                    tag: 0x0000,
                    kind: BYTE,
                    count: 4,
                    data: vec![2, 3, 0, 0],
                },
                ascii(0x0001, if lat < 0.0 { "S" } else { "N" }),
                rationals(0x0002, &dms(lat)),
                ascii(0x0003, if lon < 0.0 { "W" } else { "E" }),
                rationals(0x0004, &dms(lon)),
                rationals(0x0007, &[(utc.hour, 1), (utc.minute, 1), (utc.second, 1)]),
                ascii(
                    0x001D,
                    &format!("{:04}:{:02}:{:02}", utc.year, utc.month, utc.day),
                ),
            ];
            if let Some(alt) = tags.altitude_m.filter(|a| a.is_finite()) {
                gps.push(Entry {
                    tag: 0x0005,
                    kind: BYTE,
                    count: 1,
                    data: vec![u8::from(alt < 0.0)],
                });
                gps.push(rationals(0x0006, &[hundredths(alt)]));
            }
            if let Some(heading) = tags.heading_deg.filter(|h| h.is_finite()) {
                gps.push(ascii(0x0010, "T"));
                gps.push(rationals(0x0011, &[hundredths(heading.rem_euclid(360.0))]));
            }
            Some(gps)
        }
        _ => None,
    };

    let mut ifd0 = vec![
        ascii(0x010E, &tags.camera),
        ascii(0x0131, "UXDuck"),
        ascii(0x0132, &local),
        Entry {
            tag: 0x0112,
            kind: SHORT,
            count: 1,
            data: 1u16.to_be_bytes().to_vec(),
        },
    ];
    // Pointers are placeholders here, for the length; set below.
    ifd0.push(long(0x8769, 0));
    if gps.is_some() {
        ifd0.push(long(0x8825, 0));
    }
    let ifd0_at = 8u32;
    let exif_at = ifd0_at + ifd_len(&ifd0);
    let gps_at = exif_at + ifd_len(&exif);
    for entry in &mut ifd0 {
        match entry.tag {
            0x8769 => entry.data = exif_at.to_be_bytes().to_vec(),
            0x8825 => entry.data = gps_at.to_be_bytes().to_vec(),
            _ => {}
        }
    }

    let mut out = Vec::new();
    out.extend_from_slice(b"MM\0\x2A");
    out.extend_from_slice(&ifd0_at.to_be_bytes());
    out.extend_from_slice(&ifd(ifd0, ifd0_at));
    out.extend_from_slice(&ifd(exif, exif_at));
    if let Some(gps) = gps {
        out.extend_from_slice(&ifd(gps, gps_at));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A JPEG's header: SOI, JFIF APP0, an SOF0 for `w`×`h`, then EOI.
    fn jpeg(w: u16, h: u16) -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8];
        out.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x10]);
        out.extend_from_slice(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        out.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        out.extend_from_slice(&h.to_be_bytes());
        out.extend_from_slice(&w.to_be_bytes());
        out.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        out.extend_from_slice(&[0xFF, 0xD9]);
        out
    }

    fn u16_at(b: &[u8], at: usize) -> u16 {
        u16::from_be_bytes([b[at], b[at + 1]])
    }

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
    }

    /// The entries of the IFD at `at` in the TIFF: tag, type, count and
    /// the value or offset.
    fn entries(tiff: &[u8], at: usize) -> Vec<(u16, u16, u32, u32)> {
        let n = usize::from(u16_at(tiff, at));
        (0..n)
            .map(|i| {
                let e = at + 2 + 12 * i;
                (
                    u16_at(tiff, e),
                    u16_at(tiff, e + 2),
                    u32_at(tiff, e + 4),
                    u32_at(tiff, e + 8),
                )
            })
            .collect()
    }

    fn text(tiff: &[u8], entry: (u16, u16, u32, u32)) -> String {
        let (_, _, count, value) = entry;
        let bytes = if count <= 4 {
            value.to_be_bytes()[..count as usize].to_vec()
        } else {
            tiff[value as usize..(value + count) as usize].to_vec()
        };
        String::from_utf8(bytes)
            .unwrap()
            .trim_end_matches('\0')
            .to_owned()
    }

    fn find(list: &[(u16, u16, u32, u32)], tag: u16) -> (u16, u16, u32, u32) {
        *list.iter().find(|e| e.0 == tag).unwrap()
    }

    #[test]
    fn a_jpeg_says_its_size_and_anything_else_is_refused() {
        assert_eq!(jpeg_size(&jpeg(1280, 720)), Some((1280, 720)));
        assert_eq!(jpeg_size(b"\x89PNG\r\n\x1a\n"), None);
        assert_eq!(jpeg_size(&[0xFF, 0xD8, 0xFF, 0xDA, 0, 2]), None);
        assert_eq!(jpeg_size(&[]), None);
    }

    #[test]
    fn local_time_and_file_names_come_from_the_offset() {
        // 2026-10-18 09:42:03 UTC.
        let us = 1_792_316_523_000_000;
        assert_eq!(Civil::at(us, 0).exif(), "2026:10:18 09:42:03");
        assert_eq!(Civil::at(us, 60).file_stem(), "2026-10-18 10.42.03");
        assert_eq!(Civil::at(us, -600).exif(), "2026:10:17 23:42:03");
        assert_eq!(offset_text(60), "+01:00");
        assert_eq!(offset_text(-570), "-09:30");
        assert_eq!(
            Civil::at(951_782_400_000_000, 0).exif(),
            "2000:02:29 00:00:00"
        );
    }

    #[test]
    fn exif_goes_after_jfif_with_time_size_camera_and_position() {
        let tags = Tags {
            taken_at_us: 1_792_316_523_000_000,
            utc_offset_minutes: 60,
            camera: "Alpha front".into(),
            width: 1280,
            height: 720,
            latitude_deg: Some(-35.363_261),
            longitude_deg: Some(149.165_230),
            altitude_m: Some(584.2),
            heading_deg: Some(-10.0),
        };
        let original = jpeg(1280, 720);
        let out = with_exif(&original, &tags);
        // SOI, then JFIF kept first, then the EXIF.
        assert_eq!(&out[..4], &[0xFF, 0xD8, 0xFF, 0xE0]);
        let app1 = 2 + 2 + 0x10;
        assert_eq!(&out[app1..app1 + 2], &[0xFF, 0xE1]);
        assert_eq!(&out[app1 + 4..app1 + 10], b"Exif\0\0");
        // Still a JPEG of the same size, ending as it did.
        assert_eq!(jpeg_size(&out), Some((1280, 720)));
        assert!(out.ends_with(&[0xFF, 0xD9]));

        let tiff = &out[app1 + 10..];
        assert_eq!(&tiff[..4], b"MM\0\x2A");
        let ifd0 = entries(tiff, u32_at(tiff, 4) as usize);
        let tags0: Vec<u16> = ifd0.iter().map(|e| e.0).collect();
        let mut sorted = tags0.clone();
        sorted.sort_unstable();
        assert_eq!(tags0, sorted, "IFD entries go in tag order");
        assert_eq!(text(tiff, find(&ifd0, 0x010E)), "Alpha front");
        assert_eq!(text(tiff, find(&ifd0, 0x0132)), "2026:10:18 10:42:03");

        let exif = entries(tiff, find(&ifd0, 0x8769).3 as usize);
        assert_eq!(text(tiff, find(&exif, 0x9003)), "2026:10:18 10:42:03");
        assert_eq!(text(tiff, find(&exif, 0x9011)), "+01:00");
        assert_eq!(find(&exif, 0xA002).3, 1280);
        assert_eq!(find(&exif, 0xA003).3, 720);

        let gps = entries(tiff, find(&ifd0, 0x8825).3 as usize);
        assert_eq!(text(tiff, find(&gps, 0x0001)), "S");
        assert_eq!(text(tiff, find(&gps, 0x0003)), "E");
        let lat = find(&gps, 0x0002).3 as usize;
        let r = |at: usize| f64::from(u32_at(tiff, at)) / f64::from(u32_at(tiff, at + 4));
        let degrees = r(lat) + r(lat + 8) / 60.0 + r(lat + 16) / 3600.0;
        assert!((degrees - 35.363_261).abs() < 1e-6, "{degrees}");
        assert!((r(find(&gps, 0x0006).3 as usize) - 584.2).abs() < 1e-9);
        assert!((r(find(&gps, 0x0011).3 as usize) - 350.0).abs() < 1e-9);
        assert_eq!(text(tiff, find(&gps, 0x001D)), "2026:10:18");
    }

    #[test]
    fn without_a_position_there_is_no_gps_and_old_exif_is_replaced() {
        let tags = Tags {
            taken_at_us: 0,
            camera: "udp://127.0.0.1:5600".into(),
            width: 640,
            height: 480,
            ..Tags::default()
        };
        let once = with_exif(&jpeg(640, 480), &tags);
        let twice = with_exif(&once, &tags);
        assert_eq!(
            once, twice,
            "an EXIF already there is replaced, not added to"
        );
        let app1 = 2 + 2 + 0x10;
        let tiff = &once[app1 + 10..];
        let ifd0 = entries(tiff, 8);
        assert!(ifd0.iter().all(|e| e.0 != 0x8825));
    }
}
