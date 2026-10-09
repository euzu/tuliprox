use super::{
    acquire_live_hls_from_lineup, alias_pool, build_test_app_config, confirmed_alias_request,
    create_test_app_config_single_provider_pool, create_test_app_config_with_dual_provider_pool,
    create_test_app_config_with_pool, live_hls_playback_on_alias_with_lapsed_lease, ActiveProviderManager,
    ConnectionKind, PlaybackLeaseRef,
};
use crate::{ActiveUserManager, EventManager};
use arc_swap::ArcSwapOption;
use shared::{
    defaults::{default_probe_user_priority, default_user_priority},
    utils::Internable,
};
use std::{collections::HashSet, net::SocketAddr, sync::Arc, time::Duration};
use tuliprox_core::model::{Config, PlaybackKind, PlaybackRequestOutcome, SharedSubscriberId};

#[tokio::test]
async fn forced_reopen_and_late_cleanup_preserve_other_proxy_requests() -> Result<(), Box<dyn std::error::Error>> {
    let manager = ActiveProviderManager::new(&create_test_app_config_with_pool(2, 3), &Arc::new(EventManager::new()));
    let addr = SocketAddr::from(([127, 0, 0, 1], 50002));
    let input = Arc::from("provider_1");
    let first = manager
        .acquire_connection_with_grace_for_session(&input, &addr, false, 0, ConnectionKind::Normal, Some("first"))
        .ok_or("first allocation missing")?;
    let other = manager
        .acquire_connection_with_grace_for_session(&input, &addr, false, 0, ConnectionKind::Normal, Some("other"))
        .ok_or("other allocation missing")?;
    manager.release_playback_connections("first", &[addr]);
    assert!(first.cancel_token.as_ref().is_some_and(tokio_util::sync::CancellationToken::is_cancelled));
    assert!(!other.cancel_token.as_ref().is_some_and(tokio_util::sync::CancellationToken::is_cancelled));
    assert_eq!(manager.get_provider_connections_count(), 1);
    let replacement = manager
        .acquire_connection_with_grace_for_session(&input, &addr, false, 0, ConnectionKind::Normal, Some("first"))
        .ok_or("replacement missing")?;
    manager.refresh_adaptive_playback_lease(&input, "first", tuliprox_core::model::PlaybackKind::LiveHls, 15);
    manager.confirm_playback_activity("first");
    manager.finish_playback_request("first", PlaybackRequestOutcome::ProviderFailed, None);
    assert_eq!(manager.provider_lease_usage(&input).active, 1);
    manager.release_handle(&replacement);
    manager.release_handle(&other);
    Ok(())
}

#[tokio::test]
async fn release_snapshot_tracks_original_allocation_after_addr_reuse() {
    let app_cfg = create_test_app_config_with_pool(2, 3);
    let events = Arc::new(EventManager::new());
    let provider = ActiveProviderManager::new(&app_cfg, &events);
    let addr = SocketAddr::from(([127, 0, 0, 1], 50_022));
    let input = "provider_1".intern();
    let original = provider
        .acquire_connection_with_grace_for_session(&input, &addr, false, 0, ConnectionKind::Normal, Some("old"))
        .expect("original allocation");
    let original_snapshot = provider.release_snapshot_for_addr(&addr);
    assert!(!provider.wait_for_snapshot_release(&original_snapshot, Duration::ZERO).await);

    provider.release_handle(&original);
    let replacement = provider
        .acquire_connection_with_grace_for_session(&input, &addr, false, 0, ConnectionKind::Normal, Some("new"))
        .expect("replacement allocation");
    let replacement_snapshot = provider.release_snapshot_for_addr(&addr);
    assert!(provider.wait_for_snapshot_release(&original_snapshot, Duration::ZERO).await);
    assert!(!provider.wait_for_snapshot_release(&replacement_snapshot, Duration::ZERO).await);

    let geoip = Arc::new(ArcSwapOption::default());
    let users = ActiveUserManager::new(&Config::default(), &geoip, &events);
    users.set_pending_provider_release("user", original_snapshot.clone()).await;
    users.set_pending_provider_release("user", replacement_snapshot.clone()).await;
    assert!(!users.clear_pending_provider_release("user", &original_snapshot).await);
    assert_eq!(users.pending_provider_release("user").await, Some(replacement_snapshot.clone()));
    assert!(users.clear_pending_provider_release("user", &replacement_snapshot).await);
    assert_eq!(users.pending_provider_release("user").await, None);
    provider.release_handle(&replacement);
}

