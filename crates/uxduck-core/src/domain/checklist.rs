//! A vehicle's checklist (ADR-0038 K7 to K11, K14; S193): steps a person
//! ticks ("Propellers tight") and rules over a telemetry item or a
//! parameter ("Battery at least 22.2 V, held 3 s"), each only warning
//! until a person sets it to hold the launch (K25). They join the
//! vehicle's preflight checks with source `checklist`. A step counts in
//! the preflight or at the launch (K26, `when`).
//!
//! A step can sit under a concept ("Battery", "Controller") beside
//! UXDuck's own checks of that group (K16), and a rule can ask for a
//! person's tick too, once it passes (K17).
//!
//! A checklist belongs to one vehicle (K7): another vehicle's, a checklist
//! file or a pasted Markdown list is copied in and independent from then
//! on. A tick stays until a person clears it (K9): nothing clears it on
//! landing, a battery change or a restart. Arming writes the checklist as
//! it stood to the vehicle's history and the flight (K14).
//!
//! The checklist is a template, and each time the preflight is done is a
//! run of it (K18, S350): started by a person, holding the ticks made in
//! it, and finished by a person, which locks its preflight steps (K19).
//! A run as it ended is kept in the vehicle's history. A run is a
//! record: it never holds or opens Arm (K21).

use std::fmt::Write;

use serde::{Deserialize, Serialize};

use crate::domain::history::Author;
use crate::domain::preflight::{
    Check, CheckRef, CheckSeverity, CheckStatus, CheckWhen, PreflightOverride,
};

/// The checklist file's format and version (K7).
pub const FILE_FORMAT: &str = "uxduck.checklist/1";
/// The most steps one vehicle's checklist holds.
pub const MAX_STEPS: usize = 100;
/// The longest step title, in characters.
pub const MAX_TITLE: usize = 200;
/// The longest group name, in characters.
pub const MAX_GROUP: usize = 60;
/// The longest a rule waits for its value to hold, in seconds.
pub const MAX_HOLD_S: f64 = 600.0;

/// How a rule compares (K8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    #[serde(rename = ">=")]
    Ge,
    #[serde(rename = "<=")]
    Le,
    #[serde(rename = ">")]
    Gt,
    #[serde(rename = "<")]
    Lt,
    #[serde(rename = "==")]
    Eq,
    #[serde(rename = "!=")]
    Ne,
}

impl Op {
    pub const ALL: [Self; 6] = [Self::Ge, Self::Le, Self::Gt, Self::Lt, Self::Eq, Self::Ne];

    #[must_use]
    pub const fn symbol(self) -> &'static str {
        match self {
            Self::Ge => ">=",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Lt => "<",
            Self::Eq => "==",
            Self::Ne => "!=",
        }
    }

    #[must_use]
    pub fn from_symbol(symbol: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|op| op.symbol() == symbol)
    }

    /// Whether `value` compares so with `target`. Equal is equal at the
    /// precision parameters travel at (32-bit floats).
    #[must_use]
    pub fn holds(self, value: f64, target: f64) -> bool {
        let same = crate::domain::parameter::same_value(value, target);
        match self {
            Self::Ge => value >= target || same,
            Self::Le => value <= target || same,
            Self::Gt => value > target && !same,
            Self::Lt => value < target && !same,
            Self::Eq => same,
            Self::Ne => !same,
        }
    }

    /// "at least", as a rule's words say it when it holds.
    #[must_use]
    pub const fn words(self) -> &'static str {
        match self {
            Self::Ge => "at least",
            Self::Le => "at most",
            Self::Gt => "above",
            Self::Lt => "below",
            Self::Eq => "exactly",
            Self::Ne => "not",
        }
    }

    /// "below", as its words say it when it does not hold.
    #[must_use]
    pub const fn failing_words(self) -> &'static str {
        match self {
            Self::Ge => "below",
            Self::Le => "above",
            Self::Gt => "not above",
            Self::Lt => "not below",
            Self::Eq => "not",
            Self::Ne => "exactly",
        }
    }
}

/// A manual step for a person to tick, or a rule UXDuck checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StepKind {
    Manual,
    Rule,
}

/// What a rule reads and compares (K8): a telemetry item or a parameter.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    /// A telemetry item's key, such as `battery.voltage_v`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    /// A parameter's name, such as `ARMING_CHECK`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    pub op: Op,
    pub value: f64,
    /// The value's unit as people read it: "V", "%". From the item's key,
    /// or the parameter's definition, when not given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// How long the value must hold before the rule passes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_s: Option<f64>,
}

// Values are checked finite before they are kept (as `CheckSetting`).
impl Eq for Rule {}

