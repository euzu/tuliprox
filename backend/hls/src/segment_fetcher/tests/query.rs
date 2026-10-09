use super::{
    commit_test_key_ready, encrypted_fetch_context, shared_key_fetch_and_wait, spawn_segment_server,
    wait_for_segment_key_dependency, SegmentFetchPolicy,
};
use futures::poll;
use std::{task::Poll, time::Duration};
use tuliprox_core::utils::current_time_millis;

#[tokio::test]
async fn registered_shared_key_waiter_observes_controlled_ready_transition() {
    let server = spawn_segment_server(0).await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy = SegmentFetchPolicy::default();
    let (_, context, _) = encrypted_fetch_context(&server, &temp_dir, &policy).await;
    let now_ms = current_time_millis();
    let (fetch_token, notifier, resource_id, extension) = shared_key_fetch_and_wait(&context, now_ms).await;
    let mut wait =
        Box::pin(wait_for_segment_key_dependency(&context, notifier, resource_id, extension, Duration::from_secs(1)));
    assert!(matches!(poll!(wait.as_mut()), Poll::Pending));

    commit_test_key_ready(&context, &fetch_token, now_ms.saturating_add(1)).await;

    assert!(matches!(poll!(wait.as_mut()), Poll::Ready(Ok(()))));
}
