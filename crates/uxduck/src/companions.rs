//! Companion computers over SSH (S296, ADR-0049 O8): the engine's
//! [`CompanionPort`]. A check runs the computer's own OpenSSH `ssh`, so the
//! person's `~/.ssh/config` and ssh-agent sign in as they do in a terminal;
//! a private key kept in the credential store is used only for a computer
//! that has one. Each computer's host key is pinned in its own known-hosts
//! file under `<data dir>/companions/` on the first check, and a changed one
//! is refused with both fingerprints.
//!
//! UXDuck has a key of its own on each computer it runs on (ed25519, made
//! on first use, kept in the credential store). A check with a password
//! uses it once, through `SSH_ASKPASS` pointing back at this binary, to add
//! that key to the companion's `authorized_keys`; the password is never
//! kept, and later checks sign in with the key.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use uxduck_core::domain::companion::CompanionComputer;
use uxduck_engine::companions::{CheckAnswer, CompanionOrder, CompanionPort, Found};
use uxduck_engine::runtime::Credentials as _;

use crate::adapters::credentials::FileCredentials;
use crate::daemon::engine_task::EngineHandle;

/// How long a check may take, signing in included.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// The environment variable naming the file `ssh`'s password prompt reads.
pub const ASKPASS_FILE: &str = "UXDUCK_ASKPASS_FILE";

/// The credential holding this computer's own UXDuck key.
const OWN_KEY: &str = "companion-own-key";

/// `ssh` asking for a password: print it from the check's file.
pub fn askpass(path: &Path) -> std::process::ExitCode {
    match std::fs::read_to_string(path) {
        Ok(password) => {
            println!("{password}");
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("uxduck askpass: {error}");
            std::process::ExitCode::from(1)
        }
    }
}

/// This computer's own UXDuck key, its private key and its public line.
#[derive(Clone)]
pub(crate) struct OwnKey {
    pub private: String,
    pub public: String,
}

/// How a check signs in, besides the person's SSH config and agent.
#[derive(Clone, Copy, Default)]
pub(crate) struct SignIn<'a> {
    /// A private key the person gave for this computer: it alone is offered.
    pub pasted: Option<&'a str>,
    /// UXDuck's own key, offered too.
    pub own: Option<&'a OwnKey>,
    /// Its password, for this check alone, to add `own` to it.
    pub password: Option<&'a str>,
}

/// UXDuck's own key from the credential store, made on first use.
fn own_key(credentials: &mut FileCredentials) -> Result<OwnKey, String> {
    let private = if let Some(private) = credentials.get(OWN_KEY) {
        private
    } else {
        let dir = tempfile::tempdir().map_err(|e| format!("Cannot make UXDuck's SSH key: {e}"))?;
        let path = dir.path().join("key");
        let status = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "uxduck", "-f"])
            .arg(&path)
            .status()
            .map_err(|e| format!("Cannot run ssh-keygen to make UXDuck's SSH key: {e}"))?;
        if !status.success() {
            return Err("ssh-keygen could not make UXDuck's SSH key".to_owned());
        }
        let private = std::fs::read_to_string(&path)
            .map_err(|e| format!("Cannot read UXDuck's new SSH key: {e}"))?;
        credentials.set(OWN_KEY, &private)?;
        private
    };
    let dir = tempfile::tempdir().map_err(|e| format!("Cannot read UXDuck's SSH key: {e}"))?;
    let path = dir.path().join("key");
    write_private(&path, &private)?;
    let output = std::process::Command::new("ssh-keygen")
        .arg("-y")
        .arg("-f")
        .arg(&path)
        .output()
        .map_err(|e| format!("Cannot run ssh-keygen to read UXDuck's SSH key: {e}"))?;
    let public = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    if !output.status.success() || public.is_empty() {
        return Err("UXDuck's SSH key in the credential store cannot be read".to_owned());
    }
    Ok(OwnKey {
        private,
        public: format!("{public} uxduck"),
    })
}

