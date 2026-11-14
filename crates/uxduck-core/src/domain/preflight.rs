//! Preflight checks (ADR-0038 K1 to K6, S192): one list per vehicle, the
//! engine's to evaluate until the vehicle has launched, and the gate that
//! holds a launch, never arming, while a check set to hold it does not
//! pass (K22 to K25, S352).
//!
//! A check has a source (the vehicle, UXDuck, the vehicle's checklist, a
//! plugin), a severity (`block` holds the launch, `warn` only warns) and a
//! status, with a detail in words and where its fix is. The vehicle's own
//! pre-arm reasons come first and never gate: the vehicle refuses by
//! itself. UXDuck's own checks are each on or off per vehicle, with a
//! severity and, for some, a threshold. Checklist steps (S193) and
//! plugins' checks (S194) join the same list. A step of the checklist
//! counts in the preflight or at the launch (K26). The rest are read live
//! and count in both.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::domain::setup::SetupRow;

/// Where a check comes from (K1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckSource {
    /// The vehicle's own pre-arm reasons (K5): listed first, never turned
    /// off, never gated on by UXDuck.
    Vehicle,
    /// UXDuck's own checks (K6).
    Uxduck,
    /// A step of the vehicle's checklist (S193).
    Checklist,
    /// A plugin's check (S194).
    Plugin,
}

/// Whether a check that does not pass holds the launch (K25): a take-off,
/// or a mission's start from the ground. Never arming (K22).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckSeverity {
    /// Holds the launch until it passes.
    Block,
    /// Only warns.
    Warn,
}

/// Where a checklist step counts (K26): in the preflight, done ahead of
/// time and wherever a person likes, or at the launch, where and when the
/// vehicle takes off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckWhen {
    #[default]
    Preflight,
    Launch,
}

impl CheckWhen {
    /// For serde: the default is left off the wire.
    #[must_use]
    pub fn is_preflight(&self) -> bool {
        *self == Self::Preflight
    }
}

/// Where a check stands. Only `pass` lets a holding check through: one
/// that fails, waits, is unknown or is still to do holds the launch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckStatus {
    Pass,
    Fail,
    /// Something is under way that ends by itself, such as terrain loading.
    Waiting,
    /// UXDuck cannot tell, such as a controller whose sticks have not moved.
    Unknown,
    /// For a person to tick (S193).
    ToDo,
}

/// Where the fix for a check is, for the interface to lead there.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum CheckFix {
    /// A row of the Setup tab (ADR-0037).
    Setup { row: SetupRow },
    /// A saved plan, on the Plans page.
    Plan { plan_id: String },
    /// The Plans page.
    Plans,
    /// The vehicle's Mission tab.
    Mission,
    /// The Cockpit's controllers.
    Controllers,
    /// The vehicle's cameras.
    Cameras,
    /// A restart of the vehicle.
    Restart,
    /// The vehicle's messages.
    Messages,
    /// The vehicle's link, in Settings › Connections.
    Connections,
    /// The plugin the check is from, on the Plugins page (S194).
    Plugin { plugin_id: String },
    /// Home, set to where the vehicle is now (K27).
    Home,
}

/// UXDuck's own checks (K6): what the firmware cannot know.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OwnCheck {
    /// No setup procedure is running (ADR-0037).
    Setup,
    /// No restart is due after a calibration or a parameter change.
    Restart,
    /// The link is good: heard just now, and the radio's signal is strong
    /// enough when it reports one.
    Link,
    /// The vehicle has controller input: over RC, from a controller
    /// through UXDuck, or either, as a person sets it (`InputPath`).
    RcReceiver,
    /// A controller driving the vehicle is at neutral.
    Controller,
    /// The plan on the vehicle is the one chosen, verified and unchanged
    /// (ADR-0029).
    Plan,
    /// The vehicle has loaded terrain where its mission needs it.
    Terrain,
    /// The cameras that record while it is armed are sending video.
    Recording,
    /// Home is where the vehicle is (K27): a return goes to home, and one
    /// left indoors or at an earlier place is not where it takes off.
    /// Read at the launch only. A home far away fails once the vehicle is
    /// armed: before that most autopilots are still to move it.
    Home,
}

