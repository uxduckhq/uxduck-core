//! Installing firmware on this computer (S189, ADR-0037 V10): the engine's
//! [`FirmwarePort`]. ArduPilot's firmware list is fetched from its server
//! at run time and kept under `<data dir>/firmware/` with its age; releases
//! are downloaded when installed and never bundled in UXDuck's builds. The
//! board's bootloader is found on USB, by the vehicle's own port, its USB
//! ids or a known bootloader's, within its few-second window after a
//! replug, and installed through (`uxduck_mavlink::bootloader`).
//!
//! `UXDUCK_FIRMWARE_URL` (development builds) serves the list and releases
//! from elsewhere, such as a test's own server; `none` turns them off.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use uxduck_core::domain::link::UsbId;
use uxduck_core::domain::setup::{FirmwareBoard, FirmwareFailure, FirmwareStage};
use uxduck_engine::firmware::{
    FirmwarePort, FirmwareReport, InstallOrder, InstallSource, ReleasesFound, ReleasesOrder,
};
use uxduck_mavlink::bootloader::{self, Board, Bootloader, Error as BlError, Stage};
use uxduck_mavlink::firmware::{FirmwareFile, ManifestEntry, Release, read_file, read_manifest_gz};
use uxduck_mavlink::usb_ids::{UsbKind, lookup};

use crate::daemon::engine_task::EngineHandle;

/// ArduPilot's firmware server.
pub const DEFAULT_URL: &str = "https://firmware.ardupilot.org";
/// The list is fetched again when older than this.
const LIST_AGE_US: u64 = 24 * 3600 * 1_000_000;
/// The list is about 2 MB gzipped; a release about 2 MB.
const LIST_MAX: u64 = 64 * 1024 * 1024;
const RELEASE_MAX: u64 = uxduck_mavlink::firmware::FILE_MAX as u64;
/// How long the bootloader is looked for: a restart takes a few seconds,
/// a person replugging longer.
const FIND_FOR: Duration = Duration::from_secs(90);
/// How often ports are looked at: often enough to catch a bootloader's
/// few-second window after a replug.
const LOOK_EVERY: Duration = Duration::from_millis(100);

type Ports = Arc<dyn Fn() -> Vec<uxduck_mavlink::links::SerialPortInfo> + Send + Sync>;

/// The list in memory, with when it was fetched.
type Kept = Arc<tokio::sync::Mutex<Option<(Arc<Vec<ManifestEntry>>, u64)>>>;

/// The engine's port.
pub struct Host {
    dir: PathBuf,
    /// The firmware server; `None` when turned off.
    base: Option<String>,
    engine: EngineHandle,
    kept: Kept,
    cancels: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    ports: Ports,
}

impl Host {
    #[must_use]
    pub fn new(data_dir: &Path, engine: EngineHandle) -> Self {
        Self {
            dir: data_dir.join("firmware"),
            base: base_url(),
            engine,
            kept: Kept::default(),
            cancels: Arc::default(),
            ports: Arc::new(uxduck_mavlink::links::serial_ports),
        }
    }
}

/// The firmware server, or `None` when turned off.
fn base_url() -> Option<String> {
    match crate::update::url_override("UXDUCK_FIRMWARE_URL") {
        Some(url) if url == "none" => None,
        Some(url) => Some(url.trim_end_matches('/').to_owned()),
        None => Some(DEFAULT_URL.to_owned()),
    }
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}

impl FirmwarePort for Host {
    fn releases(&mut self, order: ReleasesOrder) {
        let (dir, base, kept, engine) = (
            self.dir.clone(),
            self.base.clone(),
            self.kept.clone(),
            self.engine.clone(),
        );
        tokio::spawn(async move {
            let result = list(&dir, base.as_deref(), &kept, order.refresh).await.map(
                |(entries, fetched_at_us, stale)| ReleasesFound {
                    releases: uxduck_mavlink::firmware::releases(
                        &entries,
                        &order.hint,
                        order.vehicle_type,
                    ),
                    fetched_at_us,
                    stale,
                },
            );
            let id = order.id;
            let _ = engine
                .run(move |e| e.firmware_reported(FirmwareReport::Releases { id, result }))
                .await;
        });
    }

