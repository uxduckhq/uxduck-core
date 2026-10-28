//! Live telemetry (ADR-0004 live record, ADR-0006 generic outputs). Kept in
//! memory with the vehicle's live record, never journaled; pushed to
//! `telemetry/subscribe` subscribers as `telemetry/update`, already coalesced
//! by the data plane to its tick rate.

use uxduck_contracts::dto::telemetry::{
    TelemetrySubscribeResult, VehicleTelemetry as TelemetryDto,
};
use uxduck_contracts::rpc::ServerNotification;
use uxduck_core::domain::telemetry::{TelemetryItem, VehicleTelemetry};

use crate::{ConnectionId, Engine, rpc};

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
        let source_observation =
            !update.positions.is_empty() || update.items.iter().any(|i| i.source != "flight");
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
        self.drive_launch_of_vehicle(&vehicle_id);
        self.review_prearm(&vehicle_id);
        if radio {
            self.review_radio(&vehicle_id);
        }
        if airspeed {
            self.review_airspeed(&vehicle_id);
        }
        self.review_flight(&vehicle_id);
        self.review_flight_time(&vehicle_id);
        self.review_practice(&vehicle_id);
        self.review_terrain(&vehicle_id);
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
                .map(TelemetryDto::from)
                .collect(),
        }
    }
}
