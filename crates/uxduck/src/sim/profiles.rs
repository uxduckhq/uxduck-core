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
    /// A frame's own physics, for SITL's `--model <frame>:<file>.json`:
    /// `ArduPlane`'s plane model reads its aerodynamic coefficients from
    /// it, keeping the frame's own for any not given.
    physics: BTreeMap<String, BTreeMap<String, BTreeMap<String, f32>>>,
}

/// The physics file, written beside the profile in the simulator's working
/// directory, which SITL runs in. A relative name: SITL's plane model reads
/// its variants from anywhere in the model's text, so a path holding
/// `-ice` or `-3d` would change how it flies.
const PHYSICS: &str = "uxduck-physics.json";

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
    let launcher = frame.rsplit_once('-').and_then(|(base, launcher)| {
        (base.starts_with("plane")
            && base != "plane-tailsitter"
            && super::LAUNCHERS.contains(&launcher))
        .then(|| format!("plane-{launcher}"))
    });
    let Some(parameters) = selected.frames.get(frame).or_else(|| {
        launcher
            .as_ref()
            .and_then(|frame| selected.frames.get(frame))
    }) else {
        // Unreviewed frames explicitly retain the firmware's own defaults.
        return Ok(None);
    };
    lines(parameters, &format!("{program}/{frame}")).map(Some)
}