impl Rule {
    /// What it reads, as it is named: the item's key or the parameter.
    #[must_use]
    pub fn reads(&self) -> &str {
        self.item
            .as_deref()
            .or(self.param.as_deref())
            .unwrap_or_default()
    }

    /// "22.2 V".
    #[must_use]
    pub fn value_words(&self) -> String {
        with_unit(self.value, self.unit.as_deref())
    }

    /// Seconds it must hold, when more than none.
    #[must_use]
    pub fn hold(&self) -> Option<f64> {
        self.hold_s.filter(|s| *s > 0.0)
    }
}

/// A value and its unit, as people read them: "22.2 V", "80 %", "3".
#[must_use]
pub fn with_unit(value: f64, unit: Option<&str>) -> String {
    let number = crate::domain::parameter::show_value(value);
    match unit {
        Some(unit) if !unit.is_empty() => format!("{number} {unit}"),
        _ => number,
    }
}

/// The unit an item's key says (ADR-0006): `_v` is volts.
#[must_use]
pub fn unit_of_key(key: &str) -> Option<&'static str> {
    const UNITS: [(&str, &str); 12] = [
        ("_mps2", "m/s²"),
        ("_degps", "°/s"),
        ("_degc", "°C"),
        ("_mah", "mAh"),
        ("_mps", "m/s"),
        ("_pct", "%"),
        ("_deg", "°"),
        ("_at", ""),
        ("_m", "m"),
        ("_v", "V"),
        ("_a", "A"),
        ("_s", "s"),
    ];
    UNITS
        .iter()
        .find(|(suffix, _)| key.ends_with(suffix))
        .map(|(_, unit)| *unit)
        .filter(|unit| !unit.is_empty())
}

/// Who ticked a step, and when (K9).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tick {
    pub by: Author,
    pub at_us: u64,
}

impl Tick {
    /// The person, or the client when no operator was known.
    #[must_use]
    pub fn who(&self) -> &str {
        self.by
            .operator_name
            .as_deref()
            .unwrap_or(&self.by.client_name)
    }
}

/// A finish a person undid (K19): who finished the preflight and when,
/// and who reopened it and when.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reopening {
    pub finished: Tick,
    pub reopened: Tick,
}

/// One time a vehicle's preflight is done (K18): a run of its checklist.
/// A person starts it, with New preflight or the first tick, and the
/// ticks on the steps are its ticks. A person finishes it (K19), which
/// locks its steps and its preflight ticks, and can reopen it to change
/// it and finish it again. A run never holds or opens Arm (K21).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub id: String,
    /// Who started it, and when.
    pub started: Tick,
    /// Who finished it, and when. None while it is under way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<Tick>,
    /// Each earlier finish that a person reopened, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reopenings: Vec<Reopening>,
    /// When the vehicle last restarted since the run started (K27).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restarted_at_us: Option<u64>,
}

/// One step of a vehicle's checklist.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    /// Unique in the checklist; its check is `checklist:<id>`.
    pub id: String,
    pub kind: StepKind,
    pub title: String,
    /// The concept it sits under in the list ("Battery"), beside UXDuck's
    /// own checks of that group (K16).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Only warning unless a person set it to hold the launch (K25).
    pub severity: CheckSeverity,
    /// Whether it counts in the preflight or at the launch (K26).
    #[serde(default, skip_serializing_if = "CheckWhen::is_preflight")]
    pub when: CheckWhen,
    /// A rule's comparison; none for a manual step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<Rule>,
    /// A rule a person ticks too, once it passes (K17).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub confirm: bool,
    /// A person's tick, on a manual step or a rule that asks for one,
    /// until a person clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tick: Option<Tick>,
}

impl Step {
    /// Its id in the vehicle's preflight checks.
    #[must_use]
    pub fn check_id(&self) -> String {
        format!("checklist:{}", self.id)
    }

    /// Whether a person ticks it: a manual step, or a rule that asks for
    /// a tick once it passes (K17).
    #[must_use]
    pub const fn needs_tick(&self) -> bool {
        self.rule.is_none() || self.confirm
    }

    /// The step's definition without its tick: what a copy takes.
    #[must_use]
    pub fn definition(&self) -> StepDef {
        StepDef {
            kind: self.kind,
            title: self.title.clone(),
            group: self.group.clone(),
            severity: self.severity,
            when: self.when,
            rule: self.rule.clone(),
            confirm: self.confirm,
        }
    }
}

/// The step id behind a check id: `checklist:<id>`.
#[must_use]
pub fn step_of_check(check_id: &str) -> Option<&str> {
    check_id.strip_prefix("checklist:")
}

/// A step as written or copied, before it has an id or a tick.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepDef {
    pub kind: StepKind,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub severity: CheckSeverity,
    #[serde(default, skip_serializing_if = "CheckWhen::is_preflight")]
    pub when: CheckWhen,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<Rule>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub confirm: bool,
}

