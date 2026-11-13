//! Calculated values (ADR-0056 K2): every operator's formulas, worked out
//! for a vehicle whenever an item one reads arrives, and added to its
//! telemetry as ordinary items in the `custom` group, so tiles, charts,
//! replays, checklist rules and the agent read them as they read the rest.
//! Deleted, they leave it.

use std::collections::BTreeSet;

use uxduck_contracts::dto::telemetry::{TelemetryCalculateParams, TelemetryCalculateResult};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::formula::Formula;
use uxduck_core::domain::telemetry::{TelemetryItem, VehicleTelemetry};
use uxduck_core::events::Change;

use uxduck_core::domain::parameter::parameter_id;

use crate::Engine;

/// An item older than this is not read, as a checklist rule's is not.
const FRESH_US: u64 = 10_000_000;
/// The source calculated items carry.
pub const SOURCE: &str = "calculated";

impl Engine {
    /// Whether any operator here has a calculated value.
    pub(crate) fn calculates(&self) -> bool {
        self.state
            .operators
            .values()
            .any(|o| !o.calculated_items.is_empty())
    }

    /// The calculated values an update changes: those that read an item in
    /// it, from its items and the vehicle's latest. One whose items or
    /// parameters have no recent value has none, so a stale number never
    /// stays. A key two operators share is the first's, by id.
    pub(crate) fn calculated_items(&self, update: &VehicleTelemetry) -> Vec<TelemetryItem> {
        let Some(vehicle) = self.vehicle(&update.vehicle_id) else {
            return Vec::new();
        };
        let now = self.clock.now_us();
        let latest = self.telemetry(&update.vehicle_id);
        let find = |key: &str| -> Option<&TelemetryItem> {
            update
                .items
                .iter()
                .find(|i| i.key == key)
                .or_else(|| latest.and_then(|t| t.items.iter().find(|i| i.key == key)))
                .filter(|i| now.saturating_sub(i.time_us) < FRESH_US && i.value.is_some())
        };
        let param = |name: &str| {
            self.state
                .parameters
                .get(&parameter_id(&vehicle.id, name))
                .map(|p| p.value)
        };
        let mut done: Vec<&str> = Vec::new();
        let mut out = Vec::new();
        for item in self
            .state
            .operators
            .values()
            .flat_map(|o| &o.calculated_items)
        {
            if done.contains(&item.key.as_str()) {
                continue;
            }
            done.push(&item.key);
            let Ok(formula) = Formula::parse(&item.formula) else {
                continue;
            };
            let Some(changed) = update
                .items
                .iter()
                .filter(|i| formula.items().contains(&i.key))
                .max_by_key(|i| i.time_us)
            else {
                continue;
            };
            let value = formula.evaluate(&|key| find(key).and_then(|i| i.value), &param);
            out.push(TelemetryItem {
                key: item.key.clone(),
                label: item.label.clone(),
                unit: item.unit,
                value,
                text: None,
                source: SOURCE.into(),
                component_id: changed.component_id,
                time_us: changed.time_us,
            });
        }
        out
    }

    /// After an operator changes or goes: a calculated value no operator
    /// works out any more has no value for each vehicle that had one, and
    /// leaves its latest telemetry, so a deleted formula's last number never
    /// stays on a tile. Whether a person deleted it here or on another
    /// computer, as a sync brings it.
    pub(crate) fn calculated_after(&mut self, change: &Change) {
        if !matches!(
            change,
            Change::OperatorUpserted { .. } | Change::OperatorRemoved { .. }
        ) {
            return;
        }
        let kept: BTreeSet<&str> = self
            .state
            .operators
            .values()
            .flat_map(|o| o.calculated_items.iter().map(|i| i.key.as_str()))
            .collect();
        let gone: Vec<(String, TelemetryItem)> = self
            .telemetry
            .iter()
            .flat_map(|(vehicle, t)| {
                t.items
                    .iter()
                    .filter(|i| i.source == SOURCE && !kept.contains(i.key.as_str()))
                    .map(|i| (vehicle.clone(), i.clone()))
            })
            .collect();
        let now = self.clock.now_us();
        for (vehicle, item) in gone {
            let key = item.key.clone();
            self.telemetry_said(
                &vehicle,
                TelemetryItem {
                    value: None,
                    text: None,
                    time_us: now,
                    ..item
                },
            );
            if let Some(latest) = self.telemetry.get_mut(&vehicle) {
                latest.items.retain(|i| i.key != key);
            }
        }
    }

    /// `telemetry/calculate`: a formula read, and worked out for a vehicle
    /// now from its latest telemetry and parameters.
    pub(crate) fn calculate(
        &self,
        params: &TelemetryCalculateParams,
    ) -> Result<TelemetryCalculateResult, ApiError> {
        let formula = Formula::parse(&params.formula)
            .map_err(|e| ApiError::new(ErrorKind::InvalidValue, e.message))?;
        let mut result = TelemetryCalculateResult {
            value: None,
            items: formula.items().to_vec(),
            params: formula.params().to_vec(),
            missing: Vec::new(),
        };
        let Some(id) = &params.vehicle_id else {
            return Ok(result);
        };
        let Some(vehicle) = self.vehicle(id) else {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                format!("no vehicle {id}"),
            ));
        };
        let now = self.clock.now_us();
        let latest = self.telemetry(&vehicle.id);
        let item = |key: &str| {
            latest
                .and_then(|t| t.items.iter().find(|i| i.key == key))
                .filter(|i| now.saturating_sub(i.time_us) < FRESH_US)
                .and_then(|i| i.value)
        };
        let param = |name: &str| {
            self.state
                .parameters
                .get(&parameter_id(&vehicle.id, name))
                .map(|p| p.value)
        };
        result.missing = formula
            .items()
            .iter()
            .filter(|key| item(key).is_none())
            .chain(formula.params().iter().filter(|name| param(name).is_none()))
            .cloned()
            .collect();
        result.value = formula.evaluate(&item, &param);
        Ok(result)
    }
}
