//! A simulated vehicle's build that stops by itself is reported on its
//! link, once and for good, while builds stopped on purpose are not
//! (ADR-0011). A shell script stands in for SITL.
use super::*;

/// A stand-in `arducopter`: it describes itself as SITL does; without
/// `keep` it prints a line and exits with 3; with it, it holds SITL's port
/// for its instance, as SITL does, until it is killed, leaving its process
/// id in `pid`. Holding the port keeps other tests' simulators off it; the
/// one it replaces may still hold it for a moment.
fn stand_in(folder: &Path) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;

    let program = folder.join("arducopter");
    let keep = folder.join("keep-running");
    let pid = folder.join("pid");
    let script = r#"#!/bin/sh
if [ "$1" = "--list-models" ]; then
  echo '{"ArduCopter":{"default_frame":"quad","frames":{"quad":{"waf_target":"bin/arducopter"}}}}'
  exit 0
fi
if [ ! -f 'KEEP' ]; then echo 'Unable to open the SITL port'; exit 3; fi
for a in "$@"; do case "$a" in --instance=*) i="${a#--instance=}";; esac; done
echo 'Waiting for connection'
echo $$ > 'PID.tmp'; mv 'PID.tmp' 'PID'
exec perl -MIO::Socket::INET -e 'until ($s = IO::Socket::INET->new(LocalAddr => "127.0.0.1", LocalPort => $ARGV[0], Listen => 1, ReuseAddr => 1)) { select(undef, undef, undef, 0.1) } while ($c = $s->accept) { sleep 600 }' $((5760 + 10 * i))
"#
    .replace("KEEP", &keep.display().to_string())
    .replace("PID", &pid.display().to_string());
    std::fs::write(&program, script).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    (program, keep, pid)
}

fn link(daemon: &Daemon, id: &str) -> Option<Value> {
    stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "state/subscribe",
        "--json",
        r#"{"entities":["link"]}"#,
    ]))["snapshot"]["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["id"] == id)
        .cloned()
}

fn detail(daemon: &Daemon, id: &str) -> String {
    link(daemon, id)
        .and_then(|l| l["statusDetail"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn up(daemon: &Daemon, id: &str) -> bool {
    link(daemon, id).is_some_and(|l| l["status"] == "up")
}

/// A new process id in `pid`, removed once read.
fn take_pid(pid: &Path) -> String {
    wait_for("the stand-in build", || pid.exists());
    let id = std::fs::read_to_string(pid).unwrap().trim().to_owned();
    std::fs::remove_file(pid).unwrap();
    id
}

#[test]
fn a_simulator_that_stops_by_itself_is_reported_and_one_stopped_on_purpose_is_not() {
    let folder = tempfile::tempdir().unwrap();
    let (program, keep, pid) = stand_in(folder.path());
    let daemon = Daemon::start(&[]);
    let started = stdout_json(&daemon.cli(&[
        "sim",
        "start",
        "--binary",
        program.to_str().unwrap(),
        "--frame",
        "quad",
    ]));
    let id = started["id"].as_str().unwrap().to_owned();

    // It exits at once: never connected, so it did not start.
    let did_not_start = "The simulator did not start (exit code 3): Unable to open the SITL port";
    wait_for("the build's exit on its link", || {
        detail(&daemon, &id) == did_not_start
    });
    assert_eq!(link(&daemon, &id).unwrap()["status"], "down");
    // The connection no longer retries, so no refusal replaces it: the
    // retries come after 1 and 2 seconds.
    std::thread::sleep(Duration::from_millis(3500));
    assert_eq!(detail(&daemon, &id), did_not_start);

    // Started again, it is watched again; stopped on purpose by a restart,
    // the build that was running is not reported.
    std::fs::write(&keep, "").unwrap();
    rpc(&daemon, "sim/restart", &json!({ "id": id }));
    take_pid(&pid);
    wait_for("the restarted build's connection", || up(&daemon, &id));
    rpc(&daemon, "sim/restart", &json!({ "id": id }));
    let running = take_pid(&pid);
    wait_for("the build started again", || up(&daemon, &id));
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let now = detail(&daemon, &id);
        assert!(!now.contains("simulator"), "a restart was reported: {now}");
        std::thread::sleep(Duration::from_millis(100));
    }

    // Killed once connected, it stopped, numbered as a shell numbers it.
    assert!(
        Command::new("kill")
            .args(["-9", &running])
            .status()
            .unwrap()
            .success()
    );
    let stopped = "The simulator stopped (exit code 137): Waiting for connection";
    wait_for("the killed build on its link", || {
        detail(&daemon, &id) == stopped
    });
    std::thread::sleep(Duration::from_millis(3500));
    assert_eq!(detail(&daemon, &id), stopped);

    // Stopped on purpose, the link goes with its build, and nothing is left.
    rpc(&daemon, "sim/restart", &json!({ "id": id }));
    let running = take_pid(&pid);
    assert!(daemon.cli(&["sim", "stop", &id]).status.success());
    wait_for("the stopped build to end", || {
        !Command::new("kill")
            .args(["-0", &running])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    });
    assert!(link(&daemon, &id).is_none());
}
