//! Installing firmware (S189, ADR-0037 V10): a procedure of kind `firmware`
//! on the Firmware row, for a vehicle whose board is on a USB port of the
//! computer running UXDuck Desktop. The engine decides and records; the
//! host's [`FirmwarePort`] fetches the firmware list and releases, and
//! speaks the board's bootloader (`uxduck_mavlink::bootloader`).
//!
//! In order: the vehicle's parameters are saved to a file in its Files; a
//! vehicle UXDuck hears is restarted into its bootloader
//! (`MAV_CMD_PREFLIGHT_REBOOT_SHUTDOWN` 3, journaled with a receipt) and,
//! once it answers, its serial link is paused so the port is free; one it
//! does not hear is replugged by the person. The port then finds the
//! bootloader by its USB ids, reads the board, resolves the release for it
//! (or checks the file against it), and erases, writes and verifies,
//! reporting each stage; the link is resumed, and the vehicle heard again
//! with its new version ends it, with a history event saying what changed.
//! Cancelling is only possible before erasing. A file for another board, an
//! image larger than the flash, an armed vehicle, a board on another
//! computer's USB and an STM32 DFU board are refused. The agent reads the
//! releases and the installed version; it cannot install.

use std::collections::BTreeMap;

use base64::Engine as _;
use serde_json::Value;
use uxduck_contracts::dto::setup::{
    FirmwareRelease, FirmwareVehicleType, ReleaseChannel, Setup as SetupDto, SetupFirmwareParams,
    SetupReleasesParams, SetupReleasesResult,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::command::{Command, Receipt, ReceiptStatus};
use uxduck_core::domain::history::HistoryEvent;
use uxduck_core::domain::link::{LinkKind, UsbId};
use uxduck_core::domain::setup::{
    FirmwareBoard, FirmwareFailure, FirmwareInstall, FirmwareSource, FirmwareStage, Instruction,
    Setup, SetupKind, SetupRow, SetupStep,
};
use uxduck_core::domain::vehicle::{Presence, Vehicle};
use uxduck_mavlink::firmware::{BoardHint, Channel, FirmwareFile, Release, Releases, VehicleType};

use super::{Order, actor, no_vehicle};
use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// `MAV_CMD_PREFLIGHT_REBOOT_SHUTDOWN`, with 3: restart and stay in the
/// bootloader until firmware is installed.
const REBOOT_SHUTDOWN: u16 = 246;
const INTO_BOOTLOADER: f32 = 3.0;
/// Once the new firmware starts, the vehicle should be heard within this.
const RECONNECT_US: u64 = 60_000_000;
/// Heard again, it says its version within this; after it, the version it
/// has is taken as the new one (the same version installed again).
const VERSION_WAIT_US: u64 = 10_000_000;
/// Progress is journaled at most this often; a new stage at once.
const PROGRESS_EVERY_US: u64 = 1_000_000;

/// The host's side: the firmware list and releases from the firmware
/// server, and the board's bootloader on USB.
pub trait FirmwarePort: Send {
    /// Lists releases; answered through [`Engine::firmware_reported`].
    fn releases(&mut self, order: ReleasesOrder);
    /// Installs; each stage, the board and the end are reported through
    /// [`Engine::firmware_reported`].
    fn install(&mut self, order: InstallOrder);
    /// Stops an install that has not started erasing; it then ends
    /// cancelled, its board restarted into its old firmware.
    fn cancel(&mut self, id: &str);
}

/// Which releases to list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleasesOrder {
    pub id: String,
    pub hint: BoardHint,
    pub vehicle_type: VehicleType,
    pub refresh: bool,
}

/// What to install.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallSource {
    /// Resolved for the board the bootloader names.
    Release {
        vehicle_type: VehicleType,
        release: String,
        platform: Option<String>,
    },
    File {
        name: String,
        file: FirmwareFile,
    },
}

/// An install for the port to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallOrder {
    pub id: String,
    pub vehicle_id: String,
    /// The serial device the vehicle's link opens, and its USB identity:
    /// where the bootloader is looked for.
    pub device: String,
    pub usb: Option<UsbId>,
    pub source: InstallSource,
    /// The firmware was asked to restart into its bootloader; otherwise
    /// the person replugs the board.
    pub restarted: bool,
}

/// The releases found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleasesFound {
    pub releases: Releases,
    /// When the list was fetched from the firmware server.
    pub fetched_at_us: u64,
    /// Fetching failed and this is the list kept from before: why.
    pub stale: Option<String>,
}

/// What the port reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FirmwareReport {
    Releases {
        id: String,
        result: Result<ReleasesFound, String>,
    },
    Stage {
        id: String,
        stage: FirmwareStage,
        done: u64,
        total: u64,
    },
    /// The bootloader said which board it is, and the release resolved for
    /// it.
    Board {
        id: String,
        board: FirmwareBoard,
        release: Option<Release>,
    },
    /// Installed and started.
    Installed {
        id: String,
    },
    Failed {
        id: String,
        failure: FirmwareFailure,
        detail: String,
        /// The board the file or release was for, where that is the
        /// failure.
        file_board_id: Option<u32>,
    },
    Cancelled {
        id: String,
    },
}

