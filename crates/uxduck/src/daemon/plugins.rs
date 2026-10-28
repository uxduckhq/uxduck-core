//! The plugin host (ADR-0006): WebAssembly plugins through Extism, on a
//! thread of their own, never on the Tokio workers. Each call has a memory
//! cap and a time limit; a plugin that traps or overruns is unloaded and the
//! engine is told. A plugin receives what its manifest subscribes to, at
//! three levels (X1): telemetry items by key, the latest of each at its
//! cadence; MAVLink messages by name, decoded here; raw frames by id, in
//! base64. Everything is filtered before it crosses into WebAssembly, and a
//! slow plugin gets a gap marker, never an unbounded queue.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::Serialize;
use serde::de::DeserializeOwned;
use uxduck_contracts::dto::plugin::{
    CommandCall, Description, Effects, FetchRequest, FetchedInput, Frame, FrameBatch, ItemBatch,
    ItemInput, MessageBatch, MessageInput, PluginDraft, PluginManifest, PluginTestResult,
    PluginUsage, StartInput, Subscriptions, TestedItem, TickInput,
};
use uxduck_core::domain::telemetry::VehicleTelemetry;
use uxduck_engine::runtime::{DraftOrder, PluginLoad, PluginPort, TestOrder};

use super::data_plane::{PluginFeed, RawFrame};
use super::engine_task::EngineHandle;

/// A plugin's memory, at most: 256 pages of 64 KiB, 16 MiB.
const MEMORY_PAGES: u32 = 256;
/// Work allowed per call, including initialization. Fuel counts executed
/// WebAssembly instructions, so host suspension cannot exhaust the budget.
const CALL_FUEL: u64 = 100_000_000;

/// The shortest tick a plugin may ask for (S101): ten calls a second.
const MIN_TICK_MS: u32 = 100;
/// The most a plugin keeps across restarts (S104).
const STORE_BYTES: usize = 64 * 1024;
/// How far back the Plugins page's figures look (S104).
const USAGE_WINDOW: Duration = Duration::from_secs(60);
/// How often the host tells the engine what plugins used.
const USAGE_EVERY: Duration = Duration::from_secs(5);

/// Writes a file whole or not at all.
fn write_atomically(file: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = file.parent().ok_or("no folder")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let partial = file.with_extension("partial");
    std::fs::write(&partial, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&partial, file).map_err(|e| e.to_string())
}
/// Messages or frames held for one plugin between batches; older ones are
/// dropped.
const HELD: usize = 1000;

/// Reads and checks a plugin directory: `plugin.json`, and the module it
/// names beside it.
///
/// # Errors
/// When the manifest is missing, invalid, or names a module elsewhere.
pub fn inspect(path: &str) -> Result<PluginManifest, String> {
    let dir = Path::new(path);
    let text = std::fs::read_to_string(dir.join("plugin.json"))
        .map_err(|e| format!("{path}: no readable plugin.json ({e})"))?;
    let manifest: PluginManifest =
        serde_json::from_str(&text).map_err(|e| format!("{path}/plugin.json: {e}"))?;
    // A module, or a script (X6, X7): one or the other.
    let file = match (&manifest.script, manifest.wasm.is_empty()) {
        (Some(script), true) => script.clone(),
        (None, false) => manifest.wasm.clone(),
        _ => {
            return Err(format!(
                "{path}/plugin.json: name a module in `wasm` or a script in `script`, one of them"
            ));
        }
    };
    let inside = Path::new(&file)
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    if !inside {
        return Err(format!("{file}: it must be a file beside plugin.json"));
    }
    if !dir.join(&file).is_file() {
        return Err(format!("{path}: {file} is missing"));
    }
    if manifest.script.is_some() {
        let extension = Path::new(&file).extension().and_then(|e| e.to_str());
        if !matches!(extension, Some("ts" | "mts" | "js" | "mjs")) {
            return Err(format!("{file}: a script is a .ts or .js file"));
        }
        // Its types must strip now, not when it first runs.
        let source =
            std::fs::read_to_string(dir.join(&file)).map_err(|e| format!("{file}: {e}"))?;
        super::script::strip_types(Path::new(&file), &source)?;
    }
    // Its vendor's messages (S60): readable, and clashing with nothing.
    let own: Vec<String> = match &manifest.dialect {
        Some(file) => {
            let messages = read_dialect(dir, file)?;
            uxduck_mavlink::named::check(&manifest.id, &messages)
                .map_err(|why| format!("{file}: {why}"))?;
            messages.iter().map(|m| m.name().to_owned()).collect()
        }
        None => Vec::new(),
    };
    // Messages by name must be the dialect's or its own (X1, X4).
    let named = manifest
        .subscribe
        .messages
        .iter()
        .flat_map(|m| m.names.iter())
        .chain(&manifest.sends);
    let unknown: Vec<&String> = named
        .filter(|name| uxduck_mavlink::named::id_of(name).is_none() && !own.contains(name))
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "{path}/plugin.json: no message {} in the ArduPilot dialect or its own",
            unknown
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(manifest)
}

/// A plugin's dialect file, beside its manifest (S60).
fn read_dialect(dir: &Path, file: &str) -> Result<Vec<uxduck_mavlink::named::Definition>, String> {
    let plain = Path::new(file)
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    let xml = Path::new(file)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("xml"));
    if !plain || !xml {
        return Err(format!(
            "{file}: a dialect is an .xml file beside plugin.json"
        ));
    }
    let xml = std::fs::read_to_string(dir.join(file)).map_err(|e| format!("{file}: {e}"))?;
    uxduck_mavlink::named::parse_dialect(&xml).map_err(|why| format!("{file}: {why}"))
}

/// Makes a plugin's vendor messages known while it runs, or forgets them.
///
/// # Errors
/// When its dialect does not read, or clashes with another's.
pub fn register_dialect(id: &str, path: Option<&str>) -> Result<(), String> {
    let Some(path) = path else {
        uxduck_mavlink::named::unregister(id);
        return Ok(());
    };
    let path = Path::new(path);
    let (dir, file) = (
        path.parent().unwrap_or(Path::new(".")),
        path.file_name()
            .and_then(|f| f.to_str())
            .unwrap_or_default(),
    );
    let messages = read_dialect(dir, file)?;
    uxduck_mavlink::named::register(id, messages).map_err(|why| format!("{file}: {why}"))
}

enum Message {
    Load(PluginLoad),
    /// The answer to a plugin's `fetch` (S170), or why there was none.
    Fetched(String, FetchedInput),
    Unload(String),
    Command(String, CommandCall),
    Test(TestOrder),
    Frame(RawFrame),
    Items(Vec<ItemInput>),
}

/// The engine's end: never blocks, but for writing a draft's few files.
#[derive(Debug)]
pub struct PluginChannel {
    sender: Sender<Message>,
    /// Where agents' drafts are written (X12).
    drafts: PathBuf,
    /// The folders of the plugins UXDuck comes with, written out (S104).
    included: Vec<String>,
}

/// The plugins UXDuck comes with (S104): each one's id and files, from the
/// repository's own `plugins/` folder.
const INCLUDED: &[(&str, &[(&str, &str)])] = &[
    (
        "clock",
        &[
            (
                "plugin.json",
                include_str!("../../../../plugins/clock-ts/plugin.json"),
            ),
            (
                "index.ts",
                include_str!("../../../../plugins/clock-ts/index.ts"),
            ),
        ],
    ),
    // Traffic from adsb.lol (S171): off until a person turns it on.
    (
        "adsb-lol",
        &[
            (
                "plugin.json",
                include_str!("../../../../plugins/adsb-lol-ts/plugin.json"),
            ),
            (
                "index.ts",
                include_str!("../../../../plugins/adsb-lol-ts/index.ts"),
            ),
        ],
    ),
];

/// Writes UXDuck's own copy of each plugin it comes with into `into`, a
/// folder per plugin, and returns the folders written.
fn write_included(into: &Path) -> Vec<String> {
    INCLUDED
        .iter()
        .filter_map(|(id, files)| {
            let dir = into.join(id);
            let written = files
                .iter()
                .try_for_each(|(name, text)| write_atomically(&dir.join(name), text.as_bytes()));
            match written {
                Ok(()) => Some(dir.display().to_string()),
                Err(error) => {
                    tracing::warn!(plugin = %id, %error, "could not write an included plugin");
                    None
                }
            }
        })
        .collect()
}

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Message")
    }
}

/// What some plugin wants, which the data plane checks before copying.
#[derive(Debug, Default)]
pub struct Wants {
    /// Message ids wanted raw or decoded.
    frames: BTreeSet<u32>,
    /// Item keys and `prefix.*` patterns.
    items: Vec<String>,
}

pub type Subscribed = Arc<RwLock<Wants>>;

/// Whether a key matches `battery.voltage_v` or `battery.*`.
#[must_use]
pub fn key_matches(pattern: &str, key: &str) -> bool {
    pattern
        .strip_suffix('*')
        .map_or(pattern == key, |prefix| key.starts_with(prefix))
}

/// Where the data plane sends what plugins asked for.
#[derive(Clone, Debug)]
pub struct Feed {
    sender: Sender<Message>,
    subscribed: Subscribed,
}

impl Feed {
    /// As the data plane holds it.
    #[must_use]
    pub fn boxed(self) -> PluginFeed {
        let (frames, items, sends) = (self.clone(), self.clone(), self.clone());
        PluginFeed {
            wants_frame: Box::new(move |id| frames.wants_frame(id)),
            frame: Box::new(move |frame| {
                let _ = self.sender.send(Message::Frame(frame));
            }),
            wants_items: Box::new(move || {
                items.subscribed.read().is_ok_and(|w| !w.items.is_empty())
            }),
            items: Box::new(move |update| sends.items(update)),
        }
    }

    #[must_use]
    pub fn wants_frame(&self, message_id: u32) -> bool {
        self.subscribed
            .read()
            .is_ok_and(|w| w.frames.contains(&message_id))
    }

