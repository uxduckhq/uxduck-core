//! Pilot inputs encoded as RC PWM, using each firmware's parameters.

use std::collections::BTreeMap;

use crate::modes::Family;
use crate::transfers::Param;

#[derive(Clone, Debug, Default)]
pub(crate) struct RcInput {
    parameters: BTreeMap<String, f32>,
    ready: bool,
}

impl RcInput {
    pub fn ready(&self) -> bool {
        self.ready
    }
    pub fn note(&mut self, param: &Param) {
        if param.name.starts_with("RC") {
            self.parameters.insert(param.name.clone(), param.value);
        }
    }

    pub fn downloaded<'a>(&mut self, all: impl Iterator<Item = &'a Param>) {
        self.parameters.clear();
        for param in all {
            self.note(param);
        }
        self.ready = true;
    }

    fn value(&self, name: &str, fallback: f32) -> Option<f32> {
        let value = self.parameters.get(name).copied().unwrap_or(fallback);
        value.is_finite().then_some(value)
    }

    fn channel(&self, family: Family, axis: &str, fallback: u8) -> Option<usize> {
        let prefix = if family == Family::Px4 {
            "RC_MAP_"
        } else {
            "RCMAP_"
        };
        // PX4 defaults are disabled (0); require the vehicle's configured map.
        let fallback = if family == Family::Px4 { 0 } else { fallback };
        let value = self.value(&format!("{prefix}{axis}"), f32::from(fallback))?;
        if value.fract() != 0.0 || !(1.0..=16.0).contains(&value) {
            return None;
        }
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "validated channel"
        )]
        Some(value as usize - 1)
    }

    /// Input -1 to 1, neutral at the calibrated trim; throttle uses the full
    /// min-to-max range on aircraft. Reversal remains the autopilot's setting,
    /// as it was with `MANUAL_CONTROL`; the mapping's Reversed box changes input.
    fn pwm(&self, channel: usize, value: f32, throttle: bool) -> Option<u16> {
        let channel = channel + 1;
        let min = self.value(&format!("RC{channel}_MIN"), 1000.0)?;
        let max = self.value(&format!("RC{channel}_MAX"), 2000.0)?;
        let trim = self.value(&format!("RC{channel}_TRIM"), 1500.0)?;
        if !(800.0..=2200.0).contains(&min)
            || !(800.0..=2200.0).contains(&max)
            || min >= max
            || !(min..=max).contains(&trim)
        {
            return None;
        }
        let value = value.clamp(-1.0, 1.0);
        let pwm = if throttle {
            min + value.midpoint(1.0) * (max - min)
        } else if value >= 0.0 {
            trim + value * (max - trim)
        } else {
            trim + value * (trim - min)
        };
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "validated PWM range"
        )]
        Some(pwm.round() as u16)
    }

    pub fn channels(
        &self,
        family: Family,
        sticks: [i16; 4],
        auxiliary: &[(u8, i16)],
    ) -> Option<[u16; 18]> {
        if !self.ready {
            return None;
        }
        let roll = if family == Family::Sub {
            self.channel(family, "LATERAL", 6)?
        } else {
            self.channel(family, "ROLL", 1)?
        };
        let pitch = if family == Family::Sub {
            self.channel(family, "FORWARD", 5)?
        } else {
            self.channel(family, "PITCH", 2)?
        };
        let throttle = self.channel(family, "THROTTLE", 3)?;
        let yaw = self.channel(family, "YAW", 4)?;
        let primary = if family == Family::Rover {
            vec![roll, throttle]
        } else {
            vec![roll, pitch, throttle, yaw]
        };
        let mut unique = primary.clone();
        unique.sort_unstable();
        unique.dedup();
        if unique.len() != primary.len() {
            return None;
        }
        let [x, y, z, r] = sticks;
        let ignored = if family == Family::Px4 { 0 } else { u16::MAX };
        let mut channels = [ignored; 18];
        channels[roll] = self.pwm(roll, f32::from(y) / 1000.0, false)?;
        channels[throttle] = self.pwm(
            throttle,
            f32::from(z.clamp(0, 1000)) / 500.0 - 1.0,
            matches!(family, Family::Copter | Family::Plane | Family::Px4),
        )?;
        if family != Family::Rover {
            // Plane/Copter use positive pitch for forward, with reversed PWM.
            // Blimp/Sub forward translation and PX4 pitch use positive PWM.
            let sign = if matches!(family, Family::Blimp | Family::Sub | Family::Px4) {
                1.0
            } else {
                -1.0
            };
            channels[pitch] = self.pwm(pitch, sign * f32::from(x) / 1000.0, false)?;
            channels[yaw] = self.pwm(yaw, f32::from(r) / 1000.0, false)?;
        }
        for &(channel, value) in auxiliary {
            if !(5..=16).contains(&channel) {
                return None;
            }
            let index = usize::from(channel - 1);
            if channels[index] != ignored {
                return None;
            }
            channels[index] = self.pwm(index, f32::from(value) / 1000.0, false)?;
        }
        Some(channels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn params(rc: &mut RcInput, values: &[(&str, f32)]) {
        let all: Vec<_> = values
            .iter()
            .map(|(name, value)| Param {
                name: (*name).into(),
                value: *value,
                kind: crate::sim::REAL32,
            })
            .collect();
        rc.downloaded(all.iter());
    }
    #[test]
    fn calibrated_remapped_inputs_leave_unassigned_channels_alone() {
        let mut rc = RcInput::default();
        assert!(rc.channels(Family::Plane, [0, 0, 500, 0], &[]).is_none());
        params(
            &mut rc,
            &[
                ("RCMAP_ROLL", 6.0),
                ("RC6_MIN", 1100.0),
                ("RC6_MAX", 1900.0),
                ("RC6_TRIM", 1400.0),
                ("RC9_TRIM", 1600.0),
            ],
        );
        let channels = rc
            .channels(Family::Plane, [1000, 500, 0, -1000], &[(9, 0)])
            .unwrap();
        assert_eq!(
            &channels[..9],
            &[65535, 1000, 1000, 1000, 65535, 1650, 65535, 65535, 1600]
        );
        assert!(
            rc.channels(Family::Plane, [0; 4], &[(6, 0)]).is_none(),
            "auxiliary cannot replace roll"
        );
        rc.note(&Param {
            name: "RC6_TRIM".into(),
            value: 1450.0,
            kind: crate::sim::REAL32,
        });
        assert_eq!(
            rc.channels(Family::Plane, [0, 0, 500, 0], &[]).unwrap()[5],
            1450
        );
    }
    #[test]
    fn blimp_throttle_is_bidirectional_and_forward_is_positive() {
        let mut rc = RcInput::default();
        params(&mut rc, &[]);
        for (z, pwm) in [(0, 1000), (500, 1500), (1000, 2000)] {
            let channels = rc.channels(Family::Blimp, [1000, 0, z, 0], &[]).unwrap();
            assert_eq!(channels[1], 2000);
            assert_eq!(channels[2], pwm);
        }
        let rover = rc
            .channels(Family::Rover, [1000, 0, 500, 1000], &[])
            .unwrap();
        assert_eq!(&rover[..4], &[1500, 65535, 1500, 65535]);
    }
    #[test]
    fn invalid_calibration_or_channel_map_is_not_sent() {
        let mut rc = RcInput::default();
        for invalid in [
            ("RCMAP_ROLL", 3.0),
            ("RCMAP_ROLL", 17.0),
            ("RC1_MIN", f32::NAN),
            ("RC1_MAX", 900.0),
        ] {
            params(&mut rc, &[invalid]);
            assert!(rc.channels(Family::Blimp, [0, 0, 500, 0], &[]).is_none());
        }
    }
}

#[cfg(test)]
mod pilot_method_tests {
    use super::*;
    fn input(values: &[(&str, f32)]) -> RcInput {
        let mut input = RcInput::default();
        input.downloaded(
            values
                .iter()
                .map(|(name, value)| Param {
                    name: (*name).into(),
                    value: *value,
                    kind: crate::sim::REAL32,
                })
                .collect::<Vec<_>>()
                .iter(),
        );
        input
    }
    #[test]
    fn sub_uses_forward_lateral_depth_yaw_and_honours_their_maps() {
        let rc = input(&[]);
        let ch = rc
            .channels(Family::Sub, [1000, -1000, 0, 1000], &[(9, 0)])
            .unwrap();
        assert_eq!(
            &ch[..9],
            &[65535, 65535, 1000, 2000, 2000, 1000, 65535, 65535, 1500]
        );
        assert!(
            rc.channels(Family::Sub, [0, 0, 500, 0], &[(5, 0)])
                .is_none()
        );
        let rc = input(&[
            ("RCMAP_FORWARD", 10.0),
            ("RC10_TRIM", 1600.0),
            ("RC10_MAX", 1900.0),
        ]);
        let ch = rc.channels(Family::Sub, [500, 0, 500, 0], &[]).unwrap();
        assert_eq!(ch[9], 1750);
        assert_eq!(ch[4], 65535);
        assert!(
            input(&[("RCMAP_FORWARD", 6.0)])
                .channels(Family::Sub, [0; 4], &[])
                .is_none()
        );
    }
    #[test]
    fn px4_requires_its_own_map_and_leaves_unspecified_channels_invalid() {
        assert!(input(&[]).channels(Family::Px4, [0; 4], &[]).is_none());
        let rc = input(&[
            ("RC_MAP_ROLL", 6.0),
            ("RC_MAP_PITCH", 2.0),
            ("RC_MAP_THROTTLE", 3.0),
            ("RC_MAP_YAW", 4.0),
            ("RC6_MIN", 1100.0),
            ("RC6_MAX", 1900.0),
            ("RC6_TRIM", 1400.0),
        ]);
        let ch = rc
            .channels(Family::Px4, [1000, 500, 0, -1000], &[(9, 0)])
            .unwrap();
        assert_eq!(&ch[..9], &[0, 2000, 1000, 1000, 0, 1650, 0, 0, 1500]);
    }
}
