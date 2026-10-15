use uxduck_core::domain::{link as core_link, vehicle as core};

use crate::dto::{link, vehicle as dto};

impl From<core::Presence> for dto::Presence {
    fn from(value: core::Presence) -> Self {
        match value {
            core::Presence::Online => Self::Online,
            core::Presence::Stale => Self::Stale,
            core::Presence::Offline => Self::Offline,
        }
    }
}

impl From<&core::Vehicle> for dto::Vehicle {
    fn from(v: &core::Vehicle) -> Self {
        Self {
            id: v.id.clone(),
            name: v.name.clone(),
            aliases: v.aliases.clone(),
            link_id: v.link_id.clone(),
            peer: v.peer.clone(),
            system_id: v.system_id,
            component_ids: v.component_ids.clone(),
            autopilot: v.autopilot.clone(),
            vehicle_type: v.vehicle_type.clone(),
            uid: v.uid.clone(),
            firmware: v.firmware.clone(),
            firmware_commit: v.firmware_commit.clone(),
            presence: v.presence.into(),
            last_heartbeat_us: v.last_heartbeat_us,
            heartbeat_age_ms: None,
            armed: v.armed,
            mode: v.mode.clone(),
            saved: v.saved,
            dismissed: v.dismissed,
            local_only: v.local_only,
            labels: v.labels.clone(),
            size_cm: v.size_cm,
            version: v.version,
        }
    }
}

impl From<&core_link::UsbId> for link::UsbId {
    fn from(value: &core_link::UsbId) -> Self {
        Self {
            vendor_id: value.vendor_id,
            product_id: value.product_id,
            serial_number: value.serial_number.clone(),
        }
    }
}

impl From<link::UsbId> for core_link::UsbId {
    fn from(value: link::UsbId) -> Self {
        Self {
            vendor_id: value.vendor_id,
            product_id: value.product_id,
            serial_number: value.serial_number,
        }
    }
}

impl From<&core_link::LinkKind> for link::LinkKind {
    fn from(value: &core_link::LinkKind) -> Self {
        match value.clone() {
            core_link::LinkKind::UdpListen { host, port } => Self::UdpListen { host, port },
            core_link::LinkKind::Serial { device, baud, usb } => Self::Serial {
                device,
                baud,
                usb: usb.as_ref().map(Into::into),
            },
            core_link::LinkKind::TcpClient { host, port } => Self::TcpClient { host, port },
            core_link::LinkKind::TcpServer { host, port } => Self::TcpServer { host, port },
            core_link::LinkKind::UdpClient { host, port } => Self::UdpClient { host, port },
            core_link::LinkKind::UdpForward { host, port } => Self::UdpForward { host, port },
            core_link::LinkKind::Simulator {
                vehicle,
                autopilot,
                latitude_deg,
                longitude_deg,
                altitude_m,
                heading_deg,
                frame,
                speedup,
                in_air_m,
                snapshot_id,
                binary,
                wipe,
                instance,
            } => Self::Simulator {
                vehicle: vehicle.into(),
                autopilot: autopilot.into(),
                latitude_deg,
                longitude_deg,
                altitude_m,
                heading_deg,
                frame,
                speedup,
                in_air_m,
                snapshot_id,
                binary,
                wipe,
                instance,
            },
        }
    }
}

impl From<link::LinkKind> for core_link::LinkKind {
    fn from(value: link::LinkKind) -> Self {
        match value {
            link::LinkKind::UdpListen { host, port } => Self::UdpListen { host, port },
            link::LinkKind::Serial { device, baud, usb } => Self::Serial {
                device,
                baud,
                usb: usb.map(Into::into),
            },
            link::LinkKind::TcpClient { host, port } => Self::TcpClient { host, port },
            link::LinkKind::TcpServer { host, port } => Self::TcpServer { host, port },
            link::LinkKind::UdpClient { host, port } => Self::UdpClient { host, port },
            link::LinkKind::UdpForward { host, port } => Self::UdpForward { host, port },
            link::LinkKind::Simulator {
                vehicle,
                autopilot,
                latitude_deg,
                longitude_deg,
                altitude_m,
                heading_deg,
                frame,
                speedup,
                in_air_m,
                snapshot_id,
                binary,
                wipe,
                instance,
            } => Self::Simulator {
                vehicle: vehicle.into(),
                autopilot: autopilot.into(),
                latitude_deg,
                longitude_deg,
                altitude_m,
                heading_deg,
                frame,
                speedup,
                in_air_m,
                snapshot_id,
                binary,
                wipe,
                instance,
            },
        }
    }
}

