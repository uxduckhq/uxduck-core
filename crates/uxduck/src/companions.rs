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

use uxduck_core::domain::companion::{CompanionClock, CompanionComputer};
use uxduck_engine::companions::{CheckAnswer, CompanionOrder, CompanionPort, Found};
use uxduck_engine::runtime::Credentials as _;

use crate::adapters::credentials::FileCredentials;
use crate::daemon::engine_task::EngineHandle;

/// How long a check may take, signing in included.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a camera change may take: a restart, up to 20 s for the cameras,
/// and the same again for a rollback (ADR-0060 C5). It goes on on the
/// computer if SSH gives up first.
const CAMERA_CHANGE_TIMEOUT: Duration = Duration::from_secs(200);

/// What a pinned SSH command runs on the computer, with JSON on its stdin.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Adapter {
    /// `NetworkManager` (ADR-0057).
    Network,
    /// `MediaMTX`'s cameras (ADR-0060).
    Cameras,
}

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

/// What the computer is asked: its system, architecture and UXDuck, then
/// its clock (S356), whether the kernel says it is synchronised and network
/// time is on (`timedatectl`, where there is one) and which time service
/// runs (`systemctl`, where there is one), then the arguments of each
/// process naming UXDuck (S372), split by the unit separator, for the id its
/// readings go out as.
const SCRIPT: &str = r#"if [ -r /etc/os-release ]; then . /etc/os-release; fi; echo "os=${PRETTY_NAME:-$(uname -sr)}"; echo "arch=$(uname -m)"; v=$(uxduck --version 2>/dev/null || "$HOME/.local/bin/uxduck" --version 2>/dev/null); echo "uxduck=$v"; echo "clock=$(date +%s%N 2>/dev/null)"; if command -v timedatectl >/dev/null 2>&1; then timedatectl show -p NTP -p NTPSynchronized 2>/dev/null; fi; if command -v systemctl >/dev/null 2>&1; then for s in systemd-timesyncd chrony chronyd ntpsec ntp ntpd openntpd; do if systemctl is-active --quiet "$s" 2>/dev/null; then echo "timeservice=$s"; fi; done; fi; for f in /proc/[0-9]*/cmdline; do a=$(tr '\000' '\037' < "$f" 2>/dev/null) || continue; case "$a" in *uxduck*) echo "process=$a";; esac; done"#;

/// One `ssh`'s output, and when it started and ended on this computer's
/// wall clock, in nanoseconds since 1970, which bound when the computer read
/// its clock.
struct Ran {
    output: std::process::Output,
    started_ns: i64,
    ended_ns: i64,
}

