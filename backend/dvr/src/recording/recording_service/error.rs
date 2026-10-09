use crate::{recording_deletion::DeletionError, recording_edit::EditError};

/// Stable service-layer errors. The HTTP layer maps each variant to a
/// stable status; the frontend maps each variant to a localized
/// message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceError {
    /// The recording owner could not be resolved from the authenticated
    /// claims (`subject_id` missing or invalid).
    UnknownOwner,
    /// The supplied `RecordingSource` does not match a configured
    /// target/input combination.
    InvalidSource,
    /// Caller asked for an action the principal cannot perform.
    Forbidden,
    /// Caller asked for shared creation but is not an administrator.
    SharedCreationNotAdministrator,
    /// Caller asked to act on a recording that is in an ineligible
    /// state (e.g., edit on a recording marked `Deleting`).
    InvalidState,
    /// `program_end - program_start` overflows or is non-positive.
    InvalidInterval,
    /// Requested padding exceeds the configured recording maximum.
    PaddingLimitExceeded,
    /// uuid not in the queue.
    UnknownRecording,
    /// `mutate`'s persist step failed and the in-memory state was
    /// kept unchanged.
    PersistenceFailed,
    /// IO error during physical deletion.
    IoError(String),
    /// Configured recording quota would be exceeded.
    QuotaExceeded,
    /// The patch would clear `rule_id` / `occurrence_key`. Both are
    /// immutable provenance; surfacing this as `InvalidState` hid the
    /// real reason from the client.
    ProvenanceImmutable,
    /// Caller tried to create a recording that already exists in the
    /// queue (same target / window). Distinct from `InvalidState` so
    /// the client can render a specific "duplicate" message.
    Duplicate,
    /// The recording's filesystem path is not within the configured
    /// storage root, or otherwise violates the path policy.
    InvalidPath,
    /// The recording cannot fit on disk; reservation would exceed
    /// available space.
    DiskFull,
    /// The server has no download engine: the `video.recording` block
    /// is missing from the configuration. Distinct from
    /// `InvalidSource` because the caller's identifiers may be
    /// perfectly valid — nothing on the server can execute them.
    Disabled,
    /// This exact request was already accepted under this idempotency key.
    /// Not a failure: the caller gets the original outcome.
    IdempotentReplay { recording_id: String },
    /// Same idempotency key, different request body.
    IdempotencyConflict,
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.code()) }
}

impl std::error::Error for ServiceError {}

impl ServiceError {
    /// Stable wire-level code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownOwner => "recording_unknown_owner",
            Self::InvalidSource => "recording_invalid_source",
            Self::Forbidden => "recording_forbidden",
            Self::SharedCreationNotAdministrator => "recording_shared_not_administrator",
            Self::InvalidState => "recording_invalid_state",
            Self::InvalidInterval => "recording_invalid_interval",
            Self::PaddingLimitExceeded => "recording_padding_limit_exceeded",
            Self::UnknownRecording => "recording_unknown",
            Self::PersistenceFailed => "recording_persistence_failed",
            Self::IoError(_) => "recording_io_error",
            Self::QuotaExceeded => "recording_quota_exceeded",
            Self::ProvenanceImmutable => "recording_provenance_immutable",
            Self::Duplicate => "recording_duplicate",
            Self::InvalidPath => "recording_invalid_path",
            Self::DiskFull => "recording_disk_full",
            Self::Disabled => "recording_disabled",
            Self::IdempotentReplay { .. } => "recording_idempotent_replay",
            Self::IdempotencyConflict => "recording_idempotency_conflict",
        }
    }
}

pub(super) fn map_edit_validation_error(error: &EditError) -> ServiceError {
    match error {
        EditError::InvalidInterval => ServiceError::InvalidInterval,
        EditError::PaddingLimitExceeded => ServiceError::PaddingLimitExceeded,
        EditError::ProvenanceCleared => ServiceError::ProvenanceImmutable,
        EditError::StateNotEditable | EditError::ChannelChangedWithoutProgramme => ServiceError::InvalidState,
    }
}

pub(super) fn map_deletion_error(error: DeletionError) -> ServiceError {
    match error {
        DeletionError::Forbidden => ServiceError::Forbidden,
        DeletionError::NotTerminal => ServiceError::InvalidState,
        DeletionError::UnknownTask | DeletionError::NotARecording => ServiceError::UnknownRecording,
        DeletionError::DeleteFailed(err) => ServiceError::IoError(err.to_string()),
        DeletionError::BeginFailed(err) | DeletionError::FinalizeFailed(err) => {
            if err.source_io().is_some() {
                ServiceError::PersistenceFailed
            } else {
                ServiceError::UnknownRecording
            }
        }
    }
}