/// What the computer is asked: its system, architecture and UXDuck.
const SCRIPT: &str = r#"if [ -r /etc/os-release ]; then . /etc/os-release; fi; echo "os=${PRETTY_NAME:-$(uname -sr)}"; echo "arch=$(uname -m)"; v=$(uxduck --version 2>/dev/null || "$HOME/.local/bin/uxduck" --version 2>/dev/null); echo "uxduck=$v""#;

pub struct Host {
    dir: PathBuf,
    data_dir: PathBuf,
    engine: EngineHandle,
}

impl Host {
    #[must_use]
    pub fn new(data_dir: &Path, engine: EngineHandle) -> Self {
        Self {
            dir: data_dir.join("companions"),
            data_dir: data_dir.to_owned(),
            engine,
        }
    }
}

impl CompanionPort for Host {
    fn request(&mut self, order: CompanionOrder) {
        match order {
            CompanionOrder::Check {
                vehicle_id,
                computer,
                password,
            } => {
                let dir = self.dir.clone();
                let mut credentials = FileCredentials::new(&self.data_dir);
                let key = computer.key.then(|| credentials.get(&computer.key_name()));
                let own = own_key(&mut credentials);
                let engine = self.engine.clone();
                tokio::spawn(async move {
                    let result = match (key, own) {
                        (Some(None), _) => Err(
                            "Its private key is missing from this computer's credential store: remove the computer and add it again"
                                .to_owned(),
                        ),
                        (_, Err(why)) => Err(why),
                        (key, Ok(own)) => {
                            let sign_in = SignIn {
                                pasted: key.as_ref().and_then(Option::as_deref),
                                own: Some(&own),
                                password: password.as_ref().map(|p| p.0.as_str()),
                            };
                            check(&dir, None, &computer, sign_in).await
                        }
                    };
                    let answer = CheckAnswer {
                        vehicle_id,
                        computer_id: computer.id,
                        result,
                    };
                    let _ = engine.run(move |e| e.companion_checked(answer)).await;
                });
            }
            CompanionOrder::Network {
                vehicle_id,
                computer,
                input,
            } => {
                let dir = self.dir.clone();
                let mut credentials = FileCredentials::new(&self.data_dir);
                let key = computer.key.then(|| credentials.get(&computer.key_name()));
                let own = own_key(&mut credentials);
                let engine = self.engine.clone();
                tokio::spawn(async move {
                    let result = match (key, own) {
                        (Some(None), _) => Err("Its saved SSH private key is missing.".to_owned()),
                        (_, Err(why)) => Err(why),
                        (key, Ok(own)) => {
                            network(
                                &dir,
                                &computer,
                                SignIn {
                                    pasted: key.as_ref().and_then(Option::as_deref),
                                    own: Some(&own),
                                    password: None,
                                },
                                &input.0,
                            )
                            .await
                        }
                    };
                    let answer = CheckAnswer {
                        vehicle_id,
                        computer_id: computer.id,
                        result,
                    };
                    let _ = engine.run(move |e| e.companion_checked(answer)).await;
                });
            }
            CompanionOrder::Forget { computer_id } => {
                let path = self.dir.join(format!("{computer_id}.known_hosts"));
                if let Err(error) = std::fs::remove_file(&path)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!(%error, path = %path.display(), "cannot forget a companion's host key");
                }
            }
        }
    }
}

/// One way a check tries to sign in.
#[derive(Clone, Copy)]
enum Way<'a> {
    /// The key the person gave for this computer, alone.
    Pasted(&'a str),
    /// The person's SSH config and agent, as in their terminal.
    Person,
    /// UXDuck's own key, alone.
    Own(&'a OwnKey),
    /// The password, with UXDuck's own key added once signed in.
    Password(&'a OwnKey, &'a str),
}

/// Signs in to `computer` and reads what it runs: with the key the person
/// gave it, else their own SSH config and agent and then UXDuck's own key,
/// each alone so a server's limit on tries is not spent on keys it will
/// refuse. `config` replaces the person's `~/.ssh/config`, for tests.
pub(crate) async fn check(
    dir: &Path,
    config: Option<&Path>,
    computer: &CompanionComputer,
    sign_in: SignIn<'_>,
) -> Result<Found, String> {
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("Cannot keep host keys in {}: {e}", dir.display()))?;
    let known_hosts = dir.join(format!("{}.known_hosts", computer.id));
    let ways = match sign_in {
        SignIn {
            pasted: Some(key), ..
        } => vec![Way::Pasted(key)],
        SignIn {
            own: Some(own),
            password: Some(password),
            ..
        } => vec![Way::Password(own, password)],
        SignIn { own: Some(own), .. } => vec![Way::Person, Way::Own(own)],
        _ => vec![Way::Person],
    };
    let mut output = None;
    for way in ways {
        let tried = run(dir, config, computer, &known_hosts, way, None).await?;
        let again = !tried.status.success() && refused(&String::from_utf8_lossy(&tried.stderr));
        output = Some(tried);
        if !again {
            break;
        }
    }
    let Some(output) = output else {
        return Err("No way to sign in".to_owned());
    };
    answer(computer, sign_in, &output)
}

