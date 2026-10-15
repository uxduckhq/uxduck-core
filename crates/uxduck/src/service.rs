//! `uxduck service install` and `uninstall` (S64): Open at login from the
//! command line, the same registration as the switch in the menu bar and
//! Settings (S108, `adapters::login_item`). On Linux with no display (a
//! Pi, a server) that is the systemd user unit written here.

use std::path::{Path, PathBuf};

use uxduck_contracts::dto::settings::{LoginItem, LoginItemState};

/// The first line of the unit UXDuck writes, so it only removes its own.
const MARK: &str = "Written by `uxduck service install`";

/// The systemd user unit.
#[must_use]
pub fn systemd_unit(exe: &Path, data_dir: &Path) -> String {
    let quote = |p: &Path| format!("\"{}\"", p.display().to_string().replace('"', "\\\""));
    format!(
        "# {MARK}: UXDuck starts at login.\n\
         [Unit]\n\
         Description=UXDuck\n\
         Documentation=https://uxduck.com\n\
         After=graphical-session.target network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={} daemon --data-dir {}\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        quote(exe),
        quote(data_dir)
    )
}

/// Where the Linux unit is written.
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
fn unit_file() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "no home folder to register UXDuck in".to_owned())?;
    Ok(std::env::var_os("XDG_CONFIG_HOME")
        .map_or_else(|| home.join(".config"), PathBuf::from)
        .join("systemd/user/uxduck.service"))
}

#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
fn run(program: &str, args: &[&str]) -> Result<(), String> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program} said: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Whether the systemd user unit is enabled.
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
pub fn systemd_enabled() -> bool {
    run(
        "systemctl",
        &["--user", "is-enabled", "--quiet", "uxduck.service"],
    )
    .is_ok()
}

/// Writes and enables the systemd user unit, or disables it and removes
/// the unit UXDuck wrote. A unit a package installed stays, disabled.
///
/// # Errors
/// Why it could not, in words.
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
pub fn systemd(on: bool) -> Result<(), String> {
    let path = unit_file()?;
    if on {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let data_dir = crate::paths::data_dir(None, false);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        std::fs::write(&path, systemd_unit(&exe, &data_dir))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        run("systemctl", &["--user", "daemon-reload"])?;
        run("systemctl", &["--user", "enable", "uxduck.service"])
    } else {
        let _ = run("systemctl", &["--user", "disable", "uxduck.service"]);
        let ours = std::fs::read_to_string(&path).is_ok_and(|text| text.contains(MARK));
        if ours {
            std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        Ok(())
    }
}

/// What the command says once it is done.
fn said(item: &LoginItem) -> String {
    match item.state {
        LoginItemState::On => "UXDuck opens at login.".into(),
        LoginItemState::Off => "UXDuck no longer opens at login.".into(),
        LoginItemState::NeedsApproval => "UXDuck will open at login once you allow it in System Settings › General › Login Items.".into(),
        LoginItemState::Unavailable => item.detail.clone().unwrap_or_default(),
    }
}

/// Turns Open at login on or off.
///
/// # Errors
/// Why it could not, in words.
pub fn set(on: bool) -> Result<String, String> {
    let item = crate::adapters::login_item::set(on)?;
    if item.state == LoginItemState::Unavailable {
        return Err(said(&item));
    }
    Ok(said(&item))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_systemd_unit_runs_the_daemon_at_login() {
        let unit = systemd_unit(
            Path::new("/home/ana/.local/bin/uxduck"),
            Path::new("/home/ana/.local/share/uxduck"),
        );
        assert_eq!(
            unit,
            "# Written by `uxduck service install`: UXDuck starts at login.\n\
             [Unit]\n\
             Description=UXDuck\n\
             Documentation=https://uxduck.com\n\
             After=graphical-session.target network-online.target\n\
             \n\
             [Service]\n\
             ExecStart=\"/home/ana/.local/bin/uxduck\" daemon --data-dir \"/home/ana/.local/share/uxduck\"\n\
             Restart=on-failure\n\
             RestartSec=2\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n"
        );
    }

    #[test]
    fn the_command_says_what_the_system_has() {
        let item = |state| LoginItem {
            state,
            detail: None,
        };
        assert_eq!(said(&item(LoginItemState::On)), "UXDuck opens at login.");
        assert!(said(&item(LoginItemState::NeedsApproval)).contains("Login Items"));
    }
}