/// The model SITL runs a frame as: the frame with UXDuck's physics, written
/// into `directory`, when the shared profile gives it some, else `model`.
pub(super) fn with_physics(
    directory: &Path,
    program: &str,
    frame: &str,
    model: String,
) -> Result<String, String> {
    let profiles = profiles()?;
    let Some(coefficients) = profiles.physics.get(program).and_then(|p| p.get(frame)) else {
        return Ok(model);
    };
    if model != frame {
        return Err(format!(
            "{program}/{frame} runs {model}, which UXDuck's physics cannot replace"
        ));
    }
    if coefficients.is_empty()
        || coefficients.iter().any(|(name, value)| {
            name.is_empty()
                || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                || !value.is_finite()
        })
    {
        return Err(format!("invalid simulation physics {program}/{frame}"));
    }
    let text = serde_json::to_string(coefficients).map_err(|error| error.to_string())?;
    let path = directory.join(PHYSICS);
    std::fs::write(&path, text).map_err(|error| {
        format!(
            "cannot write simulation physics {}: {error}",
            path.display()
        )
    })?;
    Ok(format!("{frame}:{PHYSICS}"))
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
    fn launcher_auto_takeoff_waits_for_release_acceleration() {
        for base in ["plane", "plane-elevon", "plane-jet"] {
            for launcher in super::super::LAUNCHERS {
                assert_eq!(
                    defaults("arduplane", &format!("{base}-{launcher}")),
                    Ok(Some("TKOFF_THR_MINACC 3\n".into()))
                );
            }
        }
        for frame in [
            "plane",
            "plane-elevon",
            "plane-tailsitter-catapult",
            "quadplane-catapult",
        ] {
            assert_eq!(defaults("arduplane", frame), Ok(None));
        }
        assert_eq!(defaults("arducopter", "plane-catapult"), Ok(None));
        let directory = tempfile::tempdir().expect("profile directory");
        let files = with_defaults(
            directory.path(),
            "arduplane",
            "plane-elevon-catapult",
            Some("@ROMFS/default_params/plane.parm".into()),
            false,
        )
        .expect("launcher defaults")
        .expect("profile files");
        assert!(files.starts_with("@ROMFS/default_params/plane.parm,"));
        assert_eq!(
            std::fs::read_to_string(directory.path().join("uxduck-profile.parm")).unwrap(),
            "TKOFF_THR_MINACC 3\n"
        );
    }

    #[test]
    fn supported_programs_and_unreviewed_frames_keep_upstream_defaults() {
        for program in ["arducopter", "arduplane", "ardurover", "ardusub", "blimp"] {
            assert_eq!(defaults(program, "unreviewed"), Ok(None));
        }
        assert!(defaults("missing", "rover").is_err());
    }

    #[test]
    /// The seabed lies under a submarine's home: no offset from it at arming (ADR-0043).
    fn simulated_submarines_ask_for_the_seabed_and_sound_it() {
        for frame in ["vectored", "vectored_6dof"] {
            assert_eq!(
                defaults("ardusub", frame),
                Ok(Some(
                    "RNGFND1_MAX 50\nRNGFND1_PIN 0\nRNGFND1_SCALING 12.12\nRNGFND1_TYPE 1\nTERRAIN_ENABLE 1\nTERRAIN_OFS_MAX 0\nTERRAIN_SPACING 30\n".into()
                ))
            );
        }
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

    /// The jet (S143) flies as the Reaper does: cruise 200 m/s and 220 at
    /// most (790 km/h), against the plane's 22, banking to 70° so it can turn
    /// onto its circle (its tightest turn at 55° is 2.9 km across), on `ArduPlane`'s jet limits
    /// with roll and height tuning that does not wobble (upstream's rate
    /// gains oscillated), a 4 km circle, a climb of 2 km in about 35 s,
    /// height kept by pitch alone so it never trades height for speed near
    /// the ground, a battery gauge that lasts as long as the old cruise's,
    /// and `ArduPlane`'s default throttle slew rate, so a long
    /// take-off roll from a high field does not hold full throttle after
    /// lift-off; the plane keeps its own.
    #[test]
    fn the_jet_profile_is_faster_than_the_plane_and_the_plane_keeps_upstream() {
        let jet = defaults("arduplane", "plane-jet")
            .expect("a valid profile")
            .expect("a jet profile");
        for line in [
            "AIRSPEED_CRUISE 200\n",
            "AIRSPEED_MAX 220\n",
            "AIRSPEED_MIN 25\n",
            "BATT_CAPACITY 45000\n",
            "THR_MAX 100\n",
            "THR_SLEWRATE 100\n",
            "ROLL_LIMIT_DEG 70\n",
            "LEVEL_ROLL_LIMIT 0\n",
            "RLL_RATE_P 0.08\n",
            "TECS_SINK_MIN 2\n",
            "TECS_TIME_CONST 7\n",
            "TECS_PTCH_DAMP 0.8\n",
            "TECS_VERT_ACC 15\n",
            "TECS_SPDWEIGHT 0\n",
            "TECS_CLMB_MAX 100\n",
            "TECS_SINK_MAX 30\n",
            "PTCH_LIM_MAX_DEG 45\n",
            "PTCH_LIM_MIN_DEG -25\n",
            "NAVL1_PERIOD 25\n",
            "WP_LOITER_RAD 4000\n",
            "TKOFF_DIST 6000\n",
        ] {
            assert!(jet.contains(line), "{line}");
        }
        assert_eq!(defaults("arduplane", "plane"), Ok(None));
        assert_eq!(defaults("arduplane", "quadplane"), Ok(None));
    }

    /// The jet's drag is low enough for 790 km/h at full throttle; its
    /// physics is a file beside the profile, named relative to SITL's
    /// working directory, and every other frame flies its own.
    #[test]
    fn the_jet_flies_uxducks_physics_and_other_frames_their_own() {
        let directory = tempfile::tempdir().expect("physics directory");
        assert_eq!(
            with_physics(
                directory.path(),
                "arduplane",
                "plane-jet",
                "plane-jet".into()
            ),
            Ok("plane-jet:uxduck-physics.json".into())
        );
        assert_eq!(
            std::fs::read_to_string(directory.path().join(PHYSICS)).expect("written physics"),
            r#"{"c_drag_p":0.0236}"#
        );
        assert_eq!(
            with_physics(directory.path(), "arduplane", "plane", "plane".into()),
            Ok("plane".into())
        );
        assert_eq!(
            with_physics(
                directory.path(),
                "arducopter",
                "freestyle",
                "X:@ROMFS/models/freestyle.json".into()
            ),
            Ok("X:@ROMFS/models/freestyle.json".into())
        );
        assert!(
            with_physics(
                directory.path(),
                "arduplane",
                "plane-jet",
                "plane:other.json".into()
            )
            .is_err()
        );
        assert!(
            with_physics(
                &directory.path().join("missing"),
                "arduplane",
                "plane-jet",
                "plane-jet".into()
            )
            .is_err()
        );
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
