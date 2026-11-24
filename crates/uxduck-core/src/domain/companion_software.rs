//! UXDuck on a vehicle's companion computers (S297, ADR-0061 B1, B5, B9,
//! B10): which version each computer runs against this computer's, what the
//! features this ground station uses there need, and exactly what Install,
//! Update and Remove change, shown before anything runs.
//!
//! The version a check reads (`uxduck --version`) is compared with this
//! computer's. Older is either compatible (Update available) or missing a
//! feature the computer's own `/etc/uxduck/companion.toml` turns on (Update
//! needed). A computer's UXDuck has a feature when its version is at least
//! the one the feature shipped in, or when its `uxduck companion --help`
//! offers the option the feature came with, so a build from source between
//! releases counts for what it has.

use serde::{Deserialize, Serialize};

use super::companion::{CheckState, CompanionComputer, CompanionRole};

/// The service UXDuck installs on every computer it looks after. ADR-0066's
/// recording modes reload it by this name.
pub const UNIT: &str = "uxduck-companion.service";

/// The system user the program runs as (ADR-0061 B9).
pub const USER: &str = "uxduck";

/// Where the link to the installed version is.
pub const LINK: &str = "/usr/local/bin/uxduck";

/// Where each installed version is kept, one folder per version.
pub const VERSIONS: &str = "/opt/uxduck";

/// The file of the features the computer runs (ADR-0061 B4).
pub const CONFIG: &str = "/etc/uxduck/companion.toml";

/// The polkit rule letting the program's user restart its own service.
pub const POLKIT_RULE: &str = "/etc/polkit-1/rules.d/50-uxduck.rules";

/// The program's state folder, where ADR-0066's recordings are.
pub const STATE: &str = "/var/lib/uxduck";

/// How long the new version has to be running before what was there is
/// put back (ADR-0061 B5).
pub const START_WITHIN_S: u32 = 30;

/// What a computer's own setup says about UXDuck there, as a check reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SoftwareFound {
    /// Where `/usr/local/bin/uxduck` points, or `file` when it is a file
    /// someone put there. Absent when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// The versions kept under `/opt/uxduck`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kept: Vec<String>,
    /// Whether `/etc/uxduck/companion.toml` is there.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub config: bool,
    /// The features that file turns on, such as `sender` and `recorder`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uses: Vec<String>,
    /// What its UXDuck offers by option, such as `config`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub has: Vec<String>,
    /// The systemd services that run `uxduck companion` or `uxduck forward`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub units: Vec<FoundUnit>,
    /// Whether the user UXDuck signs in as may run `sudo` without a password.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sudo: Option<bool>,
    /// Its serial ports, for `uxduck forward`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub serial: Vec<String>,
    /// Whether it runs systemd.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub systemd: bool,
    /// `getconf LONG_BIT`: 32 on a 32-bit system, even on a 64-bit kernel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bits: Option<u8>,
    /// The units its UXDuck services start after, want, require or bind
    /// to that this computer has.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub present: Vec<String>,
    /// What UXDuck kept as `.before-uxduck` when it replaced it: service
    /// file names, and `/usr/local/bin/uxduck`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub before: Vec<String>,
}

/// A systemd service on the computer that runs UXDuck.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FoundUnit {
    /// Its file name, such as `uxduck-companion.service`.
    pub name: String,
    /// Written by UXDuck's Install.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub own: bool,
    /// Its `User=`. Absent, it runs as root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// It has drop-ins under `<name>.d`, which change what it runs.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub drop_ins: bool,
    /// Its `ExecStart=`, as written.
    pub exec: String,
    /// Its file's lines, then its drop-ins', as written, with no comments
    /// or blank lines: the sections and the directives.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<String>,
}

/// What Install, Update or Remove does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallAction {
    Install,
    Update,
    Remove,
}

impl InstallAction {
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Install => "Install",
            Self::Update => "Update",
            Self::Remove => "Remove",
        }
    }
}

/// How the last Install, Update or Remove went.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallState {
    Running,
    Done,
    /// The new version did not start, and what was there before was put
    /// back.
    RolledBack,
    Failed,
}

/// One step as it ran on the computer, with what it said.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallStep {
    pub title: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output: String,
}

/// The last Install, Update or Remove on a computer, kept with it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallRecord {
    pub action: InstallAction,
    pub state: InstallState,
    /// When it started or finished.
    pub at_us: u64,
    /// The version it installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<InstallStep>,
    /// Why it failed or was put back, for people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// A feature of `uxduck companion` that this ground station uses when the
/// computer's file turns it on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Feature {
    /// What the computer's file turns on, as a check reports it.
    pub id: &'static str,
    /// For people.
    pub name: &'static str,
    /// The release it shipped in.
    pub since: &'static str,
    /// The option of `uxduck companion` it came with, as a check reports
    /// it, for builds from source between releases.
    pub option: &'static str,
}

/// What this ground station expects of a computer's UXDuck, newest last.
pub const FEATURES: [Feature; 2] = [
    // S385, ADR-0064: WHEP from the computer, its bitrate following the link.
    Feature {
        id: "sender",
        name: "camera video that follows the connection",
        since: "0.21.0",
        option: "config",
    },
    // S428, ADR-0066: recording on the vehicle, which the Record menu uses.
    Feature {
        id: "recorder",
        name: "recording on the vehicle",
        since: "0.21.0",
        option: "config",
    },
];

/// A release version, `0.21.0` or `0.21.0-beta.2`, ordered as releases are:
/// a beta before its release.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    /// The beta number, or `u32::MAX` for the release itself.
    pub beta: u32,
}

impl Version {
    /// Reads `0.21.0`, `v0.21.0`, `uxduck 0.21.0` or `0.21.0-beta.2`, the
    /// first line only, as `uxduck --version` prints it.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let line = text.lines().next()?.trim();
        let line = line.strip_prefix("uxduck ").unwrap_or(line).trim();
        let line = line.strip_prefix('v').unwrap_or(line);
        let (numbers, beta) = match line.split_once('-') {
            Some((numbers, pre)) => (numbers, pre.strip_prefix("beta.")?.parse().ok()?),
            None => (line, u32::MAX),
        };
        let mut parts = numbers.split('.').map(str::parse::<u32>);
        let version = Self {
            major: parts.next()?.ok()?,
            minor: parts.next()?.ok()?,
            patch: parts.next()?.ok()?,
            beta,
        };
        parts.next().is_none().then_some(version)
    }
}