/// A check's threshold: what it means and its range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Threshold {
    /// What the number is, for people: "Least signal".
    pub label: &'static str,
    pub unit: &'static str,
    pub default: f64,
    pub min: f64,
    pub max: f64,
}

impl OwnCheck {
    /// In the list's order.
    pub const ALL: [Self; 9] = [
        Self::Setup,
        Self::Restart,
        Self::Link,
        Self::RcReceiver,
        Self::Controller,
        Self::Plan,
        Self::Terrain,
        Self::Recording,
        Self::Home,
    ];

    /// Its key on the wire: "plan".
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::Restart => "restart",
            Self::Link => "link",
            Self::RcReceiver => "rc-receiver",
            Self::Controller => "controller",
            Self::Plan => "plan",
            Self::Terrain => "terrain",
            Self::Recording => "recording",
            Self::Home => "home",
        }
    }

    /// The check's id in a vehicle's list: "uxduck:plan".
    #[must_use]
    pub fn id(self) -> String {
        format!("uxduck:{}", self.key())
    }

    /// The one whose id this is.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        let key = id.strip_prefix("uxduck:")?;
        Self::ALL.into_iter().find(|c| c.key() == key)
    }

    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Setup => "No setup procedure running",
            Self::Restart => "No restart due",
            Self::Link => "Link",
            Self::RcReceiver => "Controller input",
            Self::Controller => "Controller at neutral",
            Self::Plan => "Plan on the vehicle",
            Self::Terrain => "Terrain loaded",
            Self::Recording => "Cameras ready to record",
            Self::Home => "Home is where the vehicle is",
        }
    }

    /// The concept it sits under in the list (K16), where a checklist's
    /// steps of the same group join it.
    #[must_use]
    pub const fn group(self) -> &'static str {
        match self {
            Self::Setup | Self::Restart => "Setup",
            Self::Link => "Link",
            Self::RcReceiver | Self::Controller => "Controller",
            Self::Plan | Self::Terrain => "Plan",
            Self::Recording => "Cameras",
            Self::Home => "Home",
        }
    }

    /// Where it counts (K26, K27): at the launch only for the checks about
    /// the place of the launch. None for the rest, which are read live and
    /// count in the preflight and at the launch alike.
    #[must_use]
    pub const fn when(self) -> Option<CheckWhen> {
        match self {
            Self::Home => Some(CheckWhen::Launch),
            _ => None,
        }
    }

    /// A fresh vehicle's: every one on, and only warning until a person
    /// sets it to hold the launch (K25).
    #[must_use]
    pub const fn default_severity(self) -> CheckSeverity {
        CheckSeverity::Warn
    }

    /// Its threshold, for the checks that have one.
    #[must_use]
    pub const fn threshold(self) -> Option<Threshold> {
        match self {
            Self::Link => Some(Threshold {
                label: "Least radio signal",
                unit: "%",
                default: 30.0,
                min: 0.0,
                max: 100.0,
            }),
            Self::Controller => Some(Threshold {
                label: "Sticks within",
                unit: "%",
                default: 10.0,
                min: 1.0,
                max: 50.0,
            }),
            Self::Home => Some(Threshold {
                label: "Home within",
                unit: "m",
                default: 50.0,
                min: 5.0,
                max: 5000.0,
            }),
            _ => None,
        }
    }

    /// Whether a person chooses where its input may come from.
    #[must_use]
    pub const fn has_input(self) -> bool {
        matches!(self, Self::RcReceiver)
    }

    /// How it is set when nobody has set it.
    #[must_use]
    pub const fn default_setting(self) -> CheckSetting {
        CheckSetting {
            on: true,
            severity: self.default_severity(),
            threshold: None,
            input: None,
        }
    }
}