    /// The items some plugin subscribed to, from one telemetry update.
    pub fn items(&self, update: &VehicleTelemetry) {
        let Ok(wants) = self.subscribed.read() else {
            return;
        };
        let items: Vec<ItemInput> = item_inputs(update)
            .into_iter()
            .filter(|item| wants.items.iter().any(|p| key_matches(p, &item.key)))
            .collect();
        drop(wants);
        if !items.is_empty() {
            let _ = self.sender.send(Message::Items(items));
        }
    }
}

/// A unit as a plugin built for ABI 2 knows it: units added since (S72)
/// arrive as `none`, which such a plugin can read, and the key still says
/// the unit (`_mps2`, `_mah`, `_degc`).
fn plugin_unit(
    unit: uxduck_core::domain::telemetry::Unit,
) -> uxduck_contracts::dto::telemetry::Unit {
    use uxduck_core::domain::telemetry::Unit as U;
    match unit {
        U::MeterPerSecondSquared | U::MilliampHour | U::DegreeCelsius => {
            uxduck_contracts::dto::telemetry::Unit::None
        }
        other => other.into(),
    }
}

/// A telemetry update's items as plugins receive them.
#[must_use]
pub fn item_inputs(update: &VehicleTelemetry) -> Vec<ItemInput> {
    update
        .items
        .iter()
        .map(|item| ItemInput {
            vehicle_id: update.vehicle_id.clone(),
            key: item.key.clone(),
            label: item.label.clone(),
            unit: plugin_unit(item.unit),
            value: item.value,
            text: item.text.clone(),
            time_us: item.time_us,
        })
        .collect()
}

impl PluginPort for PluginChannel {
    fn inspect(&self, path: &str) -> Result<PluginManifest, String> {
        inspect(path)
    }
    fn load(&mut self, plugin: PluginLoad) {
        let _ = self.sender.send(Message::Load(plugin));
    }
    fn unload(&mut self, id: &str) {
        let _ = self.sender.send(Message::Unload(id.to_owned()));
    }
    fn command(&mut self, id: &str, call: CommandCall) {
        let _ = self.sender.send(Message::Command(id.to_owned(), call));
    }
    fn draft(&mut self, order: DraftOrder) -> Result<PluginDraft, String> {
        write_draft(&self.drafts, order)
    }
    fn drafts(&self) -> Vec<PluginDraft> {
        list_drafts(&self.drafts)
    }
    fn test(&mut self, order: TestOrder) -> bool {
        self.sender.send(Message::Test(order)).is_ok()
    }
    fn included(&self) -> Vec<String> {
        self.included.clone()
    }
}

/// Writes a draft: `plugin.json` and `index.ts`, a new version each time,
/// checked as an install would check it.
fn write_draft(drafts: &Path, order: DraftOrder) -> Result<PluginDraft, String> {
    let params = order.params;
    let dir = drafts.join(&params.id);
    let previous = std::fs::read_to_string(dir.join("plugin.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<PluginManifest>(&text).ok());
    let next = previous
        .as_ref()
        .and_then(|m| m.version.rsplit('.').next()?.parse::<u32>().ok())
        .map_or(0, |n| n + 1);
    let manifest = PluginManifest {
        abi: uxduck_contracts::dto::plugin::PLUGIN_ABI,
        id: params.id.clone(),
        name: params.name.trim().to_owned(),
        version: format!("0.1.{next}"),
        description: params.description,
        wasm: String::new(),
        script: Some("index.ts".into()),
        capabilities: params
            .capabilities
            .unwrap_or_else(|| vec![uxduck_contracts::dto::initialize::Capability::Read]),
        subscribe: params.subscribe,
        sends: params.sends,
        commands: params.commands,
        written_by: Some(order.written_by),
        dialect: None,
        // An agent's draft fetches nothing until a person adds hosts.
        hosts: Vec::new(),
        traffic_source: false,
        starts_off: false,
        // A person adds checks (S194), as hosts.
        checks: Vec::new(),
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let text = serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("plugin.json"), text).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("index.ts"), &params.script).map_err(|e| e.to_string())?;
    // A mistake now, while the agent can still fix it.
    inspect(&dir.display().to_string())?;
    Ok(draft_of(&dir, &manifest))
}

fn draft_of(dir: &Path, manifest: &PluginManifest) -> PluginDraft {
    let updated_at_us = std::fs::metadata(dir.join("index.ts"))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX));
    PluginDraft {
        id: manifest.id.clone(),
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        path: dir.display().to_string(),
        written_by: manifest.written_by.clone(),
        updated_at_us,
    }
}

/// Every draft, newest first.
fn list_drafts(drafts: &Path) -> Vec<PluginDraft> {
    let mut found: Vec<PluginDraft> = std::fs::read_dir(drafts)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let dir = entry.path();
            let text = std::fs::read_to_string(dir.join("plugin.json")).ok()?;
            let manifest: PluginManifest = serde_json::from_str(&text).ok()?;
            Some(draft_of(&dir, &manifest))
        })
        .collect();
    found.sort_by_key(|d| std::cmp::Reverse(d.updated_at_us));
    found
}

/// Runs a draft against a vehicle's last minutes and keeps what the agent
/// needs: calls, items' last values, notices, log lines, the first error.
fn run_test(order: &TestOrder, tlogs: &Path) -> Result<PluginTestResult, String> {
    const MOST: usize = 20;
    let now_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX));
    let flight = order
        .flight
        .as_ref()
        .map(|f| crate::feedback::last_minutes(tlogs, f, now_us))
        .unwrap_or_default();
    let records = uxduck_mavlink::tlog::read(&flight);
    let mut result = PluginTestResult {
        flight_s: match (records.first(), records.last()) {
            #[allow(clippy::cast_precision_loss, reason = "seconds of a flight")]
            (Some((first, _)), Some((last, _))) => (last - first) as f64 / 1e6,
            _ => 0.0,
        },
        ..PluginTestResult::default()
    };
    let mut items: BTreeMap<String, TestedItem> = BTreeMap::new();
    let mut checks: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut log = VecDeque::new();
    let command = order.command.clone().map(|c| {
        (
            c,
            order
                .params
                .clone()
                .unwrap_or_else(|| serde_json::json!({})),
        )
    });
    let description = harness(
        Path::new(&order.path),
        &flight,
        command,
        &mut |function, _, outcome| {
            *result.calls.entry(function.to_owned()).or_default() += 1;
            match outcome {
                Ok(effects) => {
                    for item in &effects.items {
                        let entry = items.entry(item.key.clone()).or_insert_with(|| TestedItem {
                            key: item.key.clone(),
                            label: item.label.clone(),
                            unit: item.unit,
                            value: None,
                            text: None,
                            count: 0,
                        });
                        entry.value = item.value;
                        entry.text.clone_from(&item.text);
                        entry.count += 1;
                    }
                    for notice in &effects.notifications {
                        if result.notifications.len() < MOST {
                            result.notifications.push(notice.title.clone());
                        }
                    }
                    result.features += u32::try_from(effects.features.len()).unwrap_or(u32::MAX);
                    // The latest report of each check (S194).
                    for check in &effects.checks {
                        let status = serde_json::to_value(check.status)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_owned))
                            .unwrap_or_default();
                        let words = check
                            .detail
                            .as_deref()
                            .map(|d| format!(": {d}"))
                            .unwrap_or_default();
                        checks.insert(
                            (check.id.clone(), check.vehicle_id.clone()),
                            format!("{} on {}: {status}{words}", check.id, check.vehicle_id),
                        );
                    }
                    for line in &effects.log {
                        log.push_back(line.clone());
                        if log.len() > MOST {
                            log.pop_front();
                        }
                    }
                }
                Err(why) => result.error = Some(why.clone()),
            }
        },
    )?;
    result.panels = description.panels;
    result.items = items.into_values().collect();
    result.checks = checks.into_values().take(MOST).collect();
    result.log = log.into_iter().collect();
    Ok(result)
}

/// The two ends, before the host starts.
pub struct Host {
    receiver: Receiver<Message>,
    /// For answers to fetches, which come back on the same channel.
    sender: Sender<Message>,
    /// Where fetches run, once the host has started.
    runtime: Option<tokio::runtime::Handle>,
    subscribed: Subscribed,
    /// The links' tlogs, for a draft's test against a vehicle's last minutes.
    tlogs: PathBuf,
    /// What plugins keep across restarts, a file each (S104).
    storage: PathBuf,
}

#[must_use]
pub fn channel(plugins: &Path, tlogs: PathBuf) -> (PluginChannel, Feed, Host) {
    let (sender, receiver) = std::sync::mpsc::channel();
    let subscribed = Subscribed::default();
    (
        PluginChannel {
            sender: sender.clone(),
            drafts: plugins.join("drafts"),
            included: write_included(&plugins.join("included")),
        },
        Feed {
            sender: sender.clone(),
            subscribed: subscribed.clone(),
        },
        Host {
            receiver,
            sender,
            runtime: None,
            subscribed,
            tlogs,
            storage: plugins.join("storage"),
        },
    )
}

/// What a plugin kept, from its file; nothing when there is none or it
/// does not read.
fn read_stored(file: &Path) -> serde_json::Map<String, serde_json::Value> {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// What the host tells the engine.
enum Report {
    Described(String, Description),
    Effects(String, Effects),
    Failed(String, String),
    /// What running plugins used in the last minute (S104).
    Used(Vec<PluginUsage>),
    Tested(
        uxduck_engine::ConnectionId,
        serde_json::Value,
        Result<PluginTestResult, String>,
    ),
}

/// One input level's queue and when it is next delivered.
struct Queue<T> {
    held: T,
    gap: bool,
    cadence: Duration,
    next: Instant,
}

impl<T: Default> Queue<T> {
    fn new(cadence_ms: u32) -> Self {
        Self {
            held: T::default(),
            gap: false,
            cadence: Duration::from_millis(u64::from(cadence_ms)),
            next: Instant::now(),
        }
    }
}

/// The latest item of each key, by vehicle and key.
type LatestItems = BTreeMap<(String, String), ItemInput>;

/// The wall clock at `at`, in milliseconds since 1970: now, in the daemon;
/// on the tlog's clock, in the harness.
fn wall_ms(at: Instant) -> u64 {
    if let Some(clock) = REPLAY_CLOCK.get() {
        let elapsed = at.saturating_duration_since(clock.origin);
        return clock
            .epoch_ms
            .saturating_add(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX));
    }
    let system = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    let here = Instant::now();
    let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    if at >= here {
        system.saturating_add(ms(at - here))
    } else {
        system.saturating_sub(ms(here - at))
    }
}

