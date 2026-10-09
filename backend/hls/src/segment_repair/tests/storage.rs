use super::*;

#[test]
fn repair_log_identity_fields_separate_content_proxy_and_lease_identities() {
    let context = repair_context("lease-a", "000001");
    let fields = context.log_identity_fields();

    assert_eq!(
        fields,
        format!(
            "session={} proxy_session={} lease={}",
            context.log_identity.session(),
            context.log_identity.proxy_session(),
            super::super::safe_hls_access_lease_id(context.hls_access_lease_id.as_ref().expect("test lease"))
        )
    );
    assert_ne!(context.log_identity.session(), context.log_identity.proxy_session());
}

#[tokio::test]
async fn on_demand_selection_joins_the_prewarm_candidate_identity() {
    let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
    manager.ensure_access_lease_window(HlsAccessLeaseId("lease-a".to_string())).await;
    let context = repair_context("lease-a", "1");

    let (prewarm_mode, _, prewarm_candidate) = manager.try_select_candidate(&context).await.expect("prewarm candidate");
    let (demand_mode, _, demand_candidate) =
        manager.try_select_or_join_candidate(&context).await.expect("demand joins prewarm");

    assert_eq!(demand_mode, prewarm_mode);
    assert_eq!(demand_candidate, prewarm_candidate);
    assert_eq!(manager.windows.read().await.windows[&HlsAccessLeaseId("lease-a".to_string())].remaining_segments, 0);
}
