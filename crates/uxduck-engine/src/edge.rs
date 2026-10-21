//! This computer's connection to its account (ADR-0003 C7, S78). The root's
//! task holds one outbound WebSocket to the relay while signed in and
//! "Connect this computer to my account" is on; the engine keeps how it is
//! going, journals every connect and disconnect (what leaves the computer
//! is on the record), and answers `account/computers` with the account's
//! list and this computer's own state.

use serde_json::Value;
use uxduck_contracts::dto::edge::{AccountComputers, ComputerList, EdgeState, EdgeStatus};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::events::{Actor, Change};

use crate::Engine;

fn edge_actor() -> Actor {
    Actor {
        client_id: "edge".into(),
        operator_id: None,
        identity: None,
    }
}

impl Engine {
    /// Whether the person wants this computer connected to their account.
    #[must_use]
    pub fn edge_wanted(&self) -> bool {
        self.state.settings.connect_to_account
    }

    /// This computer's id in the account, as sync uses it: minted the first
    /// time either needs it.
    pub fn edge_device_id(&mut self) -> String {
        if self.state.sync.device_id.is_empty() {
            let mut sync = self.state.sync.clone();
            sync.device_id = format!("dv{}", &self.random.token()[..16]);
            sync.version += 1;
            self.commit_as(edge_actor(), Change::SyncChanged { sync });
        }
        self.state.sync.device_id.clone()
    }

    /// The root's task reports how the connection is; a connect or a
    /// disconnect is journaled.
    pub fn edge_changed(&mut self, status: EdgeStatus) {
        let was = self.edge_status.state == EdgeState::Connected;
        let now = status.state == EdgeState::Connected;
        if was != now {
            self.commit_as(
                edge_actor(),
                Change::EdgeConnection {
                    connected: now,
                    detail: status.problem.clone(),
                },
            );
        }
        self.edge_status = status;
    }

    #[must_use]
    pub fn edge_status(&self) -> &EdgeStatus {
        &self.edge_status
    }

    /// The account's list came back for `account/computers`: this
    /// computer's id and its connection go with it.
    pub(crate) fn computers_listed(
        &self,
        result: Result<Value, ApiError>,
    ) -> Result<Value, ApiError> {
        let list: ComputerList = serde_json::from_value(result?)
            .map_err(|e| ApiError::new(ErrorKind::Internal, e.to_string()))?;
        let id = &self.state.sync.device_id;
        let answer = AccountComputers {
            computers: list.computers,
            this_computer: (!id.is_empty()).then(|| id.clone()),
            edge: self.edge_status.clone(),
        };
        serde_json::to_value(answer).map_err(|e| ApiError::new(ErrorKind::Internal, e.to_string()))
    }
}

impl Engine {
    /// Whether flights should go to the account now (S82): stored while
    /// signed in unless switched off, and paused while armed if asked.
    ///
    /// # Errors
    /// Why they wait, for people.
    pub fn upload_wanted(&self) -> Result<(), String> {
        let settings = &self.state.settings;
        if !settings.store_flights {
            return Err("Storing flights in the account is off".into());
        }
        if !settings.upload_while_armed && self.state.vehicles.values().any(|v| v.armed) {
            return Err("Waiting until every vehicle is disarmed".into());
        }
        Ok(())
    }

    /// "Store flights and recordings in my account" (C17): off, the spool
    /// cuts nothing and lets go of what waits.
    #[must_use]
    pub const fn stores_flights(&self) -> bool {
        self.state.settings.store_flights
    }

    /// The uploader's report: what is waiting.
    pub fn upload_reported(&mut self, status: uxduck_contracts::dto::settings::UploadStatus) {
        self.upload_status = Some(status);
    }

    /// A recording's chunks the account acknowledged: journaled.
    pub fn flight_stored(&mut self, recording: String, chunks: u32, bytes: u64) {
        self.commit_as(
            edge_actor(),
            Change::FlightStored {
                recording,
                chunks,
                bytes,
            },
        );
    }
}

/// Signed out, as every computer starts.
#[must_use]
pub fn signed_out() -> EdgeStatus {
    EdgeStatus {
        state: EdgeState::SignedOut,
        round_trip_ms: None,
        clock_offset_ms: None,
        problem: None,
    }
}

/// The values a viewer of one vehicle sees (S79): attitude, speeds,
/// battery, GPS, link and the next waypoint. Everything else stays here.
const DETAIL_ITEMS: [&str; 14] = [
    "attitude.roll_deg",
    "attitude.pitch_deg",
    "attitude.yaw_deg",
    "heading.true_deg",
    "speed.air_mps",
    "speed.ground_mps",
    "speed.climb_mps",
    "battery.remaining_pct",
    "battery.voltage_v",
    "gps.fix_type",
    "gps.satellites",
    "link.rssi_pct",
    "mission.current",
    "navigation.waypoint_distance_m",
];