#[derive(Clone, Copy)]
struct ReplayClock {
    origin: Instant,
    epoch_ms: u64,
    at: Instant,
}

thread_local! {
    static REPLAY_CLOCK: std::cell::Cell<Option<ReplayClock>> = const { std::cell::Cell::new(None) };
}

/// Tests run on the host thread too. Restore its clock on every exit path.
struct ReplayClockGuard(Option<ReplayClock>);

impl Drop for ReplayClockGuard {
    fn drop(&mut self) {
        REPLAY_CLOCK.set(self.0);
    }
}

/// One call at a cadence: the function, its input, and its effects.
pub struct Delivered {
    pub function: &'static str,
    pub input: serde_json::Value,
    pub result: Result<Effects, String>,
}

fn delivered<I: Serialize>(
    plugin: &mut extism::Plugin,
    function: &'static str,
    input: &I,
) -> Delivered {
    Delivered {
        function,
        input: serde_json::to_value(input).unwrap_or_default(),
        result: effects(plugin, function, input),
    }
}

/// One loaded plugin, with what it subscribed to resolved. The harness
/// runs it too, on the tlog's clock.
pub struct Running {
    plugin: extism::Plugin,
    items: Option<(Vec<String>, Queue<LatestItems>)>,
    messages: Option<(BTreeSet<u32>, Queue<VecDeque<MessageInput>>)>,
    frames: Option<(BTreeSet<u32>, Queue<VecDeque<Frame>>)>,
    /// `on_tick` at its own cadence, whatever arrives (S101).
    tick: Option<Queue<()>>,
    /// Each call in the last minute: when, and how long it ran (S104).
    spent: VecDeque<(Instant, Duration)>,
    /// What it keeps across restarts (S104), and the file that holds it;
    /// no file in the harness.
    stored: serde_json::Map<String, serde_json::Value>,
    store_file: Option<PathBuf>,
    /// A wake-up it asked for (S104): `on_tick` once, then.
    wake: Option<Instant>,
    /// The hosts it may fetch from (S170), and how many fetches are out.
    hosts: Vec<String>,
    fetching: usize,
    /// The checks its manifest declares (S194), for the harness to refuse
    /// a report of any other, as the engine ignores it.
    checks: Vec<String>,
    traffic: Option<uxduck_contracts::dto::traffic::TrafficFeedContext>,
}

impl Running {
    #[must_use]
    pub fn new(plugin: extism::Plugin, subscribe: &Subscriptions) -> Self {
        Self {
            plugin,
            items: subscribe
                .items
                .as_ref()
                .map(|s| (s.keys.clone(), Queue::new(s.cadence_ms))),
            messages: subscribe.messages.as_ref().map(|s| {
                let ids = s
                    .names
                    .iter()
                    .filter_map(|n| uxduck_mavlink::named::id_of(n))
                    .collect();
                (ids, Queue::new(s.cadence_ms))
            }),
            frames: subscribe
                .frames
                .as_ref()
                .map(|s| (s.ids.iter().copied().collect(), Queue::new(s.cadence_ms))),
            tick: subscribe.tick_ms.map(|ms| Queue::new(ms.max(MIN_TICK_MS))),
            spent: VecDeque::new(),
            stored: serde_json::Map::new(),
            store_file: None,
            wake: None,
            hosts: Vec::new(),
            fetching: 0,
            checks: Vec::new(),
            traffic: None,
        }
    }

    /// With the checks its manifest declares (S194), which the harness
    /// holds its reports to.
    #[must_use]
    pub fn declaring(mut self, checks: &[uxduck_contracts::dto::plugin::DeclaredCheck]) -> Self {
        self.checks = checks.iter().map(|c| c.id.clone()).collect();
        self
    }

    /// The hosts it may fetch from, from its manifest (S170).
    #[must_use]
    pub fn with_hosts(mut self, hosts: Vec<String>) -> Self {
        self.hosts = hosts;
        self
    }

    /// Calls `on_fetched` with the answer to a fetch (S170).
    ///
    /// # Errors
    /// When the call traps, times out or returns something else.
    pub fn fetched(&mut self, input: &FetchedInput) -> Result<Effects, String> {
        self.fetching = self.fetching.saturating_sub(1);
        self.timed("on_fetched", input, Instant::now()).result
    }

    /// The soonest a held queue is due.
    #[must_use]
    pub fn due_at(&self) -> Option<Instant> {
        [
            self.items
                .as_ref()
                .filter(|(_, q)| !q.held.is_empty())
                .map(|(_, q)| q.next),
            self.messages
                .as_ref()
                .filter(|(_, q)| !q.held.is_empty())
                .map(|(_, q)| q.next),
            self.frames
                .as_ref()
                .filter(|(_, q)| !q.held.is_empty())
                .map(|(_, q)| q.next),
            self.tick.as_ref().map(|q| q.next),
            self.wake,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// When `on_tick` is next due, at its cadence or a wake-up it asked for.
    #[must_use]
    pub fn next_tick(&self) -> Option<Instant> {
        [self.tick.as_ref().map(|q| q.next), self.wake]
            .into_iter()
            .flatten()
            .min()
    }

    /// Calls `on_command`.
    ///
    /// # Errors
    /// When the call traps, times out or returns something else.
    pub fn command(&mut self, call: &CommandCall) -> Result<Effects, String> {
        self.timed("on_command", call, Instant::now()).result
    }

    /// Calls a function, noting how long it ran.
    fn timed<I: Serialize>(&mut self, function: &'static str, input: &I, at: Instant) -> Delivered {
        REPLAY_CLOCK.set(REPLAY_CLOCK.get().map(|clock| ReplayClock { at, ..clock }));
        let started = Instant::now();
        let delivered = delivered(&mut self.plugin, function, input);
        let now = Instant::now();
        if let Ok(Some(ms)) = delivered.result.as_ref().map(|e| e.wake_in_ms) {
            self.wake = Some(at + Duration::from_millis(u64::from(ms.max(MIN_TICK_MS))));
        }
        self.spent.push_back((now, now - started));
        while self
            .spent
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) > USAGE_WINDOW)
        {
            self.spent.pop_front();
        }
        delivered
    }

    /// Hands it what it kept (S104), once, after `describe`: `on_start`.
    /// With a file, what it keeps from now on is written there.
    ///
    /// # Errors
    /// When `on_start` fails.
    pub fn start(
        &mut self,
        stored: serde_json::Map<String, serde_json::Value>,
        store_file: Option<PathBuf>,
        now: Instant,
    ) -> Result<Effects, String> {
        self.stored = stored;
        self.store_file = store_file;
        let input = StartInput {
            stored: self.stored.clone(),
            now_ms: wall_ms(now),
        };
        self.timed("on_start", &input, now).result
    }

    /// Keeps what a call asked to keep (S104): a value per key, `null`
    /// removing one, at most `STORE_BYTES` in all, written at once.
    ///
    /// # Errors
    /// When it would keep too much, or the file cannot be written; what it
    /// kept before stays.
    pub fn keep(
        &mut self,
        store: serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), String> {
        if store.is_empty() {
            return Ok(());
        }
        let mut next = self.stored.clone();
        for (key, value) in store {
            if value.is_null() {
                next.remove(&key);
            } else {
                next.insert(key, value);
            }
        }
        let text = serde_json::to_string(&next).map_err(|e| e.to_string())?;
        if text.len() > STORE_BYTES {
            return Err(format!(
                "The plugin requested {} bytes of storage. A plugin can store at most {STORE_BYTES} bytes.",
                text.len()
            ));
        }
        if let Some(file) = &self.store_file {
            write_atomically(file, text.as_bytes())?;
        }
        self.stored = next;
        Ok(())
    }

    /// What it used in the last minute (S104).
    #[must_use]
    pub fn usage(&self, id: &str, now: Instant) -> PluginUsage {
        let recent = || {
            self.spent
                .iter()
                .filter(move |(at, _)| now.duration_since(*at) <= USAGE_WINDOW)
        };
        let busy: Duration = recent().map(|(_, took)| *took).sum();
        let slowest = recent().map(|(_, took)| *took).max().unwrap_or_default();
        PluginUsage {
            plugin_id: id.to_owned(),
            calls_per_min: u32::try_from(recent().count()).unwrap_or(u32::MAX),
            busy_pct: busy.as_secs_f64() / USAGE_WINDOW.as_secs_f64() * 100.0,
            slowest_ms: slowest.as_secs_f64() * 1000.0,
            storage_bytes: u32::try_from(
                serde_json::to_string(&self.stored).map_or(0, |t| t.len()),
            )
            .unwrap_or(u32::MAX),
            storage_limit_bytes: u32::try_from(STORE_BYTES).unwrap_or(u32::MAX),
            memory_limit_bytes: MEMORY_PAGES * 64 * 1024,
        }
    }

