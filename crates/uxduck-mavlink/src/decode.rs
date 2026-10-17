//! Decodes the messages the daemon acts on, from the ArduPilot dialect
//! (a superset of `common`). Anything else is left alone.

use mavio::dialects::ardupilotmega::Ardupilotmega;
use mavio::io::StdIoReader;
use mavio::protocol::Versionless;
use mavio::{Frame, Receiver};

/// One frame, with the parts the reducers need.
#[derive(Clone, Debug, PartialEq)]
pub struct Decoded {
    pub system_id: u8,
    pub component_id: u8,
    pub message: Message,
}

/// Raw fields in MAVLink's own units; the producers convert to SI.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    Heartbeat {
        /// `MAV_TYPE`, such as `Quadrotor` or `Gcs`.
        mav_type: String,
        /// `MAV_AUTOPILOT`; `Invalid` for anything that is not an autopilot.
        autopilot: String,
        system_status: String,
        /// `MAV_MODE_FLAG` bits; 0x80 is armed.
        base_mode: u8,
        /// The autopilot's own mode number (see `modes`).
        custom_mode: u32,
    },
    /// The answer to a command: `MAV_RESULT`.
    CommandAck {
        command: u16,
        result: u8,
        target_system: u8,
        target_component: u8,
    },
    AutopilotVersion {
        uid: u64,
        uid2: [u8; 18],
        flight_sw_version: u32,
        /// ArduPilot puts the first 8 characters of its git hash here.
        flight_custom_version: [u8; 8],
    },
    /// One mode of the vehicle's own list (`AVAILABLE_MODES`), answering a
    /// request for message 435: one message per mode, `mode_index` from 1
    /// to `number_modes`.
    AvailableModes {
        number_modes: u8,
        mode_index: u8,
        custom_mode: u32,
        /// `MAV_MODE_PROPERTY` bits.
        properties: u32,
        name: String,
    },
    /// The vehicle's list of modes changed (`AVAILABLE_MODES_MONITOR`):
    /// `seq` counts the changes.
    AvailableModesMonitor {
        seq: u8,
    },
    /// The autopilot's fused estimate.
    GlobalPositionInt {
        /// Milliseconds since the autopilot booted.
        time_boot_ms: u32,
        /// Degrees times 1e7.
        lat: i32,
        lon: i32,
        /// Millimetres above mean sea level.
        alt: i32,
        /// Millimetres above home.
        relative_alt: i32,
        /// Centimetres per second, north, east, down.
        vx: i16,
        vy: i16,
        vz: i16,
        /// Centidegrees; `u16::MAX` when unknown.
        hdg: u16,
    },
    GpsRawInt {
        /// `GPS_FIX_TYPE`.
        fix_type: u8,
        lat: i32,
        lon: i32,
        /// Millimetres above mean sea level.
        alt: i32,
        /// HDOP and VDOP times 100; `u16::MAX` when unknown.
        eph: u16,
        epv: u16,
        /// Centimetres per second; `u16::MAX` when unknown.
        vel: u16,
        /// Centidegrees; `u16::MAX` when unknown.
        cog: u16,
        /// `u8::MAX` when unknown.
        satellites_visible: u8,
        /// Millimetres above the WGS84 ellipsoid (MAVLink 2 extension; 0 when absent).
        alt_ellipsoid: i32,
        /// Millimetres (extension; 0 when absent).
        h_acc: u32,
        v_acc: u32,
    },
    /// Radians and radians per second.
    Attitude {
        time_boot_ms: u32,
        roll: f32,
        pitch: f32,
        yaw: f32,
        rollspeed: f32,
        pitchspeed: f32,
        yawspeed: f32,
    },
    SysStatus {
        /// Millivolts; `u16::MAX` when unknown.
        voltage_battery: u16,
        /// Centiamperes; -1 when unknown.
        current_battery: i16,
        /// Percent; -1 when unknown.
        battery_remaining: i8,
        /// Whether the autopilot reports its pre-arm checks
        /// (`MAV_SYS_STATUS_PREARM_CHECK` enabled), and whether they pass.
        prearm: Option<bool>,
    },
    VfrHud {
        /// Metres per second.
        airspeed: f32,
        groundspeed: f32,
        /// Degrees.
        heading: i16,
        /// Percent.
        throttle: u16,
        /// Metres per second.
        climb: f32,
    },
    /// The first eight servo outputs, in microseconds; 0 when unused.
    ServoOutputs {
        servos: [u16; 8],
    },
    /// The mission item the vehicle is flying to.
    MissionCurrent {
        seq: u16,
    },
    /// A message from the vehicle, or one part of a long one (S26).
    StatusText {
        /// `MAV_SEVERITY`: 0 emergency to 7 debug.
        severity: u8,
        text: String,
        /// Which long message this part belongs to; 0 for a whole one.
        id: u16,
        chunk_seq: u8,
        /// Whether the text filled its 50 bytes, so more may follow.
        full: bool,
    },
    /// Where the navigation controller is taking the vehicle (S65).
    NavControllerOutput {
        /// Metres to the waypoint.
        wp_dist: u16,
        /// Metres off the line to it.
        xtrack_error: f32,
    },
    /// `RADIO_STATUS`: a telemetry radio's view of the link (S72). Signal
    /// and noise in the radio's own scale, 0 to 254; 255 is unknown.
    RadioStatus {
        rssi: u8,
        remrssi: u8,
        noise: u8,
        remnoise: u8,
        rxerrors: u16,
        fixed: u16,
    },
    /// `EKF_STATUS_REPORT` (S72): `EKF_STATUS_FLAGS` and the variances of
    /// velocity, horizontal and vertical position, compass and terrain.
    EkfStatus {
        flags: u16,
        variances: [f32; 5],
    },
    /// `VIBRATION` (S72): m/s² per axis and each accelerometer's clipping count.
    Vibration {
        axes: [f32; 3],
        clipping: [u32; 3],
    },
    /// `BATTERY_STATUS` (S72): mAh used (-1 unknown), cells in mV
    /// (`u16::MAX` unused) and centi-degrees (`i16::MAX` unknown).
    BatteryStatus {
        id: u8,
        consumed_mah: i32,
        cells_mv: [u16; 10],
        temperature_cdeg: i16,
    },
    /// `DISTANCE_SENSOR` (S72): centimetres, and `MAV_SENSOR_ORIENTATION`
    /// (25 is down).
    DistanceSensor {
        distance_cm: u16,
        orientation: u8,
    },
    /// ArduPilot's `RANGEFINDER` (S72): metres.
    Rangefinder {
        distance: f32,
    },
    /// ArduPilot's `TERRAIN_REPORT`: the terrain's height above mean sea
    /// level under the vehicle and the vehicle's height above it, in
    /// metres, from the autopilot's terrain data. `spacing` is 0 when it
    /// has none for where it is, and the heights mean nothing then.
    TerrainReport {
        /// Where it reports for, 1e-7 degrees: the vehicle, or a point a
        /// `TERRAIN_CHECK` asked about (S119).
        lat: i32,
        lon: i32,
        spacing: u16,
        terrain_height: f32,
        current_height: f32,
        /// Terrain tiles still to fetch, and loaded.
        pending: u16,
        loaded: u16,
    },
    /// `FENCE_STATUS` (S132): whether the vehicle is outside its fence now,
    /// and how many breaches since it booted.
    FenceStatus {
        breached: bool,
        count: u16,
    },
    /// `EXTENDED_SYS_STATE` (S103): `MAV_LANDED_STATE`, 0 when the
    /// autopilot does not know, then on the ground, in the air, taking off,
    /// landing.
    ExtendedSysState {
        landed_state: u8,
    },
    /// ArduPilot's `WIND` (S72): where it comes from in degrees, m/s.
    Wind {
        direction: f32,
        speed: f32,
        speed_z: f32,
    },
    /// `SCALED_PRESSURE2` (S72): the second barometer's centi-degrees,
    /// which on a submarine is the water's.
    ScaledPressure2 {
        temperature_cdeg: i16,
    },
    /// `FILE_TRANSFER_PROTOCOL` (S74): MAVLink FTP, both ways.
    Ftp {
        target_system: u8,
        target_component: u8,
        payload: Box<[u8; 251]>,
    },
    /// `POSITION_TARGET_GLOBAL_INT`: where the autopilot is taking the
    /// vehicle, as ArduPilot sends it in GUIDED: degrees times 1e7, and
    /// `POSITION_TARGET_TYPEMASK` (bits 0 to 2 set: no position).
    PositionTarget {
        latitude: i32,
        longitude: i32,
        /// Metres in `coordinate_frame`, not the vehicle's measured altitude.
        altitude: f32,
        coordinate_frame: u8,
        type_mask: u16,
    },
    /// Where home is: degrees times 1e7, millimetres above mean sea level.
    HomePosition {
        latitude: i32,
        longitude: i32,
        altitude: i32,
    },
    /// A command; the daemon sends these and a vehicle answers them.
    CommandLong {
        target_system: u8,
        /// `MAV_CMD`.
        command: u16,
        param1: f32,
        param2: f32,
        param7: f32,
    },
    /// A command with a place: `DO_REPOSITION` from Go to.
    CommandInt {
        target_system: u8,
        /// `MAV_CMD`.
        command: u16,
        /// `MAV_FRAME`.
        frame: u8,
        param1: f32,
        param2: f32,
        /// Latitude, degrees times 1e7.
        x: i32,
        /// Longitude, degrees times 1e7.
        y: i32,
        /// The height in the frame.
        z: f32,
    },
    /// One parameter, in answer to a request or after a change. An index of
    /// `u16::MAX` means it was not part of a list.
    ParamValue {
        name: String,
        value: f32,
        /// `MAV_PARAM_TYPE`.
        param_type: u8,
        count: u16,
        index: u16,
    },
    /// The daemon's requests, decoded for the fake vehicle.
    ParamRequestList,
    /// By name, or by index when the name is empty.
    ParamRequestRead {
        name: String,
        index: i16,
    },
    ParamSet {
        name: String,
        value: f32,
        param_type: u8,
    },
    MissionRequestList {
        mission_type: u8,
    },
    MissionCount {
        count: u16,
        mission_type: u8,
    },
    /// `MISSION_REQUEST_INT`, or the older `MISSION_REQUEST`.
    MissionRequest {
        seq: u16,
        mission_type: u8,
    },
    MissionItem(MissionItemInt),
    /// Sticks from a ground station: x pitch, y roll, z throttle (0 to
    /// 1000), r yaw.
    ManualControl {
        target: u8,
        sticks: [i16; 4],
    },
    RcChannelsOverride {
        target: u8,
        channels: [u16; 18],
    },
    MissionAck {
        /// `MAV_MISSION_RESULT`; 0 is accepted.
        result: u8,
        mission_type: u8,
    },
    /// `TERRAIN_REQUEST` (S118): a block of the vehicle's terrain grid, by
    /// its south-west corner (1e-7 degrees) and spacing in metres, and the
    /// 4 × 4 groups of it the vehicle lacks.
    TerrainRequest {
        lat: i32,
        lon: i32,
        grid_spacing: u16,
        mask: u64,
    },
    /// `TERRAIN_CHECK`: a ground station asks for the vehicle's terrain
    /// height at a point; the fake vehicle answers (S119).
    TerrainCheck {
        lat: i32,
        lon: i32,
    },
    /// `TERRAIN_DATA`: one group of a block's heights, from a ground
    /// station; the fake vehicle keeps it (S118).
    TerrainData {
        lat: i32,
        lon: i32,
        grid_spacing: u16,
        gridbit: u8,
        data: [i16; 16],
    },
    /// A camera's stream (`VIDEO_STREAM_INFORMATION`, S39).
    VideoStream {
        stream_id: u8,
        /// `VIDEO_STREAM_TYPE`: 0 RTSP, 1 RTP over UDP, 2 MPEG-TS over TCP,
        /// 3 MPEG-TS over UDP.
        kind: u8,
        uri: String,
        name: String,
    },
    Other {
        message_id: u32,
    },
}

