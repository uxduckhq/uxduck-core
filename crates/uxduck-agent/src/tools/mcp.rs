//! MCP metadata for the existing contract methods. This does not grant access:
//! the daemon remains responsible for capabilities, approvals and validation.

use schemars::{JsonSchema, generate::SchemaSettings};
use serde_json::{Value, json};
use uxduck_contracts::dto;

/// Observational methods only. Some methods with the `read` capability still
/// change a window or replay; never infer this hint from that capability.
#[must_use]
pub fn annotations(method: &str) -> Value {
    let read_only = matches!(
        method,
        "vehicle/list"
            | "vehicle/get"
            | "telemetry/get"
            | "daemon/about"
            | "mavlink/inspect"
            | "mavlink/history"
            | "mavlink/dialects/list"
            | "firmware/list"
            | "router/events"
            | "link/diagnose"
            | "socket/inspect"
            | "daemon/logs"
            | "telemetry/window"
            | "link/list"
            | "command/list"
            | "search/query"
            | "place/search"
            | "place/locate"
            | "marker/list"
            | "location/status"
            | "traffic/status"
            | "plan/list"
            | "plan/get"
            | "plan/check"
            | "plan/examples"
            | "plan/export"
            | "plan/simulateCheck"
            | "mission/history"
            | "mission/get"
            | "sim/binaries"
            | "sim/catalogue"
            | "sim/inspect"
            | "sim/inspectModel"
            | "sim/ownBuild"
            | "sim/faults"
            | "practice/list"
            | "practice/status"
            | "snapshot/list"
            | "param/search"
            | "param/get"
            | "param/list"
            | "param/diff"
            | "param/changes"
            | "param/sets"
            | "param/setExport"
            | "tuning/state"
            | "vehicle/history"
            | "flight/list"
            | "replay/state"
            | "flight/series"
            | "replay/logFields"
            | "attachment/read"
            | "attachment/image"
            | "operation/list"
            | "logs/cards"
            | "flights/found"
            | "gripper/status"
            | "param/history"
            | "vehicle/messages"
            | "terrain/at"
            | "terrain/check"
            | "terrain/status"
            | "terrain/vehicle"
            | "terrain/areas"
            | "terrain/areaSize"
            | "files/list"
            | "files/read"
            | "setup/state"
            | "preflight/state"
            | "checklist/export"
            | "checklist/runs"
            | "video/list"
            | "video/frame"
            | "camera/list"
            | "camera/images"
            | "camera/settings"
            | "gimbal/list"
            | "video/devices"
            | "tool/search"
            | "plugin/drafts"
            | "settings/get"
            | "operator/list"
            | "telemetry/calculate"
    );
    json!({ "readOnlyHint": read_only, "destructiveHint": !read_only,
        "idempotentHint": read_only, "openWorldHint": true })
}

fn schema<T: JsonSchema>() -> Value {
    let mut value = SchemaSettings::draft2020_12()
        .for_serialize()
        .into_generator()
        .into_root_schema_for::<T>()
        .to_value();
    if let Some(object) = value.as_object_mut() {
        object.remove("$schema");
    }
    value
}