/// Whether `ssh` was refused the sign-in, rather than not reaching it.
fn refused(stderr: &str) -> bool {
    stderr.contains("Permission denied") || stderr.contains("Too many authentication failures")
}

/// Reach networking only through the exact previously verified raw host key.
/// Keys in either UXDuck's or the person's `known_hosts` are accepted only when
/// their fingerprint matches the pin. No keyscan or trust-on-first-use here.
async fn network(
    dir: &Path,
    computer: &CompanionComputer,
    sign_in: SignIn<'_>,
    input: &str,
) -> Result<Found, String> {
    let pinned = computer
        .check
        .as_ref()
        .and_then(|c| c.host_key.as_deref())
        .ok_or_else(|| "Check this computer's SSH host key first.".to_owned())?;
    let temporary =
        tempfile::tempdir().map_err(|_| "Cannot prepare SSH host verification".to_owned())?;
    let trusted = temporary.path().join("known_hosts");
    let mut files = vec![dir.join(format!("{}.known_hosts", computer.id))];
    files.extend(
        their_known_hosts(None, computer)
            .await
            .into_iter()
            .map(|path| {
                if let Some(suffix) = path.strip_prefix("~/") {
                    directories::BaseDirs::new()
                        .map_or_else(|| PathBuf::from(&path), |base| base.home_dir().join(suffix))
                } else {
                    PathBuf::from(path)
                }
            }),
    );
    let mut matched = false;
    'files: for file in files {
        let Ok(contents) = std::fs::read_to_string(file) else {
            continue;
        };
        for line in contents.lines() {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 3 || fields[0].starts_with(['@', '#']) {
                continue;
            }
            write_private(
                &trusted,
                &format!("uxduck-network {} {}", fields[1], fields[2]),
            )?;
            let output = tokio::process::Command::new("ssh-keygen")
                .args(["-l", "-E", "sha256", "-f"])
                .arg(&trusted)
                .output()
                .await
                .map_err(|_| "Cannot run ssh-keygen to verify the pinned host".to_owned())?;
            if output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .split_whitespace()
                    .nth(1)
                    == Some(pinned)
            {
                matched = true;
                break 'files;
            }
        }
    }
    if !matched {
        return Err("The pinned SSH host key is missing from known_hosts. Check the computer with its raw host key before configuring networks.".into());
    }
    let ways = if let Some(key) = sign_in.pasted {
        vec![Way::Pasted(key)]
    } else if let Some(own) = sign_in.own {
        vec![Way::Person, Way::Own(own)]
    } else {
        vec![Way::Person]
    };
    for way in ways {
        let output = run(dir, None, computer, &trusted, way, Some(input)).await?;
        if !output.status.success() && refused(&String::from_utf8_lossy(&output.stderr)) {
            continue;
        }
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
                || stderr.contains("Host key verification failed")
            {
                return Err(
                    "The companion's SSH host key no longer matches. No network changes were made."
                        .into(),
                );
            }
            return Err("Network setup failed. The companion needs Linux with NetworkManager, Python 3 and busctl. Saving needs passwordless sudo. Check its network values and service logs.".into());
        }
        return answer(computer, sign_in, &output);
    }
    Err("SSH could not sign in. Check the computer again before configuring networks.".into())
}

