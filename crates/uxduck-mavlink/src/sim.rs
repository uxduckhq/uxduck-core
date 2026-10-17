//! A minimal vehicle for tests and `uxduck dev fake-vehicle`: it sends
//! heartbeats, answers the request for `AUTOPILOT_VERSION` like an autopilot,
//! and flies a circle so there is telemetry to show. It is not a flight
//! simulator (SITL is, S17).

use mavio::dialects::ardupilotmega::enums::GpsFixType;
use mavio::dialects::ardupilotmega::enums::{
    EkfStatusFlags, MavAutopilot, MavCmd, MavFrame, MavModeFlag, MavSensorOrientation, MavState,
    MavSysStatusSensor, MavType, PositionTargetTypemask, VideoStreamType,
};
use mavio::dialects::ardupilotmega::messages::{
    Attitude, AutopilotVersion, AvailableModes, AvailableModesMonitor, BatteryStatus, CommandLong,
    DistanceSensor, EkfStatusReport, GlobalPositionInt, GpsRawInt, Heartbeat, MissionCurrent,
    NavControllerOutput, PositionTargetGlobalInt, RadioStatus, ServoOutputRaw, SysStatus,
    TerrainReport, VfrHud, Vibration, VideoStreamInformation, Wind,
};
use mavio::protocol::{V2, Versionless};
use mavio::{Frame, Message};

use crate::decode::{Message as Decoded, decode};
use crate::modes::AvailableMode;

fn frame(system_id: u8, component_id: u8, sequence: u8, message: &dyn Message) -> Vec<u8> {
    let Ok(builder) = Frame::builder()
        .version(V2)
        .system_id(system_id)
        .component_id(component_id)
        .sequence(sequence)
        .message(message)
    else {
        return Vec::new();
    };
    let frame: Frame<Versionless> = builder.build().into_versionless();
    let mut buf = vec![0u8; frame.size()];
    let written = frame.serialize(&mut buf).unwrap_or(0);
    buf.truncate(written);
    buf
}

/// A quadcopter heartbeat from an ArduPilot autopilot.
#[must_use]
pub fn heartbeat_frame(system_id: u8, component_id: u8, sequence: u8) -> Vec<u8> {
    armed_heartbeat_frame(system_id, component_id, sequence, false, 0)
}

/// A copter heartbeat with its armed flag and mode (`custom_mode`).
#[must_use]
pub fn armed_heartbeat_frame(
    system_id: u8,
    component_id: u8,
    sequence: u8,
    armed: bool,
    custom_mode: u32,
) -> Vec<u8> {
    let mut base_mode = MavModeFlag::CUSTOM_MODE_ENABLED;
    if armed {
        base_mode |= MavModeFlag::SAFETY_ARMED;
    }
    let heartbeat = Heartbeat {
        type_: MavType::Quadrotor,
        autopilot: MavAutopilot::Ardupilotmega,
        base_mode,
        custom_mode,
        system_status: MavState::Standby,
        mavlink_version: 3,
    };
    frame(system_id, component_id, sequence, &heartbeat)
}

/// A PX4 quadcopter's heartbeat in a mode (`custom_mode`), disarmed.
#[must_use]
pub fn px4_heartbeat_frame(system_id: u8, sequence: u8, custom_mode: u32) -> Vec<u8> {
    let heartbeat = Heartbeat {
        type_: MavType::Quadrotor,
        autopilot: MavAutopilot::Px4,
        base_mode: MavModeFlag::CUSTOM_MODE_ENABLED,
        custom_mode,
        system_status: MavState::Standby,
        mavlink_version: 3,
    };
    frame(system_id, 1, sequence, &heartbeat)
}

/// `GLOBAL_POSITION_INT` at an altitude above mean sea level and above
/// home, in millimetres.
#[must_use]
pub fn global_position_frame(
    system_id: u8,
    sequence: u8,
    alt_mm: i32,
    relative_alt_mm: i32,
) -> Vec<u8> {
    let position = GlobalPositionInt {
        time_boot_ms: 1000,
        alt: alt_mm,
        relative_alt: relative_alt_mm,
        hdg: u16::MAX,
        ..GlobalPositionInt::default()
    };
    frame(system_id, 1, sequence, &position)
}

/// A ground station's heartbeat, which must never become a vehicle.
#[must_use]
pub fn gcs_heartbeat_frame(system_id: u8, component_id: u8, sequence: u8) -> Vec<u8> {
    let heartbeat = Heartbeat {
        type_: MavType::Gcs,
        autopilot: MavAutopilot::Invalid,
        base_mode: MavModeFlag::empty(),
        custom_mode: 0,
        system_status: MavState::Active,
        mavlink_version: 3,
    };
    frame(system_id, component_id, sequence, &heartbeat)
}

#[must_use]
pub fn autopilot_version_frame(
    system_id: u8,
    component_id: u8,
    sequence: u8,
    uid: u64,
    flight_sw_version: u32,
) -> Vec<u8> {
    autopilot_version_frame_with_commit(
        system_id,
        component_id,
        sequence,
        uid,
        flight_sw_version,
        [0; 8],
    )
}

/// `AUTOPILOT_VERSION` with `flight_custom_version`, where ArduPilot puts
/// the first 8 characters of its git hash.
#[must_use]
pub fn autopilot_version_frame_with_commit(
    system_id: u8,
    component_id: u8,
    sequence: u8,
    uid: u64,
    flight_sw_version: u32,
    flight_custom_version: [u8; 8],
) -> Vec<u8> {
    let version = AutopilotVersion {
        uid,
        flight_sw_version,
        flight_custom_version,
        ..AutopilotVersion::default()
    };
    frame(system_id, component_id, sequence, &version)
}

/// `MAV_CMD_REQUEST_MESSAGE` for `AUTOPILOT_VERSION` (message 148).
#[must_use]
pub fn request_autopilot_version_frame(
    gcs: u8,
    target_system: u8,
    target_component: u8,
    sequence: u8,
) -> Vec<u8> {
    let command = CommandLong {
        target_system,
        target_component,
        command: MavCmd::RequestMessage,
        param1: 148.0,
        ..CommandLong::default()
    };
    frame(gcs, crate::GCS_COMPONENT_ID, sequence, &command)
}

/// `MAV_CMD_REQUEST_MESSAGE` for a message id, from the daemon's GCS
/// address; `param2` is the message's own parameter, such as a stream id
/// (0 for all).
#[must_use]
pub fn request_message_frame(
    gcs: u8,
    target_system: u8,
    target_component: u8,
    sequence: u8,
    message_id: u32,
    param2: f32,
) -> Vec<u8> {
    #[allow(clippy::cast_precision_loss, reason = "message ids are small")]
    let command = CommandLong {
        target_system,
        target_component,
        command: MavCmd::RequestMessage,
        param1: message_id as f32,
        param2,
        ..CommandLong::default()
    };
    frame(gcs, crate::GCS_COMPONENT_ID, sequence, &command)
}

/// One mode of a vehicle's own list (`AVAILABLE_MODES`): `mode_index` from
/// 1 to `number_modes`.
#[must_use]
pub fn available_modes_frame(
    system_id: u8,
    component_id: u8,
    sequence: u8,
    number_modes: u8,
    mode_index: u8,
    mode: &AvailableMode,
) -> Vec<u8> {
    use mavio::dialects::ardupilotmega::enums::{MavModeProperty, MavStandardMode};
    let mut mode_name = [0u8; 35];
    let name = mode.name.as_bytes();
    let n = name.len().min(34);
    mode_name[..n].copy_from_slice(&name[..n]);
    let properties = u8::from(mode.advanced) | (u8::from(!mode.selectable) << 1);
    let message = AvailableModes {
        number_modes,
        mode_index,
        standard_mode: MavStandardMode::NonStandard,
        custom_mode: mode.number,
        properties: MavModeProperty::from_bits_retain(properties),
        mode_name,
    };
    frame(system_id, component_id, sequence, &message)
}

/// The vehicle's list of modes changed (`AVAILABLE_MODES_MONITOR`).
#[must_use]
pub fn available_modes_monitor_frame(
    system_id: u8,
    component_id: u8,
    sequence: u8,
    seq: u8,
) -> Vec<u8> {
    frame(
        system_id,
        component_id,
        sequence,
        &AvailableModesMonitor { seq },
    )
}

/// A camera component's heartbeat (MAVLink camera protocol).
#[must_use]
pub fn camera_heartbeat_frame(system_id: u8, component_id: u8, sequence: u8) -> Vec<u8> {
    let heartbeat = Heartbeat {
        type_: MavType::Camera,
        autopilot: MavAutopilot::Invalid,
        base_mode: MavModeFlag::empty(),
        custom_mode: 0,
        system_status: MavState::Active,
        mavlink_version: 3,
    };
    frame(system_id, component_id, sequence, &heartbeat)
}