    fn install(&mut self, order: InstallOrder) {
        let cancel = Arc::new(AtomicBool::new(false));
        if let Ok(mut cancels) = self.cancels.lock() {
            cancels.insert(order.id.clone(), cancel.clone());
        }
        let job = Job {
            dir: self.dir.clone(),
            base: self.base.clone(),
            kept: self.kept.clone(),
            engine: self.engine.clone(),
            cancel,
            ports: self.ports.clone(),
        };
        let cancels = self.cancels.clone();
        tokio::spawn(async move {
            let id = order.id.clone();
            job.run(order).await;
            if let Ok(mut cancels) = cancels.lock() {
                cancels.remove(&id);
            }
        });
    }

    fn cancel(&mut self, id: &str) {
        if let Some(flag) = self.cancels.lock().ok().and_then(|c| c.get(id).cloned()) {
            flag.store(true, Ordering::SeqCst);
        }
    }
}

/// The list: kept in memory or on disk while young, else fetched; when
/// fetching fails, the one kept, saying why.
async fn list(
    dir: &Path,
    base: Option<&str>,
    kept: &Kept,
    refresh: bool,
) -> Result<(Arc<Vec<ManifestEntry>>, u64, Option<String>), String> {
    let Some(base) = base else {
        return Err("fetching firmware is turned off in this UXDuck".into());
    };
    let mut kept = kept.lock().await;
    let now = now_us();
    let young = |at: u64| now.saturating_sub(at) < LIST_AGE_US;
    if !refresh
        && let Some((entries, at)) = kept.as_ref()
        && young(*at)
    {
        return Ok((entries.clone(), *at, None));
    }
    let path = dir.join("manifest.json.gz");
    let on_disk = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| u64::try_from(d.as_micros()).unwrap_or(0));
    if !refresh
        && let Some(at) = on_disk.filter(|at| young(*at))
        && let Ok(entries) = read_kept(&path).await
    {
        let entries = Arc::new(entries);
        *kept = Some((entries.clone(), at));
        return Ok((entries, at, None));
    }
    let url = format!("{base}/manifest.json.gz");
    match get(&url, LIST_MAX, |_, _| {}).await {
        Ok(bytes) => {
            let parsed = tokio::task::spawn_blocking({
                let bytes = bytes.clone();
                move || read_manifest_gz(&bytes)
            })
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
            let _ = std::fs::create_dir_all(dir);
            let part = dir.join("manifest.json.gz.part");
            if std::fs::write(&part, &bytes).is_ok() {
                let _ = std::fs::rename(&part, &path);
            }
            let entries = Arc::new(parsed);
            *kept = Some((entries.clone(), now));
            Ok((entries, now, None))
        }
        Err(why) => {
            if let Some((entries, at)) = kept.as_ref() {
                return Ok((entries.clone(), *at, Some(why)));
            }
            if let (Some(at), Ok(entries)) = (on_disk, read_kept(&path).await) {
                let entries = Arc::new(entries);
                *kept = Some((entries.clone(), at));
                return Ok((entries, at, Some(why)));
            }
            Err(why)
        }
    }
}

async fn read_kept(path: &Path) -> Result<Vec<ManifestEntry>, String> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
        read_manifest_gz(&bytes).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Fetches `url`, at most `max` bytes, saying how far it is.
async fn get(url: &str, max: u64, mut progress: impl FnMut(u64, u64)) -> Result<Vec<u8>, String> {
    let client = crate::sim::catalogue::client()?;
    let mut response = client
        .get(url)
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .map_err(|e| format!("cannot fetch {url}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("cannot fetch {url}: {}", response.status()));
    }
    let total = response.content_length().unwrap_or(0);
    if total > max {
        return Err(format!("{url} is larger than expected"));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(total).unwrap_or(0));
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("cannot fetch {url}: {e}"))?
    {
        bytes.extend_from_slice(&chunk);
        if bytes.len() as u64 > max {
            return Err(format!("{url} is larger than expected"));
        }
        progress(bytes.len() as u64, total);
    }
    Ok(bytes)
}

/// One install.
struct Job {
    dir: PathBuf,
    base: Option<String>,
    kept: Kept,
    engine: EngineHandle,
    cancel: Arc<AtomicBool>,
    ports: Ports,
}

