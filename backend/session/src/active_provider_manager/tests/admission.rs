use super::{
    alias_pool, build_test_app_config, create_test_app_config_single_provider_pool,
    create_test_app_config_single_unlimited_provider_pool, create_test_app_config_with_dual_provider_pool,
    create_test_app_config_with_pool, live_hls_playback_on_alias_with_lapsed_lease, ActiveProviderManager,
    ConnectionKind, PlaybackLeaseRef,
};
use crate::EventManager;
use shared::{defaults::default_user_priority, utils::Internable};
use std::{collections::HashSet, net::SocketAddr, sync::Arc, time::Duration};
use tuliprox_core::model::{
    AppConfig, ConfigInputAlias, PlaybackKind, PlaybackRequestOutcome, ProviderAllocation, SharedSubscriberId,
};

pub(in crate::active_provider_manager::tests) fn create_test_app_config_with_capacity_ordered_pool() -> AppConfig {
    build_test_app_config(
        Some(vec![ConfigInputAlias {
            id: 2,
            name: "provider_2".intern(),
            url: "http://provider-2.example".to_string(),
            username: Some("user2".to_string()),
            password: Some("pass2".to_string()),
            priority: 1,
            max_connections: 1,
            exp_date: None,
            enabled: true,
            stalker: None,
        }]),
        3,
    )
}

#[tokio::test]
async fn confirmed_playbacks_fill_higher_priority_provider_before_alias() {
    let app_cfg = create_test_app_config_with_pool(2, 3);
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
    let input_name = "provider_1".intern();

    let mut selected = Vec::new();
    for index in 0..5 {
        let owner = format!("playback-{index}");
        let addr = SocketAddr::from(([172, 18, 0, 9], 50_000 + index));
        let handle = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &addr,
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some(&owner),
            )
            .expect("pool has capacity for five confirmed playbacks");
        let provider = handle.allocation.get_provider_name().expect("provider name");
        // Only real media activity confirms a playback and lets it reserve capacity.
        manager.confirm_playback_activity(&owner);
        selected.push(provider.to_string());
    }

    assert_eq!(selected, ["provider_1", "provider_1", "provider_2", "provider_2", "provider_2"]);
}

#[tokio::test]
async fn concurrent_proxy_requests_fill_exact_pool_capacity() -> Result<(), Box<dyn std::error::Error>> {
    let manager =
        Arc::new(ActiveProviderManager::new(&create_test_app_config_with_pool(2, 3), &Arc::new(EventManager::new())));
    let addr = SocketAddr::from(([127, 0, 0, 1], 50001));
    let mut tasks = tokio::task::JoinSet::new();
    for attempt in 0..32 {
        let manager = Arc::clone(&manager);
        tasks.spawn(async move {
            manager.acquire_connection_with_grace_for_session(
                &Arc::from("provider_1"),
                &addr,
                false,
                0,
                ConnectionKind::Normal,
                Some(&format!("playback-{attempt}")),
            )
        });
    }
    let mut handles = Vec::new();
    while let Some(result) = tasks.join_next().await {
        if let Some(handle) = result? {
            handles.push(handle);
        }
    }
    assert_eq!(handles.len(), 5);
    assert_eq!(handles.iter().filter(|h| h.allocation.get_provider_name().as_deref() == Some("provider_1")).count(), 2);
    assert_eq!(handles.iter().filter(|h| h.allocation.get_provider_name().as_deref() == Some("provider_2")).count(), 3);
    for handle in handles {
        manager.release_handle(&handle);
    }
    assert_eq!(manager.get_provider_connections_count(), 0);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn manifest_retries_without_media_do_not_reserve_capacity() {
    let app_cfg = create_test_app_config_with_pool(2, 3);
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
    let input_name = "provider_1".intern();
    let owner = "hls-cache:shared-session";
    let addr = SocketAddr::from(([172, 18, 0, 9], 51_000));

    // Five manifest responses without a single media segment, all on one owner.
    for _ in 0..5 {
        let handle = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &addr,
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some(owner),
            )
            .expect("manifest start allocates on the preferred provider");
        assert_eq!(handle.allocation.get_provider_name().as_deref(), Some("provider_1"));
        manager.refresh_provider_reservation(&input_name, owner, 15);
        manager.release_connection(&addr);
    }

    // Unconfirmed leases never reserve capacity, so an unrelated client is served
    // by the higher-priority provider even though its own counters read zero.
    let other = manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &SocketAddr::from(([172, 18, 0, 9], 51_001)),
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some("other-client"),
        )
        .expect("unrelated client must still get the preferred provider");
    assert_eq!(other.allocation.get_provider_name().as_deref(), Some("provider_1"));

    // The abandoned starts stop holding anything once their startup deadline passes.
    tokio::time::advance(Duration::from_secs(6)).await;
    manager.prune_expired_leases_now();
    assert!(!manager.is_provider_reserved_for_other_session(&input_name, Some("other-client")));
}