/// `VIDEO_STREAM_INFORMATION` for one stream at `uri`: RTSP for an
/// `rtsp://` address, MPEG-TS over UDP otherwise.
#[must_use]
pub fn video_stream_information_frame(
    system_id: u8,
    component_id: u8,
    sequence: u8,
    stream_id: u8,
    uri: &str,
) -> Vec<u8> {
    let mut uri_bytes = [0u8; 160];
    for (to, from) in uri_bytes.iter_mut().zip(uri.bytes()) {
        *to = from;
    }
    let mut name = [0u8; 32];
    for (to, from) in name.iter_mut().zip(b"Main".iter()) {
        *to = *from;
    }
    let message = VideoStreamInformation {
        stream_id,
        count: 1,
        type_: if uri.starts_with("rtsp://") {
            VideoStreamType::Rtsp
        } else {
            VideoStreamType::MpegTs
        },
        framerate: 30.0,
        resolution_h: 1280,
        resolution_v: 720,
        uri: uri_bytes,
        name,
        ..VideoStreamInformation::default()
    };
    frame(system_id, component_id, sequence, &message)
}

/// A `COMMAND_LONG` from the daemon's GCS address. `confirmation` counts
/// retransmissions (the MAVLink command protocol).
#[must_use]
pub fn command_long_frame(
    gcs: u8,
    target_system: u8,
    target_component: u8,
    sequence: u8,
    command: u16,
    params: [f32; 7],
    confirmation: u8,
) -> Vec<u8> {
    let command = CommandLong {
        target_system,
        target_component,
        command: MavCmd::try_from(command).unwrap_or(MavCmd::ComponentArmDisarm),
        confirmation,
        param1: params[0],
        param2: params[1],
        param3: params[2],
        param4: params[3],
        param5: params[4],
        param6: params[5],
        param7: params[6],
    };
    frame(gcs, crate::GCS_COMPONENT_ID, sequence, &command)
}

/// A `COMMAND_INT`'s place: its frame, `param1` to `param4`, and the
/// position as the message carries it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IntPlace {
    /// `MAV_FRAME`.
    pub frame: u8,
    pub params: [f32; 4],
    /// Latitude, degrees times 1e7.
    pub x: i32,
    /// Longitude, degrees times 1e7.
    pub y: i32,
    /// The height in the frame.
    pub z: f32,
}

/// A `COMMAND_INT` from the daemon's GCS address: a command with a place,
/// such as `DO_REPOSITION`. It has no confirmation count; a retry is the
/// same frame again.
#[must_use]
pub fn command_int_frame(
    gcs: u8,
    target_system: u8,
    target_component: u8,
    sequence: u8,
    command: u16,
    place: IntPlace,
) -> Vec<u8> {
    use mavio::dialects::ardupilotmega::enums::MavFrame;
    use mavio::dialects::ardupilotmega::messages::CommandInt;
    let command = CommandInt {
        target_system,
        target_component,
        frame: MavFrame::try_from(place.frame).unwrap_or(MavFrame::GlobalRelativeAltInt),
        command: MavCmd::try_from(command).unwrap_or(MavCmd::DoReposition),
        current: 0,
        autocontinue: 0,
        param1: place.params[0],
        param2: place.params[1],
        param3: place.params[2],
        param4: place.params[3],
        x: place.x,
        y: place.y,
        z: place.z,
    };
    frame(gcs, crate::GCS_COMPONENT_ID, sequence, &command)
}

/// A vehicle's `COMMAND_ACK` to a ground station's `(system, component)`.
#[must_use]
pub fn command_ack_frame(
    system_id: u8,
    component_id: u8,
    to: (u8, u8),
    sequence: u8,
    command: u16,
    result: u8,
) -> Vec<u8> {
    use mavio::dialects::ardupilotmega::enums::MavResult;
    use mavio::dialects::ardupilotmega::messages::CommandAck;
    let ack = CommandAck {
        command: MavCmd::try_from(command).unwrap_or(MavCmd::ComponentArmDisarm),
        result: MavResult::try_from(result).unwrap_or(MavResult::Failed),
        target_system: to.0,
        target_component: to.1,
        ..CommandAck::default()
    };
    frame(system_id, component_id, sequence, &ack)
}

/// `STATUSTEXT`: one message, or one 50-byte part of a long one (`id` not 0).
#[must_use]
pub fn status_text_frame(
    system_id: u8,
    sequence: u8,
    severity: u8,
    text: &str,
    (id, chunk_seq): (u16, u8),
) -> Vec<u8> {
    use mavio::dialects::ardupilotmega::enums::MavSeverity;
    use mavio::dialects::ardupilotmega::messages::Statustext;
    let mut bytes = [0u8; 50];
    let len = text.len().min(50);
    bytes[..len].copy_from_slice(&text.as_bytes()[..len]);
    let message = Statustext {
        severity: MavSeverity::try_from(severity).unwrap_or(MavSeverity::Info),
        text: bytes,
        id,
        chunk_seq,
    };
    frame(system_id, 1, sequence, &message)
}

/// `MAV_CMD_SET_MESSAGE_INTERVAL`: stream `message_id` every `interval_us`.
#[must_use]
pub fn set_message_interval_frame(
    gcs: u8,
    target_system: u8,
    target_component: u8,
    sequence: u8,
    message_id: u32,
    interval_us: u32,
) -> Vec<u8> {
    #[allow(
        clippy::cast_precision_loss,
        reason = "ids and intervals fit f32 exactly enough"
    )]
    let command = CommandLong {
        target_system,
        target_component,
        command: MavCmd::SetMessageInterval,
        param1: message_id as f32,
        param2: interval_us as f32,
        ..CommandLong::default()
    };
    frame(gcs, crate::GCS_COMPONENT_ID, sequence, &command)
}

/// A `TERRAIN_REQUEST`, which the daemon must not answer.
#[must_use]
pub fn terrain_request_frame(system_id: u8, component_id: u8, sequence: u8) -> Vec<u8> {
    frame(
        system_id,
        component_id,
        sequence,
        &mavio::dialects::ardupilotmega::messages::TerrainRequest::default(),
    )
}

/// `TERRAIN_REQUEST` for a block's missing groups, as ArduPilot asks (S118).
#[must_use]
pub fn terrain_block_request_frame(
    system_id: u8,
    component_id: u8,
    sequence: u8,
    block: crate::terrain::Block,
    mask: u64,
) -> Vec<u8> {
    frame(
        system_id,
        component_id,
        sequence,
        &mavio::dialects::ardupilotmega::messages::TerrainRequest {
            lat: block.lat,
            lon: block.lon,
            grid_spacing: block.spacing,
            mask,
        },
    )
}

/// `TERRAIN_CHECK`: the vehicle's terrain height at a point, asked by the
/// ground station (S119).
#[must_use]
pub fn terrain_check_frame(gcs: u8, sequence: u8, lat: i32, lon: i32) -> Vec<u8> {
    frame(
        gcs,
        crate::GCS_COMPONENT_ID,
        sequence,
        &mavio::dialects::ardupilotmega::messages::TerrainCheck { lat, lon },
    )
}

/// `TERRAIN_DATA`: one group's heights for a block, from the ground station
/// (S118).
#[must_use]
pub fn terrain_data_frame(
    gcs: u8,
    sequence: u8,
    block: crate::terrain::Block,
    group: crate::terrain::Group,
) -> Vec<u8> {
    frame(
        gcs,
        crate::GCS_COMPONENT_ID,
        sequence,
        &mavio::dialects::ardupilotmega::messages::TerrainData {
            lat: block.lat,
            lon: block.lon,
            grid_spacing: block.spacing,
            gridbit: group.gridbit,
            data: group.heights,
        },
    )
}

fn param_id(name: &str) -> [u8; 16] {
    let mut id = [0u8; 16];
    for (slot, byte) in id.iter_mut().zip(name.bytes()) {
        *slot = byte;
    }
    id
}

fn param_type(value: u8) -> mavio::dialects::ardupilotmega::enums::MavParamType {
    mavio::dialects::ardupilotmega::enums::MavParamType::try_from(value).unwrap_or_default()
}

fn mission_type() -> mavio::dialects::ardupilotmega::enums::MavMissionType {
    mavio::dialects::ardupilotmega::enums::MavMissionType::Mission
}

/// `MAV_MISSION_TYPE`: 0 the mission, 1 the fence, 2 rally points.
fn mission_type_of(kind: u8) -> mavio::dialects::ardupilotmega::enums::MavMissionType {
    mavio::dialects::ardupilotmega::enums::MavMissionType::try_from(kind)
        .unwrap_or_else(|_| mission_type())
}

/// `PARAM_REQUEST_LIST` from the daemon.
#[must_use]
pub fn param_request_list_frame(
    gcs: u8,
    target_system: u8,
    target_component: u8,
    sequence: u8,
) -> Vec<u8> {
    use mavio::dialects::ardupilotmega::messages::ParamRequestList;
    let request = ParamRequestList {
        target_system,
        target_component,
    };
    frame(gcs, crate::GCS_COMPONENT_ID, sequence, &request)
}

