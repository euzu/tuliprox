//! Recording-scoped WebSocket snapshot/delta filter.
//!
//! Filtering is performed for each authenticated session. Private deltas are
//! delivered only to the owner, shared deltas to sessions with
//! `recording.read`, and legacy administrator recordings only to
//! administrators.

use crate::download::DownloadQueue;
use shared::model::{
    permission::Permission,
    recording::{RecordingMetadata, RecordingOwner, RecordingVisibility},
    Claims, FileDownloadDto, QueueRevision, UserId, CURRENT_PERMISSION_SCHEMA_VERSION,
};
use std::{collections::HashSet, sync::atomic::Ordering};

/// Why a session may not see recordings.
///
/// The two reasons need different handling and used to be collapsed into
/// one `false`: the socket returned an empty list either way, so a user
/// whose token predated a permission-schema bump saw "no recordings"
/// forever with nothing to act on, while the REST routes were correctly
/// answering `recording_token_refresh_required`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingViewDenial {
    /// The token predates the current permission schema, so its
    /// permission bits cannot be trusted. The client must refresh and
    /// reconnect. Actionable, and therefore reported to the client.
    TokenRefreshRequired,
    /// The principal simply has no `recording.read`. Not actionable and
    /// not an error — an empty list is the honest answer.
    NotPermitted,
}

impl RecordingViewDenial {
    /// Stable wire code, matching the REST error codes so the frontend
    /// maps both surfaces through one table.
    pub fn code(self) -> &'static str {
        match self {
            Self::TokenRefreshRequired => "recording_token_refresh_required",
            Self::NotPermitted => "recording_forbidden",
        }
    }
}

/// `None` when the session may see recordings.
///
/// Staleness is checked *before* the permission bit on purpose: a token
/// from an older schema cannot be trusted to describe permissions at all,
/// so "refresh" is the truthful answer even when the stale token also
/// happens to lack the bit.
pub fn recording_view_denial(claims: &Claims) -> Option<RecordingViewDenial> {
    if claims.permission_schema_version < CURRENT_PERMISSION_SCHEMA_VERSION {
        return Some(RecordingViewDenial::TokenRefreshRequired);
    }
    if !claims.permissions.contains(Permission::RecordingRead) {
        return Some(RecordingViewDenial::NotPermitted);
    }
    None
}

pub fn can_view_recording(claims: &Claims) -> bool { recording_view_denial(claims).is_none() }

pub fn task_visible_to(task_meta: Option<&RecordingMetadata>, claims: &Claims, subject_id: &UserId) -> bool {
    let Some(meta) = task_meta else {
        return false;
    };
    if claims.subject_id.is_none() {
        return false;
    }
    if !can_view_recording(claims) {
        return false;
    }
    match &meta.owner {
        RecordingOwner::User(owner) => match meta.visibility {
            RecordingVisibility::Private => owner == subject_id,
            RecordingVisibility::Shared => true,
        },
        RecordingOwner::LegacyAdmin => claims.is_admin(),
    }
}

pub async fn recording_snapshot(queue: &DownloadQueue, claims: &Claims) -> (QueueRevision, Vec<FileDownloadDto>) {
    if !can_view_recording(claims) {
        return (current_revision(queue), Vec::new());
    }
    let Some(subject) = claims.subject_id.clone() else {
        return (current_revision(queue), Vec::new());
    };
    let (revision, tasks) = queue.committed_snapshot().await;
    let tasks = tasks
        .iter()
        .filter(|task| task_visible_to(task.recording.as_ref(), claims, &subject))
        .map(FileDownloadDto::from)
        .collect();
    (revision, tasks)
}

pub async fn recording_delta(
    queue: &DownloadQueue,
    claims: &Claims,
    delta: &[FileDownloadDto],
) -> (QueueRevision, Vec<FileDownloadDto>) {
    if !can_view_recording(claims) {
        return (current_revision(queue), Vec::new());
    }
    let Some(subject) = claims.subject_id.clone() else {
        return (current_revision(queue), Vec::new());
    };
    let (revision, visible) = collect_visible_set(queue, claims, &subject).await;
    let tasks = filter_delta_by_visible_ids(delta, &visible);
    (revision, tasks)
}