/// A mission item as MAVLink carries it (`MISSION_ITEM_INT`).
#[derive(Clone, Debug, PartialEq)]
pub struct MissionItemInt {
    pub seq: u16,
    /// `MAV_FRAME`.
    pub frame: u8,
    /// `MAV_CMD`.
    pub command: u16,
    pub current: bool,
    pub autocontinue: bool,
    pub params: [f32; 4],
    /// Degrees times 1e7 in global frames.
    pub x: i32,
    pub y: i32,
    pub z: f32,
    pub mission_type: u8,
}

/// A parameter name from its 16-byte, NUL-padded field.
#[must_use]
pub fn param_name(id: &[u8; 16]) -> String {
    text(id)
}

/// A NUL-padded string field.
/// One mode of a vehicle's own list, its name read to the first NUL.
fn available_modes(m: &mavio::dialects::ardupilotmega::messages::AvailableModes) -> Message {
    Message::AvailableModes {
        number_modes: m.number_modes,
        mode_index: m.mode_index,
        custom_mode: m.custom_mode,
        properties: u32::from(m.properties.bits()),
        name: text(&m.mode_name),
    }
}

fn text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Parses and decodes one frame's bytes. `None` when the bytes are not a
/// frame, or its checksum does not match a message the dialect knows.
#[must_use]
#[allow(clippy::too_many_lines, reason = "one arm per message")]
pub fn decode(bytes: &[u8]) -> Option<Decoded> {
    let mut receiver = Receiver::new::<Versionless>(StdIoReader::new(bytes));
    let frame: Frame<Versionless> = receiver.recv().ok()?;
    let (system_id, component_id) = (frame.system_id(), frame.component_id());
    let message_id = frame.message_id();
    if frame.validate_checksum::<Ardupilotmega>().is_err() {
        return None;
    }
    let message = match frame.decode::<Ardupilotmega>() {
        Ok(Ardupilotmega::Heartbeat(m)) => Message::Heartbeat {
            mav_type: format!("{:?}", m.type_),
            autopilot: format!("{:?}", m.autopilot),
            system_status: format!("{:?}", m.system_status),
            base_mode: m.base_mode.bits(),
            custom_mode: m.custom_mode,
        },
        Ok(Ardupilotmega::CommandAck(m)) => Message::CommandAck {
            command: m.command as u16,
            result: m.result as u8,
            target_system: m.target_system,
            target_component: m.target_component,
        },
        Ok(Ardupilotmega::AutopilotVersion(m)) => Message::AutopilotVersion {
            uid: m.uid,
            uid2: m.uid2,
            flight_sw_version: m.flight_sw_version,
            flight_custom_version: m.flight_custom_version,
        },
        Ok(Ardupilotmega::AvailableModes(m)) => available_modes(&m),
        Ok(Ardupilotmega::AvailableModesMonitor(m)) => {
            Message::AvailableModesMonitor { seq: m.seq }
        }
        Ok(Ardupilotmega::TerrainCheck(m)) => Message::TerrainCheck {
            lat: m.lat,
            lon: m.lon,
        },
        Ok(Ardupilotmega::TerrainData(m)) => Message::TerrainData {
            lat: m.lat,
            lon: m.lon,
            grid_spacing: m.grid_spacing,
            gridbit: m.gridbit,
            data: m.data,
        },
        Ok(Ardupilotmega::TerrainRequest(m)) => Message::TerrainRequest {
            lat: m.lat,
            lon: m.lon,
            grid_spacing: m.grid_spacing,
            mask: m.mask,
        },
        Ok(Ardupilotmega::VideoStreamInformation(m)) => Message::VideoStream {
            stream_id: m.stream_id,
            kind: m.type_ as u8,
            uri: text(&m.uri),
            name: text(&m.name),
        },
        Ok(Ardupilotmega::GlobalPositionInt(m)) => Message::GlobalPositionInt {
            time_boot_ms: m.time_boot_ms,
            lat: m.lat,
            lon: m.lon,
            alt: m.alt,
            relative_alt: m.relative_alt,
            vx: m.vx,
            vy: m.vy,
            vz: m.vz,
            hdg: m.hdg,
        },
        Ok(Ardupilotmega::GpsRawInt(m)) => Message::GpsRawInt {
            fix_type: m.fix_type as u8,
            lat: m.lat,
            lon: m.lon,
            alt: m.alt,
            eph: m.eph,
            epv: m.epv,
            vel: m.vel,
            cog: m.cog,
            satellites_visible: m.satellites_visible,
            alt_ellipsoid: m.alt_ellipsoid,
            h_acc: m.h_acc,
            v_acc: m.v_acc,
        },
        Ok(Ardupilotmega::Attitude(m)) => Message::Attitude {
            time_boot_ms: m.time_boot_ms,
            roll: m.roll,
            pitch: m.pitch,
            yaw: m.yaw,
            rollspeed: m.rollspeed,
            pitchspeed: m.pitchspeed,
            yawspeed: m.yawspeed,
        },
        Ok(Ardupilotmega::SysStatus(m)) => Message::SysStatus {
            voltage_battery: m.voltage_battery,
            current_battery: m.current_battery,
            battery_remaining: m.battery_remaining,
            prearm: prearm(
                m.onboard_control_sensors_enabled.bits(),
                m.onboard_control_sensors_health.bits(),
            ),
        },
        Ok(Ardupilotmega::VfrHud(m)) => Message::VfrHud {
            airspeed: m.airspeed,
            groundspeed: m.groundspeed,
            heading: m.heading,
            throttle: m.throttle,
            climb: m.climb,
        },
        Ok(other) => command(&other)
            .or_else(|| transfer(other))
            .unwrap_or(Message::Other { message_id }),
        Err(_) => Message::Other { message_id },
    };
    Some(Decoded {
        system_id,
        component_id,
        message,
    })
}