/// The Linux build a computer takes, from `uname -m` and `getconf LONG_BIT`,
/// or why none fits. The release builds `x86_64` and `aarch64` Linux only
/// (ADR-0061 B5).
///
/// # Errors
/// For people: why no build fits.
pub fn target(arch: &str, bits: Option<u8>) -> Result<&'static str, String> {
    let target = match arch {
        "aarch64" | "arm64" => "aarch64-unknown-linux-gnu",
        "x86_64" | "amd64" => "x86_64-unknown-linux-gnu",
        "armv7l" | "armv6l" | "armhf" | "i686" | "i386" => {
            return Err(format!(
                "It runs a 32-bit system ({arch}). UXDuck is built for 64-bit Linux only, so it needs a 64-bit system such as 64-bit Raspberry Pi OS."
            ));
        }
        other => {
            return Err(format!(
                "UXDuck has no build for its processor ({other}). It is built for 64-bit Arm and x86 Linux."
            ));
        }
    };
    if bits == Some(32) {
        return Err(format!(
            "Its kernel is 64-bit ({arch}) but its system is 32-bit. UXDuck needs a 64-bit system such as 64-bit Raspberry Pi OS."
        ));
    }
    Ok(target)
}

/// How a computer's UXDuck compares with this computer's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SoftwareStatus {
    NotChecked,
    Checking,
    /// The last check could not reach it.
    Unreachable,
    NotInstalled,
    UpToDate,
    /// Older, and has every feature its file turns on.
    UpdateAvailable,
    /// Older, and missing a feature its file turns on.
    UpdateNeeded,
    /// Newer than this computer's.
    Newer,
    /// No UXDuck build runs on it.
    Unsupported,
    /// Its `uxduck --version` is not a version UXDuck can compare.
    Unknown,
}

/// A computer's UXDuck against this computer's, and what is offered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Software {
    pub status: SoftwareStatus,
    /// Its version, as its last check read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// This computer's version.
    pub ground: String,
    /// The features its file turns on that its version does not have.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
    /// The status in a few words, for people and agents.
    pub words: String,
    /// What is offered: Install or Update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offer: Option<InstallAction>,
    /// Whether UXDuck installed it, so Remove is offered.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub managed: bool,
}

/// Whether UXDuck's Install put the UXDuck there: the link points into
/// `/opt/uxduck`.
#[must_use]
pub fn managed(found: &SoftwareFound) -> bool {
    found
        .link
        .as_deref()
        .is_some_and(|link| link.starts_with(&format!("{VERSIONS}/")))
}

/// A computer's UXDuck against `ground`, this computer's version.
#[must_use]
pub fn software(ground: &str, computer: &CompanionComputer) -> Software {
    let mut out = Software {
        status: SoftwareStatus::NotChecked,
        version: None,
        ground: ground.to_owned(),
        missing: Vec::new(),
        words: "Not checked yet".into(),
        offer: None,
        managed: false,
    };
    let Some(check) = &computer.check else {
        return out;
    };
    match check.state {
        CheckState::Checking => {
            out.status = SoftwareStatus::Checking;
            "Checking".clone_into(&mut out.words);
            return out;
        }
        CheckState::Failed => {
            out.status = SoftwareStatus::Unreachable;
            "Can't reach it. Check it to read its version.".clone_into(&mut out.words);
            return out;
        }
        CheckState::Reached => {}
    }
    let found = check.software.clone().unwrap_or_default();
    out.managed = managed(&found);
    out.version.clone_from(&check.uxduck);
    let builds = check
        .arch
        .as_deref()
        .map_or(Err("Its processor is not known yet.".to_owned()), |arch| {
            target(arch, found.bits)
        });
    let Some(theirs) = check.uxduck.as_deref() else {
        match builds {
            Ok(_) => {
                out.status = SoftwareStatus::NotInstalled;
                "Not installed".clone_into(&mut out.words);
                out.offer = Some(InstallAction::Install);
            }
            Err(why) => {
                out.status = SoftwareStatus::Unsupported;
                out.words = why;
            }
        }
        return out;
    };
    let offer = |action| builds.is_ok().then_some(action);
    let (Some(their), Some(ours)) = (Version::parse(theirs), Version::parse(ground)) else {
        out.status = SoftwareStatus::Unknown;
        out.words =
            format!("Runs UXDuck {theirs}, which this computer can't compare with {ground}");
        out.offer = offer(InstallAction::Update);
        return out;
    };
    if their > ours {
        out.status = SoftwareStatus::Newer;
        out.words = format!("Newer than this computer's UXDuck {ground}");
        return out;
    }
    if their == ours {
        out.status = SoftwareStatus::UpToDate;
        if out.managed {
            "Up to date".clone_into(&mut out.words);
        } else {
            "Up to date, installed by hand. Install lets UXDuck update it."
                .clone_into(&mut out.words);
            out.offer = offer(InstallAction::Install);
        }
        return out;
    }
    out.missing = FEATURES
        .iter()
        .filter(|f| found.uses.iter().any(|used| used == f.id))
        .filter(|f| {
            Version::parse(f.since).is_some_and(|since| their < since)
                && !found.has.iter().any(|has| has == f.option)
        })
        .map(|f| f.name.to_owned())
        .collect();
    out.offer = offer(InstallAction::Update);
    if out.missing.is_empty() {
        out.status = SoftwareStatus::UpdateAvailable;
        out.words = format!("Update available: {ground}");
    } else {
        out.status = SoftwareStatus::UpdateNeeded;
        out.words = format!("Update needed for {}", and_list(&out.missing));
    }
    out
}

fn and_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// What a person gives Install for a computer UXDuck has no service to
/// copy from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstallInputs {
    /// Where it sends its readings, `udp:HOST:PORT`.
    pub to: Option<String>,
    /// For a computer wired to the flight controller, its serial port,
    /// `serial:DEVICE@BAUD`.
    pub serial: Option<String>,
}

/// Exactly what Install, Update or Remove will do, shown before it runs
/// (ADR-0061 B9) and given to the computer as it is shown.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub action: InstallAction,
    /// The version it installs. Absent for Remove.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The build it copies, such as `aarch64-unknown-linux-gnu`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The steps, in order, for people.
    pub steps: Vec<String>,
    /// What it leaves as it is, for people.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keeps: Vec<String>,
    /// What the person should know first, such as MAVLink pausing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Whether MAVLink through this computer pauses while it restarts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mavlink_pauses: bool,
    /// The service's file, whole.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unit: String,
    /// The polkit rule, whole.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub polkit: String,
    /// What the service runs after `/usr/local/bin/uxduck`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// The groups the user joins, where the computer has them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    /// The hand-made services it replaces, by file name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replaces: Vec<String>,
}

/// A word the service's command line may hold: what `uxduck` takes, with
/// nothing systemd or a shell would read as something else.
fn plain_arg(arg: &str) -> bool {
    !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:/=@,+-[]".contains(c))
}

/// `udp:HOST:PORT`, as `uxduck companion --to` and `forward --out` take it.
fn check_to(to: &str) -> Result<(), String> {
    let ok = to.strip_prefix("udp:").is_some_and(|rest| {
        rest.rsplit_once(':').is_some_and(|(host, port)| {
            !host.is_empty() && port.parse::<u16>().is_ok_and(|p| p > 0)
        })
    }) && plain_arg(to);
    if ok {
        Ok(())
    } else {
        Err(format!(
            "\"{to}\" is not where to send. Give the vehicle's MAVLink router as udp:HOST:PORT, such as udp:192.168.50.21:15010."
        ))
    }
}