#[tokio::test(start_paused = true)]
async fn identified_reservation_recreates_a_cleared_lease() {
    let app_cfg = create_test_app_config_with_pool(2, 3);
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
    let input_name = "provider_1".intern();
    let owner = "hls-cache:restore-session";
    let addr = SocketAddr::from(([172, 18, 0, 9], 53_000));

    let handle = manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &addr,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(owner),
        )
        .expect("allocate");
    manager.refresh_identified_provider_reservation(
        &input_name,
        owner,
        tuliprox_core::model::PlaybackKind::Catchup,
        15,
    );
    assert_eq!(manager.provider_lease_usage(&input_name).starting, 1);

    // Clearing removes the lease entirely; recreating must bring it back rather
    // than silently no-op like a plain adaptive refresh would.
    let binding_tag = manager.binding_tag_for_owner(owner);
    manager.clear_identified_provider_reservation(owner, &input_name, binding_tag);
    assert_eq!(manager.provider_lease_usage(&input_name).total(), 0);

    manager.refresh_identified_provider_reservation(
        &input_name,
        owner,
        tuliprox_core::model::PlaybackKind::Catchup,
        15,
    );
    assert_eq!(manager.provider_lease_usage(&input_name).starting, 1);

    manager.release_handle(&handle);
}

#[tokio::test(start_paused = true)]
async fn reservations_do_not_survive_after_counters_reach_zero() {
    let app_cfg = create_test_app_config_with_pool(2, 3);
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
    let input_name = "provider_1".intern();
    let owner = "finished-playback";
    let addr = SocketAddr::from(([172, 18, 0, 9], 52_000));

    let handle = manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &addr,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(owner),
        )
        .expect("first playback acquires the preferred provider");
    assert_eq!(handle.allocation.get_provider_name().as_deref(), Some("provider_1"));
    manager.refresh_provider_reservation(&input_name, owner, 15);
    manager.confirm_playback_activity(owner);
    manager.release_connection(&addr);

    // Live TS is not reconnect capable: the confirmed lease is dropped on release,
    // so the provider is immediately free again for the next higher-priority client.
    manager.finish_playback_request(owner, PlaybackRequestOutcome::Completed, None);
    let next = manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &SocketAddr::from(([172, 18, 0, 9], 52_001)),
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some("next-client"),
        )
        .expect("freed capacity must be selectable again");
    assert_eq!(next.allocation.get_provider_name().as_deref(), Some("provider_1"));
}

#[tokio::test]
async fn unlimited_provider_connection_is_not_in_priority_index() {
    let app_cfg = create_test_app_config_single_unlimited_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let addr_1: SocketAddr = "127.0.0.1:44001".parse().unwrap();
    let addr_2: SocketAddr = "127.0.0.1:44002".parse().unwrap();
    let addr_3: SocketAddr = "127.0.0.1:44003".parse().unwrap();

    manager
        .acquire_connection(&input_name, &addr_1, default_user_priority(), ConnectionKind::Normal)
        .expect("acquire #1 on unlimited provider");
    manager
        .acquire_connection(&input_name, &addr_2, default_user_priority(), ConnectionKind::Normal)
        .expect("acquire #2 on unlimited provider");
    manager
        .acquire_connection(&input_name, &addr_3, default_user_priority(), ConnectionKind::Normal)
        .expect("acquire #3 on unlimited provider");

    {
        let connections = manager.read_connections();
        let priority_tree = connections.priority_index.get(&input_name);
        assert!(
            priority_tree.is_none_or(std::collections::BTreeMap::is_empty),
            "unlimited provider must not be present in priority_index, found {priority_tree:?}"
        );
        let soft_tree = connections.soft_priority_index.get(&input_name);
        assert!(
            soft_tree.is_none_or(std::collections::BTreeMap::is_empty),
            "unlimited provider must not be present in soft_priority_index, found {soft_tree:?}"
        );
        let by_provider = connections.by_provider.get(&input_name);
        assert!(
            by_provider.is_none_or(std::collections::HashSet::is_empty),
            "unlimited provider must not be present in by_provider, found {by_provider:?}"
        );
    }

    manager.release_connection(&addr_1);
    manager.release_connection(&addr_2);
    manager.release_connection(&addr_3);
}

#[tokio::test]
async fn preemption_does_not_select_unlimited_provider_as_victim() {
    let app_cfg = create_test_app_config_single_unlimited_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let low_addr: SocketAddr = "127.0.0.1:45001".parse().unwrap();
    let high_addr: SocketAddr = "127.0.0.1:45002".parse().unwrap();

    // Acquire one low-priority and one high-priority connection on the unlimited provider.
    let low = manager
        .acquire_connection(&input_name, &low_addr, 50, ConnectionKind::Normal)
        .expect("low-priority acquire on unlimited provider");
    let low_token = low.cancel_token.clone().expect("cancel token for low-priority connection");
    let _high = manager
        .acquire_connection(&input_name, &high_addr, 0, ConnectionKind::Normal)
        .expect("high-priority acquire on unlimited provider");

    // A request at an even higher priority must not be able to preempt the unlimited
    // provider's connection, because the connection is intentionally absent from the
    // preemption indices.
    let candidate = {
        let connections = manager.read_connections();
        manager.select_preemption_candidate(&connections, &input_name, 0, ConnectionKind::Normal, &HashSet::new())
    };
    assert!(
        candidate.is_none(),
        "select_preemption_candidate must not return an unlimited-provider connection as a victim, got {candidate:?}"
    );

    // The low-priority connection is still alive and its cancel token has not been fired.
    assert!(
        !low_token.is_cancelled(),
        "low-priority unlimited-provider connection must not be cancelled by preemption"
    );

    manager.release_connection(&low_addr);
    manager.release_connection(&high_addr);
}

