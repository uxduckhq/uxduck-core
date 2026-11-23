//! Downloads over a throwaway `sshd` of this user's, as a camera
//! computer is reached: segments copied into part files, checked against
//! the computer's SHA-256 and joined, a copy going on from its part file,
//! a segment that does not match copied again, one changed since it was
//! listed refused, and a computer that does not answer tried again.

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use uxduck_core::domain::companion::CompanionComputer;
use uxduck_core::domain::files::{FromComputer, RemoteSegment};
use uxduck_engine::vehicle_recordings::DownloadReport;
use uxduck_media::mp4::{ParameterSets, Sample, fragment, init_segment};
use uxduck_media::nal::Codec;

use super::{Ctx, Download, listed};
use crate::adapters::credentials::FileCredentials;
use crate::daemon::engine_task::EngineHandle;

struct Sshd {
    dir: tempfile::TempDir,
    port: u16,
    child: Child,
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn keygen(path: &Path) {
    let ok = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(path)
        .status()
        .expect("these tests need ssh-keygen")
        .success();
    assert!(ok, "ssh-keygen failed");
}

fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

impl Sshd {
    fn start() -> Self {
        let program = ["/usr/sbin/sshd", "/usr/bin/sshd", "/usr/local/sbin/sshd"]
            .into_iter()
            .map(std::path::PathBuf::from)
            .find(|path| path.exists())
            .expect("these tests need OpenSSH's sshd: install openssh-server");
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        keygen(&root.join("host"));
        keygen(&root.join("client"));
        std::fs::copy(root.join("client.pub"), root.join("authorized_keys")).unwrap();
        let port = free_port();
        std::fs::write(
            root.join("sshd_config"),
            format!(
                "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nAuthorizedKeysFile {}\nPidFile {}\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nStrictModes no\nUsePAM no\n",
                root.join("host").display(),
                root.join("authorized_keys").display(),
                root.join("sshd.pid").display(),
            ),
        )
        .unwrap();
        let log = root.join("sshd.log");
        let mut child = Command::new(program)
            .args(["-D", "-f"])
            .arg(root.join("sshd_config"))
            .arg("-E")
            .arg(&log)
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !root.join("sshd.pid").exists() {
            if child.try_wait().unwrap().is_some() || Instant::now() >= deadline {
                let _ = child.kill();
                panic!(
                    "sshd did not listen on {port}:\n{}",
                    std::fs::read_to_string(&log).unwrap_or_default()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Self { dir, port, child }
    }

    fn fingerprint(&self) -> String {
        let output = Command::new("ssh-keygen")
            .args(["-l", "-E", "sha256", "-f"])
            .arg(self.dir.path().join("host.pub"))
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .nth(1)
            .unwrap()
            .to_owned()
    }
}

fn test_engine() -> uxduck_engine::Engine {
    use crate::adapters::system::{OsRandom, SystemClock};
    use uxduck_engine::runtime::{
        Forget, MemoryCredentials, NoAgent, NoLinks, NoPlugins, NoSystem, Ports,
    };
    use uxduck_engine::{Engine, EngineConfig, HostAbout, Loaded};
    Engine::new(
        EngineConfig {
            daemon_version: "0.1.1".into(),
            capabilities: uxduck_contracts::dto::initialize::DaemonCapabilities::default(),
            ui_url: "http://127.0.0.1:48091".into(),
            checkpoint_every: 1000,
            models: vec![],
            agent_provider: "fake".into(),
            about: HostAbout::default(),
        },
        Box::new(SystemClock),
        Box::new(OsRandom),
        Ports {
            persistence: Box::new(Forget),
            agent: Box::new(NoAgent),
            credentials: Box::new(MemoryCredentials::default()),
            links: Box::new(NoLinks),
            system: Box::new(NoSystem::default()),
            plugins: Box::new(NoPlugins),
        },
        Loaded::default(),
    )
}

const SPS: [u8; 26] = [
    0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xbb, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00,
    0x10, 0x00, 0x00, 0x03, 0x03, 0x20, 0xf1, 0x83, 0x19, 0x60,
];

/// One second of 90 kHz pictures as `MediaMTX` writes a segment: its own
/// init segment, then fragments counting from its own start.
fn segment(seed: u8) -> Vec<u8> {
    let key: Vec<u8> = [
        &[0, 0, 0, 1][..],
        &SPS,
        &[0, 0, 0, 1, 0x68, 0xeb, 0xe3],
        &[0, 0, 0, 1, 0x65],
        &vec![seed; 4000],
    ]
    .concat();
    let sets = ParameterSets::of(Codec::H264, &key);
    let picture = Sample {
        annex_b: &key,
        duration: 45_000,
        key: true,
        composition_offset: 0,
    };
    [
        init_segment(Codec::H264, &sets, 1280, 720),
        fragment(1, 0, std::slice::from_ref(&picture)),
        fragment(2, 45_000, &[picture]),
    ]
    .concat()
}

/// A camera computer that is this user's `sshd`, with two segments of
/// a recording in its folder, and a download of them.
struct Rig {
    _sshd: Sshd,
    data: tempfile::TempDir,
    remote: tempfile::TempDir,
    segments: Vec<Vec<u8>>,
    download: Download,
    reports: tokio::sync::mpsc::UnboundedReceiver<DownloadReport>,
    _stop: tokio::sync::watch::Sender<bool>,
}

fn computer(port: u16, pinned: &str) -> CompanionComputer {
    serde_json::from_value(serde_json::json!({
        "id": "cmp-pi",
        "name": "Camera Pi",
        "host": "127.0.0.1",
        "port": port,
        "user": std::env::var("USER").ok(),
        "componentId": 191,
        "key": true,
        "check": { "state": "reached", "atUs": 0, "hostKey": pinned },
    }))
    .unwrap()
}

impl Rig {
    /// With `listed` changing each segment's size as listed, and no
    /// `sshd` when `answers` is false.
    fn new(answers: bool, listed: impl Fn(usize, u64) -> u64) -> Self {
        let data = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        let segments = vec![segment(1), segment(2)];
        let paths: Vec<RemoteSegment> = segments
            .iter()
            .enumerate()
            .map(|(i, bytes)| {
                // A space and a quote, as a folder may have.
                let path = remote.path().join(format!("front's record {i}.mp4"));
                std::fs::write(&path, bytes).unwrap();
                RemoteSegment {
                    path: path.display().to_string(),
                    size: listed(i, bytes.len() as u64),
                }
            })
            .collect();
        let (sshd, computer) = {
            let sshd = Sshd::start();
            let mut credentials = FileCredentials::new(data.path());
            // Nothing listens at a computer that does not answer.
            let port = if answers { sshd.port } else { free_port() };
            let computer = computer(port, &sshd.fingerprint());
            uxduck_engine::runtime::Credentials::set(
                &mut credentials,
                &computer.key_name(),
                &std::fs::read_to_string(sshd.dir.path().join("client")).unwrap(),
            )
            .unwrap();
            let companions = data.path().join("companions");
            std::fs::create_dir_all(&companions).unwrap();
            std::fs::write(
                companions.join("cmp-pi.known_hosts"),
                format!(
                    "127.0.0.1 {}",
                    std::fs::read_to_string(sshd.dir.path().join("host.pub")).unwrap()
                ),
            )
            .unwrap();
            (sshd, computer)
        };
        let (reports_to, reports) = tokio::sync::mpsc::unbounded_channel();
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let download = Download {
            ctx: Ok(Ctx {
                dir: data.path().join("companions"),
                program: crate::companions::ssh_program().expect("these tests need ssh"),
                data_dir: data.path().to_owned(),
            }),
            parts: data.path().join("vehicle-recordings").join("fd1"),
            media: data.path().join("media"),
            engine: EngineHandle::spawn(test_engine()),
            reports: reports_to,
            computer,
            through: None,
            from: FromComputer {
                computer_id: "cmp-pi".into(),
                camera: "front".into(),
                recording: "front_record/2026-11-22_12-40-00-000000.mp4".into(),
                started_at_us: 1_795_351_200_000_000,
                duration_us: 2_000_000,
                segments: paths,
                stem: "2026-11-22 12.40.00 Plane - front".into(),
                mode: None,
                over_metered: true,
                waits_for_unmetered: false,
            },
            stopped,
        };
        Self {
            _sshd: sshd,
            data,
            remote,
            segments,
            download,
            reports,
            _stop: stop,
        }
    }

    /// Runs the download until it ends or waits `waits` times: what it
    /// said.
    async fn run(self, waits: usize) -> (Vec<DownloadReport>, tempfile::TempDir) {
        let Self {
            data,
            download,
            mut reports,
            ..
        } = self;
        let task = tokio::spawn(download.run());
        let mut said = Vec::new();
        let mut waited = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            let report = tokio::time::timeout_at(deadline, reports.recv())
                .await
                .expect("the download said nothing for 60 s");
            let Some(report) = report else { break };
            let ends = matches!(
                report,
                DownloadReport::Kept(_) | DownloadReport::Failed { .. }
            );
            waited += usize::from(matches!(report, DownloadReport::Waiting { .. }));
            said.push(report);
            if ends || waited > waits {
                break;
            }
        }
        task.abort();
        (said, data)
    }
}

fn kept(said: &[DownloadReport]) -> &uxduck_engine::vehicle_recordings::KeptRecording {
    said.iter()
        .find_map(|r| match r {
            DownloadReport::Kept(kept) => Some(kept),
            _ => None,
        })
        .unwrap_or_else(|| panic!("not kept: {said:?}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recording_is_copied_checked_and_joined_into_one_file() {
    let rig = Rig::new(true, |_, size| size);
    let total: u64 = rig.segments.iter().map(|s| s.len() as u64).sum();
    let _remote = rig.remote.path().to_owned();
    let (said, data) = rig.run(0).await;
    let kept = kept(&said);
    assert_eq!(kept.file_name, "2026-11-22 12.40.00 Plane - front.mp4");
    let joined = std::fs::read(data.path().join("media").join(&kept.file_name)).unwrap();
    assert_eq!(kept.bytes, joined.len() as u64);
    assert_eq!(
        kept.duration_us,
        Some(2_000_000),
        "two seconds, one after the other"
    );
    assert!(
        said.contains(&DownloadReport::Progress { done: total }),
        "{said:?}"
    );
    assert!(
        !data.path().join("vehicle-recordings").join("fd1").exists(),
        "the part files go once it is kept"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_copy_goes_on_from_its_part_file_and_a_segment_that_does_not_match_is_copied_again() {
    let rig = Rig::new(true, |_, size| size);
    // What arrived before the link dropped, but not what the computer
    // has: the copy goes on after it, so the segment does not match.
    let parts = rig.download.parts.clone();
    std::fs::create_dir_all(&parts).unwrap();
    std::fs::write(parts.join("0.part"), vec![0u8; 1000]).unwrap();
    let (said, data) = rig.run(1).await;
    assert_eq!(said[0], DownloadReport::Progress { done: 1000 }, "{said:?}");
    assert!(
        said.iter().any(|r| matches!(r, DownloadReport::Waiting { detail, metered: false, .. }
            if detail == "front's record 0.mp4 did not match Camera Pi's copy, so UXDuck is downloading it again.")),
        "{said:?}"
    );
    let kept = kept(&said);
    assert!(data.path().join("media").join(&kept.file_name).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_segment_changed_since_it_was_listed_is_not_kept() {
    let rig = Rig::new(true, |i, size| if i == 1 { size - 100 } else { size });
    let (said, data) = rig.run(0).await;
    assert_eq!(
        said.last(),
        Some(&DownloadReport::Failed {
            detail: "front's record 1.mp4 changed on Camera Pi since it was listed. List the recordings again and download it.".into()
        }),
        "{said:?}"
    );
    assert!(
        !data.path().join("media").exists(),
        "nothing partial is kept"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_computer_that_does_not_answer_is_tried_again() {
    let rig = Rig::new(false, |_, size| size);
    let (said, _) = rig.run(0).await;
    let Some(DownloadReport::Waiting {
        detail,
        done: 0,
        metered: false,
    }) = said.last()
    else {
        panic!("{said:?}")
    };
    assert!(detail.ends_with("Trying again in 5 s."), "{detail}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_listing_runs_the_camera_adapter_and_says_why_it_failed() {
    let rig = Rig::new(true, |_, size| size);
    let ctx = rig.download.ctx.clone().unwrap();
    // No systemd here: the adapter's own reason, never "Stopped."
    let error = super::list(&ctx, &rig.download.computer, None)
        .await
        .unwrap_err();
    assert_ne!(error, "Stopped.");
    assert!(!error.is_empty());
}

#[test]
fn the_camera_adapters_listing_reads_as_the_engine_takes_it() {
    let cameras = listed(
        r#"{"cameras":[{"camera":"front","path":"front_record","mode":"armed","folder":"/var/lib/uxduck/recordings/front_record","recordings":[
            {"id":"front_record/a.mp4","startedAtUs":1,"durationUs":2,"segments":[{"path":"/r/a.mp4","size":3}],"recording":true},
            {"id":"front_record/b.ts","startedAtUs":4,"durationUs":5,"segments":[{"path":"/r/b.ts","size":6}],"notDownloadable":"MPEG-TS"}]},
            {"camera":"down","path":"down_record","mode":"always","problem":"cannot read"}]}"#,
    )
    .unwrap();
    assert_eq!(cameras.len(), 2);
    assert!(cameras[0].recordings[0].recording);
    assert_eq!(
        cameras[0].recordings[1].not_downloadable.as_deref(),
        Some("MPEG-TS")
    );
    assert_eq!(cameras[1].problem.as_deref(), Some("cannot read"));
    assert_eq!(
        listed(r#"{"cameras":[],"problem":"MediaMTX's API is off."}"#),
        Err("MediaMTX's API is off.".to_owned())
    );
}