/// An install the engine follows.
#[derive(Clone, Debug)]
pub(crate) struct Install {
    vehicle_id: String,
    /// The link paused for it, to resume.
    link_id: String,
    paused: bool,
    /// The order, sent once the vehicle answers its restart.
    order: Option<InstallOrder>,
    /// When the new firmware started: reconnecting since.
    started_us: Option<u64>,
    /// When the vehicle was first heard again.
    heard_us: Option<u64>,
}

/// What the engine keeps about firmware beyond the journal.
#[derive(Default)]
pub(crate) struct Firmware {
    pub(crate) port: Option<Box<dyn FirmwarePort>>,
    pub(crate) installs: BTreeMap<String, Install>,
    /// `setup/firmwareReleases` answered when the port reports: the
    /// connection, the request's id, the vehicle and the type listed.
    asked: BTreeMap<String, (ConnectionId, Value, String, VehicleType)>,
}

impl std::fmt::Debug for Firmware {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Firmware")
            .field("port", &self.port.is_some())
            .field("installs", &self.installs)
            .finish_non_exhaustive()
    }
}

/// Why no PX4 release is offered (S191).
const PX4_RELEASES: &str = "UXDuck does not list PX4's releases yet: PX4 publishes a .px4 file \
                            for each board with each release, on GitHub.";

fn desktop_only() -> ApiError {
    ApiError::new(
        ErrorKind::InvalidRequest,
        "Installing firmware needs UXDuck Desktop, with the board on this computer's USB.",
    )
}

/// The firmware a vehicle runs now, for the list's default.
fn own_type(vehicle: &Vehicle) -> VehicleType {
    VehicleType::of_vehicle(&vehicle.vehicle_type).unwrap_or(VehicleType::Copter)
}

fn vehicle_type(asked: Option<&str>, vehicle: &Vehicle) -> Result<VehicleType, ApiError> {
    asked.map_or_else(
        || Ok(own_type(vehicle)),
        |slug| {
            VehicleType::from_slug(slug).ok_or_else(|| {
                ApiError::new(
                    ErrorKind::InvalidValue,
                    format!(
                        "{slug} is not a vehicle type ArduPilot builds; use {}.",
                        VehicleType::ALL.map(VehicleType::slug).join(", ")
                    ),
                )
            })
        },
    )
}

fn channel(channel: Channel) -> ReleaseChannel {
    match channel {
        Channel::Stable => ReleaseChannel::Stable,
        Channel::Beta => ReleaseChannel::Beta,
        Channel::Dev => ReleaseChannel::Dev,
        Channel::Older => ReleaseChannel::Older,
    }
}

