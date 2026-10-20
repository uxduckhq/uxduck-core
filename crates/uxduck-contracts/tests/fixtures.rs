//! Every fixture deserialises into its DTO and serialises back to the same
//! JSON. The same files must pass the generated Valibot schemas
//! (`packages/protocol/src/fixtures.test.ts`).
//!
//! Names are `<method>.<part>[.<case>].json`, with `/` in the method written
//! as `_`: `request` is a `ClientRequest`, `notification` a
//! `ServerNotification` or `ClientNotification`, and `result` the method's
//! result type. Errors are `error.<kind>.json`, each an `ApiError`.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    reason = "test helpers"
)]

use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use uxduck_contracts::dto::Empty;
use uxduck_contracts::dto::initialize::{Capability, ClientKind, InitializeResult};
use uxduck_contracts::dto::link::{Link, SerialListResult};
use uxduck_contracts::dto::operator::{Operator, OperatorListResult};
use uxduck_contracts::dto::pairing::{BootstrapToken, PairingListResult};
use uxduck_contracts::dto::state::StateSubscribeResult;
use uxduck_contracts::dto::telemetry::{TelemetrySubscribeResult, VehicleTelemetry};
use uxduck_contracts::dto::thread::{AgentConfig, Folder, Item, Thread, ThreadBulkResult};
use uxduck_contracts::dto::vehicle::{Vehicle, VehicleListResult};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::{ClientNotification, ClientRequest, ServerNotification};

fn dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn fixture(name: &str) -> Value {
    let path = dir().join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Deserialises, serialises and compares. A `.minimal` case omits optional
/// fields, which come back filled with their defaults, so it is compared after
/// one more round trip instead.
fn round_trip<T: DeserializeOwned + Serialize>(name: &str, json: &Value) {
    let value: T = serde_json::from_value(json.clone()).unwrap_or_else(|e| panic!("{name}: {e}"));
    let back = serde_json::to_value(&value).unwrap_or_else(|e| panic!("{name}: {e}"));
    if name.contains(".minimal") {
        let again: T =
            serde_json::from_value(back.clone()).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            serde_json::to_value(&again).ok(),
            Some(back),
            "{name} does not survive a serde round trip"
        );
    } else {
        assert_eq!(&back, json, "{name} does not survive a serde round trip");
    }
}

