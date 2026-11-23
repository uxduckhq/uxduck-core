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
//!
//! A failed check says what failed and what to do, with the messages `ssh`
//! itself wrote, never a server's banner or what ran on the computer, and
//! with the password and any key hidden (FB-268). A key or password `ssh`
//! reads from a file is in a new file only this computer's account may
//! open, which Windows' OpenSSH requires, in a folder of its own for that
//! one `ssh`, removed when it ends and when UXDuck starts.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use uxduck_core::domain::companion::{CheckProblem, CompanionClock, CompanionComputer};
use uxduck_engine::companions::{CheckAnswer, CheckFailure, CompanionOrder, CompanionPort, Found};
use uxduck_engine::runtime::Credentials as _;

use crate::adapters::credentials::{
    FileCredentials, create_private, create_private_dir, private_dir_problem,
};
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
            // A check adds UXDuck's key only once `ssh` has asked for the
            // password (FB-268). The marker is new in the run's own folder.
            if let Err(error) = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(askpass_used(path))
            {
                eprintln!("uxduck askpass: {error}");
            }
            println!("{password}");
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("uxduck askpass: {error}");
            std::process::ExitCode::from(1)
        }
    }
}

/// The file [`askpass`] leaves beside the password's once it has given it to
/// `ssh`.
fn askpass_used(path: &Path) -> PathBuf {
    path.with_extension("used")
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
    /// The computer SSH goes through when none of its own addresses
    /// answers (`computer/route`).
    pub through: Option<&'a Through<'a>>,
}

/// A computer another is reached through, and how to sign in to it: its
/// own pasted key alone, else the person's agent with UXDuck's own key.
/// Its host key must already be pinned by a check of its own.
pub(crate) struct Through<'a> {
    pub computer: &'a CompanionComputer,
    pub pasted: Option<&'a str>,
    pub own: Option<&'a OwnKey>,
}

/// A problem with UXDuck's own key, which a check with the computer's
/// password solves by making a new one when it cannot be read.
fn key_failure(problem: String, detail: Option<String>) -> CheckFailure {
    CheckFailure {
        problem,
        kind: CheckProblem::Key,
        detail,
    }
}

/// UXDuck's own key from the credential store, made on first use. The key
/// is read whole and its public line made from it ([`own_public`], FB-268).
/// It was read with `ssh-keygen -y` from a temporary file, which Windows'
/// OpenSSH refuses when other accounts may open it, and what `ssh-keygen`
/// said was lost.
/// A stored key that cannot be read is replaced only when `replace`, for
/// a check with a password, which adds the new key to the computer.
fn own_key(credentials: &mut FileCredentials, replace: bool) -> Result<OwnKey, CheckFailure> {
    if let Some(private) = credentials.get(OWN_KEY) {
        let why = match own_public(&private) {
            Ok(own) => return Ok(own),
            Err(why) => why,
        };
        if !replace {
            return Err(key_failure(
                format!(
                    "UXDuck's own SSH key, kept on this computer, cannot be read, as {why}. Check with the computer's password to make a new one and add it."
                ),
                None,
            ));
        }
        tracing::warn!(%why, "replacing UXDuck's SSH key, which cannot be read");
    }
    let dir = tempfile::tempdir()
        .map_err(|e| key_failure(format!("UXDuck cannot make its SSH key: {e}"), None))?;
    let path = dir.path().join("key");
    let made = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "uxduck", "-f"])
        .arg(&path)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                key_failure(
                    ssh_missing().replace("OpenSSH's ssh command", "OpenSSH's ssh-keygen command"),
                    None,
                )
            } else {
                key_failure(
                    format!("UXDuck cannot run ssh-keygen to make its SSH key: {e}"),
                    None,
                )
            }
        })?;
    if !made.status.success() {
        return Err(key_failure(
            "ssh-keygen could not make UXDuck's SSH key.".to_owned(),
            hidden(&String::from_utf8_lossy(&made.stderr)),
        ));
    }
    let private = std::fs::read_to_string(&path).map_err(|e| {
        key_failure(
            format!("UXDuck cannot read the SSH key ssh-keygen made: {e}"),
            None,
        )
    })?;
    let own = own_public(&private).map_err(|why| {
        key_failure(
            format!("UXDuck cannot read the SSH key ssh-keygen made, as {why}."),
            None,
        )
    })?;
    credentials.set(OWN_KEY, &own.private).map_err(|why| {
        key_failure(
            "UXDuck cannot keep its SSH key in its credential store on this computer, so it cannot add it to the companion. The detail says why.".to_owned(),
            Some(why),
        )
    })?;
    Ok(own)
}

/// UXDuck's own key read from the whole of its private half: the Ed25519
/// key with no passphrase that UXDuck makes, its two check numbers equal,
/// its public key the same in both halves and the one its private seed
/// makes, and its padding as OpenSSH writes it. A key damaged anywhere is
/// refused with why, so a check with the password makes a new one
/// (FB-268). Its private half is written out again as `ssh-keygen` writes
/// it ([`openssh_ed25519`]), with the comment `uxduck`, so `ssh` reads
/// exactly the key checked here, whatever spacing or comment the stored
/// text had.
fn own_public(private: &str) -> Result<OwnKey, String> {
    use aws_lc_rs::signature::KeyPair as _;
    use base64::Engine as _;
    fn field<'a>(rest: &mut &'a [u8]) -> Option<&'a [u8]> {
        let length = number(rest)?;
        let (value, after) = rest.split_at_checked(usize::try_from(length).ok()?)?;
        *rest = after;
        Some(value)
    }
    fn number(rest: &mut &[u8]) -> Option<u32> {
        let (number, after) = rest.split_first_chunk::<4>()?;
        *rest = after;
        Some(u32::from_be_bytes(*number))
    }
    let damaged = || "its text is damaged or cut short".to_owned();
    let body: String = private
        .trim()
        .strip_prefix("-----BEGIN OPENSSH PRIVATE KEY-----")
        .and_then(|body| body.strip_suffix("-----END OPENSSH PRIVATE KEY-----"))
        .ok_or_else(|| "it is not an OpenSSH private key".to_owned())?
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(body)
        .map_err(|_| damaged())?;
    let mut rest = bytes
        .strip_prefix(b"openssh-key-v1\0")
        .ok_or_else(damaged)?;
    if field(&mut rest).ok_or_else(damaged)? != b"none" {
        return Err("it has a passphrase".to_owned());
    }
    if field(&mut rest) != Some(b"none")
        || field(&mut rest) != Some(b"")
        || number(&mut rest) != Some(1)
    {
        return Err(damaged());
    }
    let public = field(&mut rest).ok_or_else(damaged)?;
    let mut section = field(&mut rest).ok_or_else(damaged)?;
    if !rest.is_empty() || section.len() % 8 != 0 {
        return Err(damaged());
    }
    let mut blob = public;
    if field(&mut blob) != Some(b"ssh-ed25519") {
        return Err("it is not the Ed25519 key UXDuck makes".to_owned());
    }
    let key = field(&mut blob)
        .filter(|key| key.len() == 32 && blob.is_empty())
        .ok_or_else(damaged)?;
    let check = number(&mut section);
    if check.is_none() || check != number(&mut section) {
        return Err(damaged());
    }
    if field(&mut section) != Some(b"ssh-ed25519") || field(&mut section) != Some(key) {
        return Err(damaged());
    }
    let secret = field(&mut section)
        .filter(|secret| secret.len() == 64 && &secret[32..] == key)
        .ok_or_else(damaged)?;
    field(&mut section).ok_or_else(damaged)?;
    if section.len() >= 8
        || !section
            .iter()
            .enumerate()
            .all(|(at, byte)| usize::from(*byte) == at + 1)
    {
        return Err(damaged());
    }
    let pair = aws_lc_rs::signature::Ed25519KeyPair::from_seed_unchecked(&secret[..32])
        .map_err(|_| damaged())?;
    if pair.public_key().as_ref() != key {
        return Err(damaged());
    }
    let base64 = base64::engine::general_purpose::STANDARD;
    let bytes = openssh_ed25519(public, check.unwrap_or_default(), secret, b"uxduck");
    let mut private = String::from("-----BEGIN OPENSSH PRIVATE KEY-----\n");
    for line in base64.encode(&bytes).as_bytes().chunks(70) {
        private.push_str(&String::from_utf8_lossy(line));
        private.push('\n');
    }
    private.push_str("-----END OPENSSH PRIVATE KEY-----\n");
    Ok(OwnKey {
        private,
        public: format!("ssh-ed25519 {} uxduck", base64.encode(public)),
    })
}

/// An Ed25519 key with no passphrase as `ssh-keygen` writes it, before its
/// base64: `public` its public key's blob, `check` its private half's
/// check number, `secret` its 64-byte secret, and `comment`.
fn openssh_ed25519(public: &[u8], check: u32, secret: &[u8], comment: &[u8]) -> Vec<u8> {
    fn put(out: &mut Vec<u8>, field: &[u8]) {
        out.extend_from_slice(&u32::try_from(field.len()).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(field);
    }
    let mut section = Vec::new();
    section.extend_from_slice(&check.to_be_bytes());
    section.extend_from_slice(&check.to_be_bytes());
    put(&mut section, b"ssh-ed25519");
    put(&mut section, &secret[32..]);
    put(&mut section, secret);
    put(&mut section, comment);
    let mut pad = 1u8;
    while section.len() % 8 != 0 {
        section.push(pad);
        pad += 1;
    }
    let mut bytes = b"openssh-key-v1\0".to_vec();
    put(&mut bytes, b"none");
    put(&mut bytes, b"none");
    put(&mut bytes, b"");
    bytes.extend_from_slice(&1u32.to_be_bytes());
    put(&mut bytes, public);
    put(&mut bytes, &section);
    bytes
}

/// What signs in to a computer through its pinned host key, from the
/// credential store: the private key the person gave for it, when they gave
/// one, and UXDuck's own key.
pub(crate) fn saved_sign_in(
    data_dir: &Path,
    computer: &CompanionComputer,
) -> Result<(Option<String>, OwnKey), String> {
    let mut credentials = FileCredentials::new(data_dir);
    let key = if computer.key {
        Some(
            credentials
                .get(&computer.key_name())
                .ok_or_else(|| "Its saved SSH private key is missing.".to_owned())?,
        )
    } else {
        None
    };
    Ok((
        key,
        own_key(&mut credentials, false).map_err(|failure| failure.problem)?,
    ))
}

/// The saved private key of the computer another is reached through, when
/// it has one.
pub(crate) fn saved_through_key(
    data_dir: &Path,
    through: Option<&CompanionComputer>,
) -> Result<Option<String>, String> {
    let Some(through) = through.filter(|t| t.key) else {
        return Ok(None);
    };
    FileCredentials::new(data_dir)
        .get(&through.key_name())
        .map(Some)
        .ok_or_else(|| {
            format!(
                "{}'s saved SSH private key is missing, and this computer is reached through it.",
                through.name
            )
        })
}

/// The keys a check signs in with: the one given for the computer, from the
/// credential store when it has one, and UXDuck's own. A key given for it
/// signs in alone when no password was typed, so then a problem with
/// UXDuck's own key does not stop the check (FB-268).
fn check_keys(
    has_key: bool,
    given: Option<String>,
    own: Result<OwnKey, CheckFailure>,
    password: bool,
) -> Result<(Option<String>, Option<OwnKey>), CheckFailure> {
    if has_key && given.is_none() {
        return Err(CheckFailure {
            problem: "The private key you gave for this computer is missing from UXDuck's credential store. Remove the computer and add it again.".to_owned(),
            kind: CheckProblem::GivenKey,
            detail: None,
        });
    }
    match own {
        Ok(own) => Ok((given, Some(own))),
        Err(_) if given.is_some() && !password => Ok((given, None)),
        Err(failure) => Err(failure),
    }
}

/// The ways to sign in through a pinned host key, in the order they are
/// tried: the key the person gave for the computer alone, else their own SSH
/// config and agent and then UXDuck's own key. Never a password.
pub(crate) fn pinned_ways<'a>(pasted: Option<&'a str>, own: Option<&'a OwnKey>) -> Vec<Way<'a>> {
    if let Some(key) = pasted {
        vec![Way::Pasted(key)]
    } else if let Some(own) = own {
        vec![Way::Person, Way::Own(own)]
    } else {
        vec![Way::Person]
    }
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
    /// What `ssh` itself wrote, in its own log: never a server's banner or
    /// what ran on the computer, which are on `output`'s standard error.
    log: String,
    /// What the `ssh` it went through wrote in its own log, when it went
    /// through another computer.
    hop_log: String,
    /// Whether UXDuck's askpass gave `ssh` the password: a file UXDuck's
    /// askpass leaves, never anything a server says.
    asked: bool,
    started_ns: i64,
    ended_ns: i64,
}

/// The folder under the companions folder where each `ssh` a check or a
/// change runs gets a folder of its own ([`RunFiles`]).
const RUNS: &str = "runs";

/// The files one `ssh` for a check or a change is given, a key, a password
/// for its prompt and its log, in a new folder of their own. The folder is
/// removed when `ssh` has ended, and one a crash left is removed when
/// UXDuck starts ([`sweep`]).
struct RunFiles {
    path: PathBuf,
    dir: Option<tempfile::TempDir>,
}

impl RunFiles {
    fn new(dir: &Path) -> Result<Self, String> {
        let runs = dir.join(RUNS);
        let made = create_private_dir(dir)
            .and_then(|()| create_private_dir(&runs))
            .and_then(|()| {
                let mut builder = tempfile::Builder::new();
                builder.prefix("ssh-");
                #[cfg(unix)]
                builder.permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700));
                builder.tempdir_in(&runs)
            });
        let made = made.map_err(|e| format!("Cannot prepare the files ssh is given: {e}"))?;
        Ok(Self {
            path: made.path().to_owned(),
            dir: Some(made),
        })
    }
}

impl Drop for RunFiles {
    fn drop(&mut self) {
        if let Some(dir) = self.dir.take()
            && let Err(error) = dir.close()
        {
            tracing::warn!(%error, path = %self.path.display(), "cannot remove the files ssh was given, which may hold a password or a key");
        }
    }
}

