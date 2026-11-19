//! A terminal on a vehicle's companion computer (ADR-0065): the daemon's own
//! `ssh` session that a client attaches to. Only that it is open is kept:
//! which computer, who holds it and since when. What is typed and shown is
//! never part of it (T9).

use serde::{Deserialize, Serialize};

/// What the session runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TerminalKind {
    /// What the computer gives someone who signs in, under a
    /// pseudo-terminal, to type into.
    #[default]
    Interactive,
    /// One command with no pseudo-terminal, as `uxduck ssh … -- command`
    /// runs. The command's text is never kept.
    Command,
    /// One command an agent runs with `computer/run` (T14): no
    /// pseudo-terminal, no input and no socket. Its text is kept in its
    /// [`ComputerRun`], and what it prints goes into its answer and nowhere
    /// else.
    Run,
}

/// An open terminal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Terminal {
    pub id: String,
    pub vehicle_id: String,
    pub computer_id: String,
    /// The record of the client holding it (ADR-0005).
    pub client_id: String,
    pub client_name: String,
    pub kind: TerminalKind,
    /// Microseconds since the Unix epoch.
    pub opened_at_us: u64,
}

/// How a terminal ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TerminalEnd {
    /// `ssh` signed in and then exited, with the remote exit status.
    Exited,
    /// `ssh` could not be started or could not sign in.
    Failed,
    /// A client ended it with `terminal/close`.
    Closed,
    /// Its socket or the connection that opened it went away (T13).
    HungUp,
    /// Nothing attached to it with its ticket in time.
    Unclaimed,
    /// UXDuck stopped while it was open.
    Stopped,
    /// An operator ended it from another client (ADR-0065 T7).
    EndedByOperator,
    /// An operator took `terminal` away from the client holding it (T7).
    CapabilityRemoved,
    /// A command an agent ran was still running at its deadline (T14).
    Deadline,
}

impl TerminalEnd {
    /// What the holding client is told when someone else ended its
    /// terminal, in the daemon's own words. None when the terminal ended
    /// for a reason of its own.
    #[must_use]
    pub fn told(self) -> Option<&'static str> {
        match self {
            Self::EndedByOperator => Some("Ended by the operator."),
            Self::CapabilityRemoved => Some(
                "Ended by the operator, who no longer allows a terminal from here without asking.",
            ),
            Self::Exited
            | Self::Failed
            | Self::Closed
            | Self::HungUp
            | Self::Unclaimed
            | Self::Stopped
            | Self::Deadline => None,
        }
    }
}

/// A command an agent ran on a companion computer with `computer/run`
/// (ADR-0065 T14), journaled when it starts and again when it ends: who ran
/// it, where, when, the command itself, how it ended and whether its answer
/// left output out. What the command printed is never part of it (T9).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComputerRun {
    /// Also the id of the session it ran in, which `terminal/close` ends.
    pub id: String,
    pub vehicle_id: String,
    /// The vehicle and the computer as they were named then.
    pub vehicle_name: String,
    pub computer_id: String,
    pub computer_name: String,
    /// The record of the client that ran it (ADR-0005).
    pub client_id: String,
    pub client_name: String,
    /// The command, exactly as it was given.
    pub shell_command: String,
    /// Whether an operator was shown this command and allowed it. False
    /// when it ran under an earlier "Allow for this connection" or "Always
    /// allow", or from an operator's own client.
    pub shown_first: bool,
    /// How long it was given, in seconds.
    pub deadline_s: u64,
    /// Microseconds since the Unix epoch.
    pub started_at_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_us: Option<u64>,
    /// How it ended. None while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<TerminalEnd>,
    /// The command's exit status, when it exited on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_status: Option<i32>,
    /// The bytes of what it printed that its answer left out.
    #[serde(default)]
    pub left_out_bytes: u64,
}

/// The mark put after a newline's own mark, so a command of several lines
/// still reads as lines.
const NEWLINE: char = '\n';

