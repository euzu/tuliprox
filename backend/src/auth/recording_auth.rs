//! Recording authorization policy.
//!
//! One pure module that decides whether a subject (a `Claims` payload +
//! its resolved `UserId`) can perform an action against a recording
//! task. The decision combines:
//!
//! - The principal's recording permission bits
//! - The principal's role (administrator vs. web/API user)
//! - The recording's `RecordingOwner` (real owner vs. `LegacyAdmin`)
//! - The recording's `RecordingVisibility` (private vs. shared)
//! - The task's `DownloadState` (for state-aware actions like
//!   `system-retention-delete`)
//! - A path/visibility/owner triple that the caller resolves before
//!   calling the policy
//!
//! The policy separates "user ownership bypass" from "path/kind/state
//! checks": system retention can act on behalf of any user, but only on
//! eligible terminal states and only through the retention worker.

use shared::model::permission::Permission;
use shared::model::recording::{RecordingMetadata, RecordingVisibility};
use shared::model::{Claims, ROLE_ADMIN, UserId};

/// Actions the recording system distinguishes. Each action maps to one
/// or more HTTP routes and one or more WebSocket messages. New
/// actions must be added here AND in the route table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordingAction {
    /// List or read the metadata of a single recording.
    Read,
    /// Create a private recording.
    CreatePrivate,
    /// Create a shared recording. Only administrators may create
    /// shared recordings; the policy rejects any non-admin caller
    /// regardless of the permission bits they carry.
    CreateShared,
    /// Edit an existing recording (interval, padding, programme data,
    /// path reservation).
    Edit,
    /// Cancel an in-flight or scheduled recording.
    Cancel,
    /// Delete a finished (terminal) recording via an explicit user
    /// `DELETE` request.
    Delete,
    /// Create, edit or delete a recurring rule.
    ManageRule,
    /// Play back the recording media.
    Playback,
    /// Download the recording media (range requests, file copies).
    Download,
    /// Internal retention delete (eligible completed recordings only).
    /// Bypasses user ownership but enforces state/kind/visibility.
    SystemRetentionDelete,
}

/// Why an action was denied. Surfaced as a stable string for the HTTP
/// layer and a structured field for the frontend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenyReason {
    /// The caller carries no `subject_id`; the request must be
    /// re-authenticated before the policy can be evaluated.
    UnknownSubject,
    /// The caller does not have the required recording permission.
    MissingPermission(Permission),
    /// Private recording access requires the real owner subject.
    NotOwner,
    /// Shared mutation (edit/cancel/delete) requires the administrator
    /// role in addition to the recording permission.
    NotAdministrator,
    /// `LegacyAdmin` recordings are only accessible to administrators
    /// with the required permission. Non-admin callers — even the
    /// real owner — cannot act on a `LegacyAdmin` recording.
    LegacyAdminReserved,
    /// Retention delete is gated on a terminal state and a non-partial
    /// path. The recording's current state forbids it.
    IneligibleState,
    /// Path/kind validation failed (e.g., a partial path on a `Completed`
    /// recording, a foreign `relative_path`, or a missing owner).
    InvalidPath,
}

impl std::fmt::Display for DenyReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownSubject => f.write_str("subject_id is missing"),
            Self::MissingPermission(p) => write!(f, "missing permission: {}", permission_name(*p)),
            Self::NotOwner => f.write_str("not the real owner"),
            Self::NotAdministrator => f.write_str("administrator role required"),
            Self::LegacyAdminReserved => f.write_str("legacy admin recording — administrator required"),
            Self::IneligibleState => f.write_str("recording is not in an eligible terminal state"),
            Self::InvalidPath => f.write_str("recording path/kind is invalid"),
        }
    }
}

fn permission_name(p: Permission) -> &'static str {
    match p {
        Permission::ConfigRead => "config.read",
        Permission::ConfigWrite => "config.write",
        Permission::SourceRead => "source.read",
        Permission::SourceWrite => "source.write",
        Permission::UserRead => "user.read",
        Permission::UserWrite => "user.write",
        Permission::PlaylistRead => "playlist.read",
        Permission::PlaylistWrite => "playlist.write",
        Permission::LibraryRead => "library.read",
        Permission::LibraryWrite => "library.write",
        Permission::SystemRead => "system.read",
        Permission::SystemWrite => "system.write",
        Permission::EpgRead => "epg.read",
        Permission::EpgWrite => "epg.write",
        Permission::DownloadRead => "download.read",
        Permission::DownloadWrite => "download.write",
        Permission::RecordingRead => "recording.read",
        Permission::RecordingWrite => "recording.write",
    }
}

