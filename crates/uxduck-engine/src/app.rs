//! Opening UXDuck again, and quitting it (S107, ADR-0025 D1). Opening
//! UXDuck while a page is connected brings that page forward and opens
//! nothing new; quitting says first what would stop.

use serde_json::{Value, json};
use uxduck_contracts::dto::app::{
    AppFocusResult, AppOpenParams, AppOpenResult, ClientVisibilityParams, DesktopAppChooseParams,
    DesktopAppState, DesktopAppStatus, StopCount, StopKind, StopPreview,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::{Capability, ClientKind};
use uxduck_core::domain::link::{LinkKind, LinkStatus};
use uxduck_core::domain::settings::LaunchAction;
use uxduck_core::domain::vehicle::Presence;
use uxduck_core::domain::video::RecordingState;
use uxduck_core::events::Change;
use uxduck_core::policy::admission::Trust;

use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: &ClientRequest,
    operator: bool,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    let require = |capability: Capability| {
        if granted.contains(&capability) {
            Ok(())
        } else {
            Err(ApiError::new(
                ErrorKind::CapabilityDenied,
                capability.denied_message(),
            ))
        }
    };
    match request {
        ClientRequest::DesktopAppStatus(params) => {
            require(Capability::Read)?;
            to_value(&engine.desktop_app_status(params.refresh))
        }
        ClientRequest::DesktopAppDownload(_) => {
            require(Capability::Config)?;
            engine
                .system
                .desktop_app_download()
                .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
            Ok(json!({}))
        }
        ClientRequest::DesktopAppChoose(DesktopAppChooseParams { on }) => {
            require(Capability::Config)?;
            let path = if *on {
                Some(
                    engine
                        .system
                        .desktop_app_path()
                        .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?,
                )
            } else {
                None
            };
            engine.set_shell_path(path);
            to_value(&engine.desktop_app_status(false))
        }
        ClientRequest::DesktopAppRemove(_) => {
            require(Capability::Config)?;
            engine.set_shell_path(None);
            engine
                .system
                .desktop_app_remove()
                .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
            to_value(&engine.desktop_app_status(false))
        }
        ClientRequest::ClientVisibility(ClientVisibilityParams { visible }) => {
            let visible = *visible;
            engine.opened.seen_order += 1;
            let order = engine.opened.seen_order;
            if let Some(state) = engine.connections.get_mut(&connection) {
                state.visible = visible;
                state.seen = order;
            }
            Ok(json!({}))
        }
        _ if !operator => Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "Only UXDuck Desktop can do that.",
        )),
        ClientRequest::AppFocus(_) => to_value(&AppFocusResult {
            focused: engine.focus_page(),
        }),
        ClientRequest::AppOpen(AppOpenParams {
            page,
            reopened,
            target,
        }) => {
            if *reopened {
                engine.tray_reopened();
            }
            to_value(&engine.open_app(page.as_deref(), &[], target.map(Into::into))?)
        }
        ClientRequest::DaemonStopPreview(_) => to_value(&engine.stop_preview()),
        // The computer's addresses are for the people at it (S97).
        ClientRequest::NetworkStatus(_) => to_value(&engine.network()),
        _ => Err(ApiError::new(ErrorKind::Internal, "not an app method")),
    }
}

/// "1 simulation", "a forward link", "2 recordings": one kind's words.
fn words(kind: StopKind, count: u32, vehicle: Option<&str>) -> String {
    let (one, many) = match kind {
        StopKind::Simulation => ("1 simulation", "simulations"),
        StopKind::ForwardLink => ("a forward link", "forward links"),
        StopKind::Vehicle => ("", "vehicles"),
        StopKind::Recording => ("a recording", "recordings"),
    };
    match (kind, count, vehicle) {
        (StopKind::Vehicle, 1, Some(name)) => format!("the connection to {name}"),
        (StopKind::Vehicle, 1, None) => "the connection to a vehicle".into(),
        (StopKind::Vehicle, n, _) => format!("the connections to {n} {many}"),
        (_, 1, _) => one.into(),
        (_, n, _) => format!("{n} {many}"),
    }
}

