use uxduck_core::domain::terminal as core;

use crate::dto::terminal as dto;

impl From<core::TerminalKind> for dto::TerminalKind {
    fn from(kind: core::TerminalKind) -> Self {
        match kind {
            core::TerminalKind::Interactive => Self::Interactive,
            core::TerminalKind::Command => Self::Command,
            core::TerminalKind::Run => Self::Run,
        }
    }
}

impl From<core::TerminalEnd> for dto::TerminalEnd {
    fn from(end: core::TerminalEnd) -> Self {
        match end {
            core::TerminalEnd::Exited => Self::Exited,
            core::TerminalEnd::Failed => Self::Failed,
            core::TerminalEnd::Closed => Self::Closed,
            core::TerminalEnd::HungUp => Self::HungUp,
            core::TerminalEnd::Unclaimed => Self::Unclaimed,
            core::TerminalEnd::Stopped => Self::Stopped,
            core::TerminalEnd::EndedByOperator => Self::EndedByOperator,
            core::TerminalEnd::CapabilityRemoved => Self::CapabilityRemoved,
            core::TerminalEnd::Deadline => Self::Deadline,
        }
    }
}

impl From<&core::Terminal> for dto::Terminal {
    fn from(terminal: &core::Terminal) -> Self {
        Self {
            id: terminal.id.clone(),
            vehicle_id: terminal.vehicle_id.clone(),
            computer_id: terminal.computer_id.clone(),
            client_id: terminal.client_id.clone(),
            client_name: terminal.client_name.clone(),
            kind: terminal.kind.into(),
            opened_at_us: terminal.opened_at_us,
        }
    }
}

impl From<&core::ComputerRun> for dto::ComputerRun {
    fn from(run: &core::ComputerRun) -> Self {
        Self {
            id: run.id.clone(),
            vehicle_id: run.vehicle_id.clone(),
            vehicle_name: run.vehicle_name.clone(),
            computer_id: run.computer_id.clone(),
            computer_name: run.computer_name.clone(),
            client_id: run.client_id.clone(),
            client_name: run.client_name.clone(),
            shell_command_shown: core::shown(&run.shell_command),
            shown_first: run.shown_first,
            deadline_s: run.deadline_s,
            started_at_us: run.started_at_us,
            ended_at_us: run.ended_at_us,
            end: run.end.map(Into::into),
            exit_status: run.exit_status,
            left_out_bytes: run.left_out_bytes,
        }
    }
}