/// Why an install stopped, for the engine.
struct Stopped {
    failure: FirmwareFailure,
    detail: String,
    file_board_id: Option<u32>,
}

fn stopped(failure: FirmwareFailure, detail: impl Into<String>) -> Stopped {
    Stopped {
        failure,
        detail: detail.into(),
        file_board_id: None,
    }
}

type Found = (Bootloader<Box<dyn serialport::SerialPort>>, Board);

impl Job {
    async fn report(&self, report: FirmwareReport) {
        let _ = self.engine.run(move |e| e.firmware_reported(report)).await;
    }

    async fn stage(&self, id: &str, stage: FirmwareStage, done: u64, total: u64) {
        self.report(FirmwareReport::Stage {
            id: id.to_owned(),
            stage,
            done,
            total,
        })
        .await;
    }

    async fn run(self, order: InstallOrder) {
        let id = order.id.clone();
        let outcome = self.attempt(&order).await;
        let report = match outcome {
            Ok(()) => FirmwareReport::Installed { id },
            Err(None) => FirmwareReport::Cancelled { id },
            Err(Some(stopped)) => FirmwareReport::Failed {
                id,
                failure: stopped.failure,
                detail: stopped.detail,
                file_board_id: stopped.file_board_id,
            },
        };
        self.report(report).await;
    }

    /// `Err(None)`: cancelled.
    #[allow(clippy::too_many_lines, reason = "the stages, in order")]
    async fn attempt(&self, order: &InstallOrder) -> Result<(), Option<Stopped>> {
        self.stage(&order.id, FirmwareStage::Waiting, 0, 0).await;
        let (device, usb, cancel, ports) = (
            order.device.clone(),
            order.usb.clone(),
            self.cancel.clone(),
            self.ports.clone(),
        );
        let found = tokio::task::spawn_blocking(move || {
            find_bootloader(&device, usb.as_ref(), &cancel, &ports, FIND_FOR)
        })
        .await
        .map_err(|e| Some(stopped(FirmwareFailure::Port, e.to_string())))?;
        let (mut found_bootloader, board) = match found {
            Ok(found) => found,
            Err(Looked::Cancelled) => return Err(None),
            Err(Looked::Stopped(why)) => return Err(Some(why)),
        };
        let board_info = FirmwareBoard {
            board_id: board.board_id,
            board_rev: board.board_rev,
            bootloader: board.bootloader,
            flash_size: board.flash_size,
        };
        let (name, file, release) = match &order.source {
            InstallSource::File { name, file } => (name.clone(), file.clone(), None),
            InstallSource::Release {
                vehicle_type,
                release,
                platform,
            } => {
                let resolved = match self
                    .release_for(
                        &order.id,
                        &board,
                        *vehicle_type,
                        release,
                        platform.as_deref(),
                    )
                    .await
                {
                    Ok(resolved) => resolved,
                    Err(why) => {
                        let _ = found_bootloader.reboot();
                        return Err(why);
                    }
                };
                let file = match self.download(&order.id, &resolved).await {
                    Ok(file) => file,
                    Err(why) => {
                        let _ = found_bootloader.reboot();
                        return Err(why);
                    }
                };
                let name = resolved
                    .url
                    .rsplit('/')
                    .next()
                    .unwrap_or("the release")
                    .to_owned();
                (name, file, Some(resolved))
            }
        };
        self.report(FirmwareReport::Board {
            id: order.id.clone(),
            board: board_info,
            release,
        })
        .await;
        // Written on a blocking thread; its stages come back as they go.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(Stage, u64, u64)>();
        let cancel = self.cancel.clone();
        let image = file.image.clone();
        let file_board_id = file.board_id;
        let writing = tokio::task::spawn_blocking(move || {
            let mut last: Option<(Stage, Instant, u64)> = None;
            let result = bootloader::install(
                &mut found_bootloader,
                &board,
                file_board_id,
                &image,
                &mut |stage, done, total| {
                    // Writing is said every 2% or quarter second.
                    let due = last.as_ref().is_none_or(|(s, at, d)| {
                        *s != stage
                            || done >= total
                            || at.elapsed() >= Duration::from_millis(250)
                            || done.saturating_sub(*d) * 50 >= total.max(1)
                    });
                    if due {
                        last = Some((stage, Instant::now(), done));
                        let _ = tx.send((stage, done, total));
                    }
                },
                &|| cancel.load(Ordering::SeqCst),
            );
            drop(found_bootloader);
            result
        });
        while let Some((stage, done, total)) = rx.recv().await {
            let stage = match stage {
                Stage::Erasing => FirmwareStage::Erasing,
                Stage::Writing => FirmwareStage::Writing,
                Stage::Verifying => FirmwareStage::Verifying,
                Stage::Rebooting => FirmwareStage::Starting,
            };
            self.stage(&order.id, stage, done, total).await;
        }
        let result = writing
            .await
            .map_err(|e| Some(stopped(FirmwareFailure::Port, e.to_string())))?;
        result.map_err(|error| failed(&error, &name))
    }

