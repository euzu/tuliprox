//! DVR recording domain types.
//!
//! This module is the source of truth for `RecordingMetadata`; persistence,
//! runtime, and DTO layers all mirror it.

use super::{
    transfer::{TaskPriorityDto, TransferStatusDto},
    user_id::UserId,
};

#[cfg(test)]
mod tests;

mod metadata;
mod request;
mod task;
pub use metadata::{
    AiringStatus, DeletionPreviousState, EpgEpisodeMetadata, NotificationMarker, NotificationMarkerKind,
    RecordingMetadata, RecordingProvenance,
};
pub use request::{
    CreateRecordingRequest, RecordingOwner, RecordingSource, RecordingSourceRequest, RecordingVisibility,
};
pub use task::{
    QueueRevision, RecordingAllowedActions, RecordingKind, RecordingQuotaSummaryDto, RecordingTaskDto,
    RecordingTaskState,
};