/// Where a vehicle's controller input may come from for "Controller input"
/// to pass (K6). Its own failsafe takes either (maintainer, 2026-11-13).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputPath {
    /// Its RC receiver, or a controller through UXDuck.
    #[default]
    Either,
    /// Its own RC receiver.
    Rc,
    /// A controller through UXDuck, over the link.
    Link,
}

/// One of UXDuck's own checks as set for a vehicle.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckSetting {
    pub on: bool,
    pub severity: CheckSeverity,
    /// Its threshold when changed from the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f64>,
    /// Where its input may come from when changed from either, for the
    /// check that asks (`OwnCheck::has_input`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<InputPath>,
}

/// The plan a person chose for a vehicle's next flight, and when. An
/// upload chooses too: the later of the two counts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChosenPlan {
    /// None: no plan, flown by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
    pub at_us: u64,
}

// A threshold is checked finite before it is kept, so no setting holds a
// NaN and equality is total (as for `LinkKind`).
impl Eq for CheckSetting {}

/// What a vehicle keeps of its checks (K6): only what a person changed, so
/// a vehicle nobody set keeps following the defaults. Kept with the
/// vehicle and journaled with it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreflightSettings {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub checks: BTreeMap<OwnCheck, CheckSetting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<ChosenPlan>,
}

impl PreflightSettings {
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.checks.is_empty() && self.plan.is_none()
    }

    /// How a check is set for the vehicle.
    #[must_use]
    pub fn setting(&self, check: OwnCheck) -> CheckSetting {
        self.checks
            .get(&check)
            .copied()
            .unwrap_or_else(|| check.default_setting())
    }

    /// Its threshold, the default unless changed.
    #[must_use]
    pub fn threshold(&self, check: OwnCheck) -> Option<f64> {
        let meta = check.threshold()?;
        Some(
            self.setting(check)
                .threshold
                .unwrap_or(meta.default)
                .clamp(meta.min, meta.max),
        )
    }

    /// Where a check's input may come from, for the check that asks:
    /// either unless changed.
    #[must_use]
    pub fn input(&self, check: OwnCheck) -> Option<InputPath> {
        check
            .has_input()
            .then(|| self.setting(check).input.unwrap_or_default())
    }

    /// Changes a check's setting, keeping nothing for one set back to its
    /// default.
    pub fn set(&mut self, check: OwnCheck, setting: CheckSetting) {
        let mut setting = setting;
        if !check.has_input() || setting.input == Some(InputPath::Either) {
            setting.input = None;
        }
        if let (Some(meta), Some(value)) = (check.threshold(), setting.threshold)
            && (value - meta.default).abs() < f64::EPSILON
        {
            setting.threshold = None;
        }
        if check.threshold().is_none() {
            setting.threshold = None;
        }
        if setting == check.default_setting() {
            self.checks.remove(&check);
        } else {
            self.checks.insert(check, setting);
        }
    }
}

/// One check in a vehicle's list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    /// Unique in the list: `uxduck:plan`, `vehicle:<reason>`.
    pub id: String,
    pub source: CheckSource,
    pub title: String,
    /// The concept it sits under in the list (K16): "Plan" for UXDuck's
    /// own, a checklist step's own group. None for the vehicle's reasons
    /// and plugins' checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub severity: CheckSeverity,
    /// Where a checklist step counts (K26), and `launch` for UXDuck's
    /// checks of the place of the launch (K27). None for the vehicle's
    /// reasons, UXDuck's other checks and plugins' checks, which are read
    /// live and count in the preflight and at the launch alike.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<CheckWhen>,
    pub status: CheckStatus,
    /// What UXDuck found, in words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<CheckFix>,
    /// The plugin a `plugin` check is from (S194).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<CheckPlugin>,
}