/// One `ssh` to the computer, signing in one `way`.
#[allow(
    clippy::too_many_lines,
    reason = "SSH options and temporary credential lifetimes stay in one scope"
)]
async fn run(
    dir: &Path,
    config: Option<&Path>,
    computer: &CompanionComputer,
    known_hosts: &Path,
    way: Way<'_>,
    network: Option<&str>,
) -> Result<std::process::Output, String> {
    let key_file = dir.join(format!("{}.key", computer.id));
    let askpass_file = dir.join(format!("{}.askpass", computer.id));
    let theirs = if network.is_none() {
        their_known_hosts(config, computer).await
    } else {
        Vec::new()
    };
    let mut script = SCRIPT.to_owned();
    let mut command = tokio::process::Command::new("ssh");
    let only = |command: &mut tokio::process::Command, key: &str| -> Result<(), String> {
        write_private(&key_file, key)?;
        command
            .arg("-i")
            .arg(&key_file)
            .args(["-o", "IdentitiesOnly=yes"]);
        Ok(())
    };
    match way {
        Way::Pasted(key) => {
            only(&mut command, key)?;
            command.args(["-o", "BatchMode=yes"]);
        }
        Way::Person => {
            command.args(["-o", "BatchMode=yes"]);
        }
        Way::Own(own) => {
            only(&mut command, &own.private)?;
            command
                .args(["-o", "IdentityAgent=none"])
                .args(["-o", "BatchMode=yes"]);
        }
        Way::Password(own, password) => {
            only(&mut command, &own.private)?;
            write_private(&askpass_file, password)?;
            let exe = std::env::current_exe()
                .map_err(|e| format!("Cannot find UXDuck to answer ssh's password prompt: {e}"))?;
            command
                .args(["-o", "IdentityAgent=none"])
                .args(["-o", "BatchMode=no"])
                .args(["-o", "NumberOfPasswordPrompts=1"])
                .env("SSH_ASKPASS", exe)
                .env("SSH_ASKPASS_REQUIRE", "force")
                .env(ASKPASS_FILE, &askpass_file);
            script.push_str("; ");
            script.push_str(&add_key(&own.public));
        }
    }
    if network.is_some() {
        let script_python = include_str!("companions/network.py").replace('\'', "'\"'\"'");
        let input: serde_json::Value = serde_json::from_str(network.unwrap_or_default())
            .map_err(|_| "Invalid network request".to_owned())?;
        let sudo = if input["wifi"].is_null() && input["connectivity"].is_null() {
            ""
        } else {
            "sudo -n "
        };
        script = format!("{SCRIPT}; {sudo}python3 -c '{script_python}'");
        // For any network operation trust only the key matching the stored pin.
        // A user known-hosts entry changed elsewhere cannot authorize a mutation.
        command.args([
            "-o",
            "HostKeyAlias=uxduck-network",
            "-o",
            "GlobalKnownHostsFile=none",
            "-o",
            "CheckHostIP=no",
        ]);
    }
    command
        .arg("-T")
        .args(["-o", "ConnectTimeout=10"])
        .args(["-o", "AddKeysToAgent=no"])
        .args([
            "-o",
            if network.is_some() {
                "StrictHostKeyChecking=yes"
            } else {
                "StrictHostKeyChecking=accept-new"
            },
        ])
        .args(["-o", "HashKnownHosts=no"])
        .arg("-v")
        .arg("-o")
        .arg(format!(
            "UserKnownHostsFile=\"{}\" {}",
            known_hosts.display(),
            theirs.join(" ")
        ));
    if let Some(config) = config {
        command.arg("-F").arg(config);
    }
    if let Some(port) = computer.port {
        command.arg("-p").arg(port.to_string());
    }
    if let Some(user) = &computer.user {
        command.arg("-l").arg(user);
    }
    command
        .arg("--")
        .arg(&computer.host)
        .arg(&script)
        .stdin(if network.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let ran = tokio::time::timeout(CHECK_TIMEOUT, async {
        use tokio::io::AsyncWriteExt as _;
        let mut child = command.spawn()?;
        if let Some(input) = network {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| std::io::Error::other("SSH stdin is missing"))?;
            stdin.write_all(input.as_bytes()).await?;
            stdin.shutdown().await?;
        }
        child.wait_with_output().await
    })
    .await;
    for file in [&key_file, &askpass_file] {
        let _ = std::fs::remove_file(file);
    }
    match ran {
        Err(_) => Err(format!(
            "{} did not answer within {} s",
            computer.host,
            CHECK_TIMEOUT.as_secs()
        )),
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => Err(
            "UXDuck reaches companion computers with OpenSSH's ssh command, and it is not installed on this computer"
                .to_owned(),
        ),
        Ok(Err(error)) => Err(format!("Cannot run ssh: {error}")),
        Ok(Ok(output)) => Ok(output),
    }
}

