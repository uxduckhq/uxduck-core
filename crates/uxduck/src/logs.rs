//! The daemon's last log lines, kept in memory for a feedback report's
//! bundle (ADR-0016 O11). Logs still go to stderr; this keeps a copy of the
//! last 500 lines, without colour codes, and scrubs them when a report takes
//! them.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::Duration;
use uxduck_contracts::dto::diagnostics::{
    DaemonLogEntry, DaemonLogsParams, DaemonLogsResult, DiagnosticCoverage,
};
use uxduck_engine::runtime::Clock as _;

use tracing_subscriber::fmt::MakeWriter;

const KEPT: usize = 500;
const QUEUED: usize = 256;
const MAX_RECORD_BYTES: usize = 16_384;

enum Console {
    Record(Vec<u8>),
    Flush(mpsc::SyncSender<()>),
}

static NEXT: AtomicU64 = AtomicU64::new(0);
static LOST: AtomicBool = AtomicBool::new(false);
fn lines() -> &'static Mutex<VecDeque<DaemonLogEntry>> {
    static LINES: OnceLock<Mutex<VecDeque<DaemonLogEntry>>> = OnceLock::new();
    LINES.get_or_init(|| Mutex::new(VecDeque::with_capacity(KEPT)))
}

/// The last lines, oldest first, scrubbed.
#[must_use]
pub fn tail() -> Vec<String> {
    lines().lock().map_or_else(
        |_| Vec::new(),
        |kept| {
            kept.iter()
                .map(|line| uxduck_engine::feedback::scrub_text(&line.text))
                .collect()
        },
    )
}

/// Keeps a line, without ANSI colour codes.
fn keep(text: &str) {
    let mut clean = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Skip to the end of the escape sequence, a letter.
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            clean.push(c);
        }
    }
    if let Ok(mut kept) = lines().lock() {
        if kept.len() == KEPT {
            kept.pop_front();
            LOST.store(true, Ordering::Relaxed);
        }
        let mut words = clean.split_whitespace();
        let level = words
            .find(|word| ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"].contains(word))
            .map(str::to_owned);
        let component = level
            .as_ref()
            .and_then(|_| words.next())
            .map(|s| s.trim_end_matches(':').to_owned());
        kept.push_back(DaemonLogEntry {
            id: NEXT.fetch_add(1, Ordering::Relaxed) + 1,
            at_us: crate::adapters::system::SystemClock.now_us(),
            level,
            component,
            text: clean,
        });
    }
}

/// Keeps feedback lines immediately and hands console output to a bounded
/// worker. A stalled terminal may lose console lines, never stall the engine.
#[derive(Clone, Debug)]
pub struct Tee(mpsc::SyncSender<Console>);

impl Tee {
    pub fn new(writer: impl Write + Send + 'static) -> std::io::Result<(Self, LogGuard)> {
        let (sender, receiver) = mpsc::sync_channel(QUEUED);
        std::thread::Builder::new()
            .name("uxduck-console".into())
            .spawn(move || {
                let mut writer = writer;
                while let Ok(command) = receiver.recv() {
                    match command {
                        Console::Record(bytes) => {
                            let _ = writer.write_all(&bytes);
                        }
                        Console::Flush(done) => {
                            let _ = writer.flush();
                            let _ = done.send(());
                        }
                    }
                }
            })?;
        Ok((Self(sender.clone()), LogGuard(sender)))
    }
}

/// Gives normal CLI exit a brief chance to drain logs. A blocked console
/// must not prevent shutdown; no worker join is required.
pub struct LogGuard(mpsc::SyncSender<Console>);

impl Drop for LogGuard {
    fn drop(&mut self) {
        let (done, waiting) = mpsc::sync_channel(1);
        if self.0.try_send(Console::Flush(done)).is_ok() {
            let _ = waiting.recv_timeout(Duration::from_millis(200));
        }
    }
}

pub struct TeeWriter {
    sender: mpsc::SyncSender<Console>,
    bytes: Vec<u8>,
}

impl Write for TeeWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let remaining = MAX_RECORD_BYTES.saturating_sub(self.bytes.len());
        self.bytes
            .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for TeeWriter {
    fn drop(&mut self) {
        for line in String::from_utf8_lossy(&self.bytes).lines() {
            if !line.trim().is_empty() {
                keep(line);
            }
        }
        let _ = self
            .sender
            .try_send(Console::Record(std::mem::take(&mut self.bytes)));
    }
}

impl<'a> MakeWriter<'a> for Tee {
    type Writer = TeeWriter;

    fn make_writer(&'a self) -> Self::Writer {
        TeeWriter {
            sender: self.0.clone(),
            bytes: Vec::new(),
        }
    }
}

