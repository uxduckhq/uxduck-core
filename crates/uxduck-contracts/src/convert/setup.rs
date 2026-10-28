use uxduck_core::domain::setup as s;

use crate::dto::setup as dto;

impl From<s::SetupRow> for dto::SetupRow {
    fn from(v: s::SetupRow) -> Self {
        match v {
            s::SetupRow::Firmware => Self::Firmware,
            s::SetupRow::Accelerometer => Self::Accelerometer,
            s::SetupRow::Compass => Self::Compass,
            s::SetupRow::Radio => Self::Radio,
            s::SetupRow::Airspeed => Self::Airspeed,
            s::SetupRow::Motors => Self::Motors,
        }
    }
}

impl From<dto::SetupRow> for s::SetupRow {
    fn from(v: dto::SetupRow) -> Self {
        match v {
            dto::SetupRow::Firmware => Self::Firmware,
            dto::SetupRow::Accelerometer => Self::Accelerometer,
            dto::SetupRow::Compass => Self::Compass,
            dto::SetupRow::Radio => Self::Radio,
            dto::SetupRow::Airspeed => Self::Airspeed,
            dto::SetupRow::Motors => Self::Motors,
        }
    }
}

impl From<s::SetupKind> for dto::SetupKind {
    fn from(v: s::SetupKind) -> Self {
        match v {
            s::SetupKind::Compass => Self::Compass,
            s::SetupKind::CompassHeading => Self::CompassHeading,
            s::SetupKind::Accelerometer => Self::Accelerometer,
            s::SetupKind::Level => Self::Level,
            s::SetupKind::Radio => Self::Radio,
            s::SetupKind::Motors => Self::Motors,
            s::SetupKind::Firmware => Self::Firmware,
            s::SetupKind::AirspeedZero => Self::AirspeedZero,
            s::SetupKind::AirspeedCheck => Self::AirspeedCheck,
            s::SetupKind::AirspeedRatio => Self::AirspeedRatio,
        }
    }
}

impl From<dto::SetupKind> for s::SetupKind {
    fn from(v: dto::SetupKind) -> Self {
        match v {
            dto::SetupKind::Compass => Self::Compass,
            dto::SetupKind::CompassHeading => Self::CompassHeading,
            dto::SetupKind::Accelerometer => Self::Accelerometer,
            dto::SetupKind::Level => Self::Level,
            dto::SetupKind::Radio => Self::Radio,
            dto::SetupKind::Motors => Self::Motors,
            dto::SetupKind::Firmware => Self::Firmware,
            dto::SetupKind::AirspeedZero => Self::AirspeedZero,
            dto::SetupKind::AirspeedCheck => Self::AirspeedCheck,
            dto::SetupKind::AirspeedRatio => Self::AirspeedRatio,
        }
    }
}

impl From<s::SetupStep> for dto::SetupStep {
    fn from(v: s::SetupStep) -> Self {
        match v {
            s::SetupStep::Starting => Self::Starting,
            s::SetupStep::Running => Self::Running,
            s::SetupStep::Succeeded => Self::Succeeded,
            s::SetupStep::Failed => Self::Failed,
            s::SetupStep::Cancelled => Self::Cancelled,
        }
    }
}

impl From<s::Instruction> for dto::Instruction {
    fn from(v: s::Instruction) -> Self {
        match v {
            s::Instruction::TurnAround => Self::TurnAround,
            s::Instruction::Restart => Self::Restart,
            s::Instruction::TurnSlower => Self::TurnSlower,
            s::Instruction::CheckOrientation => Self::CheckOrientation,
            s::Instruction::MoveAway => Self::MoveAway,
            s::Instruction::TryAgain => Self::TryAgain,
            s::Instruction::GivePosition => Self::GivePosition,
            s::Instruction::PlaceAsShown => Self::PlaceAsShown,
            s::Instruction::HoldStill => Self::HoldStill,
            s::Instruction::SetLevel => Self::SetLevel,
            s::Instruction::CentreSticks => Self::CentreSticks,
            s::Instruction::MoveToEnds => Self::MoveToEnds,
            s::Instruction::MoveStick => Self::MoveStick,
            s::Instruction::CheckRadio => Self::CheckRadio,
            s::Instruction::Replug => Self::Replug,
            s::Instruction::KeepPlugged => Self::KeepPlugged,
            s::Instruction::CoverPitot => Self::CoverPitot,
            s::Instruction::BlowIntoPitot => Self::BlowIntoPitot,
            s::Instruction::CheckPitot => Self::CheckPitot,
            s::Instruction::FlyCircles => Self::FlyCircles,
            s::Instruction::HoldOnASide => Self::HoldOnASide,
            s::Instruction::TurnOnThisSide => Self::TurnOnThisSide,
        }
    }
}