/// The commands the daemon sends, seen on the way out (the inspector, the
/// fake vehicle): a `COMMAND_LONG`, or a `COMMAND_INT` with a place.
fn command(message: &Ardupilotmega) -> Option<Message> {
    Some(match message {
        Ardupilotmega::CommandLong(m) => Message::CommandLong {
            target_system: m.target_system,
            command: m.command as u16,
            param1: m.param1,
            param2: m.param2,
            param7: m.param7,
        },
        Ardupilotmega::CommandInt(m) => Message::CommandInt {
            target_system: m.target_system,
            command: m.command as u16,
            frame: m.frame as u8,
            param1: m.param1,
            param2: m.param2,
            x: m.x,
            y: m.y,
            z: m.z,
        },
        _ => return None,
    })
}

/// The parameter and mission protocol messages.
/// The health and environment messages (S72).
fn health(message: &Ardupilotmega) -> Option<Message> {
    Some(match message {
        Ardupilotmega::RadioStatus(m) => Message::RadioStatus {
            rssi: m.rssi,
            remrssi: m.remrssi,
            noise: m.noise,
            remnoise: m.remnoise,
            rxerrors: m.rxerrors,
            fixed: m.fixed,
        },
        Ardupilotmega::EkfStatusReport(m) => Message::EkfStatus {
            flags: m.flags.bits(),
            variances: [
                m.velocity_variance,
                m.pos_horiz_variance,
                m.pos_vert_variance,
                m.compass_variance,
                m.terrain_alt_variance,
            ],
        },
        Ardupilotmega::Vibration(m) => Message::Vibration {
            axes: [m.vibration_x, m.vibration_y, m.vibration_z],
            clipping: [m.clipping_0, m.clipping_1, m.clipping_2],
        },
        Ardupilotmega::BatteryStatus(m) => Message::BatteryStatus {
            id: m.id,
            consumed_mah: m.current_consumed,
            cells_mv: m.voltages,
            temperature_cdeg: m.temperature,
        },
        Ardupilotmega::DistanceSensor(m) => Message::DistanceSensor {
            distance_cm: m.current_distance,
            orientation: m.orientation as u8,
        },
        Ardupilotmega::Rangefinder(m) => Message::Rangefinder {
            distance: m.distance,
        },
        Ardupilotmega::TerrainReport(m) => Message::TerrainReport {
            lat: m.lat,
            lon: m.lon,
            spacing: m.spacing,
            terrain_height: m.terrain_height,
            current_height: m.current_height,
            pending: m.pending,
            loaded: m.loaded,
        },
        Ardupilotmega::FenceStatus(m) => Message::FenceStatus {
            breached: m.breach_status != 0,
            count: m.breach_count,
        },
        Ardupilotmega::ExtendedSysState(m) => Message::ExtendedSysState {
            landed_state: m.landed_state as u8,
        },
        Ardupilotmega::Wind(m) => Message::Wind {
            direction: m.direction,
            speed: m.speed,
            speed_z: m.speed_z,
        },
        Ardupilotmega::ScaledPressure2(m) => Message::ScaledPressure2 {
            temperature_cdeg: m.temperature,
        },
        _ => return None,
    })
}

