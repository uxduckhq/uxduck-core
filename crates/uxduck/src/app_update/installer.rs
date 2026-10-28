//! Platform update mechanisms. Arguments stay separate; no shell interpolation.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(windows, test))]
mod windows;

/// Where the Mac app is installed and updated; only Unix builds use it.
#[cfg(unix)]
pub(super) const INSTALLED_APP: &str = "/Applications/UXDuck.app";

/// Validate helpers and, on macOS, Gatekeeper acceptance before shutting down.
pub(super) fn preflight(package: &Path) -> Result<(), String> {
    if !package.is_file() {
        return Err("The downloaded installer is missing. Check for updates again.".into());
    }
    #[cfg(target_os = "macos")]
    {
        macos::check_destination(Path::new(INSTALLED_APP))?;
        let result = Command::new("/usr/sbin/spctl")
            .args(["--assess", "--type", "install"])
            .arg(package)
            .output()
            .map_err(|e| e.to_string())?;
        if !result.status.success() {
            return Err(format!(
                "macOS did not accept the signed installer: {}",
                String::from_utf8_lossy(&result.stderr)
            ));
        }
    }
    #[cfg(target_os = "linux")]
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        return Err("Installing an update needs a desktop authorization agent. On a headless computer, install the downloaded .deb manually with your package manager.".into());
    }
    #[cfg(target_os = "linux")]
    for binary in ["/usr/bin/pkexec", "/usr/bin/dpkg"] {
        if !Path::new(binary).is_file() {
            return Err(format!(
                "Installing UXDuck needs {binary}. Install it with your package manager, or install the downloaded .deb manually."
            ));
        }
    }
    Ok(())
}

/// Mac updates run as the user; native Windows/Linux installers own elevation.
/// Never reboot a computer.
pub(super) fn install(
    package: &Path,
    kind: &str,
    version: &str,
    installed: &Path,
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    if kind == "pkg" {
        return macos::install(package, Path::new(INSTALLED_APP), version);
    }
    let _ = (version, installed);
    let (program, args) = command(std::env::consts::OS, kind)?;
    let mut command = Command::new(program);
    command.args(args).arg(package);
    #[cfg(windows)]
    command.args(windows::update_properties(installed)?);
    let status = command
        // The worker's stdout is a one-line readiness pipe whose reader exits
        // before installation. Installer output must not hit that closed pipe.
        .stdout(Stdio::null())
        .stdin(Stdio::null())
        .status()
        .map_err(|e| format!("Couldn't start the installer: {e}"))?;
    // Windows Installer's 3010 is success with a reboot requested. /norestart
    // prevents that request from restarting the machine under the user.
    if status.success() || (cfg!(windows) && status.code() == Some(3010)) {
        Ok(())
    } else {
        Err(format!("The installer was cancelled or failed ({status})."))
    }
}

/// Before shutdown, the same checks a per-file update (S164) gets after it:
/// the new bundle built from the installed app and the downloaded files,
/// matching the signed list exactly, then signature, Gatekeeper and version.
#[cfg(unix)]
pub(super) fn preflight_files(
    dir: &Path,
    list: &uxduck_contracts::dto::release::BundleFiles,
    installed: &Path,
    version: &str,
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        macos::preflight_files(dir, list, installed, version)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (dir, list, installed, version);
        Err("Per-file updates are only for the Mac app.".into())
    }
}

/// Installs a per-file update (S164) by replacing the whole app, as a
/// package update does.
#[cfg(unix)]
pub(super) fn install_files(
    dir: &Path,
    list: &uxduck_contracts::dto::release::BundleFiles,
    version: &str,
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        macos::install_files(dir, list, Path::new(INSTALLED_APP), version)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (dir, list, version);
        Err("Per-file updates are only for the Mac app.".into())
    }
}

fn command(os: &str, kind: &str) -> Result<(&'static str, Vec<&'static str>), String> {
    match (os, kind) {
        ("windows", "msi") => Ok(("msiexec.exe", vec!["/passive", "/norestart", "/i"])),
        ("linux", "deb") => Ok(("/usr/bin/pkexec", vec!["/usr/bin/dpkg", "--install"])),
        _ => Err(
            "This platform needs a different installer. Download UXDuck for this computer.".into(),
        ),
    }
}

/// Keep a Windows personal installation in its user-owned location. Otherwise
/// use the full installer's standard shared location, including for portable
/// archives. On failure the old path is retained for recovery.
pub(super) fn installed_executable(current: &Path) -> Result<PathBuf, String> {
    let _ = current;
    #[cfg(windows)]
    {
        windows::installed_executable(current)
    }
    #[cfg(not(windows))]
    match std::env::consts::OS {
        "macos" => Ok(PathBuf::from(
            "/Applications/UXDuck.app/Contents/MacOS/uxduck",
        )),
        "linux" => Ok(PathBuf::from("/usr/bin/uxduck")),
        _ => Err("No installer is supported on this platform.".into()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    #[test]
    fn each_platform_uses_its_native_installer_and_never_reboots() {
        // macOS never falls back to an elevated shell or the system installer.
        assert!(command("macos", "pkg").is_err());
        assert_eq!(
            command("windows", "msi").unwrap(),
            ("msiexec.exe", vec!["/passive", "/norestart", "/i"])
        );
        assert_eq!(
            command("linux", "deb").unwrap(),
            ("/usr/bin/pkexec", vec!["/usr/bin/dpkg", "--install"])
        );
        assert!(command("linux", "tar.gz").is_err());
        assert!(command("windows", "pkg").is_err());
    }
}
