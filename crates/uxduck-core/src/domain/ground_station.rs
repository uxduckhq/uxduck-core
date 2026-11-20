//! The ground stations an `ArduPilot` vehicle listens to (FB-259), from its
//! own parameters. `ArduPilot` takes `RC_CHANNELS_OVERRIDE` and
//! `MANUAL_CONTROL` only from these MAVLink system ids, and counts only their
//! heartbeats for its lost ground station failsafe (`GCS::sysid_is_gcs`).
//! Anything else from another id is dropped without a word back.
//!
//! Before 4.7 that is one id, `SYSID_MYGCS`. `ArduPilot` 4.7 renamed it
//! `MAV_GCS_SYSID` and added `MAV_GCS_SYSID_HI`: when that is above
//! `MAV_GCS_SYSID`, every id from one to the other counts. Both ids default
//! to 255, as UXDuck's own Ground station ID does, and `MAV_GCS_SYSID_HI` to
//! 0, which is no range. PX4 has no such parameter.

/// The ids a vehicle takes as its ground station's, as its parameters say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroundStationIds {
    /// The parameter naming it: `MAV_GCS_SYSID` from `ArduPilot` 4.7, else
    /// `SYSID_MYGCS`.
    pub parameter: &'static str,
    pub low: i64,
    /// The top of the range `MAV_GCS_SYSID_HI` makes, when it is above
    /// `low`. None when only `low` counts.
    pub high: Option<i64>,
}

/// A whole number as the vehicle reported it. None for a fraction, or a
/// value that isn't finite, which no integer parameter can hold.
#[allow(
    clippy::cast_possible_truncation,
    reason = "checked whole and in range"
)]
fn whole(value: f64) -> Option<i64> {
    (value.is_finite() && value.fract() == 0.0 && value.abs() <= f64::from(u32::MAX))
        .then_some(value as i64)
}

impl GroundStationIds {
    /// From a vehicle's parameters (`param` reads one by name). None when
    /// they can't say: neither parameter is there, as on PX4, or they have
    /// not been read, or a value is not a whole number.
    ///
    /// From 4.7 both `MAV_GCS_SYSID` and `MAV_GCS_SYSID_HI` are needed,
    /// since a range can hold an id the base alone doesn't, and once the new
    /// base is there `SYSID_MYGCS` is never read. 4.7 compares the two as
    /// signed 16-bit values. Before 4.7, `SYSID_MYGCS` is read through an
    /// 8-bit getter, so -1 there is 255.
    #[must_use]
    pub fn read(param: impl Fn(&str) -> Option<f64>) -> Option<Self> {
        if let Some(low) = param("MAV_GCS_SYSID") {
            let low = whole(low)?;
            let high = whole(param("MAV_GCS_SYSID_HI")?)?;
            return Some(Self {
                parameter: "MAV_GCS_SYSID",
                low,
                high: (high > low).then_some(high),
            });
        }
        Some(Self {
            parameter: "SYSID_MYGCS",
            low: whole(param("SYSID_MYGCS")?)?.rem_euclid(256),
            high: None,
        })
    }

    /// Whether the vehicle takes `system_id` as its ground station.
    #[must_use]
    pub fn accepts(&self, system_id: u8) -> bool {
        let system_id = i64::from(system_id);
        match self.high {
            Some(high) => (self.low..=high).contains(&system_id),
            None => self.low == system_id,
        }
    }

    /// The vehicle's side, after "Its": "`MAV_GCS_SYSID` is 254", or
    /// "`MAV_GCS_SYSID` and `MAV_GCS_SYSID_HI` take ground station IDs 250 to
    /// 254".
    #[must_use]
    pub fn described(&self) -> String {
        match self.high {
            Some(high) => format!(
                "{} and {}_HI take ground station IDs {} to {high}",
                self.parameter, self.parameter, self.low
            ),
            None => format!("{} is {}", self.parameter, self.low),
        }
    }

    /// What UXDuck's Ground station ID could be instead: "254", or "one
    /// from 250 to 254". None when no id UXDuck can send as would do.
    #[must_use]
    pub fn would_take(&self) -> Option<String> {
        match self.high {
            Some(high) if self.low <= 255 && high >= 1 => {
                Some(format!("one from {} to {}", self.low.max(1), high.min(255)))
            }
            None if (1..=255).contains(&self.low) => Some(self.low.to_string()),
            _ => None,
        }
    }

