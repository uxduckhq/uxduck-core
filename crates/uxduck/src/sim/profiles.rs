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
    programs: BTreeMap<String, Program>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Program {
    frames: BTreeMap<String, BTreeMap<String, f32>>,
}

pub(super) fn defaults(program: &str, frame: &str) -> Result<Option<String>, String> {
    let profiles: Profiles = serde_json::from_str(SOURCE)
        .map_err(|error| format!("invalid embedded simulation profiles: {error}"))?;
    if profiles.schema_version != 1 {
        return Err("unsupported embedded simulation profile schema".into());
    }
    let selected = profiles
        .programs
        .get(program)
        .ok_or_else(|| format!("missing simulation profile for {program}"))?;
    let Some(parameters) = selected.frames.get(frame) else {
        // Unreviewed frames explicitly retain the firmware's own defaults.
        return Ok(None);
    };
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
                "invalid simulation profile parameter {program}/{frame}/{name}"
            ));
        }
        writeln!(text, "{name} {value}").map_err(|error| error.to_string())?;
    }
    Ok(Some(text))
}

/// Append UXDuck's embedded profile after any explicitly supplied frame defaults.
/// The file is generated before starting SITL, and a write failure prevents launch.
pub(super) fn with_defaults(
    directory: &Path,
    program: &str,
    frame: &str,
    upstream: Option<String>,
) -> Result<Option<String>, String> {
    let Some(text) = defaults(program, frame)? else {
        return Ok(upstream);
    };
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

    #[test]
    fn profile_files_follow_upstream_defaults_and_fail_if_the_directory_is_missing() {
        let directory = tempfile::tempdir().expect("profile directory");
        let path = directory.path().join("uxduck-profile.parm");
        assert_eq!(
            with_defaults(
                directory.path(),
                "ardurover",
                "rover",
                Some("base.parm,frame.parm".into())
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
                None
            )
            .is_err()
        );
        assert_eq!(
            with_defaults(
                directory.path(),
                "arducopter",
                "X",
                Some("copter.parm".into())
            ),
            Ok(Some("copter.parm".into()))
        );
    }
}
