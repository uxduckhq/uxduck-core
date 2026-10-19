//! User-owned, complete app replacement. Packages remain the signed download
//! format, but their scripts are never executed during an app update.
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

use rustix::fs::{CWD, RenameFlags, renameat_with};
use uxduck_contracts::dto::release::BundleFiles;

const REQUIREMENT: &str = "=identifier \"com.uxduck.app\" and anchor apple generic and certificate leaf[subject.OU] = \"N938V577RX\"";

pub(super) fn check_destination(app: &Path) -> Result<(), String> {
    for helper in [
        "/usr/sbin/pkgutil",
        "/usr/bin/codesign",
        "/usr/sbin/spctl",
        "/usr/bin/plutil",
    ] {
        if !Path::new(helper).is_file() {
            return Err(format!(
                "The Mac update helper {helper} is missing. Repair macOS or install UXDuck manually."
            ));
        }
    }
    let metadata = std::fs::symlink_metadata(app).map_err(|e| e.to_string())?;
    if !metadata.is_dir() || metadata.uid() != rustix::process::getuid().as_raw() {
        return Err("Install the latest UXDuck download once to enable updates without a password for this account. Managed installations must be updated by their administrator.".into());
    }
    // Creating a private staging folder proves permission before the daemon
    // stops. Never silently fall back to an administrator/password dialog.
    staging(app)?;
    Ok(())
}

fn staging(app: &Path) -> Result<tempfile::TempDir, String> {
    tempfile::Builder::new()
        .prefix(".UXDuck-update-")
        .tempdir_in(app.parent().ok_or("UXDuck has no installation folder")?)
        .map_err(|e| format!("UXDuck cannot update this installation: {e}. Install the download manually or ask the administrator of this Mac."))
}