/// `serial:DEVICE@BAUD` or `serial:DEVICE`.
fn check_serial(serial: &str) -> Result<(), String> {
    let ok = serial
        .strip_prefix("serial:/dev/")
        .is_some_and(|rest| !rest.is_empty())
        && plain_arg(serial);
    if ok {
        Ok(())
    } else {
        Err(format!(
            "\"{serial}\" is not a serial port. Give it as serial:DEVICE@BAUD, such as serial:/dev/ttyAMA0@921600."
        ))
    }
}

/// What the program runs as, after the program: the hand-made or earlier
/// service's own arguments with this page's id, or new ones.
fn service_args(
    computer: &CompanionComputer,
    from: Option<&FoundUnit>,
    found: &SoftwareFound,
    inputs: &InstallInputs,
) -> Result<Vec<String>, String> {
    let id = computer.component_id.to_string();
    if let Some(unit) = from {
        let words: Vec<&str> = unit.exec.split_whitespace().collect();
        let start = words
            .iter()
            .position(|w| w.rsplit('/').next() == Some("uxduck"))
            .ok_or_else(|| {
                format!(
                    "{} does not start uxduck, so UXDuck can't carry over what it runs.",
                    unit.name
                )
            })?;
        let mut args: Vec<String> = words[start + 1..].iter().map(|w| (*w).to_owned()).collect();
        if let Some(bad) = args.iter().find(|a| !plain_arg(a)) {
            return Err(format!(
                "{} starts uxduck with \"{bad}\", which UXDuck can't carry over into its own service. Change it by hand, then check the computer again.",
                unit.name
            ));
        }
        match args.iter().position(|a| a == "--computer-id") {
            Some(at) if at + 1 < args.len() => args[at + 1].clone_from(&id),
            Some(_) => args.push(id),
            None => {
                let at = usize::from(!args.is_empty());
                args.splice(at..at, ["--computer-id".to_owned(), id]);
            }
        }
        if args.first().map(String::as_str) == Some("forward")
            && !args.iter().any(|a| a == "--onboard")
        {
            args.insert(1, "--onboard".into());
        }
        return Ok(args);
    }
    let to = inputs
        .to
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or(
            "Say where it sends its readings: the vehicle's MAVLink router, as udp:HOST:PORT.",
        )?;
    check_to(to)?;
    match computer.role {
        CompanionRole::Report => {
            let mut args = vec![
                "companion".into(),
                "--to".into(),
                to.into(),
                "--computer-id".into(),
                id,
            ];
            if found.config {
                args.extend(["--config".into(), CONFIG.into()]);
            }
            Ok(args)
        }
        CompanionRole::Forward => {
            let serial = inputs
                .serial
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or("Choose the serial port wired to the flight controller.")?;
            check_serial(serial)?;
            Ok(vec![
                "forward".into(),
                "--onboard".into(),
                "--computer-id".into(),
                id,
                "--in".into(),
                serial.into(),
                "--out".into(),
                to.into(),
            ])
        }
    }
}

/// `[Service]` directives that limit what the program may do or reach, kept
/// from the service UXDuck replaces so its own is never less limited.
const LIMITS: &[&str] = &[
    "IPAddressAllow",
    "IPAddressDeny",
    "StateDirectory",
    "StateDirectoryMode",
    "ReadOnlyPaths",
    "ReadWritePaths",
    "InaccessiblePaths",
    "SystemCallFilter",
    "SystemCallArchitectures",
    "MemoryDenyWriteExecute",
    "LockPersonality",
    "UMask",
];

/// Families of `[Service]` sandboxing directives, such as `ProtectSystem`.
const LIMIT_FAMILIES: &[&str] = &["Protect", "Private", "Restrict"];

/// `[Service]` directives UXDuck's own service sets, by name and value.
const OWN_SERVICE: &[(&str, &str)] = &[
    ("Type", "simple"),
    ("User", USER),
    ("Group", USER),
    ("ExecStart", ""),
    ("ExecReload", RELOAD),
    ("Restart", "always"),
    ("RestartSec", "2"),
    ("AmbientCapabilities", "CAP_SYS_TIME"),
    ("CapabilityBoundingSet", "CAP_SYS_TIME"),
    ("NoNewPrivileges", "yes"),
];

/// How the service reloads: `uxduck companion` reads its settings again on
/// SIGHUP, as a camera's recording mode change asks it to (S427).
const RELOAD: &str = "/bin/kill -HUP $MAINPID";

/// What UXDuck's service keeps of the one it replaces (S297): its limits,
/// its environment and the services it starts after, with what was not
/// kept, so an update never quietly loosens a service someone hardened.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Carried {
    /// The service it comes from.
    from: String,
    /// `[Unit]` lines.
    unit: Vec<String>,
    /// `[Service]` lines.
    service: Vec<String>,
    /// What it keeps, in words.
    keeps: Vec<String>,
    /// What it does not keep, in words.
    lost: Vec<String>,
}

/// Where `IPAddressAllow` lets a service reach, in words.
fn address_words(words: &[&str]) -> String {
    let mut out: Vec<String> = Vec::new();
    for word in words {
        let named = match *word {
            "localhost" => "this computer",
            "100.64.0.0/10" | "fd7a:115c:a1e0::/48" => "Tailscale",
            "link-local" => "the local link",
            "multicast" => "multicast",
            other => other,
        };
        if !out.iter().any(|o| o == named) {
            out.push(named.to_owned());
        }
    }
    and_list(&out)
}