#[allow(clippy::too_many_lines, reason = "one arm per result type")]
fn check(name: &str) {
    let json = fixture(name);
    let mut parts = name.split('.');
    let (method, part) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    );
    match (part, method) {
        ("request", _) => round_trip::<ClientRequest>(name, &json),
        ("notification", _) if json["method"] == "cancel" => {
            round_trip::<ClientNotification>(name, &json);
        }
        ("notification", _) => round_trip::<ServerNotification>(name, &json),
        (_, "error") => round_trip::<ApiError>(name, &json),
        ("result", "initialize") => round_trip::<InitializeResult>(name, &json),
        ("result", "pairing_list") => round_trip::<PairingListResult>(name, &json),
        ("result", "session_createBootstrap") => round_trip::<BootstrapToken>(name, &json),
        ("result", "folder_create" | "folder_update") => round_trip::<Folder>(name, &json),
        ("result", "thread_bulk") => round_trip::<ThreadBulkResult>(name, &json),
        ("result", "vehicle_list" | "vehicle_label") => {
            round_trip::<VehicleListResult>(name, &json);
        }
        (
            "result",
            "vehicle_get" | "vehicle_save" | "vehicle_size" | "vehicle_bookmarks"
            | "vehicle_dismiss",
        ) => {
            round_trip::<Vehicle>(name, &json);
        }
        ("result", "link_add" | "link_enable" | "link_disable" | "link_update") => {
            round_trip::<Link>(name, &json);
        }
        ("result", "serial_list") => round_trip::<SerialListResult>(name, &json),
        ("result", "link_list") => {
            round_trip::<uxduck_contracts::dto::link::LinkListResult>(name, &json);
        }
        ("result", "telemetry_window") => {
            round_trip::<uxduck_contracts::dto::telemetry_window::TelemetryWindowResult>(
                name, &json,
            );
        }
        ("result", "command_send") => {
            round_trip::<uxduck_contracts::dto::command::Receipt>(name, &json);
        }
        ("result", "command_list") => {
            round_trip::<uxduck_contracts::dto::command::CommandListResult>(name, &json);
        }
        ("result", "command_modes") => {
            round_trip::<uxduck_contracts::dto::command::ModeListResult>(name, &json);
        }
        ("result", "search_query") => {
            round_trip::<uxduck_contracts::dto::search::SearchResult>(name, &json);
        }
        ("result", "place_search") => {
            round_trip::<uxduck_contracts::dto::search::PlaceSearchResult>(name, &json);
        }
        ("result", "place_locate") => {
            round_trip::<uxduck_contracts::dto::search::PlaceLocateResult>(name, &json);
        }
        ("result", "place_save" | "place_update") => {
            round_trip::<uxduck_contracts::dto::search::SavedPlace>(name, &json);
        }
        (
            "result",
            "plan_get" | "plan_create" | "plan_update" | "plan_undo" | "plan_redo" | "plan_place"
            | "plan_delete" | "plan_restore",
        ) => {
            round_trip::<uxduck_contracts::dto::plan::Plan>(name, &json);
        }
        ("result", "plan_list") => {
            round_trip::<uxduck_contracts::dto::plan::PlanListResult>(name, &json);
        }
        ("result", "plan_import") => {
            round_trip::<uxduck_contracts::dto::plan::PlanImportResult>(name, &json);
        }
        ("result", "plan_check") => {
            round_trip::<uxduck_contracts::dto::plan::PlanCheckResult>(name, &json);
        }
        ("result", "mission_restore") => {
            round_trip::<uxduck_contracts::dto::mission::MissionRestoreResult>(name, &json);
        }
        ("result", "mission_history") => {
            round_trip::<uxduck_contracts::dto::mission::MissionHistoryResult>(name, &json);
        }
        ("result", "plan_examples") => {
            round_trip::<uxduck_contracts::dto::plan::PlanExamplesResult>(name, &json);
        }
        ("result", "plan_upload") => {
            round_trip::<uxduck_contracts::dto::plan::PlanUploadResult>(name, &json);
        }
        ("result", "plan_export") => {
            round_trip::<uxduck_contracts::dto::plan::PlanExportResult>(name, &json);
        }
        ("result", "plugin_share" | "plugin_shared") => {
            round_trip::<uxduck_contracts::dto::plugin::PluginShare>(name, &json);
        }
        ("result", "layout_open") => {
            round_trip::<uxduck_contracts::dto::operator::LayoutOpenResult>(name, &json);
        }
        ("result", "analytics_takeFlightTime") => {
            round_trip::<uxduck_contracts::dto::analytics::FlightTimeUsage>(name, &json);
        }
        ("result", "analytics_takeEngagement") => {
            round_trip::<uxduck_contracts::dto::analytics::EngagementUsage>(name, &json);
        }
        ("result", "vehicle_messages") => {
            round_trip::<uxduck_contracts::dto::messages::VehicleMessagesResult>(name, &json);
        }
        ("result", "update_status" | "update_check" | "update_install" | "update_later") => {
            round_trip::<uxduck_contracts::dto::update::AppUpdateStatus>(name, &json);
        }
        ("result", "tray_menu") => {
            round_trip::<uxduck_contracts::dto::tray::TrayMenu>(name, &json);
        }
        ("result", "daemon_stopPreview") => {
            round_trip::<uxduck_contracts::dto::app::StopPreview>(name, &json);
        }
        ("result", "app_open") => {
            round_trip::<uxduck_contracts::dto::app::AppOpenResult>(name, &json);
        }
        ("result", "desktopApp_status" | "desktopApp_choose" | "desktopApp_remove") => {
            round_trip::<uxduck_contracts::dto::app::DesktopAppStatus>(name, &json);
        }
        ("result", "app_focus") => {
            round_trip::<uxduck_contracts::dto::app::AppFocusResult>(name, &json);
        }
        ("result", "tray_activate") => {
            round_trip::<uxduck_contracts::dto::tray::TrayAction>(name, &json);
        }
        ("result", "feedback_screenshot") => {
            round_trip::<uxduck_contracts::dto::tray::FeedbackScreenshot>(name, &json);
        }
        ("result", "plugin_shares") => {
            round_trip::<uxduck_contracts::dto::plugin::PluginSharesResult>(name, &json);
        }
        (
            "result",
            "plugin_install" | "plugin_allow" | "plugin_installShared" | "plugin_setEnabled",
        ) => {
            round_trip::<uxduck_contracts::dto::plugin::Plugin>(name, &json);
        }
        ("result", "plugin_draft") => {
            round_trip::<uxduck_contracts::dto::plugin::PluginDraft>(name, &json);
        }
        ("result", "plugin_drafts") => {
            round_trip::<uxduck_contracts::dto::plugin::PluginDraftsResult>(name, &json);
        }
        ("result", "plugin_test") => {
            round_trip::<uxduck_contracts::dto::plugin::PluginTestResult>(name, &json);
        }
        ("result", "tool_search") => {
            round_trip::<uxduck_contracts::dto::plugin::ToolSearchResult>(name, &json);
        }
        ("result", "plugin_features") => {
            round_trip::<uxduck_contracts::dto::plugin::PluginFeaturesResult>(name, &json);
        }
        ("result", "plugin_usage") => {
            round_trip::<uxduck_contracts::dto::plugin::PluginUsageResult>(name, &json);
        }
        ("result", "plugin_items") => {
            round_trip::<uxduck_contracts::dto::plugin::PluginItemsResult>(name, &json);
        }
        ("result", "param_history") => {
            round_trip::<uxduck_contracts::dto::history::ParamHistoryResult>(name, &json);
        }
        ("result", "feedback_suggest") => {
            round_trip::<uxduck_contracts::dto::feedback::SuggestionCard>(name, &json);
        }
        ("result", "feedback_send") => {
            round_trip::<uxduck_contracts::dto::feedback::FeedbackSendResult>(name, &json);
        }
        ("bundle", "feedback") => {
            round_trip::<uxduck_contracts::dto::feedback::FeedbackBundle>(name, &json);
        }
        ("result", "settings_update") => {
            round_trip::<uxduck_contracts::dto::settings::Settings>(name, &json);
        }
        ("result", "daemon_wakeups") => {
            round_trip::<uxduck_contracts::dto::settings::Wakeups>(name, &json);
        }
        ("result", "location_status" | "location_start" | "location_stop") => {
            round_trip::<uxduck_contracts::dto::location::LocationStatus>(name, &json);
        }
        ("result", "interface_status" | "interface_check") => {
            round_trip::<uxduck_contracts::dto::interface::InterfaceStatus>(name, &json);
        }
        ("result", "daemon_about") => {
            round_trip::<uxduck_contracts::dto::settings::DaemonAbout>(name, &json);
        }
        ("result", "storage_usage" | "storage_clearSimulatorLogs") => {
            round_trip::<uxduck_contracts::dto::settings::StorageUsage>(name, &json);
        }
        (
            "result",
            "controller_claim"
            | "controller_setTarget"
            | "controller_takeOver"
            | "controller_keyboardStart",
        ) => {
            round_trip::<uxduck_contracts::dto::controller::Controller>(name, &json);
        }
        ("result", "controller_inputs") => {
            round_trip::<uxduck_contracts::dto::controller::ControllerInputsResult>(name, &json);
        }
        ("result", "mapping_update" | "mapping_create") => {
            round_trip::<uxduck_contracts::dto::controller::InputMapping>(name, &json);
        }
        ("result", "param_refresh") => {
            round_trip::<uxduck_contracts::dto::parameter::ParameterSet>(name, &json);
        }
        ("result", "param_set") => {
            round_trip::<uxduck_contracts::dto::parameter::ParamSetResult>(name, &json);
        }
        ("result", "param_definitions") => {
            round_trip::<uxduck_contracts::dto::param_definitions::ParamDefinitionsResult>(
                name, &json,
            );
        }
        ("result", "param_definitions_list") => {
            round_trip::<uxduck_contracts::dto::param_definitions::DefinitionsListResult>(
                name, &json,
            );
        }
        ("result", "vehicle_history") => {
            round_trip::<uxduck_contracts::dto::history::VehicleHistoryResult>(name, &json);
        }
        ("result", "vehicle_addNote" | "vehicle_editNote" | "vehicle_deleteNote") => {
            round_trip::<uxduck_contracts::dto::history::HistoryEntry>(name, &json);
        }
        ("result", "video_devices") => {
            round_trip::<uxduck_contracts::dto::video::VideoDevicesResult>(name, &json);
        }
        ("result", "video_list") => {
            round_trip::<uxduck_contracts::dto::video::VideoListResult>(name, &json);
        }
        (
            "result",
            "video_assign"
            | "video_rename"
            | "video_track"
            | "video_capture"
            | "video_capture_enable"
            | "video_vehicleCamera"
            | "video_recordWithOthers",
        ) => {
            round_trip::<uxduck_contracts::dto::video::VideoSource>(name, &json);
        }
        ("result", "find_start" | "find_vehicles") => {
            round_trip::<uxduck_contracts::dto::find::FindResult>(name, &json);
        }
        ("result", "video_photo") => {
            round_trip::<uxduck_contracts::dto::video::VideoPhoto>(name, &json);
        }
        ("result", "video_playback" | "video_photo_show") => {
            round_trip::<uxduck_contracts::dto::video::VideoPlaybackResult>(name, &json);
        }
        ("result", "video_reveal" | "storage_reveal") => {
            round_trip::<uxduck_contracts::dto::video::VideoRevealResult>(name, &json);
        }
        ("result", "video_add") => {
            round_trip::<uxduck_contracts::dto::video::VideoAddResult>(name, &json);
        }
        ("result", "video_watch") => {
            round_trip::<uxduck_contracts::dto::video::VideoWatchResult>(name, &json);
        }
        ("result", "video_metadata_watch") => {
            round_trip::<uxduck_contracts::dto::video::VideoMetadataWatchResult>(name, &json);
        }
        ("result", "param_search") => {
            round_trip::<uxduck_contracts::dto::param_definitions::ParamSearchResult>(name, &json);
        }
        ("result", "param_get") => {
            round_trip::<uxduck_contracts::dto::param_definitions::ParamGetResult>(name, &json);
        }
        ("result", "param_list") => {
            round_trip::<uxduck_contracts::dto::param_definitions::ParamListResult>(name, &json);
        }
        ("result", "param_diff") => {
            round_trip::<uxduck_contracts::dto::param_definitions::ParamDiffResult>(name, &json);
        }
        ("result", "mavlink_inspect") => {
            round_trip::<uxduck_contracts::dto::inspect::MavlinkInspectResult>(name, &json);
        }
        ("result", "router_events") => {
            round_trip::<uxduck_contracts::dto::diagnostics::RouterEventsResult>(name, &json);
        }
        ("result", "link_diagnose") => {
            round_trip::<uxduck_contracts::dto::diagnostics::LinkDiagnoseResult>(name, &json);
        }
        ("result", "socket_inspect") => {
            round_trip::<uxduck_contracts::dto::diagnostics::SocketInspectResult>(name, &json);
        }
        ("result", "daemon_logs") => {
            round_trip::<uxduck_contracts::dto::diagnostics::DaemonLogsResult>(name, &json);
        }
        ("result", "mavlink_history") => {
            round_trip::<uxduck_contracts::dto::inspect::MavlinkHistoryResult>(name, &json);
        }
        ("result", "param_definitions_add") => {
            round_trip::<uxduck_contracts::dto::param_definitions::OwnSet>(name, &json);
        }
        ("result", "mission_refresh") => {
            round_trip::<uxduck_contracts::dto::mission::Mission>(name, &json);
        }
        ("result", "mission_upload") => {
            round_trip::<uxduck_contracts::dto::mission::MissionUploadResult>(name, &json);
        }
        ("result", "client_displayList") => {
            round_trip::<uxduck_contracts::dto::client::DisplayClientsResult>(name, &json);
        }
        ("result", "client_displayPull") => {
            round_trip::<uxduck_contracts::dto::client::DisplaySnapshot>(name, &json);
        }
        ("result", "client_list") => {
            round_trip::<uxduck_contracts::dto::client::ClientListResult>(name, &json);
        }
        ("result", "network_status") => {
            round_trip::<uxduck_contracts::dto::network::NetworkStatus>(name, &json);
        }
        ("result", "client_setCapabilities") => {
            round_trip::<uxduck_contracts::dto::client::ClientRecord>(name, &json);
        }
        ("result", "approval_list") => {
            round_trip::<uxduck_contracts::dto::client::ApprovalListResult>(name, &json);
        }
        ("result", "approval_resolve") => {
            round_trip::<uxduck_contracts::dto::client::Approval>(name, &json);
        }
        ("result", "telemetry_subscribe") => round_trip::<TelemetrySubscribeResult>(name, &json),
        ("result", "telemetry_get") => round_trip::<VehicleTelemetry>(name, &json),
        ("result", "thread_create" | "thread_update") => round_trip::<Thread>(name, &json),
        ("result", "thread_send") => round_trip::<Item>(name, &json),
        ("result", "agent_config") => round_trip::<AgentConfig>(name, &json),
        ("result", "sim_binaries") => {
            round_trip::<uxduck_contracts::dto::sim::SimBinariesResult>(name, &json);
        }
        ("result", "sim_start" | "sim_restart" | "snapshot_start") => {
            round_trip::<uxduck_contracts::dto::link::Link>(name, &json);
        }
        ("result", "sim_snapshot" | "snapshot_rename") => {
            round_trip::<uxduck_contracts::dto::sim::SimSnapshot>(name, &json);
        }
        ("result", "snapshot_list") => {
            round_trip::<uxduck_contracts::dto::sim::SnapshotListResult>(name, &json);
        }
        (
            "result",
            "account_get"
            | "account_verify"
            | "account_setUpdates"
            | "account_setUsername"
            | "account_signOutSession"
            | "account_signOutOthers",
        ) => {
            round_trip::<uxduck_contracts::dto::account::AccountState>(name, &json);
        }
        ("result", "account_sessions") => {
            round_trip::<uxduck_contracts::dto::account::AccountSessions>(name, &json);
        }
        ("result", "account_flights") => {
            round_trip::<uxduck_contracts::dto::account::AccountFlights>(name, &json);
        }
        ("result", "account_environment" | "account_setEnvironment") => {
            round_trip::<uxduck_contracts::dto::account::AccountEnvironmentState>(name, &json);
        }
        ("result", "thread_feedback") => {
            round_trip::<uxduck_contracts::dto::thread::ThreadFeedbackResult>(name, &json);
        }
        ("result", "lease_take") => {
            round_trip::<uxduck_contracts::dto::edge::Lease>(name, &json);
        }
        ("result", "files_list") => {
            round_trip::<uxduck_contracts::dto::files::FilesListResult>(name, &json);
        }
        ("result", "files_read") => {
            round_trip::<uxduck_contracts::dto::files::FilesReadResult>(name, &json);
        }
        ("result", "files_download" | "files_upload" | "files_pause" | "files_resume") => {
            round_trip::<uxduck_contracts::dto::files::FileTransfer>(name, &json);
        }
        ("result", "files_save") => {
            round_trip::<uxduck_contracts::dto::files::FileSaveResult>(name, &json);
        }
        ("result", "practice_list") => {
            round_trip::<uxduck_contracts::dto::practice::PracticeListResult>(name, &json);
        }
        ("result", "practice_start" | "practice_stop" | "practice_guide") => {
            round_trip::<uxduck_contracts::dto::practice::PracticeAttempt>(name, &json);
        }
        ("result", "practice_answer") => {
            round_trip::<uxduck_contracts::dto::practice::PracticeAnswerResult>(name, &json);
        }
        ("result", "terrain_at") => {
            round_trip::<uxduck_contracts::dto::terrain::TerrainAtResult>(name, &json);
        }
        ("result", "terrain_check") => {
            round_trip::<uxduck_contracts::dto::terrain::TerrainCheckResult>(name, &json);
        }
        ("result", "terrain_status") => {
            round_trip::<uxduck_contracts::dto::terrain::TerrainStatusResult>(name, &json);
        }
        ("result", "terrain_areas") => {
            round_trip::<uxduck_contracts::dto::terrain::TerrainAreasResult>(name, &json);
        }
        ("result", "terrain_areaSize") => {
            round_trip::<uxduck_contracts::dto::terrain::TerrainAreaSize>(name, &json);
        }
        ("result", "terrain_keepArea") => {
            round_trip::<uxduck_contracts::dto::terrain::TerrainArea>(name, &json);
        }
        ("result", "practice_status") => {
            round_trip::<uxduck_contracts::dto::practice::PracticeStatusResult>(name, &json);
        }
        ("result", "watchLink_create") => {
            round_trip::<uxduck_contracts::dto::edge::WatchLinkCreateResult>(name, &json);
        }
        ("result", "watchLink_viewers") => {
            round_trip::<uxduck_contracts::dto::edge::WatchViewersResult>(name, &json);
        }
        ("result", "watchLink_list") => {
            round_trip::<uxduck_contracts::dto::edge::WatchLinkListResult>(name, &json);
        }
        ("result", "account_computers") => {
            round_trip::<uxduck_contracts::dto::edge::AccountComputers>(name, &json);
        }
        ("result", "account_shared") => {
            round_trip::<uxduck_contracts::dto::account::SharedData>(name, &json);
        }
        ("result", "account_deleteShared") => {
            round_trip::<uxduck_contracts::dto::account::SharedDeleted>(name, &json);
        }
        ("result", "sync_get" | "sync_set" | "sync_activate") => {
            round_trip::<uxduck_contracts::dto::sync::SyncStatus>(name, &json);
        }
        ("result", "sim_catalogue") => {
            round_trip::<uxduck_contracts::dto::sim::SimCatalogueResult>(name, &json);
        }
        ("result", "journal_import") => {
            round_trip::<uxduck_contracts::dto::journal::JournalImportResult>(name, &json);
        }
        ("result", "state_subscribe") => round_trip::<StateSubscribeResult>(name, &json),
        ("result", "operator_list") => round_trip::<OperatorListResult>(name, &json),
        ("result", "operator_create" | "operator_update") => round_trip::<Operator>(name, &json),
        (
            "result",
            "trace_subscribe"
            | "client_displayRegister"
            | "client_displayReply"
            | "pairing_approve"
            | "pairing_deny"
            | "operator_remove"
            | "client_setOperator"
            | "thread_cancel"
            | "credentials_set"
            | "thread_delete"
            | "folder_delete"
            | "link_remove"
            | "controller_release"
            | "controller_keyboardInput"
            | "controller_keyboardStop"
            | "controller_forget"
            | "daemon_restart"
            | "daemon_stop"
            | "client_visibility"
            | "desktopApp_download"
            | "place_remove"
            | "transfer_cancel"
            | "plugin_remove"
            | "plugin_command"
            | "client_select"
            | "client_forget"
            | "notification_acknowledge"
            | "notification_dismiss"
            | "analytics_track"
            | "analytics_feedback"
            | "analytics_activeTime"
            | "analytics_performance"
            | "analytics_alias"
            | "debug_panic"
            | "sim_stop"
            | "snapshot_delete"
            | "param_definitions_remove"
            | "sim_download"
            | "sim_delete"
            | "account_signIn"
            | "account_signOut"
            | "vehicle_delete"
            | "video_unwatch"
            | "video_remove"
            | "video_record"
            | "video_recordAll"
            | "find_stop"
            | "plugin_unshare"
            | "watchLink_revoke"
            | "lease_release"
            | "remote_watch"
            | "account_requestDeletion"
            | "files_remove"
            | "files_mkdir"
            | "files_rename"
            | "files_cancel"
            | "terrain_removeArea"
            | "tray_location",
        ) => {
            round_trip::<Empty>(name, &json);
        }
        _ => panic!("{name}: no DTO for this fixture; add it to check()"),
    }
}

