//! Plans between the domain and the wire. The two shapes are the same
//! field for field, so values move through serde; the test below fails if
//! a field is added on one side only.

use uxduck_core::domain::plan as p;

use crate::dto::plan as dto;

#[allow(
    clippy::expect_used,
    reason = "the shapes are the same, as the test below checks"
)]
fn through<A: serde::Serialize, B: serde::de::DeserializeOwned>(value: &A) -> B {
    serde_json::to_value(value)
        .and_then(serde_json::from_value)
        .expect("plan domain and wire types have the same shape")
}

impl From<&p::Plan> for dto::Plan {
    fn from(value: &p::Plan) -> Self {
        through(value)
    }
}

impl From<&p::PlanBody> for dto::PlanBody {
    fn from(value: &p::PlanBody) -> Self {
        through(value)
    }
}

impl From<&dto::PlanBody> for p::PlanBody {
    fn from(value: &dto::PlanBody) -> Self {
        through(value)
    }
}

impl From<dto::PlanVehicleType> for p::PlanVehicleType {
    fn from(value: dto::PlanVehicleType) -> Self {
        through(&value)
    }
}

impl From<dto::PlanAutopilot> for p::PlanAutopilot {
    fn from(value: dto::PlanAutopilot) -> Self {
        through(&value)
    }
}

impl From<&dto::LatLon> for p::LatLon {
    fn from(value: &dto::LatLon) -> Self {
        p::LatLon {
            latitude_deg: value.latitude_deg,
            longitude_deg: value.longitude_deg,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_field_survives_both_ways() {
        let mut body = p::PlanBody::new(
            "Ridge".into(),
            p::PlanVehicleType::Vtol,
            p::PlanAutopilot::Px4,
        );
        body.items.push(p::PlanItem {
            command: 16,
            frame: 10,
            params: [Some(1.0), None, Some(0.0), None],
            latitude_deg: -35.0,
            longitude_deg: 149.0,
            altitude_m: 40.0,
            autocontinue: true,
            pattern: None,
        });
        body.fence.polygons.push(p::FencePolygon {
            inclusion: true,
            points: vec![
                p::LatLon {
                    latitude_deg: 1.0,
                    longitude_deg: 2.0
                };
                3
            ],
            name: Some("Field".into()),
        });
        body.fence.circles.push(p::FenceCircle {
            inclusion: false,
            center: p::LatLon {
                latitude_deg: 1.0,
                longitude_deg: 2.0,
            },
            radius_m: 30.0,
            name: Some("Pond".into()),
        });
        body.fence.return_point = Some(p::LatLon {
            latitude_deg: 3.0,
            longitude_deg: 4.0,
        });
        body.fence.settings = p::FenceSettings {
            ceiling_m: Some(100.0),
            action: Some(p::FenceAction::ReturnToLaunch),
        };
        body.rally.push(p::RallyPoint {
            latitude_deg: 1.0,
            longitude_deg: 2.0,
            altitude_m: 50.0,
            name: None,
        });
        body.planned_home = Some(p::PlannedHome {
            latitude_deg: 1.0,
            longitude_deg: 2.0,
            altitude_m: Some(584.0),
        });
        body.labels.push("Survey".into());
        body.notes = "North field".into();
        let plan = p::Plan {
            id: "plan1".into(),
            bounds: body.bounds(),
            body: body.clone(),
            place_name: Some("Canberra".into()),
            place_at: Some(p::LatLon {
                latitude_deg: 1.0,
                longitude_deg: 2.0,
            }),
            made_by: p::PlanAuthor::Agent,
            created_at_us: 1,
            changed_at_us: 2,
            deleted_at_us: Some(3),
            uploads: vec![p::PlanUpload {
                vehicle_id: "v".into(),
                plan_version: 4,
                at_us: 5,
                verified: true,
                mission_version: 9,
            }],
            undo: 6,
            redo: 7,
            version: 8,
        };
        let wire = dto::Plan::from(&plan);
        assert_eq!(
            serde_json::to_value(&wire).unwrap(),
            serde_json::to_value(&plan).unwrap()
        );
        assert_eq!(p::PlanBody::from(&wire.body), body);
    }
}