#[tokio::test]
async fn release_snapshot_tracks_shared_subscriber_without_waiting_for_other_subscribers() {
    let app_cfg = create_test_app_config_single_provider_pool();
    let events = Arc::new(EventManager::new());
    let provider = ActiveProviderManager::new(&app_cfg, &events);
    let input = "provider_1".intern();
    let first_addr = SocketAddr::from(([127, 0, 0, 1], 50_023));
    let second_addr = SocketAddr::from(([127, 0, 0, 1], 50_024));
    let first = SharedSubscriberId::from_stream_uid(50_023);
    let second = SharedSubscriberId::from_stream_uid(50_024);
    let origin = provider.acquire_connection(&input, &first_addr, 0, ConnectionKind::Normal).expect("shared origin");
    let before_promotion = provider.release_snapshot_for_addr(&first_addr);
    assert!(provider.make_shared_connection(&origin, "shared-release", first));
    provider
        .add_shared_connection(&second_addr, second, "shared-release", 0, ConnectionKind::Normal)
        .expect("second subscriber");
    let snapshot = provider.release_snapshot_for_addr(&first_addr);
    assert!(!provider.wait_for_snapshot_release(&before_promotion, Duration::ZERO).await);
    assert!(!provider.wait_for_snapshot_release(&snapshot, Duration::ZERO).await);

    provider.release_connection(&first_addr);
    assert!(provider.wait_for_snapshot_release(&before_promotion, Duration::ZERO).await);
    assert!(provider.wait_for_snapshot_release(&snapshot, Duration::ZERO).await);
    assert_eq!(provider.get_provider_connections_count(), 1);
    provider.release_connection(&second_addr);
}

#[tokio::test]
async fn test_probe_preemption_releases_capacity_and_cancels_immediately() {
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let user_addr: SocketAddr = "127.0.0.1:43001".parse().unwrap();

    let probe_handle = manager
        .acquire_connection_for_probe(&input_name, default_probe_user_priority())
        .expect("probe allocation should succeed");
    let probe_token = probe_handle.cancel_token.clone().expect("probe handle must carry cancel token");

    // User request should preempt probe and immediately acquire released capacity.
    let user_alloc = manager
        .acquire_connection_with_grace(&input_name, &user_addr, false, default_user_priority(), ConnectionKind::Normal)
        .expect("user allocation should preempt probe");
    assert_eq!(user_alloc.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

    // Cancellation happens inline during preemption; yield once to observe any deferred work.
    tokio::task::yield_now().await;
    assert!(probe_token.is_cancelled(), "probe token should be cancelled immediately after preemption");

    manager.release_connection(&user_addr);
}

#[tokio::test(start_paused = true)]
async fn test_session_provider_reservation_blocks_other_sessions_until_ttl_expires() {
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let owner_1 = "session-owner-1";
    let owner_2 = "session-owner-2";
    let addr_1: SocketAddr = "127.0.0.1:43101".parse().unwrap();
    let addr_2: SocketAddr = "127.0.0.1:43102".parse().unwrap();

    manager.refresh_provider_reservation(&input_name, owner_1, 15);
    manager.confirm_playback_activity(owner_1);

    let first = manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &addr_1,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(owner_1),
        )
        .expect("reserved owner should reacquire its provider");
    assert_eq!(first.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));
    manager.release_connection(&addr_1);

    let blocked = manager.acquire_connection_with_grace_for_session(
        &input_name,
        &addr_2,
        false,
        default_user_priority(),
        ConnectionKind::Normal,
        Some(owner_2),
    );
    assert!(blocked.is_none(), "other sessions must not take a reserved provider before TTL expiry");

    tokio::time::advance(Duration::from_secs(16)).await;

    let second = manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &addr_2,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(owner_2),
        )
        .expect("reservation should expire after TTL");
    assert_eq!(second.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));
    manager.release_connection(&addr_2);
}

