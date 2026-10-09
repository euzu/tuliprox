use super::{
    acquire_live_hls_from_lineup, alias_pool, create_test_app_config_with_dual_provider_pool,
    create_test_app_config_with_pool, ActiveProviderManager, ConnectionKind, PlaybackLeaseRef,
};
use crate::EventManager;
use shared::{defaults::default_user_priority, utils::Internable};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tuliprox_core::model::{PlaybackKind, PlaybackRequestOutcome};

#[tokio::test]
async fn unstarted_series_retry_uses_free_alias_while_started_playback_stays_pinned() {
    let app_cfg = create_test_app_config_with_dual_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
    let input = "provider_1".intern();
    let owner = "series-playback";
    let first_addr: SocketAddr = "127.0.0.1:41010".parse().unwrap();
    let busy_addr: SocketAddr = "127.0.0.1:41011".parse().unwrap();
    let retry_addr: SocketAddr = "127.0.0.1:41012".parse().unwrap();

    let failed_start = manager
        .acquire_connection_with_lease_for_session(
            &input,
            &first_addr,
            false,
            0,
            ConnectionKind::Normal,
            Some(PlaybackLeaseRef::new(owner, PlaybackKind::Series)),
        )
        .expect("first series allocation");
    assert!(manager.should_reuse_playback_provider(owner, &input));
    manager.release_handle(&failed_start);
    assert!(!manager.should_reuse_playback_provider(owner, &input));

    let busy = manager
        .acquire_connection(&input, &busy_addr, 0, ConnectionKind::Normal)
        .expect("other playback occupies first provider");
    let retry = manager
        .acquire_connection_with_lease_for_session(
            &input,
            &retry_addr,
            false,
            0,
            ConnectionKind::Normal,
            Some(PlaybackLeaseRef::new(owner, PlaybackKind::Series)),
        )
        .expect("retry should use free alias");
    let alias = retry.allocation.get_provider_name().expect("alias name");
    assert_eq!(alias.as_ref(), "provider_2");
    assert!(manager.should_reuse_playback_provider(owner, &alias));
    manager.refresh_adaptive_playback_lease(&alias, owner, PlaybackKind::Series, 15);
    manager.confirm_identified_playback_activity(owner, retry.playback_request_id.expect("request id"));
    manager.release_handle(&retry);
    assert!(manager.should_reuse_playback_provider(owner, &alias));
    manager.release_handle(&busy);
}

#[tokio::test(start_paused = true)]
async fn finished_retry_tokens_do_not_accumulate_on_an_active_binding() -> Result<(), String> {
    let app_cfg = create_test_app_config_with_pool(2, 0);
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
    let input = "provider_1".intern();
    let first = "client|alice|42|hls|0000000000000000";
    // One request stays in flight for the whole run and keeps the binding active.
    let anchor = acquire_live_hls_from_lineup(&manager, &input, first, 50_000)?;
    let anchor_request = anchor.playback_request_id.ok_or("anchor request")?;
    manager.refresh_adaptive_playback_lease(&input, first, PlaybackKind::LiveHls, 15);
    manager.confirm_identified_playback_activity(first, anchor_request).ok_or("confirmation")?;

    let mut last_token = String::new();
    for attempt in 1..=1_000_u32 {
        last_token = format!("client|alice|42|hls|{attempt:016x}");
        let handle = acquire_live_hls_from_lineup(&manager, &input, &last_token, 50_001)?;
        assert_eq!(handle.binding_tag, anchor.binding_tag, "retries share the active binding");
        let request_id = handle.playback_request_id.ok_or("retry request")?;
        manager.refresh_adaptive_playback_lease(&input, &last_token, PlaybackKind::LiveHls, 15);
        manager.release_handle(&handle);
        manager.finish_identified_playback_request(&last_token, request_id, PlaybackRequestOutcome::Completed);
        tokio::time::advance(Duration::from_secs(1)).await;
    }

    let claims = manager.write_leases().request_token_claims(super::super::playback_lease_owner(first));
    assert!(claims <= 17, "only the anchor and recently finished retries may hold a claim, got {claims}");
    // The anchor keeps its claim while its request runs; a recent retry still has its cleanup right.
    assert!(manager.write_leases().request_token_claims(super::super::playback_lease_owner(first)) >= 2);
    assert!(manager.terminate_identified_playback_owner(&last_token));
    manager.release_handle(&anchor);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn live_hls_old_token_cleanup_cannot_mutate_rebound_retry() -> Result<(), String> {
    let (manager, input, alias) = alias_pool(2, 4);
    let old = "client|alice|42|hls|0123456789abcdef";
    let new = "client|alice|42|hls|fedcba9876543210";
    let addr = SocketAddr::from(([172, 18, 0, 9], 55_000));
    let first = manager
        .acquire_exact_connection_with_lease_for_session(
            &input,
            &addr,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(PlaybackLeaseRef::new(old, PlaybackKind::LiveHls)),
        )
        .ok_or("initial allocation")?;
    let old_request = first.playback_request_id.ok_or("old request")?;
    manager.refresh_adaptive_playback_lease(&input, old, PlaybackKind::LiveHls, 60);
    manager.confirm_identified_playback_activity(old, old_request);
    manager.release_handle(&first);
    let next = manager
        .acquire_exact_connection_with_lease_for_session(
            &alias,
            &addr,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(PlaybackLeaseRef::new(new, PlaybackKind::LiveHls)),
        )
        .ok_or("rebound allocation")?;
    let new_request = next.playback_request_id.ok_or("new request")?;
    assert_ne!(first.binding_tag, next.binding_tag);
    manager.refresh_adaptive_playback_lease(&alias, new, PlaybackKind::LiveHls, 60);
    manager.confirm_identified_playback_activity(new, new_request);
    manager.release_handle(&next);
    manager.finish_identified_playback_request(new, new_request, PlaybackRequestOutcome::Completed);
    let before = manager.binding_tag_for_owner(new);
    manager.clear_provider_reservation(old);
    manager.clear_identified_provider_reservation(old, &input, first.binding_tag);
    manager.finish_identified_playback_request(old, old_request, PlaybackRequestOutcome::ProviderFailed);
    manager.refresh_playback_lease(
        &input,
        &PlaybackLeaseRef { owner: old, kind: PlaybackKind::LiveHls, request_id: old_request },
        0,
    );
    assert!(manager.confirm_identified_playback_activity(old, old_request).is_none());
    assert_eq!(manager.binding_tag_for_owner(new), before);
    assert_eq!(manager.provider_lease_usage(&input).total(), 0);
    assert_eq!(manager.provider_lease_usage(&alias).idle, 1);
    Ok(())
}