impl From<s::SensorState> for dto::SensorState {
    fn from(v: s::SensorState) -> Self {
        match v {
            s::SensorState::Waiting => Self::Waiting,
            s::SensorState::Running => Self::Running,
            s::SensorState::Succeeded => Self::Succeeded,
            s::SensorState::Failed => Self::Failed,
        }
    }
}

impl From<s::CompassFailure> for dto::CompassFailure {
    fn from(v: s::CompassFailure) -> Self {
        match v {
            s::CompassFailure::Failed => Self::Failed,
            s::CompassFailure::BadOrientation => Self::BadOrientation,
            s::CompassFailure::BadRadius => Self::BadRadius,
            s::CompassFailure::Offsets => Self::Offsets,
            s::CompassFailure::Scaling => Self::Scaling,
            s::CompassFailure::Residuals => Self::Residuals,
        }
    }
}

impl From<s::Fitness> for dto::Fitness {
    fn from(v: s::Fitness) -> Self {
        match v {
            s::Fitness::Good => Self::Good,
            s::Fitness::Acceptable => Self::Acceptable,
            s::Fitness::Poor => Self::Poor,
        }
    }
}

impl From<&s::SensorProgress> for dto::SensorProgress {
    fn from(v: &s::SensorProgress) -> Self {
        Self {
            index: v.index,
            state: v.state.into(),
            progress: v.progress,
            sections: v.sections.clone(),
            direction: v.direction,
            attempt: v.attempt,
            fitness: v.fitness,
            quality: v.quality.map(Into::into),
            offsets: v.offsets,
            failure: v.failure.map(Into::into),
            autosaved: v.autosaved,
        }
    }
}

impl From<&s::Setup> for dto::Setup {
    fn from(v: &s::Setup) -> Self {
        Self {
            vehicle_id: v.vehicle_id.clone(),
            row: v.kind.row().into(),
            kind: v.kind.into(),
            step: v.step.into(),
            progress: v.progress,
            sensors: v.sensors.iter().map(Into::into).collect(),
            instruction: v.instruction.map(Into::into),
            detail: v.detail.clone(),
            restart_needed: v.restart_needed,
            heading: v.heading.as_ref().map(Into::into),
            position: v.position.map(Into::into),
            placed: v.placed,
            sides: v.sides.as_ref().map(|sides| dto::Sides {
                needed: sides.needed.iter().map(|p| (*p).into()).collect(),
                done: sides.done.iter().map(|p| (*p).into()).collect(),
            }),
            radio: v.radio.as_ref().map(Into::into),
            motor: v.motor.as_ref().map(|m| dto::MotorTest {
                order: m.order,
                label: m.label.clone(),
                throttle: m.throttle,
            }),
            firmware: v.firmware.as_ref().map(Into::into),
            airspeed: v.airspeed.as_deref().map(Into::into),
            started_by: v.started_by.as_ref().map(Into::into),
            receipt_id: v.receipt_id.clone(),
            started_at_us: v.started_at_us,
            updated_at_us: v.updated_at_us,
            finished_at_us: v.finished_at_us,
            version: v.version,
        }
    }
}

impl From<&s::AirspeedRun> for dto::AirspeedRun {
    fn from(v: &s::AirspeedRun) -> Self {
        Self {
            start_mps: v.start_mps,
            reading_mps: v.reading_mps,
            peak_mps: v.peak_mps,
            rise_mps: v.rise_mps,
            until_us: v.until_us,
            ratio_param: v.ratio_param.clone(),
            autocal_param: v.autocal_param.clone(),
            ratio_before: v.ratio_before,
            ratio: v.ratio,
            spread_pct: v.spread_pct,
            gathered_s: v.gathered_s,
            settled: v.settled,
            autocal_on: v.autocal_on,
            writes: v
                .writes
                .iter()
                .map(|w| dto::RadioWrite {
                    name: w.name.clone(),
                    from: w.from,
                    to: w.to,
                    state: w.state.into(),
                })
                .collect(),
        }
    }
}