    pub fn take_frame(&mut self, frame: &RawFrame) {
        if let Some((ids, queue)) = &mut self.messages
            && ids.contains(&frame.message_id)
            && let Some(decoded) = uxduck_mavlink::named::decode(frame.message_id, &frame.payload)
        {
            if queue.held.len() >= HELD {
                queue.held.pop_front();
                queue.gap = true;
            }
            queue.held.push_back(MessageInput {
                vehicle_id: frame.vehicle_id.clone(),
                system_id: frame.system_id,
                component_id: frame.component_id,
                name: decoded.name,
                fields: decoded.fields,
            });
        }
        if let Some((ids, queue)) = &mut self.frames
            && ids.contains(&frame.message_id)
        {
            if queue.held.len() >= HELD {
                queue.held.pop_front();
                queue.gap = true;
            }
            queue.held.push_back(Frame {
                vehicle_id: frame.vehicle_id.clone(),
                message_id: frame.message_id,
                system_id: frame.system_id,
                component_id: frame.component_id,
                payload: base64::engine::general_purpose::STANDARD.encode(&frame.payload),
            });
        }
    }

    pub fn take_items(&mut self, items: &[ItemInput]) {
        if let Some((keys, queue)) = &mut self.items {
            for item in items {
                if keys.iter().any(|p| key_matches(p, &item.key)) {
                    queue
                        .held
                        .insert((item.vehicle_id.clone(), item.key.clone()), item.clone());
                }
            }
        }
    }

    /// Delivers whatever is due, one call per level, then a tick if one is
    /// due: the function, what it was given, and what it returned.
    pub fn deliver(&mut self, now: Instant) -> Vec<Delivered> {
        let mut results = self.deliver_held(now);
        let cadence = self.tick.as_ref().is_some_and(|q| q.next <= now);
        let woken = self.wake.is_some_and(|at| at <= now);
        if cadence || woken {
            if let Some(queue) = self.tick.as_mut().filter(|_| cadence) {
                // On cadence from when it was due, not from when it ran, so
                // a clock does not drift; a tick missed is skipped.
                queue.next = (queue.next + queue.cadence).max(now);
            }
            if woken {
                self.wake = None;
            }
            let input = TickInput {
                now_ms: wall_ms(now),
                traffic: self.traffic.clone(),
            };
            results.push(self.timed("on_tick", &input, now));
        }
        results
    }

    /// Delivers what is held and due, one call per level, without a tick.
    pub fn deliver_held(&mut self, now: Instant) -> Vec<Delivered> {
        let mut results = Vec::new();
        if let Some((_, queue)) = &mut self.items
            && !queue.held.is_empty()
            && queue.next <= now
        {
            let batch = ItemBatch {
                items: std::mem::take(&mut queue.held).into_values().collect(),
            };
            queue.next = now + queue.cadence;
            results.push(self.timed("on_items", &batch, now));
        }
        if let Some((_, queue)) = &mut self.messages
            && !queue.held.is_empty()
            && queue.next <= now
        {
            let batch = MessageBatch {
                messages: queue.held.drain(..).collect(),
                gap: std::mem::take(&mut queue.gap),
            };
            queue.next = now + queue.cadence;
            results.push(self.timed("on_messages", &batch, now));
        }
        if let Some((_, queue)) = &mut self.frames
            && !queue.held.is_empty()
            && queue.next <= now
        {
            let batch = FrameBatch {
                frames: queue.held.drain(..).collect(),
                gap: std::mem::take(&mut queue.gap),
            };
            queue.next = now + queue.cadence;
            results.push(self.timed("on_frames", &batch, now));
        }
        results
    }
}

/// What hears each call of a harness run: the function, its input, and its
/// effects or why it failed.
pub type Hears<'a> = dyn FnMut(&str, &serde_json::Value, &Result<Effects, String>) + 'a;

fn report_replay(running: &mut Running, delivered: Delivered, each: &mut Hears<'_>) -> bool {
    let result = delivered.result.and_then(|effects| {
        for item in &effects.items {
            if !uxduck_core::domain::telemetry::key_says_unit(&item.key, item.unit.into()) {
                return Err(format!("{}: the key does not say its unit", item.key));
            }
        }
        // Only the checks it declares (S194): the daemon ignores the rest.
        if let Some(check) = effects
            .checks
            .iter()
            .find(|c| !running.checks.contains(&c.id))
        {
            return Err(format!(
                "check {}: not in plugin.json's checks, so UXDuck ignores it",
                check.id
            ));
        }
        running.keep(effects.store.clone())?;
        Ok(effects)
    });
    each(delivered.function, &delivered.input, &result);
    result.is_ok()
}

/// Runs a plugin as the daemon would, against a tlog on the tlog's clock:
/// items from the daemon's own producers, decoded messages and frames at
/// each cadence, then `command`. `each` hears every call: the function, its
/// input, and its effects or why it failed; the run stops at a failure.
/// Returns what `describe` said.
///
/// # Errors
/// When the plugin does not load.
pub fn harness(
    path: &Path,
    tlog: &[u8],
    command: Option<(String, serde_json::Value)>,
    each: &mut Hears<'_>,
) -> Result<Description, String> {
    use uxduck_mavlink::session::{Config, Effect, Session};
    let manifest = inspect(&path.display().to_string())?;
    let file = manifest.script.as_deref().unwrap_or(&manifest.wasm);
    let module = path.join(file).display().to_string();
    let dialect = manifest
        .dialect
        .as_ref()
        .map(|d| path.join(d).display().to_string());
    register_dialect(&manifest.id, dialect.as_deref())?;
    let (plugin, description) = load(&module, manifest.script.is_some())?;
    let mut running = Running::new(plugin, &manifest.subscribe).declaring(&manifest.checks);
    let start = Instant::now();
    let epoch_ms = wall_ms(start);
    let _clock = ReplayClockGuard(REPLAY_CLOCK.replace(Some(ReplayClock {
        origin: start,
        epoch_ms,
        at: start,
    })));
    // What it keeps stays in memory here; it starts with nothing kept.
    let started = running.start(serde_json::Map::new(), None, start);
    let start_input = serde_json::json!({ "stored": {}, "nowMs": epoch_ms });
    if !report_replay(
        &mut running,
        Delivered {
            function: "on_start",
            input: start_input,
            result: started,
        },
        each,
    ) {
        return Ok(description);
    }
    let records = uxduck_mavlink::tlog::read(tlog);
    let start_us = records.first().map_or(0, |(at, _)| *at);
    let end_us = records.last().map_or(start_us, |(at, _)| *at);
    let at = |us: u64| start + Duration::from_micros(us.saturating_sub(start_us));
    // The session gives the daemon's vehicle ids and its telemetry items.
    let mut session = Session::new(Config::default());
    let mut vehicle = String::from("tlog-1");
    let mut next_tick = start_us;
    for (at_us, frame) in records {
        let Some(header) = uxduck_mavlink::framing::header(frame) else {
            continue;
        };
        let mut effects = session.ingest("tlog", "tlog", frame, at_us);
        if at_us >= next_tick {
            effects.extend(session.tick(at_us));
            next_tick = at_us + 100_000;
        }
        for effect in &effects {
            if let Effect::Telemetry(update) = effect {
                running.take_items(&item_inputs(update));
            }
        }
        vehicle = session
            .vehicle_at("tlog", "tlog", header.system_id)
            .map_or_else(|| format!("tlog-{}", header.system_id), str::to_owned);
        if let Some(payload) = uxduck_mavlink::framing::payload(frame) {
            running.take_frame(&RawFrame {
                vehicle_id: vehicle.clone(),
                message_id: header.message_id,
                system_id: header.system_id,
                component_id: header.component_id,
                payload: payload.to_vec(),
            });
        }
        // Ticks on the tlog's clock, at the plugin's cadence, up to now.
        while let Some(due) = running.next_tick().filter(|due| *due < at(at_us)) {
            for delivered in running.deliver(due) {
                if !report_replay(&mut running, delivered, each) {
                    return Ok(description);
                }
            }
        }
        for delivered in running.deliver(at(at_us)) {
            if !report_replay(&mut running, delivered, each) {
                return Ok(description);
            }
        }
    }
    // What is still held goes at the end, as the next cadence would; no
    // tick, which would come at a time that never was.
    let flushed_at = [
        running.items.as_ref().map(|(_, queue)| queue.next),
        running.messages.as_ref().map(|(_, queue)| queue.next),
        running.frames.as_ref().map(|(_, queue)| queue.next),
    ]
    .into_iter()
    .flatten()
    .fold(at(end_us), Instant::max);
    for delivered in running.deliver_held(flushed_at) {
        if !report_replay(&mut running, delivered, each) {
            return Ok(description);
        }
    }
    if let Some((command, params)) = command {
        let call = CommandCall {
            command,
            vehicle_id: Some(vehicle),
            params,
        };
        let delivered = running.timed("on_command", &call, flushed_at);
        report_replay(&mut running, delivered, each);
    }
    Ok(description)
}

/// How many fetches a plugin may have out at once (S170).
const FETCHES_AT_ONCE: usize = 4;
/// How long a fetch may take.
const FETCH_LIMIT: Duration = Duration::from_secs(10);
/// The most of a page a plugin is given.
const FETCH_BYTES: usize = 1024 * 1024;

/// A plugin's URL, if it may fetch it (S170): HTTPS to a host its manifest
/// names, on the usual port; plain HTTP only to `localhost`.
fn fetch_url(url: &str, hosts: &[String]) -> Result<reqwest::Url, String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("{url} is not a URL: {e}"))?;
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    if !hosts.contains(&host) {
        return Err(if hosts.is_empty() {
            "its manifest names no hosts it may fetch from".to_owned()
        } else {
            format!(
                "{host} is not one of the hosts its manifest names ({})",
                hosts.join(", ")
            )
        });
    }
    let local = host == "localhost";
    match parsed.scheme() {
        "https" => {}
        "http" if local => {}
        other => return Err(format!("{other}: only HTTPS")),
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("a URL with a user name or password".into());
    }
    if parsed.port().is_some() && !local {
        return Err("only the usual port".into());
    }
    Ok(parsed)
}

/// What came back: status, headers and body, at most `FETCH_BYTES`.
type Page = (u16, BTreeMap<String, String>, String);