#[tokio::test(start_paused = true)]
async fn different_session_cannot_take_idle_reservation_from_same_client_family() {
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let Ok(second_addr) = "127.0.0.1:43112".parse() else {
        return;
    };
    let owner_channel_1 = "client|ua|user|100";
    let owner_channel_2 = "client|ua|user|200";

    manager.refresh_provider_reservation(&input_name, owner_channel_1, 15);
    manager.confirm_playback_activity(owner_channel_1);

    let blocked = manager.acquire_connection_with_grace_for_session(
        &input_name,
        &second_addr,
        false,
        default_user_priority(),
        ConnectionKind::Normal,
        Some(owner_channel_2),
    );
    assert!(blocked.is_none(), "a related but distinct playback must not steal an idle reservation");

    manager.clear_provider_reservation(owner_channel_1);
    let acquired = manager.acquire_connection_with_grace_for_session(
        &input_name,
        &second_addr,
        false,
        default_user_priority(),
        ConnectionKind::Normal,
        Some(owner_channel_2),
    );
    assert!(acquired.is_some(), "explicitly clearing the old playback should release its provider");
    manager.release_connection(&second_addr);
}

#[tokio::test(start_paused = true)]
async fn test_clear_provider_reservation_releases_family_block() {
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let owner_1 = "session-owner-1";
    let owner_2 = "session-owner-2";
    let addr_2: SocketAddr = "127.0.0.1:43132".parse().unwrap();

    manager.refresh_provider_reservation(&input_name, owner_1, 15);
    manager.confirm_playback_activity(owner_1);

    let blocked = manager.acquire_connection_with_grace_for_session(
        &input_name,
        &addr_2,
        false,
        default_user_priority(),
        ConnectionKind::Normal,
        Some(owner_2),
    );
    assert!(blocked.is_none(), "reservation should initially block another session");

    manager.clear_provider_reservation(owner_1);

    let acquired = manager.acquire_connection_with_grace_for_session(
        &input_name,
        &addr_2,
        false,
        default_user_priority(),
        ConnectionKind::Normal,
        Some(owner_2),
    );
    assert!(acquired.is_some(), "clearing reservation should unblock the provider");
}

#[tokio::test]
async fn shared_promotion_and_release_preserve_other_allocations_on_same_socket(
) -> Result<(), Box<dyn std::error::Error>> {
    let app_cfg = build_test_app_config(None, 3);
    let events = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &events);
    let addr = "127.0.0.1:48500".parse()?;
    let input = "provider_1".intern();
    let unrelated =
        manager.acquire_connection(&input, &addr, 0, ConnectionKind::Normal).ok_or("unrelated allocation missing")?;
    let origin =
        manager.acquire_connection(&input, &addr, 5, ConnectionKind::Normal).ok_or("origin allocation missing")?;
    let first = SharedSubscriberId::from_stream_uid(1);
    let second = SharedSubscriberId::from_stream_uid(2);
    let key = "https://example.invalid/shared.ts";
    assert!(manager.make_shared_connection(&origin, key, first));
    assert_eq!(manager.get_provider_connections_count(), 2);
    assert!(manager.read_connections().single.contains_key(&unrelated.allocation_id));
    assert!(manager
        .connections
        .read()
        .unwrap()
        .single_by_addr
        .get(&addr)
        .is_some_and(|allocs| allocs.contains(&unrelated.allocation_id)));
    assert!(!unrelated.cancel_token.as_ref().is_some_and(tokio_util::sync::CancellationToken::is_cancelled));
    manager.add_shared_connection(&addr, second, key, 9, ConnectionKind::Soft)?;
    assert!(manager.reclassify_shared_connection(second, ConnectionKind::Normal, 1));
    {
        let connections = manager.read_connections();
        let shared = connections.shared.by_key.get(key).ok_or("shared origin missing")?;
        assert_eq!(shared.connections.get(&first).map(|subscriber| subscriber.priority), Some(5));
        assert_eq!(shared.priority, 1);
    }
    manager.release_shared_connection(second);
    manager.release_shared_connection(second);
    assert_eq!(manager.get_provider_connections_count(), 2);
    assert_eq!(manager.read_connections().shared.by_key.get(key).map(|shared| shared.priority), Some(5));
    manager.release_shared_connection(first);
    manager.release_handle(&origin);
    assert_eq!(manager.get_provider_connections_count(), 1);
    manager.release_handle(&unrelated);
    assert_eq!(manager.get_provider_connections_count(), 0);
    Ok(())
}

