//! Open at login (S108, ADR-0025 D5): registered where each system lists
//! such things, so the person can see it and switch it off there too.
//!
//! - macOS: a login item from the launch agent inside UXDuck.app, registered
//!   with `SMAppService` by the menu-bar helper (`uxduck-tray --login-item`),
//!   since that is the app's executable. It runs `uxduck daemon start`. It
//!   shows as UXDuck in System Settings › General › Login Items, and goes
//!   when the app is trashed.
//! - Windows: the per-user `Run` entry, which Task Manager › Startup apps
//!   shows and can switch off. It runs the helper with `--background`.
//! - Linux: `~/.config/autostart/uxduck.desktop` on a desktop, the systemd
//!   user unit on a computer with no display (a Pi, a server).
//!
//! Each starts UXDuck with no page open. Turning it on or off also removes
//! what `uxduck service install` wrote before (a launchd file, a scheduled
//! task), which people could not see or switch off.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use uxduck_contracts::dto::settings::{LoginItem, LoginItemState};

/// The first line of the files UXDuck writes, so it only removes its own.
const MARK: &str = "Written by UXDuck";
/// The old launchd agent (S64) and Task Scheduler task.
const OLD_LABEL: &str = "com.uxduck.daemon";
const OLD_TASK: &str = "UXDuck";
/// The `Run` value's name on Windows.
#[cfg_attr(not(windows), allow(dead_code))]
const RUN_VALUE: &str = "UXDuck";
#[cfg_attr(not(windows), allow(dead_code))]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg_attr(not(windows), allow(dead_code))]
const APPROVED_KEY: &str =
    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn item(state: LoginItemState) -> LoginItem {
    LoginItem {
        state,
        detail: None,
    }
}

fn unavailable(detail: impl Into<String>) -> LoginItem {
    LoginItem {
        state: LoginItemState::Unavailable,
        detail: Some(detail.into()),
    }
}

/// Whether UXDuck opens at login now, as the system says.
#[must_use]
pub fn status() -> LoginItem {
    platform::status()
}

/// Turns opening at login on or off, and removes the old registrations.
///
/// # Errors
/// Why it could not, in words.
pub fn set(on: bool) -> Result<LoginItem, String> {
    remove_old();
    platform::set(on)
}

/// What `uxduck service install` wrote before S108, when it is still there.
fn remove_old() {
    if cfg!(target_os = "macos")
        && let Some(home) = std::env::var_os("HOME")
    {
        let plist = Path::new(&home)
            .join("Library/LaunchAgents")
            .join(format!("{OLD_LABEL}.plist"));
        let ours = std::fs::read_to_string(&plist)
            .is_ok_and(|text| text.contains("Written by `uxduck service install`"));
        if ours {
            let _ = std::fs::remove_file(plist);
        }
    }
    if cfg!(windows) {
        let _ = quiet("schtasks")
            .args(["/Delete", "/TN", OLD_TASK, "/F"])
            .output();
    }
}

/// A command with no console window and no output of its own.
fn quiet(program: &str) -> Command {
    let mut command = Command::new(program);
    command.stdin(Stdio::null()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x0800_0000);
    }
    command
}

/// The menu-bar helper's answer: one line of JSON with `state` and maybe
/// `detail`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_helper(output: &str) -> Result<LoginItem, String> {
    serde_json::from_str(output.trim())
        .map_err(|error| format!("the menu-bar helper answered something else: {error}"))
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{LoginItem, LoginItemState, parse_helper, quiet, unavailable};

    fn helper(arg: &str) -> Result<LoginItem, String> {
        let Some(tray) = crate::tray::binary() else {
            return Ok(unavailable(
                "Open at login needs UXDuck installed in Applications.",
            ));
        };
        let output = quiet(&tray.display().to_string())
            .args(["--login-item", arg])
            .output()
            .map_err(|error| format!("Cannot ask the menu-bar helper: {error}"))?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
        }
        parse_helper(&String::from_utf8_lossy(&output.stdout))
    }

    pub fn status() -> LoginItem {
        helper("status").unwrap_or_else(unavailable)
    }

    pub fn set(on: bool) -> Result<LoginItem, String> {
        let item = helper(if on { "on" } else { "off" })?;
        if item.state == LoginItemState::Unavailable {
            return Err(item.detail.unwrap_or_default());
        }
        Ok(item)
    }
}

#[cfg(windows)]
mod platform {
    use super::{APPROVED_KEY, LoginItem, LoginItemState, RUN_KEY, RUN_VALUE, item, quiet};