/// The day, in UTC, as "2026-10-25".
fn date_of(us: u64) -> String {
    let days = i64::try_from(us / 86_400_000_000).unwrap_or(0);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's method).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// "4.6.2" from "4.6.2-dev" and the like: just the version, for history.
fn version_of(text: &str) -> &str {
    text.trim()
}

impl Engine {
    /// Where the firmware list and the bootloader are: UXDuck Desktop's.
    pub fn set_firmware(&mut self, port: Box<dyn FirmwarePort>) {
        self.setup.firmware.port = Some(port);
    }

    /// The serial link a vehicle is on, with its device and USB identity;
    /// why not when it is on another kind of link.
    fn usb_of(&self, vehicle: &Vehicle) -> Result<(String, Option<UsbId>), ApiError> {
        let Some(link) = self.state.links.get(&vehicle.link_id) else {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                format!("{} has no link UXDuck can use.", vehicle.name),
            ));
        };
        match &link.kind {
            LinkKind::Serial { device, usb, .. } => Ok((device.clone(), usb.clone())),
            LinkKind::Simulator { .. } => Err(ApiError::new(
                ErrorKind::InvalidValue,
                format!(
                    "{} is simulated: its firmware is the simulator's.",
                    vehicle.name
                ),
            )),
            _ => Err(ApiError::new(
                ErrorKind::InvalidValue,
                format!(
                    "Plug {} into this computer's USB and connect it there to install firmware: \
                     it is connected through {}.",
                    vehicle.name, link.name
                ),
            )),
        }
    }

    /// What is known of the vehicle's board: its id from its last install,
    /// and its USB id.
    fn board_hint(&self, vehicle: &Vehicle, platform: Option<String>) -> BoardHint {
        let board_id = self
            .row_setup(&vehicle.id, SetupRow::Firmware)
            .and_then(|s| s.firmware)
            .and_then(|f| f.board)
            .map(|b| b.board_id);
        let usb = self
            .state
            .links
            .get(&vehicle.link_id)
            .and_then(|link| match &link.kind {
                LinkKind::Serial { usb: Some(usb), .. } => Some((usb.vendor_id, usb.product_id)),
                _ => None,
            });
        BoardHint {
            board_id,
            usb,
            platform,
        }
    }

    /// `setup/firmwareReleases`: asked of the port, answered when it
    /// reports.
    fn firmware_releases(
        &mut self,
        connection: ConnectionId,
        params: &SetupReleasesParams,
    ) -> Result<Value, ApiError> {
        let vehicle = self
            .vehicle(&params.vehicle_id)
            .cloned()
            .ok_or_else(|| no_vehicle(&params.vehicle_id))?;
        let kind = vehicle_type(params.vehicle_type.as_deref(), &vehicle)?;
        if self.setup.firmware.port.is_none() {
            return Err(desktop_only());
        }
        // PX4's releases are not listed (S191): a file only.
        if super::px4::px4(&vehicle) {
            let result = SetupReleasesResult {
                vehicle_id: vehicle.id.clone(),
                vehicle_type: kind.slug().to_owned(),
                vehicle_types: Vec::new(),
                installed: vehicle.firmware.clone(),
                board_id: None,
                platform: None,
                platforms: Vec::new(),
                brand: None,
                releases: Vec::new(),
                fetched_at_us: None,
                detail: Some(format!(
                    "{PX4_RELEASES} Choose a .px4 file built for its board."
                )),
            };
            return to_value(&result);
        }
        let Some((_, request_id)) = self.replying_to.take() else {
            return Err(ApiError::new(
                ErrorKind::InvalidRequest,
                "setup/firmwareReleases is answered as a request",
            ));
        };
        let id = format!("fr{}", &self.random.token()[..16]);
        self.setup.firmware.asked.insert(
            id.clone(),
            (connection, request_id, vehicle.id.clone(), kind),
        );
        let order = ReleasesOrder {
            id,
            hint: self.board_hint(&vehicle, params.platform.clone()),
            vehicle_type: kind,
            refresh: params.refresh,
        };
        if let Some(port) = self.setup.firmware.port.as_mut() {
            port.releases(order);
        }
        // Answered when the port reports.
        Ok(Value::Null)
    }

    fn answer_releases(&mut self, id: &str, result: Result<ReleasesFound, String>) {
        let Some((connection, request_id, vehicle_id, kind)) = self.setup.firmware.asked.remove(id)
        else {
            return;
        };
        let installed = self.vehicle(&vehicle_id).and_then(|v| v.firmware.clone());
        let frame = match result {
            Ok(found) => {
                let releases = found.releases;
                let detail = if let Some(why) = found.stale {
                    Some(format!(
                        "This list is the one kept from before: the firmware server could not be \
                         reached ({why})."
                    ))
                } else if releases.releases.is_empty() && releases.platform.is_none() {
                    Some(
                        "UXDuck reads the board when it restarts it; choose the vehicle type and \
                         release to install."
                            .to_owned(),
                    )
                } else if releases.releases.is_empty() {
                    Some(format!(
                        "The firmware server has no {} firmware for this board.",
                        kind.words()
                    ))
                } else {
                    None
                };
                let result = SetupReleasesResult {
                    vehicle_id,
                    vehicle_type: kind.slug().to_owned(),
                    vehicle_types: VehicleType::ALL
                        .iter()
                        .map(|t| FirmwareVehicleType {
                            id: t.slug().to_owned(),
                            name: t.words().to_owned(),
                        })
                        .collect(),
                    installed,
                    board_id: releases.board_id,
                    platform: releases.platform.clone(),
                    platforms: releases.platforms.clone(),
                    brand: releases.brand.clone(),
                    releases: releases
                        .releases
                        .iter()
                        .map(|r| FirmwareRelease {
                            channel: channel(r.channel),
                            version: r.version.clone(),
                            url: r.url.clone(),
                            board_id: r.board_id,
                            platform: r.platform.clone(),
                            image_size: r.image_size,
                        })
                        .collect(),
                    fetched_at_us: Some(found.fetched_at_us),
                    detail,
                };
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": to_value(&result).unwrap_or(Value::Null),
                })
            }
            Err(why) => crate::rpc::router::error(
                &request_id,
                &ApiError::new(
                    ErrorKind::Busy,
                    format!("The firmware list could not be fetched: {why}."),
                ),
            ),
        };
        self.send(connection, &frame);
    }

    /// The parameters as a `.param` file (`NAME,value`, sorted, with a
    /// header), as Parameters exports them.
    fn parameters_file(&self, vehicle: &Vehicle) -> Option<String> {
        let mut lines: Vec<String> = self
            .state
            .parameters
            .values()
            .filter(|p| p.vehicle_id == vehicle.id)
            .map(|p| format!("{},{}", p.name, p.value))
            .collect();
        if lines.is_empty() {
            return None;
        }
        lines.sort();
        Some(format!(
            "# {} {}, saved by UXDuck before installing firmware\n{}\n",
            vehicle.name,
            vehicle.firmware.as_deref().unwrap_or("(version unknown)"),
            lines.join("\n")
        ))
    }

    /// Saves the parameters to the vehicle's Files; the file's name.
    fn save_parameters(&mut self, vehicle: &Vehicle) -> Option<String> {
        let text = self.parameters_file(vehicle)?;
        let now = self.clock.now_us();
        let seconds = now / 1_000_000 % 86_400;
        let time = format!(
            "{:02}{:02}{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        );
        let date = date_of(now);
        let safe: String = vehicle
            .name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        // UTC, to the second: each install keeps its own file.
        let name = format!(
            "{}-{date}-{time}-before-firmware.param",
            safe.trim_matches('-')
        );
        match self.keep_copy(&vehicle.id, &name, text.as_bytes()) {
            Ok(()) => Some(name),
            // Installing goes on: the parameters stay on the vehicle
            // unless erasing loses them, and the procedure says none were
            // saved.
            Err(_) => None,
        }
    }
}

