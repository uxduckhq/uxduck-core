//! The tray's menu (S64, ADR-0008 "The tray"). `uxduck-tray` only draws:
//! what the menu says and what each item does is decided here, from the
//! engine's state, so it is the same on every platform and tested here.
//!
//! The status line is plain when all is well ("UXDuck is running", the
//! vehicles online) and says what is wrong when something is broken or
//! waiting for an answer; only then does the icon change (ADR-0009).

use std::collections::BTreeMap;

use serde_json::{Value, json};
use uxduck_contracts::dto::app::AppOpenResult;
use uxduck_contracts::dto::settings::LoginItemState;
use uxduck_contracts::dto::tray::{
    FeedbackScreenshot, FeedbackScreenshotParams, TrayAction, TrayActivateParams, TrayItem,
    TrayMenu, TrayTone,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::link::LinkStatus;
use uxduck_core::domain::notification::{NotificationKind, NotificationState, Severity, Subject};
use uxduck_core::domain::settings::LaunchAction;
use uxduck_core::domain::vehicle::Presence;
use uxduck_core::events::Change;

use crate::commands::Raise;
use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// Where "Open documentation" goes, also when the desktop app is installed.
pub const DOCS: &str = "https://uxduck.com/docs?from=tray";

/// The docs on the website's dev server (apps/website, `pnpm dev`), which a
/// development build opens (maintainer, 2026-10-12: "not production").
const LOCAL_DOCS: &str = "http://localhost:45102/docs?from=tray";

/// Where "Open documentation" goes from this build: `UXDUCK_DOCS_URL`
/// when set, the local website in a development build, else [`DOCS`].
#[must_use]
pub fn docs_url() -> String {
    std::env::var("UXDUCK_DOCS_URL")
        .ok()
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| {
            if cfg!(debug_assertions) {
                LOCAL_DOCS
            } else {
                DOCS
            }
            .into()
        })
}

/// How long a screenshot for "Send feedback…" waits to be picked up.
const SCREENSHOT_US: u64 = 5 * 60 * 1_000_000;
/// The largest screenshot kept, as base64.
const SCREENSHOT_BYTES: usize = 6 * 1024 * 1024;

/// Screenshots the tray took, by id, with when.
#[derive(Default)]
pub(crate) struct Screenshots(BTreeMap<String, (u64, Option<String>)>);

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    operator: bool,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    if !operator {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "only an operator client may use the tray",
        ));
    }
    match request {
        ClientRequest::TrayMenu(_) => {
            engine.tray_watchers.insert(connection);
            let menu = engine.tray_menu();
            engine.last_tray_menu = Some(menu.clone());
            to_value(&menu)
        }
        ClientRequest::TrayActivate(params) => {
            let result = activate(engine, connection, params, granted);
            if let Err(error) = &result {
                engine.raise(Raise {
                    severity: Severity::Warning,
                    kind: NotificationKind::Transient,
                    subject: Subject::System,
                    title: "Could not complete the menu-bar action".into(),
                    body: Some(error.message.clone()),
                    actions: Vec::new(),
                    condition: None,
                    report: None,
                });
            }
            to_value(&result?)
        }
        ClientRequest::FeedbackScreenshot(FeedbackScreenshotParams { id }) => {
            let now = engine.clock.now_us();
            let screenshot = engine
                .tray_screenshots
                .0
                .remove(&id)
                .filter(|(at, _)| now.saturating_sub(*at) < SCREENSHOT_US)
                .and_then(|(_, shot)| shot);
            to_value(&FeedbackScreenshot { screenshot })
        }
        _ => Err(ApiError::new(ErrorKind::Internal, "not a tray method")),
    }
}