#[test]
fn every_fixture_round_trips() {
    let mut names: Vec<String> = std::fs::read_dir(dir())
        .expect("fixtures directory")
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| Path::new(name).extension().is_some_and(|ext| ext == "json"))
        .collect();
    names.sort();
    assert!(!names.is_empty());
    for name in &names {
        check(name);
    }
}

#[test]
fn every_method_has_a_request_and_a_result_fixture() {
    for method in ClientRequest::METHODS {
        let stem = method.replace('/', "_");
        for part in ["request", "result"] {
            let name = format!("{stem}.{part}.json");
            assert!(dir().join(&name).exists(), "missing fixture {name}");
        }
    }
}

#[test]
fn initialize_request_minimal_defaults_optional_capabilities() {
    let json = fixture("initialize.request.minimal.json");
    let ClientRequest::Initialize(params) = serde_json::from_value(json).expect("valid fixture")
    else {
        panic!("not initialize");
    };
    assert_eq!(params.client_info.kind, ClientKind::Agent);
    assert!(!params.capabilities.binary_channels);
    assert!(!params.capabilities.approvals);
    assert!(params.capabilities.opt_out_notifications.is_empty());
}

#[test]
fn initialize_result_grants_in_order() {
    let result: InitializeResult =
        serde_json::from_value(fixture("initialize.result.json")).expect("valid fixture");
    assert_eq!(
        result.granted,
        [
            Capability::Read,
            Capability::Control,
            Capability::Config,
            Capability::Approve,
            Capability::Location
        ]
    );
}