/// `PARAM_REQUEST_READ` by index from the daemon.
#[must_use]
pub fn param_request_read_frame(
    gcs: u8,
    target_system: u8,
    target_component: u8,
    sequence: u8,
    index: u16,
) -> Vec<u8> {
    use mavio::dialects::ardupilotmega::messages::ParamRequestRead;
    let request = ParamRequestRead {
        target_system,
        target_component,
        param_id: [0; 16],
        param_index: i16::try_from(index).unwrap_or(-1),
    };
    frame(gcs, crate::GCS_COMPONENT_ID, sequence, &request)
}

/// `PARAM_SET` from the daemon.
/// `FILE_TRANSFER_PROTOCOL` with a payload (S74): from the ground station
/// to a vehicle, or with `from` set, a vehicle's answer.
#[must_use]
pub fn ftp_frame(from: (u8, u8), target: (u8, u8), sequence: u8, payload: [u8; 251]) -> Vec<u8> {
    use mavio::dialects::ardupilotmega::messages::FileTransferProtocol;
    let message = FileTransferProtocol {
        target_network: 0,
        target_system: target.0,
        target_component: target.1,
        payload,
    };
    frame(from.0, from.1, sequence, &message)
}

#[must_use]
pub fn param_set_frame(
    gcs: u8,
    target_system: u8,
    target_component: u8,
    sequence: u8,
    name: &str,
    value: f32,
    kind: u8,
) -> Vec<u8> {
    use mavio::dialects::ardupilotmega::messages::ParamSet;
    let set = ParamSet {
        target_system,
        target_component,
        param_id: param_id(name),
        param_value: value,
        param_type: param_type(kind),
    };
    frame(gcs, crate::GCS_COMPONENT_ID, sequence, &set)
}

/// A vehicle's `PARAM_VALUE`.
#[must_use]
#[allow(clippy::too_many_arguments, reason = "the message's own fields")]
pub fn param_value_frame(
    system_id: u8,
    component_id: u8,
    sequence: u8,
    name: &str,
    value: f32,
    kind: u8,
    count: u16,
    index: u16,
) -> Vec<u8> {
    use mavio::dialects::ardupilotmega::messages::ParamValue;
    let value = ParamValue {
        param_id: param_id(name),
        param_value: value,
        param_type: param_type(kind),
        param_count: count,
        param_index: index,
    };
    frame(system_id, component_id, sequence, &value)
}

/// `MISSION_REQUEST_LIST` from the daemon.
#[must_use]
pub fn mission_request_list_frame(
    gcs: u8,
    target_system: u8,
    target_component: u8,
    sequence: u8,
    kind: u8,
) -> Vec<u8> {
    use mavio::dialects::ardupilotmega::messages::MissionRequestList;
    let request = MissionRequestList {
        target_system,
        target_component,
        mission_type: mission_type_of(kind),
    };
    frame(gcs, crate::GCS_COMPONENT_ID, sequence, &request)
}

/// Where a mission message comes from and goes to: `(system, component)`
/// of the sender, then of the target.
#[derive(Clone, Copy, Debug)]
pub struct Route {
    pub from: (u8, u8),
    pub to: (u8, u8),
}

impl Route {
    /// From the daemon, as ground station `gcs`, to a vehicle's component.
    #[must_use]
    pub const fn to_vehicle(gcs: u8, system_id: u8, component_id: u8) -> Self {
        Self {
            from: (gcs, crate::GCS_COMPONENT_ID),
            to: (system_id, component_id),
        }
    }

    /// From a vehicle's component to a ground station's `(system,
    /// component)`.
    #[must_use]
    pub const fn to_gcs(system_id: u8, component_id: u8, gcs: (u8, u8)) -> Self {
        Self {
            from: (system_id, component_id),
            to: gcs,
        }
    }
}

/// `MISSION_COUNT`: the daemon starting an upload, or a vehicle answering a
/// request for its list.
#[must_use]
pub fn mission_count_frame(route: Route, sequence: u8, count: u16, kind: u8) -> Vec<u8> {
    use mavio::dialects::ardupilotmega::messages::MissionCount;
    let message = MissionCount {
        target_system: route.to.0,
        target_component: route.to.1,
        count,
        mission_type: mission_type_of(kind),
        opaque_id: 0,
    };
    frame(route.from.0, route.from.1, sequence, &message)
}

/// `MISSION_REQUEST_INT` for item `seq`.
#[must_use]
pub fn mission_request_frame(route: Route, sequence: u8, seq: u16, kind: u8) -> Vec<u8> {
    use mavio::dialects::ardupilotmega::messages::MissionRequestInt;
    let message = MissionRequestInt {
        target_system: route.to.0,
        target_component: route.to.1,
        seq,
        mission_type: mission_type_of(kind),
    };
    frame(route.from.0, route.from.1, sequence, &message)
}

/// `MISSION_ITEM_INT`.
#[must_use]
pub fn mission_item_frame(
    route: Route,
    sequence: u8,
    item: &crate::decode::MissionItemInt,
) -> Vec<u8> {
    use mavio::dialects::ardupilotmega::enums::MavFrame;
    use mavio::dialects::ardupilotmega::messages::MissionItemInt;
    let message = MissionItemInt {
        target_system: route.to.0,
        target_component: route.to.1,
        seq: item.seq,
        frame: MavFrame::try_from(item.frame).unwrap_or(MavFrame::GlobalRelativeAlt),
        command: MavCmd::try_from(item.command).unwrap_or(MavCmd::NavWaypoint),
        current: u8::from(item.current),
        autocontinue: u8::from(item.autocontinue),
        param1: item.params[0],
        param2: item.params[1],
        param3: item.params[2],
        param4: item.params[3],
        x: item.x,
        y: item.y,
        z: item.z,
        mission_type: mission_type_of(item.mission_type),
    };
    frame(route.from.0, route.from.1, sequence, &message)
}

/// `MISSION_ACK` with `MAV_MISSION_RESULT`.
#[must_use]
pub fn mission_ack_frame(route: Route, sequence: u8, result: u8, kind: u8) -> Vec<u8> {
    use mavio::dialects::ardupilotmega::enums::MavMissionResult;
    use mavio::dialects::ardupilotmega::messages::MissionAck;
    let message = MissionAck {
        target_system: route.to.0,
        target_component: route.to.1,
        type_: MavMissionResult::try_from(result).unwrap_or(MavMissionResult::MavMissionError),
        mission_type: mission_type_of(kind),
        opaque_id: 0,
    };
    frame(route.from.0, route.from.1, sequence, &message)
}

/// `MANUAL_CONTROL` from the daemon: x pitch, y roll, z throttle (0 to
/// 1000), r yaw.
#[must_use]
pub fn manual_control_frame(gcs: u8, target_system: u8, sequence: u8, sticks: [i16; 4]) -> Vec<u8> {
    use mavio::dialects::ardupilotmega::messages::ManualControl;
    let [x, y, z, r] = sticks;
    let message = ManualControl {
        target: target_system,
        x,
        y,
        z,
        r,
        ..ManualControl::default()
    };
    frame(gcs, crate::GCS_COMPONENT_ID, sequence, &message)
}

/// RC input PWM; untouched channels use `UINT16_MAX`, including extensions.
#[must_use]
pub fn rc_override_frame(
    gcs: u8,
    target_system: u8,
    target_component: u8,
    sequence: u8,
    channels: [u16; 18],
) -> Vec<u8> {
    use mavio::dialects::ardupilotmega::messages::RcChannelsOverride;
    let message = RcChannelsOverride {
        target_system,
        target_component,
        chan1_raw: channels[0],
        chan2_raw: channels[1],
        chan3_raw: channels[2],
        chan4_raw: channels[3],
        chan5_raw: channels[4],
        chan6_raw: channels[5],
        chan7_raw: channels[6],
        chan8_raw: channels[7],
        chan9_raw: channels[8],
        chan10_raw: channels[9],
        chan11_raw: channels[10],
        chan12_raw: channels[11],
        chan13_raw: channels[12],
        chan14_raw: channels[13],
        chan15_raw: channels[14],
        chan16_raw: channels[15],
        chan17_raw: channels[16],
        chan18_raw: channels[17],
    };
    frame(gcs, crate::GCS_COMPONENT_ID, sequence, &message)
}

/// `MAV_PARAM_TYPE_REAL32`, and the integer types ArduPilot uses.
pub const REAL32: u8 = 9;
pub const INT8: u8 = 2;
pub const INT16: u8 = 4;
pub const INT32: u8 = 6;