fn run(command: &mut Command, action: &str) -> Result<(), String> {
    let output = command.output().map_err(|e| format!("{action}: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{action}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn validate(app: &Path, version: &str) -> Result<(), String> {
    for file in [
        "Contents/Info.plist",
        "Contents/MacOS/uxduck",
        "Contents/MacOS/uxduck-tray",
        "Contents/MacOS/capture/ffmpeg",
        "Contents/Resources/desktop-app/installed.json",
        "Contents/Resources/desktop-app/UXDuck.app/Contents/MacOS/UXDuck",
        "Contents/Library/LaunchAgents/com.uxduck.app.agent.plist",
    ] {
        if !app.join(file).is_file() {
            return Err(format!(
                "The update is missing {file}. UXDuck has kept its previous version."
            ));
        }
    }
    run(
        Command::new("/usr/bin/codesign")
            .args([
                "--verify",
                "--deep",
                "--strict",
                "--test-requirement",
                REQUIREMENT,
            ])
            .arg(app),
        "The updated app's UXDuck signature is invalid",
    )?;
    run(
        Command::new("/usr/sbin/spctl")
            .args(["--assess", "--type", "execute"])
            .arg(app),
        "macOS did not accept the updated app",
    )?;
    let info = Command::new("/usr/bin/plutil")
        .args(["-extract", "CFBundleShortVersionString", "raw", "-o", "-"])
        .arg(app.join("Contents/Info.plist"))
        .output()
        .map_err(|e| e.to_string())?;
    if !info.status.success() || String::from_utf8_lossy(&info.stdout).trim() != version {
        return Err("The app bundle does not match the approved version.".into());
    }
    check_version(app, version)
}

fn check_version(app: &Path, version: &str) -> Result<(), String> {
    let output = Command::new(app.join("Contents/MacOS/uxduck"))
        .arg("--version")
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success()
        || String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .nth(1)
            != Some(version)
    {
        return Err("The updated executable does not match the approved version.".into());
    }
    Ok(())
}

/// `RENAME_SWAP` makes either the complete old or complete new bundle visible,
/// including if the process dies at the commit point. Both paths share a volume.
fn exchange(a: &Path, b: &Path) -> Result<(), String> {
    renameat_with(CWD, a, CWD, b, RenameFlags::EXCHANGE)
        .map_err(|e| format!("Could not replace UXDuck atomically: {e}"))
}

fn replace(
    candidate: &Path,
    installed: &Path,
    validate_installed: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    exchange(candidate, installed)?;
    if let Err(error) = validate_installed() {
        exchange(candidate, installed)
            .map_err(|rollback| format!("{error}. Rollback failed: {rollback}"))?;
        return Err(format!("{error}. The previous UXDuck app was restored."));
    }
    Ok(())
}

pub(super) fn install(package: &Path, app: &Path, version: &str) -> Result<(), String> {
    check_destination(app)?;
    let stage = staging(app)?;
    let expanded = stage.path().join("expanded");
    run(
        Command::new("/usr/sbin/pkgutil")
            .arg("--expand-full")
            .arg(package)
            .arg(&expanded),
        "Could not unpack the app update",
    )?;
    // This is our versioned packaging contract, not a recursive search for any
    // .app an arbitrary package might contain. No scripts or global files run.
    let candidate = expanded.join("uxduck.pkg/Payload/Applications/UXDuck.app");
    commit(stage, &candidate, app, version)
}

/// The new bundle of a per-file update (S164), built in a staging folder
/// beside the app with exactly the signed list's files. Unchanged files are
/// clones of the installed ones. It is then checked as a package's app is:
/// UXDuck's signature, Gatekeeper and the version.
fn candidate(
    dir: &Path,
    list: &BundleFiles,
    app: &Path,
    stage: &tempfile::TempDir,
) -> Result<std::path::PathBuf, String> {
    let candidate = stage.path().join("UXDuck.app");
    crate::app_update::files::build(dir, list, app, &candidate)?;
    Ok(candidate)
}

/// Builds and checks a per-file update's bundle before shutdown, then
/// removes it; the worker builds it again once UXDuck has stopped.
pub(super) fn preflight_files(
    dir: &Path,
    list: &BundleFiles,
    app: &Path,
    version: &str,
) -> Result<(), String> {
    check_destination(app)?;
    let stage = staging(app)?;
    validate(&candidate(dir, list, app, &stage)?, version)?;
    stage.close().map_err(|e| e.to_string())
}

pub(super) fn install_files(
    dir: &Path,
    list: &BundleFiles,
    app: &Path,
    version: &str,
) -> Result<(), String> {
    check_destination(app)?;
    let stage = staging(app)?;
    let candidate = candidate(dir, list, app, &stage)?;
    commit(stage, &candidate, app, version)
}

/// Swaps a checked candidate in, keeping the old app until the installed
/// one passes too.
fn commit(
    stage: tempfile::TempDir,
    candidate: &Path,
    app: &Path,
    version: &str,
) -> Result<(), String> {
    validate(candidate, version)?;
    if let Err(error) = replace(candidate, app, || validate(app, version)) {
        // In particular, never delete the old bundle after a failed rollback.
        let preserved = stage.keep();
        return Err(format!("{error}. Recovery files: {}", preserved.display()));
    }
    // Keep the old bundle if cleanup fails; an installed update is still success.
    if let Err(error) = stage.close() {
        eprintln!("UXDuck updated; could not remove the old staging folder: {error}");
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "temporary bundle fixtures")]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn bundle(root: &Path, name: &str, content: &str) -> std::path::PathBuf {
        let app = root.join(name);
        std::fs::create_dir(&app).unwrap();
        std::fs::write(app.join("version"), content).unwrap();
        app
    }

    #[test]
    fn replacement_preserves_complete_old_bundle_until_validation_passes() {
        let root = tempfile::tempdir().unwrap();
        let old = bundle(root.path(), "installed.app", "old");
        let new = bundle(root.path(), "candidate.app", "new");
        replace(&new, &old, || {
            assert_eq!(std::fs::read_to_string(old.join("version")).unwrap(), "new");
            assert_eq!(std::fs::read_to_string(new.join("version")).unwrap(), "old");
            Ok(())
        })
        .unwrap();
        assert_eq!(std::fs::read_to_string(old.join("version")).unwrap(), "new");
    }

    #[test]
    fn failed_validation_rolls_back_without_a_missing_app() {
        let root = tempfile::tempdir().unwrap();
        let old = bundle(root.path(), "installed.app", "old");
        let new = bundle(root.path(), "candidate.app", "broken");
        assert!(
            replace(&new, &old, || Err("bad new executable".into()))
                .unwrap_err()
                .contains("restored")
        );
        assert_eq!(std::fs::read_to_string(old.join("version")).unwrap(), "old");
    }

    #[test]
    fn missing_source_and_symlink_destinations_fail_without_changing_the_app() {
        let root = tempfile::tempdir().unwrap();
        let old = bundle(root.path(), "installed.app", "old");
        assert!(replace(&root.path().join("missing"), &old, || Ok(())).is_err());
        assert_eq!(std::fs::read_to_string(old.join("version")).unwrap(), "old");
        let link = root.path().join("alias.app");
        std::os::unix::fs::symlink(&old, &link).unwrap();
        assert!(check_destination(&link).is_err());
    }

    #[test]
    fn permission_failure_is_detected_before_handoff() {
        if rustix::process::getuid().is_root() {
            return; // root deliberately bypasses filesystem write permissions.
        }
        let root = tempfile::tempdir().unwrap();
        let app = bundle(root.path(), "UXDuck.app", "old");
        assert!(check_destination(&app).is_ok());
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = check_destination(&app);
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            result
                .unwrap_err()
                .contains("Install the download manually")
        );
        assert_eq!(std::fs::read_to_string(app.join("version")).unwrap(), "old");
    }
}