impl From<s::VehiclePosition> for dto::VehiclePosition {
    fn from(v: s::VehiclePosition) -> Self {
        match v {
            s::VehiclePosition::Level => Self::Level,
            s::VehiclePosition::Left => Self::Left,
            s::VehiclePosition::Right => Self::Right,
            s::VehiclePosition::NoseDown => Self::NoseDown,
            s::VehiclePosition::NoseUp => Self::NoseUp,
            s::VehiclePosition::Back => Self::Back,
        }
    }
}

impl From<dto::VehiclePosition> for s::VehiclePosition {
    fn from(v: dto::VehiclePosition) -> Self {
        match v {
            dto::VehiclePosition::Level => Self::Level,
            dto::VehiclePosition::Left => Self::Left,
            dto::VehiclePosition::Right => Self::Right,
            dto::VehiclePosition::NoseDown => Self::NoseDown,
            dto::VehiclePosition::NoseUp => Self::NoseUp,
            dto::VehiclePosition::Back => Self::Back,
        }
    }
}

impl From<s::HeadingFrom> for dto::HeadingFrom {
    fn from(v: s::HeadingFrom) -> Self {
        match v {
            s::HeadingFrom::Typed => Self::Typed,
            s::HeadingFrom::Marker => Self::Marker,
            s::HeadingFrom::Map => Self::Map,
        }
    }
}

impl From<dto::HeadingFrom> for s::HeadingFrom {
    fn from(v: dto::HeadingFrom) -> Self {
        match v {
            dto::HeadingFrom::Typed => Self::Typed,
            dto::HeadingFrom::Marker => Self::Marker,
            dto::HeadingFrom::Map => Self::Map,
        }
    }
}

impl From<&s::KnownHeading> for dto::KnownHeading {
    fn from(v: &s::KnownHeading) -> Self {
        Self {
            degrees: v.degrees,
            from: v.from.into(),
            marker: v.marker.clone(),
            latitude_deg: v.latitude_deg,
            longitude_deg: v.longitude_deg,
        }
    }
}

impl From<s::Stick> for dto::Stick {
    fn from(v: s::Stick) -> Self {
        match v {
            s::Stick::Roll => Self::Roll,
            s::Stick::Pitch => Self::Pitch,
            s::Stick::Throttle => Self::Throttle,
            s::Stick::Yaw => Self::Yaw,
        }
    }
}

impl From<s::RadioStep> for dto::RadioStep {
    fn from(v: s::RadioStep) -> Self {
        match v {
            s::RadioStep::Centre => Self::Centre,
            s::RadioStep::Ends => Self::Ends,
            s::RadioStep::Sticks => Self::Sticks,
            s::RadioStep::Review => Self::Review,
            s::RadioStep::Writing => Self::Writing,
        }
    }
}

impl From<dto::RadioStep> for s::RadioStep {
    fn from(v: dto::RadioStep) -> Self {
        match v {
            dto::RadioStep::Centre => Self::Centre,
            dto::RadioStep::Ends => Self::Ends,
            dto::RadioStep::Sticks => Self::Sticks,
            dto::RadioStep::Review => Self::Review,
            dto::RadioStep::Writing => Self::Writing,
        }
    }
}

impl From<s::WriteState> for dto::WriteState {
    fn from(v: s::WriteState) -> Self {
        match v {
            s::WriteState::Writing => Self::Writing,
            s::WriteState::Written => Self::Written,
            s::WriteState::Refused => Self::Refused,
        }
    }
}

impl From<&s::Radio> for dto::Radio {
    fn from(v: &s::Radio) -> Self {
        Self {
            step: v.step.into(),
            channels: v
                .channels
                .iter()
                .map(|c| dto::RadioChannel {
                    channel: c.channel,
                    value: c.value,
                    min: c.min,
                    max: c.max,
                    centre: c.centre,
                })
                .collect(),
            asked: v.asked.map(Into::into),
            sticks: v
                .sticks
                .iter()
                .map(|f| dto::StickFound {
                    stick: f.stick.into(),
                    channel: f.channel,
                    lowers: f.lowers,
                })
                .collect(),
            fixes: v
                .fixes
                .iter()
                .map(|f| dto::RadioFix {
                    name: f.name.clone(),
                    stick: f.stick.into(),
                    from: f.from,
                    to: f.to,
                })
                .collect(),
            writes: v
                .writes
                .iter()
                .map(|w| dto::RadioWrite {
                    name: w.name.clone(),
                    from: w.from,
                    to: w.to,
                    state: w.state.into(),
                })
                .collect(),
        }
    }
}