fn transfer(message: Ardupilotmega) -> Option<Message> {
    match message {
        Ardupilotmega::ParamValue(m) => Some(Message::ParamValue {
            name: param_name(&m.param_id),
            value: m.param_value,
            param_type: m.param_type as u8,
            count: m.param_count,
            index: m.param_index,
        }),
        Ardupilotmega::ParamRequestList(_) => Some(Message::ParamRequestList),
        Ardupilotmega::ParamRequestRead(m) => Some(Message::ParamRequestRead {
            name: param_name(&m.param_id),
            index: m.param_index,
        }),
        Ardupilotmega::FileTransferProtocol(m) => Some(Message::Ftp {
            target_system: m.target_system,
            target_component: m.target_component,
            payload: Box::new(m.payload),
        }),
        Ardupilotmega::ParamSet(m) => Some(Message::ParamSet {
            name: param_name(&m.param_id),
            value: m.param_value,
            param_type: m.param_type as u8,
        }),
        Ardupilotmega::MissionRequestList(m) => Some(Message::MissionRequestList {
            mission_type: m.mission_type as u8,
        }),
        Ardupilotmega::MissionCount(m) => Some(Message::MissionCount {
            count: m.count,
            mission_type: m.mission_type as u8,
        }),
        Ardupilotmega::MissionRequestInt(m) => Some(Message::MissionRequest {
            seq: m.seq,
            mission_type: m.mission_type as u8,
        }),
        Ardupilotmega::MissionRequest(m) => Some(Message::MissionRequest {
            seq: m.seq,
            mission_type: m.mission_type as u8,
        }),
        Ardupilotmega::MissionItemInt(m) => Some(Message::MissionItem(MissionItemInt {
            seq: m.seq,
            frame: m.frame as u8,
            command: m.command as u16,
            current: m.current != 0,
            autocontinue: m.autocontinue != 0,
            params: [m.param1, m.param2, m.param3, m.param4],
            x: m.x,
            y: m.y,
            z: m.z,
            mission_type: m.mission_type as u8,
        })),
        Ardupilotmega::ManualControl(m) => Some(Message::ManualControl {
            target: m.target,
            sticks: [m.x, m.y, m.z, m.r],
        }),
        Ardupilotmega::RcChannelsOverride(m) => Some(rc_override(&m)),
        Ardupilotmega::MissionAck(m) => Some(Message::MissionAck {
            result: m.type_ as u8,
            mission_type: m.mission_type as u8,
        }),
        Ardupilotmega::MissionCurrent(m) => Some(Message::MissionCurrent { seq: m.seq }),
        Ardupilotmega::Statustext(m) => {
            let end = m.text.iter().position(|&b| b == 0).unwrap_or(m.text.len());
            Some(Message::StatusText {
                severity: m.severity as u8,
                text: String::from_utf8_lossy(&m.text[..end]).into_owned(),
                id: m.id,
                chunk_seq: m.chunk_seq,
                full: end == m.text.len(),
            })
        }
        Ardupilotmega::NavControllerOutput(m) => Some(Message::NavControllerOutput {
            wp_dist: m.wp_dist,
            xtrack_error: m.xtrack_error,
        }),
        Ardupilotmega::PositionTargetGlobalInt(m) => Some(Message::PositionTarget {
            latitude: m.lat_int,
            longitude: m.lon_int,
            altitude: m.alt,
            coordinate_frame: m.coordinate_frame as u8,
            type_mask: m.type_mask.bits(),
        }),
        Ardupilotmega::HomePosition(m) => Some(Message::HomePosition {
            latitude: m.latitude,
            longitude: m.longitude,
            altitude: m.altitude,
        }),
        Ardupilotmega::ServoOutputRaw(m) => Some(Message::ServoOutputs {
            servos: [
                m.servo1_raw,
                m.servo2_raw,
                m.servo3_raw,
                m.servo4_raw,
                m.servo5_raw,
                m.servo6_raw,
                m.servo7_raw,
                m.servo8_raw,
            ],
        }),
        other => health(&other),
    }
}

