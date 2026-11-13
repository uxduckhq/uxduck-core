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
    /// Destination of a mission transfer packet; zero means broadcast.
    pub mission_target: Option<(u8, u8)>,
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
    /// Body FRD attitude in local NED; quaternion order w, x, y, z.
    AttitudeQuaternion {
        time_boot_ms: u32,
        q: [f32; 4],
        rollspeed: f32,
        pitchspeed: f32,
        yawspeed: f32,
        repr_offset_q: [f32; 4],
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
    /// Servo outputs 1 to 16 (`SERVO_OUTPUT_RAW` port 0), in microseconds;
    /// 0 when unused, and for 9 to 16 when the frame came without its
    /// extension fields. `time_us` is the autopilot's boot time of the
    /// sample, which a simulator's pause also stops.
    ServoOutputs {
        servos: [u16; 16],
        time_us: u32,
    },
    /// The receiver's channels as the autopilot reads them (`RC_CHANNELS`),
    /// in microseconds, before any reversal: the first `count` are given,
    /// `UINT16_MAX` for one unused (S187).
    RcChannels {
        /// Device-dependent signal strength, 255 when unknown.
        rssi: u8,
        count: u8,
        channels: [u16; 18],
    },
    /// The mission item the vehicle is flying to, and what it holds: the
    /// mission's item count without home, and the ids of its mission, fence
    /// and rally points (S199). Each is 0 when the autopilot does not say,
    /// or the frame came without its extension fields.
    MissionCurrent {
        seq: u16,
        state: u8,
        mode: u8,
        total: u16,
        mission_id: u32,
        fence_id: u32,
        rally_id: u32,
    },
    /// `MISSION_ITEM_REACHED`: the mission item it has just reached (S342).
    MissionItemReached {
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
        /// Free transmit buffer, percent: how much more it can take.
        txbuf: u8,
    },
    /// `EKF_STATUS_REPORT` (S72): `EKF_STATUS_FLAGS` and the variances of
    /// velocity, horizontal and vertical position, compass and terrain.
    EkfStatus {
        flags: u16,
        variances: [f32; 5],
    },
    /// PX4's `ESTIMATOR_STATUS`: its flags as `EKF_STATUS_FLAGS`, and its
    /// innovation test ratios for velocity, horizontal and vertical position,
    /// compass and height above ground.
    EstimatorStatus {
        flags: u16,
        ratios: [f32; 5],
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
        min_distance_cm: u16,
        max_distance_cm: u16,
        signal_quality: u8,
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
    /// ArduPilot's `WIND` (S72): where it comes from in degrees, m/s. PX4's
    /// `WIND_COV` reads into it too.
    Wind {
        direction: f32,
        speed: f32,
        speed_z: f32,
    },
    /// ArduPilot's `AIRSPEED_AUTOCAL` (S190): its automatic airspeed
    /// calibration's estimate of the ratio, sent while it runs in flight.
    AirspeedAutocal {
        ratio: f32,
    },
    /// `SCALED_PRESSURE2` (S72): the second barometer's centi-degrees,
    /// which on a submarine is the water's.
    ScaledPressure2 {
        temperature_cdeg: i16,
    },
    /// `ONBOARD_COMPUTER_STATUS` (S295): a companion computer's health.
    OnboardComputerStatus(Box<crate::computer::ComputerStatus>),
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
    /// `GPS_GLOBAL_ORIGIN`: the EKF origin, in `HomePosition`'s units.
    GpsGlobalOrigin {
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
        /// `FIXED_MAG_CAL_YAW`'s latitude and longitude (S185).
        param3: f32,
        param4: f32,
        /// `PREFLIGHT_CALIBRATION`'s accelerometer (S186).
        param5: f32,
        /// `PREFLIGHT_CALIBRATION`'s airspeed (S190, in the fake vehicle).
        param6: f32,
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
        target_system: u8,
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
        /// Held button bits: `buttons`, then `buttons2` from bit 16.
        buttons: u32,
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
    /// Where a gimbal points (`GIMBAL_DEVICE_ATTITUDE_STATUS`): its
    /// attitude as a quaternion, w x y z (NaN when it has none), and
    /// `GIMBAL_DEVICE_FLAGS`, which say whether the yaw in it is from the
    /// vehicle's nose or from north.
    GimbalAttitude {
        flags: u16,
        q: [f32; 4],
    },
    /// The older `MOUNT_STATUS`: pitch and yaw from the nose, centidegrees.
    MountStatus {
        pitch_cdeg: i32,
        yaw_cdeg: i32,
    },
    /// `ADSB_VEHICLE` (S168): an aircraft the vehicle's ADS-B receiver
    /// heard. Degrees ×1e7, millimetres, centidegrees and cm/s; `flags` is
    /// `ADSB_FLAGS`, saying which fields hold something.
    AdsbVehicle {
        icao_address: u32,
        lat: i32,
        lon: i32,
        /// `ADSB_ALTITUDE_TYPE`: 0 pressure (QNH), 1 geometric.
        altitude_type: u8,
        altitude: i32,
        heading: u16,
        hor_velocity: u16,
        ver_velocity: i16,
        callsign: String,
        /// `ADSB_EMITTER_TYPE`.
        emitter_type: u8,
        /// Seconds since the receiver last heard it.
        tslc: u8,
        flags: u16,
        squawk: u16,
    },
    /// ArduPilot's `MAG_CAL_PROGRESS` (S184): one compass's calibration as
    /// it runs. `cal_status` is `MAG_CAL_STATUS`; `completion_mask` holds
    /// the 80 sections of the sphere seen so far; `direction` is where to
    /// turn the vehicle next, in its body frame.
    MagCalProgress {
        compass_id: u8,
        cal_mask: u8,
        cal_status: u8,
        attempt: u8,
        completion_pct: u8,
        completion_mask: [u8; 10],
        direction: [f32; 3],
    },
    /// `MAG_CAL_REPORT` (S184): one compass's result, sent until the
    /// vehicle restarts. `fitness` and the offsets in milligauss.
    MagCalReport {
        compass_id: u8,
        cal_mask: u8,
        cal_status: u8,
        autosaved: bool,
        fitness: f32,
        offsets: [f32; 3],
    },
    /// MAVLink's ping (`TIMESYNC`, S261): a request has `tc1` 0 and the
    /// asker's clock in `ts1`; the reply has the answerer's clock in `tc1`
    /// and `ts1` echoed, addressed to the asker when the extension is sent.
    Timesync {
        tc1: i64,
        ts1: i64,
        target_system: u8,
        target_component: u8,
    },
    /// What one rate controller was asked and did (`PID_TUNING`), sent
    /// for the axes in `GCS_PID_MASK`: rates in degrees per second, terms
    /// in the controller's output units. `axis` is `PID_TUNING_AXIS`.
    PidTuning {
        axis: u8,
        desired: f32,
        achieved: f32,
        ff: f32,
        p: f32,
        i: f32,
        d: f32,
    },
    /// The body rates the attitude controller asks for
    /// (`ATTITUDE_TARGET`), in radians per second; `type_mask` bits 1, 2
    /// and 4 say a rate is not given.
    AttitudeTarget {
        time_boot_ms: u32,
        type_mask: u8,
        body_roll_rate: f32,
        body_pitch_rate: f32,
        body_yaw_rate: f32,
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
    let packet = frame.decode::<Ardupilotmega>();
    // A fork may send a standard message with extensions ArduPilot's has
    // not (ADR-0044): its checksum is the same, and UXDuck reads the fields
    // it knows, as MAVLink asks of a receiver.
    if let Err(mavio::error::Error::Spec(mavio::error::SpecError::V2PayloadIsTooLong {
        maximum,
        ..
    })) = packet
    {
        let extra = <Ardupilotmega as mavio::protocol::Dialect>::message_info(message_id)
            .ok()?
            .crc_extra();
        let header = (system_id, component_id, frame.sequence());
        let payload = frame.payload().bytes().get(..maximum)?;
        return decode(&crate::custom::frame_from(
            header, message_id, payload, extra,
        ));
    }
    // A `MAV_CMD` or `MAV_FRAME` newer than mavio's dialect does not decode:
    // read the item or the acknowledgement from its bytes.
    if packet.is_err() && matches!(message_id, MISSION_ITEM_INT | COMMAND_ACK) {
        let mut bytes = [0u8; 38];
        let payload = frame.payload().bytes();
        let length = payload.len().min(bytes.len());
        bytes[..length].copy_from_slice(&payload[..length]);
        return Some(raw_command(system_id, component_id, message_id, &bytes));
    }
    let mission_target = packet.as_ref().ok().and_then(|packet| match packet {
        Ardupilotmega::MissionCount(m) => Some((m.target_system, m.target_component)),
        Ardupilotmega::MissionItemInt(m) => Some((m.target_system, m.target_component)),
        Ardupilotmega::MissionRequest(m) => Some((m.target_system, m.target_component)),
        Ardupilotmega::MissionRequestInt(m) => Some((m.target_system, m.target_component)),
        Ardupilotmega::MissionAck(m) => Some((m.target_system, m.target_component)),
        _ => None,
    });
    let message = match packet {
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
        Ok(Ardupilotmega::GimbalDeviceAttitudeStatus(m)) => Message::GimbalAttitude {
            flags: m.flags.bits(),
            q: m.q,
        },
        Ok(Ardupilotmega::MountStatus(m)) => Message::MountStatus {
            pitch_cdeg: m.pointing_a,
            yaw_cdeg: m.pointing_c,
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
        Ok(Ardupilotmega::PidTuning(m)) => Message::PidTuning {
            axis: m.axis as u8,
            desired: m.desired,
            achieved: m.achieved,
            ff: m.ff,
            p: m.p,
            i: m.i,
            d: m.d,
        },
        Ok(Ardupilotmega::AttitudeTarget(m)) => Message::AttitudeTarget {
            time_boot_ms: m.time_boot_ms,
            type_mask: m.type_mask.bits(),
            body_roll_rate: m.body_roll_rate,
            body_pitch_rate: m.body_pitch_rate,
            body_yaw_rate: m.body_yaw_rate,
        },
        Ok(Ardupilotmega::AttitudeQuaternion(m)) => Message::AttitudeQuaternion {
            time_boot_ms: m.time_boot_ms,
            q: [m.q1, m.q2, m.q3, m.q4],
            rollspeed: m.rollspeed,
            pitchspeed: m.pitchspeed,
            yawspeed: m.yawspeed,
            repr_offset_q: m.repr_offset_q,
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
        Ok(Ardupilotmega::Timesync(m)) => Message::Timesync {
            tc1: m.tc1,
            ts1: m.ts1,
            target_system: m.target_system,
            target_component: m.target_component,
        },
        Ok(Ardupilotmega::VfrHud(m)) => Message::VfrHud {
            airspeed: m.airspeed,
            groundspeed: m.groundspeed,
            heading: m.heading,
            throttle: m.throttle,
            climb: m.climb,
        },
        Ok(Ardupilotmega::OnboardComputerStatus(m)) => Message::OnboardComputerStatus(Box::new(
            crate::computer::ComputerStatus::from_message(&m),
        )),
        Ok(other) => command(&other)
            .or_else(|| transfer(other))
            .unwrap_or(Message::Other { message_id }),
        // A `MAG_CAL_REPORT` with a status newer than the dialect's enum
        // (8 to 10: offsets, scaling, residuals) does not decode: read it
        // from its bytes (S185).
        Err(_) if message_id == MAG_CAL_REPORT => {
            mag_cal_report(frame.payload().bytes()).unwrap_or(Message::Other { message_id })
        }
        Err(_) => Message::Other { message_id },
    };
    Some(Decoded {
        system_id,
        component_id,
        mission_target,
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
            param3: m.param3,
            param4: m.param4,
            param5: m.param5,
            param6: m.param6,
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
/// PX4's estimator and wind, read into ArduPilot's EKF and wind rows.
fn px4_health(message: &Ardupilotmega) -> Option<Message> {
    Some(match message {
        // Its flags mean what ArduPilot's do up to the predicted positions,
        // and its GPS glitch is ArduPilot's, at another bit.
        Ardupilotmega::EstimatorStatus(m) => {
            const GPS_GLITCH: u16 = 1024;
            const EKF_GPS_GLITCHING: u16 = 32768;
            let flags = m.flags.bits();
            let glitching = if flags & GPS_GLITCH == 0 {
                0
            } else {
                EKF_GPS_GLITCHING
            };
            Message::EstimatorStatus {
                flags: (flags & 0x03ff) | glitching,
                ratios: [
                    m.vel_ratio,
                    m.pos_horiz_ratio,
                    m.pos_vert_ratio,
                    m.mag_ratio,
                    m.hagl_ratio,
                ],
            }
        }
        // Its wind, north, east and down, as ArduPilot's WIND says it: where
        // it comes from, as ArduPilot works it out, its speed and its down
        // speed.
        Ardupilotmega::WindCov(m) if m.wind_x.is_finite() && m.wind_y.is_finite() => {
            Message::Wind {
                direction: (-m.wind_y).atan2(-m.wind_x).to_degrees(),
                speed: m.wind_x.hypot(m.wind_y),
                speed_z: m.wind_z,
            }
        }
        _ => return None,
    })
}

fn health(message: &Ardupilotmega) -> Option<Message> {
    Some(match message {
        // The receiver's channels, for the radio calibration (S187).
        Ardupilotmega::RcChannels(m) => rc_channels(m),
        Ardupilotmega::RadioStatus(m) => Message::RadioStatus {
            rssi: m.rssi,
            remrssi: m.remrssi,
            noise: m.noise,
            remnoise: m.remnoise,
            rxerrors: m.rxerrors,
            fixed: m.fixed,
            txbuf: m.txbuf,
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
            min_distance_cm: m.min_distance,
            max_distance_cm: m.max_distance,
            signal_quality: m.signal_quality,
            orientation: m.orientation as u8,
        },
        Ardupilotmega::Rangefinder(m) => Message::Rangefinder {
            distance: m.distance,
        },
        Ardupilotmega::AdsbVehicle(m) => Message::AdsbVehicle {
            icao_address: m.icao_address,
            lat: m.lat,
            lon: m.lon,
            altitude_type: m.altitude_type as u8,
            altitude: m.altitude,
            heading: m.heading,
            hor_velocity: m.hor_velocity,
            ver_velocity: m.ver_velocity,
            callsign: text(&m.callsign),
            emitter_type: m.emitter_type as u8,
            tslc: m.tslc,
            flags: m.flags.bits(),
            squawk: m.squawk,
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
        Ardupilotmega::AirspeedAutocal(m) => Message::AirspeedAutocal { ratio: m.ratio },
        Ardupilotmega::ScaledPressure2(m) => Message::ScaledPressure2 {
            temperature_cdeg: m.temperature,
        },
        Ardupilotmega::MagCalProgress(m) => Message::MagCalProgress {
            compass_id: m.compass_id,
            cal_mask: m.cal_mask,
            cal_status: m.cal_status as u8,
            attempt: m.attempt,
            completion_pct: m.completion_pct,
            completion_mask: m.completion_mask,
            direction: [m.direction_x, m.direction_y, m.direction_z],
        },
        Ardupilotmega::MagCalReport(m) => Message::MagCalReport {
            compass_id: m.compass_id,
            cal_mask: m.cal_mask,
            cal_status: m.cal_status as u8,
            autosaved: m.autosaved != 0,
            fitness: m.fitness,
            offsets: [m.ofs_x, m.ofs_y, m.ofs_z],
        },
        _ => return None,
    })
}

/// `MAG_CAL_REPORT`'s message id.
const MAG_CAL_REPORT: u32 = 192;
const MISSION_ITEM_INT: u32 = 73;
const COMMAND_ACK: u32 = 77;

/// A `MISSION_ITEM_INT` or `COMMAND_ACK` read field by field from its
/// payload, zero-padded to its full length, in MAVLink's wire order.
fn raw_command(system_id: u8, component_id: u8, message_id: u32, bytes: &[u8; 38]) -> Decoded {
    let four = |at: usize| [bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]];
    let two = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    if message_id == COMMAND_ACK {
        return Decoded {
            system_id,
            component_id,
            mission_target: None,
            message: Message::CommandAck {
                command: two(0),
                result: bytes[2],
                target_system: bytes[8],
                target_component: bytes[9],
            },
        };
    }
    Decoded {
        system_id,
        component_id,
        mission_target: Some((bytes[32], bytes[33])),
        message: Message::MissionItem(MissionItemInt {
            seq: two(28),
            frame: bytes[34],
            command: two(30),
            current: bytes[35] != 0,
            autocontinue: bytes[36] != 0,
            params: [0, 4, 8, 12].map(|at| f32::from_le_bytes(four(at))),
            x: i32::from_le_bytes(four(16)),
            y: i32::from_le_bytes(four(20)),
            z: f32::from_le_bytes(four(24)),
            mission_type: bytes[37],
        }),
    }
}

/// A `MAG_CAL_REPORT` from its payload: ten floats (fitness, the offsets,
/// the diagonal and off-diagonal), then `compass_id`, `cal_mask`,
/// `cal_status` and `autosaved`; `MAVLink` 2 drops trailing zeros.
fn mag_cal_report(payload: &[u8]) -> Option<Message> {
    let mut bytes = [0u8; 44];
    let length = payload.len().min(bytes.len());
    bytes[..length].copy_from_slice(&payload[..length]);
    let float = |at: usize| {
        let mut four = [0u8; 4];
        four.copy_from_slice(&bytes[at..at + 4]);
        f32::from_le_bytes(four)
    };
    let cal_status = bytes[42];
    // Only what the enum lacks; anything else decoded already or is wrong.
    if !(8..=10).contains(&cal_status) {
        return None;
    }
    Some(Message::MagCalReport {
        compass_id: bytes[40],
        cal_mask: bytes[41],
        cal_status,
        autosaved: bytes[43] != 0,
        fitness: float(0),
        offsets: [float(4), float(8), float(12)],
    })
}

fn mission_current(m: &mavio::dialects::ardupilotmega::messages::MissionCurrent) -> Message {
    Message::MissionCurrent {
        seq: m.seq,
        state: m.mission_state as u8,
        mode: m.mission_mode,
        total: m.total,
        mission_id: m.mission_id,
        fence_id: m.fence_id,
        rally_id: m.rally_points_id,
    }
}

#[allow(clippy::too_many_lines, reason = "one arm per message")]
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
            target_system: m.target_system,
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
            buttons: u32::from(m.buttons) | (u32::from(m.buttons2) << 16),
        }),
        Ardupilotmega::RcChannelsOverride(m) => Some(rc_override(&m)),
        Ardupilotmega::MissionAck(m) => Some(Message::MissionAck {
            result: m.type_ as u8,
            mission_type: m.mission_type as u8,
        }),
        Ardupilotmega::MissionCurrent(m) => Some(mission_current(&m)),
        Ardupilotmega::MissionItemReached(m) => Some(Message::MissionItemReached { seq: m.seq }),
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
        Ardupilotmega::GpsGlobalOrigin(m) => Some(Message::GpsGlobalOrigin {
            latitude: m.latitude,
            longitude: m.longitude,
            altitude: m.altitude,
        }),
        // Port 1 carries outputs 17 to 32, which nothing here uses.
        Ardupilotmega::ServoOutputRaw(m) if m.port == 0 => Some(Message::ServoOutputs {
            servos: [
                m.servo1_raw,
                m.servo2_raw,
                m.servo3_raw,
                m.servo4_raw,
                m.servo5_raw,
                m.servo6_raw,
                m.servo7_raw,
                m.servo8_raw,
                m.servo9_raw,
                m.servo10_raw,
                m.servo11_raw,
                m.servo12_raw,
                m.servo13_raw,
                m.servo14_raw,
                m.servo15_raw,
                m.servo16_raw,
            ],
            time_us: m.time_usec,
        }),
        other => health(&other).or_else(|| px4_health(&other)),
    }
}

/// Whether whole-frame bytes are a frame of the dialect, or of a dialect a
/// person added (for resynchronising byte streams).
#[must_use]
pub fn check(bytes: &[u8]) -> crate::framing::Check {
    use crate::framing::Check;
    use mavio::protocol::Dialect;
    let mut receiver = Receiver::new::<Versionless>(StdIoReader::new(bytes));
    let Ok(frame) = receiver.recv() else {
        return Check::Invalid;
    };
    let known = Ardupilotmega::message_info(frame.message_id()).is_ok();
    if known && frame.validate_checksum::<Ardupilotmega>().is_ok() {
        return Check::Valid;
    }
    // A message a person's dialect adds or changes (ADR-0044 F4).
    if crate::custom::checksum_matches(bytes) {
        return Check::Valid;
    }
    if known {
        Check::Invalid
    } else {
        Check::Unknown
    }
}

/// Whether UXDuck's own readers act on a message of the built-in dialect
/// (ADR-0044 F5): a frame of it, all zeros or all ones, decodes to more than
/// [`Message::Other`].
#[must_use]
pub fn reads(id: u32) -> bool {
    use mavio::protocol::Dialect;
    let (Ok(info), Some(message)) = (Ardupilotmega::message_info(id), crate::dialect::message(id))
    else {
        return false;
    };
    let size: usize = message.fields().map(|f| f.size()).sum();
    [0_u8, 1].into_iter().any(|fill| {
        let frame = crate::custom::frame(id, &vec![fill; size], info.crc_extra());
        decode(&frame).is_some_and(|d| !matches!(d.message, Message::Other { .. }))
    })
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

/// `RC_CHANNELS` (S187).
const fn rc_channels(m: &mavio::dialects::ardupilotmega::messages::RcChannels) -> Message {
    Message::RcChannels {
        rssi: m.rssi,
        count: m.chancount,
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

    #[test]
    fn servo_outputs_decode_sixteen_from_port_0_only() {
        use mavio::dialects::ardupilotmega::messages::ServoOutputRaw;
        let raw = |port: u8| ServoOutputRaw {
            time_usec: 1_234_567,
            port,
            servo1_raw: 1100,
            servo11_raw: 1900,
            ..ServoOutputRaw::default()
        };
        let decoded = decode(&sim::frame(1, 1, 0, &raw(0))).map(|d| d.message);
        let mut servos = [0; 16];
        servos[0] = 1100;
        servos[10] = 1900;
        assert_eq!(
            decoded,
            Some(Message::ServoOutputs {
                servos,
                time_us: 1_234_567
            })
        );
        assert!(
            !matches!(
                decode(&sim::frame(1, 1, 0, &raw(1))).map(|d| d.message),
                Some(Message::ServoOutputs { .. })
            ),
            "port 1 is outputs 17 to 32"
        );
    }

    #[test]
    fn a_mag_cal_report_with_a_newer_failure_still_decodes() {
        // 9 (bad scaling) is past the dialect's MAG_CAL_STATUS enum.
        for status in [8u8, 9, 10] {
            let frame =
                sim::mag_cal_report_frame(1, 1, 0, 1, 3, status, 31.5, [42.5, -17.25, 63.0]);
            let decoded = decode(&frame).unwrap_or_else(|| panic!("report {status}"));
            assert_eq!(
                decoded.message,
                Message::MagCalReport {
                    compass_id: 1,
                    cal_mask: 3,
                    cal_status: status,
                    autosaved: false,
                    fitness: 31.5,
                    offsets: [42.5, -17.25, 63.0],
                },
            );
        }
        // One the enum has goes the usual way, and gives the same.
        let frame = sim::mag_cal_report_frame(1, 1, 0, 0, 1, 4, 3.5, [1.0, 2.0, 3.0]);
        assert!(matches!(
            decode(&frame).map(|d| d.message),
            Some(Message::MagCalReport {
                cal_status: 4,
                autosaved: true,
                ..
            })
        ));
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
    fn px4_estimator_status_reads_with_ardupilot_ekf_flags() {
        use mavio::dialects::ardupilotmega::enums::EstimatorStatusFlags;
        use mavio::dialects::ardupilotmega::messages::EstimatorStatus;
        let packet = EstimatorStatus {
            flags: EstimatorStatusFlags::from_bits_truncate(0b100_0011_1111),
            vel_ratio: 0.1,
            pos_horiz_ratio: 0.2,
            pos_vert_ratio: 0.3,
            mag_ratio: 0.4,
            hagl_ratio: 0.5,
            tas_ratio: 0.9,
            ..Default::default()
        };
        let decoded = decode(&frame_of(1, 0, &packet)).expect("estimator status frame");
        assert_eq!(
            decoded.message,
            Message::EstimatorStatus {
                flags: 0b11_1111 | 32768,
                ratios: [0.1, 0.2, 0.3, 0.4, 0.5],
            },
            "the GPS glitch moves to ArduPilot's bit, not its uninitialised one"
        );
        assert_eq!(
            crate::producers::ekf_text(0b11_1111 | 32768),
            "GPS glitching"
        );
        assert_eq!(crate::producers::ekf_text(0b11_1111), "Good");
    }

    #[test]
    fn px4_wind_reads_as_ardupilot_wind() {
        use mavio::dialects::ardupilotmega::messages::WindCov;
        let packet = WindCov {
            wind_x: -3.0,
            wind_y: -4.0,
            wind_z: 0.5,
            ..Default::default()
        };
        let decoded = decode(&frame_of(1, 0, &packet)).expect("wind frame");
        let Message::Wind {
            direction,
            speed,
            speed_z,
        } = decoded.message
        else {
            panic!("not wind: {:?}", decoded.message);
        };
        assert!(
            (direction - 53.130_1).abs() < 0.01,
            "from the north-east: {direction}"
        );
        assert!((speed - 5.0).abs() < 1e-6);
        assert!((speed_z - 0.5).abs() < 1e-6);

        let unknown = WindCov {
            wind_x: f32::NAN,
            wind_y: f32::NAN,
            ..Default::default()
        };
        let decoded = decode(&frame_of(1, 1, &unknown)).expect("wind frame");
        assert!(
            !matches!(decoded.message, Message::Wind { .. }),
            "no estimate yet"
        );
    }

    #[test]
    fn distance_sensor_keeps_measurement_validity() {
        use mavio::dialects::ardupilotmega::messages::DistanceSensor;
        for signal_quality in [0, 1, 100] {
            let packet = DistanceSensor {
                min_distance: 20,
                max_distance: 1500,
                current_distance: 1250,
                signal_quality,
                ..Default::default()
            };
            let decoded = decode(&frame_of(7, 0, &packet)).expect("distance sensor frame");
            assert!(matches!(decoded.message, Message::DistanceSensor {
                distance_cm: 1250,
                min_distance_cm: 20,
                max_distance_cm: 1500,
                signal_quality: quality,
                orientation: 0,
            } if quality == signal_quality));
        }
    }

    #[test]
    fn rc_channels_keeps_receiver_signal_strength() {
        use mavio::dialects::ardupilotmega::messages::RcChannels;
        for rssi in [0, 127, 254, 255] {
            let packet = RcChannels {
                chancount: 1,
                chan1_raw: 1500,
                rssi,
                ..Default::default()
            };
            let decoded = decode(&frame_of(7, 0, &packet)).expect("RC channels frame");
            assert!(
                matches!(decoded.message, Message::RcChannels { rssi: actual, count: 1, channels } if actual == rssi && channels[0] == 1500)
            );
        }
    }

    #[test]
    fn decodes_optional_quaternion_attitude_without_losing_rates_or_offset() {
        use mavio::dialects::ardupilotmega::messages::AttitudeQuaternion;
        let packet = AttitudeQuaternion {
            time_boot_ms: 1234,
            q1: 0.5,
            q2: -0.5,
            q3: 0.5,
            q4: -0.5,
            rollspeed: 1.0,
            pitchspeed: -2.0,
            yawspeed: 3.0,
            repr_offset_q: [0.707, 0.0, 0.707, 0.0],
        };
        let decoded = decode(&frame_of(7, 0, &packet)).expect("quaternion frame");
        assert_eq!(decoded.component_id, 1);
        assert_eq!(
            decoded.message,
            Message::AttitudeQuaternion {
                time_boot_ms: 1234,
                q: [0.5, -0.5, 0.5, -0.5],
                rollspeed: 1.0,
                pitchspeed: -2.0,
                yawspeed: 3.0,
                repr_offset_q: [0.707, 0.0, 0.707, 0.0],
            }
        );
        assert!(!crate::session::STREAM_RATES.iter().any(|(id, _)| *id == 31));
        assert!(crate::session::STREAM_RATES.contains(&(30, 100_000)));
    }

    #[test]
    fn decodes_an_adsb_vehicle() {
        use mavio::dialects::ardupilotmega::enums::{AdsbAltitudeType, AdsbEmitterType, AdsbFlags};
        use mavio::dialects::ardupilotmega::messages::AdsbVehicle;

        let mut callsign = [0u8; 9];
        callsign[..6].copy_from_slice(b"EZY12A");
        let message = AdsbVehicle {
            icao_address: 0x40_6B_C1,
            lat: 514_700_000,
            lon: -4_500_000,
            altitude_type: AdsbAltitudeType::Geometric,
            altitude: 914_400,
            heading: 9_000,
            hor_velocity: 8_100,
            ver_velocity: 250,
            callsign,
            emitter_type: AdsbEmitterType::Rotocraft,
            tslc: 1,
            flags: AdsbFlags::from_bits_truncate(0x1f),
            squawk: 7000,
        };
        let decoded = decode(&frame_of(7, 1, &message)).unwrap_or_else(|| panic!("adsb"));
        assert_eq!(
            decoded.message,
            Message::AdsbVehicle {
                icao_address: 0x40_6B_C1,
                lat: 514_700_000,
                lon: -4_500_000,
                altitude_type: 1,
                altitude: 914_400,
                heading: 9_000,
                hor_velocity: 8_100,
                ver_velocity: 250,
                callsign: "EZY12A".into(),
                emitter_type: 7,
                tslc: 1,
                flags: 0x1f,
                squawk: 7000,
            }
        );
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
