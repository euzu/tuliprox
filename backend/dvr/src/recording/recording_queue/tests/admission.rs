use super::*;

#[tokio::test]
async fn mutate_serializes_concurrent_calls() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_file = dir.path().to_path_buf();
    let queue = std::sync::Arc::new(
        RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository"),
    );
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(5));

    let mut handles = Vec::new();
    for _ in 0..5 {
        let q = std::sync::Arc::clone(&queue);
        let b = std::sync::Arc::clone(&barrier);
        handles.push(tokio::spawn(async move {
            b.wait().await;
            mutate(&q, |_candidate| Ok(())).await
        }));
    }
    for h in handles {
        assert!(h.await.expect("join").is_ok(), "all mutations should succeed");
    }
    let final_rev = queue.revision.load(Ordering::SeqCst);
    assert_eq!(final_rev, 5);
}