/// Removes what a crash left of the files `ssh` was given under the
/// companions folder `dir`: the run folders [`RunFiles`] makes, and from
/// before them a computer's key or password file beside its known-hosts
/// file. Only those exact names are removed, and only when the folder is
/// this account's alone ([`private_dir_problem`]), so no other account can
/// change what is in it between looking and removing. Otherwise nothing is
/// removed, and the log says why.
fn sweep(dir: &Path) {
    let warn = |error: std::io::Error, path: &Path| {
        tracing::warn!(%error, path = %path.display(), "cannot remove the files ssh was given, which may hold a password or a key");
    };
    if !dir.exists() {
        return;
    }
    if let Some(why) = private_dir_problem(dir) {
        tracing::warn!(path = %dir.display(), why, "not removing what a crash left of the files ssh was given, because the companions folder is not this account's alone");
        return;
    }
    let runs = dir.join(RUNS);
    if own_folder(&runs) {
        for entry in std::fs::read_dir(&runs).into_iter().flatten().flatten() {
            let run = entry
                .file_type()
                .is_ok_and(|kind| kind.is_dir() && !kind.is_symlink())
                && run_name(&entry.file_name());
            if run && let Err(error) = std::fs::remove_dir_all(entry.path()) {
                warn(error, &entry.path());
            }
        }
    }
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let left =
            entry.file_type().is_ok_and(|kind| !kind.is_dir()) && legacy_name(&entry.file_name());
        if left && let Err(error) = std::fs::remove_file(entry.path()) {
            warn(error, &entry.path());
        }
    }
}

/// Whether `path` is a folder of its own, not a link or a Windows junction
/// to one elsewhere. One that is a link is logged.
fn own_folder(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    let linked = meta.file_type().is_symlink();
    // FILE_ATTRIBUTE_REPARSE_POINT: a junction or another link.
    #[cfg(windows)]
    let linked = linked || std::os::windows::fs::MetadataExt::file_attributes(&meta) & 0x400 != 0;
    if linked {
        tracing::warn!(path = %path.display(), "not removing the files ssh was given here, because the folder is a link");
    }
    meta.is_dir() && !linked
}

