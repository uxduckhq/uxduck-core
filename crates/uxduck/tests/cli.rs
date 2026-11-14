//! The real binary, spawned.

use std::process::Command;

#[test]
fn plugin_scaffolding_and_tlog_decoding_work_without_a_plugin_host() {
    let temp = tempfile::tempdir().unwrap();
    let plugin = temp.path().join("example");
    let output = Command::new(env!("CARGO_BIN_EXE_uxduck"))
        .args(["plugin", "new", "example", "--typescript", "--dir"])
        .arg(&plugin)
        .output()
        .expect("spawn uxduck");
    assert!(output.status.success(), "{output:?}");
    let output = Command::new(env!("CARGO_BIN_EXE_uxduck"))
        .args(["dev", "decode"])
        .arg(plugin.join("test.tlog"))
        .output()
        .expect("spawn uxduck");
    assert!(output.status.success(), "{output:?}");
    let decoded = String::from_utf8(output.stdout).unwrap();
    assert!(decoded.contains("HEARTBEAT"), "{decoded}");
}

#[cfg(not(feature = "plugins"))]
#[test]
fn decoding_with_a_plugin_explains_the_disabled_feature() {
    let output = Command::new(env!("CARGO_BIN_EXE_uxduck"))
        .args(["dev", "decode", "test.tlog", "--plugin", "example"])
        .output()
        .expect("spawn uxduck");
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(
        error.contains("this build has no plugin support"),
        "{error}"
    );
}

#[test]
fn version_prints_the_crate_version() {
    let version = |flag: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_uxduck"))
            .arg(flag)
            .output()
            .expect("spawn uxduck");
        assert!(output.status.success());
        String::from_utf8(output.stdout).expect("utf-8")
    };
    assert_eq!(
        version("-V").trim(),
        concat!("uxduck ", env!("CARGO_PKG_VERSION"))
    );
    // `--version` adds the build, as the About page's "Copy details" (S87).
    let long = version("--version");
    let lines: Vec<&str> = long.lines().collect();
    assert_eq!(lines[0], concat!("uxduck ", env!("CARGO_PKG_VERSION")));
    assert!(lines[1].starts_with("commit "), "{long}");
    assert!(lines[2].starts_with("built "), "{long}");
    assert_eq!(lines[3], concat!("target ", env!("UXDUCK_TARGET")));
}

#[test]
fn opening_shows_the_daemons_actual_startup_error() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_uxduck"))
        .arg("--data-dir")
        .arg(dir.path())
        .args(["open", "--print"])
        .env("UXDUCK_AGENT_PROVIDER", "startup-regression-invalid")
        .env("UXDUCK_API_URL", "none")
        .output()
        .expect("spawn uxduck");
    assert_eq!(output.status.code(), Some(2));
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("UXDuck stopped while starting"), "{error}");
    assert!(
        error.contains("unknown agent provider startup-regression-invalid"),
        "{error}"
    );
    assert!(!dir.path().join("daemon.json").exists());
}

#[test]
fn opening_explains_a_busy_port_without_touching_its_owner() {
    let dir = tempfile::tempdir().unwrap();
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = busy.local_addr().unwrap().port();
    let output = Command::new(env!("CARGO_BIN_EXE_uxduck"))
        .arg("--data-dir")
        .arg(dir.path())
        .args(["open", "--print"])
        .env("UXDUCK_PORT", port.to_string())
        .env("UXDUCK_AGENT_PROVIDER", "fake")
        .env("UXDUCK_API_URL", "none")
        .env("UXDUCK_TERRAIN_URL", "none")
        .env("UXDUCK_SIM_CATALOGUE_URL", "none")
        .env("UXDUCK_PARAM_CATALOGUE_URL", "none")
        .env("UXDUCK_TRAY", "")
        .output()
        .expect("spawn uxduck");
    assert_eq!(output.status.code(), Some(2));
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(
        error.contains(&format!("cannot listen on localhost:{port}")),
        "{error}"
    );
    assert!(!dir.path().join("daemon.json").exists());
    assert_eq!(busy.local_addr().unwrap().port(), port);
}

#[test]
fn opening_shows_argument_errors_from_the_background_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_uxduck"))
        .arg("--data-dir")
        .arg(dir.path())
        .args(["open", "--print"])
        .env("UXDUCK_PORT", "not-a-port")
        .env("UXDUCK_API_URL", "none")
        .output()
        .expect("spawn uxduck");
    assert_eq!(output.status.code(), Some(2));
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("invalid value 'not-a-port'"), "{error}");
    assert!(error.contains("--port"), "{error}");
    assert!(!dir.path().join("daemon.json").exists());
}

/// `ssh` runs UXDuck as its `SSH_ASKPASS` for a companion computer's
/// password (S296): it prints the check's file, whatever the prompt.
#[test]
fn as_ssh_askpass_it_prints_the_password_file() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("askpass");
    std::fs::write(&file, "raspberry").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_uxduck"))
        .arg("pi@pi-vision.local's password: ")
        .env("UXDUCK_ASKPASS_FILE", &file)
        .output()
        .expect("spawn uxduck");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "raspberry\n");
}

/// A local program that is missing is named, rather than reported as a lost
/// connection to UXDuck: `sim build` without Python (fresh-user test,
/// 2026-11-14).
#[cfg(unix)]
#[test]
fn a_missing_python_is_named_rather_than_a_lost_connection() {
    let temp = tempfile::tempdir().unwrap();
    let checkout = temp.path().join("ardupilot");
    std::fs::create_dir_all(checkout.join("Tools/autotest/pysim")).unwrap();
    std::fs::write(checkout.join("waf"), "").unwrap();
    std::fs::write(checkout.join("Tools/autotest/pysim/vehicleinfo.json"), "{}").unwrap();
    for args in [
        &["init", "-q"][..],
        &["add", "-A"],
        &[
            "-c",
            "user.name=UXDuck test",
            "-c",
            "user.email=test@uxduck.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "An ArduPilot checkout in outline",
        ],
    ] {
        let status = Command::new("git")
            .args(args)
            .current_dir(&checkout)
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?}");
    }
    let output = Command::new(env!("CARGO_BIN_EXE_uxduck"))
        .arg("--data-dir")
        .arg(temp.path().join("data"))
        .args(["sim", "build"])
        .arg(&checkout)
        .arg("--allow-network")
        .env("PYTHON", "uxduck-test-missing-python")
        .output()
        .expect("spawn uxduck");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(
        error.contains("uxduck-test-missing-python not found"),
        "{error}"
    );
    assert!(!error.contains("connection lost"), "{error}");
}

/// A missing recording is named by its path, rather than reported as a lost
/// connection to UXDuck (fresh-user test, 2026-11-14).
#[cfg(feature = "plugins")]
#[test]
fn a_plugin_test_without_its_recording_names_the_missing_file() {
    let temp = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_uxduck"))
        .args(["plugin", "test"])
        .arg(temp.path())
        .output()
        .expect("spawn uxduck");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let error = String::from_utf8(output.stderr).unwrap();
    let missing = temp.path().join("test.tlog");
    assert!(
        error.contains(&format!("cannot read {}", missing.display())),
        "{error}"
    );
    assert!(error.contains("--tlog"), "{error}");
    assert!(!error.contains("connection lost"), "{error}");
}