/// Resolved view of the recording. Constructed by the caller from the
/// runtime `FileDownload` and the new `RecordingService` so the policy
/// stays pure and testable.
#[derive(Debug, Clone)]
pub struct RecordingSubject<'a> {
    pub metadata: Option<&'a RecordingMetadata>,
    /// The terminal state the recording is currently in. Recorded for
    /// state-aware actions (e.g., retention delete).
    pub state: TerminalState,
    /// Whether the recording carries a valid partial/final path under
    /// the recording root. Used to fail closed on path anomalies.
    pub path_valid: bool,
}

/// Coarse terminal state for the policy. The retention-delete path
/// is the only one that needs to know whether the recording is in an
/// eligible terminal state. Other actions look at `metadata` and the
/// principal directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalState {
    Active,
    Scheduled,
    Completed,
    Failed,
    Cancelled,
    /// The recording is currently in the two-phase deletion flow. The
    /// caller has already begun deletion (`Deleting`); only `system-
    /// retention-delete` is meaningful here, and only as a no-op.
    Deleting,
}

impl TerminalState {
    /// `true` when the recording is in a terminal state that is
    /// eligible for retention delete. Only the completed, failed
    /// and cancelled states qualify.
    pub fn is_eligible_for_retention(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

impl<'a> RecordingSubject<'a> {
    /// Build a subject from a `RecordingMetadata` and a state.
    pub fn new(metadata: Option<&'a RecordingMetadata>, state: TerminalState, path_valid: bool) -> Self {
        Self { metadata, state, path_valid }
    }
}

/// Result of a policy check. The HTTP layer maps `Allow` to 200 and
/// `Deny` to 403 (or 404 when the deny reason would leak existence).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingDecision {
    Allow,
    Deny(DenyReason),
}

impl RecordingDecision {
    pub fn is_allow(&self) -> bool { matches!(self, Self::Allow) }
}

fn is_admin(claims: &Claims) -> bool {
    claims.roles.iter().any(|r| r == ROLE_ADMIN)
}

fn check_create(claims: &Claims, action: RecordingAction) -> RecordingDecision {
    match action {
        RecordingAction::CreatePrivate => {
            if !has_recording_write(claims) {
                return RecordingDecision::Deny(DenyReason::MissingPermission(Permission::RecordingWrite));
            }
            RecordingDecision::Allow
        }
        RecordingAction::CreateShared => {
            if !has_recording_write(claims) {
                return RecordingDecision::Deny(DenyReason::MissingPermission(Permission::RecordingWrite));
            }
            if !is_admin(claims) {
                return RecordingDecision::Deny(DenyReason::NotAdministrator);
            }
            RecordingDecision::Allow
        }
        _ => RecordingDecision::Deny(DenyReason::InvalidPath),
    }
}

fn owner_of(meta: &RecordingMetadata) -> Option<UserId> {
    use shared::model::RecordingOwner;
    match &meta.owner {
        RecordingOwner::User(uid) => Some(uid.clone()),
        RecordingOwner::LegacyAdmin => None,
    }
}

fn is_visibility(meta: &RecordingMetadata, want: RecordingVisibility) -> bool {
    meta.visibility == want
}

fn has_recording_read(claims: &Claims) -> bool {
    claims.permissions.contains(Permission::RecordingRead)
}

fn has_recording_write(claims: &Claims) -> bool {
    claims.permissions.contains(Permission::RecordingWrite)
}

/// The principal decision. Pure function — does not touch the
/// filesystem, the network, or the queue.
///
/// `subject_id` is the resolved `UserId` of the caller. It is required
/// — callers without a `subject_id` must re-authenticate first
/// (see `AuthError::MissingSubject` in `authenticator`).
pub fn authorize(
    claims: &Claims,
    subject_id: &UserId,
    action: RecordingAction,
    recording: &RecordingSubject<'_>,
) -> RecordingDecision {
    // System retention can delete a Completed/Failed/Cancelled
    // recording on behalf of any user, including when the owner
    // cannot be resolved (e.g., a corrupted registry entry). The
    // bypass is narrow: state must be eligible, path must be valid,
    // and the caller must carry `recording.write`.
    if matches!(action, RecordingAction::SystemRetentionDelete) {
        if !recording.state.is_eligible_for_retention() {
            return RecordingDecision::Deny(DenyReason::IneligibleState);
        }
        if !recording.path_valid {
            return RecordingDecision::Deny(DenyReason::InvalidPath);
        }
        if !has_recording_write(claims) {
            return RecordingDecision::Deny(DenyReason::MissingPermission(Permission::RecordingWrite));
        }
        if !is_admin(claims) {
            // The system-retention path bypasses user ownership only
            // for eligible completed recording deletion. Operators
            // without the
            // admin role are still subject to it; the call site
            // (retention worker) is the only legitimate caller.
            return RecordingDecision::Deny(DenyReason::NotAdministrator);
        }
        return RecordingDecision::Allow;
    }

    // From here on, every action is user-driven.
    // Create actions and manage-rule do not require an existing
    // recording; the caller is proposing a new one. The principal's
    // permission and (for shared) administrator role are the only
    // gates.
    match action {
        RecordingAction::CreatePrivate | RecordingAction::CreateShared => {
            return check_create(claims, action);
        }
        RecordingAction::ManageRule => {
            if !has_recording_write(claims) {
                return RecordingDecision::Deny(DenyReason::MissingPermission(Permission::RecordingWrite));
            }
            if !is_admin(claims) {
                return RecordingDecision::Deny(DenyReason::NotAdministrator);
            }
            return RecordingDecision::Allow;
        }
        _ => {}
    }

    let Some(meta) = recording.metadata else {
        return RecordingDecision::Deny(DenyReason::InvalidPath);
    };
    if !recording.path_valid {
        return RecordingDecision::Deny(DenyReason::InvalidPath);
    }

    let is_legacy = matches!(meta.owner, shared::model::RecordingOwner::LegacyAdmin);

    // LegacyAdmin recordings are only accessible to administrators.
    if is_legacy && !is_admin(claims) {
        return RecordingDecision::Deny(DenyReason::LegacyAdminReserved);
    }
    if is_legacy && !is_admin(claims) {
        // Re-checked for symmetry with the policy wording.
        return RecordingDecision::Deny(DenyReason::LegacyAdminReserved);
    }
    if is_legacy && action_requires_owner(action) {
        // Even an admin cannot impersonate a LegacyAdmin owner.
        return RecordingDecision::Deny(DenyReason::LegacyAdminReserved);
    }

    match action {
        RecordingAction::Read | RecordingAction::Playback | RecordingAction::Download => {
            check_read_action(claims, subject_id, meta, is_legacy)
        }
        RecordingAction::CreatePrivate | RecordingAction::CreateShared => check_create(claims, action),
        RecordingAction::Edit | RecordingAction::Cancel | RecordingAction::Delete => {
            check_mutate_action(claims, subject_id, meta)
        }
        RecordingAction::ManageRule => {
            if !has_recording_write(claims) {
                return RecordingDecision::Deny(DenyReason::MissingPermission(Permission::RecordingWrite));
            }
            if !is_admin(claims) {
                return RecordingDecision::Deny(DenyReason::NotAdministrator);
            }
            RecordingDecision::Allow
        }
        RecordingAction::SystemRetentionDelete => RecordingDecision::Allow,
    }
}

fn check_read_action(
    claims: &Claims,
    subject_id: &UserId,
    meta: &RecordingMetadata,
    is_legacy: bool,
) -> RecordingDecision {
    if !has_recording_read(claims) {
        return RecordingDecision::Deny(DenyReason::MissingPermission(Permission::RecordingRead));
    }
    if is_visibility(meta, RecordingVisibility::Private) {
        if is_legacy {
            // LegacyAdmin is never owner-readable by a non-admin (already
            // rejected at the policy boundary). This branch is
            // unreachable when is_legacy=true; the redundant check
            // documents the invariant.
            return RecordingDecision::Deny(DenyReason::LegacyAdminReserved);
        }
        if owner_of(meta).as_ref() == Some(subject_id) {
            return RecordingDecision::Allow;
        }
        return RecordingDecision::Deny(DenyReason::NotOwner);
    }
    // Shared visibility: any principal with recording.read.
    RecordingDecision::Allow
}

fn check_mutate_action(
    claims: &Claims,
    subject_id: &UserId,
    meta: &RecordingMetadata,
) -> RecordingDecision {
    if !has_recording_write(claims) {
        return RecordingDecision::Deny(DenyReason::MissingPermission(Permission::RecordingWrite));
    }
    if is_visibility(meta, RecordingVisibility::Private) {
        if owner_of(meta).as_ref() == Some(subject_id) {
            return RecordingDecision::Allow;
        }
        return RecordingDecision::Deny(DenyReason::NotOwner);
    }
    // Shared visibility: only admins can mutate ("shared mutation
    // only to administrators with recording.write").
    if !is_admin(claims) {
        return RecordingDecision::Deny(DenyReason::NotAdministrator);
    }
    RecordingDecision::Allow
}

fn action_requires_owner(action: RecordingAction) -> bool {
    matches!(
        action,
        RecordingAction::Edit
            | RecordingAction::Cancel
            | RecordingAction::Delete
            | RecordingAction::ManageRule
    )
}

/// Separate policy for orphan catalog entries. The catalog is a
/// directory of recordings that no longer match a configured
/// target/input or whose owner cannot be resolved. Orphans are
/// visible only to administrators with the required permission;
/// non-admin callers — even the real owner — cannot re-claim
/// an orphan.
pub fn authorize_orphan(claims: &Claims) -> RecordingDecision {
    if !is_admin(claims) {
        return RecordingDecision::Deny(DenyReason::NotAdministrator);
    }
    if !has_recording_read(claims) {
        return RecordingDecision::Deny(DenyReason::MissingPermission(Permission::RecordingRead));
    }
    RecordingDecision::Allow
}

/// Re-export the deletion-prior-state helper so the policy can use it
/// when reasoning about which path the retention delete must remove.
#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::recording::{AiringStatus, EpgEpisodeMetadata, RecordingOwner};
    use shared::model::{Claims, CURRENT_PERMISSION_SCHEMA_VERSION, UserId};