/// A few of `ArduCopter`'s parameters, with their types, as the fake
/// vehicle's table.
pub const FAKE_PARAMETERS: [(&str, f32, u8); 24] = [
    ("FORMAT_VERSION", 120.0, INT16),
    ("SYSID_THISMAV", 1.0, INT16),
    ("SYSID_MYGCS", 255.0, INT16),
    ("ANGLE_MAX", 3000.0, INT16),
    ("ARMING_CHECK", 1.0, INT32),
    ("BATT_CAPACITY", 3300.0, INT32),
    ("BATT_LOW_VOLT", 10.5, REAL32),
    ("BATT_FS_LOW_ACT", 2.0, INT8),
    ("FENCE_ENABLE", 0.0, INT8),
    ("FENCE_ALT_MAX", 100.0, REAL32),
    ("FENCE_RADIUS", 300.0, REAL32),
    ("FS_THR_ENABLE", 1.0, INT8),
    ("FS_GCS_ENABLE", 0.0, INT8),
    ("LAND_SPEED", 50.0, INT16),
    ("PILOT_SPEED_UP", 250.0, INT16),
    ("RTL_ALT", 1500.0, INT32),
    ("RTL_LOIT_TIME", 5000.0, INT32),
    ("WPNAV_SPEED", 1000.0, REAL32),
    ("WPNAV_SPEED_UP", 250.0, REAL32),
    ("WPNAV_SPEED_DN", 150.0, REAL32),
    ("WPNAV_RADIUS", 200.0, REAL32),
    ("WPNAV_ACCEL", 250.0, REAL32),
    ("SERIAL0_BAUD", 115.0, INT32),
    ("SR0_POSITION", 4.0, INT16),
];

/// `MAV_SYS_STATUS_PREARM_CHECK`, set or not.
fn prearm(set: bool) -> MavSysStatusSensor {
    MavSysStatusSensor::from_bits_truncate(if set { 0x1000_0000 } else { 0 })
}

/// How long without `MANUAL_CONTROL` before the fake's failsafe: RTL.
pub const FAILSAFE_S: f64 = 1.5;

/// ArduPilot SITL's default home (CMAC, Canberra): degrees and metres AMSL.
const HOME: (f64, f64, f64) = (-35.363_261, 149.165_230, 584.0);
/// How far the fake vehicle's terrain data puts the ground under home.
const TERRAIN_BELOW_HOME_M: f64 = 4.0;

/// Mission item 0, home, as ArduPilot keeps it.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    reason = "degrees fit the fixed point"
)]
pub fn home_item() -> crate::decode::MissionItemInt {
    crate::decode::MissionItemInt {
        seq: 0,
        frame: 0,
        command: 16,
        current: false,
        autocontinue: true,
        params: [0.0; 4],
        x: (HOME.0 * 1e7).round() as i32,
        y: (HOME.1 * 1e7).round() as i32,
        z: HOME.2 as f32,
        mission_type: 0,
    }
}

/// `MAV_COMP_ID_CAMERA`.
pub const CAMERA_COMPONENT: u8 = 100;

/// A vehicle that heartbeats and answers `AUTOPILOT_VERSION` requests.
#[allow(clippy::struct_excessive_bools, reason = "a test double's switches")]
#[derive(Clone, Debug)]
pub struct FakeVehicle {
    pub system_id: u8,
    pub uid: u64,
    /// Answers the version request only when true, to test a late UID.
    pub answers_version: bool,
    /// `flight_sw_version`: major, minor, patch and release type bytes.
    /// 4.5.1 official unless set.
    pub flight_sw_version: u32,
    /// The git hash characters it reports in `flight_custom_version`.
    pub commit: [u8; 8],
    /// Refuses to arm (`Denied`), as a failed pre-arm check does.
    pub refuses_arm: bool,
    /// Refuses to disarm unless forced, as `ArduCopter` does when it thinks
    /// it is still flying.
    pub refuses_disarm: bool,
    /// Never acknowledges commands, to test timeouts.
    pub ignores_commands: bool,
    /// Disarms itself before its first take-off, which it then refuses, as
    /// `ArduCopter` does left armed on the ground past `DISARM_DELAY`.
    pub disarms_before_take_off: bool,
    pub armed: bool,
    /// `ArduCopter`'s `custom_mode`: 0 is `STABILIZE`.
    pub mode: u32,
    /// Its parameters: name, value, `MAV_PARAM_TYPE`.
    pub parameters: Vec<(String, f32, u8)>,
    /// Leaves every nth parameter out of a list answer, as a lossy radio
    /// does, so the daemon has to ask for the gaps.
    pub drops_every: Option<usize>,
    /// The stored mission; item 0 is home, as on ArduPilot.
    pub mission: Vec<crate::decode::MissionItemInt>,
    /// Items it can store; more is refused with `MAV_MISSION_NO_SPACE`.
    pub mission_capacity: u16,
    /// Its fence and rally points (S128), as the mission protocol keeps them.
    pub fence: Vec<crate::decode::MissionItemInt>,
    pub rally: Vec<crate::decode::MissionItemInt>,
    /// An upload in progress: its kind, how many are coming, the items so far.
    upload: Option<(u8, u16, Vec<crate::decode::MissionItemInt>)>,
    /// Seconds into the run when it last booted; `time_boot_ms` counts
    /// from here.
    booted_at_s: f64,
    last_t_s: f64,
    /// Sits on the ground at home, as SITL starts, and climbs only after a
    /// take-off (S33). Otherwise it flies its circle from the start.
    pub on_ground: bool,
    /// Its height above home while `on_ground` is set.
    pub height_m: f64,
    /// Where a take-off is climbing to.
    target_m: Option<f64>,
    /// A `DO_REPOSITION` it is flying to (S9, Go to), in GUIDED.
    go_to: Option<GoTo>,
    /// Where it was at the last telemetry: latitude, longitude, height.
    last_position: (f64, f64, f64),
    /// Reports `MAV_SYS_STATUS_PREARM_CHECK`, passing from this many seconds
    /// into the run, and refuses to arm before then.
    pub prearm_from_s: Option<f64>,
    /// The latest `MANUAL_CONTROL` sticks, and when (in `telemetry` time).
    pub manual: Option<[i16; 4]>,
    manual_at_s: Option<f64>,
    /// A camera component (100) with one stream at this address, which it
    /// names when asked (S39).
    pub camera: Option<String>,
    /// Sends ArduPilot's start-up messages as it boots, two of them
    /// warnings and one long enough to come in two parts (S26).
    pub startup_burst: bool,
    /// Says `PreArm: …` every 30 s while disarmed, as ArduPilot does.
    pub prearm_failure: bool,
    /// Repeats a warning every second, from 12 s in, after the start-up.
    pub repeat_warning: bool,
    /// Shakes more and more from 10 s in, past ArduPilot's 30 and 60 m/s²
    /// (S72).
    pub vibration_high: bool,
    /// Says "Leak Detected!" every 10 s, as `ArduSub` does (S72).
    pub leak: bool,
    /// Streams only the telemetry asked for with `SET_MESSAGE_INTERVAL`, as
    /// `ArduCopter` does with every `SRn_` rate at 0. Off, it streams it
    /// all, as a vehicle another ground station has set up does.
    pub streams_on_request: bool,
    /// The messages asked for, until it reboots.
    requested: std::collections::BTreeSet<u32>,
    /// The ground station it answers: whoever last sent it a frame, as
    /// ArduPilot replies to the sender, whatever its system id.
    gcs: (u8, u8),
    /// The battery's % at the start; it drains 1 % every 12 s (S77).
    pub battery_from_pct: f64,
    /// Its GPS has no fix from this many seconds into the run: no position
    /// from then on, as ArduPilot sends none without one. From 0, it never
    /// has one.
    pub gps_lost_from_s: Option<f64>,
    /// Serves its storage and its parameters, with defaults, by MAVLink
    /// FTP as ArduPilot does (S74, S121); without it, only the list.
    pub ftp: bool,
    /// Has no terrain until a ground station sends it (S118), as ArduPilot
    /// with an empty terrain folder; `None` reports fixed terrain.
    pub terrain: Option<crate::fake_terrain::FakeTerrain>,
    /// What it stores, over FTP (S121).
    pub files: crate::fake_files::FakeFiles,
    /// The modes it lists itself when asked (`AVAILABLE_MODES`); none means
    /// older firmware, which does not answer.
    pub modes: Vec<AvailableMode>,
    said_leak_s: Option<f64>,
    /// When it last said each, in `telemetry` time.
    said_burst: bool,
    said_prearm_s: Option<f64>,
    said_repeat_s: Option<f64>,
    sequence: u8,
}

/// A `param.pck` with defaults for these parameters: the defaults are the
/// values the fake vehicle starts with.
fn param_pck_of(parameters: &[(String, f32, u8)]) -> Vec<u8> {
    let params: Vec<crate::transfers::WithDefault> = parameters
        .iter()
        .map(|(name, value, kind)| {
            let default = FAKE_PARAMETERS
                .iter()
                .find(|(n, _, _)| n == name)
                .map(|(_, d, _)| *d);
            (
                crate::transfers::Param {
                    name: name.clone(),
                    value: *value,
                    kind: *kind,
                },
                default,
            )
        })
        .collect();
    crate::transfers::encode_param_pck(&params)
}

/// A straight flight from where the vehicle was to a place, at `GO_TO_MPS`,
/// then holding there: what `ArduCopter` does with `DO_REPOSITION` in
/// GUIDED, without its turns and speeds.
#[derive(Clone, Copy, Debug, PartialEq)]
struct GoTo {
    /// Latitude, longitude and height above home when the command came.
    from: (f64, f64, f64),
    /// Latitude, longitude and height above home asked for.
    to: (f64, f64, f64),
    /// When it came, in `telemetry` time.
    at_s: f64,
}