/// `setup/firmware`: checked, the parameters saved, then the vehicle
/// restarted into its bootloader or the person asked to replug it.
#[allow(clippy::too_many_lines, reason = "the checks, in order")]
pub(super) fn install(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &SetupFirmwareParams,
    operator: bool,
) -> Result<Value, ApiError> {
    let vehicle = engine
        .vehicle(&params.vehicle_id)
        .cloned()
        .ok_or_else(|| no_vehicle(&params.vehicle_id))?;
    if !operator {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "Only a person at UXDuck, in its window or its CLI, can install firmware.",
        ));
    }
    if engine.setup.firmware.port.is_none() {
        return Err(desktop_only());
    }
    // PX4 from a `.px4` file, through the same bootloader (S191); its
    // releases are not listed.
    let px4 = super::px4::px4(&vehicle);
    if !uxduck_core::domain::setup::set_up(&vehicle.autopilot) {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!(
                "UXDuck installs ArduPilot and PX4, and {} runs neither.",
                vehicle.name
            ),
        ));
    }
    if px4 && params.file.is_none() {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!(
                "{PX4_RELEASES} Choose a .px4 file built for {}'s board.",
                vehicle.name
            ),
        ));
    }
    let (device, usb) = engine.usb_of(&vehicle)?;
    let online = vehicle.presence == Presence::Online;
    if online && vehicle.armed {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!(
                "Disarm {} first: firmware is installed only on the ground.",
                vehicle.name
            ),
        ));
    }
    if let Some(running) = engine.running_setup(&vehicle.id) {
        return Err(ApiError::new(
            ErrorKind::Busy,
            format!(
                "{} is already running on {}. Cancel it first.",
                running.kind.words(),
                vehicle.name
            ),
        ));
    }
    let (source, order_source, file_board_id) = match (&params.file, &params.release) {
        (Some(_), Some(_)) => {
            return Err(ApiError::new(
                ErrorKind::InvalidParams,
                "Install a release or a file, not both.",
            ));
        }
        (None, None) => {
            return Err(ApiError::new(
                ErrorKind::InvalidParams,
                "Choose a release (stable, beta, dev or a version) or a file to install.",
            ));
        }
        (Some(file), None) => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(file.contents.trim())
                .map_err(|_| {
                    ApiError::new(ErrorKind::InvalidValue, "The file did not arrive whole.")
                })?;
            let read = uxduck_mavlink::firmware::read_file(&bytes).map_err(|e| {
                ApiError::new(
                    ErrorKind::InvalidValue,
                    format!("{} can't be installed: {e}.", file.name),
                )
            })?;
            // Known from its last install: refused before anything.
            let known = engine
                .row_setup(&vehicle.id, SetupRow::Firmware)
                .and_then(|s| s.firmware)
                .and_then(|f| f.board);
            if let Some(board) = known
                && board.board_id != read.board_id
            {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    format!(
                        "{} is for board {}, and {}'s board is {}.",
                        file.name, read.board_id, vehicle.name, board.board_id
                    ),
                ));
            }
            if let Some(board) = known
                && read.image.len().div_ceil(4) * 4 > board.flash_size as usize
            {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    format!(
                        "{} is {} bytes, and {}'s board has {} bytes for firmware.",
                        file.name,
                        read.image.len(),
                        vehicle.name,
                        board.flash_size
                    ),
                ));
            }
            let board_id = read.board_id;
            (
                FirmwareSource::File {
                    name: file.name.clone(),
                },
                InstallSource::File {
                    name: file.name.clone(),
                    file: read,
                },
                Some(board_id),
            )
        }
        (None, Some(release)) => {
            let kind = vehicle_type(params.vehicle_type.as_deref(), &vehicle)?;
            (
                FirmwareSource::Release {
                    vehicle_type: kind.slug().to_owned(),
                    release: release.trim().to_owned(),
                    platform: params.platform.clone(),
                },
                InstallSource::Release {
                    vehicle_type: kind,
                    release: release.trim().to_owned(),
                    platform: params.platform.clone(),
                },
                None,
            )
        }
    };
    let writable = engine.not_writable(&vehicle.link_id).is_none();
    let restart = online && writable;
    let now = engine.clock.now_us();
    let id = format!("fw{}", &engine.random.token()[..16]);
    let parameters = engine.save_parameters(&vehicle);
    let receipt_id = restart.then(|| format!("rc{}", &engine.random.token()[..16]));
    let setup = Setup {
        vehicle_id: vehicle.id.clone(),
        kind: SetupKind::Firmware,
        step: SetupStep::Starting,
        progress: 0,
        sensors: Vec::new(),
        instruction: Some(if restart {
            Instruction::KeepPlugged
        } else {
            Instruction::Replug
        }),
        detail: None,
        restart_needed: false,
        heading: None,
        position: None,
        placed: false,
        sides: None,
        radio: None,
        motor: None,
        firmware: Some(FirmwareInstall {
            stage: if restart {
                FirmwareStage::Restarting
            } else {
                FirmwareStage::Waiting
            },
            done: 0,
            total: 0,
            source,
            before: vehicle.firmware.clone(),
            before_type: Some(vehicle.vehicle_type.clone()),
            board: None,
            file_board_id,
            platform: None,
            version: None,
            url: None,
            parameters,
            failure: None,
            after: None,
            after_type: None,
        }),
        airspeed: None,
        started_by: Some(engine.author(connection)),
        receipt_id: receipt_id.clone(),
        started_at_us: now,
        updated_at_us: now,
        finished_at_us: None,
        version: 0,
    };
    let actor = engine.actor(connection);
    let saved = engine.save_setup(setup, true, actor.clone());
    let order = InstallOrder {
        id: id.clone(),
        vehicle_id: vehicle.id.clone(),
        device,
        usb,
        source: order_source,
        restarted: restart,
    };
    engine.setup.firmware.installs.insert(
        id,
        Install {
            vehicle_id: vehicle.id.clone(),
            link_id: vehicle.link_id.clone(),
            paused: false,
            order: Some(order),
            started_us: None,
            heard_us: None,
        },
    );
    if let Some(receipt_id) = receipt_id {
        engine.send_setup_command(
            &vehicle,
            actor,
            receipt_id,
            Command::Long {
                command: REBOOT_SHUTDOWN,
                params: vec![INTO_BOOTLOADER],
            },
            Order::Start,
        );
    } else {
        engine.begin_install(&vehicle.id);
    }
    to_value(&SetupDto::from(&saved))
}

