//! Plugins' preflight checks (S194, ADR-0038 K12, K13).
//!
//! A plugin declares its checks in its manifest and reports where each
//! stands for a vehicle with `Effects.checks`. The engine keeps the latest
//! report of each in memory, never journaled, and lists the checks in every
//! vehicle's preflight list after its checklist, by plugin. A report older
//! than the check's `staleMs`, or a plugin that is off, stopped or gone,
//! makes the check unknown; a running plugin that has not reported on the
//! vehicle yet is waited for. Both hold arming when the check blocks.
//!
//! A plugin's `block` check stops arming only once a person allowed the
//! plugin's checks to (`plugin/setChecksBlock`, or at install); until then
//! it is listed and evaluated as a warning. A plugin only reports its own
//! checks: it cannot tick a checklist's step or change any other check,
//! and a report of a check it does not declare is ignored.

use std::collections::{BTreeMap, BTreeSet};

use uxduck_contracts::dto::plugin::{PluginManifest, ReportedCheck, ReportedStatus};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::plugin::{Plugin, PluginStatus, valid_id};
use uxduck_core::domain::preflight::{
    Check, CheckFix, CheckPlugin, CheckSeverity, CheckSource, CheckStatus,
};
use uxduck_core::domain::vehicle::Vehicle;
use uxduck_core::events::Change;

use crate::{ConnectionId, Engine};

/// A plugin declares at most this many checks.
const MOST_CHECKS: usize = 16;
/// A check's title, at most.
const TITLE_CHARS: usize = 80;
/// The words of a report, at most; longer ones are cut.
const DETAIL_CHARS: usize = 160;
/// How long a report may count, from a second to a day.
const STALE_MS: std::ops::RangeInclusive<u32> = 1_000..=86_400_000;

/// The latest report of one check for one vehicle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Report {
    status: CheckStatus,
    detail: Option<String>,
    at_us: u64,
}

/// Reports by plugin, then by vehicle and check. Lost with the daemon: a
/// plugin reports again when it starts.
pub(crate) type Reports = BTreeMap<String, BTreeMap<(String, String), Report>>;

/// What the engine checks of a manifest's `checks`.
pub(crate) fn check_declared(manifest: &PluginManifest) -> Result<(), String> {
    if manifest.checks.len() > MOST_CHECKS {
        return Err(format!(
            "{} declares {} checks; a plugin declares at most {MOST_CHECKS}",
            manifest.id,
            manifest.checks.len()
        ));
    }
    let mut seen = BTreeSet::new();
    for check in &manifest.checks {
        if !valid_id(&check.id) {
            return Err(format!(
                "check {:?} must be lower case letters, digits and dashes",
                check.id
            ));
        }
        if !seen.insert(check.id.as_str()) {
            return Err(format!("check {:?} is declared twice", check.id));
        }
        let title = check.title.trim();
        if title.is_empty() || title.chars().count() > TITLE_CHARS {
            return Err(format!(
                "check {:?} needs a title of at most {TITLE_CHARS} characters, such as \"Wind under 8 m/s\"",
                check.id
            ));
        }
        if !STALE_MS.contains(&check.stale_ms) {
            return Err(format!(
                "check {:?}: staleMs is {} ms; it may be 1000 (a second) to 86400000 (a day)",
                check.id, check.stale_ms
            ));
        }
    }
    Ok(())
}

/// A plugin's words for a report: one line, cut short, none when empty.
fn words(detail: Option<String>) -> Option<String> {
    let line = detail?.lines().next().unwrap_or_default().trim().to_owned();
    if line.is_empty() {
        return None;
    }
    if line.chars().count() <= DETAIL_CHARS {
        return Some(line);
    }
    let cut: String = line.chars().take(DETAIL_CHARS - 1).collect();
    Some(format!("{}…", cut.trim_end()))
}

/// "10 min", "45 s", "2 h": how long a report counts.
fn duration_words(ms: u32) -> String {
    let s = ms / 1000;
    match s {
        s if s >= 3600 && s % 3600 == 0 => format!("{} h", s / 3600),
        s if s >= 60 && s % 60 == 0 => format!("{} min", s / 60),
        s if s >= 60 => format!("{} min {} s", s / 60, s % 60),
        s => format!("{s} s"),
    }
}

impl Engine {
    /// What a plugin reported of its checks (K12): the latest of each,
    /// for vehicles UXDuck knows, only the checks it declares.
    pub(crate) fn plugin_checks_reported(&mut self, plugin: &Plugin, reported: Vec<ReportedCheck>) {
        if reported.is_empty() {
            return;
        }
        let now = self.clock.now_us();
        let mut vehicles = BTreeSet::new();
        for report in reported {
            if !plugin.checks.iter().any(|c| c.id == report.id) {
                crate::plugins::tracing_line(
                    &plugin.id,
                    &format!("check {}: not in its manifest's checks; ignored", report.id),
                );
                continue;
            }
            let Some(vehicle_id) = self.vehicle(&report.vehicle_id).map(|v| v.id.clone()) else {
                continue;
            };
            let status = match report.status {
                ReportedStatus::Pass => CheckStatus::Pass,
                ReportedStatus::Fail => CheckStatus::Fail,
                ReportedStatus::Waiting => CheckStatus::Waiting,
            };
            self.preflight
                .plugin_reports
                .entry(plugin.id.clone())
                .or_default()
                .insert(
                    (vehicle_id.clone(), report.id),
                    Report {
                        status,
                        detail: words(report.detail),
                        at_us: now,
                    },
                );
            vehicles.insert(vehicle_id);
        }
        // Journaled only when a status or its words change.
        for vehicle_id in vehicles {
            self.review_preflight(&vehicle_id);
        }
    }

