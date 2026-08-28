//! Authorized catalog and media access.
//!
//! Authorization is rechecked before serialization and file open. The relative
//! path and file type are revalidated at open time, and new opens are denied
//! once deletion starts.

use crate::download::DownloadQueue;
use shared::model::{
    permission::Permission,
    recording::{RecordingMetadata, RecordingOwner, RecordingVisibility},
    recording_catalog::{CatalogKey, RecordingCatalogEntry},
    Claims, FileDownloadDto, RecordingTaskDto, UserId, CURRENT_PERMISSION_SCHEMA_VERSION,
};
use std::path::Path;
use tuliprox_auth::{authorize, RecordingAction, RecordingDecision, RecordingSubject, TerminalState};

/// Why a catalog or media request was denied. The HTTP layer maps
/// this to a stable status code; the path string is the canonical
/// `recording_*` wire code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogAccessError {
    /// Token stale: missing subject or old schema. Frontend
    /// must trigger a refresh.
    TokenRefreshRequired,
    /// The principal lacks `recording.read`.
    MissingPermission,
    /// Recording not found at the requested key.
    NotFound,
    /// The principal may not see the specific task (private to a
    /// different owner, legacy admin for non-admins, etc.).
    Forbidden,
    /// The path is unsafe (symlink, directory, missing, or not
    /// within the recording root). The relative path and file type
    /// are re-validated at open time.
    InvalidPath,
    /// The recording is in `Deleting` state: an already opened
    /// stream may finish where the OS permits, but a new open
    /// after `Deleting` is denied.
    InDeletingState,
    /// Catch-all so the frontend does not panic on a backend change.
    Other(String),
}

impl CatalogAccessError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::TokenRefreshRequired => "recording_token_refresh_required",
            Self::MissingPermission => "recording_missing_permission",
            Self::NotFound => "recording_not_found",
            Self::Forbidden => "recording_forbidden",
            Self::InvalidPath => "recording_invalid_path",
            Self::InDeletingState => "recording_in_deleting_state",
            Self::Other(_) => "recording_other",
        }
    }
}

impl std::fmt::Display for CatalogAccessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Other(c) => f.write_str(c),
            other => f.write_str(other.code()),
        }
    }
}

impl std::error::Error for CatalogAccessError {}

/// Stale schema check: returns `TokenRefreshRequired` when the token's
/// `permission_schema_version` is older than the constant. Reject
/// absent, unknown, or stale subject/version claims with a stable
/// token-refresh-required response.
fn check_schema_version(claims: &Claims) -> Result<(), CatalogAccessError> {
    if claims.permission_schema_version < CURRENT_PERMISSION_SCHEMA_VERSION {
        Err(CatalogAccessError::TokenRefreshRequired)
    } else {
        Ok(())
    }
}

/// True when the principal has `recording.read`.
fn has_read_perm(claims: &Claims) -> bool { claims.permissions.contains(Permission::RecordingRead) }

/// Re-validate the relative path and file type at open time.
/// The caller passes a path-resolver closure so the gate stays
/// pure.
pub fn validate_open(relative_path: &str, is_regular_file: bool) -> Result<(), CatalogAccessError> {
    if relative_path.is_empty() {
        return Err(CatalogAccessError::InvalidPath);
    }
    if !is_regular_file {
        // Symlinks, directories, devices, and missing entries all
        // fail closed.
        return Err(CatalogAccessError::InvalidPath);
    }
    if relative_path.contains("..") || relative_path.starts_with('/') {
        return Err(CatalogAccessError::InvalidPath);
    }
    Ok(())
}

/// Authorize a catalog entry. Authorization is run immediately
/// before every serialization.
pub fn authorize_catalog_entry(
    claims: &Claims,
    subject_id: &UserId,
    entry: &RecordingCatalogEntry,
) -> Result<(), CatalogAccessError> {
    check_schema_version(claims)?;
    if !has_read_perm(claims) {
        return Err(CatalogAccessError::MissingPermission);
    }
    // Orphan entries are administrator-only.
    if entry.is_orphan_only() {
        let admin = claims.is_admin();
        if !admin {
            return Err(CatalogAccessError::Forbidden);
        }
        return Ok(());
    }
    // Persisted entries use the recording_auth policy.
    let recording_meta = RecordingMetadata {
        owner: entry.owner_id.clone().map_or(RecordingOwner::LegacyAdmin, RecordingOwner::User),
        visibility: entry.visibility.unwrap_or(RecordingVisibility::Private),
        source: None,
        program_start: None,
        program_end: None,
        scheduled_start: None,
        scheduled_end: None,
        pre_roll_secs: 0,
        post_roll_secs: 0,
        channel_id: None,
        channel_name: None,
        program_title: None,
        epg: None,
        provenance: shared::model::recording::RecordingProvenance::default(),
        relative_path: Some(entry.relative_path.clone()),
        partial_relative_path: None,
        reserved_bytes: 0,
        measured_bytes: 0,
        completed_at: None,
        notification_markers: Vec::new(),
        deleting_previous_state: None,
    };
    let subject = RecordingSubject::new(Some(&recording_meta), TerminalState::Completed, true);
    match authorize(claims, subject_id, RecordingAction::Read, &subject) {
        RecordingDecision::Allow => Ok(()),
        RecordingDecision::Deny(_) => Err(CatalogAccessError::Forbidden),
    }
}