/// Results come from their Rust DTOs, including serializing/skipped fields.
/// Keep this association beside the agent allowlist; the coverage test below
/// requires an explicit result type for each newly exposed method.
#[must_use]
#[allow(clippy::too_many_lines, reason = "one association per RPC result type")]
pub fn output_schema(method: &str) -> Option<Value> {
    let mut result = match method {
        "marker/remove" | "sim/download" | "sim/stop" | "files/remove" | "video/record"
        | "video/recordAll" | "plugin/command" => schema::<dto::Empty>(),
        "camera/list" => schema::<dto::camera::CameraListResult>(),
        "camera/images" => schema::<dto::camera::CameraImagesResult>(),
        "camera/settings" => schema::<dto::camera::CameraSettingsResult>(),
        "checklist/export" => schema::<dto::checklist::ChecklistFile>(),
        "checklist/runs" => schema::<dto::checklist::ChecklistRunsResult>(),
        "command/list" => schema::<dto::command::CommandListResult>(),
        "command/send" | "camera/command" | "gimbal/command" => schema::<dto::command::Receipt>(),
        "daemon/logs" => schema::<dto::diagnostics::DaemonLogsResult>(),
        "link/diagnose" => schema::<dto::diagnostics::LinkDiagnoseResult>(),
        "router/events" => schema::<dto::diagnostics::RouterEventsResult>(),
        "socket/inspect" => schema::<dto::diagnostics::SocketInspectResult>(),
        "mavlink/dialects/list" | "mavlink/dialects/use" => {
            schema::<dto::dialect::DialectsListResult>()
        }
        "feedback/file" => schema::<dto::feedback::FeedbackFiled>(),
        "feedback/add" => schema::<dto::feedback::FeedbackSendResult>(),
        "feedback/suggest" => schema::<dto::feedback::SuggestionCard>(),
        "files/upload" => schema::<dto::files::FileTransfer>(),
        "files/list" => schema::<dto::files::FilesListResult>(),
        "files/read" => schema::<dto::files::FilesReadResult>(),
        "find/vehicles" => schema::<dto::find::FindResult>(),
        "firmware/list" => schema::<dto::firmware_profile::FirmwareListResult>(),
        "flight/rename" => schema::<dto::flight::Flight>(),
        "flight/list" => schema::<dto::flight::FlightListResult>(),
        "gimbal/list" => schema::<dto::gimbal::GimbalListResult>(),
        "gripper/status" => schema::<dto::gripper::GripperStatus>(),
        "vehicle/addNote" => schema::<dto::history::HistoryEntry>(),
        "param/history" => schema::<dto::history::ParamHistoryResult>(),
        "vehicle/history" => schema::<dto::history::VehicleHistoryResult>(),
        "mavlink/history" => schema::<dto::inspect::MavlinkHistoryResult>(),
        "mavlink/inspect" => schema::<dto::inspect::MavlinkInspectResult>(),
        "sim/start" | "sim/restart" | "sim/pause" | "sim/fault" | "snapshot/start" => {
            schema::<dto::link::Link>()
        }
        "link/list" => schema::<dto::link::LinkListResult>(),
        "location/status" => schema::<dto::location::LocationStatus>(),
        "marker/add" | "marker/update" => schema::<dto::marker::Marker>(),
        "marker/list" => schema::<dto::marker::MarkerListResult>(),
        "vehicle/messages" => schema::<dto::messages::VehicleMessagesResult>(),
        "mission/get" => schema::<dto::mission::MissionGetResult>(),
        "mission/refresh" => schema::<dto::mission::Mission>(),
        "mission/history" => schema::<dto::mission::MissionHistoryResult>(),
        "mission/restore" => schema::<dto::mission::MissionRestoreResult>(),
        "operation/create" | "operation/rename" | "operation/addFlights" => {
            schema::<dto::operation::Operation>()
        }
        "operation/list" | "operation/removeFlights" => {
            schema::<dto::operation::OperationListResult>()
        }
        "plan/simulateCheck" => schema::<dto::operation::PlanSimulateCheckResult>(),
        "plan/simulate" => schema::<dto::operation::PlanSimulateResult>(),
        "layout/open" => schema::<dto::operator::LayoutOpenResult>(),
        "operator/update" => schema::<dto::operator::Operator>(),
        "operator/list" => schema::<dto::operator::OperatorListResult>(),
        "param/changes" => schema::<dto::param_definitions::ParamChangesResult>(),
        "param/diff" => schema::<dto::param_definitions::ParamDiffResult>(),
        "param/get" => schema::<dto::param_definitions::ParamGetResult>(),
        "param/list" => schema::<dto::param_definitions::ParamListResult>(),
        "param/search" => schema::<dto::param_definitions::ParamSearchResult>(),
        "param/setApply" => schema::<dto::param_definitions::ParamSetApplyResult>(),
        "param/setExport" => schema::<dto::param_definitions::ParamSetExportResult>(),
        "param/sets" => schema::<dto::param_definitions::ParamSetsResult>(),
        "param/definitions/use" => schema::<dto::param_definitions::VehicleDefinitions>(),
        "param/set" => schema::<dto::parameter::ParamSetResult>(),
        "param/refresh" => schema::<dto::parameter::ParameterSet>(),
        "plan/get" | "plan/create" | "plan/update" => schema::<dto::plan::Plan>(),
        "plan/check" => schema::<dto::plan::PlanCheckResult>(),
        "plan/examples" => schema::<dto::plan::PlanExamplesResult>(),
        "plan/export" => schema::<dto::plan::PlanExportResult>(),
        "plan/import" => schema::<dto::plan::PlanImportResult>(),
        "plan/list" => schema::<dto::plan::PlanListResult>(),
        "plan/upload" => schema::<dto::plan::PlanUploadResult>(),
        "plugin/install" => schema::<dto::plugin::Plugin>(),
        "plugin/draft" => schema::<dto::plugin::PluginDraft>(),
        "plugin/drafts" => schema::<dto::plugin::PluginDraftsResult>(),
        "plugin/test" => schema::<dto::plugin::PluginTestResult>(),
        "tool/search" => schema::<dto::plugin::ToolSearchResult>(),
        "practice/start" | "practice/guide" => schema::<dto::practice::PracticeAttempt>(),
        "practice/list" => schema::<dto::practice::PracticeListResult>(),
        "practice/status" => schema::<dto::practice::PracticeStatusResult>(),
        "preflight/state" | "checklist/update" | "checklist/copy" => {
            schema::<dto::preflight::Preflight>()
        }
        "flight/series" => schema::<dto::replay::FlightSeriesResult>(),
        "flights/found" => schema::<dto::replay::FoundFlightsResult>(),
        "logs/cards" => schema::<dto::replay::LogCardsResult>(),
        "replay/logFields" => schema::<dto::replay::ReplayLogFieldsResult>(),
        "replay/open" | "replay/seek" | "replay/state" | "replay/close" | "replay/add" => {
            schema::<dto::replay::ReplayState>()
        }
        "place/area" => schema::<dto::search::PlaceAreaResult>(),
        "place/locate" => schema::<dto::search::PlaceLocateResult>(),
        "place/search" => schema::<dto::search::PlaceSearchResult>(),
        "search/query" => schema::<dto::search::SearchResult>(),
        "daemon/about" => schema::<dto::settings::DaemonAbout>(),
        "settings/get" | "settings/update" => schema::<dto::settings::Settings>(),
        "setup/state" => schema::<dto::setup::SetupStateResult>(),
        "sim/binaries" => schema::<dto::sim::SimBinariesResult>(),
        "sim/catalogue" => schema::<dto::sim::SimCatalogueResult>(),
        "sim/faults" => schema::<dto::sim::SimFaultsResult>(),
        "sim/inspect" => schema::<dto::sim::SimInspectResult>(),
        "sim/inspectModel" => schema::<dto::sim::SimInspectModelResult>(),
        "sim/ownBuild" => schema::<dto::sim::SimOwnBuildResult>(),
        "sim/frames" => schema::<dto::sim::SimFramesResult>(),
        "sim/snapshot" => schema::<dto::sim::SimSnapshot>(),
        "snapshot/list" => schema::<dto::sim::SnapshotListResult>(),
        "telemetry/calculate" => schema::<dto::telemetry::TelemetryCalculateResult>(),
        "telemetry/get" => schema::<dto::telemetry::VehicleTelemetry>(),
        "telemetry/window" => schema::<dto::telemetry_window::TelemetryWindowResult>(),
        "computer/run" => schema::<dto::terminal::ComputerRunResult>(),
        "terrain/keepArea" => schema::<dto::terrain::TerrainArea>(),
        "terrain/areaSize" => schema::<dto::terrain::TerrainAreaSize>(),
        "terrain/areas" => schema::<dto::terrain::TerrainAreasResult>(),
        "terrain/at" => schema::<dto::terrain::TerrainAtResult>(),
        "terrain/check" => schema::<dto::terrain::TerrainCheckResult>(),
        "terrain/status" => schema::<dto::terrain::TerrainStatusResult>(),
        "terrain/vehicle" | "terrain/putArea" => schema::<dto::terrain::VehicleTerrainResult>(),
        "attachment/image" => schema::<dto::thread::AttachmentImageResult>(),
        "attachment/read" => schema::<dto::thread::AttachmentReadResult>(),
        "traffic/status" => schema::<dto::traffic::TrafficStatus>(),
        "tuning/prepare" => schema::<dto::tuning::TuningPrepare>(),
        "tuning/state" => schema::<dto::tuning::TuningState>(),
        "vehicle/get" | "vehicle/size" | "vehicle/look" | "vehicle/colour" | "vehicle/gripper" => {
            schema::<dto::vehicle::Vehicle>()
        }
        "vehicle/list" | "vehicle/label" => schema::<dto::vehicle::VehicleListResult>(),
        "video/add" => schema::<dto::video::VideoAddResult>(),
        "video/devices" => schema::<dto::video::VideoDevicesResult>(),
        "video/frame" => schema::<dto::video::VideoFrameResult>(),
        "video/list" => schema::<dto::video::VideoListResult>(),
        "flight/addVideo" => schema::<dto::video::VideoRecording>(),
        "video/capture" | "video/capture/enable" | "video/assign" | "video/rename" => {
            schema::<dto::video::VideoSource>()
        }
        _ => return None,
    };
    if matches!(method, "video/frame" | "attachment/image") {
        // take_images moves JPEG bytes into MCP image content and adds `shown`.
        image_metadata(&mut result);
        result["properties"]["shown"] = json!({ "type": "string" });
    }
    Some(result)
}