/// The plugin a check comes from (S194), by id and name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckPlugin {
    pub id: String,
    pub name: String,
    /// It declares the check blocking, but no person has allowed its
    /// checks to hold the launch (K13): it only warns.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub held_back: bool,
}

impl Check {
    /// Whether UXDuck holds the launch for it (K23): a check set to hold,
    /// of any source but the vehicle, which refuses by itself (K5), that
    /// does not pass.
    #[must_use]
    pub fn gates(&self) -> bool {
        self.source != CheckSource::Vehicle
            && self.severity == CheckSeverity::Block
            && self.status != CheckStatus::Pass
    }

    /// As a receipt, an approval and the vehicle's history keep it; a
    /// plugin's check names its plugin: "Wind under 8 m/s (Wind check)".
    #[must_use]
    pub fn to_ref(&self) -> CheckRef {
        CheckRef {
            id: self.id.clone(),
            title: match &self.plugin {
                Some(plugin) => format!("{} ({})", self.title, plugin.name),
                None => self.title.clone(),
            },
            status: self.status,
            detail: self.detail.clone(),
        }
    }
}

/// A check as it stood when a command was refused for it, or sent past it:
/// kept on the receipt and the approval, so the journal says which.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckRef {
    pub id: String,
    pub title: String,
    pub status: CheckStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// How a launch went past failing checks set to hold it (K24).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub enum PreflightOverride {
    /// The operator held Take off anyway, naming the checks.
    Operator,
    /// An operator allowed a client's request that listed them.
    Approval { approval_id: String },
}

/// A vehicle's checks (K2), by vehicle id: pushed as they change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Preflight {
    pub vehicle_id: String,
    /// The vehicle's own first, then UXDuck's, in their order.
    pub checks: Vec<Check>,
    /// How the vehicle's UXDuck checks are set, on or off.
    #[serde(default)]
    pub settings: PreflightSettings,
    /// The plan it counts as chosen: picked, or uploaded since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
    /// False once the vehicle has launched (in the air, or armed for one
    /// that does not fly): the list is the one it launched with.
    pub evaluating: bool,
    /// The vehicle's checklist (S193): its steps with their ticks, kept
    /// with the vehicle; its checks are in `checks` as `checklist:<id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checklist: Option<crate::domain::checklist::Checklist>,
    /// Steps ticked before the vehicle's last flight began (K9): "a flight
    /// since".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flown_since: Vec<String>,
    /// Rules the vehicle can hold itself with one of its parameters (K10).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub offers: Vec<ParamOffer>,
    pub updated_at_us: u64,
    pub version: u64,
}