#[tokio::test]
async fn test_btree_index_consistent_after_lifecycle() {
    // Verify that the priority_index stays consistent through add, evict, release.
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let addr_a: SocketAddr = "127.0.0.1:49001".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:49002".parse().unwrap();

    // Add connection A (low priority)
    let alloc_a = manager.acquire_connection(&input_name, &addr_a, 10, ConnectionKind::Normal).expect("alloc_a");

    // Check index has 1 entry
    {
        let connections = manager.read_connections();
        let tree = connections.priority_index.get(&input_name).expect("index for provider_1");
        assert_eq!(tree.len(), 1, "index should have 1 entry after first allocation");
    }

    // High-priority user evicts low-priority via grace path
    let alloc_b = manager
        .acquire_connection(&input_name, &addr_b, -5, ConnectionKind::Normal)
        .expect("alloc_b should evict alloc_a");

    // Check index: should have 1 entry (alloc_a evicted, alloc_b added)
    {
        let connections = manager.read_connections();
        let tree = connections.priority_index.get(&input_name).expect("index for provider_1");
        assert_eq!(tree.len(), 1, "index should have 1 entry after eviction + new allocation");
        // The remaining entry should be alloc_b
        let ((prio, _, _), _) = tree.iter().next().expect("one entry");
        assert_eq!(*prio, -5, "remaining entry should be the high-prio connection");
    }

    // Release alloc_b
    manager.release_handle(&alloc_b);

    // Check index: should be empty
    {
        let connections = manager.read_connections();
        let tree = connections.priority_index.get(&input_name);
        let is_empty = tree.is_none_or(std::collections::BTreeMap::is_empty);
        assert!(is_empty, "index should be empty after releasing all connections");
    }

    // Verify alloc_a handle can be safely released (already evicted - no-op)
    manager.release_handle(&alloc_a);
}

#[tokio::test]
async fn test_owner_index_consistent_through_lifecycle() {
    let app_cfg = build_test_app_config(None, 2);
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let addr_a: SocketAddr = "127.0.0.1:49051".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:49052".parse().unwrap();

    let owner_a = "owner-a";
    let owner_b = "owner-b";
    let alloc_a = manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &addr_a,
            false,
            0,
            ConnectionKind::Normal,
            Some(owner_a),
        )
        .expect("alloc_a");
    let alloc_b = manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &addr_b,
            false,
            0,
            ConnectionKind::Normal,
            Some(owner_b),
        )
        .expect("alloc_b");

    {
        let connections = manager.read_connections();
        assert_eq!(connections.by_owner.get(owner_a).map_or(0, HashSet::len), 1, "owner-a has one allocation");
        assert_eq!(connections.by_owner.get(owner_b).map_or(0, HashSet::len), 1, "owner-b has one allocation");
        assert!(connections.by_owner.get(owner_a).unwrap().contains(&alloc_a.allocation_id));
        assert!(connections.by_owner.get(owner_b).unwrap().contains(&alloc_b.allocation_id));
    }

    manager.release_handle(&alloc_a);

    {
        let connections = manager.read_connections();
        assert!(!connections.by_owner.contains_key(owner_a), "owner-a index is removed after release");
        assert_eq!(connections.by_owner.get(owner_b).map_or(0, HashSet::len), 1, "owner-b still indexed");
    }

    manager.release_handle(&alloc_b);
    {
        let connections = manager.read_connections();
        assert!(connections.by_owner.is_empty(), "owner index is empty after all releases");
    }
}

