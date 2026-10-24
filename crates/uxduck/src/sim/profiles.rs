//! Simulation-only defaults shared with the browser's ArduPilot worker.
//! Upstream frame defaults load first; EEPROM and snapshot values still win.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use serde::Deserialize;

const SOURCE: &str = include_str!("profiles/ardupilot.json");

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Profiles {
    schema_version: u8,
    /// Simulated traffic, when a simulation asks for it (ADR-0034):
    /// ArduPilot's own ADS-B simulator on serial 1, streamed to UXDuck,
    /// with avoidance off so the vehicle flies as it would without it.
    traffic: BTreeMap<String, f32>,
    programs: BTreeMap<String, Program>,
}

fn profiles() -> Result<Profiles, String> {
    let profiles: Profiles = serde_json::from_str(SOURCE)
        .map_err(|error| format!("invalid embedded simulation profiles: {error}"))?;
    if profiles.schema_version != 1 {
        return Err("unsupported embedded simulation profile schema".into());
    }
    Ok(profiles)
}

/// One `NAME value` line per parameter, refusing a name ArduPilot could not hold.
fn lines(parameters: &BTreeMap<String, f32>, what: &str) -> Result<String, String> {
    let mut text = String::new();
    for (name, value) in parameters {
        if name.is_empty()
            || name.len() > 16
            || !name
                .bytes()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
            || !value.is_finite()
        {
            return Err(format!(
                "invalid simulation profile parameter {what}/{name}"
            ));
        }
        writeln!(text, "{name} {value}").map_err(|error| error.to_string())?;
    }
    Ok(text)
}

