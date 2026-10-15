//! Development binaries survive removal of Cargo's build output.
use std::path::{Path, PathBuf};

pub fn directories() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let cache = std::env::var_os("UXDUCK_SIM_CACHE_DIR")
        .map_or_else(|| root.join(".cache/simulators"), |path| root.join(path));
    vec![cache.join("native"), root.join("target/sitl/bin")]
}