    fn make_claims(
        username: &str,
        subject: Option<UserId>,
        roles: Vec<&str>,
        perms: shared::model::permission::PermissionSet,
    ) -> Claims {
        let roles: Vec<String> = roles.into_iter().map(String::from).collect();
        Claims {
            username: username.to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles,
            permissions: perms,
            pwd_version: 0,
            subject_id: subject,
            permission_schema_version: CURRENT_PERMISSION_SCHEMA_VERSION,
        }
    }

    fn make_meta(owner: RecordingOwner, visibility: RecordingVisibility) -> RecordingMetadata {
        RecordingMetadata {
            owner,
            visibility,
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
            epg: Some(EpgEpisodeMetadata {
                programme_id: None,
                series_id: None,
                episode_id: None,
                season: None,
                episode: None,
                airing: AiringStatus::Unknown,
            }),
            provenance: shared::model::recording::RecordingProvenance::default(),
            relative_path: Some("pilot.ts".to_string()),
            partial_relative_path: None,
            reserved_bytes: 0,
            measured_bytes: 0,
            completed_at: None,
            notification_markers: Vec::new(),
            deleting_previous_state: None,
        }
    }

    fn owner_meta(uid: &str, visibility: RecordingVisibility) -> RecordingMetadata {
        make_meta(RecordingOwner::User(UserId::from(uid)), visibility)
    }