    /// A plugin stopped, was turned off or removed: its reports no longer
    /// count. Its checks are looked at again once its record says so.
    pub(crate) fn forget_plugin_reports(&mut self, id: &str) {
        self.preflight.plugin_reports.remove(id);
    }

    /// A plugin's record changed (installed, started, stopped, turned off
    /// or on, removed, allowed to block): every vehicle's checks are
    /// looked at again when it declares any or had reported.
    pub(crate) fn plugin_checks_changed(&mut self, id: &str, had_checks: bool) {
        let declares = self
            .state
            .plugins
            .get(id)
            .is_some_and(|p| !p.checks.is_empty());
        if declares || had_checks {
            self.review_preflights();
        }
    }

    /// Every plugin's checks for a vehicle, by plugin name, after the
    /// vehicle's checklist (K12, K13).
    pub(crate) fn plugin_checks(&self, vehicle: &Vehicle) -> Vec<Check> {
        let mut plugins: Vec<&Plugin> = self
            .state
            .plugins
            .values()
            .filter(|p| !p.checks.is_empty())
            .collect();
        plugins.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        let now = self.clock.now_us();
        let mut checks = Vec::new();
        for plugin in plugins {
            let may_block = plugin.checks_may_block();
            let reports = self.preflight.plugin_reports.get(&plugin.id);
            let name = &plugin.name;
            for declared in &plugin.checks {
                let to_plugin = Some(CheckFix::Plugin {
                    plugin_id: plugin.id.clone(),
                });
                let now_status = if plugin.enabled {
                    plugin.status
                } else {
                    PluginStatus::Off
                };
                let (status, detail, fix) = match now_status {
                    PluginStatus::Off => (
                        CheckStatus::Unknown,
                        Some(format!("The {name} plugin isn't running: it's turned off.")),
                        to_plugin,
                    ),
                    PluginStatus::Failed => (
                        CheckStatus::Unknown,
                        Some(format!("The {name} plugin isn't running: it stopped.")),
                        to_plugin,
                    ),
                    PluginStatus::Starting => (
                        CheckStatus::Waiting,
                        Some(format!("The {name} plugin is starting.")),
                        to_plugin,
                    ),
                    PluginStatus::Running => {
                        match reports
                            .and_then(|r| r.get(&(vehicle.id.clone(), declared.id.clone())))
                        {
                            None => (
                                CheckStatus::Waiting,
                                Some(format!(
                                    "The {name} plugin hasn't reported on {} yet.",
                                    vehicle.name
                                )),
                                to_plugin,
                            ),
                            Some(report)
                                if now.saturating_sub(report.at_us)
                                    > u64::from(declared.stale_ms) * 1000 =>
                            {
                                (
                                    CheckStatus::Unknown,
                                    Some(format!(
                                        "The {name} plugin hasn't reported for {}.",
                                        duration_words(declared.stale_ms)
                                    )),
                                    to_plugin,
                                )
                            }
                            Some(report) => (report.status, report.detail.clone(), None),
                        }
                    }
                };
                let asks = declared.severity == CheckSeverity::Block;
                checks.push(Check {
                    id: format!("plugin:{}:{}", plugin.id, declared.id),
                    source: CheckSource::Plugin,
                    title: declared.title.clone(),
                    severity: if asks && may_block {
                        CheckSeverity::Block
                    } else {
                        CheckSeverity::Warn
                    },
                    status,
                    detail,
                    fix,
                    plugin: Some(CheckPlugin {
                        id: plugin.id.clone(),
                        name: plugin.name.clone(),
                        held_back: asks && !may_block,
                    }),
                });
            }
        }
        checks
    }

    /// A person's answer to whether a plugin's blocking checks may stop
    /// arming (K13), journaled under them.
    pub(crate) fn set_checks_block(
        &mut self,
        connection: ConnectionId,
        id: &str,
        block: bool,
    ) -> Result<Plugin, ApiError> {
        let Some(mut plugin) = self.state.plugins.get(id).cloned() else {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                format!("no plugin {id}"),
            ));
        };
        if !plugin.asks_to_block() {
            return Err(ApiError::new(
                ErrorKind::InvalidRequest,
                format!(
                    "the {} plugin has no checks that could stop arming",
                    plugin.name
                ),
            ));
        }
        if plugin.checks_block == Some(block) {
            return Ok(plugin);
        }
        plugin.checks_block = Some(block);
        plugin.revision += 1;
        self.commit(
            connection,
            Change::PluginUpserted {
                plugin: plugin.clone(),
            },
        );
        self.plugin_checks_changed(id, true);
        Ok(plugin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_words_are_one_short_line() {
        assert_eq!(words(None), None);
        assert_eq!(words(Some("  \n".into())), None);
        assert_eq!(
            words(Some("Over 8 m/s.\nsecond line".into())).as_deref(),
            Some("Over 8 m/s.")
        );
        let long = words(Some("x".repeat(400))).unwrap();
        assert_eq!(long.chars().count(), DETAIL_CHARS);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn stale_times_read_as_people_say_them() {
        assert_eq!(duration_words(600_000), "10 min");
        assert_eq!(duration_words(45_000), "45 s");
        assert_eq!(duration_words(90_000), "1 min 30 s");
        assert_eq!(duration_words(7_200_000), "2 h");
    }
}
