use uxduck_core::domain::files as core;

use crate::dto::files as dto;

impl From<core::TransferDirection> for dto::TransferDirection {
    fn from(direction: core::TransferDirection) -> Self {
        match direction {
            core::TransferDirection::Download => Self::Download,
            core::TransferDirection::Upload => Self::Upload,
        }
    }
}

impl From<core::TransferState> for dto::FileTransferState {
    fn from(state: core::TransferState) -> Self {
        match state {
            core::TransferState::Queued => Self::Queued,
            core::TransferState::Running => Self::Running,
            core::TransferState::Paused => Self::Paused,
            core::TransferState::Waiting => Self::Waiting,
            core::TransferState::Failed => Self::Failed,
            core::TransferState::Done => Self::Done,
        }
    }
}

impl From<&core::FileTransfer> for dto::FileTransfer {
    fn from(transfer: &core::FileTransfer) -> Self {
        Self {
            id: transfer.id.clone(),
            vehicle_id: transfer.vehicle_id.clone(),
            direction: transfer.direction.into(),
            path: transfer.path.clone(),
            name: transfer.name.clone(),
            size: transfer.size,
            done: transfer.done,
            state: transfer.state.into(),
            detail: transfer.detail.clone(),
            rate_bps: transfer.rate_bps,
            started_at_us: transfer.started_at_us,
            updated_at_us: transfer.updated_at_us,
        }
    }
}
