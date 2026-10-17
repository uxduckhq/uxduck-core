//! Where the daemon keeps its files, and how clients find it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The data directory: `--data-dir`, then `UXDUCK_DATA_DIR`, then the
/// platform's local data directory: `UXDuck` for a release, and
/// `UXDuck-dev` beside it for `--dev` and for every development build, so
/// development never touches the real configuration and a release on the
/// same computer starts as a new user's does (maintainer, 2026-10-16).
#[must_use]
pub fn data_dir(explicit: Option<&Path>, dev: bool) -> PathBuf {
    if let Some(dir) = explicit {
        return dir.to_path_buf();
    }
    if let Some(dir) = std::env::var_os("UXDUCK_DATA_DIR") {
        return PathBuf::from(dir);
    }
    if dev || cfg!(debug_assertions) {
        dev_data_dir()
    } else {
        platform_data_dir()
    }
}

/// The development data directory. Before 2026-10-16 it was `dev` inside
/// the release's; that one is moved here once no daemon runs from it, and
/// used where it is until then.
fn dev_data_dir() -> PathBuf {
    let old = platform_data_dir().join("dev");
    let new = platform_dev_data_dir();
    move_old_dev_dir(&old, &new).unwrap_or(new)
}

/// Moves `old` to `new` when `new` is not there yet. Gives `old` while a
/// daemon holds its lock, so that daemon and its clients keep finding each
/// other; nothing when there is nothing to move or it moved.
fn move_old_dev_dir(old: &Path, new: &Path) -> Option<PathBuf> {
    if new.exists() || !old.is_dir() {
        return None;
    }
    let in_use =
        std::fs::File::open(old.join("daemon.lock")).is_ok_and(|lock| lock.try_lock().is_err());
    if in_use {
        return Some(old.to_owned());
    }
    if let Some(parent) = new.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::rename(old, new) {
        Ok(()) => {
            tracing::info!(from = %old.display(), to = %new.display(), "development data moved");
            None
        }
        Err(error) => {
            tracing::warn!(%error, from = %old.display(), "development data could not move");
            Some(old.to_owned())
        }
    }
}

/// How a terminal reaches a development daemon: `uxduck` on the path is the
/// installed release with its own data directory, so this names the
/// daemon's own executable and directory instead.
#[derive(Clone, Debug)]
pub struct DevCli {
    exe: String,
    dir: String,
}

impl DevCli {
    #[must_use]
    pub fn new(data_dir: &Path) -> Self {
        let exe = std::env::current_exe()
            .map_or_else(|_| "uxduck".into(), |p| shell_word(&p.to_string_lossy()));
        let dir = if data_dir == dev_data_dir() {
            "--dev".into()
        } else {
            format!("--data-dir {}", shell_word(&data_dir.to_string_lossy()))
        };
        Self { exe, dir }
    }

    /// `uxduck rpc call <method> --json '<params>'` for this daemon.
    #[must_use]
    pub fn rpc_call(&self, method: &str, params: &serde_json::Value) -> String {
        let json = shell_word(&params.to_string());
        format!("{} rpc call {method} --json {json} {}", self.exe, self.dir)
    }
}