/// `setup/firmwareReleases`.
pub(super) fn releases(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &SetupReleasesParams,
) -> Result<Value, ApiError> {
    engine.firmware_releases(connection, params)
}

/// The progress over the whole install, from its stage and bytes.
fn overall(stage: FirmwareStage, done: u64, total: u64) -> u8 {
    let within = |from: u64, to: u64| {
        let span = to - from;
        let part = (done.min(total) * span).checked_div(total).unwrap_or(0);
        u8::try_from(from + part).unwrap_or(100)
    };
    match stage {
        FirmwareStage::Restarting => 0,
        FirmwareStage::Waiting => 3,
        FirmwareStage::Downloading => within(5, 15),
        FirmwareStage::Erasing => 15,
        FirmwareStage::Writing => within(20, 90),
        FirmwareStage::Verifying => 92,
        FirmwareStage::Starting => 95,
        FirmwareStage::Reconnecting => 97,
    }
}

impl Engine {
    /// The firmware install running on a vehicle, with its id.
    fn install_of(&self, vehicle_id: &str) -> Option<(String, Install)> {
        self.setup
            .firmware
            .installs
            .iter()
            .find(|(_, i)| i.vehicle_id == vehicle_id)
            .map(|(id, i)| (id.clone(), i.clone()))
    }

    /// Hands the install to the port: the link's port released first.
    fn begin_install(&mut self, vehicle_id: &str) {
        let Some((id, install)) = self.install_of(vehicle_id) else {
            return;
        };
        let Some(order) = install.order.clone() else {
            return;
        };
        self.pause_link(&id, true);
        if let Some(entry) = self.setup.firmware.installs.get_mut(&id) {
            entry.order = None;
        }
        if let Some(mut setup) = self.firmware_setup(vehicle_id) {
            if let Some(firmware) = setup.firmware.as_mut() {
                firmware.stage = FirmwareStage::Waiting;
            }
            setup.step = SetupStep::Running;
            setup.progress = overall(FirmwareStage::Waiting, 0, 0);
            setup.updated_at_us = self.clock.now_us();
            self.save_setup(setup, true, actor());
        }
        if let Some(port) = self.setup.firmware.port.as_mut() {
            port.install(order);
        }
    }

    fn pause_link(&mut self, id: &str, paused: bool) {
        let Some(install) = self.setup.firmware.installs.get_mut(id) else {
            return;
        };
        if install.paused == paused {
            return;
        }
        install.paused = paused;
        let link_id = install.link_id.clone();
        if let Some(link) = self.state.links.get(&link_id).cloned() {
            self.links.pause(&link, paused);
        }
    }

    fn firmware_setup(&self, vehicle_id: &str) -> Option<Setup> {
        self.row_setup(vehicle_id, SetupRow::Firmware)
            .filter(|s| s.kind == SetupKind::Firmware)
    }