#[tokio::test(start_paused = true)]
async fn live_hls_reentry_keeps_alias_affinity_after_lease_expiry() -> Result<(), String> {
    let (manager, input, alias) = alias_pool(2, 0);
    live_hls_playback_on_alias_with_lapsed_lease(
        &manager,
        &input,
        &alias,
        "client|alice|42|hls|0123456789abcdef",
        PlaybackRequestOutcome::Completed,
    )
    .await?;

    let reentry = acquire_live_hls_from_lineup(&manager, &input, "client|alice|42|hls|fedcba9876543210", 50_011)?;
    assert_eq!(reentry.allocation.get_provider_name(), Some(Arc::clone(&alias)));
    // Affinity reserves nothing: another playback still gets the free primary slot.
    let other = acquire_live_hls_from_lineup(&manager, &input, "client|dave|42|hls|0123456789abcdef", 50_012)?;
    assert_eq!(other.allocation.get_provider_name(), Some(Arc::clone(&input)));
    manager.release_handle(&reentry);
    manager.release_handle(&other);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn provider_failure_after_lease_expiry_ends_alias_affinity() -> Result<(), String> {
    let (manager, input, alias) = alias_pool(2, 0);
    let token = "client|alice|42|hls|0123456789abcdef";
    let handle = confirmed_alias_request(&manager, &input, &alias, token)?;
    let request_id = handle.playback_request_id.ok_or("identified request")?;
    manager.release_handle(&handle);
    tokio::time::advance(Duration::from_secs(20)).await;
    manager.prune_expired_leases_now();
    assert_eq!(manager.provider_lease_usage(&alias).total(), 0, "lease must have expired");

    manager.finish_identified_playback_request(token, request_id, PlaybackRequestOutcome::ProviderFailed);

    let reentry = acquire_live_hls_from_lineup(&manager, &input, "client|alice|42|hls|fedcba9876543210", 50_011)?;
    assert_eq!(reentry.allocation.get_provider_name(), Some(Arc::clone(&input)));
    manager.release_handle(&reentry);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn terminating_token_of_expired_binding_ends_its_affinity_only() -> Result<(), String> {
    let (manager, input, alias) = alias_pool(2, 0);
    let token = "client|alice|42|hls|0123456789abcdef";
    let handle = confirmed_alias_request(&manager, &input, &alias, token)?;
    manager.release_handle(&handle);
    tokio::time::advance(Duration::from_secs(20)).await;
    assert_eq!(manager.provider_lease_usage(&alias).total(), 0, "lease must have expired");

    assert!(!manager.terminate_identified_playback_owner("client|alice|42|hls|00000000000000aa"));
    assert_eq!(manager.provider_affinity_for_owner(token), Some(Arc::clone(&alias)));
    assert!(manager.terminate_identified_playback_owner(token));
    assert_eq!(manager.provider_affinity_for_owner(token), None);
    Ok(())
}

#[tokio::test]
async fn live_hls_socket_cleanup_only_cancels_its_public_session() -> Result<(), String> {
    let app_cfg = create_test_app_config_with_pool(2, 4);
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
    let input = "provider_1".intern();
    let addr = SocketAddr::from(([172, 18, 0, 9], 55_000));
    let old = "client|alice|42|hls|0123456789abcdef";
    let new = "client|alice|42|hls|fedcba9876543210";
    let first = manager
        .acquire_exact_connection_with_lease_for_session(
            &input,
            &addr,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(PlaybackLeaseRef::new(old, PlaybackKind::LiveHls)),
        )
        .ok_or("old allocation")?;
    let next = manager
        .acquire_exact_connection_with_lease_for_session(
            &input,
            &addr,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(PlaybackLeaseRef::new(new, PlaybackKind::LiveHls)),
        )
        .ok_or("new allocation")?;
    assert_eq!(first.binding_tag, next.binding_tag);
    assert_eq!(manager.provider_lease_usage(&input).total(), 1);
    manager.release_playback_connections(old, &[addr]);
    manager.release_playback_connections_await(old, &[addr]).await;
    assert_eq!(manager.get_provider_connections_count(), 1);
    assert!(!next.cancel_token.as_ref().is_some_and(tokio_util::sync::CancellationToken::is_cancelled));
    manager.release_handle(&next);
    Ok(())
}

/// Confirmed reconnect-capable lease, physical handle release first, then the real
/// provider-error outcome: the error must reach the lease and leave no idle reserve.
#[tokio::test(start_paused = true)]
async fn provider_error_cleanup_after_physical_release_does_not_keep_idle_lease() {
    let app_cfg = create_test_app_config_with_pool(2, 3);
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
    let input_name = "provider_1".intern();
    let owner = "r5-error-owner";
    let addr = SocketAddr::from(([172, 18, 0, 9], 55_000));
    let handle = manager
        .acquire_connection_with_lease_for_session(
            &input_name,
            &addr,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(PlaybackLeaseRef::new(owner, PlaybackKind::LiveHls)),
        )
        .expect("acquire a reconnect-capable slot");
    let request_id = handle.playback_request_id.expect("identified request id");
    manager.refresh_adaptive_playback_lease(&input_name, owner, PlaybackKind::LiveHls, 15);
    manager.confirm_playback_activity(owner);

    // Physical release must not conclude the request; the outcome is decided later.
    manager.release_handle(&handle);
    assert_eq!(
        manager.provider_lease_usage(&input_name).active,
        1,
        "physical release must not finish a confirmed lease"
    );

    manager.finish_identified_playback_request(owner, request_id, PlaybackRequestOutcome::ProviderFailed);
    assert_eq!(
        manager.provider_lease_usage(&input_name).total(),
        0,
        "provider error must not keep an idle reconnect lease"
    );
}

/// Counterexample: the same sequence with a clean end keeps the configured idle window.
#[tokio::test(start_paused = true)]
async fn clean_cleanup_after_physical_release_keeps_configured_idle_window() {
    let app_cfg = create_test_app_config_with_pool(2, 3);
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
    let input_name = "provider_1".intern();
    let owner = "r5-clean-owner";
    let addr = SocketAddr::from(([172, 18, 0, 9], 55_001));

    let handle = manager
        .acquire_connection_with_lease_for_session(
            &input_name,
            &addr,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(PlaybackLeaseRef::new(owner, PlaybackKind::LiveHls)),
        )
        .expect("acquire a reconnect-capable slot");
    let request_id = handle.playback_request_id.expect("identified request id");
    manager.refresh_adaptive_playback_lease(&input_name, owner, PlaybackKind::LiveHls, 15);
    manager.confirm_playback_activity(owner);

    manager.release_handle(&handle);
    manager.finish_identified_playback_request(owner, request_id, PlaybackRequestOutcome::Completed);
    let usage = manager.provider_lease_usage(&input_name);
    assert_eq!(usage.active, 0, "clean end must move the lease out of active");
    assert_eq!(usage.idle, 1, "clean end must keep the configured reconnect window");
}

/// The RAII owner releases the allocation synchronously even when dropped outside
/// a tokio runtime, so a body/context drop can never leak a provider slot.
#[test]
fn managed_handle_drop_outside_runtime_releases_allocation() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let (manager, managed_handle) = rt.block_on(async {
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
        let input_name = "provider_1".intern();

        let handle = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &SocketAddr::from(([127, 0, 0, 1], 45_000)),
                false,
                0,
                ConnectionKind::Normal,
                Some("managed-owner"),
            )
            .expect("allocation should succeed");
        assert_eq!(manager.get_provider_connections_count(), 1);

        let managed_handle = super::super::ManagedProviderHandle::new(Arc::clone(&manager), handle);
        (manager, managed_handle)
    });

    drop(managed_handle);
    assert_eq!(manager.get_provider_connections_count(), 0);
}

