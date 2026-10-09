use super::scheduled_task;
use crate::recording::recording_queue::RecordingQueue;
use shared::model::{RecordingKind, RecordingTaskState};
use tuliprox_core::model::RecordingConfig;

#[tokio::test]
async fn media_retry_waiting_and_limit_keep_the_transfer_cause() -> Result<(), Box<dyn std::error::Error>> {
    for kind in [RecordingKind::Vod, RecordingKind::Series] {
        let queue = RecordingQueue::new();
        let task = scheduled_task(kind, chrono::Utc::now().timestamp(), 300);
        let persisted = RecordingQueue::to_persisted(&task);
        crate::recording::recording_queue::mutate(&queue, move |candidate| {
            candidate.active = vec![persisted.clone()];
            Ok(())
        })
        .await?;
        let config = RecordingConfig::from(&shared::model::RecordingConfigDto {
            retry_max_attempts: 1,
            retry_backoff_initial_secs: 1,
            retry_backoff_max_secs: 1,
            retry_backoff_jitter_percent: 0,
            ..Default::default()
        });
        let cause = "Error while opening stream: (http://user:password@upstream.example/live/1?token=secret) Operation timed out";
        let public_cause = crate::recording::recording_worker::redact_url_tokens(cause);
        assert_eq!(public_cause, "Error while opening stream: [stream URL] Operation timed out");
        let retry = super::super::prepare_active_retry(&queue, "task", &config, &public_cause).await?;
        assert!(matches!(retry, Some(super::super::RetryCommit::Waiting { delay_secs: 1, attempts: 1 })));
        {
            let active = queue.active.read().await;
            let active = active.first().ok_or("active transfer missing")?;
            assert_eq!(active.state, RecordingTaskState::RetryWaiting);
            assert!(active.error.as_deref().is_some_and(|error| error.contains(&public_cause)));
            assert!(active.next_retry_at.is_some());
        }
        let retry = super::super::prepare_active_retry(&queue, "task", &config, &public_cause).await?;
        assert!(matches!(retry, Some(super::super::RetryCommit::Failed(_))));
        let finished = queue.finished.read().await;
        let failed = finished.first().ok_or("terminal transfer missing")?;
        assert_eq!(failed.state, RecordingTaskState::Failed);
        assert_eq!(
            failed.error.as_deref(),
            Some(format!("Retry limit reached after 1 attempts: {public_cause}").as_str())
        );
        assert_eq!(failed.recording.reserved_bytes, 0);
        assert!(failed.next_retry_at.is_none());
    }
    Ok(())
}
