//! Stable recording errors and security revalidation.
//!
//! - Implement stable mappings for at least the 18 errors in the
//!   stable-wire-code table.
//! - Reauthorize immediately before every metadata conversion
//!   and file open.
//! - Re-resolve source immediately before worker execution.
//! - Revalidate state after any external filesystem measurement
//!   before committing a mutation.
//! - Ensure no handler logs-and-ignores persistence failure.
//! - Redact private title, channel, filename, user ID and rule
//!   metadata from non-owner-facing logs.
//! - Security tests for path traversal, symlink swaps, source
//!   tampering, stale claims, foreign private access, forged
//!   visibility and event leakage.
//!
//! Most stable codes already live in
//! `recording_service::ServiceError`. This module adds:
//! - The wire-code map.
//! - Path / source / state revalidation guards the queue-mutation
//!   boundary runs before committing.
//! - Log-redaction helpers that strip private metadata.

use shared::model::{
    recording::{RecordingMetadata, RecordingOwner, RecordingVisibility},
    UserId,
};

/// The full stable-wire-code table. At least 18 codes are required;
/// the existing `ServiceError` already exposes the runtime
/// variants. This enum is the wire-side surface; the HTTP layer
/// maps each variant to the response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingErrorCode {
    NotFound,
    AccessDenied,
    StateNotEditable,
    DeletionInProgress,
    InvalidInterval,
    PaddingLimitExceeded,
    InvalidTemplate,
    InvalidSource,
    DuplicateOccurrence,
    QuotaExceeded,
    InsufficientDisk,
    UnsafePath,
    DeleteFailed,
    PersistenceFailed,
    RuleInvalid,
    ConflictPreviewUnavailable,
    TokenRefreshRequired,
    PartialOperation,
}

impl RecordingErrorCode {
    pub fn wire(self) -> &'static str {
        match self {
            Self::NotFound => "recording_not_found",
            Self::AccessDenied => "recording_access_denied",
            Self::StateNotEditable => "recording_state_not_editable",
            Self::DeletionInProgress => "recording_deletion_in_progress",
            Self::InvalidInterval => "recording_invalid_interval",
            Self::PaddingLimitExceeded => "recording_padding_limit_exceeded",
            Self::InvalidTemplate => "recording_invalid_template",
            Self::InvalidSource => "recording_invalid_source",
            Self::DuplicateOccurrence => "recording_duplicate_occurrence",
            Self::QuotaExceeded => "recording_quota_exceeded",
            Self::InsufficientDisk => "recording_insufficient_disk",
            Self::UnsafePath => "recording_unsafe_path",
            Self::DeleteFailed => "recording_delete_failed",
            Self::PersistenceFailed => "recording_persistence_failed",
            Self::RuleInvalid => "recording_rule_invalid",
            Self::ConflictPreviewUnavailable => "recording_conflict_preview_unavailable",
            Self::TokenRefreshRequired => "recording_token_refresh_required",
            Self::PartialOperation => "recording_rule_partial_operation",
        }
    }
}

/// Path revalidation that runs in the queue-mutation boundary
/// *after* an external filesystem
/// measurement but *before* committing. The caller passes the
/// pre-measurement state, the post-measurement state, and the
/// (now-fresh) source the worker would re-resolve against.
pub fn revalidate_after_measurement(
    pre_state_label: &str,
    post_state_label: &str,
    source_unchanged: bool,
) -> Result<(), RecordingErrorCode> {
    if pre_state_label != post_state_label {
        return Err(RecordingErrorCode::StateNotEditable);
    }
    if !source_unchanged {
        return Err(RecordingErrorCode::InvalidSource);
    }
    Ok(())
}

/// Path revalidation that runs immediately before a file open.
/// The caller passes the relative path the metadata
/// claims. The function rejects traversal sequences (`..`,
/// absolute paths, NULs).
pub fn validate_relative_path_for_open(relative_path: &str) -> Result<(), RecordingErrorCode> {
    use std::path::{Component, Path};
    if relative_path.is_empty() || relative_path.contains('\0') {
        return Err(RecordingErrorCode::UnsafePath);
    }
    let path = Path::new(relative_path);
    if path.is_absolute() {
        return Err(RecordingErrorCode::UnsafePath);
    }
    // Only `Component::Normal` segments are accepted — `..` and `.` and any
    // root/prefix components are rejected. Names like `ep..1` are valid
    // because the dot is part of the segment, not a parent-dir component.
    let components: Vec<_> = path.components().collect();
    if components.is_empty() || !components.iter().all(|c| matches!(c, Component::Normal(_))) {
        return Err(RecordingErrorCode::UnsafePath);
    }
    Ok(())
}