    /// The vehicle answered its restart into the bootloader.
    pub(super) fn firmware_restart_answered(&mut self, setup: &Setup, receipt: &Receipt) {
        if !setup.step.running() {
            return;
        }
        let vehicle_id = setup.vehicle_id.clone();
        match receipt.status {
            // It restarts: the port is released before its bootloader
            // appears on USB.
            ReceiptStatus::Accepted => self.begin_install(&vehicle_id),
            // Restarting, it may not have answered: look for its
            // bootloader anyway, and ask for a replug if it is not there.
            ReceiptStatus::Timeout => {
                self.begin_install(&vehicle_id);
                if let Some(mut setup) = self.firmware_setup(&vehicle_id) {
                    setup.instruction = Some(Instruction::Replug);
                    self.save_setup(setup, true, actor());
                }
            }
            _ => {
                let name = self
                    .vehicle(&vehicle_id)
                    .map_or_else(|| "The vehicle".to_owned(), |v| v.name.clone());
                let detail = match receipt.detail.as_deref() {
                    Some("Unsupported") => {
                        format!(
                            "{name} can't restart into its bootloader: its firmware does not support it. Unplug it and plug it back in instead."
                        )
                    }
                    Some(said) if receipt.status == ReceiptStatus::Rejected => {
                        format!(
                            "{name} refused to restart into its bootloader ({said}). Check it is disarmed."
                        )
                    }
                    _ => format!(
                        "{name} refused to restart into its bootloader. Check it is disarmed."
                    ),
                };
                self.end_install(
                    &vehicle_id,
                    Err((FirmwareFailure::Bootloader, detail, None)),
                );
            }
        }
    }

    /// What the port reports.
    pub fn firmware_reported(&mut self, report: FirmwareReport) {
        match report {
            FirmwareReport::Releases { id, result } => self.answer_releases(&id, result),
            FirmwareReport::Stage {
                id,
                stage,
                done,
                total,
            } => self.install_stage(&id, stage, done, total),
            FirmwareReport::Board { id, board, release } => {
                let Some(install) = self.setup.firmware.installs.get(&id).cloned() else {
                    return;
                };
                let Some(mut setup) = self.firmware_setup(&install.vehicle_id) else {
                    return;
                };
                if let Some(firmware) = setup.firmware.as_mut() {
                    firmware.board = Some(board);
                    if let Some(release) = release {
                        firmware.file_board_id = Some(release.board_id);
                        firmware.platform = Some(release.platform);
                        firmware.version = Some(release.version);
                        firmware.url = Some(release.url);
                    }
                }
                // Found: no replug needed any more.
                setup.instruction = Some(Instruction::KeepPlugged);
                setup.updated_at_us = self.clock.now_us();
                self.save_setup(setup, true, actor());
            }
            FirmwareReport::Installed { id } => {
                let Some(install) = self.setup.firmware.installs.get(&id).cloned() else {
                    return;
                };
                self.pause_link(&id, false);
                let now = self.clock.now_us();
                if let Some(entry) = self.setup.firmware.installs.get_mut(&id) {
                    entry.started_us = Some(now);
                }
                self.install_stage(&id, FirmwareStage::Reconnecting, 0, 0);
                self.review_reconnect(&install.vehicle_id);
            }
            FirmwareReport::Failed {
                id,
                failure,
                detail,
                file_board_id,
            } => {
                let Some(install) = self.setup.firmware.installs.get(&id).cloned() else {
                    return;
                };
                self.end_install(&install.vehicle_id, Err((failure, detail, file_board_id)));
            }
            FirmwareReport::Cancelled { id } => {
                self.pause_link(&id, false);
                self.setup.firmware.installs.remove(&id);
            }
        }
    }

    fn install_stage(&mut self, id: &str, stage: FirmwareStage, done: u64, total: u64) {
        let Some(install) = self.setup.firmware.installs.get(id).cloned() else {
            return;
        };
        let Some(mut setup) = self
            .firmware_setup(&install.vehicle_id)
            .filter(|s| s.step.running())
        else {
            return;
        };
        let now = self.clock.now_us();
        let Some(firmware) = setup.firmware.as_mut() else {
            return;
        };
        let new_stage = firmware.stage != stage;
        firmware.stage = stage;
        firmware.done = done;
        firmware.total = total;
        setup.step = SetupStep::Running;
        setup.progress = overall(stage, done, total);
        if new_stage && stage >= FirmwareStage::Erasing {
            setup.instruction = Some(Instruction::KeepPlugged);
        }
        let due = new_stage
            || done >= total
            || now.saturating_sub(setup.updated_at_us) >= PROGRESS_EVERY_US;
        setup.updated_at_us = now;
        self.save_setup(setup, due, actor());
    }