#[tokio::test(start_paused = true)]
async fn untagged_clear_cannot_delete_successor_reservation() {
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
    let input_name = "provider_1".intern();
    let owner = "session-owner";
    let addr: SocketAddr = "127.0.0.1:43320".parse().unwrap();

    // First incarnation acquires, then is removed entirely.
    let first = manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &addr,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(owner),
        )
        .expect("first acquire");
    manager.release_handle(&first);
    manager.clear_provider_reservation(owner);

    // Second incarnation acquires a fresh lease on the same account.
    let second = manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &addr,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(owner),
        )
        .expect("second acquire");

    // An untagged clear has no delete right and must not remove the successor.
    manager.clear_identified_provider_reservation(owner, &input_name, None);
    assert!(
        manager.binding_tag_for_owner(owner).is_some(),
        "an untagged clear must not delete the successor reservation"
    );

    manager.release_handle(&second);
    manager.clear_provider_reservation(owner);
}

#[tokio::test(start_paused = true)]
async fn session_reservations_preserve_capacity_and_priority_order() {
    let app_cfg = create_test_app_config_with_capacity_ordered_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
    let input_name = "provider_1".intern();

    for index in 0..3 {
        let owner = format!("session-owner-{index}");
        let addr = SocketAddr::from(([127, 0, 0, 1], 43200 + index));
        let allocation = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &addr,
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some(&owner),
            )
            .expect("preferred provider should retain free capacity for another session");
        assert_eq!(allocation.allocation.get_provider_name().as_deref(), Some("provider_1"));
        manager.refresh_provider_reservation(&input_name, &owner, 15);
        manager.confirm_playback_activity(&owner);
    }

    let fallback_addr = SocketAddr::from(([127, 0, 0, 1], 43203));
    let fallback = manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &fallback_addr,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some("session-owner-3"),
        )
        .expect("lower-priority provider should be used after preferred capacity is exhausted");
    assert_eq!(fallback.allocation.get_provider_name().as_deref(), Some("provider_2"));

    manager.release_connection(&SocketAddr::from(([127, 0, 0, 1], 43200)));
    manager.release_connection(&fallback_addr);

    let foreign_after_release = manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &fallback_addr,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some("session-owner-4"),
        )
        .expect("idle reservation should protect one preferred-provider slot");
    assert_eq!(foreign_after_release.allocation.get_provider_name().as_deref(), Some("provider_2"));

    let reserved_owner_addr = SocketAddr::from(([127, 0, 0, 1], 43204));
    let reserved_owner = manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &reserved_owner_addr,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some("session-owner-0"),
        )
        .expect("reservation owner should reclaim its preferred-provider slot");
    assert_eq!(reserved_owner.allocation.get_provider_name().as_deref(), Some("provider_1"));

    for index in 1..5 {
        let addr = SocketAddr::from(([127, 0, 0, 1], 43200 + index));
        manager.release_connection(&addr);
    }
}

#[tokio::test(start_paused = true)]
async fn test_unlimited_provider_reservation_does_not_block_other_sessions() {
    let app_cfg = create_test_app_config_single_unlimited_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let owner_1 = "session-owner-1";
    let owner_2 = "session-owner-2";
    let addr_1: SocketAddr = "127.0.0.1:43121".parse().unwrap();
    let addr_2: SocketAddr = "127.0.0.1:43122".parse().unwrap();

    manager.refresh_provider_reservation(&input_name, owner_1, 15);

    let first = manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &addr_1,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(owner_1),
        )
        .expect("reserved owner should reacquire its unlimited provider");
    assert_eq!(first.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

    let second = manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &addr_2,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(owner_2),
        )
        .expect("other sessions should not be blocked by reservations on unlimited providers");
    assert_eq!(second.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

    manager.release_connection(&addr_1);
    manager.release_connection(&addr_2);
}