async fn fetch_page(plugin: &str, url: reqwest::Url) -> Result<Page, String> {
    let client = reqwest::Client::builder()
        .timeout(FETCH_LIMIT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    let mut response = client
        .get(url)
        .header(
            reqwest::header::USER_AGENT,
            format!("UXDuck/{} (plugin {plugin})", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                format!("no answer in {} s", FETCH_LIMIT.as_secs())
            } else {
                format!("could not reach it: {e}")
            }
        })?;
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
        })
        .collect();
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        if body.len() + chunk.len() > FETCH_BYTES {
            return Err(format!("larger than {} MB", FETCH_BYTES / (1024 * 1024)));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((status, headers, String::from_utf8_lossy(&body).into_owned()))
}

/// The input to `on_fetched` for a request and its outcome.
fn plugin_fetch_answer(request: &FetchRequest, page: Result<Page, String>) -> FetchedInput {
    let now_ms = wall_ms(Instant::now());
    match page {
        Ok((status, headers, body)) => FetchedInput {
            id: request.id.clone(),
            url: request.url.clone(),
            status: Some(status),
            headers,
            body,
            error: None,
            now_ms,
        },
        Err(why) => FetchedInput {
            id: request.id.clone(),
            url: request.url.clone(),
            error: Some(why),
            now_ms,
            ..FetchedInput::default()
        },
    }
}

/// Why a call failed, in words.
fn explain(function: &str, error: &str) -> String {
    match error {
        "plugin ran out of fuel" => {
            format!("{function} exceeded its execution budget and was stopped")
        }
        "oom" => format!(
            "{function} ran out of its {} MiB of memory",
            MEMORY_PAGES / 16
        ),
        other => format!("{function}: {other}"),
    }
}

/// Calls a plugin function with JSON in and out.
fn call<I: Serialize, O: DeserializeOwned>(
    plugin: &mut extism::Plugin,
    function: &str,
    input: &I,
) -> Result<O, String> {
    let input = serde_json::to_string(input).map_err(|e| e.to_string())?;
    let output: String = plugin
        .call::<&str, String>(function, &input)
        .map_err(|e| explain(function, &e.to_string()))?;
    if output.trim().is_empty() {
        return serde_json::from_str("{}").map_err(|e| format!("{function}: {e}"));
    }
    serde_json::from_str(&output).map_err(|e| format!("{function} returned {e}"))
}

/// The daemon's clock for plugins, which have none of their own: the
/// script host's `Date.now()`, and any plugin that imports it.
fn clock() -> extism::Function {
    extism::Function::new(
        "uxduck_now_ms",
        [],
        [extism::ValType::I64],
        extism::UserData::new(()),
        |_plugin, _inputs, outputs, _data| {
            let at = REPLAY_CLOCK
                .get()
                .map_or_else(Instant::now, |clock| clock.at);
            let ms = i64::try_from(wall_ms(at)).unwrap_or(i64::MAX);
            outputs[0] = extism::Val::I64(ms);
            Ok(())
        },
    )
}

/// Loads a module, or a script in the script host, with the host's limits
/// and asks it to describe itself.
///
/// # Errors
/// When it does not load, or `describe` fails.
pub fn load(module: &str, script: bool) -> Result<(extism::Plugin, Description), String> {
    load_with_clock(module, script, clock())
}

fn load_with_clock(
    module: &str,
    script: bool,
    clock: extism::Function,
) -> Result<(extism::Plugin, Description), String> {
    // Read here, not by Extism: its file access stays off.
    let (bytes, config) = if script {
        let source = std::fs::read_to_string(module).map_err(|e| format!("{module}: {e}"))?;
        let javascript = super::script::strip_types(Path::new(module), &source)?;
        (super::script::HOST.to_vec(), Some(javascript))
    } else {
        (
            std::fs::read(module).map_err(|e| format!("{module}: {e}"))?,
            None,
        )
    };
    let mut manifest =
        extism::Manifest::new([extism::Wasm::data(bytes)]).with_memory_max(MEMORY_PAGES);
    if let Some(javascript) = config {
        manifest = manifest.with_config_key("script", javascript);
    }
    let mut plugin = extism::PluginBuilder::new(&manifest)
        .with_functions([clock])
        .with_fuel_limit(CALL_FUEL)
        .build()
        .map_err(|e| format!("{module} does not load: {e}"))?;
    let description = call(&mut plugin, "describe", &serde_json::json!({}))?;
    Ok((plugin, description))
}

/// Calls a function if the plugin has it; no function, no effects.
///
/// # Errors
/// When the call traps, times out, or returns something that is not
/// `Effects`.
pub fn effects<I: Serialize>(
    plugin: &mut extism::Plugin,
    function: &str,
    input: &I,
) -> Result<Effects, String> {
    if !plugin.function_exists(function) {
        return Ok(Effects::default());
    }
    call(plugin, function, input)
}

impl Host {
    /// Runs the host on its own thread until the channel closes.
    pub fn start(mut self, engine: EngineHandle) {
        let runtime = tokio::runtime::Handle::current();
        self.runtime = Some(runtime.clone());
        let feed_engine = engine.clone();
        let feed_runtime = runtime.clone();
        let read_traffic = move |id: &str| {
            let id = id.to_owned();
            feed_runtime
                .block_on(feed_engine.run(move |e| e.traffic_feed_for(&id)))
                .ok()
                .flatten()
        };
        let tell = move |report: Report| {
            let engine = engine.clone();
            let _ = runtime.block_on(engine.run(move |e| match report {
                Report::Described(id, description) => e.plugin_described(&id, description),
                Report::Effects(id, effects) => e.plugin_effects(&id, effects),
                Report::Failed(id, why) => e.plugin_failed(&id, &why),
                Report::Used(usage) => e.plugins_used(usage),
                Report::Tested(connection, request, result) => {
                    e.plugin_tested(connection, &request, result);
                }
            }));
        };
        let spawned = std::thread::Builder::new()
            .name("plugins".into())
            .spawn(move || self.run(&tell, &read_traffic));
        if let Err(error) = spawned {
            tracing::error!(%error, code = "plugin-host", "could not start the plugin host");
        }
    }