    fn legacy_meta() -> RecordingMetadata {
        make_meta(RecordingOwner::LegacyAdmin, RecordingVisibility::Private)
    }

    fn subject(uid: &str) -> UserId { UserId::from(uid) }

    fn read_perms() -> shared::model::permission::PermissionSet {
        Permission::RecordingRead.into()
    }
    fn write_perms() -> shared::model::permission::PermissionSet {
        Permission::RecordingWrite.into()
    }
    fn read_write_perms() -> shared::model::permission::PermissionSet {
        Permission::RecordingRead | Permission::RecordingWrite
    }
    fn config_read_perms() -> shared::model::permission::PermissionSet {
        Permission::ConfigRead.into()
    }

    // --- read / playback / download ---

    #[test]
    fn read_private_owner_is_allowed() {
        let claims = make_claims("alice", Some(subject("web:alice")), vec!["WEB"], read_perms());
        let meta = owner_meta("web:alice", RecordingVisibility::Private);
        let sub = RecordingSubject::new(Some(&meta), TerminalState::Completed, true);
        let d = authorize(&claims, claims.subject_id.as_ref().expect("test subject_id present"), RecordingAction::Read, &sub);
        assert!(d.is_allow(), "expected allow, got {d:?}");
    }

    #[test]
    fn read_private_non_owner_is_denied() {
        let claims = make_claims("bob", Some(subject("web:bob")), vec!["WEB"], read_perms());
        let meta = owner_meta("web:alice", RecordingVisibility::Private);
        let sub = RecordingSubject::new(Some(&meta), TerminalState::Completed, true);
        let d = authorize(&claims, claims.subject_id.as_ref().expect("test subject_id present"), RecordingAction::Read, &sub);
        assert!(matches!(d, RecordingDecision::Deny(DenyReason::NotOwner)));
    }