    /// What to set on the vehicle for it to take `system_id`, after "Set":
    /// "`MAV_GCS_SYSID` to 255".
    #[must_use]
    pub fn setting_for(&self, system_id: u8) -> String {
        match self.high {
            Some(_) => format!(
                "{} and {}_HI so {system_id} is from one to the other",
                self.parameter, self.parameter
            ),
            None => format!("{} to {system_id}", self.parameter),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, f64)]) -> impl Fn(&str) -> Option<f64> {
        let owned: Vec<(String, f64)> = pairs.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect();
        move |name| owned.iter().find(|(k, _)| k == name).map(|(_, v)| *v)
    }

    #[test]
    fn before_4_7_one_id_by_its_old_name() {
        let ids = GroundStationIds::read(params(&[("SYSID_MYGCS", 255.0)])).unwrap();
        assert_eq!(ids.parameter, "SYSID_MYGCS");
        assert!(ids.accepts(255));
        assert!(!ids.accepts(254));
        let other = GroundStationIds::read(params(&[("SYSID_MYGCS", 254.0)])).unwrap();
        assert!(!other.accepts(255));
        assert_eq!(other.described(), "SYSID_MYGCS is 254");
        assert_eq!(other.would_take().as_deref(), Some("254"));
        assert_eq!(other.setting_for(255), "SYSID_MYGCS to 255");
        // ArduPilot reads it as 8 bits: -1 is 255, 256 is 0.
        let minus = GroundStationIds::read(params(&[("SYSID_MYGCS", -1.0)])).unwrap();
        assert!(minus.accepts(255));
        assert_eq!(minus.described(), "SYSID_MYGCS is 255");
        let wrapped = GroundStationIds::read(params(&[("SYSID_MYGCS", 256.0)])).unwrap();
        assert!(!wrapped.accepts(255));
        assert_eq!(wrapped.low, 0);
    }

    #[test]
    fn a_fraction_or_a_value_that_isnt_finite_says_nothing() {
        for bad in [254.5, f64::NAN, f64::INFINITY] {
            assert_eq!(
                GroundStationIds::read(params(&[("SYSID_MYGCS", bad)])),
                None,
                "{bad}"
            );
            assert_eq!(
                GroundStationIds::read(params(&[
                    ("MAV_GCS_SYSID", bad),
                    ("MAV_GCS_SYSID_HI", 0.0)
                ])),
                None,
                "{bad}"
            );
            assert_eq!(
                GroundStationIds::read(params(&[
                    ("MAV_GCS_SYSID", 254.0),
                    ("MAV_GCS_SYSID_HI", bad)
                ])),
                None,
                "{bad}"
            );
        }
    }

    #[test]
    fn from_4_7_by_its_new_name() {
        let ids = GroundStationIds::read(params(&[
            ("MAV_GCS_SYSID", 254.0),
            ("MAV_GCS_SYSID_HI", 0.0),
        ]))
        .unwrap();
        assert_eq!(ids.parameter, "MAV_GCS_SYSID");
        assert_eq!(ids.high, None, "0, as it ships, is no range");
        assert!(ids.accepts(254));
        assert!(!ids.accepts(255));
        assert_eq!(ids.described(), "MAV_GCS_SYSID is 254");
        assert_eq!(ids.setting_for(255), "MAV_GCS_SYSID to 255");
        // The new name wins where a vehicle somehow reports both.
        let both = GroundStationIds::read(params(&[
            ("SYSID_MYGCS", 255.0),
            ("MAV_GCS_SYSID", 254.0),
            ("MAV_GCS_SYSID_HI", 0.0),
        ]))
        .unwrap();
        assert!(!both.accepts(255));
        // A 4.7 base without its HI yet can't be judged, even by the old name.
        for without_hi in [
            &[("MAV_GCS_SYSID", 250.0)][..],
            &[("MAV_GCS_SYSID", 250.0), ("SYSID_MYGCS", 255.0)][..],
        ] {
            assert_eq!(GroundStationIds::read(params(without_hi)), None);
        }
        // 4.7 compares signed values, with no 8-bit wrap.
        let signed = GroundStationIds::read(params(&[
            ("MAV_GCS_SYSID", -1.0),
            ("MAV_GCS_SYSID_HI", 0.0),
        ]))
        .unwrap();
        assert!(!signed.accepts(255));
    }

    #[test]
    fn a_range_from_mav_gcs_sysid_to_its_hi() {
        let ids = GroundStationIds::read(params(&[
            ("MAV_GCS_SYSID", 250.0),
            ("MAV_GCS_SYSID_HI", 254.0),
        ]))
        .unwrap();
        assert_eq!(ids.high, Some(254));
        for inside in [250, 252, 254] {
            assert!(ids.accepts(inside), "{inside}");
        }
        for outside in [249, 255] {
            assert!(!ids.accepts(outside), "{outside}");
        }
        assert_eq!(
            ids.described(),
            "MAV_GCS_SYSID and MAV_GCS_SYSID_HI take ground station IDs 250 to 254"
        );
        assert_eq!(ids.would_take().as_deref(), Some("one from 250 to 254"));
        assert_eq!(
            ids.setting_for(42),
            "MAV_GCS_SYSID and MAV_GCS_SYSID_HI so 42 is from one to the other"
        );
        // ArduPilot reads a HI at or below MAV_GCS_SYSID as no range.
        for hi in [250.0, 100.0] {
            let single = GroundStationIds::read(params(&[
                ("MAV_GCS_SYSID", 250.0),
                ("MAV_GCS_SYSID_HI", hi),
            ]))
            .unwrap();
            assert_eq!(single.high, None, "{hi}");
            assert!(single.accepts(250));
            assert!(!single.accepts(251));
        }
    }

    #[test]
    fn nothing_to_say_without_either_parameter() {
        assert_eq!(GroundStationIds::read(params(&[])), None);
        assert_eq!(
            GroundStationIds::read(params(&[("MAV_GCS_SYSID_HI", 254.0)])),
            None
        );
        assert_eq!(
            GroundStationIds::read(params(&[("NAV_DLL_ACT", 2.0)])),
            None
        );
    }

    #[test]
    fn an_id_uxduck_cannot_send_as_is_not_offered() {
        let zero = GroundStationIds::read(params(&[("SYSID_MYGCS", 0.0)])).unwrap();
        assert!(!zero.accepts(255));
        assert_eq!(zero.would_take(), None);
    }
}