/// Whether whole-frame bytes are a frame of the dialect (for resynchronising
/// byte streams).
#[must_use]
pub fn check(bytes: &[u8]) -> crate::framing::Check {
    use crate::framing::Check;
    use mavio::protocol::Dialect;
    let mut receiver = Receiver::new::<Versionless>(StdIoReader::new(bytes));
    let Ok(frame) = receiver.recv() else {
        return Check::Invalid;
    };
    if Ardupilotmega::message_info(frame.message_id()).is_err() {
        return Check::Unknown;
    }
    if frame.validate_checksum::<Ardupilotmega>().is_ok() {
        Check::Valid
    } else {
        Check::Invalid
    }
}

/// `flight_sw_version` as `major.minor.patch`.
#[must_use]
pub fn firmware_version(flight_sw_version: u32) -> String {
    let [major, minor, patch, _] = flight_sw_version.to_be_bytes();
    format!("{major}.{minor}.{patch}")
}

/// `flight_custom_version` as the git hash it holds (ArduPilot copies the
/// first 8 characters of the hash's text), if it is one.
#[must_use]
pub fn firmware_commit(flight_custom_version: [u8; 8]) -> Option<String> {
    let text = std::str::from_utf8(&flight_custom_version).ok()?;
    (text.bytes().all(|b| b.is_ascii_hexdigit()) && text.bytes().any(|b| b != b'0'))
        .then(|| text.to_ascii_lowercase())
}