/// Quotes `text` for a POSIX shell when it needs it.
fn shell_word(text: &str) -> String {
    if !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@,=".contains(c))
    {
        return text.to_owned();
    }
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The platform's local data directory for UXDuck.
fn platform_data_dir() -> PathBuf {
    project_dir("UXDuck", "uxduck")
}

/// Its development twin: `com.uxduck.UXDuck-dev` beside it on a Mac.
fn platform_dev_data_dir() -> PathBuf {
    project_dir("UXDuck-dev", "uxduck-dev")
}

fn project_dir(application: &str, temporary: &str) -> PathBuf {
    directories::ProjectDirs::from("com", "uxduck", application).map_or_else(
        || std::env::temp_dir().join(temporary),
        |dirs| dirs.data_local_dir().to_path_buf(),
    )
}

/// Where camera recordings are kept (S40; moved 2026-10-05): they are the
/// person's footage, so the platform's videos folder, `~/Movies/UXDuck` on a
/// Mac, and `UXDuck (dev)` beside it for `--dev`. A data directory given
/// some other way (tests, a second daemon) keeps them inside it, as does a
/// platform with no videos folder.
#[must_use]
pub fn recordings_dir(data_dir: &Path) -> PathBuf {
    let name = if data_dir == platform_data_dir() {
        "UXDuck"
    } else if data_dir == dev_data_dir() {
        "UXDuck (dev)"
    } else {
        return data_dir.join("video");
    };
    directories::UserDirs::new()
        .and_then(|dirs| dirs.video_dir().map(|d| d.join(name)))
        .unwrap_or_else(|| data_dir.join("video"))
}

/// Moves recordings kept in the data directory before 2026-10-05 to
/// [`recordings_dir`], and gives that directory. A file already there is
/// left where it was.
pub fn move_recordings(data_dir: &Path) -> PathBuf {
    let to = recordings_dir(data_dir);
    move_files(&data_dir.join("video"), &to);
    to
}

fn move_files(from: &Path, to: &Path) {
    if from == to || !from.is_dir() {
        return;
    }
    if let Err(error) = std::fs::create_dir_all(to) {
        tracing::warn!(%error, to = %to.display(), "recordings could not move");
        return;
    }
    let mut moved = 0usize;
    for entry in std::fs::read_dir(from).into_iter().flatten().flatten() {
        let target = to.join(entry.file_name());
        if target.exists() {
            continue;
        }
        // Another volume cannot rename: copy, then remove.
        let done = std::fs::rename(entry.path(), &target).is_ok()
            || (std::fs::copy(entry.path(), &target).is_ok()
                && std::fs::remove_file(entry.path()).is_ok());
        if done {
            moved += 1;
        } else {
            tracing::warn!(file = %entry.path().display(), "a recording could not move");
        }
    }
    let _ = std::fs::remove_dir(from);
    if moved > 0 {
        tracing::info!(moved, to = %to.display(), "recordings moved");
    }
}

/// Unix socket paths are limited to about 104 bytes (`sun_path`).
const MAX_SOCKET_PATH: usize = 100;

/// The Unix socket: inside the data directory, or, when that path is too
/// long for a socket, a name in the temp directory derived from the data
/// directory, so the CLI finds the same one. The socket is 0600 and peers are
/// checked by uid either way.
#[must_use]
#[cfg_attr(windows, allow(dead_code, reason = "Windows uses a named pipe"))]
pub fn socket_path(data_dir: &Path) -> PathBuf {
    let path = data_dir.join("uxduck.sock");
    if path.as_os_str().len() <= MAX_SOCKET_PATH {
        return path;
    }
    std::env::temp_dir().join(format!("uxduck-{:016x}.sock", hash(data_dir)))
}

/// The Windows named pipe, derived from the data directory so a dev daemon
/// and a normal one do not collide.
#[must_use]
#[cfg_attr(
    not(windows),
    allow(dead_code, reason = "the named pipe exists on Windows only")
)]
pub fn pipe_name(data_dir: &Path) -> String {
    format!(r"\\.\pipe\uxduck-{:016x}", hash(data_dir))
}

/// FNV-1a: stable across runs and platforms, which `DefaultHasher` is not.
fn hash(path: &Path) -> u64 {
    path.to_string_lossy()
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        })
}

/// Written once the listeners are up; removed on a clean shutdown.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonInfo {
    pub pid: u32,
    pub version: String,
    pub port: u16,
    pub socket: String,
    pub ui_url: String,
    pub dev: bool,
    /// When this daemon started (µs since the Unix epoch): a restart keeps
    /// the pid on macOS and Linux, so this is how a new one is told apart.
    #[serde(default)]
    pub started_us: u64,
}

#[must_use]
pub fn info_path(data_dir: &Path) -> PathBuf {
    data_dir.join("daemon.json")
}

/// The running daemon's `daemon.json`, if there is one.
#[must_use]
pub fn read_info(data_dir: &Path) -> Option<DaemonInfo> {
    serde_json::from_slice(&std::fs::read(info_path(data_dir)).ok()?).ok()
}