/// A vehicle's checklist, by vehicle id. Journaled; kept with the vehicle
/// and removed with it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Checklist {
    pub vehicle_id: String,
    pub steps: Vec<Step>,
    /// Where its steps were last copied from, in words: "Alpha's
    /// checklist", "props.md". Nothing links back to it (K7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copied_from: Option<String>,
    /// The preflight under way, or the last one finished (K18). None
    /// until a person starts one or ticks a step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<Run>,
    pub updated_at_us: u64,
    pub version: u64,
}

impl Checklist {
    #[must_use]
    pub fn empty(vehicle_id: &str) -> Self {
        Self {
            vehicle_id: vehicle_id.to_owned(),
            steps: Vec::new(),
            copied_from: None,
            run: None,
            updated_at_us: 0,
            version: 0,
        }
    }

    /// Whether its preflight is finished (K19). Its steps then stay as
    /// they are until a person reopens it or starts a new one.
    #[must_use]
    pub fn finished(&self) -> bool {
        self.run.as_ref().is_some_and(|r| r.finished.is_some())
    }

    /// Whether a step takes no tick: its preflight is finished (K19). A
    /// launch step is ticked where and when the vehicle launches, after
    /// the preflight, so it stays open until a new preflight starts.
    #[must_use]
    pub fn locks(&self, step: &Step) -> bool {
        step.when.is_preflight() && self.finished()
    }

    /// The run a stored checklist's ticks stand for when it has none
    /// (K18): begun by its earliest tick.
    #[must_use]
    pub fn run_of_ticks(&self, id: String) -> Option<Run> {
        let first = self
            .steps
            .iter()
            .filter_map(|s| s.tick.as_ref())
            .min_by_key(|t| t.at_us)?;
        Some(Run {
            id,
            started: first.clone(),
            finished: None,
            reopenings: Vec::new(),
            restarted_at_us: None,
        })
    }

    #[must_use]
    pub fn step(&self, id: &str) -> Option<&Step> {
        self.steps.iter().find(|s| s.id == id)
    }

    /// The checklist as a file, without ids or ticks (K7).
    #[must_use]
    pub fn to_file(&self, name: Option<String>) -> ChecklistFile {
        ChecklistFile {
            format: FILE_FORMAT.into(),
            name,
            steps: self.steps.iter().map(Step::definition).collect(),
        }
    }
}

/// A checklist file (`uxduck.checklist/1`): a starting point to copy into
/// a vehicle, never linked to it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChecklistFile {
    pub format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub steps: Vec<StepDef>,
}

/// Why a step or a file is refused, in words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invalid(pub String);

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn invalid(message: impl Into<String>) -> Invalid {
    Invalid(message.into())
}

/// A group's name as it is kept: single spaces, and none at its ends.
/// None when nothing is left of it.
///
/// # Errors
/// A name longer than `MAX_GROUP` characters.
pub fn group_name(name: &str) -> Result<Option<String>, Invalid> {
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.chars().count() > MAX_GROUP {
        return Err(invalid(format!(
            "a group's name is at most {MAX_GROUP} characters"
        )));
    }
    Ok((!name.is_empty()).then_some(name))
}