/// How fast the fake vehicle flies to a place.
const GO_TO_MPS: f64 = 10.0;

/// Where the fake vehicle is and how it moves: degrees, metres above home,
/// metres per second, degrees clockwise from north, and its velocity north
/// and east in metres per second.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Flight {
    lat: f64,
    lon: f64,
    height: f64,
    speed: f64,
    heading: f64,
    north: f64,
    east: f64,
}

impl GoTo {
    /// Latitude, longitude, height, speed and heading at `t_s`, and the
    /// velocity north and east.
    fn at(&self, t_s: f64) -> (f64, f64, f64, f64, f64, (f64, f64)) {
        let metres_per_degree = 111_320.0;
        let north = (self.to.0 - self.from.0) * metres_per_degree;
        let east = (self.to.1 - self.from.1) * metres_per_degree * self.from.0.to_radians().cos();
        let distance = north.hypot(east);
        let travelled = GO_TO_MPS * (t_s - self.at_s).max(0.0);
        let fraction = if distance < 0.5 {
            1.0
        } else {
            (travelled / distance).min(1.0)
        };
        let lerp = |a: f64, b: f64| a + (b - a) * fraction;
        let arrived = fraction >= 1.0;
        let speed = if arrived { 0.0 } else { GO_TO_MPS };
        let heading = east.atan2(north).to_degrees().rem_euclid(360.0);
        let velocity = if arrived || distance < 0.5 {
            (0.0, 0.0)
        } else {
            (speed * north / distance, speed * east / distance)
        };
        (
            lerp(self.from.0, self.to.0),
            lerp(self.from.1, self.to.1),
            lerp(self.from.2, self.to.2),
            speed,
            heading,
            velocity,
        )
    }
}

impl FakeVehicle {
    #[must_use]
    pub fn new(system_id: u8, uid: u64) -> Self {
        Self {
            system_id,
            uid,
            answers_version: true,
            flight_sw_version: 0x0405_01ff,
            commit: [0; 8],
            refuses_arm: false,
            refuses_disarm: false,
            ignores_commands: false,
            disarms_before_take_off: false,
            armed: false,
            mode: 0,
            parameters: FAKE_PARAMETERS
                .iter()
                .map(|(name, value, kind)| ((*name).to_owned(), *value, *kind))
                .collect(),
            drops_every: None,
            mission: vec![home_item()],
            mission_capacity: 100,
            fence: Vec::new(),
            rally: Vec::new(),
            upload: None,
            booted_at_s: 0.0,
            last_t_s: 0.0,
            on_ground: false,
            height_m: 0.0,
            target_m: None,
            go_to: None,
            last_position: (HOME.0, HOME.1, 0.0),
            prearm_from_s: None,
            manual: None,
            manual_at_s: None,
            camera: None,
            startup_burst: false,
            prearm_failure: false,
            repeat_warning: false,
            vibration_high: false,
            leak: false,
            streams_on_request: true,
            requested: std::collections::BTreeSet::new(),
            battery_from_pct: 100.0,
            gps_lost_from_s: None,
            said_leak_s: None,
            ftp: true,
            terrain: None,
            files: crate::fake_files::FakeFiles::default(),
            modes: Vec::new(),
            said_burst: false,
            said_prearm_s: None,
            said_repeat_s: None,
            sequence: 0,
            gcs: (crate::GCS_SYSTEM_ID, crate::GCS_COMPONENT_ID),
        }
    }

    /// What it says (`STATUSTEXT`) by now, as its flags ask (S26).
    pub fn status_texts(&mut self, t_s: f64) -> Vec<Vec<u8>> {
        let mut said: Vec<(u8, String, (u16, u8))> = Vec::new();
        if self.startup_burst && !self.said_burst {
            self.said_burst = true;
            for (severity, text) in [
                (6, "ArduCopter V4.5.1 (12345678)"),
                (6, "ChibiOS: 1a2b3c4d"),
                (6, "fmuv3 0039002A 3138510F 37323432"),
                (6, "RCOut: PWM:1-12"),
                (4, "GPS 1: not healthy"),
                (6, "IMU0: fast sampling enabled 8.0kHz/2.0kHz"),
                (6, "Frame: QUAD/X"),
                (4, "Compass not calibrated"),
            ] {
                said.push((severity, text.into(), (0, 0)));
            }
            // A long one, in two parts that come back as one.
            let long =
                "EKF3 IMU0 initialised; waiting for a good GPS position before the origin is set";
            said.push((6, long[..50].into(), (7, 0)));
            said.push((6, long[50..].into(), (7, 1)));
        }
        if self.prearm_failure
            && !self.armed
            && self.said_prearm_s.is_none_or(|at| t_s - at >= 30.0)
        {
            self.said_prearm_s = Some(t_s);
            said.push((3, "PreArm: Throttle below failsafe".into(), (0, 0)));
        }
        if self.repeat_warning && t_s >= 12.0 && self.said_repeat_s.is_none_or(|at| t_s - at >= 1.0)
        {
            self.said_repeat_s = Some(t_s);
            said.push((4, "Vibration high".into(), (0, 0)));
        }
        if self.leak && self.said_leak_s.is_none_or(|at| t_s - at >= 10.0) {
            self.said_leak_s = Some(t_s);
            said.push((2, "Leak Detected!".into(), (0, 0)));
        }
        said.into_iter()
            .map(|(severity, text, part)| {
                self.sequence = self.sequence.wrapping_add(1);
                status_text_frame(self.system_id, self.sequence, severity, &text, part)
            })
            .collect()
    }

    /// Restarts: `time_boot_ms` starts again from zero. Parameters and the
    /// mission survive, as they do in an autopilot's storage.
    pub fn reboot(&mut self) {
        self.booted_at_s = self.last_t_s;
        self.armed = false;
        self.requested.clear();
    }

    /// Its next frame of `message` from `component`, if it streams it.
    fn stream(&mut self, component: u8, message: &dyn Message) -> Option<Vec<u8>> {
        if self.streams_on_request && !self.requested.contains(&message.id()) {
            return None;
        }
        let seq = self.next();
        Some(frame(self.system_id, component, seq, message))
    }

    fn to_gcs(&self) -> Route {
        Route::to_gcs(self.system_id, 1, self.gcs)
    }

    fn param_frame(&mut self, index: usize) -> Option<Vec<u8>> {
        let (name, value, kind) = self.parameters.get(index)?.clone();
        let count = u16::try_from(self.parameters.len()).unwrap_or(u16::MAX);
        let sequence = self.next();
        Some(param_value_frame(
            self.system_id,
            1,
            sequence,
            &name,
            value,
            kind,
            count,
            u16::try_from(index).unwrap_or(u16::MAX),
        ))
    }

    /// Answers the parameter and mission protocols as `ArduCopter` does.
    /// Its parameters with their defaults: the values it started with.
    #[cfg(test)]
    pub(crate) fn param_pck(&self) -> Vec<u8> {
        param_pck_of(&self.parameters)
    }

    /// Answers a MAVLink FTP request as ArduPilot does (S121): its
    /// storage, and its parameters as `@PARAM/param.pck` both ways.
    fn ftp_answer(&mut self, request: &crate::transfers::ftp::Answer) -> Vec<Vec<u8>> {
        if !self.ftp {
            return Vec::new();
        }
        let parameters = &self.parameters;
        let answered = self.files.answer(request, || param_pck_of(parameters));
        if let Some(file) = answered.params_written
            && let Ok(params) = crate::transfers::parse_param_pck(&file)
        {
            // Names it does not have are skipped, as ArduPilot does.
            for (param, _) in params {
                if param.name == "FORMAT_VERSION" {
                    continue;
                }
                if let Some(held) = self
                    .parameters
                    .iter_mut()
                    .find(|(n, _, _)| *n == param.name)
                {
                    held.1 = param.value;
                }
            }
        }
        answered
            .payloads
            .into_iter()
            .map(|payload| {
                let sequence = self.next();
                ftp_frame((self.system_id, 1), self.gcs, sequence, payload)
            })
            .collect()
    }

    fn rc_input(&mut self, target: u8, channels: [u16; 18]) {
        if target != self.system_id {
            return;
        }
        if channels[..4].iter().all(|value| *value == 0) {
            return;
        }
        let bipolar = |pwm: u16| i16::try_from(i32::from(pwm) * 2 - 3000).unwrap_or(0);
        self.manual = Some([
            -bipolar(channels[1]),
            bipolar(channels[0]),
            i16::try_from(channels[2]).unwrap_or(1500) - 1000,
            bipolar(channels[3]),
        ]);
        self.manual_at_s = Some(self.last_t_s);
    }

