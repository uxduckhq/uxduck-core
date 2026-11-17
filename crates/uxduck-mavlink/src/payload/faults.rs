//! Faults a person, a test or an agent switches on in a simulated payload
//! (ADR-0063 Y16), to see what a ground station does with each. A fault is
//! what hardware with that fault says and does on the wire. Nothing else
//! tells a ground station of it.

use uxduck_core::domain::link::{SimFault, SimFaultKind};

use super::Every;
use super::wire::Command;

/// How long a command takes while it is made slow, on the simulation's
/// clock.
pub const SLOW_MS: u64 = 5000;
/// How often it says how far along it is. Well under the second and a half
/// a ground station waits before it sends a command again.
const PROGRESS_MS: u64 = 500;

/// The faults that are on. With none the payload is healthy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each is one fault, on or off"
)]
pub struct Faults {
    /// The camera's component sends no heartbeats and answers nothing, as
    /// one unplugged. An integrated unit is one component, so its gimbal
    /// is silent with it.
    pub camera_silent: bool,
    /// The gimbal's component sends no heartbeats and no attitude, and
    /// takes no targets.
    pub gimbal_silent: bool,
    /// The camera's card is full: its storage and capture messages say no
    /// room is left, and photos and recordings fail. It ends when the card
    /// is formatted.
    pub card_full: bool,
    /// A `MAV_CMD` that the component which has it answers
    /// `MAV_RESULT_DENIED` and does not carry out.
    pub refused: Option<u16>,
    /// A `MAV_CMD` that takes [`SLOW_MS`]: it is answered
    /// `MAV_RESULT_IN_PROGRESS` with its progress, and carried out at the
    /// end.
    pub slow: Option<u16>,
    /// The gimbal's motors have failed. Its attitude status carries
    /// `GIMBAL_DEVICE_ERROR_FLAGS_MOTOR_ERROR` and it stays where it is.
    pub gimbal_error: bool,
    /// The vehicle's autopilot is in primary control of a gimbal the
    /// payload manages itself. It ends when another takes control.
    pub autopilot_holds: bool,
}

impl Faults {
    /// The faults a simulation was asked for (`sim/fault`).
    #[must_use]
    pub fn of(asked: &[SimFault]) -> Self {
        let mut faults = Self::default();
        for fault in asked {
            match fault.kind {
                SimFaultKind::CameraSilent => faults.camera_silent = true,
                SimFaultKind::GimbalSilent => faults.gimbal_silent = true,
                SimFaultKind::CardFull => faults.card_full = true,
                SimFaultKind::CommandRefused => faults.refused = fault.command,
                SimFaultKind::CommandInProgress => faults.slow = fault.command,
                SimFaultKind::GimbalError => faults.gimbal_error = true,
                SimFaultKind::AutopilotHolds => faults.autopilot_holds = true,
            }
        }
        faults
    }

    /// Whether a kind of fault is on.
    #[must_use]
    pub fn has(&self, kind: SimFaultKind) -> bool {
        match kind {
            SimFaultKind::CameraSilent => self.camera_silent,
            SimFaultKind::GimbalSilent => self.gimbal_silent,
            SimFaultKind::CardFull => self.card_full,
            SimFaultKind::CommandRefused => self.refused.is_some(),
            SimFaultKind::CommandInProgress => self.slow.is_some(),
            SimFaultKind::GimbalError => self.gimbal_error,
            SimFaultKind::AutopilotHolds => self.autopilot_holds,
        }
    }
}

/// What a component makes of a command while a command fault is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Met {
    /// Refused, and not carried out.
    Denied,
    /// Being carried out, this many percent done.
    Working(u8),
    /// Asked for afresh while it is being carried out. The guide has a
    /// component answer that it is busy.
    Busy,
}

/// What is due of a command being carried out slowly.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Due {
    Progress(Command, u8),
    /// Its time is up: it is carried out now, and its true result follows.
    Done(Command),
}

#[derive(Clone, Copy, Debug)]
struct Working {
    command: Command,
    since_ms: u64,
    progress: Every,
}

impl Working {
    #[allow(clippy::cast_possible_truncation, reason = "under a hundred")]
    fn percent(&self, now_ms: u64) -> u8 {
        (now_ms.saturating_sub(self.since_ms).min(SLOW_MS) * 100 / SLOW_MS).min(99) as u8
    }
}

/// How one component answers the commands a fault names.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Answers {
    pub refused: Option<u16>,
    pub slow: Option<u16>,
    working: Option<Working>,
}

impl Answers {
    /// Whether a command may be answered by a fault instead of carried out.
    pub fn concerns(&self, command: u16) -> bool {
        self.refused == Some(command)
            || self.slow == Some(command)
            || self.working.is_some_and(|w| w.command.command == command)
    }

    /// What a fault makes of a command the component has, or none to carry
    /// it out as usual.
    pub fn meets(&mut self, command: &Command, now_ms: u64) -> Option<Met> {
        if let Some(working) = &self.working
            && working.command.command == command.command
        {
            // The same sender sending it again, as it does when it missed
            // the answer, is told again how far along it is.
            let again = working.command.from == command.from
                && (command.confirmation > 0 || command.place.is_some());
            return Some(if again {
                Met::Working(working.percent(now_ms))
            } else {
                Met::Busy
            });
        }
        if self.refused == Some(command.command) {
            return Some(Met::Denied);
        }
        if self.slow == Some(command.command) {
            self.working = Some(Working {
                command: *command,
                since_ms: now_ms,
                progress: Every::after(PROGRESS_MS, now_ms),
            });
            return Some(Met::Working(0));
        }
        None
    }

    /// What a command being carried out owes by `now_ms`. A clock that
    /// went back, as an autopilot's does when it restarts, ends it.
    pub fn due(&mut self, now_ms: u64) -> Option<Due> {
        let working = self.working.as_mut()?;
        if now_ms < working.since_ms || now_ms - working.since_ms >= SLOW_MS {
            return self.working.take().map(|w| Due::Done(w.command));
        }
        let percent = working.percent(now_ms);
        working
            .progress
            .due(now_ms)
            .then_some(Due::Progress(working.command, percent))
    }
}
