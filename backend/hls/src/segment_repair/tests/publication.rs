use super::*;

#[tokio::test]
async fn new_repair_window_generation_allows_rechecking_candidate() {
    let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let context = repair_context("lease-a", "000001");

    manager.start_access_lease_window(lease_id.clone()).await;
    assert_eq!(selected_repair_mode(&manager, &context).await, Some(HlsSegmentRepairMode::Low));
    assert_eq!(manager.windows.read().await.checked_candidates.len(), 1);

    manager.start_access_lease_window(lease_id).await;

    assert_eq!(selected_repair_mode(&manager, &context).await, Some(HlsSegmentRepairMode::Low));
    assert_eq!(manager.windows.read().await.checked_candidates.len(), 2);
}

#[tokio::test]
async fn remove_access_lease_window_clears_window_and_generation() {
    let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    manager.start_access_lease_window(lease_id.clone()).await;

    let before = manager.stats().await;
    assert_eq!(before.windows, 1);
    assert_eq!(before.generations, 1);

    manager.remove_access_lease_window(&lease_id).await;

    let after = manager.stats().await;
    assert_eq!(after.windows, 0);
    assert_eq!(after.generations, 0);
}
