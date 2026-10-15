//! How this executable was built (S87): from `build.rs`, for `daemon/about`
//! and `uxduck --version`.

use std::sync::LazyLock;

/// The commit built from, short; none outside a git checkout and CI.
pub const COMMIT: Option<&str> = option_env!("UXDUCK_GIT_SHA");

/// When it was built, as RFC 3339 in UTC.
pub const BUILT_AT: Option<&str> = option_env!("UXDUCK_BUILT_AT");

/// The target triple.
pub const TARGET: &str = env!("UXDUCK_TARGET");

/// `uxduck --version`: the version, then the details the About page's
/// "Copy details" gives for UXDuck, one per line.
pub fn long_version() -> &'static str {
    static LONG: LazyLock<String> = LazyLock::new(|| {
        format!(
            "{}\ncommit {}\nbuilt {}\ntarget {TARGET}",
            env!("CARGO_PKG_VERSION"),
            COMMIT.unwrap_or("unknown"),
            BUILT_AT.unwrap_or("unknown"),
        )
    });
    &LONG
}
