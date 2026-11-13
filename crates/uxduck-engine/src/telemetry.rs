//! Live telemetry (ADR-0004 live record, ADR-0006 generic outputs). Kept in
//! memory with the vehicle's live record, never journaled; pushed to
//! `telemetry/subscribe` subscribers as `telemetry/update`, already coalesced
//! by the data plane to its tick rate.

use uxduck_contracts::dto::telemetry::{
    LinkHealth as LinkHealthDto, RoundTripMissing, TelemetrySubscribeResult,
    VehicleTelemetry as TelemetryDto,
};
use uxduck_contracts::rpc::ServerNotification;
use uxduck_core::domain::telemetry::{LinkHealth, TelemetryItem, VehicleTelemetry};

use crate::{ConnectionId, Engine, rpc};

/// A link report not renewed for this long is no longer measured.
const LINK_HEALTH_STALE_MS: u64 = 5_000;

impl Engine {
    /// Merges what the data plane produced and tells subscribers.
    pub fn telemetry_observed(&mut self, update: VehicleTelemetry) {
        let mut update = update;
        // The battery per cell and what is left in it, for checklist rules
        // (S193).
        if update.items.iter().any(|i| i.key.starts_with("battery.")) {
            let derived = self.battery_items(&update);
            update.items.extend(derived);
        }
        // The operators' calculated values (ADR-0056 K2), after those.
        if self.calculates() {
            let calculated = self.calculated_items(&update);
            update.items.extend(calculated);
        }
        // A replay vehicle's telemetry is shown and nothing more (ADR-0039 R3).
        let replay = self
            .state
            .vehicles
            .get(&update.vehicle_id)
            .is_some_and(|v| v.replay);
        let source_observation = !replay
            && (!update.positions.is_empty() || update.items.iter().any(|i| i.source != "flight"));
        self.telemetry_history.observe(self.clock.now_us(), &update);
        let vehicle_id = update.vehicle_id.clone();
        // The radio's channels, for its calibration (S187).
        let radio = update.items.iter().any(|i| i.key.starts_with("rc."));
        // The airspeed's reading and ratio estimate (S190).
        let airspeed = update
            .items
            .iter()
            .any(|i| i.key == "speed.air_mps" || i.key == "airspeed.ratio");
        let frame = rpc::router::notification(&ServerNotification::TelemetryUpdate(
            TelemetryDto::from(&update),
        ));
        match self.telemetry.get_mut(&update.vehicle_id) {
            Some(latest) => latest.merge(&update),
            None => {
                self.telemetry.insert(update.vehicle_id.clone(), update);
            }
        }
        let subscribers: Vec<ConnectionId> = self.telemetry_subscribers.iter().copied().collect();
        if source_observation {
            self.observe_flight_usage(&vehicle_id);
        }
        for id in subscribers {
            self.send(id, &frame);
        }
        if replay {
            return;
        }
        self.drive_launch_of_vehicle(&vehicle_id);
        self.drive_plan_run_of_vehicle(&vehicle_id);
        self.review_prearm(&vehicle_id);
        if radio {
            self.review_radio(&vehicle_id);
        }
        if airspeed {
            self.review_airspeed(&vehicle_id);
        }
        self.review_flight(&vehicle_id);
        self.review_scenery_hit(&vehicle_id);
        self.review_flight_time(&vehicle_id);
        self.review_practice(&vehicle_id);
        self.review_terrain(&vehicle_id);
        self.review_vehicle_terrain(&vehicle_id);
        self.track_rules(&vehicle_id);
        self.review_preflight_soon(&vehicle_id);
    }

    /// An item the engine itself says of a vehicle, such as why it can't
    /// arm (`arming.checks`): merged and sent as the data plane's are,
    /// without the reviews of what the vehicle sent. Unchanged, nothing.
    pub(crate) fn telemetry_said(&mut self, vehicle_id: &str, item: TelemetryItem) {
        let same = self
            .telemetry
            .get(vehicle_id)
            .and_then(|t| t.items.iter().find(|i| i.key == item.key))
            .is_some_and(|i| i.text == item.text && i.value == item.value);
        if same
            || (item.text.is_none()
                && item.value.is_none()
                && !self.telemetry.contains_key(vehicle_id))
        {
            return;
        }
        let update = VehicleTelemetry {
            vehicle_id: vehicle_id.to_owned(),
            positions: Vec::new(),
            items: vec![item],
        };
        let frame = rpc::router::notification(&ServerNotification::TelemetryUpdate(
            TelemetryDto::from(&update),
        ));
        match self.telemetry.get_mut(vehicle_id) {
            Some(latest) => latest.merge(&update),
            None => {
                self.telemetry.insert(vehicle_id.to_owned(), update);
            }
        }
        let subscribers: Vec<ConnectionId> = self.telemetry_subscribers.iter().copied().collect();
        for id in subscribers {
            self.send(id, &frame);
        }
    }

    /// How each vehicle's link is doing, from the data plane once a second
    /// (S261): kept beside its telemetry and sent to subscribers as a
    /// `telemetry/update` with only `linkHealth`, never journaled. A
    /// replay's vehicle has no link.
    pub fn link_health_observed(&mut self, reports: Vec<(String, LinkHealth)>) {
        let now_us = self.clock.now_us();
        for (vehicle_id, health) in reports {
            if self
                .state
                .vehicles
                .get(&vehicle_id)
                .is_none_or(|v| v.replay)
            {
                continue;
            }
            let frame =
                rpc::router::notification(&ServerNotification::TelemetryUpdate(TelemetryDto {
                    vehicle_id: vehicle_id.clone(),
                    positions: Vec::new(),
                    items: Vec::new(),
                    link_health: Some((&health).into()),
                }));
            self.link_health.insert(vehicle_id, (health, now_us));
            let subscribers: Vec<ConnectionId> =
                self.telemetry_subscribers.iter().copied().collect();
            for id in subscribers {
                self.send(id, &frame);
            }
        }
    }

    /// How a vehicle's link is doing now: its age counts on from the last
    /// report, and a report the data plane stopped renewing (its vehicle
    /// gone from the link) keeps only that.
    pub(crate) fn link_health_of(&self, vehicle_id: &str) -> Option<LinkHealthDto> {
        let (health, at_us) = self.link_health.get(vehicle_id)?;
        let since_ms = self.clock.now_us().saturating_sub(*at_us) / 1000;
        let mut dto = LinkHealthDto::from(health);
        dto.age_ms = health.age_ms.map(|age| age + since_ms);
        if since_ms > LINK_HEALTH_STALE_MS {
            dto.round_trip_ms = None;
            dto.one_way_ms = None;
            dto.round_trip_missing = Some(RoundTripMissing::NotSent);
            dto.loss_percent = None;
        }
        Some(dto)
    }

    /// The latest telemetry of a vehicle, by id or alias.
    #[must_use]
    pub fn telemetry(&self, vehicle_id: &str) -> Option<&VehicleTelemetry> {
        let id = &self.vehicle(vehicle_id)?.id;
        self.telemetry.get(id)
    }

    /// Registers a subscriber and returns what it should start from.
    pub(crate) fn subscribe_telemetry(
        &mut self,
        connection: ConnectionId,
    ) -> TelemetrySubscribeResult {
        self.telemetry_subscribers.insert(connection);
        TelemetrySubscribeResult {
            vehicles: self
                .telemetry
                .values()
                .filter(|t| self.state.vehicles.contains_key(&t.vehicle_id))
                .map(|t| TelemetryDto {
                    link_health: self.link_health_of(&t.vehicle_id),
                    ..TelemetryDto::from(t)
                })
                .collect(),
        }
    }
}