#[tokio::test(start_paused = true)]
async fn concurrent_same_family_playbacks_keep_independent_provider_reservations() {
    let app_cfg = create_test_app_config_with_dual_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let provider_2 = "provider_2".intern();
    let Ok(first_addr) = "127.0.0.1:43121".parse() else {
        return;
    };
    let Ok(second_addr) = "127.0.0.1:43122".parse() else {
        return;
    };
    let owner_channel_1 = "proxy|player|user|100";
    let owner_channel_2 = "proxy|player|user|200";

    let first = manager.acquire_connection_with_grace_for_session(
        &input_name,
        &first_addr,
        false,
        default_user_priority(),
        ConnectionKind::Normal,
        Some(owner_channel_1),
    );
    assert!(first.is_some(), "first playback should acquire the preferred provider");
    let Some(first) = first else {
        return;
    };
    assert_eq!(first.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));
    manager.refresh_provider_reservation(&input_name, owner_channel_1, 15);
    manager.confirm_playback_activity(owner_channel_1);
    manager.release_connection(&first_addr);

    let second = manager.acquire_connection_with_grace_for_session(
        &input_name,
        &second_addr,
        false,
        default_user_priority(),
        ConnectionKind::Normal,
        Some(owner_channel_2),
    );
    assert!(second.is_some(), "parallel playback should acquire the remaining provider");
    let Some(second) = second else {
        return;
    };
    assert_eq!(second.allocation.get_provider_name().as_deref(), Some(provider_2.as_ref()));
    manager.refresh_provider_reservation(&provider_2, owner_channel_2, 15);
    manager.confirm_playback_activity(owner_channel_2);

    let leases = manager.read_leases();
    assert_eq!(leases.provider_for_owner(owner_channel_1).as_deref(), Some(input_name.as_ref()));
    assert_eq!(leases.provider_for_owner(owner_channel_2).as_deref(), Some(provider_2.as_ref()));
    drop(leases);

    manager.clear_provider_reservation(owner_channel_2);
    let leases = manager.read_leases();
    assert_eq!(leases.provider_for_owner(owner_channel_1).as_deref(), Some(input_name.as_ref()));
    assert!(leases.lease_of_owner(owner_channel_2).is_none());
    drop(leases);

    manager.release_connection(&second_addr);
}

