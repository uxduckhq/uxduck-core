use uxduck_core::domain::command as core;
use uxduck_core::domain::notification as n;

use crate::dto::command as dto;
use crate::dto::notification as nd;

impl From<&core::Command> for dto::Command {
    fn from(value: &core::Command) -> Self {
        match value.clone() {
            core::Command::Arm => Self::Arm,
            core::Command::Disarm => Self::Disarm,
            core::Command::ArmToggle => Self::ArmToggle,
            core::Command::SetMode { mode } => Self::SetMode { mode },
            core::Command::Rtl => Self::Rtl,
            core::Command::Land => Self::Land,
            core::Command::Reboot => Self::Reboot,
            core::Command::GoTo {
                latitude_deg,
                longitude_deg,
                altitude_m,
                altitude_frame,
                loiter_radius_m,
                heading,
            } => Self::GoTo {
                latitude_deg,
                longitude_deg,
                altitude_m,
                altitude_frame: altitude_frame.into(),
                loiter_radius_m,
                heading: heading.map(Into::into),
            },
            core::Command::SetHome { place } => Self::SetHome {
                place: place.map(Into::into),
            },
            core::Command::SetEkfOrigin { place } => Self::SetEkfOrigin {
                place: place.into(),
            },
            core::Command::Long { command, params } => Self::Long { command, params },
        }
    }
}

impl From<dto::Command> for core::Command {
    fn from(value: dto::Command) -> Self {
        match value {
            dto::Command::Arm => Self::Arm,
            dto::Command::Disarm => Self::Disarm,
            dto::Command::ArmToggle => Self::ArmToggle,
            dto::Command::SetMode { mode } => Self::SetMode { mode },
            dto::Command::Rtl => Self::Rtl,
            dto::Command::Land => Self::Land,
            dto::Command::Reboot => Self::Reboot,
            dto::Command::GoTo {
                latitude_deg,
                longitude_deg,
                altitude_m,
                altitude_frame,
                loiter_radius_m,
                heading,
            } => Self::GoTo {
                latitude_deg,
                longitude_deg,
                altitude_m,
                altitude_frame: altitude_frame.into(),
                loiter_radius_m,
                heading: heading.map(Into::into),
            },
            dto::Command::SetHome { place } => Self::SetHome {
                place: place.map(Into::into),
            },
            dto::Command::SetEkfOrigin { place } => Self::SetEkfOrigin {
                place: place.into(),
            },
            dto::Command::Long { command, params } => Self::Long { command, params },
        }
    }
}

impl From<core::PlaceAmsl> for dto::PlaceAmsl {
    fn from(value: core::PlaceAmsl) -> Self {
        Self {
            latitude_deg: value.latitude_deg,
            longitude_deg: value.longitude_deg,
            altitude_amsl_m: value.altitude_amsl_m,
        }
    }
}

impl From<dto::PlaceAmsl> for core::PlaceAmsl {
    fn from(value: dto::PlaceAmsl) -> Self {
        Self {
            latitude_deg: value.latitude_deg,
            longitude_deg: value.longitude_deg,
            altitude_amsl_m: value.altitude_amsl_m,
        }
    }
}

impl From<core::GoToHeading> for dto::GoToHeading {
    fn from(value: core::GoToHeading) -> Self {
        match value {
            core::GoToHeading::FaceDestination => Self::FaceDestination,
            core::GoToHeading::KeepCurrent => Self::KeepCurrent,
            core::GoToHeading::Set { degrees } => Self::Set { degrees },
        }
    }
}

impl From<dto::GoToHeading> for core::GoToHeading {
    fn from(value: dto::GoToHeading) -> Self {
        match value {
            dto::GoToHeading::FaceDestination => Self::FaceDestination,
            dto::GoToHeading::KeepCurrent => Self::KeepCurrent,
            dto::GoToHeading::Set { degrees } => Self::Set { degrees },
        }
    }
}

impl From<core::OfflineBehaviour> for dto::OfflineBehaviour {
    fn from(value: core::OfflineBehaviour) -> Self {
        match value {
            core::OfflineBehaviour::Allow => Self::Allow,
            core::OfflineBehaviour::Warn => Self::Warn,
            core::OfflineBehaviour::Block => Self::Block,
        }
    }
}

impl From<core::ReceiptStatus> for dto::ReceiptStatus {
    fn from(value: core::ReceiptStatus) -> Self {
        match value {
            core::ReceiptStatus::Sent => Self::Sent,
            core::ReceiptStatus::Accepted => Self::Accepted,
            core::ReceiptStatus::Rejected => Self::Rejected,
            core::ReceiptStatus::Timeout => Self::Timeout,
            core::ReceiptStatus::NotSent => Self::NotSent,
            core::ReceiptStatus::Interrupted => Self::Interrupted,
        }
    }
}

