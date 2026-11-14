use uxduck_core::domain::companion as c;

use crate::dto::companion as dto;

impl From<c::CompanionRole> for dto::CompanionRole {
    fn from(v: c::CompanionRole) -> Self {
        match v {
            c::CompanionRole::Forward => Self::Forward,
            c::CompanionRole::Report => Self::Report,
        }
    }
}

impl From<dto::CompanionRole> for c::CompanionRole {
    fn from(v: dto::CompanionRole) -> Self {
        match v {
            dto::CompanionRole::Forward => Self::Forward,
            dto::CompanionRole::Report => Self::Report,
        }
    }
}

impl From<&c::CompanionComputer> for dto::CompanionComputer {
    fn from(v: &c::CompanionComputer) -> Self {
        Self {
            connectivity: v.connectivity.as_ref().map(|c| dto::VehicleConnectivity {
                video: c.video.into(),
            }),
            id: v.id.clone(),
            name: v.name.clone(),
            host: v.host.clone(),
            user: v.user.clone(),
            port: v.port,
            role: v.role.into(),
            component_id: v.component_id,
            key: v.key,
            check: v.check.as_ref().map(|check| dto::CompanionCheck {
                state: match check.state {
                    c::CheckState::Checking => dto::CheckState::Checking,
                    c::CheckState::Reached => dto::CheckState::Reached,
                    c::CheckState::Failed => dto::CheckState::Failed,
                },
                at_us: check.at_us,
                os: check.os.clone(),
                arch: check.arch.clone(),
                uxduck: check.uxduck.clone(),
                host_key: check.host_key.clone(),
                problem: check.problem.clone(),
                clock: check.clock.as_ref().map(|clock| dto::CompanionClock {
                    offset_ms: clock.offset_ms,
                    within_ms: clock.within_ms,
                    synchronised: clock.synchronised,
                    ntp: clock.ntp,
                    service: clock.service.clone(),
                }),
                network: check.network.as_ref().map(|n| dto::CompanionNetwork {
                    wifi: n
                        .wifi
                        .iter()
                        .map(|w| dto::WifiNetwork {
                            id: w.id.clone(),
                            ssid: w.ssid.clone(),
                            active: w.active,
                            managed: w.managed,
                        })
                        .collect(),
                    connectivity: n.connectivity.clone(),
                    checks_enabled: n.checks_enabled,
                    checks_available: n.checks_available,
                    check_uri: n.check_uri.clone(),
                    preference: n.preference.as_ref().map(|p| dto::NetworkPreference {
                        wifi_id: p.wifi_id.clone(),
                        cellular_id: p.cellular_id.clone(),
                    }),
                    default_devices: n.default_devices.clone(),
                    routes_known: n.routes_known,
                    transport: match n.transport() {
                        c::InternetTransport::Wifi => dto::InternetTransport::Wifi,
                        c::InternetTransport::Cellular => dto::InternetTransport::Cellular,
                        c::InternetTransport::Unknown => dto::InternetTransport::Unknown,
                    },
                    sampled_at_us: n.sampled_at_us,
                    connections: n
                        .connections
                        .iter()
                        .map(|c| dto::NetworkConnection {
                            id: c.id.clone(),
                            name: c.name.clone(),
                            kind: c.kind.clone(),
                            device: c.device.clone(),
                            ipv4: c.ipv4.clone(),
                            ipv6: c.ipv6.clone(),
                        })
                        .collect(),
                }),
            }),
        }
    }
}

impl From<c::CellularVideoPolicy> for dto::CellularVideoPolicy {
    fn from(v: c::CellularVideoPolicy) -> Self {
        match v {
            c::CellularVideoPolicy::Allow => Self::Allow,
            c::CellularVideoPolicy::LowData => Self::LowData,
            c::CellularVideoPolicy::TelemetryOnly => Self::TelemetryOnly,
        }
    }
}
impl From<dto::CellularVideoPolicy> for c::CellularVideoPolicy {
    fn from(v: dto::CellularVideoPolicy) -> Self {
        match v {
            dto::CellularVideoPolicy::Allow => Self::Allow,
            dto::CellularVideoPolicy::LowData => Self::LowData,
            dto::CellularVideoPolicy::TelemetryOnly => Self::TelemetryOnly,
        }
    }
}