fn current_revision(queue: &DownloadQueue) -> QueueRevision { QueueRevision(queue.revision.load(Ordering::SeqCst)) }

/// The ids the session may see, plus the revision they belong to.
///
/// This used to `try_lock` / `try_read` all four queue guards and skip
/// whichever one was contended — under mutation load the visible set
/// came back partial or empty, so `filter_delta_by_visible_ids` dropped
/// legitimate tasks and the client watched its recordings disappear
/// until the next full snapshot. It now waits for the same committed
/// boundary `recording_snapshot` uses, so the set is consistent and
/// complete, and the revision it is valid for is returned with it.
async fn collect_visible_set(
    queue: &DownloadQueue,
    claims: &Claims,
    subject: &UserId,
) -> (QueueRevision, HashSet<String>) {
    if !can_view_recording(claims) {
        return (current_revision(queue), HashSet::new());
    }
    let (revision, tasks) = queue.committed_snapshot().await;
    let ids = tasks
        .iter()
        .filter(|task| task_visible_to(task.recording.as_ref(), claims, subject))
        .map(|task| task.uuid.clone())
        .collect();
    (revision, ids)
}

fn filter_delta_by_visible_ids(delta: &[FileDownloadDto], visible: &HashSet<String>) -> Vec<FileDownloadDto> {
    delta.iter().filter(|task| visible.contains(&task.id)).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{
        recording::{RecordingMetadata, RecordingOwner, RecordingVisibility},
        Claims, UserId, CURRENT_PERMISSION_SCHEMA_VERSION,
    };

    fn admin_claims() -> Claims {
        let mut c = Claims {
            username: "admin".to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles: shared::model::RoleSet::ADMIN,
            permissions: Permission::RecordingRead.into(),
            pwd_version: 0,
            subject_id: Some(UserId::builtin_admin()),
            permission_schema_version: CURRENT_PERMISSION_SCHEMA_VERSION,
        };
        let _ = &mut c;
        c
    }

    fn alice_claims() -> Claims {
        Claims {
            username: "alice".to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles: shared::model::RoleSet::new(),
            permissions: Permission::RecordingRead.into(),
            pwd_version: 0,
            subject_id: Some(UserId::from("web:alice")),
            permission_schema_version: CURRENT_PERMISSION_SCHEMA_VERSION,
        }
    }

    fn no_read_claims() -> Claims {
        let mut c = alice_claims();
        c.permissions = Permission::ConfigRead.into();
        c
    }

    fn no_subject_claims() -> Claims {
        let mut c = alice_claims();
        c.subject_id = None;
        c
    }

    #[test]
    fn delta_filter_uses_task_ids_not_titles() {
        let visible = std::collections::HashSet::from(["visible".to_string()]);
        let delta = vec![FileDownloadDto {
            id: "hidden".to_string(),
            title: "same-title.ts".to_string(),
            kind: shared::model::TaskKindDto::Recording,
            priority: shared::model::TaskPriorityDto::Normal,
            status: shared::model::TransferStatusDto::Scheduled,
            retry_attempts: 0,
            downloaded_bytes: 0,
            total_bytes: None,
            next_retry_at: None,
            scheduled_start_at: None,
            duration_secs: None,
            error: None,
            recording: None,
        }];

        assert!(filter_delta_by_visible_ids(&delta, &visible).is_empty());
    }

    fn stale_claims() -> Claims {
        let mut c = alice_claims();
        c.permission_schema_version = 0;
        c
    }

    fn meta_private_alice() -> RecordingMetadata {
        let mut m = RecordingMetadata::for_legacy_admin(0, 60);
        m.owner = RecordingOwner::User(UserId::from("web:alice"));
        m.visibility = RecordingVisibility::Private;
        m
    }

    fn meta_shared_bob() -> RecordingMetadata {
        let mut m = RecordingMetadata::for_legacy_admin(0, 60);
        m.owner = RecordingOwner::User(UserId::from("web:bob"));
        m.visibility = RecordingVisibility::Shared;
        m
    }

    fn meta_legacy_admin() -> RecordingMetadata {
        let mut m = RecordingMetadata::for_legacy_admin(0, 60);
        m.owner = RecordingOwner::LegacyAdmin;
        m.visibility = RecordingVisibility::Private;
        m
    }

    #[test]
    fn can_view_recording_requires_perm_and_schema() {
        assert!(can_view_recording(&alice_claims()));
        assert!(!can_view_recording(&no_read_claims()));
        assert!(!can_view_recording(&stale_claims()));
    }

    #[test]
    fn denial_distinguishes_a_stale_token_from_a_missing_permission() {
        assert_eq!(recording_view_denial(&alice_claims()), None);
        assert_eq!(recording_view_denial(&no_read_claims()), Some(RecordingViewDenial::NotPermitted));
        assert_eq!(recording_view_denial(&stale_claims()), Some(RecordingViewDenial::TokenRefreshRequired));
    }

    #[test]
    fn a_stale_token_reports_refresh_even_without_the_permission_bit() {
        // The bits of a pre-schema-bump token mean nothing; telling the
        // user "forbidden" would send them to an administrator when all
        // they need is a new token.
        let mut claims = no_read_claims();
        claims.permission_schema_version = 0;
        assert_eq!(recording_view_denial(&claims), Some(RecordingViewDenial::TokenRefreshRequired));
    }

    #[test]
    fn denial_codes_match_the_rest_error_codes() {
        assert_eq!(RecordingViewDenial::TokenRefreshRequired.code(), "recording_token_refresh_required");
        assert_eq!(RecordingViewDenial::NotPermitted.code(), "recording_forbidden");
    }

    #[test]
    fn task_visible_to_rejects_non_recording() {
        let claims = alice_claims();
        assert!(!task_visible_to(None, &claims, &UserId::from("web:alice")));
    }

    #[test]
    fn task_visible_to_rejects_no_subject() {
        let claims = no_subject_claims();
        assert!(!task_visible_to(Some(&meta_private_alice()), &claims, &UserId::from("web:alice")));
    }

    #[test]
    fn private_recording_visible_to_owner_only() {
        let alice = alice_claims();
        let bob = Claims {
            username: "bob".to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles: shared::model::RoleSet::new(),
            permissions: Permission::RecordingRead.into(),
            pwd_version: 0,
            subject_id: Some(UserId::from("web:bob")),
            permission_schema_version: CURRENT_PERMISSION_SCHEMA_VERSION,
        };
        assert!(task_visible_to(Some(&meta_private_alice()), &alice, &UserId::from("web:alice")));
        assert!(!task_visible_to(Some(&meta_private_alice()), &bob, &UserId::from("web:bob")));
    }

    #[test]
    fn shared_recording_visible_to_any_with_read_perm() {
        let alice = alice_claims();
        assert!(task_visible_to(Some(&meta_shared_bob()), &alice, &UserId::from("web:alice")));
    }

    #[test]
    fn legacy_admin_recording_visible_to_admins_only() {
        let admin = admin_claims();
        let non_admin = alice_claims();
        assert!(task_visible_to(Some(&meta_legacy_admin()), &admin, &UserId::builtin_admin()));
        assert!(!task_visible_to(Some(&meta_legacy_admin()), &non_admin, &UserId::from("web:alice")));
    }

    #[tokio::test]
    async fn recording_snapshot_yields_no_subject_id() {
        let queue = DownloadQueue::new();
        let (rev, tasks) = recording_snapshot(&queue, &no_subject_claims()).await;
        assert_eq!(rev.0, 0);
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn recording_snapshot_yields_no_recording_read_perm() {
        let queue = DownloadQueue::new();
        let (rev, tasks) = recording_snapshot(&queue, &no_read_claims()).await;
        assert_eq!(rev.0, 0);
        assert!(tasks.is_empty());
    }
}