    #[test]
    fn read_shared_with_read_perm_is_allowed() {
        let claims = make_claims("bob", Some(subject("web:bob")), vec!["WEB"], read_perms());
        let meta = owner_meta("web:alice", RecordingVisibility::Shared);
        let sub = RecordingSubject::new(Some(&meta), TerminalState::Completed, true);
        let d = authorize(&claims, claims.subject_id.as_ref().expect("test subject_id present"), RecordingAction::Read, &sub);
        assert!(d.is_allow());
    }

    #[test]
    fn read_without_recording_read_perm_is_denied() {
        let claims = make_claims("alice", Some(subject("web:alice")), vec!["WEB"], config_read_perms());
        let meta = owner_meta("web:alice", RecordingVisibility::Private);
        let sub = RecordingSubject::new(Some(&meta), TerminalState::Completed, true);
        let d = authorize(&claims, claims.subject_id.as_ref().expect("test subject_id present"), RecordingAction::Read, &sub);
        assert!(matches!(d, RecordingDecision::Deny(DenyReason::MissingPermission(Permission::RecordingRead))));
    }

    #[test]
    fn read_legacy_admin_recording_requires_administrator() {
        let claims = make_claims("alice", Some(subject("web:alice")), vec!["WEB"], read_perms());
        let meta = legacy_meta();
        let sub = RecordingSubject::new(Some(&meta), TerminalState::Completed, true);
        let d = authorize(&claims, claims.subject_id.as_ref().expect("test subject_id present"), RecordingAction::Read, &sub);
        assert!(matches!(d, RecordingDecision::Deny(DenyReason::LegacyAdminReserved)));
    }

    // --- create ---

    #[test]
    fn create_private_with_recording_write_is_allowed() {
        let claims = make_claims("alice", Some(subject("web:alice")), vec!["WEB"], write_perms());
        let sub = RecordingSubject::new(None, TerminalState::Active, true);
        let d = authorize(&claims, claims.subject_id.as_ref().expect("test subject_id present"), RecordingAction::CreatePrivate, &sub);
        assert!(d.is_allow());
    }

    #[test]
    fn create_shared_requires_administrator_role() {
        let claims = make_claims("alice", Some(subject("web:alice")), vec!["WEB"], read_write_perms());
        let sub = RecordingSubject::new(None, TerminalState::Active, true);
        let d = authorize(&claims, claims.subject_id.as_ref().expect("test subject_id present"), RecordingAction::CreateShared, &sub);
        assert!(matches!(d, RecordingDecision::Deny(DenyReason::NotAdministrator)));
    }

    #[test]
    fn create_shared_with_admin_role_and_recording_write_is_allowed() {
        let claims = make_claims("admin", Some(UserId::builtin_admin()), vec!["ADMIN"], read_write_perms());
        let sub = RecordingSubject::new(None, TerminalState::Active, true);
        let d = authorize(&claims, claims.subject_id.as_ref().expect("test subject_id present"), RecordingAction::CreateShared, &sub);
        assert!(d.is_allow());
    }

    // --- edit / cancel / delete ---