/// `MAV_SYS_STATUS_PREARM_CHECK`: ArduPilot sets it enabled when arming
/// checks are on, and healthy when they pass or it is armed.
const PREARM_CHECK: u32 = 0x1000_0000;

fn prearm(enabled: u32, health: u32) -> Option<bool> {
    (enabled & PREARM_CHECK != 0).then_some(health & PREARM_CHECK != 0)
}

fn rc_override(m: &mavio::dialects::ardupilotmega::messages::RcChannelsOverride) -> Message {
    Message::RcChannelsOverride {
        target: m.target_system,
        channels: [
            m.chan1_raw,
            m.chan2_raw,
            m.chan3_raw,
            m.chan4_raw,
            m.chan5_raw,
            m.chan6_raw,
            m.chan7_raw,
            m.chan8_raw,
            m.chan9_raw,
            m.chan10_raw,
            m.chan11_raw,
            m.chan12_raw,
            m.chan13_raw,
            m.chan14_raw,
            m.chan15_raw,
            m.chan16_raw,
            m.chan17_raw,
            m.chan18_raw,
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim;

    #[test]
    fn decodes_a_heartbeat_and_an_autopilot_version() {
        let heartbeat =
            decode(&sim::heartbeat_frame(7, 1, 0)).unwrap_or_else(|| panic!("heartbeat"));
        assert_eq!(heartbeat.system_id, 7);
        assert!(
            matches!(&heartbeat.message, Message::Heartbeat { mav_type, autopilot, .. }
            if mav_type == "Quadrotor" && autopilot == "Ardupilotmega")
        );
        let version = decode(&sim::autopilot_version_frame(7, 1, 0, 0x1234, 0x0405_01ff))
            .unwrap_or_else(|| panic!("version"));
        assert!(matches!(
            version.message,
            Message::AutopilotVersion { uid: 0x1234, .. }
        ));
        assert_eq!(firmware_version(0x0405_01ff), "4.5.1");
    }

    /// A frame of any message, as `sim::frame` builds one.
    fn frame_of(system_id: u8, sequence: u8, message: &dyn mavio::Message) -> Vec<u8> {
        use mavio::protocol::V2;
        let Ok(builder) = Frame::builder()
            .version(V2)
            .system_id(system_id)
            .component_id(1)
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

    #[test]
    fn a_guided_target_keeps_its_commanded_altitude_and_frame() {
        use mavio::dialects::ardupilotmega::enums::{MavFrame, PositionTargetTypemask};
        use mavio::dialects::ardupilotmega::messages::PositionTargetGlobalInt;

        use crate::producers::{Navigation, Produced, Producer};

        for (frame, name) in [
            (MavFrame::Global, "amsl"),
            (MavFrame::GlobalInt, "amsl"),
            (MavFrame::GlobalRelativeAlt, "above-home"),
            (MavFrame::GlobalRelativeAltInt, "above-home"),
            (MavFrame::GlobalTerrainAlt, "above-ground"),
            (MavFrame::GlobalTerrainAltInt, "above-ground"),
        ] {
            let target = PositionTargetGlobalInt {
                coordinate_frame: frame,
                type_mask: PositionTargetTypemask::from_bits_truncate(0xfff8),
                lat_int: -353_620_000,
                lon_int: 1_491_660_000,
                alt: 80.0,
                ..PositionTargetGlobalInt::default()
            };
            let decoded = decode(&frame_of(7, 1, &target)).unwrap_or_else(|| panic!("target"));
            let mut out = Produced::default();
            Navigation.consume(1, &decoded.message, 5, &mut out);
            assert_eq!(out.items[2].key, "navigation.target_altitude_m");
            assert_eq!(out.items[2].value, Some(80.0));
            assert_eq!(out.items[3].text.as_deref(), Some(name));
        }
    }

    #[test]
    fn an_ignored_or_invalid_target_altitude_clears_the_previous_height() {
        use crate::producers::{Navigation, Produced, Producer};

        for (altitude, coordinate_frame, type_mask) in [
            (80.0, 6, 0xfffc),
            (f32::NAN, 6, 0xfff8),
            (f32::INFINITY, 6, 0xfff8),
            (80.0, 1, 0xfff8),
        ] {
            let mut out = Produced::default();
            Navigation.consume(
                1,
                &Message::PositionTarget {
                    latitude: -353_620_000,
                    longitude: 1_491_660_000,
                    altitude,
                    coordinate_frame,
                    type_mask,
                },
                5,
                &mut out,
            );
            assert_eq!(out.items.len(), 4, "horizontal target remains");
            assert_eq!(out.items[2].value, None, "no invented altitude");
        }
    }

    #[test]
    fn decodes_a_vehicles_own_list_of_modes() {
        use mavio::dialects::ardupilotmega::enums::{MavModeProperty, MavStandardMode};
        use mavio::dialects::ardupilotmega::messages::{AvailableModes, AvailableModesMonitor};
        let mut mode_name = [0u8; 35];
        mode_name[..13].copy_from_slice(b"FLY_BY_WIRE_A");
        let one = AvailableModes {
            number_modes: 3,
            mode_index: 2,
            standard_mode: MavStandardMode::NonStandard,
            custom_mode: 5,
            properties: MavModeProperty::from_bits_retain(1u8),
            mode_name,
        };
        let decoded = decode(&frame_of(7, 0, &one)).unwrap_or_else(|| panic!("modes"));
        assert_eq!(
            decoded.message,
            Message::AvailableModes {
                number_modes: 3,
                mode_index: 2,
                custom_mode: 5,
                properties: 1,
                name: "FLY_BY_WIRE_A".into(),
            }
        );
        let monitor = AvailableModesMonitor { seq: 4 };
        let decoded = decode(&frame_of(7, 1, &monitor)).unwrap_or_else(|| panic!("seq"));
        assert_eq!(decoded.message, Message::AvailableModesMonitor { seq: 4 });
    }

    #[test]
    fn a_corrupted_frame_is_refused() {
        let mut frame = sim::heartbeat_frame(7, 1, 0);
        let last = frame.len() - 1;
        frame[last] ^= 0xFF;
        assert!(decode(&frame).is_none());
    }

    #[test]
    fn the_commit_is_read_from_flight_custom_version() {
        assert_eq!(firmware_commit(*b"2a3dc4b7").as_deref(), Some("2a3dc4b7"));
        assert_eq!(firmware_commit([0; 8]), None, "not sent");
        assert_eq!(firmware_commit(*b"00000000"), None);
        assert_eq!(firmware_commit(*b"not hex!"), None);
    }
}
