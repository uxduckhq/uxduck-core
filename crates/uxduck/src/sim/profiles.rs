//! Simulation-only defaults shared with the browser's ArduPilot worker.
//! Upstream frame defaults load first; EEPROM and snapshot values still win.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use serde::Deserialize;
use uxduck_core::domain::link::{ArdupilotGimbal, PayloadProfile};

const SOURCE: &str = include_str!("profiles/ardupilot.json");

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Profiles {
    schema_version: u8,
    /// Simulated traffic, when a simulation asks for it (ADR-0034):
    /// ArduPilot's own ADS-B simulator on serial 1, streamed to UXDuck,
    /// with avoidance off so the vehicle flies as it would without it.
    traffic: BTreeMap<String, f32>,
    /// A simulated camera, when a simulation asks for one (ADR-0063 Y3):
    /// the autopilot's camera 1 is a MAVLink camera, the payload's.
    camera: BTreeMap<String, f32>,
    /// A simulated gimbal, when a simulation asks for one: the autopilot's
    /// mount 1 is a MAVLink gimbal, the payload's, which the autopilot
    /// manages. Never without the payload: the vehicle would wait for a
    /// gimbal that is not there and refuse to arm.
    gimbal: BTreeMap<String, f32>,
    /// The gimbals ArduPilot simulates itself (ADR-0063 Y4), by the name
    /// `sim/start` takes: `viewpro` and `avt-cm62`.
    ardupilot_gimbals: BTreeMap<String, OwnGimbal>,
    /// The models of ArduPilot's simulator that run none of them, as
    /// `--model` names them. A frame is one of them when its name starts
    /// with the model's, as ArduPilot matches a model. Their physics never
    /// runs a simulated device on a serial port: `SIM_Sailboat`, which the
    /// motorboat is too, `SIM_BalanceBot`, `SIM_SingleCopter` and
    /// `SIM_Calibration` do not call `update_external_payload` (ArduPilot
    /// `d5420ff97c84`). The autopilot would be told of a mount it cannot
    /// find, and would not arm.
    ardupilot_gimbals_not_on: Vec<String>,
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

/// A gimbal ArduPilot simulates itself: a simulated device on one of the
/// autopilot's serial ports, and what the autopilot is told so that its own
/// driver finds the device there.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnGimbal {
    /// ArduPilot's name for the simulated device, as
    /// `--serial2=sim:<device>` takes it.
    device: String,
    /// The mount's type, the camera's type and the serial port's protocol.
    parameters: BTreeMap<String, f32>,
    /// The autopilot drives it as a MAVLink gimbal, so a program's own
    /// `gimbal` parameters go with it: Plane and Rover send a MAVLink gimbal
    /// its targets only at a faster loop (S400).
    #[serde(default)]
    mavlink: bool,
}

/// The autopilot's serial port a gimbal of ArduPilot's own is on. Serial 1
/// is simulated traffic's.
pub(super) const OWN_GIMBAL_SERIAL: u8 = 2;

/// The name `sim/start` and the profile file give a gimbal of ArduPilot's own.
fn own_gimbal_key(model: ArdupilotGimbal) -> &'static str {
    match model {
        ArdupilotGimbal::Viewpro => "viewpro",
        ArdupilotGimbal::AvtCm62 => "avt-cm62",
    }
}

fn own_gimbal(profiles: &Profiles, model: ArdupilotGimbal) -> Result<&OwnGimbal, String> {
    let key = own_gimbal_key(model);
    let gimbal = profiles
        .ardupilot_gimbals
        .get(key)
        .ok_or_else(|| format!("missing simulation profile for ArduPilot's {key} gimbal"))?;
    let named = !gimbal.device.is_empty()
        && gimbal
            .device
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_');
    // The device answers on the port its protocol is set for, or not at all.
    let on_its_port = gimbal
        .parameters
        .contains_key(&format!("SERIAL{OWN_GIMBAL_SERIAL}_PROTOCOL"));
    if !named || !on_its_port {
        return Err(format!(
            "invalid simulation profile for ArduPilot's {key} gimbal"
        ));
    }
    Ok(gimbal)
}

/// ArduPilot's name for the simulated device of one of its own gimbals: what
/// `--serial2=sim:<device>` takes, and what a build that has the device
/// holds as text.
pub(super) fn own_gimbal_device(model: ArdupilotGimbal) -> Result<String, String> {
    let profiles = profiles()?;
    own_gimbal(&profiles, model).map(|gimbal| gimbal.device.clone())
}