/// Bounded, redacted query of current-process daemon logs.
pub fn query(
    params: &DaemonLogsParams,
    now: u64,
) -> Result<DaemonLogsResult, uxduck_contracts::errors::ApiError> {
    static START: OnceLock<u64> = OnceLock::new();
    let prefix = format!(
        "logs:{}",
        START.get_or_init(|| crate::adapters::system::SystemClock.now_us())
    );
    let (from, to, key, before) = crate::diagnostics::bounds(
        &prefix,
        params,
        params.from_us,
        params.to_us,
        params.before.as_deref(),
        now,
    )?;
    let size = crate::diagnostics::limit(params.limit)?;
    let kept = lines().lock().map_err(|_| {
        uxduck_engine::diagnostics::invalid("The daemon log capture is unavailable.")
    })?;
    let cutoff = if let Some(before) = before {
        let id = before
            .parse::<u64>()
            .map_err(|_| uxduck_engine::diagnostics::invalid("Invalid log cursor."))?;
        if !kept.iter().any(|e| e.id == id) {
            return Err(uxduck_engine::diagnostics::invalid(
                "That log cursor is no longer retained.",
            ));
        }
        id
    } else {
        u64::MAX
    };
    let mut found = kept
        .iter()
        .rev()
        .filter(|e| {
            e.id < cutoff
                && e.at_us >= from
                && e.at_us <= to
                && params.level.as_ref().is_none_or(|level| {
                    e.level
                        .as_ref()
                        .is_some_and(|v| v.eq_ignore_ascii_case(level))
                })
                && params.component.as_ref().is_none_or(|component| {
                    e.component.as_ref().is_some_and(|v| v.contains(component))
                })
                && params
                    .query
                    .as_ref()
                    .is_none_or(|query| e.text.contains(query))
        })
        .take(size + 1)
        .cloned()
        .collect::<Vec<_>>();
    let more = found.len() > size;
    found.truncate(size);
    let older = if more {
        found
            .last()
            .map(|e| crate::diagnostics::cursor(&prefix, from, to, &key, &e.id.to_string()))
    } else {
        None
    };
    for e in &mut found {
        e.text = uxduck_engine::feedback::scrub_text(&e.text);
    }
    let coverage = DiagnosticCoverage { requested_from_us: from, requested_to_us: to, available_from_us: kept.front().map(|e| e.at_us), available_to_us: kept.back().map(|e| e.at_us), truncated: LOST.load(Ordering::Relaxed), source: "memory".into(), notes: vec!["Only the last 500 lines of this daemon run are retained. Text is redacted using the feedback scrubber. Timestamps mark capture time.".into()] };
    Ok(DaemonLogsResult {
        entries: found,
        older,
        coverage,
    })
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "deterministic tests"
    )]

    use super::*;

    #[test]
    fn lines_are_kept_without_colour_and_scrubbed() {
        keep("\u{1b}[32m INFO\u{1b}[0m uxduck::daemon: listening on 192.168.1.4:45000");
        let tail = tail();
        assert!(
            tail.iter()
                .any(|line| line == " INFO uxduck::daemon: listening on [address]")
        );
    }

    struct BlockedConsole {
        entered: Option<mpsc::SyncSender<()>>,
        release: mpsc::Receiver<()>,
    }

    impl Write for BlockedConsole {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if let Some(entered) = self.entered.take() {
                entered.send(()).unwrap();
                self.release.recv().unwrap();
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_stalled_console_does_not_block_the_engine_or_feedback_lines() {
        let (entered, waiting) = mpsc::sync_channel(1);
        let (release, blocked) = mpsc::sync_channel(1);
        let (tee, guard) = Tee::new(BlockedConsole {
            entered: Some(entered),
            release: blocked,
        })
        .unwrap();
        writeln!(tee.make_writer(), "first").unwrap();
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        let (finished, done) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            // Fill and exceed the console queue while its writer is stuck.
            for _ in 0..QUEUED + 10 {
                writeln!(tee.make_writer(), "wake-lock transition").unwrap();
            }
            writeln!(tee.make_writer(), "feedback survives a full console queue").unwrap();
            tee.make_writer().flush().unwrap();
            finished.send(()).unwrap();
        });
        let result = done.recv_timeout(Duration::from_secs(2));
        release.send(()).unwrap();
        result.expect("engine logging must finish even with a blocked console");
        assert!(
            tail()
                .iter()
                .any(|line| line == "feedback survives a full console queue")
        );
        drop(guard);
    }

    struct MemoryConsole(std::sync::Arc<Mutex<Vec<u8>>>);

    impl Write for MemoryConsole {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn records_are_bounded_and_normal_shutdown_drains_console_output() {
        let bytes = std::sync::Arc::new(Mutex::new(Vec::new()));
        let (tee, guard) = Tee::new(MemoryConsole(bytes.clone())).unwrap();
        {
            let mut writer = tee.make_writer();
            writer.write_all(b"fragment ").unwrap();
            writer.write_all(&vec![b'x'; MAX_RECORD_BYTES * 2]).unwrap();
        }
        writeln!(tee.make_writer(), "next line").unwrap();
        drop(guard);
        let written = bytes.lock().unwrap();
        assert_eq!(written.len(), MAX_RECORD_BYTES + b"next line\n".len());
        assert!(written.starts_with(b"fragment "));
        assert!(written.ends_with(b"next line\n"));
    }
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;
    #[test]
    fn log_queries_filter_page_and_redact_without_losing_records() {
        keep(" INFO uxduck::diagnostic_test: unique-diagnostic-query first 192.168.1.4");
        keep(" WARN uxduck::diagnostic_test: unique-diagnostic-query second");
        let params = DaemonLogsParams {
            query: Some("unique-diagnostic-query".into()),
            component: Some("diagnostic_test".into()),
            limit: Some(1),
            ..Default::default()
        };
        let now = crate::adapters::system::SystemClock.now_us();
        let page = query(&params, now).unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].level.as_deref(), Some("WARN"));
        let next = query(
            &DaemonLogsParams {
                before: page.older,
                ..params
            },
            now + 1_000_000,
        )
        .unwrap();
        assert_eq!(next.entries.len(), 1);
        assert!(!next.entries[0].text.contains("192.168.1.4"));
        assert!(next.entries[0].text.contains("[address]"));
        assert_eq!(next.coverage.requested_to_us, page.coverage.requested_to_us);
    }
}
