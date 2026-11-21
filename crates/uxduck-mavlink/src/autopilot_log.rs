//! What an autopilot's own log covers (S426, the first step of S70): the
//! vehicle it names and the time it spans, read once so the Logbook can say
//! which flight a `.bin` or `.ulg` belongs to. It reads the log as replay does
//! (ADR-0039 R15), so the span is on the same clock the replay plays.

/// The vehicle an autopilot log names and the time it covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    /// `SYSID_THISMAV` or PX4's `MAV_SYS_ID`, else 1.
    pub system_id: u8,
    pub firmware: Option<String>,
    /// Its times are GPS time, so Unix microseconds. Else they count from
    /// boot, and the log cannot be placed beside a flight.
    pub gps_time: bool,
    pub from_us: u64,
    pub to_us: u64,
}

/// What an ArduPilot `.bin` or a PX4 `.ulg` covers.
///
/// # Errors
/// When it is neither, or holds no records UXDuck can read.
pub fn span(bytes: &[u8]) -> Result<Span, String> {
    let (tlog, system_id, firmware, gps_time) = if crate::ulog::is_ulog(bytes) {
        let (tlog, about) = crate::ulog::to_tlog(bytes)?;
        (tlog, about.system_id, about.firmware, about.gps_time)
    } else if crate::dataflash::is_dataflash(bytes) {
        let (tlog, about) = crate::dataflash::to_tlog(bytes)?;
        (tlog, about.system_id, about.firmware, about.gps_time)
    } else {
        return Err("This is not an ArduPilot .bin or a PX4 .ulg log.".into());
    };
    let (from_us, to_us) = crate::tlog::records(&tlog)
        .map(|(at, _)| at)
        .fold(None, |span: Option<(u64, u64)>, at| {
            Some(span.map_or((at, at), |(from, to)| (from.min(at), to.max(at))))
        })
        .ok_or("This log holds no records UXDuck can read.")?;
    Ok(Span {
        system_id,
        firmware,
        gps_time,
        from_us,
        to_us,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bin_and_a_ulg_say_their_vehicle_and_their_gps_time() {
        let bin = span(&crate::dataflash::fixture::flight()).unwrap();
        assert!(bin.gps_time, "{bin:?}");
        assert!(
            crate::tlog::sane(bin.from_us) && bin.from_us < bin.to_us,
            "{bin:?}"
        );
        assert!(
            bin.firmware
                .as_deref()
                .is_some_and(|f| f.starts_with("ArduCopter")),
            "{bin:?}"
        );
        let ulg = span(&crate::ulog::fixture::flight()).unwrap();
        assert!(
            ulg.gps_time && crate::tlog::sane(ulg.from_us) && ulg.from_us < ulg.to_us,
            "{ulg:?}"
        );
    }

    #[test]
    fn anything_else_says_what_it_is_not() {
        assert_eq!(
            span(b"not a log at all").unwrap_err(),
            "This is not an ArduPilot .bin or a PX4 .ulg log."
        );
    }
}
