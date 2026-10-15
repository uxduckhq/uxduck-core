// The build's stamp, shared by `uxduck` and `uxduck-web` with `include!`
// (S87): the target triple, so `uxduck update check` can pick this
// platform's artifact from the release manifest, and the commit and time
// of the build, for `daemon/about` and `uxduck --version`. Sets
// `UXDUCK_TARGET`, and `UXDUCK_GIT_SHA` and `UXDUCK_BUILT_AT` when known.

use std::process::Command;

fn stamp() {
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=UXDUCK_TARGET={target}");

    // CI names the commit; a checkout asks git. Outside either there is none.
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    let commit = std::env::var("GITHUB_SHA")
        .ok()
        .filter(|sha| !sha.is_empty())
        .map(|sha| sha.chars().take(7).collect::<String>())
        .or_else(|| git(&["rev-parse", "--short=7", "HEAD"]));
    if let Some(commit) = &commit {
        println!("cargo:rustc-env=UXDUCK_GIT_SHA={commit}");
    }
    // A new commit runs this again, so the commit is never stale. Without
    // these lines the script ran once per clean build.
    for path in ["HEAD", "packed-refs"] {
        if let Some(path) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"])
        && let Some(path) = git(&["rev-parse", "--git-path", &branch])
    {
        println!("cargo:rerun-if-changed={path}");
    }

    // The time this script ran: the build's, to the second. A reproducible
    // build sets `SOURCE_DATE_EPOCH` instead.
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    let seconds = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs())
        });
    if let Some(built_at) = seconds.map(rfc3339) {
        println!("cargo:rustc-env=UXDUCK_BUILT_AT={built_at}");
    }
}

/// A command's trimmed output, if it ran and said something.
fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (output.status.success() && !text.is_empty()).then(|| text.to_owned())
}

/// Seconds since the epoch as `2026-10-05T09:30:00Z`, without a date crate.
fn rfc3339(seconds: u64) -> String {
    let days = i64::try_from(seconds / 86_400).unwrap_or_default();
    let rest = seconds % 86_400;
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}