/// Whether two names are one group in the list: capitals do not count.
#[must_use]
pub fn same_group(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// A telemetry key as ADR-0006 names them: `group.name`, lower case words.
fn is_item_key(key: &str) -> bool {
    let word = |w: &str| {
        !w.is_empty()
            && w.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    key.len() <= 64 && key.split_once('.').is_some_and(|(g, n)| word(g) && word(n))
}

/// A parameter's name: upper case letters, digits and `_`, at most 16.
fn is_param_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 16
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

impl StepDef {
    /// A manual step of the preflight, which only warns (K25).
    #[must_use]
    pub fn manual(title: impl Into<String>) -> Self {
        Self {
            kind: StepKind::Manual,
            title: title.into(),
            group: None,
            severity: CheckSeverity::Warn,
            when: CheckWhen::Preflight,
            rule: None,
            confirm: false,
        }
    }

    /// The step tidied (title and group trimmed, a rule's unit from its
    /// key when not given, no tick asked of a manual step twice).
    ///
    /// # Errors
    /// Why it cannot be kept, in words.
    pub fn checked(self) -> Result<Self, Invalid> {
        let title = self.title.split_whitespace().collect::<Vec<_>>().join(" ");
        if title.is_empty() {
            return Err(invalid("a step needs a title"));
        }
        if title.chars().count() > MAX_TITLE {
            return Err(invalid(format!(
                "“{}…” is longer than {MAX_TITLE} characters",
                title.chars().take(40).collect::<String>()
            )));
        }
        let rule = match (self.kind, self.rule) {
            (StepKind::Manual, None) => None,
            (StepKind::Manual, Some(_)) => {
                return Err(invalid(format!(
                    "“{title}” is a manual step: it has no item, parameter or value"
                )));
            }
            (StepKind::Rule, None) => {
                return Err(invalid(format!(
                    "“{title}” is a rule: it needs an item or a parameter, an operator and a value"
                )));
            }
            (StepKind::Rule, Some(rule)) => Some(checked_rule(&title, rule)?),
        };
        let group = match self.group.as_deref().map(group_name) {
            Some(Ok(group)) => group,
            Some(Err(long)) => return Err(invalid(format!("“{title}”: {long}"))),
            None => None,
        };
        Ok(Self {
            kind: self.kind,
            title,
            group,
            severity: self.severity,
            when: self.when,
            confirm: self.confirm && rule.is_some(),
            rule,
        })
    }
}

fn checked_rule(title: &str, rule: Rule) -> Result<Rule, Invalid> {
    let item = rule
        .item
        .map(|i| i.trim().to_owned())
        .filter(|i| !i.is_empty());
    let param = rule
        .param
        .map(|p| p.trim().to_ascii_uppercase())
        .filter(|p| !p.is_empty());
    match (&item, &param) {
        (Some(_), Some(_)) => {
            return Err(invalid(format!(
                "“{title}”: a rule reads a telemetry item or a parameter, not both"
            )));
        }
        (None, None) => {
            return Err(invalid(format!(
                "“{title}”: a rule needs a telemetry item (such as battery.voltage_v) or a parameter"
            )));
        }
        (Some(key), None) if !is_item_key(key) => {
            return Err(invalid(format!(
                "“{title}”: “{key}” is not a telemetry item's key, such as battery.voltage_v"
            )));
        }
        (None, Some(name)) if !is_param_name(name) => {
            return Err(invalid(format!(
                "“{title}”: “{name}” is not a parameter's name"
            )));
        }
        _ => {}
    }
    if !rule.value.is_finite() {
        return Err(invalid(format!("“{title}”: the value must be a number")));
    }
    if let Some(hold) = rule.hold_s
        && !(hold.is_finite() && (0.0..=MAX_HOLD_S).contains(&hold))
    {
        return Err(invalid(format!(
            "“{title}”: hold for 0 to {MAX_HOLD_S} seconds"
        )));
    }
    let unit = rule
        .unit
        .map(|u| u.trim().to_owned())
        .filter(|u| !u.is_empty())
        .or_else(|| item.as_deref().and_then(unit_of_key).map(str::to_owned));
    Ok(Rule {
        item,
        param,
        op: rule.op,
        value: rule.value,
        unit,
        hold_s: rule.hold_s.filter(|s| *s > 0.0),
    })
}

impl ChecklistFile {
    /// The file's steps, checked, or why it cannot be read. Its JSON is
    /// read by whoever has it (the engine): a file from a newer UXDuck is
    /// refused by name.
    ///
    /// # Errors
    /// A format other than this one's, too many steps, or a step that
    /// cannot be kept.
    pub fn checked(self) -> Result<Self, Invalid> {
        if self.format != FILE_FORMAT {
            return Err(invalid(if self.format.starts_with("uxduck.checklist/") {
                format!(
                    "{} is from a newer UXDuck: this one reads {FILE_FORMAT}",
                    self.format
                )
            } else {
                format!("not a checklist file: its format is not {FILE_FORMAT}")
            }));
        }
        if self.steps.len() > MAX_STEPS {
            return Err(invalid(format!(
                "the file has {} steps. A checklist holds at most {MAX_STEPS}",
                self.steps.len()
            )));
        }
        let steps = self
            .steps
            .into_iter()
            .map(StepDef::checked)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            format: self.format,
            name: self.name,
            steps,
        })
    }
}