impl Engine {
    /// The vehicles anyone watching asked for, as they leave the computer
    /// (S79, ADR-0003 C8): the fused position for the list, the values a
    /// viewer shows for those open in detail. None while "Let my account
    /// watch this computer" is off.
    #[must_use]
    /// `live` is each camera wanted from away now, with its media node or
    /// its problem (S84): the daemon's live video task holds it, not the
    /// engine.
    pub fn edge_vehicles(
        &self,
        demand: &uxduck_contracts::dto::edge::EdgeDemand,
        live: &std::collections::BTreeMap<String, uxduck_contracts::dto::edge::RemoteCamera>,
    ) -> Vec<uxduck_contracts::dto::edge::RemoteVehicle> {
        use uxduck_contracts::dto::telemetry::VehicleTelemetry as TelemetryDto;
        use uxduck_core::domain::notification::{NotificationState, Severity, Subject};

        if !self.state.settings.watch_from_account {
            return Vec::new();
        }
        // Dated on the relay's clock, so viewers can age them.
        let offset = self.edge_status.clock_offset_ms.unwrap_or(0);
        let now_ms = i64::try_from(self.clock.now_us() / 1000).unwrap_or(i64::MAX);
        let at_ms = u64::try_from(now_ms.saturating_add(offset)).unwrap_or(0);
        let rank = |s: Severity| match s {
            Severity::Alarm => 0,
            Severity::Warning => 1,
            Severity::Info => 2,
        };
        self.state
            .vehicles
            .values()
            .filter(|v| !v.dismissed)
            .filter_map(|vehicle| {
                let named = |id: &String| *id == vehicle.id || vehicle.aliases.contains(id);
                let detail = demand.detail.iter().any(named);
                if !detail && !demand.summary {
                    return None;
                }
                let telemetry = self.telemetry.get(&vehicle.id).map(|live| {
                    let mut dto = TelemetryDto::from(live);
                    dto.positions.retain(|p| p.source == "fused");
                    if dto.positions.is_empty() {
                        dto.positions = TelemetryDto::from(live)
                            .positions
                            .into_iter()
                            .take(1)
                            .collect();
                    }
                    if detail {
                        dto.items.retain(|i| DETAIL_ITEMS.contains(&i.key.as_str()));
                    } else {
                        dto.items.clear();
                    }
                    dto
                });
                let mut notices: Vec<_> = self
                    .state
                    .notifications
                    .values()
                    .filter(|n| n.state == NotificationState::Active)
                    .filter(|n| matches!(&n.subject, Subject::Vehicle { id } if *id == vehicle.id))
                    .collect();
                notices.sort_by_key(|n| (rank(n.severity), std::cmp::Reverse(n.created_at_us)));
                Some(uxduck_contracts::dto::edge::RemoteVehicle {
                    id: vehicle.id.clone(),
                    name: vehicle.name.clone(),
                    vehicle_type: vehicle.vehicle_type.clone(),
                    autopilot: vehicle.autopilot.clone(),
                    size_cm: vehicle.size_cm,
                    presence: vehicle.presence.into(),
                    armed: vehicle.armed,
                    mode: vehicle.mode.clone(),
                    telemetry,
                    notices: notices.iter().take(5).map(|n| n.title.clone()).collect(),
                    lease: self.lease_of(&vehicle.id),
                    cameras: self
                        .state
                        .video_sources
                        .values()
                        .filter(|s| s.vehicle_id.as_ref().is_some_and(named))
                        .map(|s| uxduck_contracts::dto::edge::RemoteCamera {
                            id: s.id.clone(),
                            name: s.name.clone(),
                            live_on: live.get(&s.id).and_then(|l| l.live_on.clone()),
                            problem: live.get(&s.id).and_then(|l| l.problem.clone()),
                        })
                        .collect(),
                    at_ms,
                })
            })
            .collect()
    }

    /// What the daemon needs to publish a camera from away (S84), while
    /// "Let my account watch this computer" is on and the camera is known.
    #[must_use]
    pub fn edge_live_source(&self, camera_id: &str) -> Option<LiveSource> {
        use uxduck_core::domain::video::RecordingState;

        if !self.state.settings.watch_from_account {
            return None;
        }
        let source = self.state.video_sources.get(camera_id)?;
        let recording_id = self
            .state
            .video_recordings
            .values()
            .filter(|r| r.source_id == camera_id && r.state == RecordingState::Recording)
            .max_by_key(|r| r.started_at_us)
            .map(|r| r.id.clone());
        Some(LiveSource {
            codec: source.codec.clone(),
            recording_id,
            store: self.state.settings.store_flights,
        })
    }
}

/// A camera as the live video task needs it (S84).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveSource {
    /// Such as `avc1.64001f` or `hvc1…`; none until a picture came.
    pub codec: Option<String>,
    /// Its recording in progress, whose original replaces the cloud's live
    /// recording once stored (ADR-0003 C13).
    pub recording_id: Option<String>,
    /// "Store flights and recordings in my account" (C17).
    pub store: bool,
}
