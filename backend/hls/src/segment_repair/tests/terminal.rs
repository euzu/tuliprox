use super::*;

#[tokio::test]
async fn exhausted_window_candidate_is_never_remembered_as_joinable() {
    let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
    manager.ensure_access_lease_window(HlsAccessLeaseId("lease-a".to_string())).await;
    assert!(manager.try_select_candidate(&repair_context("lease-a", "1")).await.is_some());
    let skipped = repair_context("lease-a", "2");

    assert!(manager.try_select_or_join_candidate(&skipped).await.is_none());
    assert!(manager.try_select_or_join_candidate(&skipped).await.is_none());
    assert_eq!(manager.stats().await.checked_candidates, 1);
}