    #[allow(clippy::too_many_lines, reason = "one arm per message")]
    fn converse(&mut self, message: Decoded) -> Vec<Vec<u8>> {
        let mut replies = Vec::new();
        match message {
            Decoded::Ftp {
                target_system,
                payload,
                ..
            } if target_system == self.system_id => {
                let request = crate::transfers::ftp::read(&payload);
                replies.extend(self.ftp_answer(&request));
            }
            Decoded::TerrainCheck { lat, lon } => {
                replies.extend(self.terrain_check_report(lat, lon));
            }
            Decoded::TerrainData {
                lat,
                lon,
                grid_spacing,
                gridbit,
                data,
            } => {
                if let Some(terrain) = &mut self.terrain {
                    let block = crate::terrain::Block {
                        lat,
                        lon,
                        spacing: grid_spacing,
                    };
                    terrain.keep(block, gridbit, data);
                }
            }
            Decoded::ParamRequestList => {
                for index in 0..self.parameters.len() {
                    if self
                        .drops_every
                        .is_some_and(|n| n > 0 && index % n == n - 1)
                    {
                        continue;
                    }
                    replies.extend(self.param_frame(index));
                }
            }
            Decoded::ParamRequestRead { name, index } => {
                let found = if name.is_empty() {
                    usize::try_from(index).ok()
                } else {
                    self.parameters.iter().position(|(n, _, _)| *n == name)
                };
                if let Some(index) = found {
                    replies.extend(self.param_frame(index));
                }
            }
            Decoded::ParamSet {
                name,
                value,
                param_type,
                ..
            } => {
                if let Some(index) = self.parameters.iter().position(|(n, _, _)| *n == name) {
                    // FORMAT_VERSION is read-only: the old value comes back.
                    if name != "FORMAT_VERSION" {
                        self.parameters[index].1 = value;
                    }
                    replies.extend(self.param_frame(index));
                } else if self.on_ground {
                    // The simulation start's SIM_SPEEDUP and TKOFF_ALT,
                    // which the short table leaves out.
                    self.parameters.push((name, value, param_type));
                    replies.extend(self.param_frame(self.parameters.len() - 1));
                }
            }
            Decoded::MissionRequestList { mission_type } => {
                let sequence = self.next();
                let count = u16::try_from(self.stored(mission_type).len()).unwrap_or(u16::MAX);
                replies.push(mission_count_frame(
                    self.to_gcs(),
                    sequence,
                    count,
                    mission_type,
                ));
            }
            Decoded::RcChannelsOverride { target, channels } => self.rc_input(target, channels),
            Decoded::ManualControl { sticks, .. } => {
                self.manual = Some(sticks);
                self.manual_at_s = Some(self.last_t_s);
            }
            // During an upload, requests are ours to make.
            Decoded::MissionRequest { seq, mission_type } if self.upload.is_none() => {
                if let Some(mut item) = self.stored(mission_type).get(usize::from(seq)).cloned() {
                    item.mission_type = mission_type;
                    let sequence = self.next();
                    replies.push(mission_item_frame(self.to_gcs(), sequence, &item));
                }
            }
            Decoded::MissionCount {
                count,
                mission_type,
            } => {
                let sequence = self.next();
                if count > self.mission_capacity {
                    replies.push(mission_ack_frame(self.to_gcs(), sequence, 4, mission_type));
                } else if count == 0 {
                    self.store(mission_type, Vec::new());
                    replies.push(mission_ack_frame(self.to_gcs(), sequence, 0, mission_type));
                } else {
                    self.upload = Some((mission_type, count, Vec::new()));
                    replies.push(mission_request_frame(
                        self.to_gcs(),
                        sequence,
                        0,
                        mission_type,
                    ));
                }
            }
            Decoded::MissionItem(item) => {
                let Some((kind, count, mut received)) = self.upload.take() else {
                    return replies;
                };
                let sequence = self.next();
                let expected = u16::try_from(received.len()).unwrap_or(u16::MAX);
                if item.seq != expected || item.mission_type != kind {
                    // Out of order: ask again for the one it needs.
                    replies.push(mission_request_frame(
                        self.to_gcs(),
                        sequence,
                        expected,
                        kind,
                    ));
                    self.upload = Some((kind, count, received));
                    return replies;
                }
                received.push(item);
                if received.len() == usize::from(count) {
                    self.store(kind, received);
                    replies.push(mission_ack_frame(self.to_gcs(), sequence, 0, kind));
                } else {
                    replies.push(mission_request_frame(
                        self.to_gcs(),
                        sequence,
                        expected + 1,
                        kind,
                    ));
                    self.upload = Some((kind, count, received));
                }
            }
            _ => {}
        }
        replies
    }

    fn next(&mut self) -> u8 {
        self.sequence = self.sequence.wrapping_add(1);
        self.sequence
    }

    /// What it keeps of one kind: the mission, the fence or rally points.
    fn stored(&self, kind: u8) -> &[crate::decode::MissionItemInt] {
        match kind {
            1 => &self.fence,
            2 => &self.rally,
            _ => &self.mission,
        }
    }

    fn store(&mut self, kind: u8, items: Vec<crate::decode::MissionItemInt>) {
        match kind {
            1 => self.fence = items,
            2 => self.rally = items,
            _ => self.mission = items,
        }
    }

    /// A take-off's climb since the last telemetry, at 5 m/s.
    fn climb(&mut self, t_s: f64) {
        const CLIMB_MPS: f64 = 5.0;
        let dt = (t_s - self.last_t_s).max(0.0);
        if let Some(target) = self.target_m.filter(|_| self.armed) {
            self.height_m = (self.height_m + CLIMB_MPS * dt).min(target);
        }
    }

