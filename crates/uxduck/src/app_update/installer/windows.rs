//! Preserve the original installation's scope, before copying the updater out
//! of it. Portable archives continue to install to the shared default location.
use std::path::{Path, PathBuf};

const USER_PROPERTIES: [&str; 2] = ["ALLUSERS=2", "MSIINSTALLPERUSER=1"];
const MACHINE_PROPERTIES: [&str; 1] = ["ALLUSERS=1"];

#[cfg(windows)]
fn folders() -> Result<(PathBuf, PathBuf), String> {
    let folder = |name| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .ok_or_else(|| format!("Windows did not provide {name}. Install UXDuck manually."))
    };
    Ok((folder("ProgramFiles")?, folder("LOCALAPPDATA")?))
}

#[cfg(windows)]
pub(super) fn installed_executable(current: &Path) -> Result<PathBuf, String> {
    let (machine, local) = folders()?;
    Ok(destination(current, &machine, &local))
}

#[cfg(windows)]
pub(super) fn update_properties(installed: &Path) -> Result<&'static [&'static str], String> {
    let (_, local) = folders()?;
    Ok(properties(installed, &local))
}

fn user_executable(local: &Path) -> PathBuf {
    local.join("Programs").join("UXDuck").join("uxduck.exe")
}

// canonicalize(current_exe) can add the extended-path prefix. MSI paths are
// case insensitive, including on volumes whose spelling differs from the env.
fn same_path(a: &Path, b: &Path) -> bool {
    let normalize = |path: &Path| {
        path.to_string_lossy()
            .replace('/', "\\")
            .trim_start_matches("\\\\?\\")
            .to_lowercase()
    };
    normalize(a) == normalize(b)
}

fn destination(current: &Path, machine: &Path, local: &Path) -> PathBuf {
    let user = user_executable(local);
    if same_path(current, &user) {
        user
    } else {
        machine.join("UXDuck").join("uxduck.exe")
    }
}

fn properties(installed: &Path, local: &Path) -> &'static [&'static str] {
    if same_path(installed, &user_executable(local)) {
        &USER_PROPERTIES
    } else {
        &MACHINE_PROPERTIES
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn personal_update_keeps_its_destination_and_never_requests_machine_scope() {
        let local = Path::new("C:/Users/Demo/AppData/Local");
        let machine = Path::new("C:/Program Files");
        let current = Path::new(r"\\?\C:\Users\DEMO\AppData\Local\Programs\UXDuck\uxduck.exe");
        let installed = destination(current, machine, local);
        assert_eq!(installed, user_executable(local));
        assert_eq!(properties(&installed, local), USER_PROPERTIES);
    }

    #[test]
    fn shared_and_portable_updates_keep_the_shared_default() {
        let local = Path::new("C:/Users/Demo/AppData/Local");
        let machine = Path::new("D:/Program Files");
        for current in [
            Path::new("D:/Program Files/UXDuck/uxduck.exe"),
            Path::new("C:/Downloads/UXDuck/uxduck.exe"),
            Path::new("C:/Users/Other/AppData/Local/Programs/UXDuck/uxduck.exe"),
        ] {
            let installed = destination(current, machine, local);
            assert_eq!(installed, machine.join("UXDuck").join("uxduck.exe"));
            assert_eq!(properties(&installed, local), MACHINE_PROPERTIES);
        }
    }

    #[test]
    fn similar_names_and_nested_copies_do_not_select_a_personal_installation() {
        let local = Path::new("C:/Users/Demo/AppData/Local");
        for current in [
            Path::new("C:/Users/Demo/AppData/Local/Programs/UXDuck-old/uxduck.exe"),
            Path::new("C:/Users/Demo/AppData/Local/Programs/UXDuck/copy/uxduck.exe"),
        ] {
            assert_eq!(properties(current, local), MACHINE_PROPERTIES);
        }
    }
}
