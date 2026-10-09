use super::{refresh_recording_progress, scheduled_task};
use crate::recording::recording_queue::RecordingQueue;
use shared::model::{EventMessage, EventSink, RecordingKind};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tempfile::TempDir;

#[derive(Clone, Default)]
pub(in crate::recording::recording_transfer::tests) struct ProgressSink(
    pub(in crate::recording::recording_transfer::tests) Arc<AtomicUsize>,
);

impl EventSink for ProgressSink {
    fn emit(&self, event: EventMessage) {
        if matches!(event, EventMessage::RecordingProgress) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[tokio::test]
async fn live_byte_progress_is_visible_without_a_queue_revision_change() {
    let dir = TempDir::new().expect("tempdir");
    let partial = dir.path().join("capture.ts.partial");
    std::fs::write(&partial, b"growing capture").expect("write partial recording");
    let queue = RecordingQueue::new();
    *queue.active.write().await = vec![scheduled_task(RecordingKind::Live, 0, 60)];
    let events = ProgressSink::default();

    refresh_recording_progress(&queue.active, "task", &partial, &events).await;
    let (revision, tasks) = queue.committed_snapshot().await;

    assert_eq!(revision.0, 0);
    assert_eq!(tasks[0].size, b"growing capture".len() as u64);
    assert_eq!(events.0.load(Ordering::Relaxed), 1);

    refresh_recording_progress(&queue.active, "task", &partial, &events).await;
    assert_eq!(events.0.load(Ordering::Relaxed), 1, "unchanged bytes need no second event");
}
