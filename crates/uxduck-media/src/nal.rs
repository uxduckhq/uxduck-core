//! Annex B for H.264 and H.265: splitting, header checks, parameter set
//! identities and the WebCodecs codec string.

use crate::bits::{BitReader, OutOfBits, unescape_rbsp};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
    H265,
}

/// Splits an Annex B byte stream into NAL units without their start codes.
#[must_use]
pub fn split_annex_b(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(starts.len());
    for (k, &start) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map_or(data.len(), |next| next - 3);
        // Trailing zeros belong to the next start code or are padding.
        while end > start && data[end - 1] == 0 {
            end -= 1;
        }
        if end > start {
            nals.push(&data[start..end]);
        }
    }
    nals
}

pub mod h265 {
    pub const RASL_N: u8 = 8;
    pub const RASL_R: u8 = 9;
    pub const CRA: u8 = 21;
    pub const VPS: u8 = 32;
    pub const SPS: u8 = 33;
    pub const PPS: u8 = 34;
    pub const AUD: u8 = 35;
    pub const FD: u8 = 38;
    pub const SEI_PREFIX: u8 = 39;
    pub const SEI_SUFFIX: u8 = 40;
}

pub mod h264 {
    pub const IDR: u8 = 5;
    pub const SEI: u8 = 6;
    pub const SPS: u8 = 7;
    pub const PPS: u8 = 8;
    pub const AUD: u8 = 9;
    pub const FD: u8 = 12;
}

/// A NAL unit's type, and why it must not reach a decoder if it must not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// `None` when the unit is too short to have one.
    pub kind: Option<u8>,
    pub invalid: Option<&'static str>,
}

#[must_use]
pub fn parse_header(codec: Codec, nal: &[u8]) -> Header {
    let bad = |kind, why| Header {
        kind,
        invalid: Some(why),
    };
    match codec {
        Codec::H265 => {
            if nal.len() < 3 {
                return bad(None, "short");
            }
            let kind = (nal[0] >> 1) & 0x3f;
            let layer = ((nal[0] & 0x01) << 5) | (nal[1] >> 3);
            let temporal_id_plus1 = nal[1] & 0x07;
            if nal[0] & 0x80 != 0 {
                return bad(Some(kind), "forbidden bit");
            }
            if layer != 0 {
                return bad(Some(kind), "layer id");
            }
            if temporal_id_plus1 == 0 {
                return bad(Some(kind), "temporal id");
            }
            // 10-15, 22-31 reserved VCL; 41-47 reserved; 48-63 unspecified.
            if (10..=15).contains(&kind) || (22..=31).contains(&kind) || kind >= 41 {
                return bad(Some(kind), "reserved type");
            }
            Header {
                kind: Some(kind),
                invalid: None,
            }
        }
        Codec::H264 => {
            if nal.len() < 2 {
                return bad(None, "short");
            }
            let kind = nal[0] & 0x1f;
            if nal[0] & 0x80 != 0 {
                return bad(Some(kind), "forbidden bit");
            }
            // 0 and 24-31 unspecified, 16-18 and 22-23 reserved; 14, 15 and 20
            // are SVC/MVC extensions a base-layer decoder does not need.
            if kind == 0 || kind >= 22 || (14..=18).contains(&kind) || kind == 20 {
                return bad(Some(kind), "reserved type");
            }
            Header {
                kind: Some(kind),
                invalid: None,
            }
        }
    }
}

#[must_use]
pub fn is_vcl(codec: Codec, kind: u8) -> bool {
    match codec {
        Codec::H265 => kind < 32,
        Codec::H264 => (1..=5).contains(&kind),
    }
}

/// A picture a decoder can start from.
#[must_use]
pub fn is_keyframe(codec: Codec, kind: u8) -> bool {
    match codec {
        Codec::H265 => (16..=21).contains(&kind),
        Codec::H264 => kind == h264::IDR,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ParameterSet {
    Vps,
    Sps,
    Pps,
}

#[must_use]
pub fn parameter_set(codec: Codec, kind: u8) -> Option<ParameterSet> {
    match (codec, kind) {
        (Codec::H265, h265::VPS) => Some(ParameterSet::Vps),
        (Codec::H265, h265::SPS) | (Codec::H264, h264::SPS) => Some(ParameterSet::Sps),
        (Codec::H265, h265::PPS) | (Codec::H264, h264::PPS) => Some(ParameterSet::Pps),
        _ => None,
    }
}

/// The id a parameter set is stored under, if it parses.
#[must_use]
pub fn parameter_set_id(codec: Codec, set: ParameterSet, nal: &[u8]) -> Option<u32> {
    let rbsp = unescape_rbsp(nal);
    match (codec, set) {
        (Codec::H265, ParameterSet::Vps) => BitReader::new(&rbsp, 2).u(4).ok(),
        (Codec::H265, ParameterSet::Pps) => BitReader::new(&rbsp, 2).ue().ok(),
        (Codec::H265, ParameterSet::Sps) => h265_sps(nal).ok().map(|s| s.id),
        (Codec::H264, ParameterSet::Sps) => BitReader::new(&rbsp, 4).ue().ok(),
        (Codec::H264, _) => BitReader::new(&rbsp, 1).ue().ok(),
    }
}

/// What an SPS says: the WebCodecs codec string and the picture size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpsInfo {
    pub id: u32,
    /// Such as `hvc1.1.6.L120.90` or `avc1.640028`.
    pub codec: String,
    pub coded_width: Option<u32>,
    pub coded_height: Option<u32>,
    pub display_width: Option<u32>,
    pub display_height: Option<u32>,
}

/// # Errors
/// An SPS too short to read.
pub fn parse_sps(codec: Codec, nal: &[u8]) -> Result<SpsInfo, OutOfBits> {
    match codec {
        Codec::H265 => h265_sps(nal),
        Codec::H264 => h264_sps(nal),
    }
}

fn h264_sps(nal: &[u8]) -> Result<SpsInfo, OutOfBits> {
    let rbsp = unescape_rbsp(nal);
    if rbsp.len() < 5 {
        return Err(OutOfBits);
    }
    let codec = format!("avc1.{:02x}{:02x}{:02x}", rbsp[1], rbsp[2], rbsp[3]);
    let mut r = BitReader::new(&rbsp, 4);
    let id = r.ue()?;
    // The picture size is a nicety: a stream with an SPS this reader
    // cannot follow still has its codec string.
    let size = h264_size(&mut r, rbsp[1]).ok();
    Ok(SpsInfo {
        id,
        codec,
        coded_width: size.map(|s| s.0),
        coded_height: size.map(|s| s.1),
        display_width: size.map(|s| s.2),
        display_height: size.map(|s| s.3),
    })
}

fn se(r: &mut BitReader<'_>) -> Result<i64, OutOfBits> {
    let k = i64::from(r.ue()?);
    Ok(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) })
}

