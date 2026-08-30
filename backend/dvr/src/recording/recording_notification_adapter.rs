//! At-most-once recording notification adapter.
//!
//! This module owns the *decision* surface. The queue-mutation
//! boundary persists the marker; the messaging transport layer
//! performs the actual delivery. The adapter is the bridge
//! between them.

use super::recording_notification::{route, LifecycleEvent, LifecyclePayload, RoutingDecision};
use shared::model::{
    recording::{NotificationMarker, NotificationMarkerKind, RecordingMetadata},
    MsgKind,
};
use tuliprox_core::model::RecordingLifecycleMessage;

/// The dispatch decision the adapter returns. The caller
/// (queue-mutation boundary + Tokio runtime) is responsible for
/// executing the side effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchDecision {
    /// No marker present and routing permits delivery. The caller
    /// must persist the marker and then enqueue the notification.
    PersistAndDeliver { payload: LifecyclePayload, kind: NotificationMarkerKind, attempted_at: i64 },
    /// The marker is already present (startup, recovery, or
    /// duplicate transition hook). No further action.
    AlreadyDelivered { kind: NotificationMarkerKind },
    /// Routing suppressed the event (private, non-admin). The
    /// caller records the suppression for observability but does
    /// not persist a marker or deliver.
    Suppressed { reason: &'static str },
}

/// Pure: decide what to do for a transition. The caller is the
/// queue-mutation boundary; this function never mutates
/// `metadata`. The `attempted_at` is the same UTC second the
/// caller will persist; the adapter does not read the clock.
pub fn decide(
    metadata: &RecordingMetadata,
    event: LifecycleEvent,
    attempted_at: i64,
    is_admin_role: bool,
    failure_reason: Option<String>,
) -> DispatchDecision {
    let kind = match event {
        LifecycleEvent::Started => NotificationMarkerKind::Started,
        LifecycleEvent::Completed => NotificationMarkerKind::Completed,
        LifecycleEvent::Failed => NotificationMarkerKind::Failed,
    };
    if metadata.notification_markers.iter().any(|m| m.kind == kind) {
        return DispatchDecision::AlreadyDelivered { kind };
    }
    if route(&metadata.owner, metadata.visibility, is_admin_role) == RoutingDecision::Suppress {
        return DispatchDecision::Suppressed { reason: "private non-admin owner" };
    }
    let failure_reason = (event == LifecycleEvent::Failed).then_some(failure_reason).flatten();
    let payload = LifecyclePayload::from_metadata(metadata, failure_reason);
    DispatchDecision::PersistAndDeliver { payload, kind, attempted_at }
}

pub fn message_for(event: LifecycleEvent, payload: &LifecyclePayload) -> RecordingLifecycleMessage {
    RecordingLifecycleMessage {
        event: match event {
            LifecycleEvent::Started => MsgKind::RecordingStarted,
            LifecycleEvent::Completed => MsgKind::RecordingCompleted,
            LifecycleEvent::Failed => MsgKind::RecordingFailed,
        },
        programme_title: payload.programme_title.clone(),
        channel: payload.channel.clone(),
        effective_start: payload.effective_start,
        effective_end: payload.effective_end,
        visibility: payload.visibility.map(|visibility| match visibility {
            shared::model::recording::RecordingVisibility::Private => "private".to_string(),
            shared::model::recording::RecordingVisibility::Shared => "shared".to_string(),
        }),
        output_filename: payload.output_filename.clone(),
        failure_reason: payload.failure_reason.clone(),
    }
}

/// Pure: build the marker the queue-mutation boundary persists.
/// Centralized so the caller's `mutate` closure has a single
/// shape to insert.
pub fn build_marker(kind: NotificationMarkerKind, attempted_at: i64) -> NotificationMarker {
    NotificationMarker::new(kind, attempted_at)
}

