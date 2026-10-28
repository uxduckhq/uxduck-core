//! PX4's calibration status text (S191, ADR-0037 V11): PX4 reports a
//! calibration started with `MAV_CMD_PREFLIGHT_CALIBRATION` only in
//! `STATUSTEXT` lines starting `[cal]`, the protocol QGroundControl reads
//! (version 2: `[cal] calibration started: 2 accel`). This reads one line
//! into what it says. Written from the lines PX4 1.17 sends, as recorded
//! from its SITL (`tests/fixtures/px4_cal/`), and its format strings:
//!
//! - `[cal] calibration started: 2 <kind>`, `… done: <kind>`, `… failed:
//!   <why or kind>` (a reason first, then the kind), `… cancelled`;
//! - `[cal] progress <n>`, and the compass's `[cal] <side> side
//!   calibration: progress <n>`;
//! - the sides: `[cal] pending: back front left right up down` (those not
//!   done yet), `[cal] hold vehicle still on a pending side`, `[cal]
//!   detected rest position, hold still...`, `[cal] detected motion, hold
//!   still...`, `[cal] <side> orientation detected`, the accelerometer's
//!   `[cal] Hold still, measuring <side> side`, the compass's `[cal]
//!   Rotate vehicle`, `[cal] <side> side done, rotate to a different side`
//!   and `[cal] <side> side already completed`;
//! - the airspeed's `[cal] Ensure sensor is not measuring wind`, `[cal]
//!   Offset of <n> Pascal`, `[cal] Blow into front of pitot without
//!   touching`, `[cal] Positive pressure: OK (<n> Pa)`, `[cal] Negative
//!   pressure difference detected (<n> Pa)`, `[cal] Swap static and dynamic
//!   ports…`, `[cal] Create air pressure! (got <n>, wanted: 50 Pa)`.
//!
//! PX4's sides are named for the side facing down: `down` is level, `up`
//! upside down, `front` the nose down and `back` the tail down (nose up).

/// A side of the vehicle, as PX4 names the one facing down.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Side {
    /// "back": the tail down, nose up.
    Back,
    /// "front": the nose down.
    Front,
    /// "left": the left side down.
    Left,
    /// "right": the right side down.
    Right,
    /// "up": upside down.
    Up,
    /// "down": level, the right way up.
    Down,
}

impl Side {
    /// Every side, in PX4's order.
    pub const ALL: [Self; 6] = [
        Self::Back,
        Self::Front,
        Self::Left,
        Self::Right,
        Self::Up,
        Self::Down,
    ];

    /// From PX4's word for it.
    #[must_use]
    pub fn from_word(word: &str) -> Option<Self> {
        Some(match word.trim().to_ascii_lowercase().as_str() {
            "back" => Self::Back,
            "front" => Self::Front,
            "left" => Self::Left,
            "right" => Self::Right,
            "up" => Self::Up,
            "down" => Self::Down,
            _ => return None,
        })
    }

    /// PX4's word for it.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Back => "back",
            Self::Front => "front",
            Self::Left => "left",
            Self::Right => "right",
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

/// What one `[cal]` line says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CalText {
    /// "calibration started: 2 mag": the protocol's version and the kind
    /// (`mag`, `accel`, `level`, `gyro`, `airspeed`, `baro`).
    Started {
        version: u8,
        kind: String,
    },
    /// Overall progress, percent.
    Progress(u8),
    /// The compass's progress while it turns on a side, percent overall.
    SideProgress {
        side: Side,
        progress: u8,
    },
    /// The sides not done yet.
    Pending(Vec<Side>),
    /// It waits to be held still on a side not done yet.
    HoldOnASide,
    /// It sees the vehicle still, and is about to read which side is down.
    AtRest,
    /// It sees the vehicle moving, and waits for it to be still.
    Moving,
    /// It found this side down.
    Detected(Side),
    /// The accelerometer measures this side: hold it still.
    Measuring(Side),
    /// The compass: turn the vehicle round with this side down.
    Rotate,
    /// This side is done; turn it onto another.
    SideDone(Side),
    /// This side was done already; turn it onto another.
    AlreadyDone(Side),
    /// "calibration done: accel".
    Done(String),
    /// "calibration failed: …": a reason ("timeout: no motion", "reading
    /// sensor"), or the kind, which PX4 says last.
    Failed(String),
    /// "calibration warning: …".
    Warning(String),
    Cancelled,
    /// The airspeed: keep the wind off the pitot while it zeroes.
    CoverPitot,
    /// The airspeed's zero, in pascals.
    Offset(i32),
    /// The airspeed: blow into the pitot now.
    BlowIntoPitot,
    /// The airspeed: the pressure rose, in pascals.
    PressureOk(i32),
    /// The airspeed: the pressure fell, in pascals: its tubes are swapped.
    PressureNegative(i32),
    /// The airspeed: swap its tubes, or set `SENS_DPRES_REV`.
    SwapTubes,
    /// The airspeed: blow harder; the pressure it got, in pascals.
    MorePressure(i32),
    /// Any other `[cal]` line: a side's result, a compass's rotation.
    Other(String),
}