impl From<&core::Receipt> for dto::Receipt {
    fn from(r: &core::Receipt) -> Self {
        Self {
            id: r.id.clone(),
            vehicle_id: r.vehicle_id.clone(),
            command: (&r.command).into(),
            status: r.status.into(),
            detail: r.detail.clone(),
            reason: r.reason.map(|reason| match reason {
                core::RefusalReason::Expired => dto::RefusalReason::Expired,
                core::RefusalReason::Preflight => dto::RefusalReason::Preflight,
                core::RefusalReason::Replay => dto::RefusalReason::Replay,
                core::RefusalReason::Terms => dto::RefusalReason::Terms,
            }),
            preflight: r.preflight.iter().map(Into::into).collect(),
            preflight_override: r.preflight_override.as_ref().map(Into::into),
            sent_without_recent_heartbeat: r.sent_without_recent_heartbeat,
            forced: r.forced,
            attempts: r.attempts,
            client_id: r.client_id.clone(),
            operator_id: r.operator_id.clone(),
            sent_by: r.sent_by.as_ref().map(Into::into),
            remote: r.remote.as_ref().map(|by| dto::RemoteBy {
                user_id: by.user_id.clone(),
                name: by.name.clone(),
                command_id: by.command_id.clone(),
            }),
            created_at_us: r.created_at_us,
            completed_at_us: r.completed_at_us,
            version: r.version,
        }
    }
}

impl From<&n::Notification> for nd::Notification {
    fn from(v: &n::Notification) -> Self {
        Self {
            id: v.id.clone(),
            severity: match v.severity {
                n::Severity::Info => nd::Severity::Info,
                n::Severity::Warning => nd::Severity::Warning,
                n::Severity::Alarm => nd::Severity::Alarm,
            },
            kind: match v.kind {
                n::NotificationKind::Persistent => nd::NotificationKind::Persistent,
                n::NotificationKind::Transient => nd::NotificationKind::Transient,
            },
            state: match v.state {
                n::NotificationState::Active => nd::NotificationState::Active,
                n::NotificationState::Acknowledged => nd::NotificationState::Acknowledged,
                n::NotificationState::Resolved => nd::NotificationState::Resolved,
            },
            subject: match v.subject.clone() {
                n::Subject::Vehicle { id } => nd::Subject::Vehicle { id },
                n::Subject::Link { id } => nd::Subject::Link { id },
                n::Subject::Client { id } => nd::Subject::Client { id },
                n::Subject::VideoSource { id } => nd::Subject::VideoSource { id },
                n::Subject::System => nd::Subject::System,
            },
            title: v.title.clone(),
            body: v.body.clone(),
            actions: v
                .actions
                .iter()
                .filter_map(|a| {
                    Some(nd::NotificationAction {
                        label: a.label.clone(),
                        action: action(a.action.clone())?,
                    })
                })
                .collect(),
            condition: v.condition.clone(),
            report: v.report.clone(),
            created_at_us: v.created_at_us,
            version: v.version,
        }
    }
}

/// An old notification's take-back went with control from the account
/// (ADR-0048), so it has no button now.
fn action(action: n::Action) -> Option<nd::Action> {
    Some(match action {
        n::Action::EnableLink { link_id } => nd::Action::EnableLink { link_id },
        n::Action::ResolveApproval {
            approval_id,
            allow,
            remember,
        } => nd::Action::ResolveApproval {
            approval_id,
            allow,
            remember: remember.into(),
        },
        n::Action::OpenUrl { url } => nd::Action::OpenUrl { url },
        n::Action::OpenVideo { source_id } => nd::Action::OpenVideo { source_id },
        n::Action::OpenControllers { controller_id } => {
            nd::Action::OpenControllers { controller_id }
        }
        n::Action::EndFlight { flight_id } => nd::Action::EndFlight { flight_id },
        n::Action::KeepFlight { flight_id, always } => nd::Action::KeepFlight { flight_id, always },
        n::Action::OpenMessages {
            vehicle_id,
            message_id,
        } => nd::Action::OpenMessages {
            vehicle_id,
            message_id,
        },
        n::Action::ConnectSerial { device, usb } => nd::Action::ConnectSerial {
            device,
            usb: usb.as_ref().map(Into::into),
        },
        n::Action::AnswerPairing {
            code,
            allow,
            access,
        } => nd::Action::AnswerPairing {
            code,
            allow,
            access: access.map(Into::into),
        },
        n::Action::TakeBack { .. } => return None,
        n::Action::RestartSimulation { link_id } => nd::Action::RestartSimulation { link_id },
        n::Action::ResumeSimulation { link_id } => nd::Action::ResumeSimulation { link_id },
        n::Action::UseTerrainDataset {
            vehicle_id,
            dataset,
        } => nd::Action::UseTerrainDataset {
            vehicle_id,
            dataset: dataset.into(),
        },
        n::Action::OpenSetup { vehicle_id, row } => nd::Action::OpenSetup {
            vehicle_id,
            row: row.into(),
        },
        n::Action::PutTerrainAreas { vehicle_id } => nd::Action::PutTerrainAreas { vehicle_id },
    })
}

