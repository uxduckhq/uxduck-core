//! Errors to `PostHog` (ADR-0016 O13, S32), under the usage analytics
//! switch like every other event:
//!
//! - a panic hook that writes a `$exception` to the analytics outbox before
//!   the process ends, sent after the next start;
//! - a tracing layer that sends error-level events carrying a `code` field.
//!
//! No message is sent, only the type, a known code and the stack. Frames
//! keep the crate and file; paths outside the workspace are cut to what
//! names the dependency, or to the file name.

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use uxduck_contracts::dto::analytics::{DaemonEvent, ErrorFrame, is_token};

const MAX_FRAMES: usize = 50;

/// Where this workspace's sources were when this binary was built.
fn workspace_root() -> &'static str {
    // `crates/uxduck` is two levels below the root.
    let manifest = env!("CARGO_MANIFEST_DIR");
    manifest
        .strip_suffix("/crates/uxduck")
        .or_else(|| manifest.strip_suffix("\\crates\\uxduck"))
        .unwrap_or(manifest)
}

/// A source path with nothing of the machine it was built on: relative to
/// the workspace, a dependency's `name-version/…`, the standard library's
/// `library/…`, or the file name.
#[must_use]
pub fn clean_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    let path = path.strip_prefix("./").unwrap_or(&path);
    let root = workspace_root().replace('\\', "/");
    if let Some(inside) = path.strip_prefix(&format!("{root}/")) {
        return inside.to_owned();
    }
    if !(path.starts_with('/') || path.chars().nth(1) == Some(':')) {
        return path.to_owned();
    }
    if let Some((_, after)) = path.split_once("/registry/src/") {
        // Skip the index directory; keep `tokio-1.44.0/src/…`.
        if let Some((_, dependency)) = after.split_once('/') {
            return dependency.to_owned();
        }
    }
    if let Some((_, library)) = path.split_once("/library/") {
        return format!("library/{library}");
    }
    path.rsplit('/').next().unwrap_or_default().to_owned()
}

fn is_ours(function: &str) -> bool {
    function.starts_with("uxduck")
}

/// The frames of `std::backtrace::Backtrace`'s text, without the panic
/// machinery itself.
#[must_use]
pub fn backtrace_frames(text: &str) -> Vec<ErrorFrame> {
    let mut frames: Vec<ErrorFrame> = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let Some((number, function)) = line.trim_start().split_once(": ") else {
            continue;
        };
        if !number.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let mut frame = ErrorFrame {
            function: Some(function.trim().to_owned()),
            file: String::new(),
            line: None,
            column: None,
            in_app: is_ours(function.trim()),
        };
        if let Some(at) = lines
            .peek()
            .and_then(|next| next.trim_start().strip_prefix("at "))
        {
            let mut parts = at.rsplitn(3, ':');
            let column = parts.next().and_then(|c| c.parse().ok());
            let number = parts.next().and_then(|l| l.parse().ok());
            if let (Some(column), Some(number), Some(file)) = (column, number, parts.next()) {
                frame.file = clean_path(file);
                frame.line = Some(number);
                frame.column = Some(column);
            }
            lines.next();
        }
        let name = frame.function.as_deref().unwrap_or_default();
        let machinery = [
            "std::backtrace",
            "std::panicking",
            "std::panic::",
            "core::panicking",
            "rust_begin_unwind",
            "__rust",
            "uxduck::errors::",
            "<alloc::boxed::Box<F,A> as core::ops::function::Fn",
        ];
        if machinery.iter().any(|m| name.starts_with(m)) {
            continue;
        }
        frames.push(frame);
        if frames.len() == MAX_FRAMES {
            break;
        }
    }
    frames
}