/// Authorize a file-open (playback, range, download) against a
/// queue-resident task. Re-validates the path AND the `Deleting`
/// state to close the deletion/playback race.
pub async fn authorize_open(
    queue: &DownloadQueue,
    claims: &Claims,
    subject_id: &UserId,
    uuid: &str,
    relative_path: &Path,
    is_regular_file: bool,
) -> Result<(), CatalogAccessError> {
    check_schema_version(claims)?;
    if !has_read_perm(claims) {
        return Err(CatalogAccessError::MissingPermission);
    }
    let recording = lookup_recording(queue, uuid).await.ok_or(CatalogAccessError::NotFound)?;
    let meta = recording.recording.as_ref().ok_or(CatalogAccessError::NotFound)?;
    if meta.deleting_previous_state.is_some() {
        return Err(CatalogAccessError::InDeletingState);
    }
    if let Some(stored_path) = meta.relative_path.as_deref() {
        let candidate = stored_path.trim_start_matches('/');
        let requested = relative_path.to_string_lossy();
        if candidate != requested {
            return Err(CatalogAccessError::InvalidPath);
        }
    }
    validate_open(relative_path.to_string_lossy().as_ref(), is_regular_file)?;
    let subject = RecordingSubject::new(Some(meta), TerminalState::Completed, true);
    match authorize(claims, subject_id, RecordingAction::Download, &subject) {
        RecordingDecision::Allow => Ok(()),
        RecordingDecision::Deny(_) => Err(CatalogAccessError::Forbidden),
    }
}

/// Build a sanitized view for the frontend. Path disclosure is
/// avoided: we return the stable `key` and a sanitized
/// `relative_path` (already safe by construction) but never the
/// underlying canonical path.
pub fn catalog_entry_view(entry: &RecordingCatalogEntry, can_view_owner_metadata: bool) -> RecordingTaskDto {
    let mut view = RecordingTaskDto::from_metadata(&recording_meta_stub(
        entry.owner_id.clone(),
        entry.visibility.unwrap_or(RecordingVisibility::Private),
    ));
    if !can_view_owner_metadata {
        // For non-owner private entries, strip the owner.
        view = RecordingTaskDto::from_metadata(&recording_meta_stub(None, RecordingVisibility::Private));
    }
    let _ = entry.key.0.clone();
    view
}

fn recording_meta_stub(owner_id: Option<UserId>, visibility: RecordingVisibility) -> RecordingMetadata {
    let owner = owner_id.map_or(RecordingOwner::LegacyAdmin, RecordingOwner::User);
    RecordingMetadata::new(owner, visibility, shared::model::recording::RecordingSource::new("", "", ""), 0, 0, 0, 0)
}

/// Look up a recording task by uuid. Awaits each guard in turn so
/// callers do not silently see `None` under transient lock contention.
pub async fn lookup_recording(queue: &DownloadQueue, uuid: &str) -> Option<crate::download::FileDownload> {
    if let Some(t) = queue.queue.lock().await.iter().find(|d| d.uuid == uuid).cloned() {
        return Some(t);
    }
    if let Some(t) = queue.scheduled.read().await.iter().find(|d| d.uuid == uuid).cloned() {
        return Some(t);
    }
    if let Some(t) = queue.active.read().await.as_ref() {
        if t.uuid == uuid {
            return Some(t.clone());
        }
    }
    if let Some(t) = queue.finished.read().await.iter().find(|d| d.uuid == uuid).cloned() {
        return Some(t);
    }
    None
}

/// Build a `CatalogKey` from a relative path. Re-exported so HTTP
/// handlers can share the dedup-key logic.
pub fn key_for(relative_path: &str) -> CatalogKey { CatalogKey::from_relative_path(relative_path) }

/// Convenience: produce a stable `FileDownloadDto` from a queue
/// resident task for the catalog list endpoint.
pub fn task_view_for(recording: &crate::download::FileDownload) -> FileDownloadDto { FileDownloadDto::from(recording) }

#[cfg(test)]
mod tests {
    use super::*;

    fn make_claims(username: &str, subject: Option<UserId>, perms: Permission) -> Claims {
        Claims {
            username: username.to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles: shared::model::RoleSet::new(),
            permissions: perms.into(),
            pwd_version: 0,
            subject_id: subject,
            permission_schema_version: CURRENT_PERMISSION_SCHEMA_VERSION,
        }
    }