fn activate(
    engine: &mut Engine,
    connection: ConnectionId,
    params: TrayActivateParams,
    granted: &[Capability],
) -> Result<TrayAction, ApiError> {
    if (params.item.starts_with("stop-")
        || params.item == "login-item"
        || params.item.starts_with("launch-"))
        && !granted.contains(&Capability::Config)
    {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            Capability::Config.denied_message(),
        ));
    }
    Ok(match params.item.as_str() {
        // What `uxduck open` does: the desktop app when chosen (S110), else
        // the page already open, else a new one (S107: never a second).
        "open" => opened(engine.open_app(None, &[], Some(LaunchAction::Window))?),
        "open-browser" => opened(engine.open_app(None, &[], Some(LaunchAction::Browser))?),
        "launch-window" | "launch-browser" | "launch-menu-bar" => {
            engine.set_launch_action(match params.item.as_str() {
                "launch-browser" => LaunchAction::Browser,
                "launch-menu-bar" => LaunchAction::MenuBar,
                _ => LaunchAction::Window,
            });
            TrayAction::Done
        }
        "simulations" => open_page(engine, "/vehicles#simulated")?,
        "stop-simulations" => stop_all_simulations(engine, connection, granted)?,
        "docs" => TrayAction::OpenUrl { url: docs_url() },
        // The whole data folder: recordings, logs, the database (S108).
        "show-files" => {
            TrayAction::Reveal {
                path: engine.config.about.data_dir.clone().ok_or_else(|| {
                    ApiError::new(ErrorKind::NotFound, "This UXDuck keeps no files")
                })?,
            }
        }
        "login-item" => toggle_login_item(engine)?,
        "login-item-settings" => TrayAction::OpenUrl {
            url: LOGIN_ITEMS_SETTINGS.into(),
        },
        "feedback" => {
            let id = keep_screenshot(engine, params.screenshot)?;
            opened(engine.open_app(None, &[format!("--feedback={id}")], None)?)
        }
        // Explicit menu Quit stops immediately. Keep the old confirmation's
        // follow-up id working for clients connected before an update.
        "quit" | "quit-now" => {
            engine.stop = true;
            TrayAction::Done
        }
        other => {
            if let Some(id) = other.strip_prefix("stop-simulation:") {
                stop_simulation(engine, connection, id, granted)?;
                return Ok(TrayAction::Done);
            }
            if let Some(id) = other.strip_prefix("open-simulation:") {
                if !engine
                    .state
                    .links
                    .get(id)
                    .is_some_and(|link| link.kind.is_simulator())
                {
                    return Err(ApiError::new(
                        ErrorKind::NotFound,
                        "This simulation has stopped",
                    ));
                }
                let vehicle = engine
                    .state
                    .vehicles
                    .values()
                    .find(|vehicle| vehicle.link_id == id)
                    .ok_or_else(|| {
                        ApiError::new(ErrorKind::NotFound, "This simulation has no vehicle yet")
                    })?;
                let page = format!("/vehicles/{}", vehicle.id);
                return open_page(engine, &page);
            }
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                format!("no tray item {other}"),
            ));
        }
    })
}

/// "Open at login" chosen: the opposite of what the system has; System
/// Settings when macOS waits for the person's OK (S108).
fn toggle_login_item(engine: &mut Engine) -> Result<TrayAction, ApiError> {
    let on = !engine
        .login_item
        .as_ref()
        .map_or(engine.state.settings.start_at_login, |item| {
            item.state == LoginItemState::On
        });
    let item = engine
        .system
        .start_at_login(on)
        .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
    let needs_approval = item.state == LoginItemState::NeedsApproval;
    engine.login_item_changed(item);
    Ok(if needs_approval {
        TrayAction::OpenUrl {
            url: LOGIN_ITEMS_SETTINGS.into(),
        }
    } else {
        TrayAction::Done
    })
}

fn stop_all_simulations(
    engine: &mut Engine,
    connection: ConnectionId,
    granted: &[Capability],
) -> Result<TrayAction, ApiError> {
    let ids: Vec<String> = engine
        .state
        .links
        .values()
        .filter(|link| link.kind.is_simulator())
        .map(|link| link.id.clone())
        .collect();
    let mut failures = Vec::new();
    for id in ids {
        if let Err(error) = stop_simulation(engine, connection, &id, granted) {
            failures.push(error.message);
        }
    }
    if !failures.is_empty() {
        return Err(ApiError::new(ErrorKind::Internal, failures.join("; ")));
    }
    Ok(TrayAction::Done)
}

fn item(id: &str, label: &str, separator_before: bool) -> TrayItem {
    TrayItem {
        id: id.into(),
        label: label.into(),
        separator_before,
        children: Vec::new(),
        enabled: true,
        checked: None,
    }
}

/// Where System Settings shows login items, to allow UXDuck's.
pub const LOGIN_ITEMS_SETTINGS: &str =
    "x-apple.systempreferences:com.apple.LoginItems-Settings.extension";

fn open_page(engine: &mut Engine, page: &str) -> Result<TrayAction, ApiError> {
    Ok(opened(engine.open_app(Some(page), &[], None)?))
}

/// What the tray does after `open_app`: nothing when UXDuck opened, else
/// open the address.
fn opened(result: AppOpenResult) -> TrayAction {
    result
        .url
        .map_or(TrayAction::Done, |url| TrayAction::OpenUrl { url })
}