impl Engine {
    /// Only deliberate launches consult the saved choice. Explicit menu
    /// actions can always open a window or browser, without changing it.
    pub(crate) fn open_app(
        &mut self,
        page: Option<&str>,
        extra: &[String],
        target: Option<LaunchAction>,
    ) -> Result<AppOpenResult, ApiError> {
        let action = target.unwrap_or(self.state.settings.launch_action);
        if action == LaunchAction::MenuBar && page.is_none() && extra.is_empty() {
            return Ok(AppOpenResult {
                opened: true,
                url: None,
            });
        }
        // Opening a particular page is explicit, even in menu-bar-only mode.
        if action != LaunchAction::Browser {
            let path = self
                .system
                .desktop_app_path()
                .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
            let args: Vec<String> = page
                .map(|page| format!("--page={page}"))
                .into_iter()
                .chain(extra.iter().cloned())
                .collect();
            self.system
                .desktop_app_open(&path, &args)
                .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
            return Ok(AppOpenResult {
                opened: true,
                url: None,
            });
        }
        if page.is_none() && extra.is_empty() && self.focus_page() {
            return Ok(AppOpenResult {
                opened: true,
                url: None,
            });
        }
        let token = self.create_bootstrap();
        let base = self.config.ui_url.trim_end_matches('/').to_owned();
        let (path, hash) =
            page.map_or(("/", ""), |page| page.split_once('#').unwrap_or((page, "")));
        let query = std::iter::once(format!("bootstrap={token}"))
            .chain(extra.iter().filter_map(|arg| {
                arg.strip_prefix("--feedback=")
                    .map(|id| format!("feedback={id}"))
            }))
            .collect::<Vec<_>>()
            .join("&");
        let hash = if hash.is_empty() {
            String::new()
        } else {
            format!("#{hash}")
        };
        Ok(AppOpenResult {
            opened: false,
            url: Some(format!("{base}{path}?{query}{hash}")),
        })
    }

    /// Opens UXDuck in the desktop app at `path`, or in a browser.
    pub(crate) fn set_shell_path(&mut self, path: Option<String>) {
        let action = if path.is_some() {
            LaunchAction::Window
        } else {
            LaunchAction::Browser
        };
        if self.state.settings.shell_path == path && self.state.settings.launch_action == action {
            return;
        }
        let mut settings = self.state.settings.clone();
        settings.launch_action = action;
        settings.shell_path = path;
        settings.version += 1;
        self.commit_as(
            crate::plugins::daemon(),
            Change::SettingsChanged { settings },
        );
        self.push_tray();
    }

    pub(crate) fn set_launch_action(&mut self, action: LaunchAction) {
        let mut settings = self.state.settings.clone();
        settings.launch_action = action;
        if action == LaunchAction::MenuBar {
            settings.show_in_menu_bar = true;
        }
        if settings == self.state.settings {
            return;
        }
        settings.version += 1;
        self.commit_as(
            crate::plugins::daemon(),
            Change::SettingsChanged { settings },
        );
        self.push_tray();
    }

    /// "Open as a desktop app" on the page Safari and Firefox show (S110):
    /// downloads it when it is not here yet (the page has shown its size),
    /// else chooses it and opens UXDuck in it.
    ///
    /// # Errors
    /// When there is none to download, or it does not open.
    pub fn open_in_desktop_app(&mut self) -> Result<DesktopAppStatus, String> {
        let status = self.desktop_app_status(false);
        match status.state {
            DesktopAppState::Installed => {
                let path = self.system.desktop_app_path()?;
                self.set_shell_path(Some(path));
                self.open_app(None, &[], Some(LaunchAction::Window))
                    .map_err(|error| error.message)?;
            }
            DesktopAppState::Available | DesktopAppState::Failed if status.version.is_some() => {
                self.system.desktop_app_download()?;
            }
            DesktopAppState::Downloading => {}
            _ => {
                return Err(status
                    .detail
                    .unwrap_or_else(|| "There is no desktop app for this computer yet.".into()));
            }
        }
        Ok(self.desktop_app_status(false))
    }