/// Log redaction. Private title, channel, filename, user ID, and
/// rule metadata must be redacted from
/// non-owner-facing logs. The helper returns the fields that are
/// safe to log given the caller's principal.
pub fn redact_for_log(meta: &RecordingMetadata, is_admin_role: bool, is_owner: bool) -> RedactedRecording {
    let show_private = is_admin_role || is_owner;
    RedactedRecording {
        task_id_present: meta.relative_path.is_some() || meta.partial_relative_path.is_some(),
        visibility: meta.visibility,
        owner: if show_private { Some(meta.owner.clone()) } else { None },
        programme_title: if show_private { meta.program_title.clone() } else { None },
        channel: if show_private { meta.channel_name.clone() } else { None },
        output_filename: if show_private { meta.relative_path.clone() } else { None },
        rule_id: if show_private { meta.provenance.rule_id.clone() } else { None },
        occurrence_key: if show_private { meta.provenance.occurrence_key.clone() } else { None },
    }
}

/// The redacted log shape. Each optional field is `None` when
/// the principal is not the owner and not an administrator.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RedactedRecording {
    pub task_id_present: bool,
    pub visibility: RecordingVisibility,
    pub owner: Option<RecordingOwner>,
    pub programme_title: Option<String>,
    pub channel: Option<String>,
    pub output_filename: Option<String>,
    pub rule_id: Option<String>,
    pub occurrence_key: Option<String>,
}

