use uxduck_core::domain::settings as s;

use crate::dto::settings as dto;

impl From<s::LaunchAction> for dto::LaunchAction {
    fn from(value: s::LaunchAction) -> Self {
        match value {
            s::LaunchAction::Window => Self::Window,
            s::LaunchAction::Browser => Self::Browser,
            s::LaunchAction::MenuBar => Self::MenuBar,
        }
    }
}

impl From<dto::LaunchAction> for s::LaunchAction {
    fn from(value: dto::LaunchAction) -> Self {
        match value {
            dto::LaunchAction::Window => Self::Window,
            dto::LaunchAction::Browser => Self::Browser,
            dto::LaunchAction::MenuBar => Self::MenuBar,
        }
    }
}

impl From<s::ClosePolicy> for dto::ClosePolicy {
    fn from(v: s::ClosePolicy) -> Self {
        match v {
            s::ClosePolicy::KeepRunning => Self::KeepRunning,
            s::ClosePolicy::Quit => Self::Quit,
        }
    }
}

impl From<dto::ClosePolicy> for s::ClosePolicy {
    fn from(v: dto::ClosePolicy) -> Self {
        match v {
            dto::ClosePolicy::KeepRunning => Self::KeepRunning,
            dto::ClosePolicy::Quit => Self::Quit,
        }
    }
}

impl From<s::TerrainDataset> for crate::dto::terrain::TerrainDataset {
    fn from(v: s::TerrainDataset) -> Self {
        match v {
            s::TerrainDataset::Alos => Self::Alos,
            s::TerrainDataset::Copernicus => Self::Copernicus,
        }
    }
}

impl From<crate::dto::terrain::TerrainDataset> for s::TerrainDataset {
    fn from(v: crate::dto::terrain::TerrainDataset) -> Self {
        match v {
            crate::dto::terrain::TerrainDataset::Alos => Self::Alos,
            crate::dto::terrain::TerrainDataset::Copernicus => Self::Copernicus,
        }
    }
}

impl From<s::StorageCategory> for dto::StorageCategory {
    fn from(v: s::StorageCategory) -> Self {
        match v {
            s::StorageCategory::Tlogs => Self::Tlogs,
            s::StorageCategory::EventLog => Self::EventLog,
            s::StorageCategory::Database => Self::Database,
            s::StorageCategory::Backups => Self::Backups,
            s::StorageCategory::Logs => Self::Logs,
            s::StorageCategory::Plugins => Self::Plugins,
            s::StorageCategory::Video => Self::Video,
            s::StorageCategory::Simulators => Self::Simulators,
            s::StorageCategory::SimulatorLogs => Self::SimulatorLogs,
            s::StorageCategory::SimulatorData => Self::SimulatorData,
            s::StorageCategory::Parameters => Self::Parameters,
            s::StorageCategory::VehicleFiles => Self::VehicleFiles,
            s::StorageCategory::DemoVideos => Self::DemoVideos,
        }
    }
}

impl From<dto::StorageCategory> for s::StorageCategory {
    fn from(v: dto::StorageCategory) -> Self {
        match v {
            dto::StorageCategory::Tlogs => Self::Tlogs,
            dto::StorageCategory::EventLog => Self::EventLog,
            dto::StorageCategory::Database => Self::Database,
            dto::StorageCategory::Backups => Self::Backups,
            dto::StorageCategory::Logs => Self::Logs,
            dto::StorageCategory::Plugins => Self::Plugins,
            dto::StorageCategory::Video => Self::Video,
            dto::StorageCategory::Simulators => Self::Simulators,
            dto::StorageCategory::SimulatorLogs => Self::SimulatorLogs,
            dto::StorageCategory::SimulatorData => Self::SimulatorData,
            dto::StorageCategory::Parameters => Self::Parameters,
            dto::StorageCategory::VehicleFiles => Self::VehicleFiles,
            dto::StorageCategory::DemoVideos => Self::DemoVideos,
        }
    }
}