impl From<&uxduck_core::domain::frames::MotorLayout> for dto::MotorLayout {
    fn from(v: &uxduck_core::domain::frames::MotorLayout) -> Self {
        use uxduck_core::domain::frames::Spin;
        Self {
            frame: v.frame.clone(),
            motors: v
                .motors
                .iter()
                .map(|m| dto::MotorPlace {
                    order: m.order,
                    label: m.label.clone(),
                    motor: m.motor,
                    right: m.right,
                    forward: m.forward,
                    spin: m.spin.map(|spin| match spin {
                        Spin::Clockwise => dto::Spin::Clockwise,
                        Spin::CounterClockwise => dto::Spin::CounterClockwise,
                    }),
                    stack: m.stack,
                })
                .collect(),
        }
    }
}

impl From<s::FirmwareStage> for dto::FirmwareStage {
    fn from(v: s::FirmwareStage) -> Self {
        match v {
            s::FirmwareStage::Restarting => Self::Restarting,
            s::FirmwareStage::Waiting => Self::Waiting,
            s::FirmwareStage::Downloading => Self::Downloading,
            s::FirmwareStage::Erasing => Self::Erasing,
            s::FirmwareStage::Writing => Self::Writing,
            s::FirmwareStage::Verifying => Self::Verifying,
            s::FirmwareStage::Starting => Self::Starting,
            s::FirmwareStage::Reconnecting => Self::Reconnecting,
        }
    }
}

impl From<&s::FirmwareSource> for dto::FirmwareSource {
    fn from(v: &s::FirmwareSource) -> Self {
        match v {
            s::FirmwareSource::Release {
                vehicle_type,
                release,
                platform,
            } => Self::Release {
                vehicle_type: vehicle_type.clone(),
                release: release.clone(),
                platform: platform.clone(),
            },
            s::FirmwareSource::File { name } => Self::File { name: name.clone() },
        }
    }
}

impl From<s::FirmwareBoard> for dto::FirmwareBoard {
    fn from(v: s::FirmwareBoard) -> Self {
        Self {
            board_id: v.board_id,
            board_rev: v.board_rev,
            bootloader: v.bootloader,
            flash_size: v.flash_size,
        }
    }
}

impl From<s::FirmwareFailure> for dto::FirmwareFailure {
    fn from(v: s::FirmwareFailure) -> Self {
        match v {
            s::FirmwareFailure::BoardMismatch => Self::BoardMismatch,
            s::FirmwareFailure::TooLarge => Self::TooLarge,
            s::FirmwareFailure::Crc => Self::Crc,
            s::FirmwareFailure::NoBootloader => Self::NoBootloader,
            s::FirmwareFailure::Dfu => Self::Dfu,
            s::FirmwareFailure::Unsupported => Self::Unsupported,
            s::FirmwareFailure::Bootloader => Self::Bootloader,
            s::FirmwareFailure::NoRelease => Self::NoRelease,
            s::FirmwareFailure::Download => Self::Download,
            s::FirmwareFailure::BadFile => Self::BadFile,
            s::FirmwareFailure::Port => Self::Port,
            s::FirmwareFailure::NotReconnected => Self::NotReconnected,
        }
    }
}

impl From<&s::FirmwareInstall> for dto::FirmwareInstall {
    fn from(v: &s::FirmwareInstall) -> Self {
        Self {
            stage: v.stage.into(),
            done: v.done,
            total: v.total,
            source: (&v.source).into(),
            before: v.before.clone(),
            before_type: v.before_type.clone(),
            board: v.board.map(Into::into),
            file_board_id: v.file_board_id,
            platform: v.platform.clone(),
            version: v.version.clone(),
            url: v.url.clone(),
            parameters: v.parameters.clone(),
            failure: v.failure.map(Into::into),
            after: v.after.clone(),
            after_type: v.after_type.clone(),
        }
    }
}