/// Check whether the principal can read this recording. Used in
/// every authorization decision (HTTP, WebSocket, catalog). The
/// read-access matrix:
/// - private + owner = allow.
/// - private + admin = allow.
/// - private + foreign user = deny.
/// - shared + anyone with `recording.read` = allow.
pub fn authorize_read(
    meta: &RecordingMetadata,
    principal_id: &UserId,
    has_recording_read: bool,
    is_admin_role: bool,
) -> bool {
    if !has_recording_read {
        return false;
    }
    if is_admin_role {
        return true;
    }
    if matches!(meta.visibility, RecordingVisibility::Shared) {
        return true;
    }
    if let RecordingOwner::User(owner_id) = &meta.owner {
        return owner_id == principal_id;
    }
    // LegacyAdmin: only administrators can read (the admin check
    // above already handled that path).
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{
        recording::{RecordingOwner, RecordingVisibility},
        UserId,
    };

    fn user(name: &str) -> UserId {
        UserId::from(name)
    }

    fn make_meta(visibility: RecordingVisibility, owner: RecordingOwner) -> RecordingMetadata {
        RecordingMetadata {
            owner,
            visibility,
            source: None,
            program_start: Some(1_700_000_000),
            program_end: Some(1_700_003_600),
            scheduled_start: Some(1_700_000_000),
            scheduled_end: Some(1_700_003_600),
            pre_roll_secs: 0,
            post_roll_secs: 0,
            channel_id: Some("ch-1".into()),
            channel_name: Some("Channel 1".into()),
            program_title: Some("Programme".into()),
            epg: None,
            provenance: shared::model::recording::RecordingProvenance::default(),
            relative_path: Some("path/file.ts".into()),
            partial_relative_path: None,
            reserved_bytes: 0,
            measured_bytes: 0,
            completed_at: None,
            notification_markers: vec![],
            deleting_previous_state: None,
        }
    }

    #[test]
    fn wire_codes_match_plan_table() {
        assert_eq!(RecordingErrorCode::NotFound.wire(), "recording_not_found");
        assert_eq!(RecordingErrorCode::AccessDenied.wire(), "recording_access_denied");
        assert_eq!(RecordingErrorCode::StateNotEditable.wire(), "recording_state_not_editable");
        assert_eq!(RecordingErrorCode::DeletionInProgress.wire(), "recording_deletion_in_progress");
        assert_eq!(RecordingErrorCode::InvalidInterval.wire(), "recording_invalid_interval");
        assert_eq!(RecordingErrorCode::PaddingLimitExceeded.wire(), "recording_padding_limit_exceeded");
        assert_eq!(RecordingErrorCode::InvalidTemplate.wire(), "recording_invalid_template");
        assert_eq!(RecordingErrorCode::InvalidSource.wire(), "recording_invalid_source");
        assert_eq!(RecordingErrorCode::DuplicateOccurrence.wire(), "recording_duplicate_occurrence");
        assert_eq!(RecordingErrorCode::QuotaExceeded.wire(), "recording_quota_exceeded");
        assert_eq!(RecordingErrorCode::InsufficientDisk.wire(), "recording_insufficient_disk");
        assert_eq!(RecordingErrorCode::UnsafePath.wire(), "recording_unsafe_path");
        assert_eq!(RecordingErrorCode::DeleteFailed.wire(), "recording_delete_failed");
        assert_eq!(RecordingErrorCode::PersistenceFailed.wire(), "recording_persistence_failed");
        assert_eq!(RecordingErrorCode::RuleInvalid.wire(), "recording_rule_invalid");
        assert_eq!(RecordingErrorCode::ConflictPreviewUnavailable.wire(), "recording_conflict_preview_unavailable");
        assert_eq!(RecordingErrorCode::TokenRefreshRequired.wire(), "recording_token_refresh_required");
        assert_eq!(RecordingErrorCode::PartialOperation.wire(), "recording_rule_partial_operation");
    }

    #[test]
    fn revalidate_after_measurement_rejects_state_drift() {
        assert!(revalidate_after_measurement("Downloading", "Downloading", true).is_ok());
        assert_eq!(
            revalidate_after_measurement("Downloading", "Completed", true),
            Err(RecordingErrorCode::StateNotEditable)
        );
    }

    #[test]
    fn revalidate_after_measurement_rejects_source_drift() {
        assert_eq!(
            revalidate_after_measurement("Downloading", "Downloading", false),
            Err(RecordingErrorCode::InvalidSource)
        );
    }

    #[test]
    fn validate_relative_path_rejects_traversal_and_absolute() {
        assert!(validate_relative_path_for_open("users/web:alice/file.ts").is_ok());
        assert_eq!(validate_relative_path_for_open("../etc/passwd"), Err(RecordingErrorCode::UnsafePath));
        assert_eq!(validate_relative_path_for_open("/etc/passwd"), Err(RecordingErrorCode::UnsafePath));
        assert_eq!(validate_relative_path_for_open("dir\0file"), Err(RecordingErrorCode::UnsafePath));
        assert_eq!(validate_relative_path_for_open(""), Err(RecordingErrorCode::UnsafePath));
    }

    #[test]
    fn redact_for_log_hides_private_for_foreign_user() {
        let meta = make_meta(RecordingVisibility::Private, RecordingOwner::User(user("web:alice")));
        let r = redact_for_log(&meta, false, false);
        assert!(r.owner.is_none());
        assert!(r.programme_title.is_none());
        assert!(r.channel.is_none());
        assert!(r.output_filename.is_none());
    }

    #[test]
    fn redact_for_log_shows_for_owner() {
        let meta = make_meta(RecordingVisibility::Private, RecordingOwner::User(user("web:alice")));
        let r = redact_for_log(&meta, false, true);
        assert!(matches!(r.owner, Some(RecordingOwner::User(_))));
        assert_eq!(r.programme_title.as_deref(), Some("Programme"));
    }

    #[test]
    fn redact_for_log_shows_for_admin() {
        let meta = make_meta(RecordingVisibility::Private, RecordingOwner::User(user("web:alice")));
        let r = redact_for_log(&meta, true, false);
        assert!(matches!(r.owner, Some(RecordingOwner::User(_))));
    }

    #[test]
    fn authorize_read_private_owner_allowed() {
        let meta = make_meta(RecordingVisibility::Private, RecordingOwner::User(user("web:alice")));
        assert!(authorize_read(&meta, &user("web:alice"), true, false));
    }

    #[test]
    fn authorize_read_private_foreign_user_denied() {
        let meta = make_meta(RecordingVisibility::Private, RecordingOwner::User(user("web:alice")));
        assert!(!authorize_read(&meta, &user("web:bob"), true, false));
    }

    #[test]
    fn authorize_read_shared_any_recording_read_allowed() {
        let meta = make_meta(RecordingVisibility::Shared, RecordingOwner::User(user("web:alice")));
        assert!(authorize_read(&meta, &user("web:bob"), true, false));
    }

    #[test]
    fn authorize_read_without_recording_read_denied() {
        let meta = make_meta(RecordingVisibility::Shared, RecordingOwner::User(user("web:alice")));
        assert!(!authorize_read(&meta, &user("web:bob"), false, false));
    }

    #[test]
    fn authorize_read_legacy_admin_only_for_administrator() {
        let meta = make_meta(RecordingVisibility::Private, RecordingOwner::LegacyAdmin);
        assert!(!authorize_read(&meta, &user("web:alice"), true, false));
        assert!(authorize_read(&meta, &user("builtin:admin"), true, true));
    }
}