/// Writes `daemon.json` atomically.
///
/// # Errors
/// When the data directory is not writable.
pub fn write_info(data_dir: &Path, info: &DaemonInfo) -> std::io::Result<()> {
    let temp = data_dir.join("daemon.json.tmp");
    std::fs::write(&temp, serde_json::to_vec_pretty(info).unwrap_or_default())?;
    std::fs::rename(temp, info_path(data_dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_dir_wins_and_dev_is_separate() {
        let explicit = Path::new("/tmp/x");
        assert_eq!(data_dir(Some(explicit), true), explicit);
        assert!(pipe_name(Path::new("/a")).starts_with(r"\\.\pipe\uxduck-"));
        assert_ne!(pipe_name(Path::new("/a")), pipe_name(Path::new("/a/dev")));
    }

    #[test]
    fn a_long_data_dir_gets_a_short_stable_socket() {
        let short = Path::new("/tmp/u");
        assert_eq!(socket_path(short), short.join("uxduck.sock"));
        let long = PathBuf::from(format!("/tmp/{}", "d".repeat(120)));
        let socket = socket_path(&long);
        assert!(
            socket.as_os_str().len() <= MAX_SOCKET_PATH + 40,
            "{}",
            socket.display()
        );
        assert_eq!(socket, socket_path(&long));
        assert_ne!(socket, socket_path(&long.join("dev")));
    }

    #[test]
    fn a_dev_command_names_this_daemon_and_quotes_for_a_shell() {
        assert_eq!(
            shell_word("/a/target/debug/uxduck"),
            "/a/target/debug/uxduck"
        );
        assert_eq!(shell_word("/My Data/it's"), r"'/My Data/it'\''s'");
        let dev = DevCli::new(&dev_data_dir());
        let command = dev.rpc_call("pairing/approve", &serde_json::json!({ "code": "123456" }));
        assert!(
            command.ends_with(r#" rpc call pairing/approve --json '{"code":"123456"}' --dev"#),
            "{command}"
        );
        let other = DevCli::new(Path::new("/tmp/snap shot"));
        assert!(
            other
                .rpc_call("pairing/list", &serde_json::json!({}))
                .ends_with(" --data-dir '/tmp/snap shot'")
        );
    }
}

#[cfg(test)]
mod dev_dir_tests {
    use super::*;

    #[test]
    fn development_has_a_folder_of_its_own_beside_the_releases() {
        let (release, dev) = (platform_data_dir(), platform_dev_data_dir());
        assert_ne!(release, dev);
        assert!(!dev.starts_with(&release), "{}", dev.display());
        assert!(dev.to_string_lossy().to_lowercase().contains("uxduck-dev"));
    }

    #[test]
    fn the_old_dev_folder_moves_once_and_stays_while_a_daemon_runs_from_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (old, new) = (tmp.path().join("UXDuck/dev"), tmp.path().join("UXDuck-dev"));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("uxduck.db"), b"db").unwrap();
        let lock = std::fs::File::create(old.join("daemon.lock")).unwrap();
        lock.lock().unwrap();
        assert_eq!(move_old_dev_dir(&old, &new), Some(old.clone()));
        assert!(!new.exists());
        drop(lock);
        assert_eq!(move_old_dev_dir(&old, &new), None);
        assert_eq!(std::fs::read(new.join("uxduck.db")).unwrap(), b"db");
        assert!(!old.exists());
        std::fs::create_dir_all(&old).unwrap();
        assert_eq!(move_old_dev_dir(&old, &new), None, "never over the new one");
        assert!(old.exists());
    }
}

#[cfg(test)]
mod recordings_tests {
    use super::*;

    #[test]
    fn a_data_directory_given_by_hand_keeps_its_recordings() {
        let dir = Path::new("/tmp/some-uxduck");
        assert_eq!(recordings_dir(dir), dir.join("video"));
    }

    #[test]
    fn recordings_move_and_one_already_there_stays() {
        let tmp = tempfile::tempdir().unwrap();
        let (from, to) = (tmp.path().join("video"), tmp.path().join("Movies/UXDuck"));
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(from.join("vr1.mp4"), b"one").unwrap();
        std::fs::write(from.join("vr2.mp4"), b"old").unwrap();
        std::fs::write(to.join("vr2.mp4"), b"new").unwrap();
        move_files(&from, &to);
        assert_eq!(std::fs::read(to.join("vr1.mp4")).unwrap(), b"one");
        assert_eq!(std::fs::read(to.join("vr2.mp4")).unwrap(), b"new");
        assert!(from.join("vr2.mp4").exists(), "not overwritten, so kept");
        assert!(!from.join("vr1.mp4").exists());
    }
}