/// A pasted Markdown or Notion checklist as manual steps (K11): each list
/// item, to-do or plain line is a step, ticked or not (ticks are never
/// copied); a heading is the group of the steps under it (K16), unless it
/// is too long to be one; rules and blank lines are left out.
///
/// # Errors
/// No step in the text, or a line too long to be one.
pub fn steps_from_markdown(text: &str) -> Result<Vec<StepDef>, Invalid> {
    let mut steps = Vec::new();
    let mut group: Option<String> = None;
    for line in text.lines() {
        let mut rest = line.trim();
        if let Some(heading) = rest.strip_prefix('#') {
            let name = heading
                .trim_matches('#')
                .trim()
                .trim_matches(|c: char| c == '*' || c == '_' || c == '`')
                .trim();
            group =
                (!name.is_empty() && name.chars().count() <= MAX_GROUP).then(|| name.to_owned());
            continue;
        }
        if rest.is_empty()
            || rest.starts_with("```")
            || rest
                .chars()
                .all(|c| matches!(c, '-' | '*' | '_' | '=' | ' '))
        {
            continue;
        }
        // A bullet or a number: "- ", "* ", "+ ", "1. ", "1) ".
        for bullet in ["- ", "* ", "+ ", "• "] {
            if let Some(after) = rest.strip_prefix(bullet) {
                rest = after.trim_start();
                break;
            }
        }
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits > 0
            && let Some(after) = rest[digits..]
                .strip_prefix(". ")
                .or_else(|| rest[digits..].strip_prefix(") "))
        {
            rest = after.trim_start();
        }
        // A to-do box: "[ ]", "[x]", or Notion's and others' signs.
        for mark in ["[ ]", "[x]", "[X]", "☐", "☑", "☒", "✅", "✔", "□", "■"] {
            if let Some(after) = rest.strip_prefix(mark) {
                rest = after.trim_start();
                break;
            }
        }
        let title = rest
            .trim_matches(|c: char| c == '*' || c == '_' || c == '`')
            .trim();
        if title.is_empty() {
            continue;
        }
        steps.push(
            StepDef {
                group: group.clone(),
                ..StepDef::manual(title)
            }
            .checked()?,
        );
    }
    if steps.is_empty() {
        return Err(invalid("no steps in that text: one step per line"));
    }
    Ok(steps)
}

/// A step as it stood when the vehicle armed (K14).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordStep {
    pub title: String,
    pub kind: StepKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub severity: CheckSeverity,
    /// Whether it counted in the preflight or at the launch (K26).
    #[serde(default, skip_serializing_if = "CheckWhen::is_preflight")]
    pub when: CheckWhen,
    pub status: CheckStatus,
    /// A rule that asked for a person's tick too (K17).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub confirm: bool,
    /// Who ticked it and when, for a step a person ticks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tick: Option<Tick>,
    /// A rule's comparison and the value it read at arming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<Rule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
}

/// The checklist as it stood when the vehicle armed (K14), with what its
/// launch went past (K24): written to its history and its flight.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChecklistRecord {
    pub steps: Vec<RecordStep>,
    /// Checks set to hold, of any source, that UXDuck's launch was sent
    /// past, and how.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overridden: Vec<CheckRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_by: Option<PreflightOverride>,
    /// It left the ground without UXDuck launching it (on the sticks,
    /// say) while these checks set to hold did not pass (K23).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failing: Vec<CheckRef>,
    /// The preflight it armed under (K19): under way, finished or none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<Run>,
}

/// A preflight as it ended (K18, K19), kept in the vehicle's history:
/// finished by a person, which locked it, or left unfinished when a new
/// one started.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRecord {
    pub id: String,
    pub started: Tick,
    /// None when it was left unfinished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<Tick>,
    /// Each earlier finish of it that a person reopened, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reopenings: Vec<Reopening>,
    /// Each step as it stood: its tick, its rule's value and its status.
    pub steps: Vec<RecordStep>,
    /// The vehicle's own reasons, UXDuck's checks and plugins' checks as
    /// they stood.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<Check>,
}

impl RunRecord {
    /// The run in a sentence, for the history and the agent: "Preflight
    /// finished: 8 of 9 steps passed. Not passing: Link". One that was
    /// reopened says so: "Preflight finished again".
    #[must_use]
    pub fn describe(&self) -> String {
        let preflight = || self.steps.iter().filter(|s| s.when.is_preflight());
        let passed = preflight()
            .filter(|s| s.status == CheckStatus::Pass)
            .count();
        let total = preflight().count();
        let mut text = match (self.finished.is_some(), self.reopenings.is_empty()) {
            (true, true) => "Preflight finished",
            (true, false) => "Preflight finished again",
            (false, true) => "Preflight left unfinished",
            (false, false) => "Preflight reopened and left unfinished",
        }
        .to_owned();
        if total > 0 {
            let _ = write!(
                text,
                ": {passed} of {total} {} passed",
                if total == 1 { "step" } else { "steps" }
            );
        }
        // The launch's checks are for the place and the minute of the
        // launch, not for the preflight.
        let failing: Vec<&str> = self
            .checks
            .iter()
            .filter(|c| c.when != Some(CheckWhen::Launch) && c.status != CheckStatus::Pass)
            .map(|c| c.title.as_str())
            .collect();
        if !failing.is_empty() {
            let _ = write!(text, ". Not passing: {}", failing.join(", "));
        }
        text
    }
}