    fn run(
        self,
        tell: &dyn Fn(Report),
        read_traffic: &dyn Fn(&str) -> Option<uxduck_contracts::dto::traffic::TrafficFeedContext>,
    ) {
        let mut running: BTreeMap<String, Running> = BTreeMap::new();
        let mut reported = Instant::now();
        loop {
            let now = Instant::now();
            if now.duration_since(reported) >= USAGE_EVERY {
                reported = now;
                tell(Report::Used(
                    running.iter().map(|(id, r)| r.usage(id, now)).collect(),
                ));
            }
            // Until something is due, and never past the next usage report,
            // which a plugin asleep until a wake-up would otherwise hold up.
            let report_in = USAGE_EVERY.saturating_sub(reported.elapsed());
            let wait = running
                .values()
                .filter_map(Running::due_at)
                .map(|at| at.saturating_duration_since(Instant::now()))
                .min()
                .unwrap_or(Duration::from_secs(1))
                .min(report_in);
            match self.receiver.recv_timeout(wait) {
                Ok(Message::Load(load)) => {
                    running.remove(&load.id);
                    // Its messages first, so its subscriptions find them.
                    if let Err(why) = register_dialect(&load.id, load.dialect.as_deref()) {
                        tell(Report::Failed(load.id, why));
                        continue;
                    }
                    match self::load(&load.module, load.script) {
                        Ok((plugin, description)) => {
                            tracing::info!(plugin = %load.id, "plugin loaded");
                            tell(Report::Described(load.id.clone(), description));
                            let mut started = Running::new(plugin, &load.subscribe)
                                .with_hosts(load.hosts.clone());
                            let file = self.storage.join(format!("{}.json", load.id));
                            let result =
                                started.start(read_stored(&file), Some(file), Instant::now());
                            running.insert(load.id.clone(), started);
                            self.settle(&mut running, &load.id, result, tell);
                        }
                        Err(why) => tell(Report::Failed(load.id, why)),
                    }
                }
                Ok(Message::Fetched(id, input)) => {
                    if let Some(plugin) = running.get_mut(&id) {
                        let result = plugin.fetched(&input);
                        self.settle(&mut running, &id, result, tell);
                    }
                }
                Ok(Message::Unload(id)) => {
                    running.remove(&id);
                    uxduck_mavlink::named::unregister(&id);
                }
                Ok(Message::Command(id, call)) => {
                    if let Some(plugin) = running.get_mut(&id) {
                        let result = plugin.command(&call);
                        self.settle(&mut running, &id, result, tell);
                    }
                }
                Ok(Message::Frame(frame)) => {
                    for plugin in running.values_mut() {
                        plugin.take_frame(&frame);
                    }
                }
                Ok(Message::Test(order)) => {
                    let result = run_test(&order, &self.tlogs);
                    tell(Report::Tested(order.connection, order.request_id, result));
                }
                Ok(Message::Items(items)) => {
                    for plugin in running.values_mut() {
                        plugin.take_items(&items);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    crate::wakes::count("plugins: host (1 Hz, or each plugin's cadence)");
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
            let now = Instant::now();
            let due: Vec<String> = running
                .iter()
                .filter(|(_, r)| r.due_at().is_some_and(|at| at <= now))
                .map(|(id, _)| id.clone())
                .collect();
            for id in due {
                let Some(plugin) = running.get_mut(&id) else {
                    continue;
                };
                plugin.traffic = read_traffic(&id);
                for delivered in plugin.deliver(now) {
                    self.settle(&mut running, &id, delivered.result, tell);
                    if !running.contains_key(&id) {
                        break;
                    }
                }
            }
            self.resubscribe(&running);
        }
    }

    /// Passes a call's effects on, or unloads a plugin whose call failed.
    fn settle(
        &self,
        running: &mut BTreeMap<String, Running>,
        id: &str,
        result: Result<Effects, String>,
        tell: &dyn Fn(Report),
    ) {
        match result {
            Ok(mut effects) => {
                let store = std::mem::take(&mut effects.store);
                let fetch = std::mem::take(&mut effects.fetch);
                if let Some(plugin) = running.get_mut(id) {
                    if let Err(why) = plugin.keep(store) {
                        tracing::warn!(plugin = %id, %why, "plugin storage refused");
                        effects.log.push(format!("storage refused: {why}"));
                    }
                    for request in fetch {
                        self.fetch(id, plugin, request);
                    }
                }
                tell(Report::Effects(id.to_owned(), effects));
            }
            Err(why) => {
                running.remove(id);
                tracing::warn!(plugin = %id, %why, "plugin unloaded");
                tell(Report::Failed(id.to_owned(), why));
            }
        }
    }

    /// Fetches a page for a plugin (S170) off this thread, from a host its
    /// manifest names; the answer, or why there is none, comes back as
    /// `Message::Fetched`.
    fn fetch(&self, id: &str, plugin: &mut Running, request: FetchRequest) {
        let refuse = |why: String| {
            tracing::warn!(plugin = %id, url = %request.url, %why, "plugin fetch refused");
            plugin_fetch_answer(&request, Err(why))
        };
        let answer = match (fetch_url(&request.url, &plugin.hosts), &self.runtime) {
            (Err(why), _) => Some(refuse(why)),
            (Ok(_), None) => Some(refuse("UXDuck cannot fetch here".into())),
            (Ok(_), Some(_)) if plugin.fetching >= FETCHES_AT_ONCE => Some(refuse(format!(
                "{FETCHES_AT_ONCE} fetches are already out; wait for an answer"
            ))),
            (Ok(url), Some(runtime)) => {
                plugin.fetching += 1;
                let (sender, id) = (self.sender.clone(), id.to_owned());
                runtime.spawn(async move {
                    let answer = plugin_fetch_answer(&request, fetch_page(&id, url).await);
                    let _ = sender.send(Message::Fetched(id, answer));
                });
                None
            }
        };
        if let Some(answer) = answer {
            // Counted out, so the answer's arrival counts it back in.
            plugin.fetching += 1;
            let _ = self.sender.send(Message::Fetched(id.to_owned(), answer));
        }
    }

    fn resubscribe(&self, running: &BTreeMap<String, Running>) {
        let frames: BTreeSet<u32> = running
            .values()
            .flat_map(|r| {
                let messages = r.messages.iter().flat_map(|(ids, _)| ids.iter().copied());
                let frames = r.frames.iter().flat_map(|(ids, _)| ids.iter().copied());
                messages.chain(frames).collect::<Vec<_>>()
            })
            .collect();
        let mut items: Vec<String> = running
            .values()
            .flat_map(|r| r.items.iter().flat_map(|(keys, _)| keys.clone()))
            .collect();
        items.sort();
        items.dedup();
        if let Ok(mut wants) = self.subscribed.write()
            && (wants.frames != frames || wants.items != items)
        {
            *wants = Wants { frames, items };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_paused_call_resumes_without_spending_its_execution_budget() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("index.ts");
        std::fs::write(
            &source,
            "let calls = 0; export function onTick() { Date.now(); return { store: { calls: ++calls } }; }",
        )
        .unwrap();
        let pause = Arc::new(AtomicBool::new(false));
        let paused = pause.clone();
        let clock = extism::Function::new(
            "uxduck_now_ms",
            [],
            [extism::ValType::I64],
            extism::UserData::new(()),
            move |_plugin, _inputs, outputs, _data| {
                // Hold an active call longer than the old deadline, without
                // executing guest instructions. No machine sleep is required.
                if paused.swap(false, Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(1100));
                }
                outputs[0] = extism::Val::I64(2000);
                Ok(())
            },
        );
        let (mut plugin, _) = load_with_clock(source.to_str().unwrap(), true, clock).unwrap();
        pause.store(true, Ordering::SeqCst);
        for expected in 1..=3 {
            let result: Effects = call(
                &mut plugin,
                "on_tick",
                &TickInput {
                    now_ms: 2000,
                    ..TickInput::default()
                },
            )
            .unwrap();
            assert_eq!(result.store["calls"], expected);
        }
        assert!(!pause.load(Ordering::SeqCst), "the pause was not exercised");
    }

    #[test]
    fn execution_budgets_stop_infinite_scripts_during_loading_and_ticks() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("index.ts");
        for script in [
            "while (true) {}",
            "export function describe() { while (true) {} }",
        ] {
            std::fs::write(&source, script).unwrap();
            let error = load(source.to_str().unwrap(), true).err().unwrap();
            assert_eq!(
                error,
                "describe exceeded its execution budget and was stopped"
            );
        }
        std::fs::write(&source, "export function onTick() { while (true) {} }").unwrap();
        let (mut plugin, _) = load(source.to_str().unwrap(), true).unwrap();
        let error = call::<_, Effects>(
            &mut plugin,
            "on_tick",
            &TickInput {
                now_ms: 2000,
                ..TickInput::default()
            },
        )
        .unwrap_err();
        assert_eq!(
            error,
            "on_tick exceeded its execution budget and was stopped"
        );
    }

    fn replay_script(source: &str) -> Vec<Delivered> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("plugin.json"),
            serde_json::json!({
                "abi": 2, "id": "replay-test", "name": "Replay test", "version": "1",
                "script": "index.ts", "capabilities": ["read"]
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(dir.path().join("index.ts"), source).unwrap();
        let mut fake = uxduck_mavlink::sim::FakeVehicle::new(1, 0xAB);
        let mut tlog = Vec::new();
        for at_us in [0, 5_000_000] {
            uxduck_mavlink::tlog::record(&mut tlog, at_us, &fake.heartbeat()).unwrap();
        }
        let mut calls = Vec::new();
        harness(dir.path(), &tlog, None, &mut |function, input, result| {
            calls.push(Delivered {
                function: if function == "on_start" {
                    "on_start"
                } else {
                    "on_tick"
                },
                input: input.clone(),
                result: result.clone(),
            });
        })
        .unwrap();
        assert!(REPLAY_CLOCK.get().is_none(), "the replay clock leaked");
        calls
    }

    #[test]
    fn replay_wakeups_and_date_now_follow_the_recording() {
        let calls = replay_script(
            r#"
            import { value } from "@uxduck/sdk";
            export function onStart() { return { wakeInMs: 1000 }; }
            export function onTick({ nowMs }) {
                return { wakeInMs: 1000, items: [value(null, "test.clock_error", "Clock error", "none", Date.now() - nowMs)] };
            }
        "#,
        );
        assert_eq!(calls.len(), 6);
        let started = calls[0].input["nowMs"].as_u64().unwrap();
        for (index, call) in calls.iter().enumerate().skip(1) {
            assert_eq!(
                call.input["nowMs"].as_u64().unwrap() - started,
                index as u64 * 1000
            );
            assert_eq!(call.result.as_ref().unwrap().items[0].value, Some(0.0));
        }
    }

    #[test]
    fn replay_checks_cumulative_storage_after_every_call() {
        let calls = replay_script(
            r#"
            export function onStart() { return { store: { first: "x".repeat(40000) }, wakeInMs: 1000 }; }
            export function onTick() { return { store: { second: "x".repeat(40000) }, wakeInMs: 1000 }; }
        "#,
        );
        assert_eq!(calls.len(), 2);
        assert!(calls[1].result.as_ref().unwrap_err().contains("65536"));
    }

    #[test]
    fn replay_rejects_items_the_engine_would_drop() {
        let calls = replay_script(
            r#"
            import { value } from "@uxduck/sdk";
            export function onStart() { return { items: [value(null, "test.distance", "Distance", "meter", 10)] }; }
        "#,
        );
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0]
                .result
                .as_ref()
                .unwrap_err()
                .contains("key does not say its unit")
        );
    }

    fn example(id: &str) -> extism::Plugin {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins")
            .join(id);
        let manifest = inspect(dir.to_str().unwrap()).unwrap();
        load(dir.join(manifest.script.unwrap()).to_str().unwrap(), true)
            .unwrap()
            .0
    }

    #[test]
    fn clock_placeholders_keep_the_units_their_keys_require() {
        let mut plugin = example("clock-ts");
        let effects: Effects = call(
            &mut plugin,
            "on_start",
            &serde_json::json!({
                "stored": {}, "nowMs": 1000
            }),
        )
        .unwrap();
        for item in &effects.items {
            assert!(
                uxduck_core::domain::telemetry::key_says_unit(&item.key, item.unit.into()),
                "{}",
                item.key
            );
        }
        let unset = effects
            .items
            .iter()
            .find(|item| item.key == "alarm.at_at")
            .unwrap();
        assert_eq!(unset.unit, uxduck_contracts::dto::telemetry::Unit::Time);
        assert_eq!(unset.text.as_deref(), Some("—"));
        assert!(unset.value.is_none());
    }

    #[test]
    fn battery_watch_handles_missing_values_hysteresis_and_separate_vehicles() {
        let mut plugin = example("battery-watch-ts");
        let mut sample = |vehicle: &str, value: Option<f64>| {
            call::<_, Effects>(&mut plugin, "on_items", &serde_json::json!({
                "items": [{ "vehicleId": vehicle, "key": "battery.remaining_pct", "value": value }]
            })).unwrap()
        };
        assert!(sample("a", None).items.is_empty());
        for (value, warnings) in [(19.0, 1), (18.0, 0), (26.0, 0), (19.0, 1)] {
            let effects = sample("a", Some(value));
            assert_eq!(effects.notifications.len(), warnings);
            assert_eq!(effects.items[0].value, Some(value));
        }
        assert_eq!(sample("b", Some(19.0)).notifications.len(), 1);
    }

    #[test]
    fn fleet_markers_keep_each_vehicle_and_use_geojson_coordinate_order() {
        let mut plugin = example("fleet-markers-ts");
        let effects: Effects = call(&mut plugin, "on_messages", &serde_json::json!({
            "messages": [
                { "vehicleId": "a", "name": "GLOBAL_POSITION_INT", "fields": { "lat": 10_000_000, "lon": 20_000_000 } },
                { "vehicleId": "b", "name": "GLOBAL_POSITION_INT", "fields": { "lat": 30_000_000, "lon": 40_000_000 } },
                { "vehicleId": "a", "name": "GLOBAL_POSITION_INT", "fields": { "lat": 50_000_000, "lon": 60_000_000 } }
            ]
        })).unwrap();
        assert_eq!(effects.features.len(), 2);
        assert_eq!(effects.features[0].id, "vehicle-a");
        assert_eq!(
            effects.features[0].geometry["coordinates"],
            serde_json::json!([6, 5])
        );
        assert_eq!(effects.features[1].id, "vehicle-b");
    }

    #[test]
    fn session_timer_restores_state_and_reset_needs_no_vehicle() {
        let mut plugin = example("session-timer-ts");
        let started: Effects = call(
            &mut plugin,
            "on_start",
            &serde_json::json!({
                "stored": { "startedAt": 1000 }, "nowMs": 61_000
            }),
        )
        .unwrap();
        assert_eq!(started.items[0].value, Some(60.0));
        assert_eq!(started.items[0].rate_per_s, Some(1.0));
        assert_eq!(started.items[0].vehicle_id, None);
        assert_eq!(started.wake_in_ms, Some(60_000));
        let mut restarted = example("session-timer-ts");
        let resumed: Effects = call(
            &mut restarted,
            "on_start",
            &serde_json::json!({
                "stored": started.store, "nowMs": 121_000
            }),
        )
        .unwrap();
        assert_eq!(resumed.items[0].value, Some(120.0));
        let reset: Effects = call(
            &mut restarted,
            "on_command",
            &serde_json::json!({
                "command": "reset", "params": {}
            }),
        )
        .unwrap();
        assert_eq!(reset.items[0].value, Some(0.0));
        assert!(reset.store["startedAt"].as_f64().unwrap() > 121_000.0);
    }

    /// The sample wind check (S194): the vehicle's estimate per vehicle, a
    /// speed set by hand instead, words without live numbers, a wake-up to
    /// report again before its 30 s run out, and only its declared check.
    #[test]
    fn wind_check_reports_each_vehicle_from_its_estimate_or_by_hand() {
        use uxduck_contracts::dto::plugin::ReportedStatus;
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/wind-check-ts");
        let manifest = inspect(dir.to_str().unwrap()).unwrap();
        assert_eq!(manifest.checks.len(), 1);
        assert_eq!(manifest.checks[0].id, "wind");
        assert_eq!(manifest.checks[0].stale_ms, 30_000);
        assert_eq!(
            manifest.checks[0].severity,
            uxduck_contracts::dto::preflight::CheckSeverity::Block
        );
        let mut plugin = example("wind-check-ts");
        let mut items = |batch: serde_json::Value| -> Effects {
            call(
                &mut plugin,
                "on_items",
                &serde_json::json!({ "items": batch }),
            )
            .unwrap()
        };
        let calm = items(serde_json::json!([
            { "vehicleId": "a", "key": "wind.speed_mps", "label": "Wind", "unit": "meter-per-second", "value": 4.2, "timeUs": 1 },
            { "vehicleId": "b", "key": "wind.speed_mps", "label": "Wind", "unit": "meter-per-second", "value": 9.5, "timeUs": 1 },
            { "vehicleId": "b", "key": "battery.voltage_v", "label": "Battery", "unit": "volt", "value": 12.0, "timeUs": 1 }
        ]));
        let status = |effects: &Effects, vehicle: &str| {
            effects
                .checks
                .iter()
                .find(|c| c.vehicle_id == vehicle)
                .map(|c| (c.status, c.detail.clone().unwrap_or_default()))
        };
        assert_eq!(calm.checks.len(), 2);
        assert!(calm.checks.iter().all(|c| c.id == "wind"));
        assert_eq!(
            status(&calm, "a"),
            Some((ReportedStatus::Pass, "Under 8 m/s.".into()))
        );
        assert_eq!(
            status(&calm, "b"),
            Some((ReportedStatus::Fail, "8 m/s or more.".into()))
        );
        assert_eq!(
            calm.wake_in_ms,
            Some(10_000),
            "reports again before it goes stale"
        );
        let speed = calm
            .items
            .iter()
            .find(|i| i.key == "wind_check.speed_mps" && i.vehicle_id.as_deref() == Some("a"))
            .unwrap();
        assert_eq!(speed.value, Some(4.2));
        assert!(
            uxduck_core::domain::telemetry::key_says_unit(&speed.key, speed.unit.into()),
            "its key says its unit"
        );
        // The words stay as the wind changes within the same side of 8 m/s.
        let breezier = items(serde_json::json!([
            { "vehicleId": "a", "key": "wind.speed_mps", "label": "Wind", "unit": "meter-per-second", "value": 6.0, "timeUs": 2 }
        ]));
        assert_eq!(status(&breezier, "a"), status(&calm, "a"));

        // Set by hand, it counts instead of the estimate; and back.
        let by_hand: Effects = call(
            &mut plugin,
            "on_command",
            &serde_json::json!({ "command": "set-wind", "vehicleId": "a", "params": { "speedMps": 12 } }),
        )
        .unwrap();
        assert_eq!(status(&by_hand, "a").unwrap().0, ReportedStatus::Fail);
        let woken: Effects =
            call(&mut plugin, "on_tick", &serde_json::json!({ "nowMs": 1 })).unwrap();
        assert_eq!(woken.checks.len(), 2, "every vehicle again");
        assert_eq!(status(&woken, "a").unwrap().0, ReportedStatus::Fail);
        let estimate: Effects = call(
            &mut plugin,
            "on_command",
            &serde_json::json!({ "command": "use-estimate", "vehicleId": "a", "params": {} }),
        )
        .unwrap();
        assert_eq!(status(&estimate, "a").unwrap().0, ReportedStatus::Pass);
        // A vehicle with no estimate, set by hand only: no estimate is
        // waited for until a speed is set.
        let fresh: Effects = call(
            &mut plugin,
            "on_command",
            &serde_json::json!({ "command": "use-estimate", "vehicleId": "c", "params": {} }),
        )
        .unwrap();
        assert_eq!(
            status(&fresh, "c"),
            Some((
                ReportedStatus::Waiting,
                "No wind estimate from the vehicle yet.".into()
            ))
        );
    }

    #[test]
    fn replay_refuses_a_check_the_manifest_does_not_declare() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("plugin.json"),
            serde_json::json!({
                "abi": 2, "id": "checks", "name": "Checks", "version": "0.1.0",
                "script": "index.ts", "capabilities": ["read"],
                "checks": [{ "id": "wind", "title": "Wind under 8 m/s", "severity": "block" }]
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("index.ts"),
            r#"
            import { check } from "@uxduck/sdk";
            export function onStart() { return { checks: [check("v", "rain", "pass")] }; }
        "#,
        )
        .unwrap();
        let mut outcomes = Vec::new();
        harness(dir.path(), &[], None, &mut |_, _, result| {
            outcomes.push(result.clone());
        })
        .unwrap();
        assert!(
            outcomes[0]
                .as_ref()
                .unwrap_err()
                .contains("check rain: not in plugin.json's checks"),
            "{outcomes:?}"
        );
    }

    fn plugin_dir(manifest: &serde_json::Value) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("plugin.json"), manifest.to_string()).unwrap();
        std::fs::write(dir.path().join("p.wasm"), b"\0asm").unwrap();
        dir
    }

    #[test]
    fn a_manifest_names_only_messages_the_dialect_has() {
        let manifest = |names: &[&str], sends: &[&str]| {
            serde_json::json!({
                "abi": 2, "id": "p", "name": "P", "version": "1", "wasm": "p.wasm",
                "capabilities": ["read"],
                "subscribe": { "messages": { "names": names } },
                "sends": sends
            })
        };
        let good = plugin_dir(&manifest(&["GLOBAL_POSITION_INT"], &["MANUAL_CONTROL"]));
        let read = inspect(good.path().to_str().unwrap()).unwrap();
        assert_eq!(read.subscribe.messages.unwrap().cadence_ms, 200);
        let bad = plugin_dir(&manifest(&["GLOBAL_POSITION"], &["NOT_A_MESSAGE"]));
        let why = inspect(bad.path().to_str().unwrap()).unwrap_err();
        assert!(
            why.contains("no message GLOBAL_POSITION, NOT_A_MESSAGE"),
            "{why}"
        );
    }

    #[test]
    fn a_plugin_fetches_only_over_https_from_hosts_it_names() {
        let hosts = vec!["api.adsb.lol".to_owned(), "localhost".to_owned()];
        assert!(fetch_url("https://api.adsb.lol/v2/point/51/0/10", &hosts).is_ok());
        assert!(fetch_url("http://localhost:8080/data/aircraft.json", &hosts).is_ok());
        let why = |url: &str| fetch_url(url, &hosts).unwrap_err();
        assert!(why("https://evil.example/").contains("not one of the hosts"));
        assert!(why("http://api.adsb.lol/").contains("only HTTPS"));
        assert!(why("https://api.adsb.lol:8443/").contains("usual port"));
        assert!(why("https://me:pw@api.adsb.lol/").contains("user name"));
        assert!(why("file:///etc/passwd").contains("not one of the hosts"));
        assert!(
            fetch_url("https://api.adsb.lol/", &[])
                .unwrap_err()
                .contains("names no hosts")
        );
    }

    /// A one-shot HTTP server on localhost answering `body` with a header.
    async fn serve_once(body: Vec<u8>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nX-RateLimit-Remaining: 499\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(&body).await;
        });
        format!("http://localhost:{port}/v2/point")
    }

    #[tokio::test]
    async fn a_fetch_brings_status_headers_and_body_and_no_more_than_a_megabyte() {
        let hosts = vec!["localhost".to_owned()];
        let url = serve_once(br#"{"ac":[]}"#.to_vec()).await;
        let (status, headers, body) = fetch_page("test", fetch_url(&url, &hosts).unwrap())
            .await
            .unwrap();
        assert_eq!(status, 200);
        assert_eq!(
            headers.get("x-ratelimit-remaining").map(String::as_str),
            Some("499")
        );
        assert_eq!(body, r#"{"ac":[]}"#);
        let url = serve_once(vec![b' '; FETCH_BYTES + 1]).await;
        let why = fetch_page("test", fetch_url(&url, &hosts).unwrap())
            .await
            .unwrap_err();
        assert_eq!(why, "larger than 1 MB");
    }

    #[test]
    fn a_script_gets_its_answer_in_on_fetched_and_reports_traffic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("index.ts"),
            r"
export function describe() { return { panels: [] }; }
export function onFetched(input: { id: string; status?: number; body: string }) {
  const ac = JSON.parse(input.body).ac as { hex: string; lat: number; lon: number }[];
  return {
    log: [`${input.id} ${input.status}`],
    traffic: ac.map((a) => ({ address: a.hex, latitudeDeg: a.lat, longitudeDeg: a.lon })),
  };
}
",
        )
        .unwrap();
        let module = dir.path().join("index.ts").display().to_string();
        let (plugin, _) = load(&module, true).unwrap();
        let mut running =
            Running::new(plugin, &Subscriptions::default()).with_hosts(vec!["api.adsb.lol".into()]);
        let effects = running
            .fetched(&FetchedInput {
                id: "near".into(),
                url: "https://api.adsb.lol/v2/point/51/0/10".into(),
                status: Some(200),
                body: r#"{"ac":[{"hex":"406bc1","lat":51.4,"lon":-0.4}]}"#.into(),
                ..FetchedInput::default()
            })
            .unwrap();
        assert_eq!(effects.log, ["near 200"]);
        assert_eq!(effects.traffic.len(), 1);
        assert_eq!(effects.traffic[0].address, "406bc1");
        assert_eq!(effects.traffic[0].latitude_deg, Some(51.4));
    }

    /// The adsb.lol plugin UXDuck includes, loaded in the script host.
    fn adsb_lol() -> (tempfile::TempDir, extism::Plugin) {
        let dir = tempfile::tempdir().unwrap();
        let written = write_included(dir.path());
        let folder = written.iter().find(|d| d.ends_with("adsb-lol")).unwrap();
        let manifest = inspect(folder).unwrap();
        assert!(manifest.starts_off && manifest.traffic_source);
        assert_eq!(manifest.hosts, ["api.adsb.lol"]);
        let (plugin, _) = load(&format!("{folder}/index.ts"), true).unwrap();
        (dir, plugin)
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one plugin's whole day, step by step"
    )]
    fn adsb_lol_asks_around_the_vehicles_and_reports_what_it_hears() {
        use uxduck_contracts::dto::plugin::MessageBatch;
        let (_dir, mut plugin) = adsb_lol();
        let now = wall_ms(Instant::now());
        let started: Effects = effects(
            &mut plugin,
            "on_start",
            &StartInput {
                stored: serde_json::Map::new(),
                now_ms: now,
            },
        )
        .unwrap();
        assert_eq!(
            started.traffic_status.unwrap().note.as_deref(),
            Some("0 of 500 requests today")
        );
        // No vehicle yet: nothing asked.
        let idle: Effects = effects(
            &mut plugin,
            "on_tick",
            &TickInput {
                now_ms: now,
                ..TickInput::default()
            },
        )
        .unwrap();
        assert!(idle.fetch.is_empty());
        let message = |name: &str, fields: serde_json::Value| MessageInput {
            vehicle_id: "alpha".into(),
            system_id: 1,
            component_id: 1,
            name: name.into(),
            fields: fields.as_object().unwrap().clone(),
        };
        let _: Effects = effects(
            &mut plugin,
            "on_messages",
            &MessageBatch {
                messages: vec![
                    message(
                        "GLOBAL_POSITION_INT",
                        serde_json::json!({ "lat": 514_700_000, "lon": -4_500_000 }),
                    ),
                    message("HEARTBEAT", serde_json::json!({ "base_mode": 209 })),
                ],
                gap: false,
            },
        )
        .unwrap();
        let asked: Effects = effects(
            &mut plugin,
            "on_tick",
            &TickInput {
                now_ms: now + 5000,
                ..TickInput::default()
            },
        )
        .unwrap();
        assert_eq!(asked.fetch.len(), 1);
        assert_eq!(
            asked.fetch[0].url,
            "https://api.adsb.lol/v2/point/51.4700/-0.4500/25"
        );
        assert_eq!(asked.store["used"], 1);
        // Arming does not change the explicit 30-second interval; one at a time.
        let soon: Effects = effects(
            &mut plugin,
            "on_tick",
            &TickInput {
                now_ms: now + 10_000,
                ..TickInput::default()
            },
        )
        .unwrap();
        assert!(soon.fetch.is_empty());
        let mut headers = BTreeMap::new();
        headers.insert(
            "x-ratelimit-limit".to_owned(),
            "4, 4;w=1, 60;w=60, 500;w=86400".to_owned(),
        );
        let heard: Effects = effects(
            &mut plugin,
            "on_fetched",
            &FetchedInput {
                id: "around".into(),
                url: asked.fetch[0].url.clone(),
                status: Some(200),
                headers,
                body: r#"{"ac":[{"hex":"39bdb2","type":"adsb_icao","flight":"AFR58PM ","alt_baro":3075,"alt_geom":3000,"gs":157.7,"track":264.54,"geom_rate":2048,"squawk":"2012","category":"A3","lat":51.476813,"lon":-0.524139,"seen_pos":0.4,"seen":0.2},{"hex":"~2a3b4c","lat":51.5,"lon":-0.4}]}"#.into(),
                error: None,
                now_ms: now + 5300,
            },
        )
        .unwrap();
        assert_eq!(
            heard.traffic.len(),
            1,
            "a TIS-B track (~) has no ICAO address"
        );
        let afr = &heard.traffic[0];
        assert_eq!(afr.address, "39bdb2");
        assert_eq!(afr.callsign.as_deref(), Some("AFR58PM"));
        assert!((afr.altitude_m.unwrap() - 914.4).abs() < 0.01);
        assert!((afr.ground_speed_mps.unwrap() - 81.13).abs() < 0.01);
        assert_eq!(
            afr.category,
            uxduck_contracts::dto::traffic::TrafficCategory::Large
        );
        let status = heard.traffic_status.unwrap();
        assert_eq!(status.credit.as_deref(), Some("adsb.lol (ODbL)"));
        assert_eq!(status.note.as_deref(), Some("1 of 500 requests today"));
        let again: Effects = effects(
            &mut plugin,
            "on_tick",
            &TickInput {
                now_ms: now + 35_500,
                ..TickInput::default()
            },
        )
        .unwrap();
        assert_eq!(again.fetch.len(), 1);
        // Told it is over the limit: it stops and says so, as a problem.
        let mut later = BTreeMap::new();
        later.insert("retry-after".to_owned(), "3600".to_owned());
        let stopped: Effects = effects(
            &mut plugin,
            "on_fetched",
            &FetchedInput {
                id: "around".into(),
                url: again.fetch[0].url.clone(),
                status: Some(429),
                headers: later,
                now_ms: now + 35_600,
                ..FetchedInput::default()
            },
        )
        .unwrap();
        let status = stopped.traffic_status.unwrap();
        assert!(status.problem);
        assert!(
            status
                .note
                .unwrap()
                .starts_with("adsb.lol's free limit is used up")
        );
        let quiet: Effects = effects(
            &mut plugin,
            "on_tick",
            &TickInput {
                now_ms: now + 60_000,
                ..TickInput::default()
            },
        )
        .unwrap();
        assert!(quiet.fetch.is_empty());
    }

    #[test]
    fn adsb_lol_says_when_it_cannot_reach_adsb_lol() {
        let (_dir, mut plugin) = adsb_lol();
        let failed: Effects = effects(
            &mut plugin,
            "on_fetched",
            &FetchedInput {
                id: "around".into(),
                url: "https://api.adsb.lol/v2/point/51/0/25".into(),
                error: Some("no answer in 10 s".into()),
                now_ms: wall_ms(Instant::now()),
                ..FetchedInput::default()
            },
        )
        .unwrap();
        let status = failed.traffic_status.unwrap();
        assert!(status.problem);
        assert!(
            status
                .note
                .unwrap()
                .starts_with("Cannot reach adsb.lol: no answer in 10 s")
        );
    }

    #[test]
    fn keys_match_exactly_or_by_prefix() {
        assert!(key_matches("battery.*", "battery.voltage_v"));
        assert!(key_matches("battery.voltage_v", "battery.voltage_v"));
        assert!(!key_matches("battery.voltage_v", "battery.current_a"));
        assert!(!key_matches("battery.*", "speed.air_mps"));
    }

    #[test]
    fn plugins_get_units_added_since_their_abi_as_none() {
        use uxduck_core::domain::telemetry::{TelemetryItem, Unit, VehicleTelemetry};
        let item = |key: &str, unit| TelemetryItem {
            key: key.into(),
            label: key.into(),
            unit,
            value: Some(1.0),
            text: None,
            source: "health".into(),
            component_id: 1,
            time_us: 1,
        };
        let inputs = super::item_inputs(&VehicleTelemetry {
            vehicle_id: "v".into(),
            positions: Vec::new(),
            items: vec![
                item("vibration.max_mps2", Unit::MeterPerSecondSquared),
                item("battery.voltage_v", Unit::Volt),
            ],
        });
        assert_eq!(inputs[0].unit, uxduck_contracts::dto::telemetry::Unit::None);
        assert_eq!(inputs[1].unit, uxduck_contracts::dto::telemetry::Unit::Volt);
    }
}