/// The first number in `text`, with its sign.
fn number(text: &str) -> Option<i32> {
    let start = text.find(|c: char| c.is_ascii_digit() || c == '-')?;
    let rest = &text[start..];
    let end = rest
        .char_indices()
        .skip(1)
        .find(|(_, c)| !c.is_ascii_digit())
        .map_or(rest.len(), |(i, _)| i);
    rest[..end].parse().ok()
}

/// The number between `<` and `>`, as a percentage.
fn percent(text: &str) -> Option<u8> {
    let inner = text.split_once('<')?.1.split_once('>')?.0;
    let value: u32 = inner.trim().parse().ok()?;
    u8::try_from(value.min(100)).ok()
}

/// Reads a status text line; `None` when it is not one of PX4's `[cal]`
/// lines.
#[must_use]
#[allow(clippy::too_many_lines, reason = "one arm per line PX4 says")]
pub fn parse(text: &str) -> Option<CalText> {
    // PX4 ends some texts with a tab (its events marker).
    let line = text.trim().strip_prefix("[cal]")?.trim();
    let lower = line.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("calibration started:") {
        let mut words = rest.split_whitespace();
        let first = words.next().unwrap_or_default();
        return Some(match first.parse::<u8>() {
            Ok(version) => CalText::Started {
                version,
                kind: words.collect::<Vec<_>>().join(" "),
            },
            Err(_) => CalText::Started {
                version: 1,
                kind: rest.trim().to_owned(),
            },
        });
    }
    if let Some(kind) = lower.strip_prefix("calibration done:") {
        return Some(CalText::Done(kind.trim().to_owned()));
    }
    if lower.starts_with("calibration failed:") {
        return Some(CalText::Failed(
            line["calibration failed:".len()..].trim().to_owned(),
        ));
    }
    if lower.starts_with("calibration warning:") {
        return Some(CalText::Warning(
            line["calibration warning:".len()..].trim().to_owned(),
        ));
    }
    if lower.starts_with("calibration cancelled") {
        return Some(CalText::Cancelled);
    }
    if lower.starts_with("progress") {
        return percent(&lower).map(CalText::Progress);
    }
    if let Some(rest) = lower.strip_prefix("pending:") {
        return Some(CalText::Pending(
            rest.split_whitespace()
                .filter_map(Side::from_word)
                .collect(),
        ));
    }
    if lower.starts_with("hold vehicle still on a pending side") {
        return Some(CalText::HoldOnASide);
    }
    if lower.starts_with("detected rest position") {
        return Some(CalText::AtRest);
    }
    if lower.starts_with("detected motion") {
        return Some(CalText::Moving);
    }
    if lower.starts_with("rotate vehicle") {
        return Some(CalText::Rotate);
    }
    if let Some(side) = lower
        .strip_prefix("hold still, measuring ")
        .and_then(|rest| rest.strip_suffix(" side"))
        .and_then(Side::from_word)
    {
        return Some(CalText::Measuring(side));
    }
    // "<side> orientation detected", "<side> side done, …", "<side> side
    // already completed", "<side> side calibration: progress <n>".
    let (first, rest) = lower.split_once(' ').unwrap_or((lower.as_str(), ""));
    if let Some(side) = Side::from_word(first) {
        if rest.starts_with("orientation detected") {
            return Some(CalText::Detected(side));
        }
        if rest.starts_with("side done") {
            return Some(CalText::SideDone(side));
        }
        if rest.starts_with("side already completed") {
            return Some(CalText::AlreadyDone(side));
        }
        if rest.starts_with("side calibration") {
            return Some(match percent(rest) {
                Some(progress) => CalText::SideProgress { side, progress },
                None => CalText::Other(line.to_owned()),
            });
        }
    }
    if lower.starts_with("ensure sensor is not measuring wind") {
        return Some(CalText::CoverPitot);
    }
    if lower.starts_with("offset of") {
        return Some(
            number(&lower).map_or_else(|| CalText::Other(line.to_owned()), CalText::Offset),
        );
    }
    if lower.starts_with("blow into") {
        return Some(CalText::BlowIntoPitot);
    }
    if lower.starts_with("positive pressure") {
        let pa = lower.split_once('(').and_then(|(_, r)| number(r));
        return Some(pa.map_or_else(|| CalText::Other(line.to_owned()), CalText::PressureOk));
    }
    if lower.starts_with("negative pressure") {
        let pa = lower.split_once('(').and_then(|(_, r)| number(r));
        return Some(pa.map_or_else(
            || CalText::Other(line.to_owned()),
            CalText::PressureNegative,
        ));
    }
    if lower.starts_with("swap static and dynamic") {
        return Some(CalText::SwapTubes);
    }
    if lower.starts_with("create air pressure") {
        let got = lower
            .split_once("got")
            .and_then(|(_, r)| number(r))
            .unwrap_or(0);
        return Some(CalText::MorePressure(got));
    }
    Some(CalText::Other(line.to_owned()))
}