    /// The desktop app as the host has it, and whether it is chosen.
    pub fn desktop_app_status(&mut self, refresh: bool) -> DesktopAppStatus {
        let mut status = self.system.desktop_app(refresh);
        status.chosen = self.state.settings.launch_action == LaunchAction::Window;
        status
    }

    /// Asks one browser page to come forward, never an Electron or remote
    /// page. A desktop window is brought forward by its own process.
    pub(crate) fn focus_page(&mut self) -> bool {
        let page = self
            .connections
            .iter()
            .filter(|(_, c)| {
                c.admitted
                    .as_ref()
                    .is_some_and(|a| a.kind == ClientKind::OperatorUi)
                    && matches!(c.trust, Trust::PairedBrowser { .. })
            })
            .max_by_key(|(_, c)| (c.visible, c.seen))
            .map(|(id, _)| *id);
        let Some(page) = page else {
            return false;
        };
        self.send(
            page,
            &json!({ "jsonrpc": "2.0", "method": "app/focus", "params": {} }),
        );
        true
    }

    /// What quitting would stop now (S107).
    #[must_use]
    pub fn stop_preview(&self) -> StopPreview {
        let state = &self.state;
        let simulations = state
            .links
            .values()
            .filter(|l| l.kind.is_simulator() && l.status != LinkStatus::Stopped)
            .count();
        let forwards = state
            .links
            .values()
            .filter(|l| {
                matches!(l.kind, LinkKind::UdpForward { .. })
                    && !matches!(l.status, LinkStatus::Disabled | LinkStatus::Stopped)
            })
            .count();
        let vehicles: Vec<&str> = state
            .vehicles
            .values()
            .filter(|v| {
                v.presence == Presence::Online
                    && !state
                        .links
                        .get(&v.link_id)
                        .is_some_and(|l| l.kind.is_simulator())
            })
            .map(|v| v.name.as_str())
            .collect();
        let recordings = state
            .video_recordings
            .values()
            .filter(|r| r.state == RecordingState::Recording)
            .count();
        let stops: Vec<StopCount> = [
            (StopKind::Simulation, simulations),
            (StopKind::ForwardLink, forwards),
            (StopKind::Vehicle, vehicles.len()),
            (StopKind::Recording, recordings),
        ]
        .into_iter()
        .filter(|(_, count)| *count > 0)
        .map(|(kind, count)| StopCount {
            kind,
            count: u32::try_from(count).unwrap_or(u32::MAX),
        })
        .collect();
        let parts: Vec<String> = stops
            .iter()
            .map(|s| words(s.kind, s.count, vehicles.first().copied()))
            .collect();
        let summary = match parts.as_slice() {
            [] => None,
            [one] => Some(format!("{one} will stop.")),
            [rest @ .., last] => Some(format!("{} and {last} will stop.", rest.join(", "))),
        }
        .map(|text| {
            let mut chars = text.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        });
        StopPreview { stops, summary }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_kind_reads_as_the_tray_says_it() {
        assert_eq!(words(StopKind::Simulation, 1, None), "1 simulation");
        assert_eq!(words(StopKind::Simulation, 2, None), "2 simulations");
        assert_eq!(words(StopKind::ForwardLink, 1, None), "a forward link");
        assert_eq!(
            words(StopKind::Vehicle, 1, Some("Alpha")),
            "the connection to Alpha"
        );
        assert_eq!(
            words(StopKind::Vehicle, 3, Some("Alpha")),
            "the connections to 3 vehicles"
        );
        assert_eq!(words(StopKind::Recording, 2, None), "2 recordings");
    }
}