#[tokio::test]
async fn closing_state_blocks_slot_release_until_completion_token_cancelled() {
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
    let input_name: Arc<str> = "provider_1".intern();

    let handle1 = manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &SocketAddr::from(([127, 0, 0, 1], 46_001)),
            false,
            0,
            ConnectionKind::Normal,
            Some("session-1"),
        )
        .expect("first allocation should succeed");
    assert_eq!(manager.get_provider_connections_count(), 1);

    let completion_token = handle1.completion_token.clone().expect("completion token present");
    let alloc_id = handle1.allocation_id;

    // Mark Closing (simulating supersede / release_playback_connections_await)
    manager.mark_closing(alloc_id);

    // Drop the handle while completion_token is still pending (upstream body not yet closed)
    manager.release_handle(&handle1);

    // The slot must still be occupied by the Closing allocation
    assert_eq!(manager.get_provider_connections_count(), 1);

    // A second start attempt must fail because the provider is at its limit (1 connection)
    let handle2 = manager.acquire_connection_with_grace_for_session(
        &input_name,
        &SocketAddr::from(([127, 0, 0, 1], 46_002)),
        false,
        0,
        ConnectionKind::Normal,
        Some("session-2"),
    );
    assert!(handle2.is_none(), "second start must be rejected while first slot is Closing");

    // Now signal completion (upstream body owner dropped upstream socket)
    completion_token.cancel();

    // Allow background reaper task to execute complete_release
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(manager.get_provider_connections_count(), 0);

    // Second start attempt now succeeds
    let handle3 = manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &SocketAddr::from(([127, 0, 0, 1], 46_003)),
            false,
            0,
            ConnectionKind::Normal,
            Some("session-3"),
        )
        .expect("start must succeed after completion releases the slot");
    assert_eq!(manager.get_provider_connections_count(), 1);

    manager.release_handle(&handle3);
    assert_eq!(manager.get_provider_connections_count(), 0);
}