    /// The vehicle was heard: an install reconnecting ends once it says
    /// its new version, or after a while with the version it has.
    pub(crate) fn review_reconnect(&mut self, vehicle_id: &str) {
        let Some((id, install)) = self.install_of(vehicle_id) else {
            return;
        };
        let Some(started) = install.started_us else {
            return;
        };
        let Some(vehicle) = self.vehicle(vehicle_id).cloned() else {
            return;
        };
        let now = self.clock.now_us();
        let heard = vehicle.presence == Presence::Online && vehicle.last_heartbeat_us >= started;
        if heard
            && install.heard_us.is_none()
            && let Some(entry) = self.setup.firmware.installs.get_mut(&id)
        {
            entry.heard_us = Some(now);
        }
        let heard_us = self
            .setup
            .firmware
            .installs
            .get(&id)
            .and_then(|i| i.heard_us);
        let before = self
            .firmware_setup(vehicle_id)
            .and_then(|s| s.firmware)
            .and_then(|f| f.before);
        let said_new = heard && vehicle.firmware.is_some() && vehicle.firmware != before;
        let waited = heard_us.is_some_and(|at| now.saturating_sub(at) >= VERSION_WAIT_US);
        if said_new || waited {
            self.end_install(vehicle_id, Ok(()));
        } else if heard_us.is_none() && now.saturating_sub(started) >= RECONNECT_US {
            self.end_install(
                vehicle_id,
                Err((
                    FirmwareFailure::NotReconnected,
                    format!(
                        "Installed, and {} has not been heard since. Check its USB cable, and that its link is on.",
                        vehicle.name
                    ),
                    None,
                )),
            );
        }
    }

    /// Once a second: installs reconnecting time out, and one left running
    /// by a UXDuck that stopped ends.
    pub fn expire_firmware(&mut self, now: u64) {
        let reconnecting: Vec<String> = self
            .setup
            .firmware
            .installs
            .values()
            .filter(|i| i.started_us.is_some())
            .map(|i| i.vehicle_id.clone())
            .collect();
        for vehicle_id in reconnecting {
            self.review_reconnect(&vehicle_id);
        }
        let orphaned: Vec<Setup> = self
            .state
            .setups
            .values()
            .filter(|s| {
                s.kind == SetupKind::Firmware
                    && s.step.running()
                    && self.install_of(&s.vehicle_id).is_none()
            })
            .cloned()
            .collect();
        for mut setup in orphaned {
            setup.step = SetupStep::Failed;
            setup.instruction = Some(Instruction::TryAgain);
            setup.detail = Some(
                "UXDuck restarted during the install. If the board has no firmware now, install again."
                    .into(),
            );
            setup.updated_at_us = now;
            setup.finished_at_us = Some(now);
            self.save_setup(setup, true, actor());
        }
    }

    /// `setup/cancel` of an install: only before erasing.
    pub(super) fn cancel_install(&mut self, vehicle: &Vehicle) -> Result<Value, ApiError> {
        let Some(mut setup) = self
            .firmware_setup(&vehicle.id)
            .filter(|s| s.step.running())
        else {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                format!("nothing is being installed on {}", vehicle.name),
            ));
        };
        let stage = setup
            .firmware
            .as_ref()
            .map_or(FirmwareStage::Restarting, |f| f.stage);
        if !stage.cancellable() {
            return Err(ApiError::new(
                ErrorKind::Busy,
                format!(
                    "It can't be cancelled once erasing has started: {} has no firmware until it is written.",
                    vehicle.name
                ),
            ));
        }
        if let Some((id, install)) = self.install_of(&vehicle.id) {
            if install.order.is_some() {
                // Not handed to the port yet: nothing to stop there.
                self.setup.firmware.installs.remove(&id);
            } else if let Some(port) = self.setup.firmware.port.as_mut() {
                port.cancel(&id);
            }
        }
        let now = self.clock.now_us();
        setup.step = SetupStep::Cancelled;
        setup.instruction = None;
        setup.detail = Some(if stage == FirmwareStage::Restarting {
            format!(
                "Cancelled. If {} restarted into its bootloader, unplug it and plug it back in.",
                vehicle.name
            )
        } else {
            "Cancelled: the board keeps its firmware.".to_owned()
        });
        setup.updated_at_us = now;
        setup.finished_at_us = Some(now);
        let saved = self.save_setup(setup, true, actor());
        to_value(&SetupDto::from(&saved))
    }

    /// Ends the install: well, with the vehicle's new version, or not.
    /// Either way in the history (V13), the link resumed.
    fn end_install(
        &mut self,
        vehicle_id: &str,
        result: Result<(), (FirmwareFailure, String, Option<u32>)>,
    ) {
        if let Some((id, _)) = self.install_of(vehicle_id) {
            self.pause_link(&id, false);
            self.setup.firmware.installs.remove(&id);
        }
        let Some(mut setup) = self.firmware_setup(vehicle_id).filter(|s| s.step.running()) else {
            return;
        };
        let vehicle = self.vehicle(vehicle_id).cloned();
        let now = self.clock.now_us();
        setup.updated_at_us = now;
        setup.finished_at_us = Some(now);
        let Some(mut firmware) = setup.firmware.take() else {
            return;
        };
        let text = match result {
            Ok(()) => {
                setup.step = SetupStep::Succeeded;
                setup.progress = 100;
                setup.instruction = None;
                firmware.after = vehicle.as_ref().and_then(|v| v.firmware.clone());
                firmware.after_type = vehicle.as_ref().map(|v| v.vehicle_type.clone());
                format!("Firmware installed: {}", change_words(&firmware))
            }
            Err((failure, detail, file_board_id)) => {
                setup.step = SetupStep::Failed;
                setup.instruction = Some(Instruction::TryAgain);
                firmware.failure = Some(failure);
                if file_board_id.is_some() {
                    firmware.file_board_id = file_board_id;
                }
                if failure == FirmwareFailure::NotReconnected {
                    // Written and verified: it is installed.
                    setup.step = SetupStep::Succeeded;
                    setup.progress = 100;
                    setup.instruction = None;
                }
                let text = format!(
                    "Firmware install failed: {detail} ({})",
                    source_words(&firmware)
                );
                setup.detail = Some(detail);
                text
            }
        };
        setup.firmware = Some(firmware);
        let event = HistoryEvent::Calibrated {
            kind: SetupKind::Firmware,
            succeeded: setup.step == SetupStep::Succeeded,
            text,
        };
        match setup.started_by.clone() {
            Some(author) => self.record_by(vehicle_id, event, author),
            None => self.record(vehicle_id, event),
        }
        let succeeded = setup.step == SetupStep::Succeeded;
        self.save_setup(setup, true, actor());
        // Its parameters are the new firmware's now.
        if succeeded && vehicle.is_some_and(|v| v.presence == Presence::Online) {
            self.download_parameters(vehicle_id);
        }
    }
}