#[test]
fn unknown_method_is_rejected() {
    let json = serde_json::json!({ "method": "nope", "params": {} });
    assert!(serde_json::from_value::<ClientRequest>(json).is_err());
}

#[test]
fn methods_match_the_enum() {
    for method in ClientRequest::METHODS {
        let stem = method.replace('/', "_");
        let params = if dir().join(format!("{stem}.request.json")).exists() {
            fixture(&format!("{stem}.request.json"))["params"].clone()
        } else if method.starts_with("pairing/a") || method == "pairing/deny" {
            serde_json::json!({ "code": "123456" })
        } else if method == "initialize" {
            fixture("initialize.request.json")["params"].clone()
        } else {
            serde_json::json!({})
        };
        let parsed = serde_json::from_value::<ClientRequest>(
            serde_json::json!({ "method": method, "params": params }),
        );
        assert!(
            parsed.is_ok(),
            "{method} is listed in METHODS but not in the enum"
        );
    }
}

#[test]
fn update_required_error_fixture_carries_the_minimum() {
    let error: ApiError =
        serde_json::from_value(fixture("error.update-required.json")).expect("valid fixture");
    assert_eq!(error.kind(), ErrorKind::UpdateRequired);
    assert_eq!(error.code, ErrorKind::UpdateRequired.code());
}

/// Every telemetry item in the fixtures says its unit in its key (ADR-0006).
#[test]
fn telemetry_fixtures_name_their_units() {
    for entry in std::fs::read_dir(dir()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !name.starts_with("telemetry_") {
            continue;
        }
        let json = fixture(&name);
        let mut items = Vec::new();
        collect_items(&json, &mut items);
        for item in items {
            let unit: uxduck_core::domain::telemetry::Unit =
                serde_json::from_value(item["unit"].clone()).unwrap();
            let key = item["key"].as_str().unwrap();
            assert!(
                uxduck_core::domain::telemetry::key_says_unit(key, unit),
                "{name}: {key} does not say {unit:?}"
            );
        }
    }
}

fn collect_items(value: &Value, out: &mut Vec<Value>) {
    match value {
        Value::Object(map) => {
            if map.contains_key("key") && map.contains_key("unit") {
                out.push(value.clone());
            }
            for v in map.values() {
                collect_items(v, out);
            }
        }
        Value::Array(list) => list.iter().for_each(|v| collect_items(v, out)),
        _ => {}
    }
}