impl ChecklistRecord {
    /// The record in a sentence, for the history and the agent: "Armed
    /// with its checklist: 2 of 2 steps passed".
    #[must_use]
    pub fn describe(&self) -> String {
        // The preflight's steps: the launch's are for later, at arming.
        let preflight = || self.steps.iter().filter(|s| s.when.is_preflight());
        let passed = preflight()
            .filter(|s| s.status == CheckStatus::Pass)
            .count();
        let total = preflight().count();
        let mut text = if total == 0 {
            "Armed".to_owned()
        } else {
            format!(
                "Armed with its checklist: {passed} of {total} {} passed",
                if total == 1 { "step" } else { "steps" }
            )
        };
        if !self.overridden.is_empty() {
            let titles: Vec<&str> = self.overridden.iter().map(|c| c.title.as_str()).collect();
            let how = match &self.override_by {
                Some(PreflightOverride::Approval { .. }) => "by an approval",
                _ => "by the operator",
            };
            let _ = write!(text, ". Launched past {} {how}", titles.join(", "));
        }
        if !self.failing.is_empty() {
            let titles: Vec<&str> = self.failing.iter().map(|c| c.title.as_str()).collect();
            let _ = write!(
                text,
                ". Left the ground without UXDuck launching it while {} did not pass",
                titles.join(", ")
            );
        }
        // Under a finished preflight (K19). One under way says nothing:
        // a vehicle is armed for a test on the ground too.
        if let Some(finished) = self.run.as_ref().and_then(|r| r.finished.as_ref()) {
            let _ = write!(text, ". Preflight finished by {}", finished.who());
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(item: &str, op: Op, value: f64) -> StepDef {
        StepDef {
            kind: StepKind::Rule,
            title: "Battery".into(),
            group: None,
            severity: CheckSeverity::Block,
            when: CheckWhen::Preflight,
            rule: Some(Rule {
                item: Some(item.into()),
                param: None,
                op,
                value,
                unit: None,
                hold_s: Some(3.0),
            }),
            confirm: false,
        }
    }

    #[test]
    fn operators_compare_at_the_precision_parameters_travel_at() {
        assert!(Op::Ge.holds(22.2, 22.2));
        assert!(Op::Ge.holds(f64::from(22.2_f32), 22.2));
        assert!(!Op::Ge.holds(22.1, 22.2));
        assert!(Op::Gt.holds(22.3, 22.2) && !Op::Gt.holds(22.2, 22.2));
        assert!(Op::Le.holds(1.0, 1.0) && !Op::Lt.holds(1.0, 1.0));
        assert!(Op::Eq.holds(1.0, 1.0) && Op::Ne.holds(1.0, 2.0));
        assert_eq!(Op::from_symbol("!="), Some(Op::Ne));
        assert_eq!(
            serde_json::to_value(Op::Ge).unwrap(),
            serde_json::json!(">=")
        );
    }

    #[test]
    fn steps_are_checked_and_a_rules_unit_comes_from_its_key() {
        let step = rule("battery.voltage_v", Op::Ge, 22.2).checked().unwrap();
        assert_eq!(step.rule.unwrap().unit.as_deref(), Some("V"));
        assert!(rule("Battery Voltage", Op::Ge, 1.0).checked().is_err());
        assert!(
            rule("battery.voltage_v", Op::Ge, f64::NAN)
                .checked()
                .is_err()
        );
        let mut both = rule("battery.voltage_v", Op::Ge, 1.0);
        both.rule.as_mut().unwrap().param = Some("BATT_ARM_VOLT".into());
        assert!(both.checked().is_err());
        let mut param = rule("x.y", Op::Eq, 1.0);
        param.rule.as_mut().unwrap().item = None;
        param.rule.as_mut().unwrap().param = Some("arming_check".into());
        assert_eq!(
            param.checked().unwrap().rule.unwrap().param.as_deref(),
            Some("ARMING_CHECK")
        );
        let mut held = rule("battery.voltage_v", Op::Ge, 1.0);
        held.rule.as_mut().unwrap().hold_s = Some(601.0);
        assert!(held.checked().is_err());
        assert!(StepDef::manual("  ").checked().is_err());
        assert_eq!(
            StepDef::manual("  Propellers   tight ")
                .checked()
                .unwrap()
                .title,
            "Propellers tight"
        );
        let mut manual = StepDef::manual("Props");
        manual.rule = rule("battery.voltage_v", Op::Ge, 1.0).rule;
        assert!(manual.checked().is_err());
    }

    #[test]
    fn a_group_is_tidied_and_only_a_rule_asks_for_a_tick_too() {
        let mut step = rule("battery.voltage_v", Op::Ge, 22.2);
        step.group = Some("  Battery   and power ".into());
        step.confirm = true;
        let step = step.checked().unwrap();
        assert_eq!(step.group.as_deref(), Some("Battery and power"));
        assert!(step.confirm);
        let mut manual = StepDef::manual("Props");
        manual.group = Some("   ".into());
        manual.confirm = true;
        let manual = manual.checked().unwrap();
        assert_eq!(manual.group, None);
        assert!(!manual.confirm, "a manual step is ticked anyway");
        let mut long = StepDef::manual("Props");
        long.group = Some("g".repeat(MAX_GROUP + 1));
        assert!(long.checked().unwrap_err().0.contains("group"));
    }

    #[test]
    fn markdown_and_notion_lists_become_manual_steps() {
        let steps = steps_from_markdown(
            "# Before the flight\n\n- [ ] Propellers tight\n- [x] Payload latched\n* Battery strapped\n1. **Area clear**\n☐ Camera on\n---\nPlain line\n",
        )
        .unwrap();
        let titles: Vec<&str> = steps.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Propellers tight",
                "Payload latched",
                "Battery strapped",
                "Area clear",
                "Camera on",
                "Plain line"
            ]
        );
        assert!(
            steps
                .iter()
                .all(|s| s.kind == StepKind::Manual && s.severity == CheckSeverity::Warn)
        );
        assert!(
            steps
                .iter()
                .all(|s| s.group.as_deref() == Some("Before the flight")),
            "a heading is the group of the steps under it"
        );
        assert!(steps_from_markdown("# Only a heading\n\n").is_err());
    }

