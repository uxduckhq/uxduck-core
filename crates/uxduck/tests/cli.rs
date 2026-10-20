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