/// Whether `name` is a run folder [`RunFiles`] makes: `ssh-` and six
/// letters or digits.
fn run_name(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .and_then(|name| name.strip_prefix("ssh-"))
        .is_some_and(|rest| rest.len() == 6 && rest.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// Whether `name` is a key or password file a check wrote before run
/// folders: a computer's id, `cc` and twelve hex digits, with `.key` or
/// `.askpass`.
fn legacy_name(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .and_then(|name| {
            name.strip_suffix(".key")
                .or_else(|| name.strip_suffix(".askpass"))
        })
        .and_then(|id| id.strip_prefix("cc"))
        .is_some_and(|hex| {
            hex.len() == 12
                && hex
                    .chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        })
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
        sweep(&data_dir.join("companions"));
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
        through: Option<Box<CompanionComputer>>,
        adapter: Adapter,
        input: String,
    ) {
        let dir = self.dir.clone();
        let saved = saved_sign_in(&self.data_dir, &computer).and_then(|saved| {
            Ok((
                saved,
                saved_through_key(&self.data_dir, through.as_deref())?,
            ))
        });
        let engine = self.engine.clone();
        tokio::spawn(async move {
            let result = match saved {
                Err(why) => Err(CheckFailure::from(why)),
                Ok(((key, own), through_key)) => {
                    let through = through.as_deref().map(|computer| Through {
                        computer,
                        pasted: through_key.as_deref(),
                        own: Some(&own),
                    });
                    pinned(
                        &dir,
                        &computer,
                        SignIn {
                            pasted: key.as_deref(),
                            own: Some(&own),
                            password: None,
                            through: through.as_ref(),
                        },
                        adapter,
                        &input,
                    )
                    .await
                    .map_err(CheckFailure::from)
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
                through,
            } => {
                let dir = self.dir.clone();
                let mut credentials = FileCredentials::new(&self.data_dir);
                let key = computer
                    .key
                    .then(|| credentials.get(&computer.key_name()))
                    .flatten();
                let own = own_key(&mut credentials, password.is_some());
                let keys = check_keys(computer.key, key, own, password.is_some());
                let through_key = saved_through_key(&self.data_dir, through.as_deref());
                let engine = self.engine.clone();
                tokio::spawn(async move {
                    let result = match (keys, through_key) {
                        (Err(failure), _) => Err(failure),
                        (_, Err(why)) => Err(CheckFailure::from(why)),
                        (Ok((pasted, own)), Ok(through_key)) => {
                            let through = through.as_deref().map(|computer| Through {
                                computer,
                                pasted: through_key.as_deref(),
                                own: own.as_ref(),
                            });
                            let sign_in = SignIn {
                                pasted: pasted.as_deref(),
                                own: own.as_ref(),
                                password: password.as_ref().map(|p| p.0.as_str()),
                                through: through.as_ref(),
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
                through,
            } => self.run_pinned(vehicle_id, computer, through, Adapter::Network, input.0),
            CompanionOrder::Cameras {
                vehicle_id,
                computer,
                input,
                through,
            } => {
                let mut request: serde_json::Value =
                    serde_json::from_str(&input).unwrap_or_default();
                if request["watchdog"] == "install" {
                    request["watchdogScript"] =
                        include_str!("companions/camera_watchdog.py").into();
                }
                self.run_pinned(
                    vehicle_id,
                    computer,
                    through,
                    Adapter::Cameras,
                    request.to_string(),
                );
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
pub(crate) enum Way<'a> {
    /// The key the person gave for this computer, alone.
    Pasted(&'a str),
    /// The person's SSH config and agent, as in their terminal.
    Person,
    /// UXDuck's own key, alone.
    Own(&'a OwnKey),
    /// The password, with UXDuck's own key added once signed in.
    Password(&'a OwnKey, &'a str),
    /// The password alone, running nothing: what proves the password signs
    /// in before UXDuck's key is added ([`proven`]).
    PasswordOnly(&'a str),
}

/// Signs in to `computer` and reads what it runs: with its password when
/// one was typed, to add UXDuck's own key, else the key the person gave it,
/// else their own SSH config and agent and then UXDuck's own key, each alone
/// so a server's limit on tries is not spent on keys it will refuse.
/// `config` replaces the person's `~/.ssh/config`, for tests.
pub(crate) async fn check(
    dir: &Path,
    config: Option<&Path>,
    computer: &CompanionComputer,
    sign_in: SignIn<'_>,
) -> Result<Found, CheckFailure> {
    if let Some(password) = sign_in.password
        && let Some(problem) = password_problem(password)
    {
        return Err(CheckFailure {
            problem,
            kind: CheckProblem::Password,
            detail: None,
        });
    }
    create_private_dir(dir)
        .map_err(|e| format!("Cannot keep host keys in {}: {e}", dir.display()))?;
    let known_hosts = dir.join(format!("{}.known_hosts", computer.id));
    // A typed password comes first, even for a computer with a key given
    // for it: the person chose it, and it alone adds UXDuck's key (FB-268).
    let ways = match sign_in {
        SignIn {
            own: Some(own),
            password: Some(password),
            ..
        } => vec![Way::Password(own, password)],
        SignIn {
            pasted: Some(key), ..
        } => vec![Way::Pasted(key)],
        SignIn { own: Some(own), .. } => vec![Way::Person, Way::Own(own)],
        _ => vec![Way::Person],
    };
    let mut last = None;
    for way in ways {
        if let Way::Password(_, password) = way {
            let probe = run(
                dir,
                config,
                computer,
                &known_hosts,
                Way::PasswordOnly(password),
                None,
                sign_in.through,
            )
            .await?;
            if !proven(&probe) {
                return Err(unproven(computer, way, &probe));
            }
        }
        let tried = run(
            dir,
            config,
            computer,
            &known_hosts,
            way,
            None,
            sign_in.through,
        )
        .await?;
        let again = !tried.output.status.success() && refused(&tried.log);
        last = Some((way, tried));
        if !again {
            break;
        }
    }
    let Some((way, output)) = last else {
        return Err("No way to sign in".into());
    };
    answer(computer, sign_in, way, &output)
}

/// Whether a `ssh` that could sign in with the password alone
/// ([`Way::PasswordOnly`]) did: it ended with status 0, so it signed in and
/// ran its command, and UXDuck's askpass gave it the password. Nothing
/// a server says is read (FB-268).
fn proven(probe: &Ran) -> bool {
    probe.output.status.success() && probe.asked
}

/// Why a check with the password stopped before adding UXDuck's key, when
/// it was not [`proven`].
fn unproven(computer: &CompanionComputer, way: Way<'_>, probe: &Ran) -> CheckFailure {
    let host = &computer.host;
    let status = probe.output.status.code();
    if status == Some(0) {
        return CheckFailure {
            problem: format!(
                "Signed in to {host} without its password, so UXDuck did not add its key. Check again without the password."
            ),
            kind: CheckProblem::Other,
            detail: None,
        };
    }
    if status == Some(255) || status.is_none() {
        let pinned = computer.check.as_ref().and_then(|c| c.host_key.as_deref());
        return check_failure(computer, pinned, way, &probe.log);
    }
    CheckFailure {
        problem: format!(
            "Signed in to {host} with the password, but it could not run a command there, so UXDuck did not add its key. Check that its user's shell is a POSIX shell such as sh or bash."
        ),
        kind: CheckProblem::Other,
        detail: status
            .map(|code| format!("The command UXDuck ran there ended with status {code}.")),
    }
}

/// The most bytes of a password `ssh` reads from UXDuck's askpass: OpenSSH's
/// `ssh_askpass` reads into 1,024 bytes and keeps one for its end.
const ASKPASS_MAX: usize = 1023;

/// Why `ssh` cannot be given `password` as typed, when it cannot: it stops
/// reading a password at a line break or a NUL, and reads at most
/// [`ASKPASS_MAX`] bytes of it, so it would sign in with another (FB-268).
fn password_problem(password: &str) -> Option<String> {
    if password.contains(['\r', '\n', '\0']) {
        return Some("ssh ends a password at a line break or a NUL character, so UXDuck cannot pass this one to it as typed. Change the password, or add your own key to ssh-agent and check again.".to_owned());
    }
    (password.len() > ASKPASS_MAX).then(|| {
        format!(
            "ssh reads at most {ASKPASS_MAX} bytes of a password, and this one is {} bytes, so UXDuck cannot pass it to ssh as typed. Use a shorter password, or add your own key to ssh-agent and check again.",
            password.len()
        )
    })
}

/// Whether `ssh` was refused the sign-in before anything ran on the
/// computer ([`refusal`]), so trying another way is safe.
pub(crate) fn refused(log: &str) -> bool {
    refusal(log).is_some()
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
    if !trust_pinned(dir, computer, pinned, &trusted).await? {
        return Err(format!(
            "The pinned SSH host key is missing from known_hosts. Check the computer with its raw host key before setting up its {what}."
        ));
    }
    for way in pinned_ways(sign_in.pasted, sign_in.own) {
        let ran = run_adapter(
            dir,
            None,
            computer,
            &trusted,
            way,
            Some((adapter, input)),
            sign_in.through,
        )
        .await
        .map_err(|failure| failure.problem)?;
        let output = &ran.output;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if [ran.log.as_str(), &stderr].iter().any(|said| {
                said.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
                    || said.contains("Host key verification failed")
            }) {
                return Err(format!(
                    "The companion's SSH host key no longer matches. No {what} were changed."
                ));
            }
            // Only a refusal before signing in, when nothing ran, is tried
            // again another way.
            if refused(&ran.log) {
                continue;
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
            let confirmed = run(dir, None, computer, &trusted, way, Some(&confirm), sign_in.through).await
                .map_err(|_| "The routing change could not be confirmed. Automatic rollback starts after 90 seconds on the companion.".to_owned())?;
            if !confirmed.output.status.success() {
                return Err("The routing change could not be confirmed. Automatic rollback starts after 90 seconds on the companion.".into());
            }
            return answer(computer, sign_in, way, &confirmed).map_err(|failure| failure.problem);
        }
        return answer(computer, sign_in, way, &ran).map_err(|failure| failure.problem);
    }
    Err(format!(
        "SSH could not sign in. Check the computer again before setting up its {what}."
    ))
}

/// Writes `trusted`, a known-hosts file holding only the raw host key whose
/// fingerprint is `pinned`, under the alias [`HostKeys::Pinned`] gives
/// `ssh`. The key is looked for in UXDuck's own file for the computer and
/// in the files the person's own `ssh` reads. `false` when none of them has
/// it.
pub(crate) async fn trust_pinned(
    dir: &Path,
    computer: &CompanionComputer,
    pinned: &str,
    trusted: &Path,
) -> Result<bool, String> {
    trust_as(dir, computer, pinned, trusted, PINNED_ALIAS).await
}

/// [`trust_pinned`] under `alias`, which is [`THROUGH_ALIAS`] for the
/// computer another is reached through.
async fn trust_as(
    dir: &Path,
    computer: &CompanionComputer,
    pinned: &str,
    trusted: &Path,
    alias: &str,
) -> Result<bool, String> {
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
            write_private(trusted, &format!("{alias} {} {}", fields[1], fields[2]))?;
            let output = tokio::process::Command::new("ssh-keygen")
                .args(["-l", "-E", "sha256", "-f"])
                .arg(trusted)
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
        // The last key tried is not the pinned one: nothing is trusted.
        let _ = std::fs::remove_file(trusted);
    }
    Ok(matched)
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
    through: Option<&Through<'_>>,
) -> Result<Ran, CheckFailure> {
    run_adapter(
        dir,
        config,
        computer,
        known_hosts,
        way,
        network.map(|input| (Adapter::Network, input)),
        through,
    )
    .await
}

/// The alias a pinned host key is kept under, so only that key counts
/// whatever the computer's name or address resolves to.
const PINNED_ALIAS: &str = "uxduck-network";

/// The alias the pinned host key of the computer another is reached
/// through is kept under, in a file of its own.
const THROUGH_ALIAS: &str = "uxduck-through";

/// The computer a route goes through, at one of its addresses, and the
/// files its own `ssh` reads and writes: its key, written by [`ssh`], its
/// pinned host key, written by [`trust_through`], and its log, so what it
/// says about itself reaches neither a terminal nor a server's words.
#[derive(Clone, Copy)]
pub(crate) struct Hop<'a> {
    pub through: &'a Through<'a>,
    pub at: &'a str,
    pub key_file: &'a Path,
    pub known_hosts: &'a Path,
    pub log: &'a Path,
}

/// One way to a computer: one of its own addresses, or its address as seen
/// from the computer it goes through, reached at one of that one's.
#[derive(Clone, Copy)]
pub(crate) struct Route<'a> {
    pub host: &'a str,
    pub hop: Option<Hop<'a>>,
}

impl std::fmt::Display for Route<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.hop {
            None => f.write_str(self.host),
            Some(hop) => write!(
                f,
                "{} through {} at {}",
                self.host, hop.through.computer.name, hop.at
            ),
        }
    }
}

/// The routes to `computer` in the order they are tried: each of its own
/// addresses, then its address through `through` at each of that one's.
/// A password goes to its first address alone, since another address's key
/// is checked only after signing in.
pub(crate) fn routes<'a>(
    computer: &'a CompanionComputer,
    through: Option<&'a Through<'a>>,
    key_file: &'a Path,
    known_hosts: &'a Path,
    log: &'a Path,
    password: bool,
) -> Vec<Route<'a>> {
    let mut routes: Vec<Route<'a>> = computer
        .hosts()
        .map(|host| Route { host, hop: None })
        .collect();
    if password {
        routes.truncate(1);
        return routes;
    }
    if let Some(through) = through {
        routes.extend(through.computer.hosts().map(|at| Route {
            host: &computer.host,
            hop: Some(Hop {
                through,
                at,
                key_file,
                known_hosts,
                log,
            }),
        }));
    }
    routes
}

/// Writes the pinned host key of the computer `hop` goes through to its
/// `known_hosts`, as [`trust_pinned`] does for the computer itself. Why not,
/// for people, when that computer has not been checked or the key it
/// pinned is gone.
pub(crate) async fn trust_through(dir: &Path, hop: Hop<'_>) -> Result<(), String> {
    let computer = hop.through.computer;
    let unknown = format!(
        "Through {}: check {} first, so UXDuck knows its host key",
        computer.name, computer.name
    );
    let Some(pinned) = computer.check.as_ref().and_then(|c| c.host_key.as_deref()) else {
        return Err(unknown);
    };
    if trust_as(dir, computer, pinned, hop.known_hosts, THROUGH_ALIAS).await? {
        Ok(())
    } else {
        Err(unknown)
    }
}

/// What a change or a terminal says when no route reached the computer:
/// each route with why, in UXDuck's own words, and what that means.
pub(crate) fn cannot_reach(computer: &CompanionComputer, missed: &[String], then: &str) -> String {
    format!(
        "Can't reach {}. {}. {then}",
        computer.name,
        missed.join(". ")
    )
}

/// How one `ssh` verifies the host it reaches.
#[derive(Clone, Copy)]
pub(crate) enum HostKeys<'a> {
    /// A check: UXDuck's own file for the computer first, where a new key
    /// is pinned, then the files the person's own `ssh` reads.
    AcceptNew {
        known_hosts: &'a Path,
        theirs: &'a [String],
    },
    /// Only the exact pinned key, alone in this file ([`trust_pinned`]).
    /// Networks, cameras and terminals.
    Pinned(&'a Path),
}

/// What one `ssh` is for, which decides its terminal and where it writes
/// what it says about itself.
#[derive(Clone, Copy)]
pub(crate) enum Session<'a> {
    /// One batch command with no terminal. `ssh -v` writes on its standard
    /// error, where a check reads the host key it was sent.
    Batch,
    /// A terminal (ADR-0065): a pseudo-terminal when `tty`, no escape
    /// character, and what `ssh` says about itself in `log`, so none of it
    /// reaches the terminal's bytes. At `LogLevel=VERBOSE` it says when it
    /// signed in and never the command it runs.
    Terminal { tty: bool, log: &'a Path },
}

/// The arguments and environment of one `ssh` to a computer.
pub(crate) struct Ssh {
    pub args: Vec<std::ffi::OsString>,
    pub envs: Vec<(&'static str, std::ffi::OsString)>,
}

/// The one place `ssh`'s options are chosen, for checks, network and camera
/// changes and terminals: signing in one `way`, verifying the host with
/// `host_keys`, never adding keys to the person's agent, and running
/// `remote` on the computer, or what it gives someone who signs in when
/// there is none. A key or password `way` needs is written to `key_file` or
/// `askpass_file`, which the caller removes when `ssh` has ended. It goes
/// to `route`, through another computer when the route says so, whose key
/// the caller removes too.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "every choice a caller makes about one ssh, in one place"
)]
pub(crate) fn ssh(
    key_file: &Path,
    askpass_file: &Path,
    config: Option<&Path>,
    computer: &CompanionComputer,
    route: Route<'_>,
    way: Way<'_>,
    host_keys: HostKeys<'_>,
    session: Session<'_>,
    remote: Option<&str>,
) -> Result<Ssh, String> {
    use std::ffi::OsString;
    let mut args: Vec<OsString> = Vec::new();
    let mut envs: Vec<(&'static str, OsString)> = Vec::new();
    let option = |args: &mut Vec<OsString>, option: &str| {
        args.push("-o".into());
        args.push(option.into());
    };
    let only = |args: &mut Vec<OsString>, key: &str| -> Result<(), String> {
        write_private(key_file, key)?;
        args.push("-i".into());
        args.push(key_file.into());
        args.push("-o".into());
        args.push("IdentitiesOnly=yes".into());
        Ok(())
    };
    match way {
        Way::Pasted(key) => {
            only(&mut args, key)?;
            option(&mut args, "BatchMode=yes");
        }
        Way::Person => option(&mut args, "BatchMode=yes"),
        Way::Own(own) => {
            only(&mut args, &own.private)?;
            option(&mut args, "IdentityAgent=none");
            option(&mut args, "BatchMode=yes");
        }
        Way::Password(_, password) | Way::PasswordOnly(password) => {
            // The password alone, as typed, by the password method alone:
            // no key or other method is tried, so its status 0 means the
            // password signed in (FB-268).
            write_new(askpass_file, password.as_bytes())
                .map_err(|e| format!("Cannot pass the password to ssh: {e}"))?;
            let exe = std::env::current_exe()
                .map_err(|e| format!("Cannot find UXDuck to answer ssh's password prompt: {e}"))?;
            option(&mut args, "PreferredAuthentications=password");
            option(&mut args, "PubkeyAuthentication=no");
            option(&mut args, "KbdInteractiveAuthentication=no");
            option(&mut args, "GSSAPIAuthentication=no");
            option(&mut args, "HostbasedAuthentication=no");
            option(&mut args, "IdentityAgent=none");
            // A session the person's own ssh already opened would sign in
            // without the password.
            option(&mut args, "ControlMaster=no");
            option(&mut args, "ControlPath=none");
            option(&mut args, "BatchMode=no");
            option(&mut args, "NumberOfPasswordPrompts=1");
            envs.push(("SSH_ASKPASS", exe.into()));
            envs.push(("SSH_ASKPASS_REQUIRE", "force".into()));
            envs.push((ASKPASS_FILE, askpass_file.into()));
        }
    }
    if matches!(host_keys, HostKeys::Pinned(_)) {
        // Trust only the key matching the stored pin. A user known-hosts
        // entry changed elsewhere cannot authorize a change or a terminal.
        option(&mut args, &format!("HostKeyAlias={PINNED_ALIAS}"));
        option(&mut args, "GlobalKnownHostsFile=none");
        option(&mut args, "CheckHostIP=no");
        option(&mut args, "ControlMaster=no");
        option(&mut args, "ControlPath=none");
    }
    match session {
        Session::Batch | Session::Terminal { tty: false, .. } => args.push("-T".into()),
        Session::Terminal { tty: true, .. } => {
            args.push("-tt".into());
            // Every byte typed goes to the computer: `~.` ends nothing here.
            option(&mut args, "EscapeChar=none");
        }
    }
    option(&mut args, "ConnectTimeout=10");
    option(&mut args, "AddKeysToAgent=no");
    option(
        &mut args,
        match host_keys {
            HostKeys::Pinned(_) => "StrictHostKeyChecking=yes",
            HostKeys::AcceptNew { .. } => "StrictHostKeyChecking=accept-new",
        },
    );
    option(&mut args, "HashKnownHosts=no");
    match session {
        Session::Batch => args.push("-v".into()),
        Session::Terminal { log, .. } => {
            args.push("-E".into());
            args.push(log.into());
            option(&mut args, "LogLevel=VERBOSE");
        }
    }
    let (known_hosts, theirs) = match host_keys {
        HostKeys::AcceptNew {
            known_hosts,
            theirs,
        } => (known_hosts, theirs.join(" ")),
        HostKeys::Pinned(trusted) => (trusted, String::new()),
    };
    option(
        &mut args,
        &format!("UserKnownHostsFile=\"{}\" {theirs}", known_hosts.display()),
    );
    if let Some(config) = config {
        args.push("-F".into());
        args.push(config.into());
    }
    if let Some(port) = computer.port {
        args.push("-p".into());
        args.push(port.to_string().into());
    }
    if let Some(user) = &computer.user {
        args.push("-l".into());
        args.push(user.into());
    }
    if let Some(hop) = route.hop {
        option(
            &mut args,
            &format!(
                "ProxyCommand={}",
                proxy_command(hop.through, hop.at, hop.key_file, hop.known_hosts, hop.log)?
            ),
        );
    }
    args.push("--".into());
    args.push(route.host.into());
    if let Some(remote) = remote {
        args.push(remote.into());
    }
    Ok(Ssh { args, envs })
}

/// OpenSSH's `ssh` on this computer's `PATH`, as a check runs it.
pub(crate) fn ssh_program() -> Option<PathBuf> {
    program_in(std::env::var_os("PATH").as_deref(), "ssh")
}

pub(crate) fn program_in(path: Option<&std::ffi::OsStr>, name: &str) -> Option<PathBuf> {
    let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    std::env::split_paths(path?)
        .map(|dir| dir.join(&file))
        .find(|candidate| candidate.is_file())
}

/// What a check or a terminal says when this computer has no `ssh`, with
/// how to get it.
pub(crate) fn ssh_missing() -> String {
    let get = if cfg!(windows) {
        "Add OpenSSH Client in Windows Settings under System, Optional features."
    } else if cfg!(target_os = "macos") {
        "macOS comes with it, so check that /usr/bin is on UXDuck's PATH."
    } else {
        "Install your system's OpenSSH client package, such as openssh-client."
    };
    format!(
        "UXDuck reaches companion computers with OpenSSH's ssh command, and it is not installed on this computer. {get}"
    )
}

/// `ssh` to the computer at each of its addresses in turn, then through
/// the computer it goes through at each of that one's, until one is
/// reached. Only a route that never reached the computer is left for the
/// next ([`not_reached`]), so nothing runs on it twice. When none was
/// reached, it says so with each route's reason. A check with only one route
/// keeps what that route's `ssh` said, which [`check_failure`] reads. The
/// files of the `ssh` a route goes through are in a run folder of their own
/// ([`RunFiles`]).
async fn run_adapter(
    dir: &Path,
    config: Option<&Path>,
    computer: &CompanionComputer,
    known_hosts: &Path,
    way: Way<'_>,
    adapter: Option<(Adapter, &str)>,
    through: Option<&Through<'_>>,
) -> Result<Ran, CheckFailure> {
    let hop_files = RunFiles::new(dir)?;
    let hop_key = hop_files.path.join("through_key");
    let hop_hosts = hop_files.path.join("through_known_hosts");
    let hop_log = hop_files.path.join("through.log");
    let password = matches!(way, Way::Password(..) | Way::PasswordOnly(_));
    let routes = routes(computer, through, &hop_key, &hop_hosts, &hop_log, password);
    let alone = adapter.is_none() && routes.len() == 1;
    let mut missed = Vec::new();
    let mut trusted = false;
    for route in routes {
        if let Some(hop) = route.hop
            && !trusted
        {
            if let Err(why) = trust_through(dir, hop).await {
                missed.push(why);
                break;
            }
            trusted = true;
        }
        // The `ssh` it goes through adds to its log: each route reads its own.
        let _ = std::fs::remove_file(&hop_log);
        let ran = run_route(dir, config, computer, known_hosts, way, adapter, route).await?;
        if alone || !not_reached(ran.output.status.code(), &ran.log, &ran.hop_log) {
            return Ok(ran);
        }
        missed.push(format!(
            "{route}: {}",
            unreached_why(
                &format!("{}\n{}", ran.log, ran.hop_log),
                route.hop.map(|hop| hop.through.computer.name.as_str()),
            )
        ));
    }
    Err(CheckFailure {
        problem: cannot_reach(computer, &missed, "Nothing was changed on it."),
        kind: CheckProblem::Unreachable,
        detail: None,
    })
}

/// The `ssh` that carries a connection through `through`, reached at
/// `at`, accepting only its pinned host key from `known_hosts`. Its key, a
/// pasted one alone or else UXDuck's own beside the person's agent, is
/// written to `key_file`. It writes what it says about itself to `log`, so
/// none of it reaches a terminal, and a server's banner never reaches it.
/// OpenSSH runs it with the shell on Unix, so each word is quoted for `sh`,
/// and `%` is doubled where the outer `ssh` would expand it. The inner
/// `ssh` expands `%` in its key, known-hosts and log paths again, so those
/// are doubled twice. The target is `[%h]:%p`, as OpenSSH's own `ProxyJump`
/// writes it, so an IPv6 address keeps its port. Windows' OpenSSH starts
/// it without a shell, so it is quoted for `CreateProcess` there, which is
/// untested.
pub(crate) fn proxy_command(
    through: &Through<'_>,
    at: &str,
    key_file: &Path,
    known_hosts: &Path,
    log: &Path,
) -> Result<String, String> {
    const TARGET: &str = "[%h]:%p";
    let path = |path: &Path| path.display().to_string().replace('%', "%%");
    let mut words: Vec<String> = [
        "ssh",
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
    ]
    .map(str::to_owned)
    .to_vec();
    match (through.pasted, through.own) {
        (Some(key), _) => {
            write_private(key_file, key)?;
            words.extend(["-o".into(), "IdentitiesOnly=yes".into()]);
            words.extend(["-i".into(), path(key_file)]);
        }
        (None, Some(own)) => {
            write_private(key_file, &own.private)?;
            words.extend(["-i".into(), path(key_file)]);
        }
        (None, None) => {}
    }
    for option in [
        format!("HostKeyAlias={THROUGH_ALIAS}"),
        format!("UserKnownHostsFile=\"{}\"", path(known_hosts)),
        "GlobalKnownHostsFile=none".to_owned(),
        "StrictHostKeyChecking=yes".to_owned(),
        "CheckHostIP=no".to_owned(),
        "ControlMaster=no".to_owned(),
        "ControlPath=none".to_owned(),
        "AddKeysToAgent=no".to_owned(),
    ] {
        words.extend(["-o".into(), option]);
    }
    words.extend(["-E".into(), path(log)]);
    if let Some(port) = through.computer.port {
        words.extend(["-p".into(), port.to_string()]);
    }
    if let Some(user) = &through.computer.user {
        words.extend(["-l".into(), user.clone()]);
    }
    words.extend(["-W".into(), TARGET.into(), "--".into(), at.to_owned()]);
    Ok(words
        .iter()
        .map(|word| {
            let word = if word == TARGET {
                word.clone()
            } else {
                word.replace('%', "%%")
            };
            if cfg!(windows) {
                format!("\"{}\"", word.replace('"', "\\\""))
            } else {
                format!("'{}'", word.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" "))
}

/// Whether `ssh`, which ended with `code`, never reached the computer, as its
/// own `log` and the `hop_log` of the `ssh` it went through say: its name was
/// not found, nothing answered, or the computer it goes through could not
/// reach it or refused UXDuck. Nothing ran on the computer, so another
/// route may be tried. A sign-in, a refusal by the computer itself
/// ([`refusal`]), a host key it refused and a command's own status of 255
/// never count, nor does an address `ssh` tried before one that answered,
/// since only its last line is read.
pub(crate) fn not_reached(code: Option<i32>, log: &str, hop_log: &str) -> bool {
    if code != Some(255)
        || signed_in(log)
        || log.contains("Authenticated to ")
        || log.contains("Authentication succeeded")
        || refusal(log).is_some()
        || host_key_failed(log)
    {
        return false;
    }
    let last = last_line(log);
    let unanswered = last.starts_with("ssh: connect to host ")
        || last.starts_with("ssh: Could not resolve hostname ")
        || [
            "banner exchange",
            "kex_exchange_identification",
            "Connection closed by UNKNOWN",
            "Connection to UNKNOWN",
        ]
        .iter()
        .any(|said| last.contains(said));
    let hop_missed = [
        "stdio forwarding failed",
        "open failed",
        "ssh: connect to host ",
        "Could not resolve hostname",
        "Permission denied",
        "Host key verification failed",
    ]
    .iter()
    .any(|said| hop_log.contains(said));
    unanswered || hop_missed
}

/// Why a route was not reached, in UXDuck's own words, chosen by what
/// `ssh` and the `ssh` it went through wrote in their logs. Nothing they
/// wrote is repeated, since a server can write there too (FB-268). `hop`
/// names the computer it went through, whose own refusals show in `log` too.
pub(crate) fn unreached_why(log: &str, hop: Option<&str>) -> String {
    if let Some(hop) = hop {
        if log.contains("Host key verification failed") {
            return format!("{hop}'s host key is not the one UXDuck pinned");
        }
        if log.contains("Permission denied") {
            return format!("{hop} refused UXDuck's sign-in");
        }
    }
    if log.contains("Could not resolve hostname") {
        return "name not found".to_owned();
    }
    [
        ("Connection refused", "connection refused"),
        ("No route to host", "no route to host"),
        ("Network is unreachable", "network is unreachable"),
        ("Host is down", "host is down"),
        ("timed out", "timed out"),
    ]
    .iter()
    .find(|(said, _)| log.contains(said))
    .map_or("no answer", |(_, why)| why)
    .to_owned()
}

/// One `ssh` to the computer on one route, signing in one `way`.
#[allow(
    clippy::too_many_lines,
    reason = "the script and temporary credential lifetimes stay in one scope"
)]
async fn run_route(
    dir: &Path,
    config: Option<&Path>,
    computer: &CompanionComputer,
    known_hosts: &Path,
    way: Way<'_>,
    adapter: Option<(Adapter, &str)>,
    route: Route<'_>,
) -> Result<Ran, String> {
    let network = adapter.map(|(_, input)| input);
    let cameras = matches!(adapter, Some((Adapter::Cameras, _)));
    let mut timeout = CHECK_TIMEOUT;
    let files = RunFiles::new(dir)?;
    let key_file = files.path.join("key");
    let askpass_file = files.path.join("askpass");
    let log_file = files.path.join("ssh.log");
    let theirs = if network.is_none() {
        their_known_hosts(config, computer).await
    } else {
        Vec::new()
    };
    let mut script = match way {
        Way::PasswordOnly(_) => "exit 0".to_owned(),
        Way::Password(own, _) => format!("{}; {SCRIPT}", add_key(&own.public)),
        _ => SCRIPT.to_owned(),
    };
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
    }
    // For any network or camera operation trust only the key matching the
    // stored pin.
    let host_keys = if network.is_some() {
        HostKeys::Pinned(known_hosts)
    } else {
        HostKeys::AcceptNew {
            known_hosts,
            theirs: &theirs,
        }
    };
    let built = ssh(
        &key_file,
        &askpass_file,
        config,
        computer,
        route,
        way,
        host_keys,
        Session::Batch,
        Some(&script),
    );
    let built = match built {
        Ok(built) => built,
        Err(why) => {
            for file in [&key_file, &askpass_file] {
                let _ = std::fs::remove_file(file);
            }
            if let Some(hop) = route.hop {
                let _ = std::fs::remove_file(hop.key_file);
            }
            return Err(why);
        }
    };
    let mut command = tokio::process::Command::new("ssh");
    // What `ssh` says about itself goes to its own log, apart from a
    // server's banner and what runs on the computer (FB-268).
    command
        .arg("-E")
        .arg(&log_file)
        .args(&built.args)
        .envs(built.envs.iter().map(|(name, value)| (*name, value)))
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
    if let Some(hop) = route.hop {
        let _ = std::fs::remove_file(hop.key_file);
    }
    // A test that fails says what its `ssh` was given and what it said. The
    // last argument is the script, which is left out.
    #[cfg(test)]
    if let Ok(Ok(output)) = &ran
        && !output.status.success()
    {
        eprintln!(
            "ssh ended with {:?}, given {:?}, and said:\n{}",
            output.status,
            &built.args[..built.args.len() - 1],
            String::from_utf8_lossy(&output.stderr)
        );
    }
    match ran {
        Err(_) => Err(format!(
            "{route} did not answer within {} s",
            timeout.as_secs()
        )),
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => Err(ssh_missing()),
        Ok(Err(error)) => Err(format!("Cannot run ssh: {error}")),
        Ok(Ok(output)) => Ok(Ran {
            output,
            log: std::fs::read_to_string(&log_file).unwrap_or_default(),
            hop_log: route
                .hop
                .and_then(|hop| std::fs::read_to_string(hop.log).ok())
                .unwrap_or_default(),
            asked: askpass_used(&askpass_file).exists(),
            started_ns,
            ended_ns,
        }),
    }
}

/// What a finished `ssh`, signed in one `way`, says: the computer's
/// system, or why not.
fn answer(
    computer: &CompanionComputer,
    sign_in: SignIn<'_>,
    way: Way<'_>,
    ran: &Ran,
) -> Result<Found, CheckFailure> {
    let output = &ran.output;
    let pinned = computer.check.as_ref().and_then(|c| c.host_key.as_deref());
    if !output.status.success() && !signed_in(&ran.log) {
        return Err(check_failure(computer, pinned, way, &ran.log));
    }
    let (host_key, certified) = served_key(&ran.log)
        .ok_or_else(|| "Signed in, but ssh did not say which host key it was sent".to_owned())?;
    if let Some(pinned) = pinned
        && !certified
        && pinned != host_key
    {
        return Err(CheckFailure {
            problem: changed(&computer.host, pinned, Some(&host_key)),
            kind: CheckProblem::HostKey,
            detail: None,
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Signed in, so what failed ran on the computer, whose words are never
    // shown: they may echo anything, a password included (FB-268).
    let ended = output
        .status
        .code()
        .map(|code| format!("The commands UXDuck ran there ended with status {code}."));
    if matches!(way, Way::Password(..)) && !ran.asked {
        return Err(CheckFailure {
            problem: format!(
                "Signed in to {} without its password, so UXDuck did not add its key. Check again without the password.",
                computer.host
            ),
            kind: CheckProblem::Other,
            detail: ended,
        });
    }
    if sign_in.password.is_some() && !stdout.lines().any(|l| l == "keyed=yes") {
        return Err(CheckFailure {
            problem: format!(
                "Signed in to {} with the password, but could not add UXDuck's key to its ~/.ssh/authorized_keys. Check that the user UXDuck signs in as owns its home folder.",
                computer.host
            ),
            kind: CheckProblem::Other,
            detail: ended,
        });
    }
    if !output.status.success() {
        return Err(CheckFailure {
            problem: format!(
                "Signed in to {}, but the commands UXDuck runs there to read it failed. Check that its user's shell is a POSIX shell such as sh or bash.",
                computer.host
            ),
            kind: CheckProblem::Other,
            detail: ended,
        });
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

/// Writes a key or a host key for `ssh` to read in a new file at `path`
/// that only this computer's account may open, from the moment it exists
/// (FB-268). A file already at `path` is removed, never opened, so neither
/// its permissions nor whatever it links to play a part.
pub(crate) fn write_private(path: &Path, key: &str) -> Result<(), String> {
    let mut text = key.trim().to_owned();
    text.push('\n');
    write_new(path, text.as_bytes()).map_err(|e| format!("Cannot write its key for ssh: {e}"))
}

/// [`write_private`]'s file, holding `bytes` exactly.
fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error);
    }
    let mut file = create_private(path)?;
    std::io::Write::write_all(&mut file, bytes)
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

/// Whether a terminal's `ssh` signed in, as its log says at
/// `LogLevel=VERBOSE`.
pub(crate) fn signed_in(log: &str) -> bool {
    log.lines().any(|line| {
        line.starts_with("Authenticated to ")
            || line.starts_with("debug1: Authentication succeeded")
    })
}

/// Why a terminal's `ssh` did not sign in, for people, from what it wrote
/// in its log: a check's own words, with both fingerprints for a host key
/// that changed.
pub(crate) fn terminal_failure(computer: &CompanionComputer, log: &str) -> String {
    let pinned = computer.check.as_ref().and_then(|c| c.host_key.as_deref());
    if log.contains("Host key verification failed")
        && !log.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
    {
        return changed(&computer.host, pinned.unwrap_or("its first key"), None);
    }
    failure(&computer.host, pinned, log)
}

/// The last thing `ssh` said about itself, such as that the connection
/// closed: never anything typed or shown, which its log does not hold.
pub(crate) fn last_said(log: &str) -> Option<String> {
    log.lines()
        .map(str::trim)
        .rfind(|l| {
            !l.is_empty()
                && !l.starts_with("Authenticated to ")
                && !l.starts_with("Transferred:")
                && !l.starts_with("Bytes per second")
                && !l.starts_with("debug")
        })
        .map(str::to_owned)
}

/// The ways to sign in `ssh` knows by name, the only ones a check's detail
/// names. A server may send any other name.
const METHODS: [&str; 6] = [
    "publickey",
    "password",
    "keyboard-interactive",
    "hostbased",
    "gssapi-with-mic",
    "gssapi-keyex",
];

/// The last line in `ssh`'s log: its own last word, since it stops once it
/// gives up, so nothing a server sends comes after it (FB-268).
fn last_line(log: &str) -> &str {
    log.lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or_default()
}

/// Whether `ssh` refused the computer's host key. A server can only add
/// these words, never hide them, so they come before every other reading.
fn host_key_failed(log: &str) -> bool {
    log.lines().map(str::trim).any(|line| {
        line == "Host key verification failed."
            || line.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
    })
}

/// The ways to sign in the computer offered when it refused every one,
/// from `ssh`'s last line, `user@host: Permission denied (publickey,password).`,
/// without the ones `ssh` does not know by name. Empty when the computer
/// ended the connection while UXDuck was signing in, as sshd does after too
/// many tries. None when `ssh` signed in, refused the host key or ended for
/// another reason: only Some says nothing ran on the computer.
fn refusal(log: &str) -> Option<Vec<&'static str>> {
    if signed_in(log) || host_key_failed(log) {
        return None;
    }
    let last = last_line(log);
    if let Some(methods) = last
        .split_once(": Permission denied (")
        .filter(|(who, _)| !who.starts_with("debug"))
        .and_then(|(_, rest)| rest.strip_suffix(")."))
    {
        let mut known = Vec::new();
        for method in methods.split(',') {
            if method.is_empty()
                || !method
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "@._-".contains(c))
            {
                return None;
            }
            known.extend(METHODS.iter().find(|name| **name == method));
        }
        return Some(known);
    }
    let ended = [
        "Disconnected from ",
        "Connection closed by ",
        "Connection reset by ",
    ]
    .iter()
    .any(|said| last.starts_with(said));
    let signing_in = log.lines().any(|line| {
        line.trim()
            .starts_with("debug1: Authentications that can continue: ")
    });
    (ended && signing_in).then(Vec::new)
}

/// Why `ssh` did not reach the computer at all, for people, from its last
/// line.
fn unreached(host: &str, log: &str) -> Option<String> {
    let last = last_line(log);
    if last.starts_with("ssh: Could not resolve hostname ") {
        return Some(format!(
            "{host} was not found: check its name, and that it is on this network"
        ));
    }
    let why = last.strip_prefix("ssh: connect to host ")?;
    if why.ends_with(": Connection refused") {
        return Some(format!(
            "{host} refused the connection: is SSH turned on, and is the port right?"
        ));
    }
    [
        "Connection timed out",
        "Operation timed out",
        "No route to host",
        "Network is unreachable",
        "Host is down",
    ]
    .iter()
    .any(|said| why.ends_with(said))
    .then(|| {
        format!(
            "{host} did not answer. Check that it is on, that it is on this network, and that its address is right."
        )
    })
}

/// The fingerprint `ssh` says a changed host key now has, when it is one.
fn sent_fingerprint(log: &str) -> Option<&str> {
    log.lines().map(str::trim).find_map(|line| {
        let key = line.strip_suffix('.').unwrap_or(line);
        let hash = key.strip_prefix("SHA256:")?;
        (hash.len() == 43
            && hash
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/'))
        .then_some(key)
    })
}

/// `text` from a local tool without any line that looks like part of a key.
/// None when nothing is left.
fn hidden(text: &str) -> Option<String> {
    let keyish = |line: &str| {
        line.contains("PRIVATE KEY")
            || line.split_whitespace().any(|word| {
                word.len() >= 40
                    && word
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
                    && !word.starts_with("SHA256:")
            })
    };
    let kept = text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty() && !keyish(line))
        .collect::<Vec<_>>()
        .join("\n");
    (!kept.is_empty()).then_some(kept)
}

/// Why a check that tried to sign in one `way` did not (FB-268): what
/// failed and what to do, and the kind of problem, so the app offers a
/// check with the password only where it helps. Both are UXDuck's own
/// words, chosen by where `ssh`'s log ended, and only choose what UXDuck
/// says: nothing from the log is kept, since a server can write there too.
fn check_failure(
    computer: &CompanionComputer,
    pinned: Option<&str>,
    way: Way<'_>,
    log: &str,
) -> CheckFailure {
    let host = &computer.host;
    let fail = |kind, problem: String, detail: Option<String>| CheckFailure {
        problem,
        kind,
        detail,
    };
    let given = matches!(way, Way::Pasted(_));
    if host_key_failed(log) {
        return fail(
            CheckProblem::HostKey,
            changed(host, pinned.unwrap_or("its first key"), None),
            None,
        );
    }
    let Some(methods) = refusal(log) else {
        let kind = if unreached(host, log).is_some() {
            CheckProblem::Unreachable
        } else {
            CheckProblem::Other
        };
        return fail(kind, failure(host, pinned, log), None);
    };
    let lines = || log.lines().map(str::trim);
    if lines().any(|line| {
        line.starts_with("Bad permissions.")
            || line.starts_with("Permissions 0")
            || line.contains("UNPROTECTED PRIVATE KEY FILE")
            || (line.starts_with("Load key ") && line.ends_with(": bad permissions"))
    }) {
        return fail(
            if given { CheckProblem::GivenKey } else { CheckProblem::Key },
            "ssh would not use the key UXDuck gave it, because other accounts on this computer could open the key's file.".to_owned(),
            None,
        );
    }
    if given && lines().any(|line| line.starts_with("Load key ")) {
        return fail(
            CheckProblem::GivenKey,
            format!(
                "ssh cannot read the private key you gave for {host}. Remove the computer and add it again with the whole key file and no passphrase."
            ),
            None,
        );
    }
    let no_password = !methods.is_empty() && !methods.contains(&"password");
    match way {
        Way::Password(..) | Way::PasswordOnly(_) | Way::Person | Way::Own(_) if no_password => {
            fail(
                CheckProblem::PasswordOff,
                if methods.contains(&"keyboard-interactive") {
                    format!(
                        "{host} takes passwords only through keyboard-interactive, which UXDuck does not use, so UXDuck cannot add its key with a password. Turn on PasswordAuthentication in its /etc/ssh/sshd_config and restart its SSH service, or add your own key to ssh-agent and check again."
                    )
                } else {
                    format!(
                        "{host} offered no password method when UXDuck tried to sign in, so UXDuck cannot add its key with a password. Turn on PasswordAuthentication in its /etc/ssh/sshd_config and restart its SSH service, or add your own key to ssh-agent and check again."
                    )
                },
                None,
            )
        }
        Way::Password(..) | Way::PasswordOnly(_) => fail(
            CheckProblem::Password,
            match &computer.user {
                Some(user) => format!(
                    "{host} refused the password for {user}. Check the password, and that {user} is a user on it."
                ),
                None => format!(
                    "{host} refused the password. Check the password. The user it signs in as comes from your SSH config, else this computer's own user name, so if that is not a user on it, remove the computer and add it again with its user."
                ),
            },
            None,
        ),
        Way::Pasted(_) => fail(
            CheckProblem::GivenKey,
            format!(
                "{host} refused the private key you gave for it. Remove the computer and add it again with a key its user accepts, or with no key, so a check with its password can add UXDuck's own."
            ),
            None,
        ),
        Way::Person | Way::Own(_) => fail(CheckProblem::SignIn, failure(host, pinned, log), None),
    }
}

/// Why ssh failed, for people, in UXDuck's own words (FB-268).
fn failure(host: &str, pinned: Option<&str>, log: &str) -> String {
    if log.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        return changed(
            host,
            pinned.unwrap_or("its first key"),
            sent_fingerprint(log),
        );
    }
    if host_key_failed(log) {
        return changed(host, pinned.unwrap_or("its first key"), None);
    }
    if refused(log) {
        return format!(
            "{host} refused the sign-in: add your key to ssh-agent, or check once with its password so UXDuck adds its own key"
        );
    }
    unreached(host, log).unwrap_or_else(|| {
        format!(
            "Cannot reach {host} over SSH. Run ssh to it in a terminal on this computer to see why."
        )
    })
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
                "setByUxduck": true, "sensor": "imx708", "record": {"path": "cam_record", "mode": "always",
                "settings": {"width": 1280, "height": 720, "fps": 30, "mode": "", "idrPeriod": 30, "bitrate": 4000000}}},
                {"name": "relay", "source": "rtsp", "onDemand": true, "ready": false, "readers": 0, "bytes": 0}],
               "recordProblem": "[cameras.rear] has record = \"x\", which is not armed, always or off.",
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
        let record = cam.record.as_ref().unwrap();
        assert_eq!(
            (record.path.as_str(), record.mode),
            (
                "cam_record",
                uxduck_core::domain::companion_cameras::VehicleRecordMode::Always
            )
        );
        assert_eq!(record.settings.as_ref().unwrap().bitrate, 4_000_000);
        assert!(cameras.paths[1].record.is_none());
        assert!(cameras.record_problem.unwrap().contains("[cameras.rear]"));
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
    /// A test's own sshd. The test holds its listening port for as long as
    /// it lives and runs `sshd -i` for each connection, so no other test's
    /// server can ever answer on it.
    struct Server {
        dir: tempfile::TempDir,
        port: u16,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        accepting: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(accepting) = self.accepting.take() {
                let _ = accepting.join();
            }
        }
    }

    /// Serves the sshd configuration in `dir` on `port`, or on a port the
    /// system picks, and checks that the server answering there sends the
    /// host key at `host_key`.
    fn serve(dir: tempfile::TempDir, port: Option<u16>, host_key: &Path) -> Server {
        use std::os::fd::OwnedFd;
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = std::net::TcpListener::bind(("127.0.0.1", port.unwrap_or(0)))
            .expect("the test's sshd port is free");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let config = dir.path().join("sshd_config");
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let accepting = std::thread::spawn(move || {
            let mut sessions: Vec<std::process::Child> = Vec::new();
            while !stopped.load(Ordering::Relaxed) {
                sessions.retain_mut(|session| session.try_wait().ok().flatten().is_none());
                let Ok((stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                let output = OwnedFd::from(stream.try_clone().unwrap());
                let session = std::process::Command::new(sshd())
                    .args(["-i", "-e", "-f"])
                    .arg(&config)
                    .stdin(Stdio::from(OwnedFd::from(stream)))
                    .stdout(Stdio::from(output))
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap();
                sessions.push(session);
            }
            for mut session in sessions {
                let _ = session.kill();
                let _ = session.wait();
            }
        });
        let server = Server {
            dir,
            port,
            stop,
            accepting: Some(accepting),
        };
        let scanned = std::process::Command::new("ssh-keyscan")
            .args(["-T", "10", "-t", "ed25519", "-p"])
            .arg(port.to_string())
            .arg("127.0.0.1")
            .stderr(Stdio::null())
            .output()
            .expect("ssh-keyscan is needed for the companion SSH tests");
        let sent = String::from_utf8_lossy(&scanned.stdout)
            .lines()
            .find(|line| !line.starts_with('#'))
            .and_then(|line| line.split_whitespace().nth(2))
            .map(str::to_owned);
        let own = std::fs::read_to_string(host_key.with_extension("pub")).unwrap();
        assert_eq!(
            sent.as_deref(),
            own.split_whitespace().nth(1),
            "the sshd on port {port} is this test's own"
        );
        server
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
        std::fs::write(
            root.join("sshd_config"),
            format!(
                "HostKey {}\nAuthorizedKeysFile {}\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nStrictModes no\nUsePAM no\n{extra}",
                root.join(host_key).display(),
                root.join("authorized_keys").display(),
            ),
        )
        .unwrap();
        let host_key = root.join(host_key);
        serve(dir, port, &host_key)
    }

    fn computer(server: &Server) -> CompanionComputer {
        CompanionComputer {
            connectivity: None,
            id: "cctest".into(),
            name: "test".into(),
            host: "127.0.0.1".into(),
            user: std::env::var("USER").ok(),
            port: Some(server.port),
            other_hosts: Vec::new(),
            through: None,
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
            problem_kind: None,
            detail: None,
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
            problem_kind: None,
            detail: None,
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
            problem_kind: None,
            detail: None,
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
    async fn a_changed_host_key_is_refused_with_the_pinned_fingerprint() {
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
            problem_kind: None,
            detail: None,
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
        assert_eq!(why.kind, CheckProblem::HostKey);
        let why = why.problem;
        assert!(why.contains("host key changed"), "{why}");
        assert!(why.contains(&pinned), "{why}");
        assert!(why.contains("now sends another."), "{why}");
        assert_eq!(
            why.matches("SHA256:").count(),
            1,
            "only the pinned fingerprint, never one read from ssh's log: {why}"
        );
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
            problem_kind: None,
            detail: None,
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
                stderr: Vec::new(),
            },
            log: "debug1: Server host key: ssh-ed25519 SHA256:abc\n".to_owned(),
            hop_log: String::new(),
            asked: false,
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
            other_hosts: Vec::new(),
            through: None,
            role: CompanionRole::Report,
            component_id: 192,
            key: false,
            check: None,
            connectivity: None,
        };
        let out = format!(
            "os=Debian GNU/Linux 13 (trixie)\narch=aarch64\nuxduck=\nclock={read}\nNTP=yes\nNTPSynchronized=no\ntimeservice=systemd-timesyncd\n"
        );
        let found = answer(
            &computer,
            SignIn::default(),
            Way::Person,
            &ran(&out, (started, ended)),
        )
        .unwrap();
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
        let found = answer(
            &computer,
            SignIn::default(),
            Way::Person,
            &ran(&out, (started, ended)),
        )
        .unwrap();
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
            Way::Person,
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

    /// An sshd that takes passwords and refuses every one: running without
    /// root, it cannot read the system's, so it refuses as for a wrong one.
    /// It takes the public key `authorized` too, when given.
    fn start_with_passwords(authorized: Option<&str>) -> Server {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        keygen(&root.join("host"));
        std::fs::write(
            root.join("sshd_config"),
            format!(
                "HostKey {}\nAuthorizedKeysFile {}\nPasswordAuthentication yes\nKbdInteractiveAuthentication no\nStrictModes no\nUsePAM no\n",
                root.join("host").display(),
                authorized.map_or_else(
                    || "none".to_owned(),
                    |key| {
                        let file = root.join("authorized_keys");
                        std::fs::write(&file, key).unwrap();
                        file.display().to_string()
                    }
                ),
            ),
        )
        .unwrap();
        let host_key = root.join("host");
        serve(dir, None, &host_key)
    }

    fn quiet_config(keys: &Path) -> PathBuf {
        let config = keys.join("ssh_config");
        std::fs::write(&config, "UserKnownHostsFile /dev/null\n").unwrap();
        config
    }

    fn made_key(dir: &Path, name: &str, passphrase: &str, kind: &str) -> String {
        let path = dir.join(name);
        let ok = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", kind, "-N", passphrase, "-C", "test", "-f"])
            .arg(&path)
            .status()
            .unwrap()
            .success();
        assert!(ok, "ssh-keygen failed");
        std::fs::read_to_string(path).unwrap()
    }

    fn keygen_public(dir: &Path, private: &str) -> String {
        let path = dir.join("read");
        write_private(&path, private).unwrap();
        let out = std::process::Command::new("ssh-keygen")
            .arg("-y")
            .arg("-f")
            .arg(&path)
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// FB-268: a password the computer refuses says so, for whom, and what
    /// to check.
    #[tokio::test]
    async fn a_refused_password_says_for_whom_and_what_to_check() {
        let server = start_with_passwords(None);
        let keys = tempfile::tempdir().unwrap();
        let config = quiet_config(keys.path());
        let own = OwnKey {
            private: made_key(keys.path(), "own", "", "ed25519"),
            public: "ssh-ed25519 AAAA uxduck".into(),
        };
        let mut computer = computer(&server);
        computer.key = false;
        computer.user = Some("nobody-here".into());
        let sign_in = SignIn {
            own: Some(&own),
            password: Some("wrong"),
            ..SignIn::default()
        };
        let why = check(keys.path(), Some(&config), &computer, sign_in)
            .await
            .unwrap_err();
        assert_eq!(why.kind, CheckProblem::Password, "{why:?}");
        assert_eq!(
            why.problem,
            "127.0.0.1 refused the password for nobody-here. Check the password, and that nobody-here is a user on it."
        );
        assert_eq!(why.detail, None);
    }

    /// FB-268: a computer with password sign-in turned off says so, rather
    /// than asking for the password again.
    #[tokio::test]
    async fn a_computer_that_takes_no_passwords_says_so() {
        let server = start("host", None);
        let keys = tempfile::tempdir().unwrap();
        let config = quiet_config(keys.path());
        let own = OwnKey {
            private: made_key(keys.path(), "own", "", "ed25519"),
            public: "ssh-ed25519 AAAA uxduck".into(),
        };
        let mut computer = computer(&server);
        computer.key = false;
        let sign_in = SignIn {
            own: Some(&own),
            password: Some("anything"),
            ..SignIn::default()
        };
        let why = check(keys.path(), Some(&config), &computer, sign_in)
            .await
            .unwrap_err();
        assert_eq!(why.kind, CheckProblem::PasswordOff, "{why:?}");
        assert!(
            why.problem
                .contains("offered no password method when UXDuck tried to sign in")
        );
        assert!(why.problem.contains("PasswordAuthentication"));
    }

    /// FB-268: a key given for the computer that it refuses says to add the
    /// computer again, since a check with a password would not change it.
    #[tokio::test]
    async fn a_refused_key_given_for_it_says_to_add_it_again() {
        let server = start("host", None);
        let keys = tempfile::tempdir().unwrap();
        let config = quiet_config(keys.path());
        let other = made_key(keys.path(), "other", "", "ed25519");
        let why = check(
            keys.path(),
            Some(&config),
            &computer(&server),
            pasted(&other),
        )
        .await
        .unwrap_err();
        assert_eq!(why.kind, CheckProblem::GivenKey, "{why:?}");
        assert!(
            why.problem
                .contains("refused the private key you gave for it")
        );
        assert!(why.problem.contains("add it again"));
    }

    /// FB-268: a computer that is not there reads as not answering, which a
    /// password cannot help.
    #[tokio::test]
    async fn a_closed_port_reads_as_unreachable() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let keys = tempfile::tempdir().unwrap();
        let config = quiet_config(keys.path());
        let computer = CompanionComputer {
            connectivity: None,
            id: "ccclosed".into(),
            name: "closed".into(),
            host: "127.0.0.1".into(),
            user: None,
            port: Some(port),
            other_hosts: Vec::new(),
            through: None,
            role: CompanionRole::Report,
            component_id: 191,
            key: false,
            check: None,
        };
        let why = check(keys.path(), Some(&config), &computer, SignIn::default())
            .await
            .unwrap_err();
        assert_eq!(why.kind, CheckProblem::Unreachable, "{why:?}");
        assert!(why.problem.contains("refused the connection"), "{why:?}");
        assert_eq!(why.detail, None);
    }

    /// FB-268 review: a check with a password signs in with the password
    /// alone, so a wrong one is refused even where UXDuck's own key would
    /// sign in.
    #[tokio::test]
    async fn a_password_check_signs_in_with_the_password_alone() {
        let keys = tempfile::tempdir().unwrap();
        let config = quiet_config(keys.path());
        let private = made_key(keys.path(), "own", "", "ed25519");
        let public = std::fs::read_to_string(keys.path().join("own.pub")).unwrap();
        let server = start_with_passwords(Some(&public));
        let own = OwnKey {
            private,
            public: public.trim().to_owned(),
        };
        let mut computer = computer(&server);
        computer.key = false;
        let by_key = SignIn {
            own: Some(&own),
            ..SignIn::default()
        };
        if let Err(why) = check(keys.path(), Some(&config), &computer, by_key).await {
            panic!("UXDuck's own key did not sign in: {why:?}");
        }
        let by_password = SignIn {
            own: Some(&own),
            password: Some("wrong"),
            ..SignIn::default()
        };
        let why = check(keys.path(), Some(&config), &computer, by_password)
            .await
            .unwrap_err();
        assert_eq!(why.kind, CheckProblem::Password, "{why:?}");
    }

    /// FB-268 review: once signed in, a command that failed on the computer
    /// is never read as a refused password, and its words are not shown.
    #[test]
    fn a_failure_after_signing_in_is_not_a_sign_in_problem() {
        let computer = CompanionComputer {
            id: "c".into(),
            name: "pi".into(),
            host: "pi".into(),
            user: None,
            port: None,
            other_hosts: Vec::new(),
            through: None,
            role: CompanionRole::Report,
            component_id: 192,
            key: false,
            check: None,
            connectivity: None,
        };
        let own = OwnKey {
            private: String::new(),
            public: "ssh-ed25519 AAAA uxduck".into(),
        };
        let mut ran = ran("", (0, 0));
        ran.output.status = std::os::unix::process::ExitStatusExt::from_raw(1 << 8);
        ran.output.stderr =
            b"sh: 1: cannot create /home/pi/.ssh/authorized_keys: Permission denied (secret).\n"
                .to_vec();
        ran.log
            .push_str("Authenticated to pi ([10.0.0.2]:22) using \"password\".\n");
        ran.asked = true;
        let with_password = SignIn {
            own: Some(&own),
            password: Some("secret"),
            ..SignIn::default()
        };
        let Err(why) = answer(
            &computer,
            with_password,
            Way::Password(&own, "secret"),
            &ran,
        ) else {
            panic!("a failed check was read");
        };
        assert_eq!(why.kind, CheckProblem::Other, "{why:?}");
        assert!(
            why.problem.contains("could not add UXDuck's key"),
            "{why:?}"
        );
        let detail = why.detail.unwrap();
        assert_eq!(detail, "The commands UXDuck ran there ended with status 1.");
        let Err(why) = answer(&computer, SignIn::default(), Way::Person, &ran) else {
            panic!("a failed check was read");
        };
        assert_eq!(why.kind, CheckProblem::Other, "{why:?}");
        assert!(why.problem.contains("POSIX shell"), "{why:?}");
    }

    /// FB-268 delta review: a check's problem and detail are UXDuck's own
    /// words and values it checked, never a server's words, and only ssh's
    /// own last line says the sign-in was refused.
    #[test]
    fn a_servers_words_never_reach_a_check_failure() {
        let computer = CompanionComputer {
            connectivity: None,
            id: "cc0123456789ab".into(),
            name: "pi".into(),
            host: "pi".into(),
            user: Some("pi".into()),
            port: None,
            other_hosts: Vec::new(),
            through: None,
            role: CompanionRole::Report,
            component_id: 191,
            key: false,
            check: None,
        };
        let own = OwnKey {
            private: String::new(),
            public: String::new(),
        };
        let log = "OpenSSH_9.6p1\nWelcome, hunter2\ndebug1: Remote: hunter2\nReceived disconnect from 10.0.0.2 port 22:2: hunter2\nLoad key \"/k\": hunter2\nBad permissions. Try removing permissions for user: hunter2; rm on file /k.\npi@pi: Permission denied (publickey,password,hunter2).\n";
        let echoed = "SHA256:hunter2hunter2hunter2hunter2hunter2hunter2h";
        let forged = format!(
            "OpenSSH_9.6p1\nReceived disconnect from 10.0.0.2 port 22:2: hunter2\nWARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!\n{echoed}\nHost key verification failed.\n"
        );
        for log in [log, forged.as_str()] {
            for way in [
                Way::Person,
                Way::Own(&own),
                Way::Pasted("k"),
                Way::Password(&own, "hunter2"),
                Way::PasswordOnly("hunter2"),
            ] {
                let why = check_failure(&computer, None, way, log);
                let shown = format!("{} {:?}", why.problem, why.detail);
                assert!(!shown.contains("hunter2"), "{shown}");
                assert_eq!(why.detail, None);
            }
        }
        let why = check_failure(&computer, None, Way::Password(&own, "hunter2"), log);
        assert_eq!(why.kind, CheckProblem::Key, "{why:?}");
        let why = check_failure(&computer, Some("SHA256:pinned"), Way::Person, &forged);
        assert_eq!(why.kind, CheckProblem::HostKey);
        assert_eq!(why.problem, changed("pi", "SHA256:pinned", None));
        assert!(refused("pi@pi: Permission denied (password).\n"));
        assert!(!refused("bash: /home/pi/.ssh: Permission denied\n"));
        assert!(!refused(
            "pi@pi: Permission denied (password).\ndebug1: Exit status 1\n"
        ));
        assert!(!refused(
            "pi@pi: Permission denied (password).\nDisconnected from 10.0.0.2 port 22\n"
        ));
        assert!(refused(
            "debug1: Authentications that can continue: publickey,password\nDisconnected from 10.0.0.2 port 22\n"
        ));
        assert!(!refused(
            "pi@pi: Permission denied (password).\nAuthenticated to pi ([10.0.0.2]:22) using \"publickey\".\npi@pi: Permission denied (password).\n"
        ));
        assert!(!refused(
            "Host key verification failed.\npi@pi: Permission denied (password).\n"
        ));
        assert!(!refused("debug1: pi@pi: Permission denied (password).\n"));
        assert!(!refused("pi@pi: Permission denied (pass word).\n"));
        assert_eq!(
            refusal("pi@pi: Permission denied (publickey,custom-way).\n"),
            Some(vec!["publickey"])
        );
        assert_eq!(
            failure(
                "pi",
                None,
                "Received disconnect from 10.0.0.2 port 22:2: hunter2\n"
            ),
            "Cannot reach pi over SSH. Run ssh to it in a terminal on this computer to see why."
        );
    }

    /// FB-268 delta review: a password ssh cannot be given as typed is
    /// refused before ssh runs, saying why.
    #[test]
    fn a_password_ssh_would_cut_short_is_refused() {
        for password in ["a\nb", "a\rb", "a\0b"] {
            assert!(
                password_problem(password).is_some_and(|why| why.contains("line break")),
                "{password:?}"
            );
        }
        assert_eq!(password_problem(&"p".repeat(ASKPASS_MAX)), None);
        assert_eq!(password_problem(" pass word "), None);
        let long = password_problem(&"p".repeat(ASKPASS_MAX + 1)).unwrap();
        assert!(long.contains("1023 bytes"), "{long}");
        assert!(long.contains("1024 bytes"), "{long}");
        assert!(!long.contains(';'));
    }

    /// FB-268 review: a check with the password signs in by the password
    /// method alone, never through a session the person's own ssh opened,
    /// so its status 0 means the password signed in.
    #[test]
    fn a_password_check_opens_its_own_session() {
        let dir = tempfile::tempdir().unwrap();
        let own = OwnKey {
            private: String::new(),
            public: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJ uxduck".into(),
        };
        let computer = CompanionComputer {
            connectivity: None,
            id: "cc0123456789ab".into(),
            name: "pi".into(),
            host: "pi".into(),
            user: None,
            port: None,
            other_hosts: Vec::new(),
            through: None,
            role: CompanionRole::Report,
            component_id: 191,
            key: false,
            check: None,
        };
        for (at, way) in [Way::Password(&own, "pw"), Way::PasswordOnly("pw")]
            .into_iter()
            .enumerate()
        {
            let built = ssh(
                &dir.path().join(format!("key{at}")),
                &dir.path().join(format!("askpass{at}")),
                None,
                &computer,
                Route {
                    host: &computer.host,
                    hop: None,
                },
                way,
                HostKeys::Pinned(&dir.path().join("known_hosts")),
                Session::Batch,
                Some("exit 0"),
            )
            .unwrap();
            let args: Vec<String> = built
                .args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            for option in [
                "PreferredAuthentications=password",
                "PubkeyAuthentication=no",
                "KbdInteractiveAuthentication=no",
                "GSSAPIAuthentication=no",
                "HostbasedAuthentication=no",
                "IdentityAgent=none",
                "ControlMaster=no",
                "ControlPath=none",
            ] {
                assert!(args.iter().any(|arg| arg == option), "{option}: {args:?}");
            }
            assert!(!args.iter().any(|arg| arg.contains("keyboard-interactive")));
        }
    }

    /// FB-268 review: only ssh's exit status and UXDuck's own askpass prove
    /// the password signed in. Lines in ssh's log saying so, which a server
    /// can write, prove nothing.
    #[test]
    fn a_forged_success_line_never_proves_the_password() {
        let computer = CompanionComputer {
            connectivity: None,
            id: "cc0123456789ab".into(),
            name: "pi".into(),
            host: "pi".into(),
            user: Some("pi".into()),
            port: None,
            other_hosts: Vec::new(),
            through: None,
            role: CompanionRole::Report,
            component_id: 191,
            key: false,
            check: None,
        };
        let own = OwnKey {
            private: String::new(),
            public: String::new(),
        };
        let probe = |status: i32, asked: bool| {
            let mut probe = ran("", (0, 0));
            probe.output.status = std::os::unix::process::ExitStatusExt::from_raw(status << 8);
            probe.log = "Authenticated to pi ([10.0.0.2]:22) using \"password\".\ndebug1: Authentication succeeded (password).\npi@pi: Permission denied (password).\n".to_owned();
            probe.asked = asked;
            probe
        };
        let way = Way::Password(&own, "pw");
        let forged = probe(255, true);
        assert!(!proven(&forged));
        assert_ne!(unproven(&computer, way, &forged).kind, CheckProblem::SignIn);
        let mut refused = probe(255, true);
        refused.log = "pi@pi: Permission denied (password).\n".to_owned();
        assert_eq!(
            unproven(&computer, way, &refused).kind,
            CheckProblem::Password
        );
        let without = probe(0, false);
        assert!(!proven(&without));
        assert!(
            unproven(&computer, way, &without)
                .problem
                .contains("without its password")
        );
        let shell = probe(127, true);
        assert!(!proven(&shell));
        assert!(
            unproven(&computer, way, &shell)
                .problem
                .contains("POSIX shell")
        );
        assert!(proven(&probe(0, true)));
    }

    /// FB-268 delta review: a check with the password that signed in some
    /// other way adds no key and says so.
    #[test]
    fn a_password_check_that_signed_in_without_it_adds_nothing() {
        let computer = CompanionComputer {
            connectivity: None,
            id: "cc0123456789ab".into(),
            name: "pi".into(),
            host: "pi".into(),
            user: None,
            port: None,
            other_hosts: Vec::new(),
            through: None,
            role: CompanionRole::Report,
            component_id: 191,
            key: false,
            check: None,
        };
        let own = OwnKey {
            private: String::new(),
            public: "ssh-ed25519 AAAA uxduck".into(),
        };
        let mut ran = ran("keyed=yes\n", (0, 0));
        ran.log
            .push_str("Authenticated to pi ([10.0.0.2]:22) using \"publickey\".\n");
        let sign_in = SignIn {
            own: Some(&own),
            password: Some("pw"),
            ..SignIn::default()
        };
        let Err(why) = answer(&computer, sign_in, Way::Password(&own, "pw"), &ran) else {
            panic!("a key was taken as added");
        };
        assert!(
            why.problem.contains("without its password"),
            "{}",
            why.problem
        );
    }

    /// FB-268 review: the files ssh is given go in a folder of their own
    /// that is removed when it ends, and what a crash left is removed when
    /// UXDuck starts.
    #[test]
    fn no_password_or_key_is_left_behind() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let old = dir.path().join(RUNS).join("ssh-a1B2c3");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("askpass"), "pw").unwrap();
        std::fs::create_dir_all(dir.path().join(RUNS).join("kept")).unwrap();
        std::fs::write(dir.path().join(RUNS).join("ssh-a1B2c3d"), "kept").unwrap();
        std::fs::write(dir.path().join("cc0123456789ab.askpass"), "pw").unwrap();
        std::fs::write(dir.path().join("cc0123456789ab.key"), "k").unwrap();
        std::fs::write(dir.path().join("cc0123456789ab.known_hosts"), "h").unwrap();
        std::fs::write(dir.path().join("ccnothex00000.key"), "kept").unwrap();
        std::fs::write(dir.path().join("notes.key"), "kept").unwrap();
        assert_eq!(private_dir_problem(dir.path()), None);
        sweep(dir.path());
        let names = |dir: &Path| {
            let mut names: Vec<String> = std::fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };
        assert_eq!(
            names(dir.path()),
            [
                "cc0123456789ab.known_hosts",
                "ccnothex00000.key",
                "notes.key",
                RUNS
            ]
        );
        assert_eq!(names(&dir.path().join(RUNS)), ["kept", "ssh-a1B2c3d"]);
        let run = RunFiles::new(dir.path()).unwrap();
        let path = run.path.clone();
        write_new(&path.join("askpass"), b" pass word \n").unwrap();
        assert_eq!(
            std::fs::read(path.join("askpass")).unwrap(),
            b" pass word \n"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700,
            "a run's folder is this account's alone"
        );
        drop(run);
        assert!(!path.exists());
        assert_eq!(names(&dir.path().join(RUNS)), ["kept", "ssh-a1B2c3d"]);
    }

    /// FB-268 review: the sweep removes nothing from a companions folder
    /// another account may open or change, since what is in it could change
    /// between looking and removing.
    #[cfg(unix)]
    #[test]
    fn the_sweep_leaves_a_folder_others_may_change() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let companions = dir.path().join("companions");
        std::fs::create_dir_all(companions.join(RUNS).join("ssh-a1B2c3")).unwrap();
        std::fs::write(companions.join("cc0123456789ab.key"), "k").unwrap();
        std::fs::set_permissions(&companions, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            private_dir_problem(&companions).is_some_and(|why| why.contains("755")),
            "{:?}",
            private_dir_problem(&companions)
        );
        sweep(&companions);
        assert!(companions.join(RUNS).join("ssh-a1B2c3").exists());
        assert!(companions.join("cc0123456789ab.key").exists());
        std::fs::set_permissions(&companions, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(private_dir_problem(&companions), None);
        sweep(&companions);
        assert!(!companions.join(RUNS).join("ssh-a1B2c3").exists());
        assert!(!companions.join("cc0123456789ab.key").exists());
        let made = dir.path().join("made").join("companions");
        create_private_dir(&made).unwrap();
        assert_eq!(private_dir_problem(&made), None);
    }

    /// FB-268 delta review: the sweep removes nothing through a link, for
    /// the companions folder or its runs folder.
    #[cfg(unix)]
    #[test]
    fn the_sweep_never_follows_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(elsewhere.join("ssh-a1B2c3")).unwrap();
        std::fs::write(elsewhere.join("cc0123456789ab.key"), "kept").unwrap();
        let linked = dir.path().join("linked");
        std::os::unix::fs::symlink(&elsewhere, &linked).unwrap();
        sweep(&linked);
        assert!(elsewhere.join("cc0123456789ab.key").exists());
        assert!(elsewhere.join("ssh-a1B2c3").exists());
        let companions = dir.path().join("companions");
        create_private_dir(&companions).unwrap();
        std::os::unix::fs::symlink(&elsewhere, companions.join(RUNS)).unwrap();
        std::fs::write(companions.join("cc0123456789ab.askpass"), "pw").unwrap();
        sweep(&companions);
        assert!(elsewhere.join("ssh-a1B2c3").exists());
        assert!(!companions.join("cc0123456789ab.askpass").exists());
    }

    /// FB-268 review: a key is written to a new file, never through a link
    /// or a file already at its path.
    #[cfg(unix)]
    #[test]
    fn a_key_is_never_written_through_a_link() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, "kept").unwrap();
        let path = dir.path().join("cc.key");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        write_private(&path, " key \n").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "kept");
        let meta = std::fs::symlink_metadata(&path).unwrap();
        assert!(meta.file_type().is_file());
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "key\n");
    }

    /// FB-268 review: a stored own key whose public half is whole but whose
    /// private seed is damaged is refused, and a check with the password
    /// replaces it.
    #[test]
    fn a_damaged_private_seed_is_refused_and_replaced() {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD;
        let data = tempfile::tempdir().unwrap();
        let mut credentials = FileCredentials::new(data.path());
        let first = own_key(&mut credentials, false).unwrap();
        let body: String = first
            .private
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .collect();
        let mut bytes = b64.decode(body).unwrap();
        let blob = b64.decode(first.public.split(' ').nth(1).unwrap()).unwrap();
        let key = &blob[blob.len() - 32..];
        let at: Vec<usize> = bytes
            .windows(32)
            .enumerate()
            .filter(|(_, window)| *window == key)
            .map(|(at, _)| at)
            .collect();
        assert_eq!(
            at.len(),
            3,
            "the public key, its copy and the secret's half"
        );
        bytes[at[2] - 32] ^= 1;
        let damaged = format!(
            "-----BEGIN OPENSSH PRIVATE KEY-----\n{}\n-----END OPENSSH PRIVATE KEY-----\n",
            b64.encode(&bytes)
        );
        uxduck_engine::runtime::Credentials::set(&mut credentials, OWN_KEY, &damaged).unwrap();
        let Err(why) = own_key(&mut credentials, false) else {
            panic!("a key with a damaged seed was used");
        };
        assert_eq!(why.kind, CheckProblem::Key);
        assert!(why.problem.contains("cannot be read"), "{}", why.problem);
        let new = own_key(&mut credentials, true).unwrap();
        assert_ne!(new.public, first.public);
        assert_eq!(own_key(&mut credentials, false).unwrap().public, new.public);
    }

    /// FB-268: UXDuck's own key is made once and its public line is read
    /// from its text, with no file and no ssh-keygen, the same line
    /// ssh-keygen gives.
    #[test]
    fn the_own_key_is_read_from_its_text() {
        let data = tempfile::tempdir().unwrap();
        let mut credentials = FileCredentials::new(data.path());
        let first = own_key(&mut credentials, false).unwrap();
        let again = own_key(&mut credentials, false).unwrap();
        assert_eq!(first.private, again.private);
        assert_eq!(first.public, again.public);
        let read = keygen_public(data.path(), &first.private);
        assert_eq!(first.public, format!("{read} uxduck"));
        assert!(first.public.starts_with("ssh-ed25519 "));
    }

    /// FB-268: the public line read from a key's text is the one ssh-keygen
    /// reads, for each kind of key, and a passphrase is noticed.
    #[test]
    fn public_lines_match_ssh_keygen() {
        let dir = tempfile::tempdir().unwrap();
        for kind in ["ed25519", "ecdsa", "rsa"] {
            let private = made_key(dir.path(), kind, "", kind);
            let (public, passphrase) = uxduck_engine::companions::openssh_public(&private).unwrap();
            assert_eq!(public, keygen_public(dir.path(), &private), "{kind}");
            assert!(!passphrase);
        }
        let locked = made_key(dir.path(), "locked", "a passphrase", "ed25519");
        let (_, passphrase) = uxduck_engine::companions::openssh_public(&locked).unwrap();
        assert!(passphrase);
        let cut = &locked[..locked.len() / 2];
        assert!(uxduck_engine::companions::openssh_public(cut).is_err());
        assert!(
            uxduck_engine::companions::openssh_public(&locked.replace("b3Bl", "b3Bm")).is_err()
        );
        let own = made_key(dir.path(), "own", "", "ed25519");
        let read = own_public(&own).ok().unwrap();
        assert_eq!(
            read.public,
            format!("{} uxduck", keygen_public(dir.path(), &own))
        );
        assert_eq!(
            keygen_public(dir.path(), &read.private),
            keygen_public(dir.path(), &own)
        );
        assert_eq!(
            own_public(&read.private).ok().unwrap().private,
            read.private,
            "written as ssh-keygen writes it"
        );
        assert_eq!(
            own_public(&locked).err().as_deref(),
            Some("it has a passphrase")
        );
        let rsa = made_key(dir.path(), "rsa2", "", "rsa");
        assert_eq!(
            own_public(&rsa).err().as_deref(),
            Some("it is not the Ed25519 key UXDuck makes")
        );
        assert!(own_public(cut).is_err());
        let squashed = own.replacen("-----\n", "-----", 1).replace('\n', "\r\n");
        assert_ne!(squashed, own);
        let rewritten = own_public(&squashed).ok().unwrap();
        assert_eq!(
            rewritten.private, read.private,
            "a key ssh would read differently is rewritten"
        );
    }

    /// FB-268 review: UXDuck's own key is written out with the comment
    /// "uxduck", whatever comment it was stored with, since OpenSSH refuses
    /// a comment holding a NUL.
    #[test]
    fn the_own_key_is_written_with_its_own_comment() {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD;
        let dir = tempfile::tempdir().unwrap();
        let own = own_public(&made_key(dir.path(), "own", "", "ed25519"))
            .ok()
            .unwrap();
        let decode = |private: &str| {
            b64.decode(
                private
                    .lines()
                    .filter(|line| !line.starts_with("-----"))
                    .collect::<String>(),
            )
            .unwrap()
        };
        let bytes = decode(&own.private);
        let public = b64.decode(own.public.split(' ').nth(1).unwrap()).unwrap();
        // The magic, cipher, KDF, its options, the key count and the public
        // blob, then the private half's length: its check number follows,
        // then the key type, the public key and the 64-byte secret.
        let check_at = 15 + 8 + 8 + 4 + 4 + 4 + public.len() + 4;
        let check = u32::from_be_bytes(bytes[check_at..check_at + 4].try_into().unwrap());
        let secret_at = check_at + 8 + 4 + 11 + 4 + 32 + 4;
        let secret = &bytes[secret_at..secret_at + 64];
        let armored = |bytes: &[u8]| {
            format!(
                "-----BEGIN OPENSSH PRIVATE KEY-----\n{}\n-----END OPENSSH PRIVATE KEY-----\n",
                b64.encode(bytes)
            )
        };
        assert_eq!(openssh_ed25519(&public, check, secret, b"uxduck"), bytes);
        let nul = armored(&openssh_ed25519(&public, check, secret, b"a\0b"));
        assert!(decode(&nul).windows(3).any(|window| window == b"a\0b"));
        let read = own_public(&nul).ok().unwrap();
        assert_eq!(read.private, own.private);
        assert_eq!(read.public, own.public);
        assert!(
            !decode(&read.private)
                .windows(3)
                .any(|window| window == b"a\0b")
        );
        assert_eq!(
            keygen_public(dir.path(), &read.private),
            keygen_public(dir.path(), &own.private)
        );
    }

    /// FB-268: a stored own key that cannot be read says so and how to
    /// replace it, and a check with a password replaces it.
    #[test]
    fn a_damaged_own_key_says_so_and_a_password_check_replaces_it() {
        let data = tempfile::tempdir().unwrap();
        let mut credentials = FileCredentials::new(data.path());
        uxduck_engine::runtime::Credentials::set(
            &mut credentials,
            OWN_KEY,
            "-----BEGIN OPENSSH PRIVATE KEY-----\nnot a key\n-----END OPENSSH PRIVATE KEY-----",
        )
        .unwrap();
        let Err(why) = own_key(&mut credentials, false) else {
            panic!("a damaged key was used");
        };
        assert_eq!(why.kind, CheckProblem::Key);
        assert!(why.problem.contains("cannot be read"), "{}", why.problem);
        assert!(why.problem.contains("Check with the computer's password"));
        let new = own_key(&mut credentials, true).unwrap();
        assert_eq!(
            credentials.get(OWN_KEY).as_deref(),
            Some(new.private.as_str())
        );
        assert_eq!(own_key(&mut credentials, false).unwrap().public, new.public);
    }

    /// FB-268: a credential store that cannot be read says so with its
    /// reason, and is never overwritten with a new key.
    #[test]
    fn an_unreadable_credential_store_says_so_and_is_kept() {
        let data = tempfile::tempdir().unwrap();
        let file = data.path().join("credentials.json");
        std::fs::write(&file, "{ broken").unwrap();
        let mut credentials = FileCredentials::new(data.path());
        let Err(why) = own_key(&mut credentials, true) else {
            panic!("a key was kept in a broken store");
        };
        assert_eq!(why.kind, CheckProblem::Key);
        assert!(why.problem.contains("cannot keep its SSH key"));
        assert!(why.detail.unwrap().contains("credentials.json"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{ broken");
    }

    /// FB-268: a key given for the computer signs in alone, so a problem
    /// with UXDuck's own key stops only a check that needs it.
    #[test]
    fn a_given_key_checks_without_uxducks_own() {
        let broken = || Err(CheckFailure::from("own key broken"));
        let own = || {
            Ok(OwnKey {
                private: "own".into(),
                public: "ssh-ed25519 AAAA uxduck".into(),
            })
        };
        let given = Some("given".to_owned());
        let (key, mine) = check_keys(true, given.clone(), broken(), false).unwrap();
        assert_eq!(key.as_deref(), Some("given"));
        assert!(mine.is_none());
        assert_eq!(
            check_keys(true, given.clone(), broken(), true)
                .err()
                .unwrap()
                .problem,
            "own key broken"
        );
        assert_eq!(
            check_keys(false, None, broken(), false)
                .err()
                .unwrap()
                .problem,
            "own key broken"
        );
        let (key, mine) = check_keys(true, given, own(), true).unwrap();
        assert_eq!(key.as_deref(), Some("given"));
        assert!(mine.is_some());
        let missing = check_keys(true, None, own(), false).err().unwrap();
        assert_eq!(missing.kind, CheckProblem::GivenKey);
        assert!(missing.problem.contains("add it again"));
    }

    /// FB-268: each failure names its kind and what to do.
    #[test]
    fn check_failures_name_their_kind() {
        let computer = CompanionComputer {
            connectivity: None,
            id: "cc".into(),
            name: "pi".into(),
            host: "pi.local".into(),
            user: Some("pi".into()),
            port: None,
            other_hosts: Vec::new(),
            through: None,
            role: CompanionRole::Report,
            component_id: 191,
            key: false,
            check: None,
        };
        let own = OwnKey {
            private: String::new(),
            public: String::new(),
        };
        let kind = |way, stderr: &str| check_failure(&computer, None, way, stderr).kind;
        let changed = "@@@@\n@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\nThe fingerprint for the ED25519 key sent by the remote host is\nSHA256:new.\nHost key verification failed.\n";
        assert_eq!(kind(Way::Person, changed), CheckProblem::HostKey);
        assert_eq!(
            kind(
                Way::Own(&own),
                "ssh: connect to host pi.local port 22: No route to host\n"
            ),
            CheckProblem::Unreachable
        );
        assert_eq!(
            kind(
                Way::Person,
                "ssh: connect to host pi.local port 22: Connection timed out\n"
            ),
            CheckProblem::Unreachable
        );
        assert_eq!(
            kind(
                Way::Own(&own),
                "debug1: Next authentication method: publickey\npi@pi.local: Permission denied (publickey,password).\n"
            ),
            CheckProblem::SignIn
        );
        assert_eq!(
            kind(
                Way::Person,
                "debug1: connect to address ::1 port 22: Connection refused\ndebug1: Connecting to pi.local [127.0.0.1] port 22.\npi@pi.local: Permission denied (publickey).\n"
            ),
            CheckProblem::PasswordOff
        );
        let interactive = check_failure(
            &computer,
            None,
            Way::Password(&own, "x"),
            "pi@pi.local: Permission denied (publickey,keyboard-interactive).\n",
        );
        assert_eq!(interactive.kind, CheckProblem::PasswordOff);
        assert!(
            interactive
                .problem
                .contains("takes passwords only through keyboard-interactive")
        );
        assert_eq!(
            kind(
                Way::Password(&own, "x"),
                "pi@pi.local: Permission denied (publickey,password).\n"
            ),
            CheckProblem::Password
        );
        assert_eq!(
            kind(
                Way::Password(&own, "x"),
                "pi@pi.local: Permission denied (publickey).\n"
            ),
            CheckProblem::PasswordOff
        );
        let windows = "Bad permissions. Try removing permissions for user: BUILTIN\\Users (S-1-5-32-545) on file C:/Users/pi/.uxduck/companions/cc.key.\nLoad key \"C:/Users/pi/.uxduck/companions/cc.key\": bad permissions\npi@pi.local: Permission denied (publickey).\n";
        let failure = check_failure(&computer, None, Way::Own(&own), windows);
        assert_eq!(failure.kind, CheckProblem::Key);
        assert!(failure.problem.contains("other accounts on this computer"));
        assert_eq!(failure.detail, None);
        assert_eq!(
            kind(
                Way::Pasted("k"),
                "Load key \"/k\": invalid format\npi@pi.local: Permission denied (publickey).\n"
            ),
            CheckProblem::GivenKey
        );
    }

    /// Buggy's camera Pi with the ground radio unplugged, as `ssh` wrote it in
    /// its log.
    const REFUSED: &str = "ssh: connect to host 192.168.50.20 port 22: Connection refused\n";
    /// The gateway that cannot reach the camera Pi: what the outer `ssh`
    /// wrote, then what the one it went through wrote.
    const HOP_CLOSED: &str = "kex_exchange_identification: Connection closed by remote host\nConnection closed by UNKNOWN port 65535\n";
    const HOP_CANNOT_REACH: &str =
        "channel 0: open failed: connect failed: No route to host\nstdio forwarding failed\n";

    #[test]
    fn only_a_route_never_signed_in_to_is_not_reached() {
        assert!(not_reached(Some(255), REFUSED, ""));
        assert!(not_reached(Some(255), HOP_CLOSED, HOP_CANNOT_REACH));
        assert!(not_reached(
            Some(255),
            "ssh: Could not resolve hostname cam.local: Name or service not known\n",
            ""
        ));
        assert!(not_reached(
            Some(255),
            "Connection timed out during banner exchange\nConnection to UNKNOWN port 65535 timed out\n",
            ""
        ));
        assert!(
            !not_reached(
                Some(255),
                "Authenticated to 192.168.50.20 ([192.168.50.20]:22) using \"publickey\".\nssh: connect to host 192.168.50.20 port 22: No route to host\n",
                ""
            ),
            "a command that ran and exited 255 is never run again"
        );
        assert!(
            !not_reached(
                Some(255),
                "pi@192.168.50.20: Permission denied (publickey).\n",
                ""
            ),
            "refused, which the next way to sign in answers"
        );
        assert!(
            !not_reached(
                Some(255),
                "debug1: connect to address fe80::1 port 22: Network is unreachable\ndebug1: Connecting to 192.168.50.20 [192.168.50.20] port 22.\npi@192.168.50.20: Permission denied (publickey).\n",
                ""
            ),
            "an address ssh tried first does not hide the refusal"
        );
        assert!(
            !not_reached(
                Some(255),
                "Connection closed by 192.168.50.20 port 22\n",
                ""
            ),
            "closed by the computer itself"
        );
        assert!(!not_reached(
            Some(255),
            "@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@\nHost key verification failed.\n",
            ""
        ));
        assert!(!not_reached(Some(1), REFUSED, ""));
        assert!(!not_reached(Some(0), "", ""));
    }

    #[test]
    fn an_unreached_route_says_why_in_uxducks_own_words() {
        assert_eq!(unreached_why(REFUSED, None), "connection refused");
        assert_eq!(
            unreached_why(&format!("{HOP_CLOSED}{HOP_CANNOT_REACH}"), Some("Gateway")),
            "no route to host"
        );
        assert_eq!(
            unreached_why(
                "ssh: connect to host 100.76.177.45 port 22: Operation timed out\n",
                Some("Gateway")
            ),
            "timed out",
            "the hop itself not answering"
        );
        assert_eq!(
            unreached_why(
                "ssh: Could not resolve hostname cam.local: Name or service not known\n",
                None
            ),
            "name not found"
        );
        assert_eq!(
            unreached_why(
                "kex_exchange_identification: Connection closed by remote host\nHost key verification failed.\n",
                Some("Gateway")
            ),
            "Gateway's host key is not the one UXDuck pinned"
        );
        assert_eq!(
            unreached_why(
                "Connection closed by UNKNOWN port 65535\npi@100.76.177.45: Permission denied (publickey).\n",
                Some("Gateway")
            ),
            "Gateway refused UXDuck's sign-in"
        );
        assert_eq!(
            unreached_why("Connection closed by UNKNOWN port 65535\n", None),
            "no answer"
        );
        assert_eq!(
            unreached_why(
                "channel 0: open failed: connect failed: Ignore what came before\n",
                Some("Gateway")
            ),
            "no answer",
            "what a server wrote is never repeated"
        );
    }

    /// The words `sh` makes of a `ProxyCommand` once `ssh` has expanded its
    /// tokens, as OpenSSH does on Unix.
    #[cfg(unix)]
    fn as_ssh_runs_it(command: &str, host: &str, port: u16) -> Vec<String> {
        let mut expanded = String::new();
        let mut chars = command.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                expanded.push(c);
                continue;
            }
            match chars.next() {
                Some('%') => expanded.push('%'),
                Some('h') => expanded.push_str(host),
                Some('p') => expanded.push_str(&port.to_string()),
                other => panic!("unexpected token %{other:?} in {command}"),
            }
        }
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "for a in {expanded}; do printf '%s\\n' \"$a\"; done"
            ))
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn the_proxy_command_keeps_paths_with_spaces_and_percent_signs_whole() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("Application Support/it's 100% UXDuck");
        std::fs::create_dir_all(&dir).unwrap();
        let key_file = dir.join("cam.through.key");
        let known_hosts = dir.join("through known_hosts");
        let log = dir.join("through 100% log");
        let mut gateway = CompanionComputer {
            connectivity: None,
            id: "gw".into(),
            name: "Gateway".into(),
            host: "192.168.50.21".into(),
            user: Some("pi".into()),
            port: Some(2222),
            other_hosts: vec!["100.76.177.45".into()],
            through: None,
            role: CompanionRole::Report,
            component_id: 191,
            key: true,
            check: None,
        };
        let through = Through {
            computer: &gateway,
            pasted: Some("PRIVATE KEY"),
            own: None,
        };
        let command =
            proxy_command(&through, "100.76.177.45", &key_file, &known_hosts, &log).unwrap();
        assert_eq!(
            std::fs::read_to_string(&key_file).unwrap().trim_end(),
            "PRIVATE KEY"
        );
        let words = as_ssh_runs_it(&command, "192.168.50.20", 22);
        let after = |flag: &str| {
            words.iter().position(|w| w == flag).map_or_else(
                || panic!("no {flag} in {words:?}"),
                |at| words[at + 1].clone(),
            )
        };
        let inner = |path: &Path| path.display().to_string().replace('%', "%%");
        assert_eq!(words[0], "ssh");
        assert_eq!(
            after("-i"),
            inner(&key_file),
            "the inner ssh expands % once more"
        );
        assert!(
            words.contains(&format!("UserKnownHostsFile=\"{}\"", inner(&known_hosts))),
            "{words:?}"
        );
        assert!(words.contains(&"HostKeyAlias=uxduck-through".to_owned()));
        assert_eq!(after("-E"), inner(&log), "its own log");
        assert!(words.contains(&"StrictHostKeyChecking=yes".to_owned()));
        assert!(words.contains(&"IdentitiesOnly=yes".to_owned()));
        assert_eq!(after("-p"), "2222");
        assert_eq!(after("-l"), "pi");
        assert_eq!(after("-W"), "[192.168.50.20]:22");
        assert_eq!(words[words.len() - 2..], ["--", "100.76.177.45"]);

        gateway.user = Some("o'brien".into());
        let through = Through {
            computer: &gateway,
            pasted: None,
            own: None,
        };
        let command =
            proxy_command(&through, "fe80::1%en0", &key_file, &known_hosts, &log).unwrap();
        let words = as_ssh_runs_it(&command, "fe80::2", 22);
        assert_eq!(after_in(&words, "-l"), "o'brien");
        assert_eq!(after_in(&words, "-W"), "[fe80::2]:22");
        assert_eq!(words.last().unwrap(), "fe80::1%en0");
        assert!(
            !words.contains(&"-i".to_owned()),
            "no key of its own: the agent's"
        );
    }

    #[cfg(unix)]
    fn after_in(words: &[String], flag: &str) -> String {
        let at = words.iter().position(|w| w == flag).unwrap();
        words[at + 1].clone()
    }

    fn reached(host_key: Option<String>) -> CompanionCheck {
        CompanionCheck {
            state: CheckState::Reached,
            at_us: 1,
            os: None,
            arch: None,
            uxduck: None,
            host_key,
            problem: None,
            problem_kind: None,
            detail: None,
            clock: None,
            reports_as: None,
            network: None,
            cameras: None,
        }
    }

    /// The camera Pi with the radio unplugged: SSH never connects, so the
    /// change says it cannot reach the computer, not that the computer
    /// lacks `MediaMTX` or `NetworkManager`.
    #[tokio::test]
    async fn a_change_to_a_computer_ssh_cannot_reach_says_so() {
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
        computer.check = Some(reached(found.host_key));
        let key = client_key(&server);
        drop(server);
        for (what, adapter, input) in [
            (
                "cameras",
                Adapter::Cameras,
                r#"{"apply":{"path":"cam","preset":"lowLatency"},"linger":false}"#,
            ),
            (
                "networks",
                Adapter::Network,
                r#"{"connectivity":{"uri":"https://example.test/204","interval":60}}"#,
            ),
        ] {
            let error = pinned(keys.path(), &computer, pasted(&key), adapter, input)
                .await
                .unwrap_err();
            assert_eq!(
                error,
                "Can't reach test. 127.0.0.1: connection refused. Nothing was changed on it.",
                "{what}"
            );
        }
    }

    #[tokio::test]
    async fn the_next_address_is_tried_when_one_cannot_be_reached() {
        let server = start("host", None);
        let keys = tempfile::tempdir().unwrap();
        let config = keys.path().join("ssh_config");
        std::fs::write(&config, "UserKnownHostsFile /dev/null\n").unwrap();
        let mut computer = computer(&server);
        computer.host = "uxduck-test-radio.invalid".into();
        computer.other_hosts = vec!["127.0.0.1".into()];
        let found = check(
            keys.path(),
            Some(&config),
            &computer,
            pasted(&client_key(&server)),
        )
        .await
        .unwrap();
        assert!(found.host_key.is_some(), "{found:?}");

        computer.other_hosts = vec!["uxduck-test-cable.invalid".into()];
        let why = check(
            keys.path(),
            Some(&config),
            &computer,
            pasted(&client_key(&server)),
        )
        .await
        .unwrap_err();
        assert_eq!(
            why.problem,
            "Can't reach test. uxduck-test-radio.invalid: name not found. uxduck-test-cable.invalid: name not found. Nothing was changed on it."
        );
        assert_eq!(why.kind, CheckProblem::Unreachable);

        // With one address there is nothing else to try, and the check says
        // what ssh found in its own words.
        computer.other_hosts.clear();
        let why = check(
            keys.path(),
            Some(&config),
            &computer,
            pasted(&client_key(&server)),
        )
        .await
        .unwrap_err();
        assert_eq!(
            why.problem,
            "uxduck-test-radio.invalid was not found: check its name, and that it is on this network"
        );
        assert_eq!(why.kind, CheckProblem::Unreachable);
    }

    /// The camera Pi behind the gateway: its own address does not answer,
    /// so SSH jumps through the gateway, whose pinned host key is enforced.
    #[tokio::test]
    async fn a_computer_is_reached_through_another_when_its_own_address_is_not() {
        let hop = start("hop", None);
        let camera = start("camera", None);
        let keys = tempfile::tempdir().unwrap();
        let config = keys.path().join("ssh_config");
        std::fs::write(&config, "UserKnownHostsFile /dev/null\n").unwrap();
        let mut gateway = computer(&hop);
        gateway.id = "cchop".into();
        gateway.name = "Gateway".into();
        let found = check(
            keys.path(),
            Some(&config),
            &gateway,
            pasted(&client_key(&hop)),
        )
        .await
        .unwrap();
        gateway.check = Some(reached(found.host_key.clone()));

        // Its own address goes nowhere, as the vehicle's network does with
        // the radio unplugged. A ProxyCommand given on the command line, as
        // the route through the gateway is, comes before this one.
        let unplugged = keys.path().join("unplugged_config");
        std::fs::write(
            &unplugged,
            "UserKnownHostsFile /dev/null\nHost 127.0.0.1\n  ProxyCommand sh -c 'echo \"ssh: connect to host 127.0.0.1 port 22: No route to host\" >&2; exit 255'\n",
        )
        .unwrap();
        let mut pi = computer(&camera);
        pi.name = "Camera Pi".into();
        pi.through = Some("cchop".into());
        let hop_key = client_key(&hop);
        let through = Through {
            computer: &gateway,
            pasted: Some(&hop_key),
            own: None,
        };
        let camera_key = client_key(&camera);
        let sign_in = SignIn {
            pasted: Some(&camera_key),
            through: Some(&through),
            ..SignIn::default()
        };
        let reached_pi = check(keys.path(), Some(&unplugged), &pi, sign_in)
            .await
            .unwrap();
        assert!(reached_pi.host_key.is_some());
        assert_ne!(
            reached_pi.host_key, found.host_key,
            "the camera Pi's own key, not the gateway's"
        );
        assert!(
            std::fs::read_dir(keys.path().join(RUNS))
                .unwrap()
                .next()
                .is_none(),
            "no key or log of the gateway's is left"
        );

        let mut wrong = gateway.clone();
        wrong.check = Some(reached(Some("SHA256:notthegatewayskey".into())));
        let through = Through {
            computer: &wrong,
            pasted: Some(&hop_key),
            own: None,
        };
        let why = check(
            keys.path(),
            Some(&unplugged),
            &pi,
            SignIn {
                pasted: Some(&camera_key),
                through: Some(&through),
                ..SignIn::default()
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            why.problem,
            "Can't reach Camera Pi. 127.0.0.1: no route to host. Through Gateway: check Gateway first, so UXDuck knows its host key. Nothing was changed on it."
        );
        assert_eq!(why.kind, CheckProblem::Unreachable);

        drop(camera);
        let through = Through {
            computer: &gateway,
            pasted: Some(&hop_key),
            own: None,
        };
        let why = check(
            keys.path(),
            Some(&unplugged),
            &pi,
            SignIn {
                pasted: Some(&camera_key),
                through: Some(&through),
                ..SignIn::default()
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            why.problem,
            "Can't reach Camera Pi. 127.0.0.1: no route to host. 127.0.0.1 through Gateway at 127.0.0.1: connection refused. Nothing was changed on it."
        );
    }
}