/// What a finished `ssh` says: the computer's system, or why not.
fn answer(
    computer: &CompanionComputer,
    sign_in: SignIn<'_>,
    output: &std::process::Output,
) -> Result<Found, String> {
    let pinned = computer.check.as_ref().and_then(|c| c.host_key.as_deref());
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(failure(&computer.host, pinned, &stderr));
    }
    let (host_key, certified) = served_key(&stderr)
        .ok_or_else(|| "Signed in, but ssh did not say which host key it was sent".to_owned())?;
    if let Some(pinned) = pinned
        && !certified
        && pinned != host_key
    {
        return Err(changed(&computer.host, pinned, Some(&host_key)));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if sign_in.password.is_some() && !stdout.lines().any(|l| l == "keyed=yes") {
        return Err(format!(
            "Signed in to {}, but could not add UXDuck's key to its ~/.ssh/authorized_keys",
            computer.host
        ));
    }
    let field = |name: &str| {
        stdout
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    Ok(Found {
        network: field("network")
            .map(|json| serde_json::from_str(&json))
            .transpose()
            .map_err(|_| "The computer returned an invalid network status".to_owned())?,
        os: field("os"),
        arch: field("arch"),
        uxduck: field("uxduck").map(|v| v.strip_prefix("uxduck ").unwrap_or(&v).to_owned()),
        host_key: Some(host_key),
    })
}

/// Adds `public` to the signed-in user's `authorized_keys` once, and says so.
fn add_key(public: &str) -> String {
    format!(
        "k='{public}'; mkdir -p ~/.ssh && chmod 700 ~/.ssh && touch ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys && (grep -qxF \"$k\" ~/.ssh/authorized_keys || echo \"$k\" >> ~/.ssh/authorized_keys) && echo keyed=yes"
    )
}

fn write_private(path: &Path, key: &str) -> Result<(), String> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut text = key.trim().to_owned();
    text.push('\n');
    std::io::Write::write_all(
        &mut options
            .open(path)
            .map_err(|e| format!("Cannot write its key for ssh: {e}"))?,
        text.as_bytes(),
    )
    .map_err(|e| format!("Cannot write its key for ssh: {e}"))
}