    /// The release of that kind for the board its bootloader named.
    async fn release_for(
        &self,
        id: &str,
        board: &Board,
        vehicle_type: uxduck_mavlink::firmware::VehicleType,
        release: &str,
        platform: Option<&str>,
    ) -> Result<Release, Option<Stopped>> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(None);
        }
        self.stage(id, FirmwareStage::Downloading, 0, 0).await;
        let (entries, _, _) = list(&self.dir, self.base.as_deref(), &self.kept, false)
            .await
            .map_err(|why| {
                Some(stopped(
                    FirmwareFailure::Download,
                    format!("The firmware list could not be fetched: {why}. The board keeps its firmware."),
                ))
            })?;
        uxduck_mavlink::firmware::resolve(&entries, board.board_id, platform, vehicle_type, release)
            .ok_or_else(|| {
                Some(stopped(
                    FirmwareFailure::NoRelease,
                    format!(
                        "The firmware server has no {} {release} for board {}. The board keeps its firmware.",
                        vehicle_type.words(),
                        board.board_id
                    ),
                ))
            })
    }

    /// Downloads a release from the server the list came from.
    async fn download(&self, id: &str, release: &Release) -> Result<FirmwareFile, Option<Stopped>> {
        let base = self.base.clone().unwrap_or_default();
        let from_server = release.url.starts_with(&format!("{base}/"))
            || release.url.starts_with(&format!("{DEFAULT_URL}/"));
        if !from_server {
            return Err(Some(stopped(
                FirmwareFailure::Download,
                format!("{} is not on the firmware server.", release.url),
            )));
        }
        let url = if release.url.starts_with(&format!("{base}/")) {
            release.url.clone()
        } else {
            // A test's server stands in for ArduPilot's.
            release.url.replacen(DEFAULT_URL, &base, 1)
        };
        let total = release.image_size.unwrap_or(0);
        let engine = self.engine.clone();
        let report_id = id.to_owned();
        let mut last = Instant::now();
        let bytes = get(&url, RELEASE_MAX, move |done, size| {
            if last.elapsed() >= Duration::from_millis(250) || done == size {
                last = Instant::now();
                let report = FirmwareReport::Stage {
                    id: report_id.clone(),
                    stage: FirmwareStage::Downloading,
                    done,
                    total: size.max(total),
                };
                let engine = engine.clone();
                tokio::spawn(async move {
                    let _ = engine.run(move |e| e.firmware_reported(report)).await;
                });
            }
        })
        .await
        .map_err(|why| {
            Some(stopped(
                FirmwareFailure::Download,
                format!(
                    "The release could not be downloaded: {why}. The board keeps its firmware."
                ),
            ))
        })?;
        if self.cancel.load(Ordering::SeqCst) {
            return Err(None);
        }
        read_file(&bytes).map_err(|e| {
            Some(stopped(
                FirmwareFailure::BadFile,
                format!("The release can't be installed: {e}. The board keeps its firmware."),
            ))
        })
    }
}