fn stop_simulation(
    engine: &mut Engine,
    connection: ConnectionId,
    id: &str,
    granted: &[Capability],
) -> Result<(), ApiError> {
    crate::sim::handle(
        engine,
        connection,
        ClientRequest::SimStop(uxduck_contracts::dto::link::IdParams { id: id.into() }),
        granted,
    )?;
    Ok(())
}

/// Keeps the tray's screenshot for the feedback dialog to fetch; old ones go.
fn keep_screenshot(engine: &mut Engine, screenshot: Option<String>) -> Result<String, ApiError> {
    if screenshot
        .as_ref()
        .is_some_and(|s| s.len() > SCREENSHOT_BYTES)
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "the screenshot is larger than 6 MB",
        ));
    }
    let data_url = screenshot.map(|base64| {
        let kind = if base64.starts_with("iVBOR") {
            "png"
        } else {
            "jpeg"
        };
        format!("data:image/{kind};base64,{base64}")
    });
    let now = engine.clock.now_us();
    let id = format!("ss{}", &engine.random.token()[..16]);
    let kept = &mut engine.tray_screenshots.0;
    kept.retain(|_, (at, _)| now.saturating_sub(*at) < SCREENSHOT_US);
    kept.insert(id.clone(), (now, data_url));
    Ok(id)
}

fn link_words(status: LinkStatus) -> &'static str {
    match status {
        LinkStatus::PortInUse => "its port is in use",
        LinkStatus::DisabledByLoopDetection => "turned off: messages were looping back",
        LinkStatus::Disabled => "turned off",
        LinkStatus::Stopped => "not started",
        _ => "not connected",
    }
}

impl Engine {
    /// A tray that cannot recover must be visible in the app as well as stderr.
    pub fn tray_failed(&mut self, reason: &str) {
        self.raise(Raise {
            severity: Severity::Alarm,
            kind: NotificationKind::Persistent,
            subject: Subject::System,
            title: "UXDuck's menu bar needs attention".into(),
            body: Some(reason.into()),
            actions: Vec::new(),
            condition: Some("tray-failed".into()),
            report: None,
        });
    }

    /// Clears the failure once the helper is running again or deliberately off.
    pub fn tray_recovered(&mut self) {
        self.resolve("tray-failed");
    }
    /// The menu as it stands.
    #[must_use]
    pub fn tray_menu(&self) -> TrayMenu {
        let state = &self.state;
        let waiting = state.approvals.len();
        let alarm = state
            .notifications
            .values()
            .find(|n| n.state == NotificationState::Active && n.severity == Severity::Alarm);
        let broken = state.links.values().find(|l| {
            matches!(
                l.status,
                LinkStatus::Down | LinkStatus::PortInUse | LinkStatus::DisabledByLoopDetection
            )
        });
        let (status, tone) = if waiting > 0 {
            let requests = if waiting == 1 {
                "1 request".to_owned()
            } else {
                format!("{waiting} requests")
            };
            (
                format!("Waiting for your answer: {requests}"),
                TrayTone::Waiting,
            )
        } else if let Some(alarm) = alarm {
            (alarm.title.clone(), TrayTone::Broken)
        } else if let Some(link) = broken {
            (
                format!("{}: {}", link.name, link_words(link.status)),
                TrayTone::Broken,
            )
        } else {
            let online: Vec<&str> = state
                .vehicles
                .values()
                .filter(|v| v.presence == Presence::Online)
                .map(|v| v.name.as_str())
                .collect();
            let status = match online.as_slice() {
                [] => "UXDuck is running".to_owned(),
                [one] => format!("UXDuck is running · {one} online"),
                many => format!("UXDuck is running · {} vehicles online", many.len()),
            };
            (status, TrayTone::Plain)
        };
        let mut launch = item("launch", "When I open UXDuck", false);
        launch.children = [
            ("launch-window", "Open a window", LaunchAction::Window),
            ("launch-browser", "Open in browser", LaunchAction::Browser),
            (
                "launch-menu-bar",
                "Run in menu bar only",
                LaunchAction::MenuBar,
            ),
        ]
        .into_iter()
        .map(|(id, label, action)| {
            let mut choice = item(id, label, false);
            choice.checked = Some(state.settings.launch_action == action);
            choice
        })
        .collect();
        let mut items = vec![
            item("open", "Open window", false),
            item("open-browser", "Open in browser", false),
            launch,
        ];
        items.extend(self.simulation_menu());
        items.push(item("show-files", "Show files", true));
        items.extend(self.login_menu());
        items.extend([
            item("docs", "Open documentation", true),
            item("feedback", "Send feedback…", false),
            item("quit", "Quit UXDuck", true),
        ]);
        TrayMenu {
            status,
            tone,
            items,
        }
    }