/// Records every panic as a `$exception` before the default hook prints it.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut frames = Vec::new();
        if let Some(location) = info.location() {
            frames.push(ErrorFrame {
                function: Some("panic".into()),
                file: clean_path(location.file()),
                line: Some(location.line()),
                column: Some(location.column()),
                in_app: !location.file().contains("/registry/src/"),
            });
        }
        let trace = std::backtrace::Backtrace::force_capture().to_string();
        frames.extend(backtrace_frames(&trace));
        frames.truncate(MAX_FRAMES);
        crate::analytics::record_now(DaemonEvent::Exception {
            error_type: "panic".into(),
            code: "panic".into(),
            frames,
        });
        previous(info);
    }));
}

/// Sends error-level events that carry a `code` field, such as
/// `tracing::error!(code = "store-io", %error, "…")`, as `$exception`
/// with the event's place in the source. The message is not sent.
#[derive(Clone, Copy, Debug, Default)]
pub struct ErrorLayer;

struct CodeField(Option<String>);

impl Visit for CodeField {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "code" {
            self.0 = Some(value.to_owned());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "code" {
            self.0 = Some(format!("{value:?}").trim_matches('"').to_owned());
        }
    }
}

impl<S: Subscriber> Layer<S> for ErrorLayer {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let metadata = event.metadata();
        if *metadata.level() != Level::ERROR {
            return;
        }
        let mut code = CodeField(None);
        event.record(&mut code);
        let Some(code) = code.0.filter(|c| is_token(c)) else {
            return;
        };
        let frames = metadata
            .file()
            .map(|file| ErrorFrame {
                function: Some(metadata.target().to_owned()),
                file: clean_path(file),
                line: metadata.line(),
                column: None,
                in_app: true,
            })
            .into_iter()
            .collect();
        crate::analytics::queue_event(DaemonEvent::Exception {
            error_type: "error".into(),
            code,
            frames,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_keep_nothing_of_the_machine() {
        let root = workspace_root();
        assert_eq!(
            clean_path(&format!("{root}/crates/uxduck/src/daemon/mod.rs")),
            "crates/uxduck/src/daemon/mod.rs"
        );
        assert_eq!(
            clean_path("crates/uxduck-engine/src/lib.rs"),
            "crates/uxduck-engine/src/lib.rs"
        );
        assert_eq!(
            clean_path(
                "/home/ana/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/tokio-1.44.0/src/runtime/mod.rs"
            ),
            "tokio-1.44.0/src/runtime/mod.rs"
        );
        assert_eq!(
            clean_path(
                "/rustc/4d91de4e48198da2e33413efdcd9cd2cc0c46688/library/std/src/panicking.rs"
            ),
            "library/std/src/panicking.rs"
        );
        assert_eq!(clean_path("/Users/ana/work/other/thing.rs"), "thing.rs");
        assert_eq!(clean_path(r"C:\Users\ana\thing.rs"), "thing.rs");
    }

    #[test]
    fn backtraces_become_frames_without_the_panic_machinery() {
        let text = "   0: std::backtrace_rs::backtrace::libunwind::trace
             at /rustc/abc/library/std/src/../../backtrace/src/backtrace/libunwind.rs:117:9
   1: std::panicking::begin_panic_handler
             at /rustc/abc/library/std/src/panicking.rs:697:5
   2: uxduck::daemon::run::{{closure}}
             at ./crates/uxduck/src/daemon/mod.rs:188:13
   3: tokio::runtime::task::core::Core<T,S>::poll
             at /home/ana/.cargo/registry/src/index.crates.io-1/tokio-1.44.0/src/runtime/task/core.rs:331:17
   4: main";
        let frames = backtrace_frames(text);
        assert_eq!(frames.len(), 3);
        assert_eq!(
            frames[0].function.as_deref(),
            Some("uxduck::daemon::run::{{closure}}")
        );
        assert_eq!(frames[0].file, "crates/uxduck/src/daemon/mod.rs");
        assert_eq!(frames[0].line, Some(188));
        assert!(frames[0].in_app);
        assert_eq!(frames[1].file, "tokio-1.44.0/src/runtime/task/core.rs");
        assert!(!frames[1].in_app);
        assert_eq!(frames[2].function.as_deref(), Some("main"));
        assert_eq!(frames[2].file, "");
    }
}