/// The known-hosts files the person's own `ssh` reads for this computer, as
/// `ssh -G` says, so their pinned keys and an organisation's
/// `@cert-authority` lines count too. UXDuck's own file comes first, where a
/// new key is written.
async fn their_known_hosts(config: Option<&Path>, computer: &CompanionComputer) -> Vec<String> {
    let mut command = tokio::process::Command::new("ssh");
    command.arg("-G");
    if let Some(config) = config {
        command.arg("-F").arg(config);
    }
    if let Some(port) = computer.port {
        command.arg("-p").arg(port.to_string());
    }
    if let Some(user) = &computer.user {
        command.arg("-l").arg(user);
    }
    let Ok(output) = command.arg("--").arg(&computer.host).output().await else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("userknownhostsfile "))
        .map(|files| files.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// The host key `ssh -v` says it was sent, and whether it came with a host
/// certificate, which an organisation's CA vouches for instead of a pin.
fn served_key(stderr: &str) -> Option<(String, bool)> {
    stderr.lines().find_map(|line| {
        let (rest, certified) = if let Some(rest) = line.strip_prefix("debug1: Server host key: ") {
            (rest, false)
        } else {
            (
                line.strip_prefix("debug1: Server host certificate: ")?,
                true,
            )
        };
        let key = rest
            .split_whitespace()
            .find(|w| w.starts_with("SHA256:"))?
            .trim_end_matches(',');
        Some((key.to_owned(), certified))
    })
}

fn changed(host: &str, pinned: &str, now: Option<&str>) -> String {
    format!(
        "{host}'s host key changed: UXDuck pinned {pinned}, and it now sends {}. Someone may be pretending to be it. If it was reinstalled, remove it and add it again.",
        now.unwrap_or("another")
    )
}

/// Why ssh failed, for people.
fn failure(host: &str, pinned: Option<&str>, stderr: &str) -> String {
    if stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        let now = stderr
            .split_whitespace()
            .find(|w| w.starts_with("SHA256:"))
            .map(|w| w.trim_end_matches('.'));
        return changed(host, pinned.unwrap_or("its first key"), now);
    }
    if refused(stderr) {
        return format!(
            "{host} refused the sign-in: add your key to ssh-agent, or check once with its password so UXDuck adds its own key"
        );
    }
    if stderr.contains("Could not resolve hostname") {
        return format!("{host} was not found: check its name, and that it is on this network");
    }
    if stderr.contains("Connection refused") {
        return format!("{host} refused the connection: is SSH turned on, and is the port right?");
    }
    let last = stderr
        .lines()
        .map(str::trim)
        .rfind(|l| {
            !l.is_empty()
                && !l.starts_with("Warning: Permanently added")
                && !l.starts_with("debug")
                && !l.starts_with("OpenSSH_")
                && !l.starts_with("Transferred:")
                && !l.starts_with("Bytes per second")
        })
        .unwrap_or("ssh failed");
    format!("Cannot reach {host}: {last}")
}