    #[test]
    fn markdown_headings_become_groups() {
        let long = "x".repeat(MAX_GROUP + 1);
        let steps = steps_from_markdown(&format!(
            "Loose step\n## **Battery** ##\n- [ ] Strapped\n- Charged\n### Controller\n1. Sticks free\n# {long}\n- Under a long heading\n"
        ))
        .unwrap();
        let groups: Vec<(&str, Option<&str>)> = steps
            .iter()
            .map(|s| (s.title.as_str(), s.group.as_deref()))
            .collect();
        assert_eq!(
            groups,
            [
                ("Loose step", None),
                ("Strapped", Some("Battery")),
                ("Charged", Some("Battery")),
                ("Sticks free", Some("Controller")),
                ("Under a long heading", None)
            ]
        );
    }

    fn ticked_at(at_us: u64, who: &str) -> Tick {
        Tick {
            by: Author {
                client_name: "uxduck-web".into(),
                client_kind: None,
                operator_id: None,
                operator_name: Some(who.into()),
            },
            at_us,
        }
    }

    fn step(id: &str, when: CheckWhen, tick: Option<Tick>) -> Step {
        Step {
            id: id.into(),
            kind: StepKind::Manual,
            title: id.into(),
            group: None,
            severity: CheckSeverity::Warn,
            when,
            rule: None,
            confirm: false,
            tick,
        }
    }

    #[test]
    fn a_finished_run_locks_the_preflight_steps_and_leaves_the_launch_ones() {
        let mut checklist = Checklist {
            steps: vec![
                step("props", CheckWhen::Preflight, Some(ticked_at(9, "Ada"))),
                step("area", CheckWhen::Launch, None),
                step("battery", CheckWhen::Preflight, Some(ticked_at(5, "Ben"))),
            ],
            ..Checklist::empty("v")
        };
        // A stored checklist's ticks are its first run, begun by the
        // earliest of them.
        let run = checklist.run_of_ticks("run1".into()).unwrap();
        assert_eq!((run.started.at_us, run.started.who()), (5, "Ben"));
        assert!(run.finished.is_none());
        assert!(Checklist::empty("v").run_of_ticks("r".into()).is_none());
        checklist.run = Some(run);
        assert!(!checklist.locks(&checklist.steps[0]), "under way");
        if let Some(run) = &mut checklist.run {
            run.finished = Some(ticked_at(20, "Ben"));
        }
        assert!(checklist.finished() && checklist.locks(&checklist.steps[0]));
        assert!(
            !checklist.locks(&checklist.steps[1]),
            "a launch step is ticked at the launch, after the preflight"
        );
        // Reopened, it takes ticks again and keeps when it was finished.
        if let Some(run) = &mut checklist.run {
            let finished = run.finished.take().unwrap();
            run.reopenings.push(Reopening {
                finished,
                reopened: ticked_at(30, "Ada"),
            });
        }
        assert!(!checklist.finished() && !checklist.locks(&checklist.steps[0]));
        let stored = serde_json::to_value(&checklist).unwrap();
        assert_eq!(stored["run"]["reopenings"][0]["finished"]["atUs"], 20);
        assert_eq!(stored["run"]["reopenings"][0]["reopened"]["atUs"], 30);
        // A checklist stored before runs reads as one with none.
        let stored: Checklist = serde_json::from_value(serde_json::json!({
            "vehicleId": "v", "steps": [], "updatedAtUs": 1, "version": 1
        }))
        .unwrap();
        assert!(stored.run.is_none());
        assert!(
            serde_json::to_value(&stored).unwrap().get("run").is_none(),
            "and is written back the same"
        );
    }