#[allow(
    clippy::too_many_lines,
    reason = "each line of the service is sorted, then said in words, in one place"
)]
fn carry(unit: &FoundUnit, present: &[String]) -> Carried {
    let mut carried = Carried {
        from: unit.name.clone(),
        ..Carried::default()
    };
    let mut section = String::new();
    let mut allow: Vec<&str> = Vec::new();
    let mut deny: Vec<&str> = Vec::new();
    let mut sandbox: Vec<String> = Vec::new();
    let mut environment: Vec<String> = Vec::new();
    let mut after: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    let mut reloads = false;
    for line in &unit.lines {
        let line = line.trim();
        if line.starts_with('[') {
            line.clone_into(&mut section);
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            carried.lost.push(line.to_owned());
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match section.as_str() {
            "[Unit]" if ["After", "Wants", "Requires", "BindsTo"].contains(&key) => {
                let kept: Vec<&str> = value
                    .split_whitespace()
                    .filter(|w| *w != "network.target")
                    .filter(|w| {
                        let there = present.iter().any(|p| p == w);
                        if !there && !missing.iter().any(|m| m == w) {
                            missing.push((*w).to_owned());
                        }
                        there
                    })
                    .collect();
                if !kept.is_empty() {
                    carried.unit.push(format!("{key}={}", kept.join(" ")));
                    for w in kept {
                        if key == "After" && !after.iter().any(|a| a == w) {
                            after.push(w.to_owned());
                        }
                    }
                }
            }
            "[Unit]" if key == "Description" => {}
            "[Install]" if key == "WantedBy" && value == "multi-user.target" => {}
            "[Service]"
                if LIMITS.contains(&key) || LIMIT_FAMILIES.iter().any(|f| key.starts_with(f)) =>
            {
                carried.service.push(format!("{key}={value}"));
                match key {
                    "IPAddressAllow" => allow.extend(value.split_whitespace()),
                    "IPAddressDeny" => deny.extend(value.split_whitespace()),
                    _ => sandbox.push(format!("{key}={value}")),
                }
            }
            "[Service]" if key == "Environment" || key == "EnvironmentFile" => {
                carried.service.push(format!("{key}={value}"));
                environment.push(value.to_owned());
            }
            "[Service]" => match OWN_SERVICE.iter().find(|(k, _)| *k == key) {
                Some(("ExecReload", ours)) if value == *ours => reloads = true,
                Some(("ExecStart" | "User" | "Group" | "Type" | "Restart" | "RestartSec", _)) => {}
                Some((_, ours)) if value == *ours => {}
                Some((_, ours)) => carried
                    .lost
                    .push(format!("{key}={value} (UXDuck's service has {key}={ours})")),
                None => carried.lost.push(format!("{key}={value}")),
            },
            _ => carried.lost.push(format!("{section} {key}={value}")),
        }
    }
    let from = &unit.name;
    if !allow.is_empty() || !deny.is_empty() {
        let mut words = String::from("Its network limit from ");
        words.push_str(from);
        words.push_str(": ");
        if allow.is_empty() {
            words.push_str("no connections with ");
            words.push_str(&address_words(&deny));
        } else {
            words.push_str("viewers and every other connection only from ");
            words.push_str(&address_words(&allow));
            if !deny.contains(&"any") && !deny.is_empty() {
                words.push_str(", and none with ");
                words.push_str(&address_words(&deny));
            }
        }
        carried.keeps.push(words);
    }
    if !sandbox.is_empty() {
        carried
            .keeps
            .push(format!("Its sandbox from {from}: {}", and_list(&sandbox)));
    }
    if !environment.is_empty() {
        carried.keeps.push(format!(
            "Its environment from {from}: {}",
            and_list(&environment)
        ));
    }
    if !after.is_empty() {
        carried.keeps.push(format!(
            "Starting after {}, as {from} did",
            and_list(&after)
        ));
    }
    if reloads {
        carried
            .keeps
            .push("Reloading with SIGHUP, which a camera's recording mode change uses".into());
    }
    if !missing.is_empty() {
        carried.lost.push(format!(
            "waiting for {}, which this computer doesn't have",
            and_list(&missing)
        ));
    }
    carried
}

/// The service file Install writes, with what it keeps of the service it
/// replaces. Its first line marks it as UXDuck's.
fn unit_file(version: &str, args: &[String], carried: &Carried) -> String {
    let unit: String = carried.unit.iter().map(|l| l.clone() + "\n").collect();
    let service: String = if carried.service.is_empty() {
        String::new()
    } else {
        format!(
            "# Kept from {}, so this service is no less limited than it was.\n{}",
            carried.from,
            carried
                .service
                .iter()
                .map(|l| l.clone() + "\n")
                .collect::<String>()
        )
    };
    format!(
        "# Written by UXDuck {version} (S297). Install, Update and Remove on the vehicle's Setup page replace or delete this file.\n\
         [Unit]\n\
         Description=UXDuck on this companion computer\n\
         After=network.target\n\
         {unit}\
         \n\
         [Service]\n\
         Type=simple\n\
         User={USER}\n\
         Group={USER}\n\
         ExecStart={LINK} {}\n\
         ExecReload={RELOAD}\n\
         Restart=always\n\
         RestartSec=2\n\
         AmbientCapabilities=CAP_SYS_TIME\n\
         CapabilityBoundingSet=CAP_SYS_TIME\n\
         NoNewPrivileges=yes\n\
         {service}\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        args.join(" ")
    )
}

/// The polkit rule Install writes: the program's user may start, stop and
/// restart its own service and nothing else (ADR-0061 B9).
#[must_use]
pub fn polkit_rule(version: &str) -> String {
    format!(
        "// Written by UXDuck {version} (S297). Lets the {USER} user start, stop and restart {UNIT} and nothing else.\n\
         polkit.addRule(function(action, subject) {{\n\
         \x20   if (action.id == \"org.freedesktop.systemd1.manage-units\" &&\n\
         \x20       subject.user == \"{USER}\" &&\n\
         \x20       action.lookup(\"unit\") == \"{UNIT}\" &&\n\
         \x20       [\"start\", \"stop\", \"restart\"].indexOf(action.lookup(\"verb\")) >= 0) {{\n\
         \x20       return polkit.Result.YES;\n\
         \x20   }}\n\
         }});\n"
    )
}

/// What `action` will do on `computer`, with `ground` the version this
/// computer installs, or why it cannot.
///
/// # Errors
/// For people: why it cannot be done.
#[allow(clippy::too_many_lines, reason = "the plan reads in the order it runs")]
pub fn plan(
    ground: &str,
    computer: &CompanionComputer,
    action: InstallAction,
    inputs: &InstallInputs,
) -> Result<Plan, String> {
    let check = computer
        .check
        .as_ref()
        .filter(|c| c.state == CheckState::Reached)
        .ok_or_else(|| {
            format!(
                "Check {} first, so UXDuck knows what it runs.",
                computer.name
            )
        })?;
    let found = check.software.clone().unwrap_or_default();
    if !found.systemd {
        return Err(format!(
            "{} has no systemd, which runs UXDuck there as a service.",
            computer.name
        ));
    }
    if found.sudo != Some(true) {
        return Err(format!(
            "{} asks for a password for sudo, and Install has no one to type it. Let {} run sudo without a password, then check it again.",
            computer.name,
            computer
                .user
                .as_deref()
                .unwrap_or("the user UXDuck signs in as")
        ));
    }
    let own = found.units.iter().find(|u| u.own);
    let hand: Vec<&FoundUnit> = found.units.iter().filter(|u| !u.own).collect();
    let mavlink_pauses = computer.role == CompanionRole::Forward
        || found.units.iter().any(|u| u.exec.contains(" forward"));
    if action == InstallAction::Remove {
        if !managed(&found) && own.is_none() {
            return Err(format!(
                "UXDuck did not install UXDuck on {}, so there is nothing of its own to remove.",
                computer.name
            ));
        }
        let mut steps = vec![
            format!("Stop and turn off {UNIT}, and delete its file"),
            format!("Delete the polkit rule {POLKIT_RULE}"),
            format!("Delete {LINK} and every version under {VERSIONS}"),
        ];
        for kept in &found.before {
            if kept == LINK {
                steps.push(format!(
                    "Put back the uxduck someone put at {LINK}, kept as {LINK}.before-uxduck"
                ));
            } else {
                steps.push(format!(
                    "Put back the hand-made {kept} UXDuck replaced, kept as {kept}.before-uxduck"
                ));
            }
        }
        steps.push(format!(
            "Give {STATE} back to root if UXDuck gave it to {USER}"
        ));
        steps.push(format!("Delete the system user {USER} if UXDuck made it"));
        let services: Vec<String> = found
            .before
            .iter()
            .filter(|kept| kept.as_str() != LINK)
            .cloned()
            .collect();
        if !services.is_empty() {
            steps.push(format!(
                "Turn on and start {} as they were before UXDuck replaced them",
                and_list(&services)
            ));
        }
        return Ok(Plan {
            action,
            version: None,
            target: None,
            steps,
            keeps: vec![
                format!("{CONFIG}, the settings of its cameras and recording"),
                format!("{STATE}/recordings, the vehicle's recordings"),
                "Services and files someone made by hand that UXDuck did not replace".into(),
            ],
            warnings: mavlink_pauses
                .then(|| {
                    format!(
                        "{} carries the vehicle's MAVLink. Removing UXDuck stops it.",
                        computer.name
                    )
                })
                .into_iter()
                .collect(),
            mavlink_pauses,
            unit: String::new(),
            polkit: String::new(),
            args: Vec::new(),
            groups: Vec::new(),
            replaces: Vec::new(),
        });
    }
    let arch = check.arch.as_deref().unwrap_or_default();
    let target = target(arch, found.bits)?;
    let Some(ours) = Version::parse(ground) else {
        return Err(format!(
            "This computer's UXDuck ({ground}) is not a release, so it has no build to install."
        ));
    };
    if let Some(theirs) = check.uxduck.as_deref()
        && Version::parse(theirs).is_some_and(|their| their > ours)
    {
        return Err(format!(
            "{} runs UXDuck {theirs}, newer than this computer's {ground}. Update UXDuck here instead.",
            computer.name
        ));
    }
    let version = ground.to_owned();
    let from = own.or_else(|| hand.first().copied());
    let args = service_args(computer, from, &found, inputs)?;
    let carried = from
        .map(|unit| carry(unit, &found.present))
        .unwrap_or_default();
    let groups: Vec<String> = match computer.role {
        CompanionRole::Report => vec!["video".into()],
        CompanionRole::Forward => vec!["video".into(), "dialout".into()],
    };
    let mut steps = vec![format!(
        "Copy UXDuck {version} for {} from this computer, then check its SHA-256 there against the release's signed list",
        if target.starts_with("aarch64") {
            "64-bit Arm"
        } else {
            "x86-64"
        }
    )];
    steps.push(format!(
        "Make the system user {USER}, with no password and no login, unless it is there, and add it to the {} groups where the computer has them",
        and_list(&groups)
    ));
    steps.push(format!(
        "Put it in {VERSIONS}/{version}/uxduck, with {LINK} pointing at it"
    ));
    if found.link.as_deref() == Some("file") {
        steps.push(format!(
            "Keep the uxduck someone put at {LINK} as {LINK}.before-uxduck"
        ));
    }
    let mut write = format!(
        "Write {UNIT}, which runs uxduck {} as {USER}, with CAP_SYS_TIME so it can set its clock",
        args.join(" ")
    );
    if !carried.service.is_empty() || !carried.unit.is_empty() {
        write.push_str(", and with the limits ");
        write.push_str(&carried.from);
        write.push_str(" had");
    }
    steps.push(write);
    let replaces: Vec<String> = hand.iter().map(|u| u.name.clone()).collect();
    for unit in &hand {
        let mut step = format!(
            "Replace the hand-made {}: stop and turn it off, and keep its file as {}.before-uxduck",
            unit.name, unit.name
        );
        if unit.drop_ins {
            step.push_str(" and its drop-ins as ");
            step.push_str(&unit.name);
            step.push_str(".d.before-uxduck");
        }
        steps.push(step);
    }
    if let Some(unit) = own
        && unit.drop_ins
    {
        steps.push(format!(
            "Keep the drop-ins of {UNIT} as {UNIT}.d.before-uxduck, since they change what it runs"
        ));
    }
    steps.push(format!(
        "Write the polkit rule {POLKIT_RULE}, where polkit is installed"
    ));
    steps.push(format!(
        "Give {STATE} to {USER}, if it is there, so it can make room for its recordings"
    ));
    steps.push(format!(
        "Start it, and put back what was there if it is not running UXDuck {version} within {START_WITHIN_S} s"
    ));
    let current = found.link.as_deref().and_then(|l| {
        l.strip_prefix(&format!("{VERSIONS}/"))
            .and_then(|rest| rest.split('/').next())
    });
    let old: Vec<String> = found
        .kept
        .iter()
        .filter(|k| Some(k.as_str()) != current && **k != version)
        .cloned()
        .collect();
    if !old.is_empty() {
        steps.push(format!(
            "Delete {}, kept from before, which is neither the new version nor the one to go back to",
            and_list(&old)
        ));
    }
    let mut warnings = Vec::new();
    if mavlink_pauses {
        warnings.push(format!(
            "{} carries the vehicle's MAVLink. It pauses for a few seconds while the service restarts, so do this on the ground.",
            computer.name
        ));
    }
    if !carried.lost.is_empty() {
        warnings.push(format!(
            "UXDuck's service does not keep these from {}: {}. Add them back by hand if it needs them.",
            carried.from,
            carried.lost.join(", ")
        ));
    }
    for unit in hand
        .iter()
        .filter(|u| Some(u.name.as_str()) != from.map(|f| f.name.as_str()))
    {
        let limited = carry(unit, &found.present);
        if !limited.service.is_empty() {
            warnings.push(format!(
                "{} also had limits ({}). UXDuck's one service keeps only those of {}.",
                unit.name,
                limited.service.join(", "),
                carried.from
            ));
        }
    }
    for unit in &hand {
        let was = unit.user.as_deref().unwrap_or("root");
        if was != USER {
            warnings.push(format!(
                "{} ran as {was}. UXDuck's service runs as {USER}, which can use the cameras but not files only {was} can.",
                unit.name
            ));
        }
    }
    if let Some(id) = check.reports_as
        && id != computer.component_id
    {
        warnings.push(format!(
            "It reported as onboard computer {id}. From now on it reports as {}, the id this page gives it.",
            computer.component_id
        ));
    }
    Ok(Plan {
        action,
        version: Some(version.clone()),
        target: Some(target.to_owned()),
        steps,
        keeps: [
            format!("{CONFIG}, the settings of its cameras and recording"),
            format!("{STATE}/recordings, the vehicle's recordings"),
        ]
        .into_iter()
        .chain(carried.keeps.iter().cloned())
        .collect(),
        warnings,
        mavlink_pauses,
        unit: unit_file(&version, &args, &carried),
        polkit: polkit_rule(&version),
        args,
        groups,
        replaces,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::companion::{CompanionCheck, CompanionRole};

    fn computer(uxduck: Option<&str>, found: SoftwareFound) -> CompanionComputer {
        CompanionComputer {
            connectivity: None,
            id: "cc1".into(),
            name: "Camera Pi".into(),
            host: "pi.local".into(),
            user: Some("pi".into()),
            port: None,
            other_hosts: Vec::new(),
            through: None,
            role: CompanionRole::Report,
            component_id: 192,
            key: false,
            check: Some(CompanionCheck {
                state: CheckState::Reached,
                at_us: 1,
                os: Some("Debian GNU/Linux 12 (bookworm)".into()),
                arch: Some("aarch64".into()),
                uxduck: uxduck.map(str::to_owned),
                host_key: Some("SHA256:x".into()),
                problem: None,
                problem_kind: None,
                detail: None,
                clock: None,
                reports_as: None,
                network: None,
                cameras: None,
                software: Some(found),
            }),
            install: None,
            update_noticed: None,
        }
    }

    fn ready() -> SoftwareFound {
        SoftwareFound {
            systemd: true,
            sudo: Some(true),
            bits: Some(64),
            ..SoftwareFound::default()
        }
    }

    fn to(to: &str) -> InstallInputs {
        InstallInputs {
            to: Some(to.into()),
            serial: None,
        }
    }

    #[test]
    fn versions_order_as_releases_do() {
        let v = |s| Version::parse(s).unwrap();
        assert!(v("0.20.0") < v("0.21.0"));
        assert!(v("0.21.0-beta.2") < v("0.21.0"));
        assert!(v("0.21.0-beta.2") > v("0.21.0-beta.1"));
        assert!(v("0.9.9") < v("0.10.0"));
        assert_eq!(v("uxduck 0.20.0\ncommit 605aa02"), v("0.20.0"));
        assert_eq!(v("v1.2.3"), v("1.2.3"));
        assert!(Version::parse("0.20").is_none());
        assert!(Version::parse("0.20.0.1").is_none());
        assert!(Version::parse("0.20.0-rc.1").is_none());
        assert!(Version::parse("dev").is_none());
    }

    #[test]
    fn older_and_compatible_is_an_update_available() {
        let s = software("0.21.0", &computer(Some("0.20.0"), ready()));
        assert_eq!(s.status, SoftwareStatus::UpdateAvailable);
        assert_eq!(s.words, "Update available: 0.21.0");
        assert_eq!(s.offer, Some(InstallAction::Update));
        assert!(s.missing.is_empty());
    }

    #[test]
    fn older_and_missing_a_feature_its_file_uses_is_an_update_needed() {
        let found = SoftwareFound {
            config: true,
            uses: vec!["sender".into(), "recorder".into()],
            ..ready()
        };
        let s = software("0.21.0", &computer(Some("0.20.0"), found));
        assert_eq!(s.status, SoftwareStatus::UpdateNeeded);
        assert_eq!(
            s.words,
            "Update needed for camera video that follows the connection and recording on the vehicle"
        );
        assert_eq!(s.missing.len(), 2);
    }

    #[test]
    fn a_build_from_source_with_the_option_has_the_feature() {
        // The Plane's camera computer: 0.20.0 from main, with --config.
        let found = SoftwareFound {
            config: true,
            uses: vec!["sender".into(), "recorder".into()],
            has: vec!["config".into()],
            ..ready()
        };
        let s = software("0.21.0", &computer(Some("0.20.0"), found));
        assert_eq!(s.status, SoftwareStatus::UpdateAvailable);
    }

    #[test]
    fn equal_newer_missing_and_unknown() {
        let mut managed = ready();
        managed.link = Some("/opt/uxduck/0.21.0/uxduck".into());
        let s = software("0.21.0", &computer(Some("0.21.0"), managed));
        assert_eq!(
            (s.status, s.offer, s.managed),
            (SoftwareStatus::UpToDate, None, true)
        );
        let s = software("0.21.0", &computer(Some("0.21.0"), ready()));
        assert_eq!(
            (s.status, s.offer),
            (SoftwareStatus::UpToDate, Some(InstallAction::Install))
        );
        let s = software("0.21.0", &computer(Some("0.22.0"), ready()));
        assert_eq!((s.status, s.offer), (SoftwareStatus::Newer, None));
        let s = software("0.21.0", &computer(None, ready()));
        assert_eq!(
            (s.status, s.offer),
            (SoftwareStatus::NotInstalled, Some(InstallAction::Install))
        );
        let s = software("0.21.0", &computer(Some("my-build"), ready()));
        assert_eq!(
            (s.status, s.offer),
            (SoftwareStatus::Unknown, Some(InstallAction::Update))
        );
    }

    #[test]
    fn a_32_bit_system_is_refused_with_its_reason() {
        let mut c = computer(
            None,
            SoftwareFound {
                bits: Some(32),
                ..ready()
            },
        );
        let s = software("0.21.0", &c);
        assert_eq!(s.status, SoftwareStatus::Unsupported);
        assert!(s.words.contains("32-bit"), "{}", s.words);
        c.check.as_mut().unwrap().arch = Some("armv7l".into());
        let s = software("0.21.0", &c);
        assert!(s.words.contains("armv7l"), "{}", s.words);
        let why = plan("0.21.0", &c, InstallAction::Install, &to("udp:a:1")).unwrap_err();
        assert!(why.contains("32-bit"), "{why}");
    }

    #[test]
    fn unreachable_and_unchecked() {
        let mut c = computer(Some("0.20.0"), ready());
        c.check.as_mut().unwrap().state = CheckState::Failed;
        assert_eq!(software("0.21.0", &c).status, SoftwareStatus::Unreachable);
        c.check = None;
        assert_eq!(software("0.21.0", &c).status, SoftwareStatus::NotChecked);
    }

    #[test]
    fn a_fresh_install_asks_where_to_send_and_writes_the_service() {
        let c = computer(None, ready());
        let none = plan(
            "0.21.0",
            &c,
            InstallAction::Install,
            &InstallInputs::default(),
        );
        assert!(none.unwrap_err().contains("udp:HOST:PORT"));
        assert!(plan("0.21.0", &c, InstallAction::Install, &to("udp:x:1;reboot")).is_err());
        assert!(plan("0.21.0", &c, InstallAction::Install, &to("tcp:x:1")).is_err());
        let p = plan(
            "0.21.0",
            &c,
            InstallAction::Install,
            &to("udp:192.168.50.21:15010"),
        )
        .unwrap();
        assert_eq!(
            p.args,
            [
                "companion",
                "--to",
                "udp:192.168.50.21:15010",
                "--computer-id",
                "192"
            ]
        );
        assert_eq!(p.target.as_deref(), Some("aarch64-unknown-linux-gnu"));
        assert!(p.unit.starts_with("# Written by UXDuck 0.21.0"));
        assert!(p.unit.contains(
            "\nExecStart=/usr/local/bin/uxduck companion --to udp:192.168.50.21:15010 --computer-id 192\n"
        ));
        assert!(p.unit.contains("\nUser=uxduck\n"));
        assert!(p.unit.contains("\nAmbientCapabilities=CAP_SYS_TIME\n"));
        assert!(p.unit.contains("\nExecReload=/bin/kill -HUP $MAINPID\n"));
        assert!(!p.mavlink_pauses);
        assert!(p.polkit.contains("subject.user == \"uxduck\""));
        assert!(
            p.polkit
                .contains("action.lookup(\"unit\") == \"uxduck-companion.service\"")
        );
        for words in p.steps.iter().chain(&p.keeps).chain(&p.warnings) {
            assert!(!words.contains(';'), "{words}");
        }
    }

    #[test]
    fn a_hand_made_service_is_replaced_keeping_what_it_ran_with_the_pages_id() {
        let found = SoftwareFound {
            config: true,
            link: Some("file".into()),
            units: vec![FoundUnit {
                name: "uxduck-companion.service".into(),
                own: false,
                user: None,
                drop_ins: true,
                exec: "/usr/local/bin/uxduck companion --config /etc/uxduck/companion.toml --computer-id 191".into(),
                lines: Vec::new(),
            }],
            kept: vec!["s370".into()],
            ..ready()
        };
        let mut c = computer(Some("0.20.0"), found);
        c.check.as_mut().unwrap().reports_as = Some(191);
        let p = plan(
            "0.21.0",
            &c,
            InstallAction::Update,
            &InstallInputs::default(),
        )
        .unwrap();
        assert_eq!(
            p.args,
            [
                "companion",
                "--config",
                "/etc/uxduck/companion.toml",
                "--computer-id",
                "192"
            ]
        );
        assert_eq!(p.replaces, ["uxduck-companion.service"]);
        let all = p.steps.join("\n");
        assert!(
            all.contains(
                "keep its file as uxduck-companion.service.before-uxduck and its drop-ins"
            ),
            "{all}"
        );
        assert!(all.contains("/usr/local/bin/uxduck.before-uxduck"), "{all}");
        assert!(all.contains("Delete s370"), "{all}");
        let warned = p.warnings.join("\n");
        assert!(warned.contains("ran as root"), "{warned}");
        assert!(warned.contains("reports as 192"), "{warned}");
    }

    /// Plane's hand-made service, as the maintainer installed it (2026-11-23).
    const PLANE: &str = "[Unit]
Description=UXDuck companion
After=network-online.target uxduck-camera-rtsp.service tailscaled.service
Wants=network-online.target

[Service]
Type=simple
User=uxduck
Group=uxduck
ExecStart=/usr/local/bin/uxduck companion --config /etc/uxduck/companion.toml
ExecReload=/bin/kill -HUP $MAINPID
Restart=always
RestartSec=2
Environment=RUST_LOG=info
StateDirectory=uxduck
StateDirectoryMode=0750
# Viewers come over the radio LAN and Tailscale only, never the 4G modem.
IPAddressAllow=localhost 192.168.50.0/24 100.64.0.0/10 fd7a:115c:a1e0::/48
IPAddressDeny=any
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
";

    /// A unit's lines as a check sends them: no comments or blank lines.
    fn read(unit: &str) -> Vec<String> {
        unit.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with(';'))
            .map(str::to_owned)
            .collect()
    }

    /// S297: an update never loosens a hand-made service. Plane's network
    /// limit keeps the WHEP sender off the 4G modem, so UXDuck's service
    /// keeps it, with the sandbox, the environment, what it starts after
    /// and the SIGHUP reload, says so before it runs, and says what it
    /// leaves out. The next update keeps them again from UXDuck's own file.
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one update and the next, read in order"
    )]
    fn an_update_keeps_a_hand_made_services_limits_and_says_so() {
        let found = SoftwareFound {
            config: true,
            link: Some("file".into()),
            units: vec![FoundUnit {
                name: "uxduck-companion.service".into(),
                own: false,
                user: Some("uxduck".into()),
                drop_ins: false,
                exec: "/usr/local/bin/uxduck companion --config /etc/uxduck/companion.toml".into(),
                lines: read(PLANE),
            }],
            present: vec![
                "network-online.target".into(),
                "uxduck-camera-rtsp.service".into(),
            ],
            ..ready()
        };
        let c = computer(Some("0.20.0"), found);
        let p = plan(
            "0.21.0",
            &c,
            InstallAction::Update,
            &InstallInputs::default(),
        )
        .unwrap();
        for kept in [
            "After=network-online.target uxduck-camera-rtsp.service\n",
            "Wants=network-online.target\n",
            "ExecReload=/bin/kill -HUP $MAINPID\n",
            "Environment=RUST_LOG=info\n",
            "StateDirectory=uxduck\nStateDirectoryMode=0750\n",
            "IPAddressAllow=localhost 192.168.50.0/24 100.64.0.0/10 fd7a:115c:a1e0::/48\nIPAddressDeny=any\n",
            "ProtectSystem=strict\nProtectHome=yes\nPrivateTmp=yes\n",
        ] {
            assert!(p.unit.contains(kept), "{kept}\n{}", p.unit);
        }
        assert!(!p.unit.contains("tailscaled"), "{}", p.unit);
        assert_eq!(
            p.args,
            [
                "companion",
                "--computer-id",
                "192",
                "--config",
                "/etc/uxduck/companion.toml"
            ]
        );
        let keeps = p.keeps.join("\n");
        assert!(
            keeps.contains("Its network limit from uxduck-companion.service: viewers and every other connection only from this computer, 192.168.50.0/24 and Tailscale"),
            "{keeps}"
        );
        assert!(
            keeps.contains("Its sandbox from uxduck-companion.service: StateDirectory=uxduck, StateDirectoryMode=0750, ProtectSystem=strict, ProtectHome=yes and PrivateTmp=yes"),
            "{keeps}"
        );
        assert!(keeps.contains("RUST_LOG=info"), "{keeps}");
        assert!(
            keeps.contains("Starting after network-online.target and uxduck-camera-rtsp.service"),
            "{keeps}"
        );
        assert!(keeps.contains("Reloading with SIGHUP"), "{keeps}");
        let warned = p.warnings.join("\n");
        assert!(
            warned.contains("does not keep these from uxduck-companion.service: waiting for tailscaled.service, which this computer doesn't have"),
            "{warned}"
        );
        assert!(!warned.contains("ran as"), "{warned}");
        for words in p.keeps.iter().chain(&p.warnings).chain(&p.steps) {
            assert!(!words.contains(';'), "{words}");
        }

        let again = SoftwareFound {
            config: true,
            link: Some("/opt/uxduck/0.21.0/uxduck".into()),
            units: vec![FoundUnit {
                name: "uxduck-companion.service".into(),
                own: true,
                user: Some("uxduck".into()),
                drop_ins: false,
                exec: format!("/usr/local/bin/uxduck {}", p.args.join(" ")),
                lines: read(&p.unit),
            }],
            present: vec![
                "network-online.target".into(),
                "uxduck-camera-rtsp.service".into(),
            ],
            ..ready()
        };
        let next = plan(
            "0.22.0",
            &computer(Some("0.21.0"), again),
            InstallAction::Update,
            &InstallInputs::default(),
        )
        .unwrap();
        assert_eq!(
            next.unit,
            p.unit.replace("UXDuck 0.21.0", "UXDuck 0.22.0"),
            "the next update keeps the same limits"
        );
        assert!(next.warnings.is_empty(), "{:?}", next.warnings);
    }

    /// S297: a service that was looser in a way UXDuck's is not says what
    /// UXDuck's own has instead.
    #[test]
    fn a_hand_made_services_other_settings_are_named_when_not_kept() {
        let found = SoftwareFound {
            units: vec![FoundUnit {
                name: "companion.service".into(),
                own: false,
                user: Some("uxduck".into()),
                drop_ins: false,
                exec: "/usr/local/bin/uxduck companion --to udp:10.0.0.1:15010".into(),
                lines: read(
                    "[Service]\nExecStartPre=/bin/true\nCapabilityBoundingSet=CAP_NET_ADMIN\nExecStart=/usr/local/bin/uxduck companion --to udp:10.0.0.1:15010\n",
                ),
            }],
            ..ready()
        };
        let p = plan(
            "0.21.0",
            &computer(None, found),
            InstallAction::Install,
            &InstallInputs::default(),
        )
        .unwrap();
        let warned = p.warnings.join("\n");
        assert!(warned.contains("ExecStartPre=/bin/true"), "{warned}");
        assert!(
            warned.contains("CapabilityBoundingSet=CAP_NET_ADMIN (UXDuck's service has CapabilityBoundingSet=CAP_SYS_TIME)"),
            "{warned}"
        );
    }

    /// S297: Remove puts back what UXDuck replaced and starts it.
    #[test]
    fn remove_puts_back_what_uxduck_replaced() {
        let found = SoftwareFound {
            link: Some("/opt/uxduck/0.21.0/uxduck".into()),
            before: vec![
                "uxduck-companion.service".into(),
                "/usr/local/bin/uxduck".into(),
            ],
            ..ready()
        };
        let p = plan(
            "0.21.0",
            &computer(Some("0.21.0"), found),
            InstallAction::Remove,
            &InstallInputs::default(),
        )
        .unwrap();
        let all = p.steps.join("\n");
        assert!(
            all.contains("Put back the hand-made uxduck-companion.service UXDuck replaced"),
            "{all}"
        );
        assert!(
            all.contains("Put back the uxduck someone put at /usr/local/bin/uxduck"),
            "{all}"
        );
        assert!(
            all.contains("Turn on and start uxduck-companion.service as they were"),
            "{all}"
        );
    }

    #[test]
    fn an_id_is_added_where_the_service_had_none() {
        let found = SoftwareFound {
            units: vec![FoundUnit {
                name: "companion.service".into(),
                own: false,
                user: Some("pi".into()),
                drop_ins: false,
                exec: "/home/pi/.local/bin/uxduck companion --to udp:10.0.0.1:15010".into(),
                lines: Vec::new(),
            }],
            ..ready()
        };
        let c = computer(Some("0.20.0"), found);
        let p = plan(
            "0.21.0",
            &c,
            InstallAction::Update,
            &InstallInputs::default(),
        )
        .unwrap();
        assert_eq!(
            p.args,
            [
                "companion",
                "--computer-id",
                "192",
                "--to",
                "udp:10.0.0.1:15010"
            ]
        );
    }

    #[test]
    fn a_service_whose_arguments_cannot_be_carried_over_is_refused() {
        let found = SoftwareFound {
            units: vec![FoundUnit {
                name: "uxduck-companion.service".into(),
                own: false,
                user: Some("pi".into()),
                drop_ins: false,
                exec: "/usr/local/bin/uxduck companion --to udp:%H:15010".into(),
                lines: Vec::new(),
            }],
            ..ready()
        };
        let c = computer(Some("0.20.0"), found);
        let why = plan(
            "0.21.0",
            &c,
            InstallAction::Update,
            &InstallInputs::default(),
        )
        .unwrap_err();
        assert!(why.contains("%H"), "{why}");
    }

    #[test]
    fn a_forward_says_mavlink_pauses_and_takes_its_serial_port() {
        let mut c = computer(
            None,
            SoftwareFound {
                serial: vec!["/dev/ttyAMA0".into()],
                ..ready()
            },
        );
        c.role = CompanionRole::Forward;
        let inputs = to("udp:192.168.50.1:14550");
        let why = plan("0.21.0", &c, InstallAction::Install, &inputs).unwrap_err();
        assert!(why.contains("serial port"), "{why}");
        let inputs = InstallInputs {
            serial: Some("serial:/dev/ttyAMA0@921600".into()),
            ..inputs
        };
        let p = plan("0.21.0", &c, InstallAction::Install, &inputs).unwrap();
        assert_eq!(
            p.args,
            [
                "forward",
                "--onboard",
                "--computer-id",
                "192",
                "--in",
                "serial:/dev/ttyAMA0@921600",
                "--out",
                "udp:192.168.50.1:14550"
            ]
        );
        assert!(p.mavlink_pauses);
        assert!(p.warnings[0].contains("MAVLink"));
        assert_eq!(p.groups, ["video", "dialout"]);
    }

    #[test]
    fn install_needs_sudo_without_a_password_and_systemd() {
        let c = computer(
            None,
            SoftwareFound {
                sudo: Some(false),
                ..ready()
            },
        );
        let why = plan("0.21.0", &c, InstallAction::Install, &to("udp:a:1")).unwrap_err();
        assert!(why.contains("sudo"), "{why}");
        let c = computer(
            None,
            SoftwareFound {
                systemd: false,
                ..ready()
            },
        );
        let why = plan("0.21.0", &c, InstallAction::Install, &to("udp:a:1")).unwrap_err();
        assert!(why.contains("systemd"), "{why}");
    }

    #[test]
    fn remove_is_only_for_what_uxduck_installed() {
        let c = computer(Some("0.20.0"), ready());
        assert!(
            plan(
                "0.21.0",
                &c,
                InstallAction::Remove,
                &InstallInputs::default()
            )
            .is_err()
        );
        let c = computer(
            Some("0.21.0"),
            SoftwareFound {
                link: Some("/opt/uxduck/0.21.0/uxduck".into()),
                ..ready()
            },
        );
        let p = plan(
            "0.21.0",
            &c,
            InstallAction::Remove,
            &InstallInputs::default(),
        )
        .unwrap();
        assert!(p.keeps[0].contains("companion.toml"));
        assert!(p.unit.is_empty());
    }

    #[test]
    fn a_development_ground_has_no_build_to_install() {
        let c = computer(None, ready());
        let why = plan("dev", &c, InstallAction::Install, &to("udp:a:1")).unwrap_err();
        assert!(why.contains("not a release"), "{why}");
    }

    #[test]
    fn newer_is_never_installed_over() {
        let c = computer(Some("0.22.0"), ready());
        let why = plan(
            "0.21.0",
            &c,
            InstallAction::Update,
            &InstallInputs::default(),
        )
        .unwrap_err();
        assert!(why.contains("newer"), "{why}");
    }
}