/// The parameters that give a simulation its simulated traffic.
pub(super) fn traffic() -> Result<String, String> {
    lines(&profiles()?.traffic, "traffic")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Program {
    frames: BTreeMap<String, BTreeMap<String, f32>>,
}

pub(super) fn defaults(program: &str, frame: &str) -> Result<Option<String>, String> {
    let profiles = profiles()?;
    let selected = profiles
        .programs
        .get(program)
        .ok_or_else(|| format!("missing simulation profile for {program}"))?;
    let Some(parameters) = selected.frames.get(frame) else {
        // Unreviewed frames explicitly retain the firmware's own defaults.
        return Ok(None);
    };
    lines(parameters, &format!("{program}/{frame}")).map(Some)
}

/// Append UXDuck's embedded profile, and simulated traffic's parameters when
/// asked for, after any explicitly supplied frame defaults. The file is
/// generated before starting SITL, and a write failure prevents launch.
pub(super) fn with_defaults(
    directory: &Path,
    program: &str,
    frame: &str,
    upstream: Option<String>,
    traffic: bool,
) -> Result<Option<String>, String> {
    let mut text = defaults(program, frame)?.unwrap_or_default();
    if traffic {
        text.push_str(&self::traffic()?);
    }
    if text.is_empty() {
        return Ok(upstream);
    }
    let path = directory.join("uxduck-profile.parm");
    std::fs::write(&path, text).map_err(|error| {
        format!(
            "cannot write simulation profile {}: {error}",
            path.display()
        )
    })?;
    let path = path.display().to_string();
    Ok(Some(upstream.map_or_else(
        || path.clone(),
        |files| format!("{files},{path}"),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_programs_and_unreviewed_frames_keep_upstream_defaults() {
        for program in ["arducopter", "arduplane", "ardurover", "ardusub", "blimp"] {
            assert_eq!(defaults(program, "unreviewed"), Ok(None));
        }
        assert!(defaults("missing", "rover").is_err());
    }

    #[test]
    fn car_and_boat_profiles_scale_steering_but_skid_frames_keep_their_mixing() {
        for frame in ["rover", "motorboat"] {
            assert_eq!(
                defaults("ardurover", frame),
                Ok(Some(
                    "ACRO_TURN_RATE 60\nMANUAL_OPTIONS 1\nMANUAL_STR_EXPO 0.35\n".into()
                ))
            );
        }
        for frame in ["rover-skid", "motorboat-skid"] {
            assert_eq!(
                defaults("ardurover", frame),
                Ok(Some("ACRO_TURN_RATE 60\nMANUAL_STR_EXPO 0.35\n".into()))
            );
        }
    }

    /// The jet (S143) cruises at 50 m/s, against the plane's 22, on
    /// `ArduPlane`'s jet limits with roll and height tuning that does not
    /// wobble (upstream's rate gains oscillated), and a circle, navigation
    /// period and TECS pitch damping that hold height at true airspeeds up
    /// to about 80 m/s, high up, and `ArduPlane`'s default throttle slew rate,
    /// so a long take-off roll from a high field does not hold full throttle
    /// after lift-off; the plane keeps its own.
    #[test]
    fn the_jet_profile_is_faster_than_the_plane_and_the_plane_keeps_upstream() {
        let jet = defaults("arduplane", "plane-jet")
            .expect("a valid profile")
            .expect("a jet profile");
        for line in [
            "AIRSPEED_CRUISE 50\n",
            "AIRSPEED_MAX 80\n",
            "AIRSPEED_MIN 25\n",
            "THR_MAX 100\n",
            "THR_SLEWRATE 100\n",
            "ROLL_LIMIT_DEG 55\n",
            "LEVEL_ROLL_LIMIT 0\n",
            "RLL_RATE_P 0.08\n",
            "TECS_SINK_MIN 2\n",
            "TECS_TIME_CONST 7\n",
            "TECS_PTCH_DAMP 0.8\n",
            "TECS_VERT_ACC 15\n",
            "PTCH_LIM_MAX_DEG 20\n",
            "PTCH_LIM_MIN_DEG -25\n",
            "NAVL1_PERIOD 25\n",
            "WP_LOITER_RAD 400\n",
        ] {
            assert!(jet.contains(line), "{line}");
        }
        assert_eq!(defaults("arduplane", "plane"), Ok(None));
        assert_eq!(defaults("arduplane", "quadplane"), Ok(None));
    }

    #[test]
    fn profile_files_follow_upstream_defaults_and_fail_if_the_directory_is_missing() {
        let directory = tempfile::tempdir().expect("profile directory");
        let path = directory.path().join("uxduck-profile.parm");
        assert_eq!(
            with_defaults(
                directory.path(),
                "ardurover",
                "rover",
                Some("base.parm,frame.parm".into()),
                false
            ),
            Ok(Some(format!("base.parm,frame.parm,{}", path.display())))
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("written profile"),
            "ACRO_TURN_RATE 60\nMANUAL_OPTIONS 1\nMANUAL_STR_EXPO 0.35\n"
        );
        assert!(
            with_defaults(
                &directory.path().join("missing"),
                "ardurover",
                "rover",
                None,
                false
            )
            .is_err()
        );
        assert_eq!(
            with_defaults(
                directory.path(),
                "arducopter",
                "X",
                Some("copter.parm".into()),
                false
            ),
            Ok(Some("copter.parm".into()))
        );
    }

    /// Simulated traffic only when asked for (ADR-0034): a copter that
    /// keeps upstream's defaults gets a profile of traffic alone.
    #[test]
    fn simulated_traffic_is_added_only_when_asked_for() {
        let directory = tempfile::tempdir().expect("profile directory");
        let path = directory.path().join("uxduck-profile.parm");
        assert_eq!(
            with_defaults(directory.path(), "arducopter", "X", None, true),
            Ok(Some(path.display().to_string()))
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("written profile"),
            "ADSB_TYPE 1\nAVD_ENABLE 0\nMAV1_ADSB 5\nSERIAL1_PROTOCOL 1\nSIM_ADSB_ALT 300\nSIM_ADSB_COUNT 5\nSIM_ADSB_RADIUS 3000\n"
        );
        with_defaults(directory.path(), "ardurover", "rover", None, true).expect("rover");
        let rover = std::fs::read_to_string(&path).expect("written profile");
        assert!(rover.starts_with("ACRO_TURN_RATE 60\n") && rover.contains("SIM_ADSB_COUNT 5\n"));
    }
}