/// A command as a person is shown it before allowing it (T14), with every
/// character they could not otherwise see made visible, so nothing in it
/// can hide: a newline is `⏎` and then the line break, a tab is `⇥`, and a
/// carriage return, an escape, every other control character, a space that
/// is not the plain one, a zero-width or bidirectional-override character,
/// and any other format, separator, private-use or unassigned character is
/// its code point, `⟨U+000D⟩` or `⟨U+202E⟩`, which reads at any size. A
/// character used here as a mark is shown as its code point too when the
/// command holds it, so a mark is never the command's own. Nothing is left
/// out and nothing is shortened.
#[must_use]
pub fn shown(command: &str) -> String {
    let mut out = String::with_capacity(command.len());
    for c in command.chars() {
        match c {
            NEWLINE => out.push_str("⏎\n"),
            '\t' => out.push('⇥'),
            ' ' => out.push(' '),
            c if c.is_control() || hidden(c) => {
                use std::fmt::Write as _;
                let _ = write!(out, "⟨U+{:04X}⟩", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out
}

/// Whether a character that is not a control is shown as its code point:
/// one a person cannot see, cannot tell from a space, or would take for one
/// of `shown`'s marks.
fn hidden(c: char) -> bool {
    matches!(u32::from(c),
        // The no-break space and the soft hyphen.
        0xA0 | 0xAD
        // Combining grapheme joiner, Arabic letter mark and other marks
        // that draw nothing.
        | 0x034F | 0x061C | 0x115F | 0x1160 | 0x17B4 | 0x17B5 | 0x180B..=0x180F
        // Every width of space, zero-width characters, the bidirectional
        // marks, embeddings, overrides and isolates, line and paragraph
        // separators, and invisible operators.
        | 0x1680 | 0x2000..=0x200F | 0x2028..=0x202F | 0x205F..=0x206F
        | 0x3000 | 0x3164 | 0xFE00..=0xFE0F | 0xFEFF | 0xFFA0 | 0xFFF0..=0xFFFF
        // The marks themselves: the tab's arrow, the return symbol and the
        // brackets around a code point.
        | 0x21E5 | 0x23CE | 0x27E8 | 0x27E9
        // Private use, where a font draws what it likes or nothing.
        | 0xE000..=0xF8FF
        // Musical and shorthand format controls, tags and more variation
        // selectors, and the private-use planes.
        | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A | 0xE0000..=0xE0FFF | 0xF0000..=0x0010_FFFF
    )
}

#[cfg(test)]
mod tests {
    use super::shown;

    #[test]
    fn a_plain_command_is_shown_as_it_is() {
        let command = "systemctl status mediamtx --no-pager | head -n 20 && echo 'été ok'";
        assert_eq!(shown(command), command);
    }

    #[test]
    fn nothing_a_person_cannot_see_stays_unseen() {
        assert_eq!(shown("ls\nrm -rf /"), "ls⏎\nrm -rf /");
        assert_eq!(shown("ls\rrm -rf /"), "ls⟨U+000D⟩rm -rf /");
        assert_eq!(shown("ls\r\nrm"), "ls⟨U+000D⟩⏎\nrm");
        assert_eq!(
            shown("a\tb\u{1b}[2Kc\u{7f}\u{0}"),
            "a⇥b⟨U+001B⟩[2Kc⟨U+007F⟩⟨U+0000⟩"
        );
        // Every control character there is, and none is left as it was.
        for control in ('\u{0}'..='\u{1f}').chain('\u{7f}'..='\u{9f}') {
            let seen = shown(&control.to_string());
            assert!(!seen.contains(control) || control == '\n', "{control:?}");
            assert!(!seen.is_empty(), "{control:?}");
        }
        // A right-to-left override, which draws what follows backwards.
        assert_eq!(shown("echo \u{202e}/ fr- mr"), "echo ⟨U+202E⟩/ fr- mr");
        // Zero-width, a no-break space, an isolate, a tag and a C1 control.
        assert_eq!(
            shown("a\u{200b}b\u{a0}c\u{2066}d\u{e0041}e\u{9b}f\u{feff}"),
            "a⟨U+200B⟩b⟨U+00A0⟩c⟨U+2066⟩d⟨U+E0041⟩e⟨U+009B⟩f⟨U+FEFF⟩"
        );
    }

    #[test]
    fn a_mark_is_never_the_commands_own() {
        // A command that holds the marks cannot pass for another.
        assert_eq!(
            shown("echo ⏎ ⇥ ⟨U+202E⟩"),
            "echo ⟨U+23CE⟩ ⟨U+21E5⟩ ⟨U+27E8⟩U+202E⟨U+27E9⟩"
        );
        // So two different commands are never shown the same.
        assert_ne!(shown("a\nb"), shown("a⏎\nb"));
        assert_ne!(shown("a\tb"), shown("a⇥b"));
        assert_ne!(shown("a\rb"), shown("a⟨U+000D⟩b"));
    }

    #[test]
    fn a_long_command_is_shown_whole() {
        let command = format!("echo {} ; rm -rf /", "x".repeat(5000));
        assert_eq!(shown(&command), command);
    }
}
