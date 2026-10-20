//! Stamps the build (`stamp.rs`), rebuilds when the release key changes,
//! and builds the script host into the executable (`plugins` feature).

include!("stamp.rs");

#[path = "../../packaging/windows/resource.rs"]
mod windows_resource;

use std::path::Path;

fn main() {
    windows_resource::embed("uxduck");
    stamp();
    println!("cargo:rerun-if-env-changed=UXDUCK_RELEASE_PUBLIC_KEY");
    println!("cargo:rerun-if-env-changed=UXDUCK_UPDATE_TEST_URL");
    println!("cargo:rerun-if-env-changed=UXDUCK_UPDATE_TEST_KEY");
    match (
        std::env::var("UXDUCK_UPDATE_TEST_URL").ok(),
        std::env::var("UXDUCK_UPDATE_TEST_KEY").ok(),
    ) {
        (None, None) => {}
        (Some(url), Some(key))
            if !key.trim().is_empty()
                && (url.starts_with("https://") || url.starts_with("http://127.0.0.1:")) => {}
        _ => panic!(
            "isolated updater tests require both UXDUCK_UPDATE_TEST_URL \
             (HTTPS or loopback HTTP) and UXDUCK_UPDATE_TEST_KEY; \
             leave both unset for a production build"
        ),
    }
    if std::env::var_os("CARGO_FEATURE_PLUGINS").is_some() {
        script_host();
    }
}

/// Builds `plugins/script-host` for WebAssembly and leaves it in `OUT_DIR`
/// for `include_bytes!`: every build of UXDuck carries the engine its
/// TypeScript plugins run in, so no installer can leave it behind.
fn script_host() {
    let plugins = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
    for input in [
        "script-host",
        "Cargo.toml",
        "Cargo.lock",
        "../packages/sdk/src/runtime.js",
    ] {
        println!("cargo:rerun-if-changed={}", plugins.join(input).display());
    }
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    // The plugins' own target folder: sharing ours would wait on the lock
    // this build holds. The host's flags are not WebAssembly's.
    let built = Command::new(cargo)
        .args([
            "build",
            "--release",
            "--locked",
            "--target",
            "wasm32-unknown-unknown",
            "-p",
            "script-host",
        ])
        .current_dir(&plugins)
        .env("CARGO_TARGET_DIR", plugins.join("target"))
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_BUILD_TARGET")
        .stdout(std::process::Stdio::null())
        .status();
    match built {
        Ok(status) if status.success() => {}
        Ok(_) => panic!(
            "building the script host (plugins/script-host) for wasm32-unknown-unknown failed; \
             its errors are above. `rustup target add wasm32-unknown-unknown` if the target is missing"
        ),
        Err(error) => panic!("could not run cargo to build the script host: {error}"),
    }
    let module = plugins.join("target/wasm32-unknown-unknown/release/script_host.wasm");
    let Some(out) = std::env::var_os("OUT_DIR") else {
        panic!("cargo sets OUT_DIR for build scripts");
    };
    let out = Path::new(&out).join("script_host.wasm");
    std::fs::copy(&module, &out).unwrap_or_else(|error| panic!("{}: {error}", module.display()));
}
