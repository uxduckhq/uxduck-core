//! Live telemetry (ADR-0004 live record, ADR-0006 generic outputs). Kept in
//! memory with the vehicle's live record, never journaled; pushed to
//! `telemetry/subscribe` subscribers as `telemetry/update`, already coalesced
//! by the data plane to its tick rate.

use uxduck_contracts::dto::telemetry::{
    TelemetrySubscribeResult, VehicleTelemetry as TelemetryDto,
};
use uxduck_contracts::rpc::ServerNotification;
use uxduck_core::domain::telemetry::VehicleTelemetry;

use crate::{ConnectionId, Engine, rpc};

impl Engine {
    /// Merges what the data plane produced and tells subscribers.
    pub fn telemetry_observed(&mut self, update: VehicleTelemetry) {
        self.telemetry_history.observe(self.clock.now_us(), &update);
        let vehicle_id = update.vehicle_id.clone();
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
        for id in subscribers {
            self.send(id, &frame);
        }
        self.drive_launch_of_vehicle(&vehicle_id);
        self.review_prearm(&vehicle_id);
        self.review_flight(&vehicle_id);
        self.review_flight_time(&vehicle_id);
        self.review_practice(&vehicle_id);
        self.review_terrain(&vehicle_id);
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