    /// "Open at login" as a switch showing what the system has (S108); the
    /// way to allow it when macOS waits for the person; nothing where it
    /// cannot be had.
    fn login_menu(&self) -> Option<TrayItem> {
        let state = self.login_item.as_ref().map(|item| item.state);
        let mut login = match state {
            Some(LoginItemState::Unavailable) => return None,
            Some(LoginItemState::NeedsApproval) => item(
                "login-item-settings",
                "Open at login: allow in System Settings…",
                false,
            ),
            _ => item("login-item", "Open at login", false),
        };
        if state != Some(LoginItemState::NeedsApproval) {
            login.checked = Some(state.map_or(self.state.settings.start_at_login, |state| {
                state == LoginItemState::On
            }));
        }
        Some(login)
    }

    fn simulation_menu(&self) -> Vec<TrayItem> {
        let mut items = Vec::new();
        for link in self
            .state
            .links
            .values()
            .filter(|link| link.kind.is_simulator())
        {
            let vehicle = self
                .state
                .vehicles
                .values()
                .find(|vehicle| vehicle.link_id == link.id);
            let name = vehicle.map_or(link.name.as_str(), |vehicle| vehicle.name.as_str());
            let status = if link.status == LinkStatus::Down
                || link.launch.as_ref().is_some_and(|launch| {
                    launch.stage == uxduck_core::domain::link::LaunchStage::Failed
                }) {
                "Failed"
            } else if link.status == LinkStatus::Stopped {
                "Not started"
            } else if vehicle.is_none() || link.status == LinkStatus::Configured {
                "Starting…"
            } else {
                "Running"
            };
            let mut open = item(
                &format!("open-simulation:{}", link.id),
                "Open vehicle",
                false,
            );
            open.enabled = vehicle.is_some();
            let mut simulation = item(
                &format!("simulation:{}", link.id),
                &format!("{name} · {status}"),
                items.is_empty(),
            );
            // One from the last run is not running: stopping removes it.
            let stop_words = if link.status == LinkStatus::Stopped {
                "Remove simulation"
            } else {
                "Stop simulation"
            };
            simulation.children = vec![
                open,
                item(&format!("stop-simulation:{}", link.id), stop_words, false),
            ];
            items.push(simulation);
        }
        let mut stop_all = item("stop-simulations", "Stop all simulations", items.is_empty());
        stop_all.enabled = self
            .state
            .links
            .values()
            .any(|link| link.kind.is_simulator());
        items.push(stop_all);
        items
    }

    /// Sends the menu to the tray when it changed.
    pub(crate) fn push_tray(&mut self) {
        if self.tray_watchers.is_empty() {
            return;
        }
        let menu = self.tray_menu();
        if self.last_tray_menu.as_ref() == Some(&menu) {
            return;
        }
        let frame = json!({ "jsonrpc": "2.0", "method": "tray/menuChanged", "params": menu });
        self.last_tray_menu = Some(menu);
        let watchers: Vec<ConnectionId> = self.tray_watchers.iter().copied().collect();
        for connection in watchers {
            self.send(connection, &frame);
        }
    }

    /// UXDuck was opened again while it ran: the tray says so.
    pub(crate) fn tray_reopened(&mut self) {
        let frame = json!({ "jsonrpc": "2.0", "method": "tray/reopened", "params": {} });
        let watchers: Vec<ConnectionId> = self.tray_watchers.iter().copied().collect();
        for connection in watchers {
            self.send(connection, &frame);
        }
    }

    /// A forward was added: suggest starting at login, once, so it runs
    /// after a restart with no window open (ADR-0008).
    pub(crate) fn suggest_start_at_login(&mut self) {
        let settings = &self.state.settings;
        if settings.start_at_login || settings.start_at_login_suggested {
            return;
        }
        let mut settings = settings.clone();
        settings.start_at_login_suggested = true;
        settings.version += 1;
        self.commit_as(
            crate::plugins::daemon(),
            Change::SettingsChanged { settings },
        );
        self.raise(Raise {
            severity: Severity::Info,
            kind: NotificationKind::Transient,
            subject: Subject::System,
            title: "Keep UXDuck running after a restart?".into(),
            body: Some(
                "Turn on Open at login in Settings and it'll start quietly when you log in.".into(),
            ),
            actions: Vec::new(),
            condition: None,
            report: None,
        });
    }
}