#[tokio::test]
async fn test_higher_priority_user_preempts_lower_priority_user() {
    // User with priority 5 (low) is connected; user with priority -1 (high) arrives.
    // The low-priority user should be preempted.
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let low_prio_addr: SocketAddr = "127.0.0.1:44001".parse().unwrap();
    let high_prio_addr: SocketAddr = "127.0.0.1:44002".parse().unwrap();

    // Low-priority user connects (priority 5 = lower importance)
    let low_alloc = manager
        .acquire_connection(&input_name, &low_prio_addr, 5, ConnectionKind::Normal)
        .expect("low-priority user should get connection");
    assert_eq!(low_alloc.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

    // Provider is now exhausted
    assert!(manager.is_exhausted(&input_name));

    // High-priority user arrives (priority -1 = higher importance), should preempt low-priority user
    let high_alloc = manager
        .acquire_connection_with_grace(&input_name, &high_prio_addr, false, -1, ConnectionKind::Normal)
        .expect("high-priority user should preempt low-priority user and get connection");
    assert_eq!(high_alloc.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

    manager.release_connection(&high_prio_addr);
}

#[tokio::test]
async fn test_same_priority_user_does_not_preempt() {
    // Two users with the same priority — new one should NOT preempt the existing one.
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let user_1_addr: SocketAddr = "127.0.0.1:45001".parse().unwrap();
    let user_2_addr: SocketAddr = "127.0.0.1:45002".parse().unwrap();

    // User 1 connects with priority 0
    let alloc1 = manager
        .acquire_connection(&input_name, &user_1_addr, default_user_priority(), ConnectionKind::Normal)
        .expect("user1 should get connection");
    assert_eq!(alloc1.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

    // Provider is now exhausted
    assert!(manager.is_exhausted(&input_name));

    // User 2 arrives with the same priority 0 — should NOT preempt user 1
    let alloc2 = manager.acquire_connection_with_grace(
        &input_name,
        &user_2_addr,
        false,
        default_user_priority(),
        ConnectionKind::Normal,
    );
    assert!(alloc2.is_none(), "same-priority user should not preempt existing user");

    manager.release_connection(&user_1_addr);
}

#[tokio::test]
async fn test_lower_priority_user_does_not_preempt_higher_priority_user() {
    // User with high priority is connected; user with low priority arrives — should NOT preempt.
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let high_prio_addr: SocketAddr = "127.0.0.1:46001".parse().unwrap();
    let low_prio_addr: SocketAddr = "127.0.0.1:46002".parse().unwrap();

    // High-priority user connects (priority -10)
    let alloc1 = manager
        .acquire_connection(&input_name, &high_prio_addr, -10, ConnectionKind::Normal)
        .expect("high-priority user should get connection");
    assert_eq!(alloc1.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

    // Provider is now exhausted
    assert!(manager.is_exhausted(&input_name));

    // Low-priority user arrives (priority 10) — should NOT preempt high-priority user
    let alloc2 = manager.acquire_connection_with_grace(&input_name, &low_prio_addr, false, 10, ConnectionKind::Normal);
    assert!(alloc2.is_none(), "low-priority user should not preempt high-priority user");

    manager.release_connection(&high_prio_addr);
}

#[tokio::test]
async fn test_grace_period_triggers_preemption_of_lower_priority() {
    // Provider full, high-prio user arrives with grace allowed,
    // low-prio victim should be evicted and provider should not be over limit.
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let low_prio_addr: SocketAddr = "127.0.0.1:47001".parse().unwrap();
    let high_prio_addr: SocketAddr = "127.0.0.1:47002".parse().unwrap();

    // Low-priority user connects (priority 20 = low importance)
    let low_alloc = manager
        .acquire_connection(&input_name, &low_prio_addr, 20, ConnectionKind::Normal)
        .expect("low-priority user should get connection");
    assert_eq!(low_alloc.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));
    let low_token = low_alloc.cancel_token.clone().expect("must have cancel token");

    // Provider is now exhausted
    assert!(manager.is_exhausted(&input_name));

    // High-priority user arrives WITH grace allowed (default streaming path)
    // This should get a GracePeriod allocation and then evict the low-prio user
    let high_alloc = manager
        .acquire_connection(&input_name, &high_prio_addr, 0, ConnectionKind::Normal)
        .expect("high-priority user should get grace allocation and evict low-prio");
    assert_eq!(high_alloc.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

    // Low-priority user's cancel token should be cancelled
    assert!(low_token.is_cancelled(), "low-prio user should be cancelled after eviction");

    // Provider should not be over limit (eviction freed a slot)
    assert!(!manager.is_over_limit(&input_name), "provider should not be over limit after eviction");

    manager.release_connection(&high_prio_addr);
}

#[tokio::test]
async fn test_equal_priority_user_gets_grace_without_preempting_existing_stream() {
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let user_1_addr: SocketAddr = "127.0.0.1:48001".parse().unwrap();
    let user_2_addr: SocketAddr = "127.0.0.1:48002".parse().unwrap();

    // User 1 connects with priority 0
    let alloc1 = manager
        .acquire_connection(&input_name, &user_1_addr, default_user_priority(), ConnectionKind::Normal)
        .expect("user1 should get connection");
    assert_eq!(alloc1.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));
    let token1 = alloc1.cancel_token.clone().expect("must have cancel token");

    // Provider is now exhausted
    assert!(manager.is_exhausted(&input_name));

    // User 2 arrives with the same priority and should be granted grace instead of being rejected.
    let alloc2 = manager
        .acquire_connection(&input_name, &user_2_addr, default_user_priority(), ConnectionKind::Normal)
        .expect("same-priority user should get grace allocation");
    assert!(matches!(alloc2.allocation, ProviderAllocation::GracePeriod(_)));
    assert_eq!(alloc2.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

    // User 1 should NOT be cancelled, and the provider should be temporarily over limit.
    assert!(!token1.is_cancelled(), "same-prio user should not be evicted");
    assert!(manager.is_over_limit(&input_name), "provider should be temporarily over limit during grace");

    manager.release_connection(&user_1_addr);
    manager.release_connection(&user_2_addr);
}

#[tokio::test]
async fn test_higher_priority_user_preempts_first_inserted_low_priority_victim_on_exact_tie() {
    let app_cfg = create_test_app_config_with_dual_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let old_low_addr: SocketAddr = "127.0.0.1:48101".parse().unwrap();
    let new_low_addr: SocketAddr = "127.0.0.1:48102".parse().unwrap();
    let high_prio_addr: SocketAddr = "127.0.0.1:48103".parse().unwrap();

    // Place two equal-priority low-priority victims on different provider aliases.
    // The test then normalizes their created_at timestamps to an exact tie so the
    // selector must fall back to the final stable tie-break instead of clock order.
    let old_alloc = manager
        .acquire_exact_connection_with_grace(&"provider_2".intern(), &old_low_addr, false, 20, ConnectionKind::Normal)
        .expect("old low-priority allocation should succeed");
    let old_token = old_alloc.cancel_token.clone().expect("old allocation should have cancel token");
    assert_eq!(old_alloc.allocation.get_provider_name().as_deref(), Some("provider_2"));

    let new_alloc = manager
        .acquire_exact_connection_with_grace(&"provider_1".intern(), &new_low_addr, false, 20, ConnectionKind::Normal)
        .expect("new low-priority allocation should succeed");
    let new_token = new_alloc.cancel_token.clone().expect("new allocation should have cancel token");
    assert_eq!(new_alloc.allocation.get_provider_name().as_deref(), Some("provider_1"));

    {
        let mut connections = manager.write_connections();
        let old_created_at = connections
            .single
            .get(&old_alloc.allocation_id)
            .map(|info| info.created_at)
            .expect("old allocation should still be registered");

        let (new_created_at, new_priority) = {
            let info = connections
                .single
                .get_mut(&new_alloc.allocation_id)
                .expect("new allocation should still be registered");
            let original_created_at = info.created_at;
            info.created_at = old_created_at;
            (original_created_at, info.priority)
        };

        let provider_name = "provider_1".intern();
        let tree =
            connections.priority_index.get_mut(&provider_name).expect("priority index for provider_1 should exist");
        let owner = tree
            .remove(&(new_priority, std::cmp::Reverse(new_created_at), new_alloc.allocation_id))
            .expect("new allocation should still be indexed");
        tree.insert((new_priority, std::cmp::Reverse(old_created_at), new_alloc.allocation_id), owner);
    }

    assert!(manager.is_exhausted(&input_name));

    // Higher-priority request should now select the first-inserted victim because
    // priority and created_at are exactly tied across provider aliases.
    let high_alloc = manager
        .acquire_connection(&input_name, &high_prio_addr, 0, ConnectionKind::Normal)
        .expect("higher-priority request should preempt the first-inserted low-priority victim on exact tie");
    assert_eq!(high_alloc.allocation.get_provider_name().as_deref(), Some("provider_2"));

    assert!(old_token.is_cancelled(), "first-inserted low-priority victim should be canceled first on exact tie");
    assert!(!new_token.is_cancelled(), "later allocation should remain active after the stable tie-break");

    manager.release_connection(&high_prio_addr);
    manager.release_connection(&old_low_addr);
    manager.release_connection(&new_low_addr);
}

#[tokio::test]
async fn test_shared_priority_downgrades_after_high_priority_user_leaves() {
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let stream_key = "http://example.com/shared/live";
    let addr_a: SocketAddr = "127.0.0.1:48501".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:48502".parse().unwrap();

    // A starts shared stream with high importance (priority 0).
    let alloc_a = manager
        .acquire_connection(&input_name, &addr_a, 0, ConnectionKind::Normal)
        .expect("A should get initial connection");
    let shared_token = alloc_a.cancel_token.clone().expect("shared allocation should have cancel token");
    assert!(manager.make_shared_connection(&alloc_a, stream_key, SharedSubscriberId::from_stream_uid(1)));

    // B joins the same shared stream with lower importance (priority 1).
    let join_result = manager.add_shared_connection(
        &addr_b,
        SharedSubscriberId::from_stream_uid(2),
        stream_key,
        1,
        ConnectionKind::Normal,
    );
    assert!(join_result.is_ok(), "B should join existing shared stream, got: {join_result:?}");

    // A leaves shared stream. Shared allocation should now inherit B's lower priority.
    manager.release_connection(&addr_a);
    {
        let connections = manager.read_connections();
        let shared = connections.shared.by_key.get(stream_key).expect("shared entry should remain for B");
        assert_eq!(shared.priority, 1, "shared priority must downgrade to remaining subscriber priority");
    }

    // A starts another stream with higher importance and should preempt B's shared stream.
    let alloc_a2 = manager
        .acquire_connection(&input_name, &addr_a, 0, ConnectionKind::Normal)
        .expect("A should preempt lower-priority shared stream");
    assert_eq!(alloc_a2.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));
    assert!(shared_token.is_cancelled(), "shared stream should be cancelled when preempted");
    assert!(!manager.is_over_limit(&input_name), "provider should not remain over limit after preemption");

    manager.release_connection(&addr_a);
    manager.release_connection(&addr_b);
}

#[tokio::test]
async fn test_higher_priority_user_preempts_shared_stream_with_multiple_subscribers() {
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let stream_key = "http://example.com/shared/live/multi";
    let addr_a: SocketAddr = "127.0.0.1:48601".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:48602".parse().unwrap();
    let addr_high: SocketAddr = "127.0.0.1:48603".parse().unwrap();

    let shared_alloc = manager
        .acquire_connection(&input_name, &addr_a, 5, ConnectionKind::Normal)
        .expect("low-priority shared stream should get initial connection");
    let shared_token = shared_alloc.cancel_token.clone().expect("shared allocation should have cancel token");
    assert!(manager.make_shared_connection(&shared_alloc, stream_key, SharedSubscriberId::from_stream_uid(1)));

    manager
        .add_shared_connection(&addr_b, SharedSubscriberId::from_stream_uid(2), stream_key, 6, ConnectionKind::Normal)
        .expect("second subscriber should join shared stream");

    let high_alloc = manager
        .acquire_connection(&input_name, &addr_high, 0, ConnectionKind::Normal)
        .expect("higher-priority user should preempt lower-priority shared stream");
    assert_eq!(high_alloc.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));
    assert!(shared_token.is_cancelled(), "shared stream should be cancelled when preempted");

    {
        let connections = manager.read_connections();
        assert!(
            !connections.shared.by_key.contains_key(stream_key),
            "preempted shared stream must be removed even with multiple subscribers"
        );
    }

    manager.release_connection(&addr_high);
    manager.release_connection(&addr_a);
    manager.release_connection(&addr_b);
}

#[tokio::test]
async fn test_preemption_respects_registered_body_owner_completion() {
    let app_cfg = build_test_app_config(None, 1);
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let addr_1: SocketAddr = "127.0.0.1:49061".parse().unwrap();
    let addr_2: SocketAddr = "127.0.0.1:49062".parse().unwrap();

    // 1. First request with prio 5 occupies limit=1
    let handle_1 = manager
        .acquire_connection(&input_name, &addr_1, 5, ConnectionKind::Normal)
        .expect("first allocation should succeed");

    // 2. Register body owner on handle_1
    manager.register_body_owner(handle_1.allocation_id);

    // 3. Higher-priority request with prio -1 arrives without grace while handle_1 is active
    let handle_2 = manager.acquire_connection_with_grace(&input_name, &addr_2, false, -1, ConnectionKind::Normal);

    // 4. Replacement must NOT be admitted before completion
    assert!(handle_2.is_none(), "replacement must not be admitted before victim completion");
    assert!(
        handle_1.cancel_token.as_ref().expect("cancel token").is_cancelled(),
        "victim should receive cancel signal"
    );
    assert_eq!(
        handle_1.close_reason.load(std::sync::atomic::Ordering::Acquire),
        tuliprox_core::model::ProviderCloseReason::PriorityPreempted as u8,
        "victim close reason must be PriorityPreempted"
    );

    // 5. Victim completes upstream reading
    handle_1.completion_token.as_ref().expect("completion token").cancel();

    // Allow background complete_release task to run
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    // 6. Now replacement request can be admitted
    let handle_2 = manager
        .acquire_connection(&input_name, &addr_2, -1, ConnectionKind::Normal)
        .expect("replacement should succeed after victim has completed release");

    manager.release_handle(&handle_2);
}

#[tokio::test]
async fn test_preemption_respects_opening_connection_completion() {
    let app_cfg = build_test_app_config(None, 1);
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let addr_1: SocketAddr = "127.0.0.1:49063".parse().unwrap();
    let addr_2: SocketAddr = "127.0.0.1:49064".parse().unwrap();

    // 1. First request with prio 5 occupies limit=1
    let handle_1 = manager
        .acquire_connection(&input_name, &addr_1, 5, ConnectionKind::Normal)
        .expect("first allocation should succeed");
    manager.mark_opening(handle_1.allocation_id);

    // Verify lifecycle is Opening and body owner is not yet registered
    {
        let connections = manager.read_connections();
        let info = connections.single.get(&handle_1.allocation_id).expect("handle_1 exists");
        assert_eq!(info.lifecycle, tuliprox_core::model::ConnectionLifecycle::Opening);
        assert!(!info.has_body_owner);
    }

    // 2. Higher-priority request with prio -1 arrives without grace while handle_1 is in Opening
    let handle_2 = manager.acquire_connection_with_grace(&input_name, &addr_2, false, -1, ConnectionKind::Normal);

    // 3. Replacement must NOT be admitted before completion of opening
    assert!(handle_2.is_none(), "replacement must not be admitted while victim is still opening");
    assert!(
        handle_1.cancel_token.as_ref().expect("cancel token").is_cancelled(),
        "victim should receive cancel signal during open"
    );
    assert_eq!(
        handle_1.close_reason.load(std::sync::atomic::Ordering::Acquire),
        tuliprox_core::model::ProviderCloseReason::PriorityPreempted as u8,
        "victim close reason must be PriorityPreempted"
    );

    // 4. Victim's open future completes and signals completion
    handle_1.completion_token.as_ref().expect("completion token").cancel();

    // Allow background complete_release task to run
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    // 5. Now replacement request can be admitted
    let handle_2 = manager
        .acquire_connection(&input_name, &addr_2, -1, ConnectionKind::Normal)
        .expect("replacement should succeed after victim open future has completed release");

    manager.release_handle(&handle_2);
}

#[tokio::test]
async fn test_normal_connection_preempts_existing_soft_connection() {
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let soft_addr: SocketAddr = "127.0.0.1:49011".parse().unwrap();
    let normal_addr: SocketAddr = "127.0.0.1:49012".parse().unwrap();

    let soft_alloc = manager
        .acquire_connection(&input_name, &soft_addr, default_user_priority(), ConnectionKind::Soft)
        .expect("soft allocation");
    let soft_token = soft_alloc.cancel_token.clone().expect("soft allocations expose a cancel token");

    let normal_alloc = manager
        .acquire_connection(&input_name, &normal_addr, default_user_priority(), ConnectionKind::Normal)
        .expect("normal allocation should preempt soft allocation");

    tokio::task::yield_now().await;
    assert!(soft_token.is_cancelled(), "soft allocation should be preempted by normal traffic");
    assert_eq!(manager.get_provider_connections_count(), 1);

    manager.release_handle(&normal_alloc);
}

#[tokio::test]
async fn test_higher_priority_soft_preempts_lower_priority_soft() {
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let low_soft_addr: SocketAddr = "127.0.0.1:49013".parse().unwrap();
    let high_soft_addr: SocketAddr = "127.0.0.1:49014".parse().unwrap();

    let low_soft_alloc = manager
        .acquire_connection(&input_name, &low_soft_addr, 10, ConnectionKind::Soft)
        .expect("low-priority soft allocation");
    let low_soft_token = low_soft_alloc.cancel_token.clone().expect("soft allocations expose a cancel token");

    let high_soft_alloc = manager
        .acquire_connection(&input_name, &high_soft_addr, -5, ConnectionKind::Soft)
        .expect("higher-priority soft allocation should preempt lower-priority soft allocation");

    tokio::task::yield_now().await;
    assert!(low_soft_token.is_cancelled(), "lower-priority soft allocation should be preempted");
    assert_eq!(manager.get_provider_connections_count(), 1);

    manager.release_handle(&high_soft_alloc);
}

#[tokio::test]
async fn test_reclassify_soft_to_normal_prevents_same_priority_preemption() {
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let promoted_addr: SocketAddr = "127.0.0.1:49015".parse().unwrap();
    let challenger_addr: SocketAddr = "127.0.0.1:49016".parse().unwrap();

    let promoted_alloc = manager
        .acquire_connection(&input_name, &promoted_addr, default_user_priority(), ConnectionKind::Soft)
        .expect("initial soft allocation");
    let promoted_token = promoted_alloc.cancel_token.clone().expect("soft allocations expose a cancel token");

    assert!(
        manager.reclassify_connection(&promoted_addr, ConnectionKind::Normal, default_user_priority()),
        "soft allocation should be promotable to normal"
    );

    let challenger = manager.acquire_connection_with_grace(
        &input_name,
        &challenger_addr,
        false,
        default_user_priority(),
        ConnectionKind::Normal,
    );
    assert!(challenger.is_none(), "same-priority normal traffic should not preempt a promoted normal connection");
    assert!(!promoted_token.is_cancelled(), "promoted connection should remain active");
    assert_eq!(manager.get_provider_connections_count(), 1);

    manager.release_handle(&promoted_alloc);
}

#[tokio::test(start_paused = true)]
async fn affinity_never_forces_grace_over_allocation_while_primary_is_free() -> Result<(), String> {
    let (manager, input, alias) = alias_pool(2, 1);
    live_hls_playback_on_alias_with_lapsed_lease(
        &manager,
        &input,
        &alias,
        "client|alice|42|hls|0123456789abcdef",
        PlaybackRequestOutcome::Completed,
    )
    .await?;
    let alias_holder = manager
        .acquire_exact_connection_with_lease_for_session(
            &alias,
            &SocketAddr::from(([172, 18, 0, 9], 50_020)),
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(PlaybackLeaseRef::new("client|erin|9|hls|0123456789abcdef", PlaybackKind::LiveHls)),
        )
        .ok_or("alias holder")?;

    // Grace is enabled by default and allowed for this request.
    let reentry = manager
        .acquire_connection_with_lease_for_session(
            &input,
            &SocketAddr::from(([172, 18, 0, 9], 50_011)),
            true,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(PlaybackLeaseRef::new("client|alice|42|hls|fedcba9876543210", PlaybackKind::LiveHls)),
        )
        .ok_or("re-entry")?;
    assert_eq!(reentry.allocation.get_provider_name(), Some(Arc::clone(&input)));
    assert!(
        matches!(reentry.allocation, ProviderAllocation::Available(_)),
        "a free primary slot beats a grace allocation"
    );
    manager.release_handle(&reentry);
    manager.release_handle(&alias_holder);
    Ok(())
}

#[tokio::test]
async fn test_single_request_preemption_awaits_victim_completion() {
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
    let input_name: Arc<str> = "provider_1".intern();

    let low_addr = SocketAddr::from(([127, 0, 0, 1], 48_001));
    let high_addr = SocketAddr::from(([127, 0, 0, 1], 48_002));

    let low_handle = manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &low_addr,
            false,
            10,
            ConnectionKind::Normal,
            Some("session-victim"),
        )
        .expect("initial low-priority allocation succeeds");

    manager.mark_opening(low_handle.allocation_id);
    assert!(manager.register_body_owner(low_handle.allocation_id));

    let cancel_token = low_handle.cancel_token.clone().unwrap();
    let completion_token = low_handle.completion_token.clone().unwrap();

    // Simulate upstream body owner: when cancelled, complete release after 30ms
    let comp_clone = completion_token.clone();
    tokio::spawn(async move {
        cancel_token.cancelled().await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        comp_clone.cancel();
    });

    // High priority acquire should preempt the victim, await its completion, and succeed in the same call
    let high_handle = manager
        .acquire_connection_with_lease_for_session_await(
            &input_name,
            &high_addr,
            false,
            0,
            ConnectionKind::Normal,
            None,
        )
        .await
        .expect("high-priority allocation must succeed in the same call after awaiting victim completion");

    assert_eq!(manager.get_provider_connections_count(), 1);
    manager.release_handle(&high_handle);
    assert_eq!(manager.get_provider_connections_count(), 0);
}