/// A bootloader error in words, with what to do.
fn failed(error: &BlError, name: &str) -> Option<Stopped> {
    use bootloader::Step;
    let after_erasing = |step: Step| {
        matches!(step, Step::Erase | Step::Write | Step::Verify)
            .then_some(" The board has no firmware until one is installed: install again.")
            .unwrap_or(" The board keeps its firmware.")
    };
    Some(match error {
        BlError::Cancelled => return None,
        BlError::BoardMismatch { file, board } => Stopped {
            failure: FirmwareFailure::BoardMismatch,
            detail: format!(
                "{name} is for board {file}, and this board is {board}. The board keeps its firmware."
            ),
            file_board_id: Some(*file),
        },
        BlError::TooLarge { image, flash } => stopped(
            FirmwareFailure::TooLarge,
            format!(
                "{name} is {image} bytes, and this board has {flash} bytes for firmware. The board keeps its firmware."
            ),
        ),
        BlError::Crc { .. } => stopped(
            FirmwareFailure::Crc,
            "What was written does not match the file. Install again: the board stays in its bootloader until it is right.",
        ),
        BlError::Unsupported { bootloader } => stopped(
            FirmwareFailure::Unsupported,
            format!(
                "Its bootloader (revision {bootloader}) is one UXDuck does not install through. Update it with another ground station first. The board keeps its firmware."
            ),
        ),
        BlError::Timeout(step) => stopped(
            FirmwareFailure::Bootloader,
            format!(
                "The bootloader stopped answering while {}.{}",
                step.words(),
                after_erasing(*step)
            ),
        ),
        BlError::Refused(step) | BlError::Garbled(step) => stopped(
            FirmwareFailure::Bootloader,
            format!(
                "The bootloader refused while {}.{}",
                step.words(),
                after_erasing(*step)
            ),
        ),
        BlError::Io(error) => stopped(
            FirmwareFailure::Port,
            format!("{error}. Check the USB cable, then install again."),
        ),
    })
}

/// How looking for the bootloader ended, when not found.
enum Looked {
    Cancelled,
    Stopped(Stopped),
}

/// Opens a serial port as UXDuck's links do: a pseudo-terminal has no line
/// speed, and macOS refuses to set one.
fn open(path: &str) -> serialport::Result<Box<dyn serialport::SerialPort>> {
    let open = |baud| {
        serialport::new(path, baud)
            .timeout(Duration::from_millis(50))
            .open()
    };
    match open(115_200) {
        Err(error) if error.description.contains("typewriter") => open(0),
        other => other,
    }
}

/// Whether a port may be the board's bootloader: its own port, its USB
/// ids, a known bootloader's, or one that names itself one.
fn candidate(
    port: &uxduck_mavlink::links::SerialPortInfo,
    device: &str,
    usb: Option<&UsbId>,
) -> bool {
    if port.device == device {
        return true;
    }
    let Some(id) = &port.usb else {
        return false;
    };
    usb.is_some_and(|u| u.vendor_id == id.vendor_id)
        || lookup(id.vendor_id, id.product_id).is_some_and(|k| k.kind == UsbKind::Bootloader)
        || port
            .description
            .as_deref()
            .is_some_and(|d| d.ends_with("-BL") || d.contains("BL "))
}

/// Looks at the ports until a bootloader answers, the time runs out or it
/// is cancelled. An STM32 in its DFU mode is named and not used.
fn find_bootloader(
    device: &str,
    usb: Option<&UsbId>,
    cancel: &AtomicBool,
    ports: &Ports,
    look_for: Duration,
) -> Result<Found, Looked> {
    let until = Instant::now() + look_for;
    while Instant::now() < until {
        if cancel.load(Ordering::SeqCst) {
            return Err(Looked::Cancelled);
        }
        let all = ports();
        if all.iter().any(|p| {
            p.usb
                .as_ref()
                .is_some_and(|u| u.vendor_id == 0x0483 && u.product_id == 0xdf11)
        }) {
            return Err(Looked::Stopped(stopped(
                FirmwareFailure::Dfu,
                "This board is in its STM32 DFU mode (USB id 0483:df11), which UXDuck does not install through. Install ArduPilot's bootloader with a DFU tool such as STM32CubeProgrammer, then install here.",
            )));
        }
        let mut paths: Vec<String> = all
            .iter()
            .filter(|p| candidate(p, device, usb))
            .map(|p| p.device.clone())
            .collect();
        // Its own path even when the system does not list it (a pty).
        if !paths.iter().any(|p| p == device) {
            paths.push(device.to_owned());
        }
        for path in paths {
            let Ok(port) = open(&path) else {
                continue;
            };
            let mut found = Bootloader::new(port);
            if found.sync(2).is_ok() {
                return match found.identify() {
                    Ok(board) => Ok((found, board)),
                    Err(error) => Err(Looked::Stopped(failed(&error, "the file").unwrap_or_else(
                        || stopped(FirmwareFailure::Bootloader, error.to_string()),
                    ))),
                };
            }
        }
        std::thread::sleep(LOOK_EVERY);
    }
    Err(Looked::Stopped(stopped(
        FirmwareFailure::NoBootloader,
        "No bootloader appeared on USB. Unplug the board, plug it back in, and install again.",
    )))
}

