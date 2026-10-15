use uxduck_core::domain::find as core;

use crate::dto::find as dto;

impl From<dto::FindScope> for core::FindScope {
    fn from(v: dto::FindScope) -> Self {
        match v {
            dto::FindScope::Detect => Self::Detect,
            dto::FindScope::Nearby => Self::Nearby,
            dto::FindScope::Network => Self::Network,
        }
    }
}

impl From<&core::Find> for dto::Find {
    fn from(v: &core::Find) -> Self {
        Self {
            id: v.id.clone(),
            place: match v.place {
                core::FindPlace::Usb => dto::FindPlace::Usb,
                core::FindPlace::ThisComputer => dto::FindPlace::ThisComputer,
                core::FindPlace::Wifi => dto::FindPlace::Wifi,
                core::FindPlace::Network => dto::FindPlace::Network,
            },
            link: v.link.as_ref().map(Into::into),
            evidence: v.evidence.clone(),
            usb: v.usb.as_ref().map(Into::into),
            device_name: v.device_name.clone(),
            vehicle: v.vehicle.as_ref().map(|f| dto::FoundVehicle {
                system_id: f.system_id,
                vehicle_type: f.vehicle_type.clone(),
                autopilot: f.autopilot.clone(),
                armed: f.armed,
                vehicle_id: f.vehicle_id.clone(),
                name: f.name.clone(),
            }),
            reason: v.reason.map(|r| match r {
                core::FindReason::NoMavlink => dto::FindReason::NoMavlink,
                core::FindReason::Bootloader => dto::FindReason::Bootloader,
                core::FindReason::Busy => dto::FindReason::Busy,
                core::FindReason::Permission => dto::FindReason::Permission,
                core::FindReason::SilentKnownBoard => dto::FindReason::SilentKnownBoard,
                core::FindReason::Unreachable => dto::FindReason::Unreachable,
            }),
            detail: v.detail.clone(),
            seen_us: v.seen_us,
        }
    }
}