fn image_metadata(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if let Some(properties) = object.get_mut("properties").and_then(Value::as_object_mut) {
                properties.remove("jpegBase64");
            }
            if let Some(required) = object.get_mut("required").and_then(Value::as_array_mut) {
                required.retain(|field| field != "jpegBase64");
            }
            for child in object.values_mut() {
                image_metadata(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                image_metadata(item);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_advertised_method_has_a_contract_result_schema() {
        for tool in crate::tools::tools() {
            let schema = output_schema(&tool.method).expect(&tool.method);
            assert_eq!(schema["type"], "object", "{}", tool.name);
            if let Some(result) = tool.description.split("Result: `").nth(1) {
                assert_eq!(
                    schema["title"],
                    result.split('`').next().unwrap(),
                    "{}",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn capability_is_not_a_read_only_annotation() {
        for method in [
            "replay/open",
            "replay/seek",
            "replay/close",
            "layout/open",
            "sim/frames",
            "feedback/file",
            "feedback/add",
            "command/send",
            "param/set",
            "computer/run",
            "unknown",
        ] {
            assert_eq!(annotations(method)["readOnlyHint"], false, "{method}");
        }
        for method in [
            "telemetry/get",
            "telemetry/window",
            "vehicle/history",
            "param/get",
        ] {
            assert_eq!(annotations(method)["readOnlyHint"], true, "{method}");
            assert_eq!(annotations(method)["destructiveHint"], false, "{method}");
        }
    }

    #[test]
    fn image_schemas_describe_metadata_not_extracted_bytes() {
        for method in ["video/frame", "attachment/image"] {
            let schema = output_schema(method).unwrap();
            assert!(!schema.to_string().contains("jpegBase64"));
            assert_eq!(schema["properties"]["shown"]["type"], "string");
        }
    }
}