fn wall_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

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

    /// Runs one adapter through the computer's exact pinned host key and
    /// tells the engine what it answered.
    fn run_pinned(
        &self,
        vehicle_id: String,
        computer: Box<CompanionComputer>,
        adapter: Adapter,
        input: String,
    ) {
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
                    pinned(
                        &dir,
                        &computer,
                        SignIn {
                            pasted: key.as_ref().and_then(Option::as_deref),
                            own: Some(&own),
                            password: None,
                        },
                        adapter,
                        &input,
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
            } => self.run_pinned(vehicle_id, computer, Adapter::Network, input.0),
            CompanionOrder::Cameras {
                vehicle_id,
                computer,
                input,
            } => {
                let mut request: serde_json::Value =
                    serde_json::from_str(&input).unwrap_or_default();
                if request["watchdog"] == "install" {
                    request["watchdogScript"] =
                        include_str!("companions/camera_watchdog.py").into();
                }
                self.run_pinned(vehicle_id, computer, Adapter::Cameras, request.to_string());
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
        let again = !tried.output.status.success()
            && refused(&String::from_utf8_lossy(&tried.output.stderr));
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

/// Reach networking and cameras only through the exact previously verified
/// raw host key. Keys in either UXDuck's or the person's `known_hosts` are
/// accepted only when their fingerprint matches the pin. No keyscan or
/// trust-on-first-use here.
#[allow(
    clippy::too_many_lines,
    reason = "host key verification, the ways to sign in and the routing checkpoint stay in one scope"
)]
async fn pinned(
    dir: &Path,
    computer: &CompanionComputer,
    sign_in: SignIn<'_>,
    adapter: Adapter,
    input: &str,
) -> Result<Found, String> {
    let what = match adapter {
        Adapter::Network => "networks",
        Adapter::Cameras => "cameras",
    };
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
        return Err(format!(
            "The pinned SSH host key is missing from known_hosts. Check the computer with its raw host key before setting up its {what}."
        ));
    }
    let ways = if let Some(key) = sign_in.pasted {
        vec![Way::Pasted(key)]
    } else if let Some(own) = sign_in.own {
        vec![Way::Person, Way::Own(own)]
    } else {
        vec![Way::Person]
    };
    for way in ways {
        let ran = run_adapter(dir, None, computer, &trusted, way, Some((adapter, input))).await?;
        let output = &ran.output;
        if !output.status.success() && refused(&String::from_utf8_lossy(&output.stderr)) {
            continue;
        }
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
                || stderr.contains("Host key verification failed")
            {
                return Err(format!(
                    "The companion's SSH host key no longer matches. No {what} were changed."
                ));
            }
            if adapter == Adapter::Cameras {
                return Err(camera_failure(&stderr));
            }
            return Err("Network setup failed. The companion needs Linux with NetworkManager, Python 3 and busctl. Saving needs passwordless sudo. Check its network values and service logs.".into());
        }
        if let Some(token) = String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.strip_prefix("checkpoint="))
        {
            let confirm = serde_json::json!({"confirm": token}).to_string();
            // A new TCP/SSH connection must reach the same pinned host after
            // reapplying routes. Otherwise the companion restores its checkpoint.
            let confirmed = run(dir, None, computer, &trusted, way, Some(&confirm)).await
                .map_err(|_| "The routing change could not be confirmed. Automatic rollback starts after 90 seconds on the companion.".to_owned())?;
            if !confirmed.output.status.success() {
                return Err("The routing change could not be confirmed. Automatic rollback starts after 90 seconds on the companion.".into());
            }
            return answer(computer, sign_in, &confirmed);
        }
        return answer(computer, sign_in, &ran);
    }
    Err(format!(
        "SSH could not sign in. Check the computer again before setting up its {what}."
    ))
}

/// Why the camera adapter failed, for people: its own reason when it gave
/// one, which never holds a secret.
fn camera_failure(stderr: &str) -> String {
    if let Some(reason) = stderr
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("uxduck-camera: "))
    {
        return reason.trim().to_owned();
    }
    if stderr.contains("sudo:") {
        return "Changing cameras needs passwordless sudo on the computer for the user UXDuck signs in as.".to_owned();
    }
    if stderr.contains("python3: not found") || stderr.contains("python3: command not found") {
        return "Reading cameras needs Python 3 on the computer.".to_owned();
    }
    "Reading or changing the cameras failed. The computer needs Linux with systemd, Python 3 and MediaMTX's API. Changes need passwordless sudo.".to_owned()
}

/// One `ssh` to the computer for a check, or with a network request on its
/// stdin.
async fn run(
    dir: &Path,
    config: Option<&Path>,
    computer: &CompanionComputer,
    known_hosts: &Path,
    way: Way<'_>,
    network: Option<&str>,
) -> Result<Ran, String> {
    run_adapter(
        dir,
        config,
        computer,
        known_hosts,
        way,
        network.map(|input| (Adapter::Network, input)),
    )
    .await
}

