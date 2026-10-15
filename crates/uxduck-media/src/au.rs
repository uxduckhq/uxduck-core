//! Video PES payloads into access units a strict decoder accepts (ADR-0017
//! V2): NAL units a sloppy encoder filled with junk are dropped, the latest
//! parameter sets go in front of every keyframe, and everything waits for
//! a keyframe after a start, a loss or a decoder error. The experiment's
//! camera writes an access unit delimiter with private bytes that sometimes
//! hold `00 00 01` and so fake a NAL unit of a reserved type; both go here.

use std::collections::{BTreeMap, BTreeSet};

use crate::nal::{
    Codec, ParameterSet, SpsInfo, h264, h265, is_keyframe, is_vcl, parameter_set, parameter_set_id,
    parse_header, parse_sps, split_annex_b,
};

/// One picture, ready for a decoder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessUnit {
    /// Annex B, four-byte start codes.
    pub data: Vec<u8>,
    pub key: bool,
    /// Microseconds, unwrapped.
    pub timestamp_us: u64,
    pub sps: SpsInfo,
}

/// What to do with a picture that lost packets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnDamage {
    /// Drop it and everything up to the next keyframe: a clean freeze.
    #[default]
    Wait,
    /// Hand it to the decoder to conceal.
    Submit,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FilterStats {
    pub units: u64,
    pub keyframes: u64,
    pub nals_kept: u64,
    /// By why: `AUD`, `filler`, `SEI`, `forbidden bit`, `layer id`, ...
    pub nals_dropped: BTreeMap<&'static str, u64>,
    /// Pictures skipped while waiting for a keyframe.
    pub skipped_waiting_for_key: u64,
    pub skipped_rasl: u64,
    /// Pictures with lost packets.
    pub damaged: u64,
    pub wait_reasons: BTreeMap<&'static str, u64>,
}

const START_CODE: [u8; 4] = [0, 0, 0, 1];
const WRAP: i64 = 1 << 33;

#[derive(Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent switches and states"
)]
pub struct Filter {
    pub codec: Codec,
    pub stats: FilterStats,
    /// Off passes each PES through untouched, to see what a decoder gets
    /// without it.
    enabled: bool,
    strip_sei: bool,
    on_damage: OnDamage,
    sets: BTreeMap<(ParameterSet, u32), Vec<u8>>,
    sps: Option<SpsInfo>,
    waiting_for_key: bool,
    skipping_rasl: bool,
    last_pts: Option<i64>,
    pts_offset: i64,
}

impl Filter {
    #[must_use]
    pub fn new(codec: Codec) -> Self {
        Self {
            codec,
            stats: FilterStats::default(),
            enabled: true,
            strip_sei: true,
            on_damage: OnDamage::Wait,
            sets: BTreeMap::new(),
            sps: None,
            waiting_for_key: true,
            skipping_rasl: false,
            last_pts: None,
            pts_offset: 0,
        }
    }

    #[must_use]
    pub const fn passing_through(mut self) -> Self {
        self.enabled = false;
        self
    }

    #[must_use]
    pub const fn on_damage(mut self, on_damage: OnDamage) -> Self {
        self.on_damage = on_damage;
        self
    }

    /// The latest sequence parameter set's picture and codec string.
    #[must_use]
    pub const fn sps(&self) -> Option<&SpsInfo> {
        self.sps.as_ref()
    }

    #[must_use]
    pub const fn waiting_for_key(&self) -> bool {
        self.waiting_for_key
    }

    /// Drops everything until the next keyframe, such as after a decoder
    /// error or a slow client (V7).
    pub fn wait_for_keyframe(&mut self, reason: &'static str) {
        self.waiting_for_key = true;
        *self.stats.wait_reasons.entry(reason).or_default() += 1;
    }

