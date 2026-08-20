//! Recording-scoped WebSocket snapshot/delta filter.
//!
//! Filtering is performed for each authenticated session. Private deltas are
//! delivered only to the owner, shared deltas to sessions with
//! `recording.read`, and legacy administrator recordings only to
//! administrators.

use std::collections::HashSet;
use std::sync::atomic::Ordering;

use shared::model::permission::Permission;
use shared::model::recording::{RecordingMetadata, RecordingOwner, RecordingVisibility};
use shared::model::{Claims, FileDownloadDto, UserId, CURRENT_PERMISSION_SCHEMA_VERSION};

use crate::api::model::download::DownloadQueue;
use shared::model::QueueRevision;

pub fn can_view_recording(claims: &Claims) -> bool {
    claims.permissions.contains(Permission::RecordingRead)
        && claims.permission_schema_version >= CURRENT_PERMISSION_SCHEMA_VERSION
}

pub fn task_visible_to(
    task_meta: Option<&RecordingMetadata>,
    claims: &Claims,
    subject_id: &UserId,
) -> bool {
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
        RecordingOwner::LegacyAdmin => {
            claims.roles.iter().any(|r| r == shared::model::ROLE_ADMIN)
        }
    }
}

pub async fn recording_snapshot(
    queue: &DownloadQueue,
    claims: &Claims,
) -> (QueueRevision, Vec<FileDownloadDto>) {
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

pub fn recording_delta(
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
    let visible = collect_visible_set(queue, claims, &subject);
    let tasks = filter_delta_by_visible_ids(delta, &visible);
    (current_revision(queue), tasks)
}

fn current_revision(queue: &DownloadQueue) -> QueueRevision {
    QueueRevision(queue.revision.load(Ordering::SeqCst))
}

fn collect_visible(
    queue: &DownloadQueue,
    out: &mut Vec<FileDownloadDto>,
    subject: &UserId,
    claims: &Claims,
) {
    let mut push = |t: &crate::api::model::FileDownload| {
        if task_visible_to(t.recording.as_ref(), claims, subject) {
            out.push(FileDownloadDto::from(t));
        }
    };
    if let Ok(q) = queue.queue.try_lock() {
        for t in q.iter() {
            push(t);
        }
    }
    if let Ok(s) = queue.scheduled.try_read() {
        for t in s.iter() {
            push(t);
        }
    }
    if let Ok(a) = queue.active.try_read() {
        if let Some(t) = a.as_ref() {
            push(t);
        }
    }
    if let Ok(f) = queue.finished.try_read() {
        for t in f.iter() {
            push(t);
        }
    }
}

fn collect_visible_set(
    queue: &DownloadQueue,
    claims: &Claims,
    subject: &UserId,
) -> HashSet<String> {
    let mut ids = HashSet::new();
    if !can_view_recording(claims) {
        return ids;
    }
    let mut tmp = Vec::new();
    collect_visible(queue, &mut tmp, subject, claims);
    for t in tmp {
        ids.insert(t.id);
    }
    ids
}

fn filter_delta_by_visible_ids(delta: &[FileDownloadDto], visible: &HashSet<String>) -> Vec<FileDownloadDto> {
    delta.iter().filter(|task| visible.contains(&task.id)).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::recording::{RecordingMetadata, RecordingOwner, RecordingVisibility};
    use shared::model::{Claims, CURRENT_PERMISSION_SCHEMA_VERSION, UserId};

    fn admin_claims() -> Claims {
        let mut c = Claims {
            username: "admin".to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles: vec!["ADMIN".to_string()],
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
            roles: Vec::new(),
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
            roles: Vec::new(),
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