#[cfg(all(test, unix))]
#[allow(clippy::expect_used, clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use uxduck_core::domain::companion::{CheckState, CompanionCheck, CompanionRole};

    #[test]
    fn network_manager_adapter_regressions() {
        let status = std::process::Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/companions/network_test.py"
            ))
            .status()
            .expect("the NetworkManager adapter tests need Python 3");
        assert!(status.success());
    }

    /// An sshd of this user's on a free port, with its own host key and
    /// one client key, run from a temporary folder.
    struct Server {
        dir: tempfile::TempDir,
        port: u16,
        child: std::process::Child,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn keygen(path: &Path) {
        let ok = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(path)
            .status()
            .expect("ssh-keygen is needed for the companion SSH tests")
            .success();
        assert!(ok, "ssh-keygen failed");
    }

    fn sshd() -> PathBuf {
        let path = ["/usr/sbin/sshd", "/usr/bin/sshd", "/usr/local/sbin/sshd"]
            .into_iter()
            .map(PathBuf::from)
            .find(|p| p.exists());
        path.expect("the companion SSH tests need OpenSSH's sshd: install openssh-server")
    }

    fn start(host_key: &str, port: Option<u16>) -> Server {
        start_signed(host_key, port, None)
    }

    /// An sshd whose host key is signed by the CA key at `ca`, when given.
    fn start_signed(host_key: &str, port: Option<u16>, ca: Option<&Path>) -> Server {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        keygen(&root.join(host_key));
        let mut extra = String::new();
        if let Some(ca) = ca {
            let ok = std::process::Command::new("ssh-keygen")
                .args(["-q", "-s"])
                .arg(ca)
                .args(["-I", "test", "-h", "-n", "127.0.0.1"])
                .arg(root.join(format!("{host_key}.pub")))
                .status()
                .unwrap()
                .success();
            assert!(ok, "ssh-keygen could not sign the host key");
            extra = format!(
                "HostCertificate {}\n",
                root.join(format!("{host_key}-cert.pub")).display()
            );
        }
        keygen(&root.join("client"));
        std::fs::copy(root.join("client.pub"), root.join("authorized_keys")).unwrap();
        let port = port.unwrap_or_else(|| {
            std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port()
        });
        std::fs::write(
            root.join("sshd_config"),
            format!(
                "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nAuthorizedKeysFile {}\nPidFile {}\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nStrictModes no\nUsePAM no\n{extra}",
                root.join(host_key).display(),
                root.join("authorized_keys").display(),
                root.join("sshd.pid").display(),
            ),
        )
        .unwrap();
        let child = std::process::Command::new(sshd())
            .args(["-D", "-e", "-f"])
            .arg(root.join("sshd_config"))
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        for _ in 0..100 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Server { dir, port, child }
    }

    fn computer(server: &Server) -> CompanionComputer {
        CompanionComputer {
            id: "cctest".into(),
            name: "test".into(),
            host: "127.0.0.1".into(),
            user: std::env::var("USER").ok(),
            port: Some(server.port),
            role: CompanionRole::Report,
            component_id: 191,
            key: true,
            check: None,
        }
    }

    fn pasted(key: &str) -> SignIn<'_> {
        SignIn {
            pasted: Some(key),
            ..SignIn::default()
        }
    }

    fn client_key(server: &Server) -> String {
        std::fs::read_to_string(server.dir.path().join("client")).unwrap()
    }

    #[tokio::test]
    async fn a_network_change_refuses_a_replaced_host_before_running_commands() {
        let first = start("host", None);
        let keys = tempfile::tempdir().unwrap();
        let config = keys.path().join("ssh_config");
        std::fs::write(&config, "UserKnownHostsFile /dev/null\n").unwrap();
        let mut computer = computer(&first);
        let found = check(
            keys.path(),
            Some(&config),
            &computer,
            pasted(&client_key(&first)),
        )
        .await
        .unwrap();
        computer.check = Some(CompanionCheck {
            state: CheckState::Reached,
            at_us: 1,
            os: None,
            arch: None,
            uxduck: None,
            host_key: found.host_key,
            problem: None,
            network: None,
        });
        let port = first.port;
        drop(first);
        let second = start("replacement", Some(port));
        let error = network(
            keys.path(),
            &computer,
            pasted(&client_key(&second)),
            r#"{"connectivity":{"uri":"https://example.test/204","interval":60}}"#,
        )
        .await
        .unwrap_err();
        assert!(error.contains("host key no longer matches"), "{error}");
    }

    #[tokio::test]
    async fn a_check_reads_the_system_and_pins_the_host_key() {
        let server = start("host", None);
        let keys = tempfile::tempdir().unwrap();
        let config = keys.path().join("ssh_config");
        std::fs::write(&config, "UserKnownHostsFile /dev/null\n").unwrap();
        let mut computer = computer(&server);
        let found = check(
            keys.path(),
            Some(&config),
            &computer,
            pasted(&client_key(&server)),
        )
        .await
        .unwrap();
        assert!(found.os.is_some(), "{found:?}");
        assert_eq!(
            found.arch.as_deref(),
            Some(std::env::consts::ARCH).map(|a| if a == "aarch64" && cfg!(target_os = "macos") {
                "arm64"
            } else {
                a
            })
        );
        let pinned = found.host_key.clone().unwrap();
        assert!(pinned.starts_with("SHA256:"));
        assert!(
            !keys.path().join("cctest.key").exists(),
            "the key is not left on disk"
        );
        computer.check = Some(CompanionCheck {
            state: CheckState::Reached,
            at_us: 1,
            os: None,
            arch: None,
            uxduck: None,
            host_key: Some(pinned.clone()),
            problem: None,
            network: None,
        });
        let again = check(
            keys.path(),
            Some(&config),
            &computer,
            pasted(&client_key(&server)),
        )
        .await
        .unwrap();
        assert_eq!(again.host_key, Some(pinned));
    }

    #[tokio::test]
    async fn a_changed_host_key_is_refused_with_both_fingerprints() {
        let keys = tempfile::tempdir().unwrap();
        let config = keys.path().join("ssh_config");
        std::fs::write(&config, "UserKnownHostsFile /dev/null\n").unwrap();
        let first = start("host", None);
        let mut computer = computer(&first);
        let found = check(
            keys.path(),
            Some(&config),
            &computer,
            pasted(&client_key(&first)),
        )
        .await
        .unwrap();
        let pinned = found.host_key.unwrap();
        computer.check = Some(CompanionCheck {
            state: CheckState::Reached,
            at_us: 1,
            os: None,
            arch: None,
            uxduck: None,
            host_key: Some(pinned.clone()),
            problem: None,
            network: None,
        });
        let port = first.port;
        drop(first);
        let second = start("other", Some(port));
        let why = check(
            keys.path(),
            Some(&config),
            &computer,
            pasted(&client_key(&second)),
        )
        .await
        .unwrap_err();
        assert!(why.contains("host key changed"), "{why}");
        assert!(why.contains(&pinned), "{why}");
        let now = why
            .split_whitespace()
            .filter(|w| w.starts_with("SHA256:"))
            .nth(1)
            .unwrap_or_else(|| panic!("no second fingerprint: {why}"));
        assert_ne!(now.trim_end_matches('.'), pinned);
    }

    /// The person's agent and config are tried, and UXDuck's own key alone
    /// after them, so it signs in even when their keys are refused.
    #[tokio::test]
    async fn uxduck_signs_in_with_its_own_key_once_it_is_on_the_computer() {
        let server = start("host", None);
        let keys = tempfile::tempdir().unwrap();
        let config = keys.path().join("ssh_config");
        std::fs::write(&config, "UserKnownHostsFile /dev/null\n").unwrap();
        let own = OwnKey {
            private: client_key(&server),
            public: std::fs::read_to_string(server.dir.path().join("client.pub")).unwrap(),
        };
        let mut computer = computer(&server);
        computer.key = false;
        let sign_in = SignIn {
            own: Some(&own),
            ..SignIn::default()
        };
        let found = check(keys.path(), Some(&config), &computer, sign_in)
            .await
            .unwrap();
        assert!(found.host_key.is_some());
    }

    /// A host key an organisation's CA signed is trusted through the
    /// person's own known-hosts `@cert-authority` line, and a new signed key on
    /// the same computer is not refused as a changed pin.
    #[tokio::test]
    async fn a_host_certified_by_a_ca_the_person_trusts_is_not_pinned() {
        let keys = tempfile::tempdir().unwrap();
        let ca = keys.path().join("ca");
        keygen(&ca);
        let trusted = keys.path().join("known_hosts");
        let ca_public = std::fs::read_to_string(keys.path().join("ca.pub")).unwrap();
        std::fs::write(&trusted, format!("@cert-authority * {ca_public}")).unwrap();
        let config = keys.path().join("ssh_config");
        std::fs::write(
            &config,
            format!(
                "UserKnownHostsFile {}
",
                trusted.display()
            ),
        )
        .unwrap();
        let first = start_signed("host", None, Some(&ca));
        let mut computer = computer(&first);
        let found = check(
            keys.path(),
            Some(&config),
            &computer,
            pasted(&client_key(&first)),
        )
        .await
        .unwrap();
        computer.check = Some(CompanionCheck {
            state: CheckState::Reached,
            at_us: 1,
            os: None,
            arch: None,
            uxduck: None,
            host_key: found.host_key,
            problem: None,
            network: None,
        });
        let port = first.port;
        drop(first);
        let second = start_signed("other", Some(port), Some(&ca));
        check(
            keys.path(),
            Some(&config),
            &computer,
            pasted(&client_key(&second)),
        )
        .await
        .unwrap();
        assert!(
            !std::fs::read_to_string(keys.path().join("cctest.known_hosts"))
                .unwrap_or_default()
                .contains("ssh-ed25519"),
            "nothing pinned for a certified host"
        );
    }

    #[test]
    fn the_own_key_is_added_once_with_safe_permissions() {
        let home = tempfile::tempdir().unwrap();
        let line = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJ uxduck";
        for _ in 0..2 {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(add_key(line))
                .env("HOME", home.path())
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "keyed=yes");
        }
        let file = home.path().join(".ssh/authorized_keys");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), format!("{line}\n"));
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(
                &std::fs::metadata(&file).unwrap().permissions()
            ) & 0o777,
            0o600
        );
    }

    #[test]
    fn failures_read_as_what_to_do() {
        assert!(
            failure(
                "pi.local",
                None,
                "pi@pi.local: Permission denied (publickey)."
            )
            .contains("ssh-agent")
        );
        assert!(
            failure(
                "pi.local",
                None,
                "ssh: Could not resolve hostname pi.local: nodename nor servname provided"
            )
            .contains("not found")
        );
    }
}