/// PX4's words for a calibration from a known heading (its "mag quick
/// calibration", `MAV_CMD_FIXED_MAG_CAL_YAW`), which are not `[cal]` lines:
/// "Assuming vehicle is facing heading 90.0 degrees", then "Mag quick
/// calibration finished", or why it could not ("GPS required for mag quick
/// cal", "attitude required for mag quick cal").
#[derive(Clone, Debug, PartialEq)]
pub enum QuickCal {
    /// The heading it calibrates from, degrees.
    Facing(f32),
    Finished,
    /// Why it could not, as it said it.
    Failed(String),
}

/// Reads one of PX4's quick calibration lines, or `None`.
#[must_use]
pub fn parse_quick(text: &str) -> Option<QuickCal> {
    let line = text.trim();
    let lower = line.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("assuming vehicle is facing heading") {
        let degrees = rest.split_whitespace().next()?.parse::<f32>().ok()?;
        return Some(QuickCal::Facing(degrees));
    }
    if lower.starts_with("mag quick calibration finished") {
        return Some(QuickCal::Finished);
    }
    if lower.contains("mag quick cal") {
        return Some(QuickCal::Failed(line.to_owned()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_that_are_not_cal_are_none() {
        assert_eq!(parse("PreArm: Compass not calibrated"), None);
        assert_eq!(parse("Calibration: Disabling RC input\t"), None);
        assert_eq!(parse_quick("[cal] progress <10>"), None);
    }

    #[test]
    fn the_compass_side_progress_and_numbers() {
        assert_eq!(
            parse("[cal] up side calibration: progress <42>"),
            Some(CalText::SideProgress {
                side: Side::Up,
                progress: 42
            })
        );
        assert_eq!(parse("[cal] progress <250>"), Some(CalText::Progress(100)));
        assert_eq!(
            parse("[cal] Offset of -3 Pascal"),
            Some(CalText::Offset(-3))
        );
        assert_eq!(
            parse("[cal] Create air pressure! (got 12, wanted: 50 Pa)"),
            Some(CalText::MorePressure(12))
        );
        assert_eq!(
            parse("[cal] Negative pressure difference detected (-71 Pa)"),
            Some(CalText::PressureNegative(-71))
        );
        assert_eq!(
            parse("[cal] calibration started: 1 mag"),
            Some(CalText::Started {
                version: 1,
                kind: "mag".into()
            })
        );
    }
}