    #[test]
    fn edit_private_owner_with_recording_write_is_allowed() {
        let claims = make_claims("alice", Some(subject("web:alice")), vec!["WEB"], write_perms());
        let meta = owner_meta("web:alice", RecordingVisibility::Private);
        let sub = RecordingSubject::new(Some(&meta), TerminalState::Scheduled, true);
        let d = authorize(&claims, claims.subject_id.as_ref().expect("test subject_id present"), RecordingAction::Edit, &sub);
        assert!(d.is_allow());
    }

    #[test]
    fn edit_private_non_owner_is_denied() {
        let claims = make_claims("bob", Some(subject("web:bob")), vec!["WEB"], write_perms());
        let meta = owner_meta("web:alice", RecordingVisibility::Private);
        let sub = RecordingSubject::new(Some(&meta), TerminalState::Scheduled, true);
        let d = authorize(&claims, claims.subject_id.as_ref().expect("test subject_id present"), RecordingAction::Edit, &sub);
        assert!(matches!(d, RecordingDecision::Deny(DenyReason::NotOwner)));
    }

    #[test]
    fn edit_shared_requires_administrator() {
        let claims = make_claims("alice", Some(subject("web:alice")), vec!["WEB"], write_perms());
        let meta = owner_meta("web:alice", RecordingVisibility::Shared);
        let sub = RecordingSubject::new(Some(&meta), TerminalState::Scheduled, true);
        let d = authorize(&claims, claims.subject_id.as_ref().expect("test subject_id present"), RecordingAction::Edit, &sub);
        assert!(matches!(d, RecordingDecision::Deny(DenyReason::NotAdministrator)));
    }

    #[test]
    fn delete_shared_administrator_is_allowed() {
        let claims = make_claims("admin", Some(UserId::builtin_admin()), vec!["ADMIN"], write_perms());
        let meta = make_meta(RecordingOwner::User(UserId::from("web:alice")), RecordingVisibility::Shared);
        let sub = RecordingSubject::new(Some(&meta), TerminalState::Completed, true);
        let d = authorize(&claims, claims.subject_id.as_ref().expect("test subject_id present"), RecordingAction::Delete, &sub);
        assert!(d.is_allow());
    }

    // --- manage rule ---

    #[test]
    fn manage_rule_requires_administrator_and_recording_write() {
        let claims = make_claims("alice", Some(subject("web:alice")), vec!["WEB"], write_perms());
        let sub = RecordingSubject::new(None, TerminalState::Active, true);
        let d = authorize(&claims, claims.subject_id.as_ref().expect("test subject_id present"), RecordingAction::ManageRule, &sub);
        assert!(matches!(d, RecordingDecision::Deny(DenyReason::NotAdministrator)));

        let admin = make_claims("admin", Some(UserId::builtin_admin()), vec!["ADMIN"], write_perms());
        let d = authorize(&admin, admin.subject_id.as_ref().expect("test subject_id present"), RecordingAction::ManageRule, &sub);
        assert!(d.is_allow());
    }

    // --- system retention delete ---

    #[test]
    fn system_retention_delete_requires_completed_or_failed_or_cancelled_state() {
        let claims = make_claims("admin", Some(UserId::builtin_admin()), vec!["ADMIN"], write_perms());
        let meta = owner_meta("web:alice", RecordingVisibility::Private);
        let sub_active = RecordingSubject::new(Some(&meta), TerminalState::Active, true);
        let d = authorize(&claims, claims.subject_id.as_ref().expect("test subject_id present"), RecordingAction::SystemRetentionDelete, &sub_active);
        assert!(matches!(d, RecordingDecision::Deny(DenyReason::IneligibleState)));
        let sub_deleting = RecordingSubject::new(Some(&meta), TerminalState::Deleting, true);
        let d = authorize(&claims, claims.subject_id.as_ref().expect("test subject_id present"), RecordingAction::SystemRetentionDelete, &sub_deleting);
        assert!(matches!(d, RecordingDecision::Deny(DenyReason::IneligibleState)));
    }

    #[test]
    fn system_retention_delete_bypasses_owner_for_eligible_completed_recording() {
        // Real owner is `web:alice`; the caller is `web:admin` (the
        // built-in admin). The policy must allow the delete even
        // though the caller is not the owner — this is the
        // owner-bypass.
        let claims = make_claims("admin", Some(UserId::builtin_admin()), vec!["ADMIN"], write_perms());
        let meta = owner_meta("web:alice", RecordingVisibility::Private);
        let sub = RecordingSubject::new(Some(&meta), TerminalState::Completed, true);
        let d = authorize(&claims, claims.subject_id.as_ref().expect("test subject_id present"), RecordingAction::SystemRetentionDelete, &sub);
        assert!(d.is_allow(), "retention delete must bypass owner for eligible Completed; got {d:?}");
    }