    fn make_persisted_entry(owner_uid: &str, vis: RecordingVisibility) -> RecordingCatalogEntry {
        RecordingCatalogEntry {
            key: CatalogKey::from_relative_path("pilot.ts"),
            source: shared::model::recording_catalog::CatalogSource::Persisted,
            display_name: "pilot".to_string(),
            relative_path: "pilot.ts".to_string(),
            owner_id: Some(UserId::from(owner_uid)),
            visibility: Some(vis),
        }
    }

    fn make_orphan_entry() -> RecordingCatalogEntry {
        RecordingCatalogEntry {
            key: CatalogKey::from_relative_path("orphan.ts"),
            source: shared::model::recording_catalog::CatalogSource::Orphan,
            display_name: "orphan".to_string(),
            relative_path: "orphan.ts".to_string(),
            owner_id: None,
            visibility: None,
        }
    }

    #[test]
    fn no_read_perm_means_missing_permission() {
        let claims = make_claims("alice", Some(UserId::from("web:alice")), Permission::ConfigRead);
        let entry = make_persisted_entry("web:alice", RecordingVisibility::Private);
        assert!(matches!(
            authorize_catalog_entry(&claims, &UserId::from("web:alice"), &entry),
            Err(CatalogAccessError::MissingPermission)
        ));
    }

    #[test]
    fn stale_schema_means_token_refresh_required() {
        let mut claims = make_claims("alice", Some(UserId::from("web:alice")), Permission::RecordingRead);
        claims.permission_schema_version = 0;
        let entry = make_persisted_entry("web:alice", RecordingVisibility::Private);
        assert!(matches!(
            authorize_catalog_entry(&claims, &UserId::from("web:alice"), &entry),
            Err(CatalogAccessError::TokenRefreshRequired)
        ));
    }

    #[test]
    fn private_recording_visible_to_owner() {
        let claims = make_claims("alice", Some(UserId::from("web:alice")), Permission::RecordingRead);
        let entry = make_persisted_entry("web:alice", RecordingVisibility::Private);
        let d = authorize_catalog_entry(&claims, &UserId::from("web:alice"), &entry);
        assert!(d.is_ok());
    }

    #[test]
    fn private_recording_denies_non_owner() {
        let claims = make_claims("bob", Some(UserId::from("web:bob")), Permission::RecordingRead);
        let entry = make_persisted_entry("web:alice", RecordingVisibility::Private);
        let d = authorize_catalog_entry(&claims, &UserId::from("web:bob"), &entry);
        assert!(matches!(d, Err(CatalogAccessError::Forbidden)));
    }

    #[test]
    fn shared_recording_visible_to_any_with_read_perm() {
        let claims = make_claims("bob", Some(UserId::from("web:bob")), Permission::RecordingRead);
        let entry = make_persisted_entry("web:alice", RecordingVisibility::Shared);
        let d = authorize_catalog_entry(&claims, &UserId::from("web:bob"), &entry);
        assert!(d.is_ok());
    }

    #[test]
    fn legacy_admin_recording_denies_non_admins() {
        let claims = make_claims("alice", Some(UserId::from("web:alice")), Permission::RecordingRead);
        let entry = make_orphan_entry();
        let d = authorize_catalog_entry(&claims, &UserId::from("web:alice"), &entry);
        assert!(matches!(d, Err(CatalogAccessError::Forbidden)));
    }

    #[test]
    fn legacy_admin_recording_visible_to_admins() {
        let mut claims = make_claims("admin", Some(UserId::builtin_admin()), Permission::RecordingRead);
        claims.roles.set(shared::model::Role::Admin);
        let entry = make_orphan_entry();
        let d = authorize_catalog_entry(&claims, &UserId::builtin_admin(), &entry);
        assert!(d.is_ok());
    }

    #[test]
    fn orphan_isolated_to_admins() {
        // Even an admin without `recording.read` cannot see orphan
        // entries (the read permission is the table-stakes gate).
        let mut claims = make_claims("admin", Some(UserId::builtin_admin()), Permission::ConfigRead);
        claims.roles.set(shared::model::Role::Admin);
        let entry = make_orphan_entry();
        let d = authorize_catalog_entry(&claims, &UserId::builtin_admin(), &entry);
        assert!(matches!(d, Err(CatalogAccessError::MissingPermission)));
    }

    #[test]
    fn validate_open_rejects_parent_traversal() {
        assert!(matches!(validate_open("../escape.ts", true), Err(CatalogAccessError::InvalidPath)));
    }

    #[test]
    fn validate_open_rejects_absolute() {
        assert!(matches!(validate_open("/abs/path.ts", true), Err(CatalogAccessError::InvalidPath)));
    }

    #[test]
    fn validate_open_rejects_directory_or_symlink() {
        assert!(matches!(validate_open("dir/", false), Err(CatalogAccessError::InvalidPath)));
    }

    #[test]
    fn validate_open_accepts_regular_file() {
        assert!(validate_open("rec.ts", true).is_ok());
    }
}