impl From<core::RiskOf> for dto::CommandRisk {
    fn from(value: core::RiskOf) -> Self {
        Self {
            level: match value.level {
                core::Risk::None => dto::Risk::None,
                core::Risk::Confirm => dto::Risk::Confirm,
                core::Risk::Dangerous => dto::Risk::Dangerous,
            },
            reason: value.reason.map(str::to_owned),
        }
    }
}

impl From<nd::Severity> for n::Severity {
    fn from(value: nd::Severity) -> Self {
        match value {
            nd::Severity::Info => Self::Info,
            nd::Severity::Warning => Self::Warning,
            nd::Severity::Alarm => Self::Alarm,
        }
    }
}

impl From<core::CommandClass> for dto::CommandClass {
    fn from(class: core::CommandClass) -> Self {
        match class {
            core::CommandClass::Safety => Self::Safety,
            core::CommandClass::Mission => Self::Mission,
            core::CommandClass::Payload => Self::Payload,
            core::CommandClass::Critical => Self::Critical,
        }
    }
}

impl From<dto::CommandClass> for core::CommandClass {
    fn from(class: dto::CommandClass) -> Self {
        match class {
            dto::CommandClass::Safety => Self::Safety,
            dto::CommandClass::Mission => Self::Mission,
            dto::CommandClass::Payload => Self::Payload,
            dto::CommandClass::Critical => Self::Critical,
        }
    }
}

#[cfg(test)]
mod heading_tests {
    use super::*;
    #[test]
    fn heading_choices_survive_wire_and_journal_conversion_and_old_commands_still_decode() {
        for heading in [
            None,
            Some(dto::GoToHeading::FaceDestination),
            Some(dto::GoToHeading::KeepCurrent),
            Some(dto::GoToHeading::Set { degrees: 125.0 }),
        ] {
            let command = dto::Command::GoTo {
                latitude_deg: 37.7,
                longitude_deg: -122.4,
                altitude_m: -5.0,
                altitude_frame: crate::dto::telemetry::AltitudeFrame::AboveHome,
                loiter_radius_m: Some(-75.0),
                heading,
            };
            let json = serde_json::to_value(&command).unwrap();
            if heading.is_none() {
                assert!(json.get("heading").is_none());
            }
            let parsed: dto::Command = serde_json::from_value(json).unwrap();
            let core: core::Command = parsed.into();
            assert_eq!(dto::Command::from(&core), command);
        }
    }
}

#[cfg(test)]
mod home_tests {
    use super::*;

    #[test]
    fn home_and_origin_have_the_same_shape_on_the_wire_and_in_the_journal() {
        let place = dto::PlaceAmsl {
            latitude_deg: -35.363_261,
            longitude_deg: 149.165_23,
            altitude_amsl_m: 584.1,
        };
        for (command, json) in [
            (
                dto::Command::SetHome { place: None },
                serde_json::json!({ "type": "set-home" }),
            ),
            (
                dto::Command::SetHome { place: Some(place) },
                serde_json::json!({
                    "type": "set-home",
                    "place": { "latitudeDeg": -35.363_261, "longitudeDeg": 149.165_23, "altitudeAmslM": 584.1 },
                }),
            ),
            (
                dto::Command::SetEkfOrigin { place },
                serde_json::json!({
                    "type": "set-ekf-origin",
                    "place": { "latitudeDeg": -35.363_261, "longitudeDeg": 149.165_23, "altitudeAmslM": 584.1 },
                }),
            ),
        ] {
            assert_eq!(serde_json::to_value(&command).unwrap(), json);
            let core: core::Command = serde_json::from_value(json.clone()).unwrap();
            assert_eq!(serde_json::to_value(&core).unwrap(), json);
            assert_eq!(dto::Command::from(&core), command);
        }
    }
}