/// Where the firmware came from, in a few words.
fn source_words(firmware: &FirmwareInstall) -> String {
    let board = firmware.board.map(|b| match &firmware.platform {
        Some(platform) => format!("board {platform} ({})", b.board_id),
        None => format!("board {}", b.board_id),
    });
    let source = match &firmware.source {
        FirmwareSource::Release {
            vehicle_type,
            release,
            ..
        } => {
            let kind = VehicleType::from_slug(vehicle_type)
                .map_or_else(|| vehicle_type.clone(), |t| t.words().to_owned());
            match &firmware.version {
                Some(version) => format!("{kind} {release} {version} from firmware.ardupilot.org"),
                None => format!("{kind} {release} from firmware.ardupilot.org"),
            }
        }
        FirmwareSource::File { name } => format!("from {name}"),
    };
    match board {
        Some(board) => format!("{source}, {board}"),
        None => source,
    }
}

/// What changed: "Quadrotor 4.6.2 → 4.7.2 (Copter stable 4.7.2 from
/// firmware.ardupilot.org, board `CubeOrange` (140)); parameters saved to
/// Alpha-2026-10-25-before-firmware.param".
fn change_words(firmware: &FirmwareInstall) -> String {
    let before = firmware.before.as_deref().map_or("unknown", version_of);
    let after = firmware.after.as_deref().map_or("unknown", version_of);
    let types = match (&firmware.before_type, &firmware.after_type) {
        (Some(b), Some(a)) if a != b => format!(" ({b} → {a})"),
        _ => String::new(),
    };
    let mut text = format!("{before} → {after}{types}; {}", source_words(firmware));
    if let Some(file) = &firmware.parameters {
        text.push_str("; parameters before saved as ");
        text.push_str(file);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_utc_days() {
        assert_eq!(date_of(0), "1970-01-01");
        // 2026-10-25T12:00:00Z.
        assert_eq!(date_of(1_792_929_600_000_000), "2026-10-25");
        assert_eq!(date_of(951_782_400_000_000), "2000-02-29");
    }

    #[test]
    fn progress_runs_through_the_stages() {
        assert_eq!(overall(FirmwareStage::Restarting, 0, 0), 0);
        assert_eq!(overall(FirmwareStage::Writing, 0, 100), 20);
        assert_eq!(overall(FirmwareStage::Writing, 50, 100), 55);
        assert_eq!(overall(FirmwareStage::Writing, 100, 100), 90);
        assert_eq!(overall(FirmwareStage::Downloading, 0, 0), 5);
        assert!(FirmwareStage::Downloading.cancellable());
        assert!(!FirmwareStage::Erasing.cancellable());
    }

    #[test]
    fn what_changed_reads_in_words() {
        let firmware = FirmwareInstall {
            stage: FirmwareStage::Reconnecting,
            done: 0,
            total: 0,
            source: FirmwareSource::Release {
                vehicle_type: "copter".into(),
                release: "stable".into(),
                platform: None,
            },
            before: Some("4.6.2".into()),
            before_type: Some("Quadrotor".into()),
            board: Some(FirmwareBoard {
                board_id: 140,
                board_rev: 0,
                bootloader: 5,
                flash_size: 2_000_000,
            }),
            file_board_id: Some(140),
            platform: Some("CubeOrange".into()),
            version: Some("4.7.2".into()),
            url: None,
            parameters: Some("Alpha-2026-10-25-120000-before-firmware.param".into()),
            failure: None,
            after: Some("4.7.2".into()),
            after_type: Some("Quadrotor".into()),
        };
        assert_eq!(
            change_words(&firmware),
            "4.6.2 → 4.7.2; Copter stable 4.7.2 from firmware.ardupilot.org, board CubeOrange \
             (140); parameters before saved as Alpha-2026-10-25-120000-before-firmware.param"
        );
    }
}