    /// One PES payload; the access unit it makes, if any.
    pub fn push(&mut self, es: &[u8], pts: Option<u64>, damaged: bool) -> Option<AccessUnit> {
        let timestamp_us = self.unwrap(pts);
        if damaged {
            self.stats.damaged += 1;
            if self.on_damage == OnDamage::Wait {
                self.wait_for_keyframe("damaged PES");
                return None;
            }
        }
        let mut kept: Vec<&[u8]> = Vec::new();
        let mut present = BTreeSet::new();
        let mut vcl = Vec::new();
        for nal in split_annex_b(es) {
            let header = parse_header(self.codec, nal);
            let drop = if self.enabled {
                self.drop_reason(header.kind, header.invalid)
            } else {
                None
            };
            if let Some(why) = drop {
                *self.stats.nals_dropped.entry(why).or_default() += 1;
                continue;
            }
            if let (Some(kind), None) = (header.kind, header.invalid) {
                if let Some(set) = parameter_set(self.codec, kind) {
                    self.remember(set, nal);
                    present.insert(set);
                }
                if is_vcl(self.codec, kind) {
                    vcl.push(kind);
                }
            }
            kept.push(nal);
        }
        if vcl.is_empty() {
            return None;
        }
        let key = vcl.iter().any(|&k| is_keyframe(self.codec, k));
        if self.waiting_for_key && !key {
            self.stats.skipped_waiting_for_key += 1;
            return None;
        }
        if key {
            self.waiting_for_key = false;
            self.skipping_rasl = self.codec == Codec::H265 && vcl.contains(&h265::CRA);
        } else if self.skipping_rasl {
            // Leading pictures of a CRA we started from refer to pictures we
            // never had.
            if vcl.iter().all(|&k| k == h265::RASL_N || k == h265::RASL_R) {
                self.stats.skipped_rasl += 1;
                return None;
            }
            self.skipping_rasl = false;
        }
        let Some(sps) = self.sps.clone() else {
            self.wait_for_keyframe("no parameter sets yet");
            return None;
        };
        self.stats.units += 1;
        if key {
            self.stats.keyframes += 1;
        }
        self.stats.nals_kept += kept.len() as u64;
        if !self.enabled {
            return Some(AccessUnit {
                data: es.to_vec(),
                key,
                timestamp_us,
                sps,
            });
        }
        let mut parts: Vec<&[u8]> = Vec::new();
        if key {
            // Parameter sets travel with every keyframe, whether the camera
            // sent them or not.
            let order: &[ParameterSet] = match self.codec {
                Codec::H265 => &[ParameterSet::Vps, ParameterSet::Sps, ParameterSet::Pps],
                Codec::H264 => &[ParameterSet::Sps, ParameterSet::Pps],
            };
            for set in order {
                if !present.contains(set) {
                    parts.extend(
                        self.sets
                            .range((*set, 0)..=(*set, u32::MAX))
                            .map(|(_, nal)| nal.as_slice()),
                    );
                }
            }
        }
        parts.extend(kept);
        let mut data = Vec::with_capacity(parts.iter().map(|p| p.len() + 4).sum());
        for part in parts {
            data.extend_from_slice(&START_CODE);
            data.extend_from_slice(part);
        }
        Some(AccessUnit {
            data,
            key,
            timestamp_us,
            sps,
        })
    }

    fn drop_reason(&self, kind: Option<u8>, invalid: Option<&'static str>) -> Option<&'static str> {
        if invalid.is_some() {
            return invalid;
        }
        let kind = kind?;
        match self.codec {
            Codec::H265 => match kind {
                h265::AUD => Some("AUD"),
                h265::FD => Some("filler"),
                h265::SEI_PREFIX | h265::SEI_SUFFIX if self.strip_sei => Some("SEI"),
                _ => None,
            },
            Codec::H264 => match kind {
                h264::AUD => Some("AUD"),
                h264::FD => Some("filler"),
                h264::SEI if self.strip_sei => Some("SEI"),
                _ => None,
            },
        }
    }

    fn remember(&mut self, set: ParameterSet, nal: &[u8]) {
        let Some(id) = parameter_set_id(self.codec, set, nal) else {
            return;
        };
        self.sets.insert((set, id), nal.to_vec());
        if set == ParameterSet::Sps
            && let Ok(sps) = parse_sps(self.codec, nal)
        {
            // A truncated SPS keeps the previous one.
            self.sps = Some(sps);
        }
    }

    /// A 33-bit 90 kHz PTS as microseconds that keep increasing across the
    /// wrap.
    fn unwrap(&mut self, pts: Option<u64>) -> u64 {
        let pts = pts
            .and_then(|p| i64::try_from(p).ok())
            .or(self.last_pts)
            .unwrap_or(0);
        if let Some(last) = self.last_pts {
            if pts < last - WRAP / 2 {
                self.pts_offset += WRAP;
            } else if pts > last + WRAP / 2 {
                self.pts_offset -= WRAP;
            }
        }
        self.last_pts = Some(pts);
        let us = ((pts + self.pts_offset) * 100 + 4) / 9;
        u64::try_from(us).unwrap_or(0)
    }
}