    /// What the `Run` entry runs: the helper, which starts UXDuck with no
    /// page and no console window.
    fn command() -> Result<String, String> {
        let tray = crate::tray::binary()
            .ok_or("Open at login needs UXDuck installed from its installer.")?;
        Ok(format!("\"{}\" --background", tray.display()))
    }

    fn query(key: &str) -> Option<String> {
        let output = quiet("reg")
            .args(["query", key, "/v", RUN_VALUE])
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }

    pub fn status() -> LoginItem {
        if query(RUN_KEY).is_none() {
            return item(LoginItemState::Off);
        }
        // Task Manager keeps its own switch: a first byte that is odd (3)
        // means switched off there.
        let off = query(APPROVED_KEY).is_some_and(|text| {
            text.split_whitespace()
                .last()
                .and_then(|hex| u8::from_str_radix(hex.get(..2)?, 16).ok())
                .is_some_and(|first| first % 2 == 1)
        });
        if off {
            LoginItem {
                state: LoginItemState::Off,
                detail: Some("Switched off in Task Manager › Startup apps.".into()),
            }
        } else {
            item(LoginItemState::On)
        }
    }

    pub fn set(on: bool) -> Result<LoginItem, String> {
        let output = if on {
            let command = command()?;
            // A fresh entry starts out allowed in Task Manager.
            let _ = quiet("reg")
                .args(["delete", APPROVED_KEY, "/v", RUN_VALUE, "/f"])
                .output();
            quiet("reg")
                .args(["add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/d"])
                .arg(command)
                .arg("/f")
                .output()
        } else {
            if query(RUN_KEY).is_none() {
                return Ok(item(LoginItemState::Off));
            }
            quiet("reg")
                .args(["delete", RUN_KEY, "/v", RUN_VALUE, "/f"])
                .output()
        }
        .map_err(|error| format!("reg: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "reg said: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(status())
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use super::{LoginItem, LoginItemState, MARK, autostart_entry, config_dir, item, unavailable};

    fn desktop() -> bool {
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
    }

    fn autostart() -> Option<std::path::PathBuf> {
        Some(config_dir()?.join("autostart/uxduck.desktop"))
    }

    pub fn status() -> LoginItem {
        if !desktop() {
            return if crate::service::systemd_enabled() {
                item(LoginItemState::On)
            } else {
                item(LoginItemState::Off)
            };
        }
        let Some(path) = autostart() else {
            return unavailable("No home folder.");
        };
        match std::fs::read_to_string(path) {
            Ok(text)
                if !text.contains("Hidden=true")
                    && !text.contains("X-GNOME-Autostart-enabled=false") =>
            {
                item(LoginItemState::On)
            }
            _ => item(LoginItemState::Off),
        }
    }

    pub fn set(on: bool) -> Result<LoginItem, String> {
        if !desktop() {
            crate::service::systemd(on)?;
            return Ok(status());
        }
        let path = autostart().ok_or("No home folder to register UXDuck in.")?;
        if on {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
            std::fs::write(&path, autostart_entry(&exe))
                .map_err(|e| format!("{}: {e}", path.display()))?;
        } else if std::fs::read_to_string(&path).is_ok_and(|text| text.contains(MARK)) {
            std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        Ok(status())
    }
}

/// `~/.config`, or `XDG_CONFIG_HOME`.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn config_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
}

/// The Linux desktop's autostart entry: UXDuck with no page, at login.
#[must_use]
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn autostart_entry(exe: &Path) -> String {
    format!(
        "# {MARK}: UXDuck opens at login.\n\
         [Desktop Entry]\n\
         Type=Application\n\
         Name=UXDuck\n\
         Comment=Starts UXDuck in the background when you log in\n\
         Exec=\"{}\" daemon start\n\
         NoDisplay=true\n\
         X-GNOME-Autostart-enabled=true\n",
        exe.display().to_string().replace('"', "\\\"")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_autostart_entry_starts_uxduck_with_no_page() {
        assert_eq!(
            autostart_entry(Path::new("/usr/bin/uxduck")),
            "# Written by UXDuck: UXDuck opens at login.\n\
             [Desktop Entry]\n\
             Type=Application\n\
             Name=UXDuck\n\
             Comment=Starts UXDuck in the background when you log in\n\
             Exec=\"/usr/bin/uxduck\" daemon start\n\
             NoDisplay=true\n\
             X-GNOME-Autostart-enabled=true\n"
        );
    }

    #[test]
    fn the_helper_s_answer_is_read() {
        assert_eq!(
            parse_helper("{\"state\":\"needs-approval\"}\n").unwrap(),
            LoginItem {
                state: LoginItemState::NeedsApproval,
                detail: None
            }
        );
        assert!(parse_helper("oops").is_err());
    }
}
