//! Recording-scoped WebSocket snapshot filter.
//!
//! Filtering is performed for each authenticated session. Private tasks are
//! delivered only to their owner, shared tasks to sessions with
//! `recording.read`. There is deliberately no delta message: the socket
//! publishes owner-filtered full snapshots so a partial list can never be
//! mistaken for the complete one.

use crate::recording::recording_queue::RecordingQueue;
use shared::model::{
    permission::Permission,
    recording::{RecordingMetadata, RecordingVisibility},
    Claims, QueueRevision, RecordingQuotaSummaryDto, RecordingTaskDto, UserId, CURRENT_PERMISSION_SCHEMA_VERSION,
};
use std::sync::atomic::Ordering;

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

pub fn task_visible_to(meta: &RecordingMetadata, claims: &Claims, subject_id: &UserId) -> bool {
    if claims.subject_id.is_none() || !can_view_recording(claims) {
        return false;
    }
    match meta.visibility {
        RecordingVisibility::Private => meta.owner_id() == subject_id,
        RecordingVisibility::Shared => true,
    }
}

/// Everything a session needs to render the recording library, so it never
/// has to poll a list, a quota, or an availability route.
pub struct RecordingSnapshot {
    pub revision: QueueRevision,
    pub available: bool,
    pub quota: RecordingQuotaSummaryDto,
    pub tasks: Vec<RecordingTaskDto>,
}

/// Owner-filtered full snapshot. Every task is projected through
/// `RecordingTask::to_view`, so no internal field and no foreign owner id
/// can reach the session.
pub async fn recording_snapshot(
    queue: &RecordingQueue,
    claims: &Claims,
    app_config: &tuliprox_core::model::AppConfig,
) -> RecordingSnapshot {
    let available = crate::recording::recording_supervisor::recording_enabled(app_config);
    let empty = |revision| RecordingSnapshot {
        revision,
        available,
        quota: RecordingQuotaSummaryDto::default(),
        tasks: Vec::new(),
    };
    if !can_view_recording(claims) {
        return empty(current_revision(queue));
    }
    let Some(subject) = claims.subject_id.clone() else {
        return empty(current_revision(queue));
    };
    let (revision, tasks) = queue.committed_snapshot().await;
    let quota = quota_summary_for(&subject, &tasks, app_config);
    let tasks = tasks
        .iter()
        .filter(|task| task_visible_to(&task.recording, claims, &subject))
        .map(|task| task.to_view(task.owner_id() == &subject))
        .collect();
    RecordingSnapshot { revision, available, quota, tasks }
}

/// The caller's own quota position, computed from the same committed
/// snapshot the task list came from so the two cannot disagree.
fn quota_summary_for(
    subject: &UserId,
    tasks: &[crate::recording::recording_queue::RecordingTask],
    app_config: &tuliprox_core::model::AppConfig,
) -> RecordingQuotaSummaryDto {
    let totals = crate::recording::recording_quota::compute_totals(tasks);
    let config = app_config.config.load();
    let limits = crate::recording::recording_service::quota_limits_from_config(
        config.recording().and_then(|recording| recording.quota.as_ref()),
    );
    let quota = crate::recording::recording_quota::regular_user_dto(subject, &totals, &limits, tasks);
    RecordingQuotaSummaryDto {
        private_used_bytes: quota.private.measured_bytes.saturating_add(quota.private.reserved_bytes),
        private_limit_bytes: quota.private.limit_bytes,
        shared_used_bytes: quota.shared.used_bytes,
        shared_limit_bytes: quota.shared.limit_bytes,
    }
}

fn current_revision(queue: &RecordingQueue) -> QueueRevision { QueueRevision(queue.revision.load(Ordering::SeqCst)) }

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{
        recording::{RecordingMetadata, RecordingOwner, RecordingSource, RecordingVisibility},
        Claims, UserId, CURRENT_PERMISSION_SCHEMA_VERSION,
    };

    fn claims_for(username: &str, subject: Option<UserId>, admin: bool) -> Claims {
        Claims {
            username: username.to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles: if admin { vec!["ADMIN".to_string()] } else { Vec::new() },
            permissions: Permission::RecordingRead.into(),
            pwd_version: 0,
            subject_id: subject,
            permission_schema_version: CURRENT_PERMISSION_SCHEMA_VERSION,
        }
    }

    fn alice_claims() -> Claims { claims_for("alice", Some(UserId::from("web:alice")), false) }

    fn bob_claims() -> Claims { claims_for("bob", Some(UserId::from("web:bob")), false) }

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

    fn stale_claims() -> Claims {
        let mut c = alice_claims();
        c.permission_schema_version = 0;
        c
    }

    fn meta(owner: &str, visibility: RecordingVisibility) -> RecordingMetadata {
        RecordingMetadata::new_media(
            RecordingOwner::User(UserId::from(owner)),
            visibility,
            RecordingSource::new("target", "1", "input"),
            "Movie".to_string(),
        )
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
    fn task_visible_to_rejects_no_subject() {
        assert!(!task_visible_to(
            &meta("web:alice", RecordingVisibility::Private),
            &no_subject_claims(),
            &UserId::from("web:alice")
        ));
    }

    #[test]
    fn private_recording_visible_to_owner_only() {
        let private = meta("web:alice", RecordingVisibility::Private);
        assert!(task_visible_to(&private, &alice_claims(), &UserId::from("web:alice")));
        assert!(!task_visible_to(&private, &bob_claims(), &UserId::from("web:bob")));
    }

    #[test]
    fn shared_recording_visible_to_any_with_read_perm() {
        let shared = meta("web:bob", RecordingVisibility::Shared);
        assert!(task_visible_to(&shared, &alice_claims(), &UserId::from("web:alice")));
    }

    /// An `AppConfig` with no recording block, which is all these tests
    /// need: they assert on filtering, not on quota.
    fn bare_app_config() -> tuliprox_core::model::AppConfig {
        use std::sync::Arc;
        tuliprox_core::model::AppConfig {
            config: Arc::new(arc_swap::ArcSwap::from_pointee(tuliprox_core::model::Config::default())),
            sources: Arc::new(arc_swap::ArcSwap::from_pointee(tuliprox_core::model::SourcesConfig::default())),
            hdhomerun: Arc::new(arc_swap::ArcSwapOption::empty()),
            api_proxy: Arc::new(arc_swap::ArcSwapOption::empty()),
            file_locks: Arc::new(tuliprox_core::utils::FileLockManager::default()),
            paths: Arc::new(arc_swap::ArcSwap::from_pointee(shared::model::ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(arc_swap::ArcSwapOption::empty()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(tuliprox_core::model::MediaToolCapabilities::default()),
        }
    }

    #[tokio::test]
    async fn recording_snapshot_yields_no_subject_id() {
        let queue = RecordingQueue::new();
        let snapshot = recording_snapshot(&queue, &no_subject_claims(), &bare_app_config()).await;
        assert_eq!(snapshot.revision.0, 0);
        assert!(snapshot.tasks.is_empty());
        assert_eq!(snapshot.quota, RecordingQuotaSummaryDto::default(), "a session with no subject is charged nothing");
    }

    #[tokio::test]
    async fn recording_snapshot_yields_no_recording_read_perm() {
        let queue = RecordingQueue::new();
        let snapshot = recording_snapshot(&queue, &no_read_claims(), &bare_app_config()).await;
        assert_eq!(snapshot.revision.0, 0);
        assert!(snapshot.tasks.is_empty());
    }
}