#[tokio::test]
async fn reaper_cleanup_is_idempotent_and_removes_all_indices() {
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
    let input_name: Arc<str> = "provider_1".intern();

    let handle = manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &SocketAddr::from(([127, 0, 0, 1], 47_001)),
            false,
            0,
            ConnectionKind::Normal,
            Some("session-idempotent"),
        )
        .expect("allocation should succeed");

    let completion_token = handle.completion_token.clone().expect("completion token present");
    let alloc_id = handle.allocation_id;
    manager.mark_closing(alloc_id);

    manager.release_handle(&handle);
    completion_token.cancel();

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(manager.get_provider_connections_count(), 0);

    // Duplicate releases must be completely safe and no-op
    manager.complete_release(alloc_id);
    manager.release_handle(&handle);
    manager.release_connection(&SocketAddr::from(([127, 0, 0, 1], 47_001)));
    assert_eq!(manager.get_provider_connections_count(), 0);
}

#[tokio::test]
async fn capacity_release_wakes_only_waiters_of_that_provider() {
    use futures::FutureExt;
    let app_cfg = create_test_app_config_with_dual_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
    let provider_1: Arc<str> = "provider_1".intern();
    let provider_2: Arc<str> = "provider_2".intern();
    let acquire = |provider: &Arc<str>, port: u16| {
        manager
            .acquire_exact_connection_with_lease_for_session(
                provider,
                &SocketAddr::from(([127, 0, 0, 1], port)),
                false,
                0,
                ConnectionKind::Normal,
                None,
            )
            .expect("slot available")
    };
    let handle_1 = acquire(&provider_1, 48_201);
    let handle_2 = acquire(&provider_2, 48_202);

    let notify = manager.provider_capacity_notify(&provider_1);
    let woken = notify.notified();
    tokio::pin!(woken);
    woken.as_mut().enable();
    manager.release_handle(&handle_2);
    assert!(woken.as_mut().now_or_never().is_none(), "another provider's release must not wake the waiter");
    manager.release_handle(&handle_1);
    assert!(woken.as_mut().now_or_never().is_some(), "the waiter's provider release wakes it");
}