/// ITU-T H.264 7.3.2.1.1 after `seq_parameter_set_id`: coded and cropped
/// width and height.
fn h264_size(r: &mut BitReader<'_>, profile_idc: u8) -> Result<(u32, u32, u32, u32), OutOfBits> {
    let mut chroma_format_idc = 1;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            r.u(1)?; // separate_colour_plane_flag
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.u(1)?; // qpprime_y_zero_transform_bypass_flag
        if r.u(1)? == 1 {
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                if r.u(1)? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i64, 8i64);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + se(r)? + 256) % 256;
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            r.u(1)?; // delta_pic_order_always_zero_flag
            se(r)?; // offset_for_non_ref_pic
            se(r)?; // offset_for_top_to_bottom_field
            for _ in 0..r.ue()? {
                se(r)?;
            }
        }
        _ => {}
    }
    r.ue()?; // max_num_ref_frames
    r.u(1)?; // gaps_in_frame_num_value_allowed_flag
    let width_mbs = r.ue()? + 1;
    let height_units = r.ue()? + 1;
    let frame_mbs_only = r.u(1)?;
    if frame_mbs_only == 0 {
        r.u(1)?; // mb_adaptive_frame_field_flag
    }
    r.u(1)?; // direct_8x8_inference_flag
    let width = width_mbs * 16;
    let height = (2 - frame_mbs_only) * height_units * 16;
    let (mut display_width, mut display_height) = (width, height);
    if r.u(1)? == 1 {
        let (left, right, top, bottom) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
        let (unit_x, unit_y) = match chroma_format_idc {
            1 => (2, 2 * (2 - frame_mbs_only)),
            2 => (2, 2 - frame_mbs_only),
            _ => (1, 2 - frame_mbs_only),
        };
        display_width = width.saturating_sub(unit_x * (left + right));
        display_height = height.saturating_sub(unit_y * (top + bottom));
    }
    Ok((width, height, display_width, display_height))
}

/// ISO/IEC 14496-15 Annex E codec string, and the picture size.
fn h265_sps(nal: &[u8]) -> Result<SpsInfo, OutOfBits> {
    let rbsp = unescape_rbsp(nal);
    let mut r = BitReader::new(&rbsp, 2);
    r.u(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.u(3)?;
    r.u(1)?; // sps_temporal_id_nesting_flag
    let profile_space = r.u(2)?;
    let tier = r.u(1)?;
    let profile_idc = r.u(5)?;
    let compatibility = r.u(32)?;
    let mut constraints = [0u32; 6];
    for c in &mut constraints {
        *c = r.u(8)?;
    }
    let level_idc = r.u(8)?;
    let mut sub_profile = Vec::new();
    let mut sub_level = Vec::new();
    for _ in 0..max_sub_layers_minus1 {
        sub_profile.push(r.u(1)? == 1);
        sub_level.push(r.u(1)? == 1);
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            r.u(2)?;
        }
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if sub_profile[i] {
            r.skip(88);
        }
        if sub_level[i] {
            r.skip(8);
        }
    }
    let id = r.ue()?;
    let chroma_format_idc = r.ue()?;
    if chroma_format_idc == 3 {
        r.u(1)?; // separate_colour_plane_flag
    }
    let coded_width = r.ue()?;
    let coded_height = r.ue()?;
    let (mut display_width, mut display_height) = (coded_width, coded_height);
    if r.u(1)? == 1 {
        let sub_width = if chroma_format_idc == 1 || chroma_format_idc == 2 {
            2
        } else {
            1
        };
        let sub_height = if chroma_format_idc == 1 { 2 } else { 1 };
        let (left, right, top, bottom) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
        display_width = display_width.saturating_sub(sub_width * (left + right));
        display_height = display_height.saturating_sub(sub_height * (top + bottom));
    }
    let last = constraints
        .iter()
        .rposition(|&c| c != 0)
        .map_or(0, |i| i + 1);
    let mut parts = vec![
        "hvc1".to_owned(),
        format!(
            "{}{profile_idc}",
            ["", "A", "B", "C"][profile_space as usize]
        ),
        format!("{:X}", compatibility.reverse_bits()),
        format!("{}{level_idc}", if tier == 1 { "H" } else { "L" }),
    ];
    parts.extend(constraints[..last].iter().map(|c| format!("{c:02X}")));
    Ok(SpsInfo {
        id,
        codec: parts.join("."),
        coded_width: Some(coded_width),
        coded_height: Some(coded_height),
        display_width: Some(display_width),
        display_height: Some(display_height),
    })
}