/// Whether ArduPilot's simulator runs its own gimbals on a frame: not on one
/// whose model runs no simulated device.
pub(super) fn runs_own_gimbals(frame: &str) -> Result<bool, String> {
    let frame = frame.to_ascii_lowercase();
    Ok(!profiles()?
        .ardupilot_gimbals_not_on
        .iter()
        .any(|model| frame.starts_with(model.as_str())))
}

/// The parameters that tell the autopilot of a gimbal ArduPilot simulates
/// itself (ADR-0063 Y4): its mount, its camera and the serial port they are
/// on. Defaults, read as the autopilot starts, as the payload's are.
pub(super) fn own_gimbal_defaults(program: &str, model: ArdupilotGimbal) -> Result<String, String> {
    let profiles = profiles()?;
    let gimbal = own_gimbal(&profiles, model)?;
    let key = own_gimbal_key(model);
    let mut text = lines(&gimbal.parameters, &format!("ardupilotGimbals/{key}"))?;
    if gimbal.mavlink
        && let Some(program_gimbal) = profiles.programs.get(program).map(|p| &p.gimbal)
    {
        text.push_str(&lines(program_gimbal, &format!("{program}/gimbal"))?);
    }
    Ok(text)
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
    /// More for this program when it carries a simulated gimbal. Plane and
    /// Rover run their loop at 50 Hz, where the autopilot finds the gimbal
    /// and never sends it a target (S400).
    #[serde(default)]
    gimbal: BTreeMap<String, f32>,
    frames: BTreeMap<String, BTreeMap<String, f32>>,
}

/// What a simulation asked for beyond its frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Extras {
    pub traffic: bool,
    pub camera: bool,
    pub gimbal: bool,
    /// A gimbal ArduPilot simulates itself, in place of the two above.
    pub ardupilot_gimbal: Option<ArdupilotGimbal>,
    /// What kind the camera and the gimbal are (ADR-0063 Y16).
    pub payload_profile: Option<PayloadProfile>,
}

impl Extras {
    /// Whether the autopilot is told of a camera and of a mount. A camera
    /// that only sends video has nothing for an autopilot to trigger or
    /// set, so the autopilot has no camera. An integrated camera and
    /// gimbal manages its own gimbal, so the autopilot has no mount: told
    /// of one, it would wait for a gimbal device that no component is.
    fn autopilot_has(self) -> (bool, bool) {
        match self.payload_profile {
            Some(PayloadProfile::StreamOnly) => (false, self.gimbal),
            Some(PayloadProfile::Integrated | PayloadProfile::ThermalTracking) => {
                (self.camera, false)
            }
            Some(PayloadProfile::PhotoVideo) | None => (self.camera, self.gimbal),
        }
    }
}

