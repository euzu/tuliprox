use super::*;

#[tokio::test]
async fn gc_keeps_fetching_segments() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
        session.segments.get_mut(&1).expect("segment").status =
            SegmentCacheStatus::Fetching { priority: SegmentFetchPriority::Prefetch, started_at_ms: 1 };
    }

    let report = gc.run_once(10_000).await.expect("gc should run");

    assert_eq!(report.segments_deleted_duration, 0);
    assert!(session.read().await.segments.contains_key(&1));
}