    /// `SYS_STATUS`: the battery, and the pre-arm checks if it reports them.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a percentage"
    )]
    fn status(&self, remaining: f64) -> SysStatus {
        let reports = self.prearm_from_s.is_some();
        SysStatus {
            voltage_battery: (12_600.0 - 12.0 * (100.0 - remaining)) as u16,
            current_battery: 1450,
            battery_remaining: remaining as i8,
            onboard_control_sensors_enabled: prearm(reports),
            onboard_control_sensors_health: prearm(reports && self.ready()),
            ..SysStatus::default()
        }
    }

    fn ready(&self) -> bool {
        self.prearm_from_s.is_none_or(|from| self.last_t_s >= from)
    }

    /// The frame to send once a second.
    pub fn heartbeat(&mut self) -> Vec<u8> {
        let sequence = self.next();
        armed_heartbeat_frame(self.system_id, 1, sequence, self.armed, self.mode)
    }

    /// The camera component's heartbeat, when it has a camera.
    pub fn camera_heartbeat(&mut self) -> Option<Vec<u8>> {
        self.camera.as_ref()?;
        let sequence = self.next();
        Some(camera_heartbeat_frame(
            self.system_id,
            CAMERA_COMPONENT,
            sequence,
        ))
    }

    /// Carries out a command as `ArduCopter` would and returns `MAV_RESULT`,
    /// or `None` for commands it does not acknowledge here.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "mode numbers are small"
    )]
    fn obey(&mut self, command: u16, param1: f32, param2: f32, param7: f32) -> Option<u8> {
        match command {
            400 if param1 > 0.5 => {
                // Forcing (21196 in param2) skips the pre-arm checks.
                let forced = (param2 - 21196.0).abs() < 0.5;
                if !forced && (self.refuses_arm || !self.ready()) {
                    return Some(2);
                }
                self.armed = true;
                Some(0)
            }
            // MAV_CMD_NAV_TAKEOFF: armed and in GUIDED (4) only.
            22 => {
                if self.disarms_before_take_off {
                    self.disarms_before_take_off = false;
                    self.armed = false;
                }
                if !(self.on_ground && self.armed && self.mode == 4) {
                    return Some(4);
                }
                self.target_m = Some(f64::from(param7));
                Some(0)
            }
            400 => {
                // `MAV_RESULT_FAILED`, unless forced.
                if self.refuses_disarm && (param2 - 21196.0).abs() >= 0.5 {
                    return Some(4);
                }
                self.armed = false;
                Some(0)
            }
            176 => {
                self.mode = param2 as u32;
                if self.mode == 6 {
                    self.return_home();
                } else if self.mode != 4 {
                    self.go_to = None;
                }
                Some(0)
            }
            20 => {
                self.mode = 6;
                self.return_home();
                Some(0)
            }
            // MAV_CMD_SET_MESSAGE_INTERVAL: below zero stops it, and zero is
            // the default rate, which on `ArduCopter` is none. ArduPilot
            // acknowledges it, but nothing waits for that, so this does not.
            511 => {
                let message_id = param1 as u32;
                if param2 > 0.0 {
                    self.requested.insert(message_id);
                } else {
                    self.requested.remove(&message_id);
                }
                None
            }
            // The camera protocol: take photos, start and stop video, zoom.
            2000 | 2001 | 2500 | 2501 | 531 => Some(0),
            246 if (param1 - 1.0).abs() < f32::EPSILON => {
                if self.armed {
                    return Some(4);
                }
                self.reboot();
                Some(0)
            }
            _ => None,
        }
    }

    /// RTL: straight back over home at the height it flies, and holding
    /// there, without `ArduCopter`'s climb and landing.
    fn return_home(&mut self) {
        self.go_to = Some(GoTo {
            from: self.last_position,
            to: (HOME.0, HOME.1, self.last_position.2),
            at_s: self.last_t_s,
        });
    }

    /// `DO_REPOSITION` as `ArduCopter` takes it (`MAV_RESULT`): in GUIDED,
    /// or asked to change to it, with a place on Earth and a height above
    /// home, sea level or the terrain; the vehicle then flies straight
    /// there and holds.
    fn obey_place(
        &mut self,
        command: u16,
        frame: u8,
        param2: f32,
        (x, y, z): (i32, i32, f32),
    ) -> Option<u8> {
        if command != 192 {
            return None;
        }
        // MAV_FRAME_GLOBAL_INT, _RELATIVE_ALT_INT, _TERRAIN_ALT_INT (and
        // their float twins): the height above home they mean.
        let above_home = |z: f64| match frame {
            0 | 5 => Some(z - HOME.2),
            3 | 6 => Some(z),
            10 | 11 => Some(z - TERRAIN_BELOW_HOME_M),
            _ => None,
        };
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a small bitmask"
        )]
        let change_mode = (param2.max(0.0) as u32) & 1 == 1;
        if self.mode != 4 && !change_mode {
            return Some(2);
        }
        let (latitude_deg, longitude_deg) = (f64::from(x) / 1e7, f64::from(y) / 1e7);
        if !(-90.0..=90.0).contains(&latitude_deg)
            || !(-180.0..=180.0).contains(&longitude_deg)
            || !z.is_finite()
            || z == 0.0
        {
            return Some(2);
        }
        let Some(height) = above_home(f64::from(z)) else {
            return Some(3);
        };
        self.mode = 4;
        self.go_to = Some(GoTo {
            from: self.last_position,
            to: (latitude_deg, longitude_deg, height),
            at_s: self.last_t_s,
        });
        Some(0)
    }

    /// The replies to bytes it received: `AUTOPILOT_VERSION` when asked, and
    /// `COMMAND_ACK` for arm, disarm, mode changes, RTL and a place to go to.
    /// Its own list of modes, one frame each, when it has one to give.
    fn list_modes(&mut self) -> Vec<Vec<u8>> {
        let count = u8::try_from(self.modes.len()).unwrap_or(u8::MAX);
        let modes = self.modes.clone();
        modes
            .iter()
            .enumerate()
            .map(|(index, mode)| {
                let sequence = self.next();
                let index = u8::try_from(index + 1).unwrap_or(u8::MAX);
                available_modes_frame(self.system_id, 1, sequence, count, index, mode)
            })
            .collect()
    }

    pub fn answer(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut replies = Vec::new();
        for frame in crate::framing::split(bytes).frames {
            if let Some(decoded) = decode(frame)
                && decoded.system_id != self.system_id
            {
                self.gcs = (decoded.system_id, decoded.component_id);
                replies.extend(self.converse(decoded.message));
            }
            if let Some(Decoded::CommandInt {
                command,
                frame,
                param2,
                x,
                y,
                z,
                ..
            }) = decode(frame).map(|d| d.message)
                && !self.ignores_commands
                && let Some(result) = self.obey_place(command, frame, param2, (x, y, z))
            {
                let sequence = self.next();
                replies.push(command_ack_frame(
                    self.system_id,
                    1,
                    self.gcs,
                    sequence,
                    command,
                    result,
                ));
            }
            if let Some(Decoded::CommandLong {
                command,
                param1,
                param2,
                param7,
                ..
            }) = decode(frame).map(|d| d.message)
                && !self.ignores_commands
                && let Some(result) = self.obey(command, param1, param2, param7)
            {
                let sequence = self.next();
                replies.push(command_ack_frame(
                    self.system_id,
                    1,
                    self.gcs,
                    sequence,
                    command,
                    result,
                ));
            }
            if let Some(Decoded::CommandLong {
                command: 512,
                param1,
                ..
            }) = decode(frame).map(|d| d.message)
                && (param1 - 435.0).abs() < f32::EPSILON
            {
                replies.extend(self.list_modes());
            }
            if let Some(Decoded::CommandLong {
                command: 512,
                param1,
                ..
            }) = decode(frame).map(|d| d.message)
                && (param1 - 269.0).abs() < f32::EPSILON
                && let Some(uri) = self.camera.clone()
            {
                let sequence = self.next();
                replies.push(video_stream_information_frame(
                    self.system_id,
                    CAMERA_COMPONENT,
                    sequence,
                    1,
                    &uri,
                ));
            }
            if let Some(decoded) = decode(frame)
                && let Decoded::CommandLong {
                    command: 512,
                    param1,
                    ..
                } = decoded.message
                && (param1 - 148.0).abs() < f32::EPSILON
                && self.answers_version
            {
                let sequence = self.next();
                replies.push(autopilot_version_frame_with_commit(
                    self.system_id,
                    1,
                    sequence,
                    self.uid,
                    self.flight_sw_version,
                    self.commit,
                ));
            }
        }
        replies
    }

    /// Where it is at `t_s`: on its circle of `radius` at `height`, once
    /// every `period_s`, or on its way to a place it was sent
    /// (`DO_REPOSITION`), straight there and then holding.
    fn flight_at(&mut self, t_s: f64, radius: f64, height: f64, period_s: f64) -> Flight {
        let angle = std::f64::consts::TAU * t_s / period_s;
        let metres_per_degree = 111_320.0;
        let speed = std::f64::consts::TAU * radius / period_s;
        let mut flight = Flight {
            lat: HOME.0 + radius * angle.cos() / metres_per_degree,
            lon: HOME.1 + radius * angle.sin() / (metres_per_degree * HOME.0.to_radians().cos()),
            height,
            speed,
            // Flying clockwise seen from above: heading is the angle plus 90°.
            heading: (angle.to_degrees() + 90.0).rem_euclid(360.0),
            north: -speed * angle.sin(),
            east: speed * angle.cos(),
        };
        if let Some(go_to) = self.go_to.filter(|_| !self.on_ground) {
            let (lat, lon, height, speed, heading, (north, east)) = go_to.at(t_s);
            flight = Flight {
                lat,
                lon,
                height,
                speed,
                heading,
                north,
                east,
            };
        }
        self.last_position = (flight.lat, flight.lon, flight.height);
        flight
    }

    /// Telemetry at `t_s` seconds into the flight: a 80 m circle at 30 m above
    /// home, once a minute, around ArduPilot SITL's default home (CMAC,
    /// Canberra), with a battery that slowly drains.
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "values are bounded by construction"
    )]
    pub fn telemetry(&mut self, t_s: f64) -> Vec<Vec<u8>> {
        const RADIUS_M: f64 = 80.0;
        const PERIOD_S: f64 = 60.0;
        const HEIGHT_M: f64 = 30.0;
        self.climb(t_s);
        // On the ground or climbing, it stays over home.
        let radius = if self.on_ground { 0.0 } else { RADIUS_M };
        let height = if self.on_ground {
            self.height_m
        } else {
            HEIGHT_M
        };
        let Flight {
            lat,
            lon,
            height,
            speed,
            heading,
            north,
            east,
        } = self.flight_at(t_s, radius, height, PERIOD_S);
        let mm = |metres: f64| (metres * 1000.0).round() as i32;
        self.last_t_s = t_s;
        // The pilot's sticks went quiet: ArduPilot's failsafe returns home.
        if self.manual_at_s.is_some_and(|at| t_s - at > FAILSAFE_S) {
            self.manual_at_s = None;
            self.mode = 6;
            self.return_home();
        }
        let time_boot_ms = ((t_s - self.booted_at_s).max(0.0) * 1000.0) as u32;
        let id = 1;
        // No fix: a latitude and longitude of 0, which is no position.
        let fix = self.gps_lost_from_s.is_none_or(|from| t_s < from);
        let e7 = |degrees: f64| {
            if fix {
                (degrees * 1e7).round() as i32
            } else {
                0
            }
        };
        let mut frames = Vec::new();
        frames.extend(self.stream(
            id,
            &GlobalPositionInt {
                time_boot_ms,
                lat: e7(lat),
                lon: e7(lon),
                alt: mm(HOME.2 + height),
                relative_alt: mm(height),
                vx: (north * 100.0) as i16,
                vy: (east * 100.0) as i16,
                vz: 0,
                hdg: (heading * 100.0) as u16,
            },
        ));
        frames.extend(self.stream(
            id,
            &GpsRawInt {
                time_usec: (t_s * 1e6) as u64,
                fix_type: if fix {
                    GpsFixType::_3dFix
                } else {
                    GpsFixType::NoFix
                },
                lat: e7(lat),
                lon: e7(lon),
                alt: mm(HOME.2 + height),
                eph: 70,
                epv: 120,
                vel: (speed * 100.0) as u16,
                cog: (heading * 100.0) as u16,
                satellites_visible: 14,
                ..GpsRawInt::default()
            },
        ));
        frames.extend(self.stream(
            id,
            &Attitude {
                time_boot_ms,
                roll: 12f32.to_radians(),
                pitch: (-3f32).to_radians(),
                yaw: heading.to_radians() as f32,
                ..Attitude::default()
            },
        ));
        let remaining = (self.battery_from_pct - t_s / 12.0).max(0.0);
        let status = self.status(remaining);
        frames.extend(self.stream(id, &status));
        frames.extend(self.stream(
            id,
            &VfrHud {
                airspeed: speed as f32,
                groundspeed: speed as f32,
                heading: heading as i16,
                throttle: 42,
                alt: (HOME.2 + height) as f32,
                climb: 0.0,
            },
        ));
        frames.extend(self.outputs(t_s));
        frames.extend(self.health(t_s, height));
        frames
    }

    /// The health and environment values (S72): its radio, EKF, vibration,
    /// battery used, wind and the rangefinder under it.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "small simulated values"
    )]
    fn health(&mut self, t_s: f64, height: f64) -> Vec<Vec<u8>> {
        let s = self.system_id;
        let shake = if self.vibration_high && t_s > 10.0 {
            (12.0 + (t_s - 10.0) * 2.0).min(75.0)
        } else {
            12.0
        } as f32;
        let mut cells = [u16::MAX; 10];
        cells[..3].copy_from_slice(&[4200, 4190, 4210]);
        let mut frames = Vec::new();
        let seq = self.next();
        // A telemetry radio reports as the vehicle's component 68, asked or
        // not: it is the radio's own message, not the autopilot's.
        frames.push(frame(
            s,
            68,
            seq,
            &RadioStatus {
                rssi: 200,
                remrssi: 190,
                noise: 40,
                remnoise: 42,
                rxerrors: 3,
                fixed: 1,
                ..RadioStatus::default()
            },
        ));
        frames.extend(self.stream(
            1,
            &EkfStatusReport {
                flags: EkfStatusFlags::from_bits_truncate(0b1_0111_1111),
                velocity_variance: 0.12,
                pos_horiz_variance: 0.18,
                pos_vert_variance: 0.1,
                compass_variance: 0.05,
                terrain_alt_variance: 0.0,
                ..EkfStatusReport::default()
            },
        ));
        frames.extend(self.stream(
            1,
            &Vibration {
                vibration_x: shake * 0.8,
                vibration_y: shake * 0.9,
                vibration_z: shake,
                clipping_0: if shake > 60.0 { 4 } else { 0 },
                ..Vibration::default()
            },
        ));
        frames.extend(self.stream(
            1,
            &BatteryStatus {
                // 14.5 A since it started.
                current_consumed: (t_s * 14.5 / 3.6) as i32,
                voltages: cells,
                temperature: i16::MAX,
                ..BatteryStatus::default()
            },
        ));
        frames.extend(self.stream(
            1,
            &Wind {
                direction: 270.0,
                speed: 4.2,
                speed_z: 0.0,
            },
        ));
        frames.extend(self.stream(
            1,
            &DistanceSensor {
                current_distance: (height * 100.0) as u16,
                max_distance: 4000,
                orientation: MavSensorOrientation::MavSensorRotationPitch270,
                ..DistanceSensor::default()
            },
        ));
        frames.extend(self.terrain_report(height));
        frames
    }

    /// A `TERRAIN_REPORT` for a point a ground station asked about (S119):
    /// from its own terrain, or the fixed terrain without.
    #[allow(clippy::cast_possible_truncation, reason = "small simulated values")]
    fn terrain_check_report(&mut self, lat: i32, lon: i32) -> Vec<Vec<u8>> {
        let ground = match &self.terrain {
            Some(terrain) => terrain.height(lat, lon),
            None => Some((HOME.2 - TERRAIN_BELOW_HOME_M) as f32),
        };
        let (pending, loaded) = self
            .terrain
            .as_ref()
            .map_or((0, 56), |t| t.counts(lat, lon));
        let seq = self.next();
        vec![frame(
            self.system_id,
            1,
            seq,
            &TerrainReport {
                lat,
                lon,
                spacing: if ground.is_some() {
                    crate::fake_terrain::SPACING
                } else {
                    0
                },
                terrain_height: ground.unwrap_or(0.0),
                current_height: 0.0,
                pending,
                loaded,
            },
        )]
    }

    /// With terrain of its own (S118): asks for what it lacks under it, and
    /// reports the height there only once it has it.
    #[allow(clippy::cast_possible_truncation, reason = "positions in 1e-7 degrees")]
    fn terrain_of_its_own(&mut self, height: f64) -> Vec<Vec<u8>> {
        let (lat, lon, _) = self.last_position;
        let (lat, lon) = ((lat * 1e7) as i32, (lon * 1e7) as i32);
        let mut frames = Vec::new();
        let Some(terrain) = &mut self.terrain else {
            return frames;
        };
        let ask = terrain.ask(lat, lon, self.last_t_s);
        let (pending, loaded) = terrain.counts(lat, lon);
        let ground = terrain.height(lat, lon);
        if let Some((block, mask)) = ask {
            let sequence = self.next();
            frames.push(terrain_block_request_frame(
                self.system_id,
                1,
                sequence,
                block,
                mask,
            ));
        }
        let amsl = HOME.2 + height;
        frames.extend(self.stream(
            1,
            &TerrainReport {
                lat,
                lon,
                spacing: if ground.is_some() {
                    crate::fake_terrain::SPACING
                } else {
                    0
                },
                terrain_height: ground.unwrap_or(0.0),
                current_height: ground.map_or(0.0, |g| (amsl - f64::from(g)) as f32),
                pending,
                loaded,
            },
        ));
        frames
    }

    /// `TERRAIN_REPORT`: the autopilot's terrain data puts the ground 4 m
    /// under home, so the height above terrain differs from the height
    /// above home.
    #[allow(clippy::cast_possible_truncation, reason = "small simulated values")]
    fn terrain_report(&mut self, height: f64) -> Vec<Vec<u8>> {
        if self.terrain.is_some() {
            return self.terrain_of_its_own(height);
        }
        self.stream(
            1,
            &TerrainReport {
                lat: (HOME.0 * 1e7) as i32,
                lon: (HOME.1 * 1e7) as i32,
                spacing: 100,
                terrain_height: (HOME.2 - TERRAIN_BELOW_HOME_M) as f32,
                current_height: (height + TERRAIN_BELOW_HOME_M) as f32,
                pending: 0,
                loaded: 56,
            },
        )
        .into_iter()
        .collect()
    }

    /// `SERVO_OUTPUT_RAW`, `MISSION_CURRENT` and `NAV_CONTROLLER_OUTPUT`,
    /// and `POSITION_TARGET_GLOBAL_INT` while it flies to a place, as
    /// `ArduCopter` sends it in GUIDED.
    fn outputs(&mut self, t_s: f64) -> Vec<Vec<u8>> {
        let id = 1;
        let servos = self.servos(t_s);
        let mut frames = Vec::new();
        frames.extend(self.stream(id, &servos));
        let current = MissionCurrent {
            seq: 0,
            ..MissionCurrent::default()
        };
        frames.extend(self.stream(id, &current));
        // 120 m to the waypoint, for the instruments (S43).
        let navigation = NavControllerOutput {
            wp_dist: 120,
            ..NavControllerOutput::default()
        };
        frames.extend(self.stream(id, &navigation));
        // In GUIDED and RTL, as ArduPilot sends it while it has a place to go.
        if let Some(go_to) = self.go_to.filter(|_| matches!(self.mode, 4 | 6)) {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "degrees times 1e7 fit; the time is clamped at zero"
            )]
            let target = PositionTargetGlobalInt {
                time_boot_ms: ((t_s - self.booted_at_s).max(0.0) * 1000.0) as u32,
                coordinate_frame: MavFrame::GlobalRelativeAltInt,
                // Velocity, acceleration and yaw ignored: a place.
                type_mask: PositionTargetTypemask::from_bits_truncate(0b1111_1111_1000),
                lat_int: (go_to.to.0 * 1e7).round() as i32,
                lon_int: (go_to.to.1 * 1e7).round() as i32,
                alt: go_to.to.2 as f32,
                ..PositionTargetGlobalInt::default()
            };
            frames.extend(self.stream(id, &target));
        }
        frames
    }

    /// A quad's four motors: idle when disarmed, around hover when armed,
    /// a little uneven so a 3D view has something to spin.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "pulses in microseconds"
    )]
    fn servos(&self, t_s: f64) -> ServoOutputRaw {
        let motor = |phase: f64| {
            if self.armed {
                (1500.0 + 40.0 * (t_s + phase).sin()) as u16
            } else {
                1000
            }
        };
        ServoOutputRaw {
            time_usec: (t_s * 1e6) as u32,
            servo1_raw: motor(0.0),
            servo2_raw: motor(1.0),
            servo3_raw: motor(2.0),
            servo4_raw: motor(3.0),
            ..ServoOutputRaw::default()
        }
    }
}