impl From<s::PilotInputMethod> for dto::PilotInputMethod {
    fn from(v: s::PilotInputMethod) -> Self {
        match v {
            s::PilotInputMethod::Automatic => Self::Automatic,
            s::PilotInputMethod::ManualControl => Self::ManualControl,
            s::PilotInputMethod::RcChannelsOverride => Self::RcChannelsOverride,
        }
    }
}
impl From<dto::PilotInputMethod> for s::PilotInputMethod {
    fn from(v: dto::PilotInputMethod) -> Self {
        match v {
            dto::PilotInputMethod::Automatic => Self::Automatic,
            dto::PilotInputMethod::ManualControl => Self::ManualControl,
            dto::PilotInputMethod::RcChannelsOverride => Self::RcChannelsOverride,
        }
    }
}

/// The wire form, with the command line's close policy where none was set.
#[must_use]
pub fn settings_dto(settings: &s::Settings, default_close: s::ClosePolicy) -> dto::Settings {
    dto::Settings {
        include_beta: Some(settings.include_beta),
        close_policy: settings.close_policy.unwrap_or(default_close).into(),
        close_policy_set: settings.close_policy.is_some(),
        auto_delete: settings
            .auto_delete_days
            .iter()
            .map(|(category, days)| dto::AutoDelete {
                category: (*category).into(),
                days: *days,
            })
            .chain(
                settings
                    .simulator_log_retention_days
                    .map(|days| dto::AutoDelete {
                        category: dto::StorageCategory::SimulatorLogs,
                        days,
                    }),
            )
            .collect(),
        simulator_log_max_bytes: Some(settings.simulator_log_max_bytes),
        shell_path: settings.shell_path.clone(),
        launch_action: Some(settings.launch_action.into()),
        usage_analytics: settings.usage_analytics,
        share_conversations: settings.share_conversations,
        share_flight_data: settings.share_flight_data,
        video_ports: settings.video_ports.clone(),
        video_recording: settings.video_recording.into(),
        show_in_menu_bar: settings.show_in_menu_bar,
        connect_to_account: settings.connect_to_account,
        watch_from_account: settings.watch_from_account,
        remote_control: settings
            .remote_control
            .iter()
            .map(|c| (*c).into())
            .collect(),
        network_address: settings.network_address.clone(),
        network_addresses: if settings.network_addresses.is_empty() {
            settings.network_address.iter().cloned().collect()
        } else {
            settings.network_addresses.clone()
        },
        supervision_grace_s: settings.supervision_grace_s,
        gcs_system_id: settings.gcs_system_id,
        store_flights: settings.store_flights,
        save_flights: settings.save_flights,
        upload_while_armed: settings.upload_while_armed,
        upload_video_while_flying: settings.upload_video_while_flying,
        start_at_login: settings.start_at_login,
        login_item: None,
        message_notify_severity: settings.message_notify_severity,
        recreate_simulations: settings.recreate_simulations,
        traffic_alert_m: settings.traffic_alert_m,
        traffic_alert_up_m: settings.traffic_alert_up_m,
        terrain_dataset: settings.terrain_dataset.into(),
        terrain_areas: settings
            .terrain_areas
            .iter()
            .map(|a| crate::dto::terrain::TerrainArea {
                id: a.id.clone(),
                name: a.name.clone(),
                south_deg: a.south_deg,
                west_deg: a.west_deg,
                north_deg: a.north_deg,
                east_deg: a.east_deg,
            })
            .collect(),
        vehicle_terrain_datasets: settings
            .vehicle_terrain_datasets
            .iter()
            .map(
                |(vehicle_id, dataset)| crate::dto::terrain::VehicleTerrainDataset {
                    vehicle_id: vehicle_id.clone(),
                    dataset: Some((*dataset).into()),
                },
            )
            .collect(),
        version: settings.version,
        pilot_input_methods: settings
            .pilot_input_methods
            .iter()
            .map(|(vehicle_id, method)| dto::PilotInputSetting {
                vehicle_id: vehicle_id.clone(),
                method: (*method).into(),
            })
            .collect(),
    }
}