/// Detect duplicate transition hooks in unit tests.
#[cfg(test)]
pub fn is_duplicate_transition(markers: &[NotificationMarker], event: LifecycleEvent) -> bool {
    let kind = match event {
        LifecycleEvent::Started => NotificationMarkerKind::Started,
        LifecycleEvent::Completed => NotificationMarkerKind::Completed,
        LifecycleEvent::Failed => NotificationMarkerKind::Failed,
    };
    markers.iter().any(|m| m.kind == kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{
        recording::{NotificationMarkerKind, RecordingMetadata, RecordingOwner, RecordingVisibility},
        UserId,
    };

    fn user(name: &str) -> UserId { UserId::from(name) }

    #[inline]
    fn make_meta(visibility: RecordingVisibility, owner: RecordingOwner) -> RecordingMetadata {
        crate::recording::make_test_meta(visibility, owner, Some("path/file.ts"))
    }

    #[test]
    fn fresh_event_for_global_target_persists_and_delivers() {
        let meta = make_meta(RecordingVisibility::Shared, RecordingOwner::User(user("web:alice")));
        let d = decide(&meta, LifecycleEvent::Started, 1_000, false, None);
        assert!(matches!(d, DispatchDecision::PersistAndDeliver { kind: NotificationMarkerKind::Started, .. }));
    }

    #[test]
    fn marker_already_present_short_circuits() {
        let mut meta = make_meta(RecordingVisibility::Shared, RecordingOwner::User(user("web:alice")));
        meta.notification_markers.push(NotificationMarker::new(NotificationMarkerKind::Started, 500));
        let d = decide(&meta, LifecycleEvent::Started, 1_000, false, None);
        assert!(matches!(d, DispatchDecision::AlreadyDelivered { kind: NotificationMarkerKind::Started }));
    }

    #[test]
    fn private_non_admin_is_suppressed() {
        let meta = make_meta(RecordingVisibility::Private, RecordingOwner::User(user("web:alice")));
        let d = decide(&meta, LifecycleEvent::Completed, 1_000, false, None);
        assert!(matches!(d, DispatchDecision::Suppressed { .. }));
    }

    #[test]
    fn administrator_own_private_is_delivered() {
        let meta = make_meta(RecordingVisibility::Private, RecordingOwner::User(user("builtin:admin")));
        let d = decide(&meta, LifecycleEvent::Started, 1_000, true, None);
        assert!(matches!(d, DispatchDecision::PersistAndDeliver { .. }));
    }

    #[test]
    fn legacy_admin_owner_always_delivers() {
        let meta = make_meta(RecordingVisibility::Private, RecordingOwner::LegacyAdmin);
        let d = decide(&meta, LifecycleEvent::Failed, 1_000, false, Some("recording failed".to_string()));
        match d {
            DispatchDecision::PersistAndDeliver { kind, attempted_at, .. } => {
                assert_eq!(kind, NotificationMarkerKind::Failed);
                assert_eq!(attempted_at, 1_000);
            }
            _ => panic!("expected PersistAndDeliver"),
        }
    }

    #[test]
    fn failed_event_carries_failure_reason() {
        let meta = make_meta(RecordingVisibility::Shared, RecordingOwner::User(user("web:alice")));
        let d = decide(&meta, LifecycleEvent::Failed, 1_000, false, Some("encoder died".to_string()));
        match d {
            DispatchDecision::PersistAndDeliver { payload, .. } => {
                let fields = payload.template_fields();
                let get = |k: &str| fields.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
                assert_eq!(get("failure_reason").as_deref(), Some("encoder died"));
            }
            _ => panic!("expected PersistAndDeliver"),
        }
    }

    #[test]
    fn duplicate_transition_detected() {
        let markers = vec![NotificationMarker::new(NotificationMarkerKind::Started, 500)];
        assert!(is_duplicate_transition(&markers, LifecycleEvent::Started));
        assert!(!is_duplicate_transition(&markers, LifecycleEvent::Completed));
    }

    #[test]
    fn build_marker_preserves_kind_and_timestamp() {
        let m = build_marker(NotificationMarkerKind::Completed, 1_000);
        assert_eq!(m.kind, NotificationMarkerKind::Completed);
        assert_eq!(m.attempted_at, 1_000);
    }
}