impl From<core_link::SimVehicle> for link::SimVehicle {
    fn from(value: core_link::SimVehicle) -> Self {
        match value {
            core_link::SimVehicle::Plane => Self::Plane,
            core_link::SimVehicle::Multicopter => Self::Multicopter,
            core_link::SimVehicle::Vtol => Self::Vtol,
            core_link::SimVehicle::Rover => Self::Rover,
            core_link::SimVehicle::Boat => Self::Boat,
            core_link::SimVehicle::Submarine => Self::Submarine,
            core_link::SimVehicle::Airship => Self::Airship,
        }
    }
}

impl From<link::SimVehicle> for core_link::SimVehicle {
    fn from(value: link::SimVehicle) -> Self {
        match value {
            link::SimVehicle::Plane => Self::Plane,
            link::SimVehicle::Multicopter => Self::Multicopter,
            link::SimVehicle::Vtol => Self::Vtol,
            link::SimVehicle::Rover => Self::Rover,
            link::SimVehicle::Boat => Self::Boat,
            link::SimVehicle::Submarine => Self::Submarine,
            link::SimVehicle::Airship => Self::Airship,
        }
    }
}

impl From<core_link::SimAutopilot> for link::SimAutopilot {
    fn from(value: core_link::SimAutopilot) -> Self {
        match value {
            core_link::SimAutopilot::Ardupilot => Self::Ardupilot,
            core_link::SimAutopilot::Px4 => Self::Px4,
        }
    }
}

impl From<link::SimAutopilot> for core_link::SimAutopilot {
    fn from(value: link::SimAutopilot) -> Self {
        match value {
            link::SimAutopilot::Ardupilot => Self::Ardupilot,
            link::SimAutopilot::Px4 => Self::Px4,
        }
    }
}

impl From<core_link::LinkPolicy> for link::LinkPolicy {
    fn from(value: core_link::LinkPolicy) -> Self {
        match value {
            core_link::LinkPolicy::Read => Self::Read,
            core_link::LinkPolicy::ReadWrite => Self::ReadWrite,
        }
    }
}

impl From<link::LinkPolicy> for core_link::LinkPolicy {
    fn from(value: link::LinkPolicy) -> Self {
        match value {
            link::LinkPolicy::Read => Self::Read,
            link::LinkPolicy::ReadWrite => Self::ReadWrite,
        }
    }
}

impl From<core_link::Topology> for link::Topology {
    fn from(value: core_link::Topology) -> Self {
        match value {
            core_link::Topology::Auto => Self::Auto,
            core_link::Topology::Single => Self::Single,
            core_link::Topology::Multi => Self::Multi,
        }
    }
}

impl From<link::Topology> for core_link::Topology {
    fn from(value: link::Topology) -> Self {
        match value {
            link::Topology::Auto => Self::Auto,
            link::Topology::Single => Self::Single,
            link::Topology::Multi => Self::Multi,
        }
    }
}

impl From<core_link::LinkStatus> for link::LinkStatus {
    fn from(value: core_link::LinkStatus) -> Self {
        match value {
            core_link::LinkStatus::Configured => Self::Configured,
            core_link::LinkStatus::Up => Self::Up,
            core_link::LinkStatus::Down => Self::Down,
            core_link::LinkStatus::PortInUse => Self::PortInUse,
            core_link::LinkStatus::DisabledByLoopDetection => Self::DisabledByLoopDetection,
            core_link::LinkStatus::Disabled => Self::Disabled,
            core_link::LinkStatus::Stopped => Self::Stopped,
        }
    }
}

impl From<&core_link::Link> for link::Link {
    fn from(l: &core_link::Link) -> Self {
        Self {
            id: l.id.clone(),
            name: l.name.clone(),
            kind: (&l.kind).into(),
            policy: l.policy.into(),
            topology: l.topology.into(),
            status: l.status.into(),
            status_detail: l.status_detail.clone(),
            created_at_us: l.created_at_us,
            vehicle_filter: l.vehicle_filter.clone(),
            from_link: l.from_link.clone(),
            keep_alive: l.keep_alive,
            launch: l.launch.as_ref().map(|launch| link::Launch {
                stage: match launch.stage {
                    core_link::LaunchStage::Restoring => link::LaunchStage::Restoring,
                    core_link::LaunchStage::Climbing => link::LaunchStage::Climbing,
                    core_link::LaunchStage::Flying => link::LaunchStage::Flying,
                    core_link::LaunchStage::Failed => link::LaunchStage::Failed,
                },
                detail: launch.detail.clone(),
            }),
            version: l.version,
        }
    }
}