/// A rule the vehicle can hold too, with one of its parameters (K10):
/// `BATT_ARM_VOLT` for a rule that the battery is at least a voltage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParamOffer {
    pub step_id: String,
    /// The parameter: `BATT_ARM_VOLT`.
    pub name: String,
    /// What it would be set to: the rule's value.
    pub value: f64,
    /// What it is on the vehicle now, when UXDuck has read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<f64>,
    /// The vehicle holds the rule already: the parameter is at least the
    /// rule's value.
    pub held: bool,
    /// Why setting it would not hold arming, in words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Preflight {
    /// The checks that hold a launch now (K23).
    pub fn gating(&self) -> impl Iterator<Item = &Check> {
        self.checks.iter().filter(|c| c.gates())
    }

    /// The same list, ignoring when it was made: whether there is news.
    #[must_use]
    pub fn same_as(&self, other: &Self) -> bool {
        self.vehicle_id == other.vehicle_id
            && self.checks == other.checks
            && self.settings == other.settings
            && self.plan_id == other.plan_id
            && self.evaluating == other.evaluating
            && self.checklist == other.checklist
            && self.flown_since == other.flown_since
            && self.offers == other.offers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(source: CheckSource, severity: CheckSeverity, status: CheckStatus) -> Check {
        Check {
            id: "x".into(),
            source,
            title: "X".into(),
            group: None,
            severity,
            when: None,
            status,
            detail: None,
            fix: None,
            plugin: None,
        }
    }

    #[test]
    fn only_blocking_checks_that_do_not_pass_gate_and_never_the_vehicles() {
        use CheckSeverity::{Block, Warn};
        use CheckSource::{Plugin, Uxduck, Vehicle};
        use CheckStatus::{Fail, Pass, ToDo, Unknown, Waiting};
        for status in [Fail, Waiting, Unknown, ToDo] {
            assert!(check(Uxduck, Block, status).gates(), "{status:?}");
            assert!(check(Plugin, Block, status).gates(), "{status:?}");
            assert!(!check(Uxduck, Warn, status).gates(), "{status:?}");
            assert!(!check(Vehicle, Block, status).gates(), "{status:?}");
        }
        assert!(!check(Uxduck, Block, Pass).gates());
    }

    #[test]
    fn settings_keep_only_what_differs_from_the_default() {
        let mut settings = PreflightSettings::default();
        assert_eq!(
            settings.setting(OwnCheck::Plan).severity,
            CheckSeverity::Warn
        );
        for check in OwnCheck::ALL {
            assert_eq!(
                settings.setting(check).severity,
                CheckSeverity::Warn,
                "{check:?} only warns until a person sets it to hold"
            );
        }
        assert_eq!(settings.threshold(OwnCheck::Link), Some(30.0));
        assert_eq!(settings.threshold(OwnCheck::Plan), None);
        settings.set(
            OwnCheck::Plan,
            CheckSetting {
                on: true,
                severity: CheckSeverity::Block,
                threshold: Some(3.0),
                input: Some(InputPath::Link),
            },
        );
        assert_eq!(
            settings
                .checks
                .get(&OwnCheck::Plan)
                .and_then(|s| s.threshold),
            None,
            "a check without a threshold keeps none"
        );
        assert_eq!(
            settings.checks.get(&OwnCheck::Plan).and_then(|s| s.input),
            None,
            "a check that takes no input keeps none"
        );
        assert_eq!(settings.input(OwnCheck::Plan), None);
        // Controller input takes either path until a person asks for one.
        assert_eq!(
            settings.input(OwnCheck::RcReceiver),
            Some(InputPath::Either)
        );
        settings.set(
            OwnCheck::RcReceiver,
            CheckSetting {
                input: Some(InputPath::Link),
                ..OwnCheck::RcReceiver.default_setting()
            },
        );
        assert_eq!(settings.input(OwnCheck::RcReceiver), Some(InputPath::Link));
        settings.set(
            OwnCheck::RcReceiver,
            CheckSetting {
                input: Some(InputPath::Either),
                ..OwnCheck::RcReceiver.default_setting()
            },
        );
        assert!(
            !settings.checks.contains_key(&OwnCheck::RcReceiver),
            "either is the default, so nothing is kept"
        );
        settings.set(
            OwnCheck::Link,
            CheckSetting {
                on: true,
                severity: CheckSeverity::Warn,
                threshold: Some(150.0),
                input: None,
            },
        );
        assert_eq!(settings.threshold(OwnCheck::Link), Some(100.0));
        settings.set(OwnCheck::Link, OwnCheck::Link.default_setting());
        settings.set(OwnCheck::Plan, OwnCheck::Plan.default_setting());
        assert!(settings.is_default());
        assert_eq!(OwnCheck::from_id("uxduck:terrain"), Some(OwnCheck::Terrain));
        assert_eq!(OwnCheck::Home.when(), Some(CheckWhen::Launch));
        assert_eq!(OwnCheck::Plan.when(), None);
        assert_eq!(settings.threshold(OwnCheck::Home), Some(50.0));
        assert_eq!(OwnCheck::from_id("vehicle:terrain"), None);
    }
}