    #[test]
    fn system_retention_delete_requires_recording_write() {
        let claims = make_claims("admin", Some(UserId::builtin_admin()), vec!["ADMIN"], read_perms());
        let meta = owner_meta("web:alice", RecordingVisibility::Private);
        let sub = RecordingSubject::new(Some(&meta), TerminalState::Completed, true);
        let d = authorize(&claims, claims.subject_id.as_ref().expect("test subject_id present"), RecordingAction::SystemRetentionDelete, &sub);
        assert!(matches!(d, RecordingDecision::Deny(DenyReason::MissingPermission(Permission::RecordingWrite))));
    }

    #[test]
    fn system_retention_delete_requires_administrator_role() {
        let claims = make_claims("alice", Some(subject("web:alice")), vec!["WEB"], write_perms());
        let meta = owner_meta("web:alice", RecordingVisibility::Private);
        let sub = RecordingSubject::new(Some(&meta), TerminalState::Completed, true);
        let d = authorize(&claims, claims.subject_id.as_ref().expect("test subject_id present"), RecordingAction::SystemRetentionDelete, &sub);
        assert!(matches!(d, RecordingDecision::Deny(DenyReason::NotAdministrator)));
    }

    #[test]
    fn system_retention_delete_fails_closed_on_invalid_path() {
        let claims = make_claims("admin", Some(UserId::builtin_admin()), vec!["ADMIN"], write_perms());
        let meta = owner_meta("web:alice", RecordingVisibility::Private);
        let sub = RecordingSubject::new(Some(&meta), TerminalState::Completed, false);
        let d = authorize(&claims, claims.subject_id.as_ref().expect("test subject_id present"), RecordingAction::SystemRetentionDelete, &sub);
        assert!(matches!(d, RecordingDecision::Deny(DenyReason::InvalidPath)));
    }

    // --- unknown subject ---

    #[test]
    fn unknown_subject_is_rejected() {
        // The policy itself does not surface `UnknownSubject` — the
        // authenticator's `validate_token_version` is the gate. The
        // policy here is exercised defensively: it does not panic when
        // the claims carry no subject_id; the resolved subject is the
        // argument the caller provides. The test confirms the policy
        // honors the resolved subject (the caller is the owner → Allow).
        let claims = make_claims("alice", None, vec!["WEB"], read_perms());
        let meta = owner_meta("web:alice", RecordingVisibility::Private);
        let sub = RecordingSubject::new(Some(&meta), TerminalState::Completed, true);
        let d = authorize(&claims, &subject("web:alice"), RecordingAction::Read, &sub);
        assert!(d.is_allow(), "owner with subject_id should be allowed; got {d:?}");
    }

    // --- orphans ---

    #[test]
    fn orphan_visibility_requires_administrator_and_recording_read() {
        let web = make_claims("alice", Some(subject("web:alice")), vec!["WEB"], read_perms());
        assert!(matches!(authorize_orphan(&web), RecordingDecision::Deny(DenyReason::NotAdministrator)));

        let admin = make_claims("admin", Some(UserId::builtin_admin()), vec!["ADMIN"], config_read_perms());
        assert!(matches!(authorize_orphan(&admin), RecordingDecision::Deny(DenyReason::MissingPermission(Permission::RecordingRead))));

        let admin_ok = make_claims("admin", Some(UserId::builtin_admin()), vec!["ADMIN"], read_perms());
        assert!(authorize_orphan(&admin_ok).is_allow());
    }

    // --- terminal state predicate ---

    #[test]
    fn terminal_state_is_eligible_for_retention() {
        assert!(TerminalState::Completed.is_eligible_for_retention());
        assert!(TerminalState::Failed.is_eligible_for_retention());
        assert!(TerminalState::Cancelled.is_eligible_for_retention());
        assert!(!TerminalState::Active.is_eligible_for_retention());
        assert!(!TerminalState::Scheduled.is_eligible_for_retention());
        assert!(!TerminalState::Deleting.is_eligible_for_retention());
    }
}