/// The parameters that tell the autopilot of the simulated camera and
/// gimbal it carries (ADR-0063 Y3). They are defaults, read as the autopilot
/// starts: set later they wait for a restart.
pub(super) fn payload(program: &str, camera: bool, gimbal: bool) -> Result<String, String> {
    let profiles = profiles()?;
    let mut text = String::new();
    if camera {
        text.push_str(&lines(&profiles.camera, "camera")?);
    }
    if gimbal {
        text.push_str(&lines(&profiles.gimbal, "gimbal")?);
        if let Some(program_gimbal) = profiles.programs.get(program).map(|p| &p.gimbal) {
            text.push_str(&lines(program_gimbal, &format!("{program}/gimbal"))?);
        }
    }
    Ok(text)
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

/// Append UXDuck's embedded profile, and the parameters of simulated
/// traffic, a camera, a gimbal and a gimbal of ArduPilot's own when asked
/// for, after any explicitly supplied frame defaults. The file is generated before starting SITL, and
/// a write failure prevents launch.
pub(super) fn with_defaults(
    directory: &Path,
    program: &str,
    frame: &str,
    upstream: Option<String>,
    extras: Extras,
) -> Result<Option<String>, String> {
    let mut text = defaults(program, frame)?.unwrap_or_default();
    if extras.traffic {
        text.push_str(&self::traffic()?);
    }
    let (camera, mount) = extras.autopilot_has();
    text.push_str(&payload(program, camera, mount)?);
    if let Some(model) = extras.ardupilot_gimbal {
        text.push_str(&own_gimbal_defaults(program, model)?);
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

    const TRAFFIC: Extras = Extras {
        traffic: true,
        camera: false,
        gimbal: false,
        ardupilot_gimbal: None,
        payload_profile: None,
    };

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
            Extras::default(),
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
                Extras::default()
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
                Extras::default()
            )
            .is_err()
        );
        assert_eq!(
            with_defaults(
                directory.path(),
                "arducopter",
                "X",
                Some("copter.parm".into()),
                Extras::default()
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
            with_defaults(directory.path(), "arducopter", "X", None, TRAFFIC),
            Ok(Some(path.display().to_string()))
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("written profile"),
            "ADSB_TYPE 1\nAVD_ENABLE 0\nMAV1_ADSB 5\nSERIAL1_PROTOCOL 1\nSIM_ADSB_ALT 300\nSIM_ADSB_COUNT 5\nSIM_ADSB_RADIUS 3000\n"
        );
        with_defaults(directory.path(), "ardurover", "rover", None, TRAFFIC).expect("rover");
        let rover = std::fs::read_to_string(&path).expect("written profile");
        assert!(rover.starts_with("ACRO_TURN_RATE 60\n") && rover.contains("SIM_ADSB_COUNT 5\n"));
    }

    /// ADR-0063 Y4: a gimbal ArduPilot simulates itself is a device on the
    /// autopilot's serial port 2, and the autopilot is told of its
    /// mount, its camera and that port's protocol. Only the one it drives
    /// over MAVLink asks Plane and Rover for the faster loop.
    #[test]
    fn ardupilots_own_gimbals_are_a_device_on_a_serial_port_with_their_parameters() {
        use ArdupilotGimbal::{AvtCm62, Viewpro};
        assert_eq!(own_gimbal_device(Viewpro), Ok("viewpro".into()));
        assert_eq!(own_gimbal_device(AvtCm62), Ok("avt_cm62_gimbal".into()));
        // The profile names each as sim/start does, and names no other.
        let named: Vec<String> = [AvtCm62, Viewpro]
            .into_iter()
            .map(|model| {
                assert_eq!(
                    serde_json::to_value(model).expect("a gimbal's name"),
                    serde_json::json!(own_gimbal_key(model))
                );
                own_gimbal_key(model).to_owned()
            })
            .collect();
        let profiles = profiles().expect("the profiles");
        assert_eq!(
            profiles
                .ardupilot_gimbals
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            named
        );

        // ArduPilot's boats, balance bot and single-rotor models run no
        // simulated device, and a frame is told by how its name starts.
        for frame in [
            "quad",
            "heli",
            "plane-jet",
            "quadplane",
            "rover",
            "rover-skid",
            "vectored",
        ] {
            assert_eq!(runs_own_gimbals(frame), Ok(true), "{frame}");
        }
        for frame in [
            "motorboat",
            "motorboat-skid",
            "sailboat",
            "sailboat-motor",
            "balancebot",
            "singlecopter",
            "coaxcopter",
            "calibration",
        ] {
            assert_eq!(runs_own_gimbals(frame), Ok(false), "{frame}");
        }

        let viewpro = "CAM1_TYPE 4\nMNT1_TYPE 11\nSERIAL2_PROTOCOL 8\n";
        let cm62 = "CAM1_TYPE 6\nMNT1_TYPE 6\nSERIAL2_PROTOCOL 2\n";
        for program in ["arducopter", "arducopter-heli", "ardusub"] {
            assert_eq!(own_gimbal_defaults(program, Viewpro), Ok(viewpro.into()));
            assert_eq!(own_gimbal_defaults(program, AvtCm62), Ok(cm62.into()));
        }
        for program in ["arduplane", "ardurover"] {
            assert_eq!(own_gimbal_defaults(program, Viewpro), Ok(viewpro.into()));
            assert_eq!(
                own_gimbal_defaults(program, AvtCm62),
                Ok(format!("{cm62}SCHED_LOOP_RATE 100\n"))
            );
        }

        // After upstream's defaults, which they must win over.
        let directory = tempfile::tempdir().expect("profile directory");
        let path = directory.path().join("uxduck-profile.parm");
        let carrying = |model| Extras {
            ardupilot_gimbal: Some(model),
            ..Extras::default()
        };
        assert_eq!(
            with_defaults(
                directory.path(),
                "arducopter",
                "X",
                Some("copter.parm".into()),
                carrying(Viewpro)
            ),
            Ok(Some(format!("copter.parm,{}", path.display())))
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("written profile"),
            viewpro
        );
        // A rover keeps its steering profile and gains the gimbal's lines.
        with_defaults(
            directory.path(),
            "ardurover",
            "rover",
            None,
            carrying(AvtCm62),
        )
        .expect("rover");
        let written = std::fs::read_to_string(&path).expect("written profile");
        assert!(
            written.ends_with(&format!("{cm62}SCHED_LOOP_RATE 100\n")),
            "{written}"
        );
        assert!(
            written.len() > cm62.len() + 20,
            "the rover's own profile is kept: {written}"
        );
    }

    /// ADR-0063 Y3, on S400's findings: the autopilot is told of a camera
    /// or a gimbal only when the simulation carries one, and Plane and Rover
    /// then run their loop at 100 Hz, where they send the gimbal its targets.
    #[test]
    fn a_camera_and_a_gimbal_are_named_only_when_the_simulation_carries_them() {
        for program in ["arducopter", "arducopter-heli", "ardusub"] {
            assert_eq!(payload(program, false, false), Ok(String::new()));
            assert_eq!(payload(program, true, false), Ok("CAM1_TYPE 6\n".into()));
            assert_eq!(payload(program, false, true), Ok("MNT1_TYPE 6\n".into()));
            assert_eq!(
                payload(program, true, true),
                Ok("CAM1_TYPE 6\nMNT1_TYPE 6\n".into())
            );
        }
        for program in ["arduplane", "ardurover"] {
            assert_eq!(payload(program, false, false), Ok(String::new()));
            // A camera alone asks nothing of the loop.
            assert_eq!(payload(program, true, false), Ok("CAM1_TYPE 6\n".into()));
            assert_eq!(
                payload(program, true, true),
                Ok("CAM1_TYPE 6\nMNT1_TYPE 6\nSCHED_LOOP_RATE 100\n".into())
            );
        }

        let directory = tempfile::tempdir().expect("profile directory");
        let path = directory.path().join("uxduck-profile.parm");
        let carrying = |camera, gimbal| Extras {
            traffic: false,
            camera,
            gimbal,
            ardupilot_gimbal: None,
            payload_profile: None,
        };
        // Neither: a copter keeps upstream's file alone, and nothing is written.
        assert_eq!(
            with_defaults(
                directory.path(),
                "arducopter",
                "X",
                Some("copter.parm".into()),
                Extras::default()
            ),
            Ok(Some("copter.parm".into()))
        );
        assert!(!path.exists());
        // Both: after upstream's defaults, which they must win over.
        assert_eq!(
            with_defaults(
                directory.path(),
                "arducopter",
                "X",
                Some("copter.parm".into()),
                carrying(true, true)
            ),
            Ok(Some(format!("copter.parm,{}", path.display())))
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("written profile"),
            "CAM1_TYPE 6\nMNT1_TYPE 6\n"
        );
        // A rover with a gimbal keeps its steering profile and gains the loop rate.
        with_defaults(
            directory.path(),
            "ardurover",
            "rover",
            None,
            carrying(false, true),
        )
        .expect("rover");
        assert_eq!(
            std::fs::read_to_string(&path).expect("written profile"),
            "ACRO_TURN_RATE 60\nMANUAL_OPTIONS 1\nMANUAL_STR_EXPO 0.35\nMNT1_TYPE 6\nSCHED_LOOP_RATE 100\n"
        );
    }

    /// ADR-0063 Y16: the kind of camera and gimbal decides what the
    /// autopilot is told of. A camera that only sends video is not one the
    /// autopilot can trigger, and a camera and gimbal in one manages its own
    /// gimbal, so the autopilot has no mount to wait for.
    #[test]
    fn the_autopilot_is_told_of_what_each_kind_of_camera_and_gimbal_leaves_to_it() {
        let directory = tempfile::tempdir().expect("profile directory");
        let path = directory.path().join("uxduck-profile.parm");
        let written = |camera, gimbal, payload_profile| {
            let _ = std::fs::remove_file(&path);
            with_defaults(
                directory.path(),
                "arducopter",
                "X",
                None,
                Extras {
                    traffic: false,
                    camera,
                    gimbal,
                    ardupilot_gimbal: None,
                    payload_profile,
                },
            )
            .expect("defaults");
            std::fs::read_to_string(&path).ok()
        };
        let both = Some("CAM1_TYPE 6\nMNT1_TYPE 6\n".to_owned());
        assert_eq!(written(true, true, None), both);
        assert_eq!(written(true, true, Some(PayloadProfile::PhotoVideo)), both);
        assert_eq!(
            written(true, true, Some(PayloadProfile::StreamOnly)),
            Some("MNT1_TYPE 6\n".into())
        );
        // Nothing for the autopilot, so no file is written at all.
        assert_eq!(written(true, false, Some(PayloadProfile::StreamOnly)), None);
        assert_eq!(
            written(true, true, Some(PayloadProfile::Integrated)),
            Some("CAM1_TYPE 6\n".into())
        );
    }
}
