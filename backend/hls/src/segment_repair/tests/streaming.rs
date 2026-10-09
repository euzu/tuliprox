use super::*;

#[test]
fn repair_object_metadata_key_ignores_origin_fetch_uri_for_diagnostics() {
    let mut first = repair_context("lease-a", "1");
    first.origin_fetch_uri_for_diagnostics = "http://mirror-a.example/live/1.ts".to_string();
    let mut second = first.clone();
    second.origin_fetch_uri_for_diagnostics = "http://redirect-b.example/cdn/path/1.ts".to_string();

    assert_eq!(
        repair_object_metadata_key(&first, HlsSegmentRepairMode::Low),
        repair_object_metadata_key(&second, HlsSegmentRepairMode::Low)
    );
}

#[tokio::test]
async fn repair_window_candidate_key_ignores_origin_fetch_uri_for_diagnostics() {
    let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 2));
    manager.start_access_lease_window(HlsAccessLeaseId("lease-a".to_string())).await;
    let mut first = repair_context("lease-a", "1");
    first.origin_fetch_uri_for_diagnostics = "http://mirror-a.example/live/1.ts".to_string();
    let mut same_rendered_object_other_fetch_uri = first.clone();
    same_rendered_object_other_fetch_uri.origin_fetch_uri_for_diagnostics =
        "http://redirect-b.example/cdn/path/1.ts".to_string();
    let second_rendered_object = repair_context("lease-a", "2");

    assert_eq!(selected_repair_mode(&manager, &first).await, Some(HlsSegmentRepairMode::Low));
    assert_eq!(selected_repair_mode(&manager, &same_rendered_object_other_fetch_uri).await, None);
    assert_eq!(selected_repair_mode(&manager, &second_rendered_object).await, Some(HlsSegmentRepairMode::Low));
}
