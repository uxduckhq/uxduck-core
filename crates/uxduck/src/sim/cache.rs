//! Development binaries survive removal of Cargo's build output.
use std::path::{Path, PathBuf};

pub fn directories() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    // A cache named by UXDUCK_SIM_CACHE_DIR is the only one: the checkout's
    // legacy builds would otherwise leak into an isolated test.
    match std::env::var_os("UXDUCK_SIM_CACHE_DIR") {
        Some(path) => vec![root.join(path).join("native")],
        None => vec![
            root.join(".cache/simulators/native"),
            root.join("target/sitl/bin"),
        ],
    }
}
