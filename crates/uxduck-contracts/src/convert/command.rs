use uxduck_core::domain::command as core;
use uxduck_core::domain::notification as n;

use crate::dto::command as dto;
use crate::dto::notification as nd;

impl From<&core::Command> for dto::Command {
    fn from(value: &core::Command) -> Self {
        match value.clone() {
            core::Command::Arm => Self::Arm,
            core::Command::Disarm => Self::Disarm,
            core::Command::SetMode { mode } => Self::SetMode { mode },
            core::Command::Rtl => Self::Rtl,
            core::Command::Land => Self::Land,
            core::Command::Reboot => Self::Reboot,
            core::Command::GoTo {
                latitude_deg,
                longitude_deg,
                altitude_m,
                altitude_frame,
                heading,
            } => Self::GoTo {
                latitude_deg,
                longitude_deg,
                altitude_m,
                altitude_frame: altitude_frame.into(),
                heading: heading.map(Into::into),
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
            dto::Command::SetMode { mode } => Self::SetMode { mode },
            dto::Command::Rtl => Self::Rtl,
            dto::Command::Land => Self::Land,
            dto::Command::Reboot => Self::Reboot,
            dto::Command::GoTo {
                latitude_deg,
                longitude_deg,
                altitude_m,
                altitude_frame,
                heading,
            } => Self::GoTo {
                latitude_deg,
                longitude_deg,
                altitude_m,
                altitude_frame: altitude_frame.into(),
                heading: heading.map(Into::into),
            },
            dto::Command::Long { command, params } => Self::Long { command, params },
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
            }),
            sent_without_recent_heartbeat: r.sent_without_recent_heartbeat,
            forced: r.forced,
            attempts: r.attempts,
            client_id: r.client_id.clone(),
            operator_id: r.operator_id.clone(),
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
                .map(|a| nd::NotificationAction {
                    label: a.label.clone(),
                    action: match a.action.clone() {
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
                        n::Action::TakeBack { vehicle_id } => nd::Action::TakeBack { vehicle_id },
                        n::Action::RestartSimulation { link_id } => {
                            nd::Action::RestartSimulation { link_id }
                        }
                        n::Action::UseTerrainDataset {
                            vehicle_id,
                            dataset,
                        } => nd::Action::UseTerrainDataset {
                            vehicle_id,
                            dataset: dataset.into(),
                        },
                    },
                })
                .collect(),
            condition: v.condition.clone(),
            report: v.report.clone(),
            created_at_us: v.created_at_us,
            version: v.version,
        }
    }
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