/// `uxduck setup firmware <port>`: a board with no vehicle, such as one
/// whose firmware is gone, installed from this process with no daemon: the
/// person plugs it in, and its bootloader is caught as it starts.
///
/// # Errors
/// Why it did not install, in words.
#[allow(
    clippy::too_many_lines,
    reason = "the stages, in order, said as they go"
)]
pub async fn install_on_port(
    data_dir: &Path,
    device: &str,
    file: Option<&Path>,
    vehicle_type: Option<&str>,
    release: Option<&str>,
    platform: Option<&str>,
) -> Result<(), String> {
    let source = if let Some(path) = file {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let name = path.file_name().map_or_else(
            || "the file".to_owned(),
            |n| n.to_string_lossy().into_owned(),
        );
        let file = read_file(&bytes).map_err(|e| format!("{name} can't be installed: {e}."))?;
        InstallSource::File { name, file }
    } else {
        let slug = vehicle_type.ok_or_else(|| {
            "Say which firmware: --vehicle-type copter (or heli, plane, rover, sub, blimp, \
             tracker) and --release stable, or --file."
                .to_owned()
        })?;
        let kind = uxduck_mavlink::firmware::VehicleType::from_slug(slug)
            .ok_or_else(|| format!("{slug} is not a vehicle type ArduPilot builds."))?;
        InstallSource::Release {
            vehicle_type: kind,
            release: release.unwrap_or("stable").to_owned(),
            platform: platform.map(str::to_owned),
        }
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let erasing = Arc::new(AtomicBool::new(false));
    {
        let (cancel, erasing) = (cancel.clone(), erasing.clone());
        tokio::spawn(async move {
            while tokio::signal::ctrl_c().await.is_ok() {
                if erasing.load(Ordering::SeqCst) {
                    eprintln!("It can't be cancelled once erasing has started.");
                } else {
                    cancel.store(true, Ordering::SeqCst);
                }
            }
        });
    }
    println!("Waiting for its bootloader: plug the board in, or unplug it and plug it back in.");
    let ports: Ports = Arc::new(uxduck_mavlink::links::serial_ports);
    let (device_owned, look_cancel) = (device.to_owned(), cancel.clone());
    let (mut found, board) = tokio::task::spawn_blocking(move || {
        find_bootloader(&device_owned, None, &look_cancel, &ports, FIND_FOR)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|looked| match looked {
        Looked::Cancelled => "Cancelled.".to_owned(),
        Looked::Stopped(why) => why.detail,
    })?;
    println!(
        "Found board {} (bootloader {}, {} KB for firmware).",
        board.board_id,
        board.bootloader,
        board.flash_size / 1024
    );
    let (name, file) = match source {
        InstallSource::File { name, file } => (name, file),
        InstallSource::Release {
            vehicle_type,
            release,
            platform,
        } => {
            let dir = data_dir.join("firmware");
            let base = base_url();
            let fetched = async {
                let (entries, _, _) = list(&dir, base.as_deref(), &Kept::default(), false).await?;
                let resolved = uxduck_mavlink::firmware::resolve(
                    &entries,
                    board.board_id,
                    platform.as_deref(),
                    vehicle_type,
                    &release,
                )
                .ok_or_else(|| {
                    format!(
                        "The firmware server has no {} {release} for board {}.",
                        vehicle_type.words(),
                        board.board_id
                    )
                })?;
                println!(
                    "Downloading {} {} for {}…",
                    vehicle_type.words(),
                    resolved.version,
                    resolved.platform
                );
                let bytes = get(&resolved.url, RELEASE_MAX, |_, _| {}).await?;
                let file = read_file(&bytes)
                    .map_err(|e| format!("the release can't be installed: {e}"))?;
                let name = resolved
                    .url
                    .rsplit('/')
                    .next()
                    .unwrap_or("the release")
                    .to_owned();
                Ok::<_, String>((name, file))
            }
            .await;
            match fetched {
                Ok(fetched) => fetched,
                Err(why) => {
                    let _ = found.reboot();
                    return Err(format!("{why} The board keeps its firmware."));
                }
            }
        }
    };
    let result = tokio::task::spawn_blocking(move || {
        let mut shown = (None, 0u64);
        bootloader::install(
            &mut found,
            &board,
            file.board_id,
            &file.image,
            &mut |stage, done, total| {
                if stage != Stage::Writing {
                    erasing.store(true, Ordering::SeqCst);
                }
                let percent = (done * 100).checked_div(total).unwrap_or(0);
                if shown.0 != Some(stage) || percent >= shown.1 + 10 {
                    shown = (Some(stage), percent);
                    match stage {
                        Stage::Erasing => println!("Erasing…"),
                        Stage::Writing => println!("Writing {percent}%"),
                        Stage::Verifying => println!("Verifying…"),
                        Stage::Rebooting => println!("Restarting…"),
                    }
                }
            },
            &|| cancel.load(Ordering::SeqCst),
        )
    })
    .await
    .map_err(|e| e.to_string())?;
    match result {
        Ok(()) => {
            println!("Installed {name}.");
            Ok(())
        }
        Err(error) => Err(failed(&error, &name).map_or_else(
            || "Cancelled: the board keeps its firmware.".to_owned(),
            |s| s.detail,
        )),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "deterministic tests")]

    use super::*;
    use uxduck_mavlink::links::SerialPortInfo;

    fn port(device: &str, vid: u16, pid: u16, description: Option<&str>) -> SerialPortInfo {
        SerialPortInfo {
            device: device.into(),
            usb: Some(UsbId {
                vendor_id: vid,
                product_id: pid,
                serial_number: None,
            }),
            description: description.map(str::to_owned),
        }
    }

    #[test]
    fn the_bootloader_is_looked_for_where_the_board_can_be() {
        let cube = UsbId {
            vendor_id: 0x2dae,
            product_id: 0x1016,
            serial_number: None,
        };
        // Its own port, and the same maker's (the Cube's bootloader).
        assert!(candidate(
            &port("/dev/a", 1, 1, None),
            "/dev/a",
            Some(&cube)
        ));
        assert!(candidate(
            &port("/dev/b", 0x2dae, 0x1001, None),
            "/dev/a",
            Some(&cube)
        ));
        // ArduPilot's generic bootloader id, and one that names itself one.
        assert!(candidate(
            &port("/dev/c", 0x1209, 0x5741, None),
            "/dev/a",
            None
        ));
        assert!(candidate(
            &port("/dev/d", 0x3162, 0x0050, Some("MatekH743-BL")),
            "/dev/a",
            None
        ));
        // A radio is not.
        assert!(!candidate(
            &port("/dev/e", 0x0403, 0x6015, None),
            "/dev/a",
            Some(&cube)
        ));
    }

    #[test]
    fn a_dfu_board_is_named_and_not_used() {
        let ports: Ports = Arc::new(|| vec![port("/dev/dfu", 0x0483, 0xdf11, None)]);
        let found = find_bootloader(
            "/dev/nothing",
            None,
            &AtomicBool::new(false),
            &ports,
            Duration::from_secs(1),
        );
        match found {
            Err(Looked::Stopped(why)) => {
                assert_eq!(why.failure, FirmwareFailure::Dfu);
                assert!(why.detail.contains("0483:df11"));
            }
            _ => panic!("not refused"),
        }
    }

    #[test]
    fn errors_say_whether_the_board_keeps_its_firmware() {
        let mismatch = failed(
            &BlError::BoardMismatch {
                file: 9,
                board: 140,
            },
            "arduplane.apj",
        )
        .unwrap();
        assert_eq!(mismatch.file_board_id, Some(9));
        assert_eq!(
            mismatch.detail,
            "arduplane.apj is for board 9, and this board is 140. The board keeps its firmware."
        );
        let erasing = failed(&BlError::Timeout(bootloader::Step::Write), "x").unwrap();
        assert!(
            erasing.detail.ends_with("install again."),
            "{}",
            erasing.detail
        );
        assert!(failed(&BlError::Cancelled, "x").is_none());
    }
}