#[tokio::test(start_paused = true)]
async fn dropped_preempting_request_still_frees_the_victim_slot() {
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
    let input_name: Arc<str> = "provider_1".intern();
    let victim = manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &SocketAddr::from(([127, 0, 0, 1], 48_111)),
            false,
            10,
            ConnectionKind::Normal,
            Some("session-victim"),
        )
        .expect("victim allocation succeeds");
    manager.mark_opening(victim.allocation_id);
    assert!(manager.register_body_owner(victim.allocation_id));

    // The preempting request waits without deadline and is dropped (client disconnect).
    let waiting = {
        let manager = Arc::clone(&manager);
        let input_name = Arc::clone(&input_name);
        tokio::spawn(async move {
            manager
                .acquire_exact_connection_with_lease_for_session_await(
                    &input_name,
                    &SocketAddr::from(([127, 0, 0, 1], 48_112)),
                    false,
                    0,
                    ConnectionKind::Normal,
                    None,
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(victim.cancel_token.as_ref().is_some_and(tokio_util::sync::CancellationToken::is_cancelled));
    waiting.abort();
    assert!(waiting.await.is_err_and(|err| err.is_cancelled()));
    assert_eq!(manager.get_provider_connections_count(), 1, "the draining victim keeps its slot for now");

    tokio::time::sleep(super::super::PREEMPTION_COMPLETION_TIMEOUT).await;
    tokio::task::yield_now().await;
    assert_eq!(manager.get_provider_connections_count(), 0, "the reaper frees the never-completing victim");
}

#[tokio::test(start_paused = true)]
async fn exact_acquire_until_stops_at_deadline_and_still_forces_victim_release() {
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
    let input_name: Arc<str> = "provider_1".intern();
    let victim = manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &SocketAddr::from(([127, 0, 0, 1], 48_101)),
            false,
            10,
            ConnectionKind::Normal,
            Some("session-victim"),
        )
        .expect("victim allocation succeeds");
    manager.mark_opening(victim.allocation_id);
    assert!(manager.register_body_owner(victim.allocation_id));

    // The victim never completes its body, so only the deadline can end the wait.
    let started = tokio::time::Instant::now();
    let acquired = manager
        .acquire_exact_connection_with_lease_for_session_until(
            &input_name,
            &SocketAddr::from(([127, 0, 0, 1], 48_102)),
            false,
            0,
            ConnectionKind::Normal,
            None,
            Some(started + Duration::from_millis(100)),
        )
        .await;
    assert!(acquired.is_none());
    assert_eq!(started.elapsed(), Duration::from_millis(100), "the caller deadline bounds the wait");
    assert!(victim.cancel_token.as_ref().is_some_and(tokio_util::sync::CancellationToken::is_cancelled));
    assert_eq!(manager.get_provider_connections_count(), 1, "the draining victim keeps its slot for now");

    tokio::time::sleep(super::super::PREEMPTION_COMPLETION_TIMEOUT).await;
    tokio::task::yield_now().await;
    assert_eq!(manager.get_provider_connections_count(), 0, "the forced victim release still runs");
}

#[tokio::test]
async fn dropped_cleanup_future_does_not_permanently_leak_slot() {
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
    let input_name: Arc<str> = "provider_1".intern();
    let addr = SocketAddr::from(([127, 0, 0, 1], 49_002));
    let addrs = [addr];

    let handle = manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &addr,
            false,
            0,
            ConnectionKind::Normal,
            Some("session-drop"),
        )
        .expect("allocation succeeds");

    manager.mark_opening(handle.allocation_id);
    assert!(manager.register_body_owner(handle.allocation_id));
    let completion_token = handle.completion_token.clone().expect("token present");

    // Start release_playback_connections_await but drop it before completion
    {
        let cleanup_fut = manager.release_playback_connections_await("session-drop", &addrs);
        // Poll once and drop
        tokio::select! {
            biased;
            () = async {} => {},
            () = cleanup_fut => {},
        }
    }

    // Slot must still be occupied
    assert_eq!(manager.get_provider_connections_count(), 1);

    // Now the handle is released and the body owner signals completion
    manager.release_handle(&handle);
    completion_token.cancel();

    // Give the background reaper task time to run
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        manager.get_provider_connections_count(),
        0,
        "slot must be freed after completion and not permanently leaked in Closing state"
    );
}