    #[test]
    fn a_run_as_it_ended_says_what_it_found() {
        let record_step = |title: &str, when, status| RecordStep {
            title: title.into(),
            kind: StepKind::Manual,
            group: None,
            severity: CheckSeverity::Warn,
            when,
            status,
            confirm: false,
            tick: None,
            rule: None,
            value: None,
        };
        let check = |title: &str, when, status| Check {
            id: format!("uxduck:{title}"),
            source: crate::domain::preflight::CheckSource::Uxduck,
            title: title.into(),
            group: None,
            severity: CheckSeverity::Warn,
            when,
            status,
            detail: None,
            fix: None,
            plugin: None,
        };
        let mut run = RunRecord {
            id: "run1".into(),
            started: ticked_at(1, "Ben"),
            finished: Some(ticked_at(2, "Ben")),
            reopenings: Vec::new(),
            steps: vec![
                record_step("Props", CheckWhen::Preflight, CheckStatus::Pass),
                record_step("Battery", CheckWhen::Preflight, CheckStatus::ToDo),
                record_step("Area clear", CheckWhen::Launch, CheckStatus::ToDo),
            ],
            checks: vec![
                check("Link", None, CheckStatus::Fail),
                check("Plan on the vehicle", None, CheckStatus::Pass),
                check("Home", Some(CheckWhen::Launch), CheckStatus::Unknown),
            ],
        };
        assert_eq!(
            run.describe(),
            "Preflight finished: 1 of 2 steps passed. Not passing: Link",
            "the launch's step and check are not the preflight's"
        );
        run.finished = None;
        run.checks.clear();
        assert_eq!(
            run.describe(),
            "Preflight left unfinished: 1 of 2 steps passed"
        );
        run.steps.clear();
        assert_eq!(run.describe(), "Preflight left unfinished");
        // One that was reopened says so, finished again or not.
        run.reopenings.push(Reopening {
            finished: ticked_at(2, "Ben"),
            reopened: ticked_at(3, "Ada"),
        });
        assert_eq!(run.describe(), "Preflight reopened and left unfinished");
        run.finished = Some(ticked_at(4, "Ada"));
        assert_eq!(run.describe(), "Preflight finished again");
    }

    #[test]
    fn a_file_has_no_ticks_and_names_its_version() {
        let checklist = Checklist {
            vehicle_id: "v".into(),
            steps: vec![Step {
                id: "s1".into(),
                kind: StepKind::Manual,
                title: "Propellers tight".into(),
                group: Some("Airframe".into()),
                severity: CheckSeverity::Block,
                when: CheckWhen::Launch,
                rule: None,
                confirm: false,
                tick: Some(Tick {
                    by: Author {
                        client_name: "uxduck-web".into(),
                        client_kind: None,
                        operator_id: None,
                        operator_name: Some("Ben".into()),
                    },
                    at_us: 5,
                }),
            }],
            copied_from: None,
            run: None,
            updated_at_us: 1,
            version: 1,
        };
        let file = checklist.to_file(Some("Alpha".into()));
        assert_eq!(
            file.steps,
            [StepDef {
                group: Some("Airframe".into()),
                severity: CheckSeverity::Block,
                when: CheckWhen::Launch,
                ..StepDef::manual("Propellers tight")
            }],
            "no ticks, and its group, whether it holds and where it counts"
        );
        // A step as written holds nothing and counts in the preflight.
        let written = StepDef::manual("Area clear");
        assert_eq!(written.severity, CheckSeverity::Warn);
        assert_eq!(written.when, CheckWhen::Preflight);
        let json = serde_json::to_value(&written).unwrap();
        assert!(
            json.get("when").is_none(),
            "the default is left out: {json}"
        );
        let launch: StepDef = serde_json::from_value(serde_json::json!({
            "kind": "manual", "title": "Area clear", "severity": "warn", "when": "launch"
        }))
        .unwrap();
        assert_eq!(launch.when, CheckWhen::Launch);
        assert_eq!(file.clone().checked().unwrap(), file);
        let newer = ChecklistFile {
            format: "uxduck.checklist/2".into(),
            ..file.clone()
        };
        assert!(newer.checked().unwrap_err().0.contains("newer"));
        let other = ChecklistFile {
            format: "something".into(),
            ..file
        };
        assert!(other.checked().is_err());
    }
}