/// One `ssh` to the computer, signing in one `way`.
#[allow(
    clippy::too_many_lines,
    reason = "SSH options and temporary credential lifetimes stay in one scope"
)]
async fn run_adapter(
    dir: &Path,
    config: Option<&Path>,
    computer: &CompanionComputer,
    known_hosts: &Path,
    way: Way<'_>,
    adapter: Option<(Adapter, &str)>,
) -> Result<Ran, String> {
    let network = adapter.map(|(_, input)| input);
    let cameras = matches!(adapter, Some((Adapter::Cameras, _)));
    let mut timeout = CHECK_TIMEOUT;
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
        let sudo = if ["wifi", "connectivity", "preference", "confirm"]
            .iter()
            .all(|key| input[key].is_null())
        {
            ""
        } else {
            "sudo -n "
        };
        script = format!("{SCRIPT}; {sudo}python3 -c '{script_python}'");
        if cameras {
            let change = input["apply"].is_object()
                || input["linger"] == true
                || !input["watchdog"].is_null();
            if change {
                timeout = CAMERA_CHANGE_TIMEOUT;
            }
            let sudo = if change { "sudo -n " } else { "" };
            let quoted = include_str!("companions/camera.py").replace('\'', "'\"'\"'");
            script = format!("{SCRIPT}; {sudo}python3 -c '{quoted}'");
        }
        // For any network operation trust only the key matching the stored pin.
        // A user known-hosts entry changed elsewhere cannot authorize a mutation.
        command.args([
            "-o",
            "HostKeyAlias=uxduck-network",
            "-o",
            "GlobalKnownHostsFile=none",
            "-o",
            "CheckHostIP=no",
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
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
    let started_ns = wall_ns();
    let ran = tokio::time::timeout(timeout, async {
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
    let ended_ns = wall_ns();
    for file in [&key_file, &askpass_file] {
        let _ = std::fs::remove_file(file);
    }
    match ran {
        Err(_) => Err(format!(
            "{} did not answer within {} s",
            computer.host,
            timeout.as_secs()
        )),
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => Err(
            "UXDuck reaches companion computers with OpenSSH's ssh command, and it is not installed on this computer"
                .to_owned(),
        ),
        Ok(Err(error)) => Err(format!("Cannot run ssh: {error}")),
        Ok(Ok(output)) => Ok(Ran {
            output,
            started_ns,
            ended_ns,
        }),
    }
}

/// What a finished `ssh` says: the computer's system, or why not.
fn answer(computer: &CompanionComputer, sign_in: SignIn<'_>, ran: &Ran) -> Result<Found, String> {
    let output = &ran.output;
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
    let yes = |name: &str| field(name).map(|v| v == "yes");
    let clock = field("clock")
        .and_then(|read| clock_read(&read, (ran.started_ns, ran.ended_ns)))
        .map(|(offset_ms, within_ms)| CompanionClock {
            offset_ms,
            within_ms,
            synchronised: yes("NTPSynchronized"),
            ntp: yes("NTP"),
            service: field("timeservice"),
        });
    Ok(Found {
        network: field("network")
            .map(|json| serde_json::from_str(&json))
            .transpose()
            .map_err(|_| "The computer returned an invalid network status".to_owned())?,
        cameras: field("cameras")
            .map(|json| serde_json::from_str(&json))
            .transpose()
            .map_err(|_| "The computer returned an invalid camera status".to_owned())?,
        os: field("os"),
        arch: field("arch"),
        uxduck: field("uxduck").map(|v| v.strip_prefix("uxduck ").unwrap_or(&v).to_owned()),
        host_key: Some(host_key),
        clock,
        reports_as: reported_id(&stdout),
    })
}

/// The onboard computer id the computer's `uxduck companion` or
/// `uxduck forward --onboard` was started with (S372), from the check's
/// `process=` lines. None when neither runs, when one runs without
/// `--computer-id` (it chooses its own) or when two were given different
/// ids, because then the ids alone cannot say which readings are its.
fn reported_id(stdout: &str) -> Option<u8> {
    let ids: std::collections::BTreeSet<Option<u8>> = stdout
        .lines()
        .filter_map(|line| line.strip_prefix("process="))
        .filter_map(|line| {
            let args: Vec<&str> = line.split('\u{1f}').filter(|a| !a.is_empty()).collect();
            let program = Path::new(args.first()?).file_name()?;
            if program != "uxduck" {
                return None;
            }
            let reports = match args[1..]
                .iter()
                .find(|a| matches!(**a, "companion" | "forward"))
            {
                Some(&"companion") => true,
                Some(_) => args.contains(&"--onboard"),
                None => false,
            };
            if !reports {
                return None;
            }
            let given = args
                .windows(2)
                .find_map(|pair| (pair[0] == "--computer-id").then_some(pair[1]))
                .or_else(|| args.iter().find_map(|a| a.strip_prefix("--computer-id=")));
            Some(
                given
                    .and_then(|id| id.parse::<u8>().ok())
                    .filter(|id| uxduck_core::domain::companion::COMPONENT_IDS.contains(id)),
            )
        })
        .collect();
    match ids.into_iter().collect::<Vec<_>>()[..] {
        [Some(id)] => Some(id),
        _ => None,
    }
}

/// The computer's clock, `date +%s%N` as it printed it, against this
/// computer's while the command ran (S356): its offset from the middle of
/// that time and half of it, in milliseconds. A `date` without `%N`
/// (macOS, busybox) prints whole seconds and a letter, read as the middle of
/// its second with half a second more doubt. None for anything else.
fn clock_read(read: &str, (started_ns, ended_ns): (i64, i64)) -> Option<(i64, u64)> {
    let digits = read.trim_end_matches(|c: char| !c.is_ascii_digit());
    let value: i64 = digits.parse().ok()?;
    let (read_ns, doubt_ns) = if digits.len() == read.len() && digits.len() > 12 {
        (value, 0)
    } else {
        (
            value.checked_mul(1_000_000_000)?.checked_add(500_000_000)?,
            500_000_000,
        )
    };
    let middle_ns = started_ns.checked_add(ended_ns.checked_sub(started_ns)? / 2)?;
    let half_ns = (ended_ns.checked_sub(started_ns)? / 2).checked_add(doubt_ns)?;
    let offset_ns = read_ns.checked_sub(middle_ns)?;
    let ms = 1_000_000;
    Some((
        (offset_ns + offset_ns.signum() * ms / 2) / ms,
        u64::try_from((half_ns + ms - 1) / ms).ok()?,
    ))
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

    #[test]
    fn camera_adapter_regressions() {
        for test in ["camera_test.py", "camera_watchdog_test.py"] {
            let status = std::process::Command::new("python3")
                .arg(format!(
                    "{}/src/companions/{test}",
                    env!("CARGO_MANIFEST_DIR")
                ))
                .status()
                .expect("the camera adapter tests need Python 3");
            assert!(status.success(), "{test}");
        }
    }

    #[test]
    fn the_camera_adapter_answer_is_read_field_for_field() {
        let cameras: uxduck_core::domain::companion_cameras::CompanionCameras = serde_json::from_str(
            r#"{"paths": [{"name": "cam", "source": "rpiCamera", "onDemand": false, "ready": true, "readers": 1, "bytes": 123456, "cameraId": 0,
                "settings": {"width": 1536, "height": 864, "fps": 50, "mode": "2304:1296:10:P", "idrPeriod": 50, "bitrate": 2000000},
                "setByUxduck": true, "sensor": "imx708"}, {"name": "relay", "source": "rtsp", "onDemand": true, "ready": false, "readers": 0, "bytes": 0}],
               "watchdog": {"installed": true, "running": true, "gaveUp": true, "restarts": [{"atUs": 1700000000500000, "path": "cam"}]},
               "unit": "mediamtx.service", "dropIn": "/etc/systemd/system/mediamtx.service.d/60-uxduck-camera.conf",
               "config": "/usr/local/etc/mediamtx.yml", "linger": {"user": "pi", "lingering": false, "removeIpc": true},
               "api": "127.0.0.1:9997", "applied": {"path": "cam", "outcome": "rolled-back", "failed": "cam", "log": ["ERR [path cam] gone"]}}"#,
        )
        .unwrap();
        let cam = &cameras.paths[0];
        assert!(cam.set_by_uxduck && cam.ready && !cam.on_demand);
        assert_eq!(
            (cam.camera_id, cam.readers, cam.bytes),
            (Some(0), 1, 123_456)
        );
        assert_eq!(cam.settings.as_ref().unwrap().idr_period, 50);
        assert!(cameras.paths[1].on_demand);
        assert!(cameras.linger.unwrap().at_risk());
        assert!(cameras.watchdog.gave_up && cameras.watchdog.running);
        assert_eq!(cameras.watchdog.restarts[0].at_us, 1_700_000_000_500_000);
        let applied = cameras.applied.unwrap();
        assert_eq!(
            applied.outcome,
            uxduck_core::domain::companion_cameras::PictureOutcome::RolledBack
        );
        assert_eq!(applied.log, ["ERR [path cam] gone"]);
        assert!(cameras.drop_in.unwrap().ends_with("60-uxduck-camera.conf"));
    }

    #[test]
    fn camera_failures_name_the_adapter_reason() {
        assert_eq!(
            camera_failure("debug1: x\nuxduck-camera: Use 1 to 120 pictures a second.\n"),
            "Use 1 to 120 pictures a second."
        );
        assert!(camera_failure("sudo: a password is required\n").contains("passwordless sudo"));
        assert!(camera_failure("bash: python3: command not found\n").contains("Python 3"));
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
            connectivity: None,
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
            clock: None,
            reports_as: None,
            network: None,
            cameras: None,
        });
        let port = first.port;
        drop(first);
        let second = start("replacement", Some(port));
        let error = pinned(
            keys.path(),
            &computer,
            pasted(&client_key(&second)),
            Adapter::Network,
            r#"{"connectivity":{"uri":"https://example.test/204","interval":60}}"#,
        )
        .await
        .unwrap_err();
        assert!(error.contains("host key no longer matches"), "{error}");
    }

    #[tokio::test]
    async fn a_camera_change_refuses_a_replaced_host_before_running_commands() {
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
            clock: None,
            reports_as: None,
            network: None,
            cameras: None,
        });
        let port = first.port;
        drop(first);
        let second = start("replacement", Some(port));
        let error = pinned(
            keys.path(),
            &computer,
            pasted(&client_key(&second)),
            Adapter::Cameras,
            r#"{"apply":{"path":"cam","settings":{"width":1536,"height":864,"fps":50,"mode":"2304:1296:10:P","idrPeriod":50,"bitrate":2000000}},"linger":false}"#,
        )
        .await
        .unwrap_err();
        assert!(error.contains("host key no longer matches"), "{error}");
        assert!(error.contains("No cameras were changed"), "{error}");
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
        // The same computer's clock: in step, to within the command's time.
        let clock = found.clock.clone().expect("its clock is read");
        assert!(
            clock.offset_ms.unsigned_abs() <= clock.within_ms + 1,
            "{clock:?}"
        );
        assert!(clock.within_ms < 30_000, "{clock:?}");
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
            clock: None,
            reports_as: None,
            network: None,
            cameras: None,
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
            clock: None,
            reports_as: None,
            network: None,
            cameras: None,
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
            clock: None,
            reports_as: None,
            network: None,
            cameras: None,
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

    fn ran(stdout: &str, (started_ns, ended_ns): (i64, i64)) -> Ran {
        Ran {
            output: std::process::Output {
                status: std::os::unix::process::ExitStatusExt::from_raw(0),
                stdout: stdout.as_bytes().to_vec(),
                stderr: b"debug1: Server host key: ssh-ed25519 SHA256:abc\n".to_vec(),
            },
            started_ns,
            ended_ns,
        }
    }

    #[test]
    fn a_check_reads_the_clock_and_whether_it_keeps_time() {
        // 13 November 2026, the command taking 400 ms, and a computer that
        // read its clock 8 days 6 h behind halfway through.
        let started = 1_794_000_000_000_000_000;
        let ended = started + 400_000_000;
        let read = started + 200_000_000 - 712_800_000_000_000;
        let computer = CompanionComputer {
            id: "c".into(),
            name: "pi".into(),
            host: "pi".into(),
            user: None,
            port: None,
            role: CompanionRole::Report,
            component_id: 192,
            key: false,
            check: None,
            connectivity: None,
        };
        let out = format!(
            "os=Debian GNU/Linux 13 (trixie)\narch=aarch64\nuxduck=\nclock={read}\nNTP=yes\nNTPSynchronized=no\ntimeservice=systemd-timesyncd\n"
        );
        let found = answer(&computer, SignIn::default(), &ran(&out, (started, ended))).unwrap();
        assert_eq!(
            found.clock,
            Some(CompanionClock {
                offset_ms: -712_800_000,
                within_ms: 200,
                synchronised: Some(false),
                ntp: Some(true),
                service: Some("systemd-timesyncd".into()),
            })
        );
        // Without timedatectl or systemd (Alpine, a container), and with a
        // date that has no nanoseconds: the clock alone, with more doubt.
        let seconds = (started + 200_000_000) / 1_000_000_000;
        let out = format!("os=Alpine Linux v3.20\narch=x86_64\nclock={seconds}N\n");
        let found = answer(&computer, SignIn::default(), &ran(&out, (started, ended))).unwrap();
        let clock = found.clock.unwrap();
        assert_eq!((clock.offset_ms, clock.within_ms), (300, 700));
        assert_eq!(
            (clock.synchronised, clock.ntp, clock.service),
            (None, None, None)
        );
        // No date at all: no clock, and the rest as before.
        let found = answer(
            &computer,
            SignIn::default(),
            &ran("os=x\nclock=\n", (started, ended)),
        )
        .unwrap();
        assert_eq!((found.os.as_deref(), found.clock), (Some("x"), None));
    }

    #[test]
    fn a_check_reads_the_id_its_uxduck_sends_as() {
        let process = |args: &[&str]| format!("process={}\u{1f}", args.join("\u{1f}"));
        let companion = process(&[
            "/usr/local/bin/uxduck",
            "companion",
            "--to",
            "udp:192.168.50.21:15010",
            "--system-id",
            "1",
            "--computer-id",
            "191",
        ]);
        let this_check = process(&["bash", "-c", "uxduck --version; uxduck companion"]);
        let daemon = process(&[
            "/opt/uxduck/0.17.0/uxduck",
            "daemon",
            "--data-dir",
            "/var/lib/uxduck",
        ]);
        assert_eq!(
            reported_id(&format!("os=Debian\n{this_check}\n{companion}\n{daemon}\n")),
            Some(191),
            "Buggy's camera Pi, pinned to 191 when it was set up by hand"
        );
        let forward = process(&[
            "uxduck",
            "forward",
            "serial:/dev/ttyAMA0",
            "--onboard",
            "--computer-id=193",
        ]);
        assert_eq!(reported_id(&forward), Some(193));
        let relay = process(&[
            "uxduck",
            "forward",
            "serial:/dev/ttyAMA0",
            "--computer-id=193",
        ]);
        assert_eq!(reported_id(&relay), None, "a forward that does not report");
        let choosing = process(&["uxduck", "forward", "udp:0.0.0.0:14550", "--onboard"]);
        assert_eq!(reported_id(&choosing), None, "it chooses its own id");
        assert_eq!(
            reported_id(&format!("{companion}\n{forward}")),
            None,
            "two ids for one computer"
        );
        assert_eq!(reported_id(&format!("{companion}\n{companion}")), Some(191));
        let out_of_range = process(&["uxduck", "companion", "--computer-id", "1"]);
        assert_eq!(reported_id(&out_of_range), None);
        assert_eq!(reported_id("os=Debian\nuxduck=0.16.0\n"), None);
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
