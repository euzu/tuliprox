use super::update_active_download_for_worker;
use crate::recording::{
    recording_notification::LifecycleEvent,
    recording_notification_adapter::{build_marker, decide, message_for, DispatchDecision},
    recording_queue::{QueueMutationError, RecordingTask},
};
use shared::model::{EventMessage, EventSink, RecordingKind, RecordingMetadata};
use std::sync::Arc;
use tokio::sync::RwLock;
use tuliprox_core::model::{AppConfig, MessageContent};

/// Publish a queue change. Sessions answer it by pulling an owner-filtered
/// snapshot; no task data is broadcast globally, so a session can never see
/// another user's recording.
pub(super) fn publish_recording_change<E: EventSink>(event_manager: &E) {
    event_manager.emit(EventMessage::RecordingChanged);
}

/// Announce that a running recording has grown.
///
/// A separate event from [`publish_recording_change`]: a capture can produce
/// hundreds of these a second, and a session throttles them. A state
/// transition must never be throttled, so it does not come through here.
pub(super) fn publish_recording_progress<E: EventSink>(event_manager: &E) {
    event_manager.emit(EventMessage::RecordingProgress);
}

pub(super) fn broadcast_worker_mutation<E: EventSink>(
    event_manager: &E,
    result: Result<bool, QueueMutationError>,
    action: &str,
) -> Result<bool, QueueMutationError> {
    match result {
        Ok(true) => {
            publish_recording_change(event_manager);
            Ok(true)
        }
        Ok(false) => Ok(false),
        Err(err) => Err(QueueMutationError::new(format!("{action}: {err}"))),
    }
}

pub(super) fn broadcast_required_worker_mutation<E: EventSink>(
    event_manager: &E,
    result: Result<bool, QueueMutationError>,
    action: &str,
) -> Result<(), QueueMutationError> {
    if broadcast_worker_mutation(event_manager, result, action)? {
        Ok(())
    } else {
        Err(QueueMutationError::new(format!("{action}: active task changed")))
    }
}

pub(super) async fn refresh_recording_progress<E: EventSink>(
    active: &RwLock<Vec<RecordingTask>>,
    worker_uuid: &str,
    file_path: &std::path::Path,
    event_manager: &E,
) {
    let current_size = tokio::fs::metadata(file_path).await.map_or(0, |metadata| metadata.len());
    let changed = update_active_download_for_worker(active, worker_uuid, |task| {
        if task.kind == RecordingKind::Live && task.size != current_size {
            task.size = current_size;
            true
        } else {
            false
        }
    })
    .await;
    if changed {
        publish_recording_progress(event_manager);
    }
}

pub(super) fn mark_recording_metadata_notification(
    meta: &mut RecordingMetadata,
    event: LifecycleEvent,
    failure_reason: Option<String>,
) -> RecordingNotificationPlan {
    let is_admin_owner = meta.owner.user_id().is_builtin_admin();
    match decide(meta, event, chrono::Utc::now().timestamp(), is_admin_owner, failure_reason) {
        DispatchDecision::PersistAndDeliver { payload, kind, attempted_at } => {
            meta.notification_markers.push(build_marker(kind.clone(), attempted_at));
            RecordingNotificationPlan {
                message: Some(MessageContent::RecordingLifecycle(message_for(event, &payload))),
            }
        }
        DispatchDecision::AlreadyDelivered { .. } | DispatchDecision::Suppressed { .. } => {
            RecordingNotificationPlan::empty()
        }
    }
}

pub(super) struct RecordingNotificationPlan {
    pub(super) message: Option<MessageContent>,
}

impl RecordingNotificationPlan {
    pub(super) fn empty() -> Self { Self { message: None } }
}

/// Hand a lifecycle notification off for delivery once its marker has
/// been persisted.
///
/// The marker is written inside the queue-mutation boundary, so it is
/// durable before this runs; delivery must be durable too. The
/// notification outbox owns that: it persists the entry, retries per
/// channel with backoff, and dead-letters what it cannot deliver.
///
/// A direct send is the fallback for paths that run before the supervisor
/// installs the outbox (notably unit tests), and for an outbox that refuses
/// the entry.
pub(super) fn spawn_recording_notification_after_persist(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    plan: RecordingNotificationPlan,
    persisted: bool,
) {
    if !persisted {
        return;
    }
    let Some(message) = plan.message else {
        return;
    };
    let event = tuliprox_core::model::NotificationEvent::from_content(&message);
    // A full or closed outbox hands the event back; fall through to the
    // direct send rather than dropping it outright.
    let event = match crate::recording::recording_supervisor::notification_outbox() {
        Some(outbox) => match outbox.enqueue(event) {
            None => return,
            Some(rejected) => rejected,
        },
        None => event,
    };
    let app_config = Arc::clone(app_config);
    let client = client.clone();
    tokio::spawn(async move {
        tuliprox_messaging::send_event(&app_config, &client, event).await;
    });
}
