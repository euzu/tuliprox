use super::{
    acquire_live_hls_from_lineup, alias_pool, build_test_app_config, confirmed_alias_request,
    create_test_app_config_single_provider_pool, create_test_app_config_single_unlimited_provider_pool,
    create_test_app_config_with_dual_provider_pool, create_test_app_config_with_pool,
    live_hls_playback_on_alias_with_lapsed_lease, ActiveProviderManager, ConnectionKind, PlaybackLeaseRef,
};
use crate::{EventManager, SharedStreamManager};
use shared::{defaults::default_user_priority, utils::Internable};
use std::{
    net::SocketAddr,
    sync::{Arc, Weak},
    time::Duration,
};
use tuliprox_core::model::{PlaybackKind, PlaybackRequestOutcome};

#[test]
fn shared_stream_manager_backref_is_weak_and_does_not_leak() {
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let provider = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
    let shared = Arc::new(SharedStreamManager::new(Arc::clone(&provider)));
    provider.set_shared_stream_manager(&shared);

    // The only strong reference to the shared manager is the local `shared`; the
    // provider holds a Weak backref, so dropping `shared` releases both managers.
    drop(shared);
    assert!(provider.shared_stream_manager.get().and_then(Weak::upgrade).is_none());
}

#[tokio::test]
async fn two_playbacks_sharing_one_proxy_socket_keep_separate_slots() {
    let app_cfg = create_test_app_config_with_pool(2, 3);
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
    let input_name = "provider_1".intern();
    // Both external clients arrive through the same reverse-proxy peer socket.
    let proxy_addr = SocketAddr::from(([172, 18, 0, 9], 53_000));

    let first = manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &proxy_addr,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some("device-one"),
        )
        .expect("first device behind the proxy acquires a slot");
    let second = manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &proxy_addr,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some("device-two"),
        )
        .expect("second device behind the proxy acquires its own slot");
    assert_ne!(first.allocation_id, second.allocation_id);
    manager.confirm_playback_activity("device-one");
    manager.confirm_playback_activity("device-two");

    // Releasing one playback must free exactly its own allocation and leave the
    // other device's slot on the same transport untouched.
    manager.release_handle(&first);
    manager.finish_playback_request("device-one", PlaybackRequestOutcome::Completed, None);
    let remaining = manager.provider_capacities_for_input(&input_name);
    let primary = remaining.iter().find(|(name, _, _)| name.as_ref() == "provider_1").expect("primary pool entry");
    assert_eq!(primary.1, 1, "exactly one slot must remain in use on the shared socket");
    assert!(manager.read_leases().lease_of_owner("device-one").is_none());
    assert!(manager.read_leases().lease_of_owner("device-two").is_some());

    manager.release_handle(&second);
    manager.finish_playback_request("device-two", PlaybackRequestOutcome::Completed, None);
}

#[tokio::test]
async fn test_force_exact_acquire_does_not_overallocate_busy_provider() {
    let app_cfg = create_test_app_config_with_dual_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let client_1_addr: SocketAddr = "127.0.0.1:40001".parse().unwrap();
    let client_2_addr: SocketAddr = "127.0.0.1:40002".parse().unwrap();

    let first_alloc = manager
        .acquire_connection(&input_name, &client_1_addr, default_user_priority(), ConnectionKind::Normal)
        .expect("client1 initial allocation");
    let pinned_provider = first_alloc.allocation.get_provider_name().expect("provider name expected");
    assert_eq!(pinned_provider.as_ref(), "provider_1");

    // provider_1 has max_connections=1 and is already in use by client1
    let forced = manager.force_exact_acquire_connection(
        &pinned_provider,
        &client_2_addr,
        default_user_priority(),
        ConnectionKind::Normal,
    );
    assert!(forced.is_none(), "forced exact acquire must not over-allocate busy provider");

    manager.release_connection(&client_1_addr);
    manager.release_connection(&client_2_addr);
}

#[tokio::test]
async fn test_force_session_fallback_uses_different_provider_when_current_is_busy() {
    let app_cfg = create_test_app_config_with_dual_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let client_1_addr: SocketAddr = "127.0.0.1:41001".parse().unwrap();
    let client_2_addr: SocketAddr = "127.0.0.1:41002".parse().unwrap();

    // Step 1: Client1 starts movie -> provider_1
    let first_alloc = manager
        .acquire_connection(&input_name, &client_1_addr, default_user_priority(), ConnectionKind::Normal)
        .expect("client1 initial allocation");
    assert_eq!(first_alloc.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

    // Step 2: Client1 stops -> release provider_1
    manager.release_connection(&client_1_addr);

    // Step 3: Client2 starts live -> provider_1
    let live_alloc = manager
        .acquire_connection(&input_name, &client_2_addr, default_user_priority(), ConnectionKind::Normal)
        .expect("client2 live allocation");
    let busy_provider = live_alloc.allocation.get_provider_name().expect("provider name expected");
    assert_eq!(busy_provider.as_ref(), input_name.as_ref());
    assert!(manager.is_exhausted(&busy_provider));

    // Step 4: Client1 restarts same movie.
    // This emulates force-session fallback path by acquiring without provider grace.
    let fallback_alloc = manager
        .acquire_connection_with_grace(&input_name, &client_1_addr, false, 0, ConnectionKind::Normal)
        .expect("client1 fallback allocation without grace");
    let fallback_provider = fallback_alloc.allocation.get_provider_name().expect("fallback provider expected");

    assert_ne!(fallback_provider.as_ref(), busy_provider.as_ref());
    assert_eq!(fallback_provider.as_ref(), "provider_2");

    manager.release_connection(&client_1_addr);
    manager.release_connection(&client_2_addr);
}

#[tokio::test]
async fn test_seek_reacquire_stays_on_same_provider_account_until_stop() {
    let app_cfg = create_test_app_config_with_dual_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let client_1_addr: SocketAddr = "127.0.0.1:42001".parse().unwrap();
    let client_2_addr: SocketAddr = "127.0.0.1:42002".parse().unwrap();

    // Initial playback for client1.
    let first_alloc = manager
        .acquire_connection(&input_name, &client_1_addr, default_user_priority(), ConnectionKind::Normal)
        .expect("client1 initial allocation");
    let pinned_provider = first_alloc.allocation.get_provider_name().expect("provider name expected");
    assert_eq!(pinned_provider.as_ref(), "provider_1");

    // Another client occupies the alternate account while client1 keeps seeking.
    let second_alloc = manager
        .acquire_connection(&input_name, &client_2_addr, default_user_priority(), ConnectionKind::Normal)
        .expect("client2 allocation");
    let second_provider = second_alloc.allocation.get_provider_name().expect("provider name expected");
    assert_eq!(second_provider.as_ref(), "provider_2");

    // Simulate repeated seek/range reconnects for client1:
    // release old connection for the same client, then force exact pinned provider.
    for _ in 0..3 {
        manager.release_connection(&client_1_addr);
        let seek_alloc = manager
            .force_exact_acquire_connection(
                &pinned_provider,
                &client_1_addr,
                default_user_priority(),
                ConnectionKind::Normal,
            )
            .expect("seek reacquire should stay on pinned provider");
        let seek_provider = seek_alloc.allocation.get_provider_name().expect("provider name expected");
        assert_eq!(seek_provider.as_ref(), pinned_provider.as_ref());
    }

    // Stream stop / cleanup.
    manager.release_connection(&client_1_addr);
    manager.release_connection(&client_2_addr);
}

#[tokio::test(start_paused = true)]
async fn denied_confirmation_followed_by_refresh_cannot_reserve_foreign_slot() {
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
    let input_name = "provider_1".intern();
    let owner_x = "session-owner-x";
    let owner_y = "session-owner-y";
    let owner_z = "session-owner-z";
    let addr_y: SocketAddr = "127.0.0.1:43310".parse().unwrap();
    let addr_z: SocketAddr = "127.0.0.1:43311".parse().unwrap();

    // Y holds the only provider slot.
    let y = manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &addr_y,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(owner_y),
        )
        .expect("y acquires the single slot");

    // X starts a lease and confirms media while Y holds the slot, so its
    // reservation right is denied. A later refresh must not restore it.
    manager.refresh_provider_reservation(&input_name, owner_x, 15);
    manager.confirm_playback_activity(owner_x);
    manager.refresh_provider_reservation(&input_name, owner_x, 15);

    manager.release_handle(&y);

    // If X's refresh had re-granted the denied reservation, Z would be blocked here.
    let z = manager.acquire_connection_with_grace_for_session(
        &input_name,
        &addr_z,
        false,
        default_user_priority(),
        ConnectionKind::Normal,
        Some(owner_z),
    );
    assert!(z.is_some(), "a denied reservation must not be restored by a later refresh");
    if let Some(z) = z {
        manager.release_handle(&z);
    }
}

#[tokio::test]
async fn test_reclassify_connection_for_owner_preserves_other_owner_on_same_socket() {
    let app_cfg = build_test_app_config(None, 2);
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

    let input_name = "provider_1".intern();
    let shared_addr: SocketAddr = "127.0.0.1:49020".parse().unwrap();

    let alloc_1 = manager
        .acquire_connection_with_lease_for_session(
            &input_name,
            &shared_addr,
            false,
            -10,
            ConnectionKind::Soft,
            Some(PlaybackLeaseRef::new("owner-1", PlaybackKind::LiveTs)),
        )
        .expect("first soft allocation");

    let alloc_2 = manager
        .acquire_connection_with_lease_for_session(
            &input_name,
            &shared_addr,
            false,
            -10,
            ConnectionKind::Soft,
            Some(PlaybackLeaseRef::new("owner-2", PlaybackKind::LiveTs)),
        )
        .expect("second soft allocation");

    assert!(manager.reclassify_connection_for_owner(&shared_addr, Some("owner-1"), ConnectionKind::Normal, 0));

    {
        let connections = manager.read_connections();
        let info_1 = connections.single.get(&alloc_1.allocation_id).expect("alloc_1 exists");
        assert_eq!(info_1.kind, ConnectionKind::Normal);
        assert_eq!(info_1.priority, 0);

        let info_2 = connections.single.get(&alloc_2.allocation_id).expect("alloc_2 exists");
        assert_eq!(info_2.kind, ConnectionKind::Soft);
        assert_eq!(info_2.priority, -10);
    }

    manager.release_handle(&alloc_1);
    manager.release_handle(&alloc_2);
}

#[test]
fn recording_allocation_is_claimed_without_consuming_a_second_slot() {
    let app_cfg = build_test_app_config(None, 1);
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
    let input_name = "provider_1".intern();

    let reserved =
        manager.acquire_connection_for_download(&input_name, 0).expect("recording reserves the only provider slot");
    assert_eq!(manager.get_provider_connections_count(), 1);

    let claimed = manager
        .claim_download_connection(reserved.allocation_id, &input_name)
        .expect("internal recording request claims the existing reservation");
    assert_eq!(claimed.allocation_id, reserved.allocation_id);
    assert_eq!(manager.get_provider_connections_count(), 1, "claim must not allocate a second slot");
    assert!(
        manager.claim_download_connection(reserved.allocation_id, &input_name).is_none(),
        "one reservation can only own one provider body"
    );

    manager.complete_release(reserved.allocation_id);
    assert_eq!(manager.get_provider_connections_count(), 0);
}

/// A=2 / B=3 pool: five confirmed playbacks exhaust the pool and a sixth start is
/// rejected; releasing one confirmed playback frees exactly one slot again.
#[tokio::test]
async fn sixth_playback_rejected_after_pool_exhausted_by_confirmed_leases() {
    let app_cfg = create_test_app_config_with_pool(2, 3);
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
    let input_name = "provider_1".intern();

    let mut acquired = Vec::new();
    for index in 0..5u16 {
        let owner = format!("playback-{index}");
        let addr = SocketAddr::from(([172, 18, 0, 9], 60_000 + index));
        let handle = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &addr,
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some(&owner),
            )
            .expect("pool has capacity for five playbacks");
        manager.confirm_playback_activity(&owner);
        acquired.push((handle, owner));
    }

    let sixth = manager.acquire_connection_with_grace_for_session(
        &input_name,
        &SocketAddr::from(([172, 18, 0, 9], 60_005)),
        false,
        default_user_priority(),
        ConnectionKind::Normal,
        Some("playback-5"),
    );
    assert!(sixth.is_none(), "sixth start must be rejected once the pool is exhausted");

    manager.release_handle(&acquired[0].0);
    let replacement = manager.acquire_connection_with_grace_for_session(
        &input_name,
        &SocketAddr::from(([172, 18, 0, 9], 60_006)),
        false,
        default_user_priority(),
        ConnectionKind::Normal,
        Some("playback-6"),
    );
    assert!(replacement.is_some(), "freed capacity must be selectable again");
    manager.release_handle(&replacement.expect("replacement handle"));
    for (handle, _owner) in &acquired[1..] {
        manager.release_handle(handle);
    }
    assert_eq!(manager.get_provider_connections_count(), 0);
}

#[test]
fn live_hls_lease_owner_preserves_other_playback_identities() {
    let owner = "client|alice|42";
    assert_eq!(super::super::playback_lease_owner("client|alice|42|hls|0123456789abcdef"), owner);
    for token in [
        owner,
        "client|alice|42|hls|short",
        "client|alice|42|hls|0123456789abcde!",
        "m3u-catchup|client|alice|42|hls|0123456789abcdef",
        "catchup|client|alice|42|hls|0123456789abcdef",
        "hls-cache:client|hls|0123456789abcdef",
    ] {
        assert_eq!(super::super::playback_lease_owner(token), token);
    }
    assert_ne!(
        super::super::playback_lease_owner("client|alice|42|hls|0123456789abcdef"),
        super::super::playback_lease_owner("other-client|alice|42|hls|0123456789abcdef")
    );
}

#[tokio::test(start_paused = true)]
async fn live_hls_retries_with_unique_tokens_keep_one_provider_lease() -> Result<(), String> {
    let app_cfg = create_test_app_config_with_pool(2, 4);
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
    let input = "provider_1".intern();
    let alias = "provider_2".intern();
    let mut original_tag = None;
    let mut previous_request: Option<(
        String,
        tuliprox_core::model::PlaybackRequestId,
        Option<tuliprox_core::model::ProviderBindingTag>,
    )> = None;
    for attempt in 0..104_u16 {
        let token = format!("client|alice|42|hls|{attempt:016x}");
        let handle = manager
            .acquire_connection_with_lease_for_session(
                &input,
                &SocketAddr::from(([172, 18, 0, 9], 55_000 + attempt)),
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some(PlaybackLeaseRef::new(&token, PlaybackKind::LiveHls)),
            )
            .ok_or("retry must reuse the reserved provider")?;
        let request_id = handle.playback_request_id.ok_or("identified HLS request")?;
        let provider = handle.allocation.get_provider_name().ok_or("provider")?;
        assert_eq!(provider, input);
        let tag = handle.binding_tag.ok_or("binding tag")?;
        let first_tag = original_tag.get_or_insert(tag);
        assert_eq!(tag.lease_id, first_tag.lease_id);
        manager.refresh_adaptive_playback_lease(&provider, &token, PlaybackKind::LiveHls, 60);
        manager.confirm_identified_playback_activity(&token, request_id);
        assert!(manager.should_reuse_playback_provider(&token, &provider));
        manager.release_handle(&handle);
        manager.finish_identified_playback_request(&token, request_id, PlaybackRequestOutcome::Completed);
        manager.clear_provider_reservation(&token);
        if let Some((old_token, old_request, old_tag)) = previous_request.take() {
            manager.clear_identified_provider_reservation(&old_token, &provider, old_tag);
            manager.finish_identified_playback_request(&old_token, old_request, PlaybackRequestOutcome::ProviderFailed);
            assert!(manager.confirm_identified_playback_activity(&old_token, old_request).is_none());
        }
        previous_request = Some((token, request_id, handle.binding_tag));
        assert_eq!(manager.provider_lease_usage(&input).idle, 1);
        assert_eq!(manager.provider_lease_usage(&alias).total(), 0);
    }
    let foreign = manager
        .acquire_connection_with_lease_for_session(
            &input,
            &SocketAddr::from(([172, 18, 0, 10], 56_000)),
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(PlaybackLeaseRef::new("other-client|alice|42|hls|0123456789abcdef", PlaybackKind::LiveHls)),
        )
        .ok_or("another playback must retain free provider capacity")?;
    manager.release_handle(&foreign);
    tokio::time::advance(Duration::from_secs(61)).await;
    assert_eq!(manager.provider_lease_usage(&input).total(), 0);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn live_hls_alias_affinity_ends_after_its_window() -> Result<(), String> {
    let (manager, input, alias) = alias_pool(2, 0);
    live_hls_playback_on_alias_with_lapsed_lease(
        &manager,
        &input,
        &alias,
        "client|alice|42|hls|0123456789abcdef",
        PlaybackRequestOutcome::Completed,
    )
    .await?;
    tokio::time::advance(Duration::from_secs(shared::defaults::default_provider_affinity_ttl_secs())).await;

    let reentry = acquire_live_hls_from_lineup(&manager, &input, "client|alice|42|hls|fedcba9876543210", 50_011)?;
    assert_eq!(reentry.allocation.get_provider_name(), Some(Arc::clone(&input)));
    manager.release_handle(&reentry);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn delayed_failure_of_older_binding_keeps_successor_affinity() -> Result<(), String> {
    let (manager, input, alias) = alias_pool(2, 0);
    let old_token = "client|alice|42|hls|0123456789abcdef";
    let new_token = "client|alice|42|hls|fedcba9876543210";

    // An older binding on the primary that never confirmed media.
    let old = manager
        .acquire_exact_connection_with_lease_for_session(
            &input,
            &SocketAddr::from(([172, 18, 0, 9], 50_000)),
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(PlaybackLeaseRef::new(old_token, PlaybackKind::LiveHls)),
        )
        .ok_or("old binding")?;
    let old_request = old.playback_request_id.ok_or("old request")?;
    let old_tag = old.binding_tag.ok_or("old tag")?;
    manager.release_handle(&old);

    let current = confirmed_alias_request(&manager, &input, &alias, new_token)?;
    let current_request = current.playback_request_id.ok_or("current request")?;
    manager.release_handle(&current);
    manager.finish_identified_playback_request(new_token, current_request, PlaybackRequestOutcome::Completed);

    // Delayed manifest failures of the older binding arrive after the rebind.
    assert!(!manager.forget_identified_provider_affinity(old_token, &input, old_tag));
    manager.finish_identified_playback_request(old_token, old_request, PlaybackRequestOutcome::ProviderFailed);
    tokio::time::advance(Duration::from_secs(20)).await;
    assert_eq!(manager.provider_lease_usage(&alias).total(), 0, "lease must have expired");
    manager.finish_identified_playback_request(old_token, old_request, PlaybackRequestOutcome::ProviderFailed);

    let reentry = acquire_live_hls_from_lineup(&manager, &input, "client|alice|42|hls|00000000000000aa", 50_011)?;
    assert_eq!(reentry.allocation.get_provider_name(), Some(Arc::clone(&alias)));
    manager.release_handle(&reentry);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn administrative_termination_of_hls_token_ends_affinity_and_lease() -> Result<(), String> {
    let (manager, input, alias) = alias_pool(2, 0);
    let token = "client|alice|42|hls|0123456789abcdef";
    let handle = confirmed_alias_request(&manager, &input, &alias, token)?;
    let request_id = handle.playback_request_id.ok_or("identified request")?;
    manager.release_handle(&handle);
    manager.finish_identified_playback_request(token, request_id, PlaybackRequestOutcome::Completed);
    assert_eq!(manager.provider_lease_usage(&alias).idle, 1);

    // The bare-owner clear has no delete right for public HLS tokens.
    manager.clear_provider_reservation(token);
    assert_eq!(manager.provider_lease_usage(&alias).idle, 1);
    manager.terminate_identified_playback_owner(token);
    assert_eq!(manager.provider_lease_usage(&alias).total(), 0);

    let reentry = acquire_live_hls_from_lineup(&manager, &input, "client|alice|42|hls|fedcba9876543210", 50_011)?;
    assert_eq!(reentry.allocation.get_provider_name(), Some(Arc::clone(&input)));
    manager.release_handle(&reentry);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn stale_hls_token_termination_keeps_newer_binding() -> Result<(), String> {
    let (manager, input, alias) = alias_pool(2, 0);
    let old_token = "client|alice|42|hls|0123456789abcdef";
    let new_token = "client|alice|42|hls|fedcba9876543210";

    let old = confirmed_alias_request(&manager, &input, &alias, old_token)?;
    let old_request = old.playback_request_id.ok_or("old request")?;
    manager.release_handle(&old);
    manager.finish_identified_playback_request(old_token, old_request, PlaybackRequestOutcome::Completed);

    // The retry re-enters the idle lease and starts a new binding incarnation.
    let new = acquire_live_hls_from_lineup(&manager, &input, new_token, 50_011)?;
    assert_eq!(new.allocation.get_provider_name(), Some(Arc::clone(&alias)));
    assert_ne!(new.binding_tag, old.binding_tag);
    let new_request = new.playback_request_id.ok_or("new request")?;
    manager.refresh_adaptive_playback_lease(&alias, new_token, PlaybackKind::LiveHls, 15);
    manager.confirm_identified_playback_activity(new_token, new_request).ok_or("confirmation")?;
    manager.release_handle(&new);
    manager.finish_identified_playback_request(new_token, new_request, PlaybackRequestOutcome::Completed);
    assert_eq!(manager.provider_lease_usage(&alias).idle, 1);

    assert!(!manager.terminate_identified_playback_owner(old_token));
    assert_eq!(manager.provider_lease_usage(&alias).idle, 1, "stale token must keep the newer lease");
    assert_eq!(manager.provider_affinity_for_owner(new_token), Some(Arc::clone(&alias)));

    assert!(manager.terminate_identified_playback_owner(new_token));
    assert_eq!(manager.provider_lease_usage(&alias).total(), 0);
    assert_eq!(manager.provider_affinity_for_owner(new_token), None);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn failure_on_fallback_provider_keeps_preference_for_preferred_provider() -> Result<(), String> {
    let (manager, input, alias) = alias_pool(2, 1);
    let token = "client|alice|42|hls|0123456789abcdef";
    live_hls_playback_on_alias_with_lapsed_lease(&manager, &input, &alias, token, PlaybackRequestOutcome::Completed)
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

    // The full alias sends the re-entry to the primary, which then fails before media.
    let fallback = acquire_live_hls_from_lineup(&manager, &input, "client|alice|42|hls|fedcba9876543210", 50_011)?;
    assert_eq!(fallback.allocation.get_provider_name(), Some(Arc::clone(&input)));
    let fallback_request = fallback.playback_request_id.ok_or("fallback request")?;
    manager.release_handle(&fallback);
    manager.finish_identified_playback_request(
        "client|alice|42|hls|fedcba9876543210",
        fallback_request,
        PlaybackRequestOutcome::FailedBeforeMedia,
    );

    assert_eq!(manager.provider_affinity_for_owner(token), Some(Arc::clone(&alias)));
    manager.release_handle(&alias_holder);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn live_ts_playback_records_no_provider_affinity() -> Result<(), String> {
    let (manager, input, alias) = alias_pool(2, 0);
    let busy_1 = acquire_live_hls_from_lineup(&manager, &input, "busy-1|bob|7|hls|0123456789abcdef", 50_001)?;
    let busy_2 = acquire_live_hls_from_lineup(&manager, &input, "busy-2|carol|8|hls|0123456789abcdef", 50_002)?;
    let owner = "ts-session-token";
    let handle = manager
        .acquire_connection_with_lease_for_session(
            &input,
            &SocketAddr::from(([172, 18, 0, 9], 50_010)),
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(PlaybackLeaseRef::new(owner, PlaybackKind::LiveTs)),
        )
        .ok_or("ts playback")?;
    assert_eq!(handle.allocation.get_provider_name(), Some(Arc::clone(&alias)));
    let request_id = handle.playback_request_id.ok_or("ts request")?;
    manager.confirm_identified_playback_activity(owner, request_id).ok_or("confirmation")?;
    manager.release_handle(&handle);
    manager.finish_identified_playback_request(owner, request_id, PlaybackRequestOutcome::ClientClosed);
    manager.release_handle(&busy_1);
    manager.release_handle(&busy_2);

    assert_eq!(manager.provider_affinity_for_owner(owner), None, "live TS has nothing to return to");
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn huge_reconnect_and_affinity_windows_never_panic() -> Result<(), String> {
    let (manager, input, alias) = alias_pool(2, 0);
    manager.write_leases().set_affinity_ttl(crate::provider_leases::ProviderAffinityTtl(u64::MAX));
    let token = "client|alice|42|hls|0123456789abcdef";
    let handle = acquire_live_hls_from_lineup(&manager, &input, token, 50_010)?;
    let provider = handle.allocation.get_provider_name().ok_or("provider")?;
    let request_id = handle.playback_request_id.ok_or("request")?;
    manager.refresh_adaptive_playback_lease(&provider, token, PlaybackKind::LiveHls, u64::MAX);
    manager.confirm_identified_playback_activity(token, request_id).ok_or("confirmation")?;
    manager.release_handle(&handle);
    manager.finish_identified_playback_request(token, request_id, PlaybackRequestOutcome::Completed);
    assert_eq!(manager.provider_affinity_for_owner(token), Some(provider));
    assert_eq!(manager.provider_lease_usage(&alias).total(), 0);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn zero_provider_affinity_ttl_ends_affinity_with_reconnect_window() -> Result<(), String> {
    let app_cfg = create_test_app_config_with_pool(2, 0);
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
    manager.write_leases().set_affinity_ttl(crate::provider_leases::ProviderAffinityTtl(0));
    let input = "provider_1".intern();
    let alias = "provider_2".intern();
    live_hls_playback_on_alias_with_lapsed_lease(
        &manager,
        &input,
        &alias,
        "client|alice|42|hls|0123456789abcdef",
        PlaybackRequestOutcome::Completed,
    )
    .await?;

    let reentry = acquire_live_hls_from_lineup(&manager, &input, "client|alice|42|hls|fedcba9876543210", 50_011)?;
    assert_eq!(reentry.allocation.get_provider_name(), Some(Arc::clone(&input)));
    manager.release_handle(&reentry);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn live_hls_provider_failure_ends_alias_affinity() -> Result<(), String> {
    let (manager, input, alias) = alias_pool(2, 0);
    live_hls_playback_on_alias_with_lapsed_lease(
        &manager,
        &input,
        &alias,
        "client|alice|42|hls|0123456789abcdef",
        PlaybackRequestOutcome::ProviderFailed,
    )
    .await?;

    let reentry = acquire_live_hls_from_lineup(&manager, &input, "client|alice|42|hls|fedcba9876543210", 50_011)?;
    assert_eq!(reentry.allocation.get_provider_name(), Some(Arc::clone(&input)));
    manager.release_handle(&reentry);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn live_hls_full_affinity_provider_falls_back_to_lineup() -> Result<(), String> {
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

    let reentry = acquire_live_hls_from_lineup(&manager, &input, "client|alice|42|hls|fedcba9876543210", 50_011)?;
    assert_eq!(reentry.allocation.get_provider_name(), Some(Arc::clone(&input)));
    manager.release_handle(&reentry);
    manager.release_handle(&alias_holder);
    Ok(())
}

/// max=1: X fetch → physical release → Y acquire → X first byte within the startup
/// deadline. X's late confirmation records media but must not over-commit Y's slot.
#[tokio::test(start_paused = true)]
async fn late_first_byte_cannot_reserve_slot_taken_by_other_playback() {
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
    let input_name = "provider_1".intern();

    let x_handle = manager
        .acquire_connection_with_lease_for_session(
            &input_name,
            &SocketAddr::from(([172, 18, 0, 9], 40_000)),
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(PlaybackLeaseRef::new("x", PlaybackKind::LiveHls)),
        )
        .expect("x acquires the only slot");
    manager.refresh_adaptive_playback_lease(&input_name, "x", PlaybackKind::LiveHls, 15);
    manager.release_handle(&x_handle);

    let y_handle = manager
        .acquire_connection_with_lease_for_session(
            &input_name,
            &SocketAddr::from(([172, 18, 0, 9], 40_001)),
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(PlaybackLeaseRef::new("y", PlaybackKind::LiveHls)),
        )
        .expect("y acquires the now-free slot");

    manager.confirm_playback_activity("x");
    assert_eq!(manager.get_provider_connections_count(), 1, "y still holds the only slot");
    assert!(
        !manager.is_provider_reserved_for_other_session(&input_name, Some("y")),
        "x's late confirmation must not over-commit the provider"
    );

    manager.confirm_playback_activity("y");
    manager.release_handle(&y_handle);
    manager.finish_playback_request("y", PlaybackRequestOutcome::Completed, None);
}

/// Counterexample: the same X fetch → release → first-byte sequence without a
/// competing Y keeps the free slot as a real reservation.
#[tokio::test(start_paused = true)]
async fn first_byte_reserves_when_slot_still_free() {
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
    let input_name = "provider_1".intern();

    let x_handle = manager
        .acquire_connection_with_lease_for_session(
            &input_name,
            &SocketAddr::from(([172, 18, 0, 9], 40_002)),
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(PlaybackLeaseRef::new("x", PlaybackKind::LiveHls)),
        )
        .expect("x acquires the only slot");
    manager.refresh_adaptive_playback_lease(&input_name, "x", PlaybackKind::LiveHls, 15);
    manager.release_handle(&x_handle);

    // No competing playback took the slot, so X's first byte reserves it.
    manager.confirm_playback_activity("x");
    assert!(
        manager.is_provider_reserved_for_other_session(&input_name, Some("other")),
        "a free slot must be reservable by the confirming playback"
    );

    manager.finish_playback_request("x", PlaybackRequestOutcome::Completed, None);
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
pub(in crate::active_provider_manager::tests) fn latency_percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

#[cfg(target_os = "linux")]
pub(in crate::active_provider_manager::tests) fn resident_set_kib() -> Option<u64> {
    let Ok(content) = std::fs::read_to_string("/proc/self/status") else {
        return None;
    };
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.split_whitespace().next().and_then(|v| v.parse::<u64>().ok());
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
pub(in crate::active_provider_manager::tests) fn resident_set_kib() -> Option<u64> { None }

pub(in crate::active_provider_manager::tests) struct LeaseWorkloadStats {
    pub(in crate::active_provider_manager::tests) ops: u64,
    pub(in crate::active_provider_manager::tests) elapsed_secs: f64,
    pub(in crate::active_provider_manager::tests) acquire_p50: u64,
    pub(in crate::active_provider_manager::tests) acquire_p95: u64,
    pub(in crate::active_provider_manager::tests) acquire_p99: u64,
    pub(in crate::active_provider_manager::tests) confirm_p50: u64,
    pub(in crate::active_provider_manager::tests) confirm_p95: u64,
    pub(in crate::active_provider_manager::tests) confirm_p99: u64,
    pub(in crate::active_provider_manager::tests) release_p50: u64,
    pub(in crate::active_provider_manager::tests) release_p95: u64,
    pub(in crate::active_provider_manager::tests) release_p99: u64,
}

#[allow(clippy::cast_possible_truncation)]
pub(in crate::active_provider_manager::tests) async fn measure_lease_workload(
    manager: &Arc<ActiveProviderManager>,
    input_name: &Arc<str>,
    concurrency: usize,
    rounds_per_task: usize,
) -> LeaseWorkloadStats {
    let samples = Arc::new(std::sync::Mutex::new(Vec::<(u64, u64, u64)>::new()));
    let started = std::time::Instant::now();
    let mut tasks = tokio::task::JoinSet::new();
    for task_index in 0..concurrency {
        let manager = Arc::clone(manager);
        let input_name = Arc::clone(input_name);
        let samples = Arc::clone(&samples);
        tasks.spawn(async move {
            for round in 0..rounds_per_task {
                let owner = format!("bench-{task_index}-{round}");
                let addr = SocketAddr::from(([127, 0, 0, 1], 40_000 + (task_index as u16)));
                let acquire_at = std::time::Instant::now();
                let Some(handle) = manager.acquire_connection_with_grace_for_session(
                    &input_name,
                    &addr,
                    false,
                    0,
                    ConnectionKind::Normal,
                    Some(&owner),
                ) else {
                    continue;
                };
                let acquire_us = acquire_at.elapsed().as_micros() as u64;

                let confirm_at = std::time::Instant::now();
                manager.confirm_playback_activity(&owner);
                let confirm_us = confirm_at.elapsed().as_micros() as u64;

                let release_at = std::time::Instant::now();
                manager.release_handle(&handle);
                let release_us = release_at.elapsed().as_micros() as u64;

                // A physical release never concludes the request; model the real
                // lifecycle by finishing with a clean outcome so the lease returns
                // to baseline (LiveTs is not reconnect-capable, so it is removed).
                manager.finish_playback_request(&owner, PlaybackRequestOutcome::Completed, None);

                samples.lock().unwrap().push((acquire_us, confirm_us, release_us));
            }
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.expect("benchmark task must not fail");
    }

    let elapsed_secs = started.elapsed().as_secs_f64();
    let samples = Arc::try_unwrap(samples).expect("samples unique").into_inner().unwrap();
    let mut acquire = Vec::with_capacity(samples.len());
    let mut confirm = Vec::with_capacity(samples.len());
    let mut release = Vec::with_capacity(samples.len());
    for (a, c, r) in samples {
        acquire.push(a);
        confirm.push(c);
        release.push(r);
    }
    acquire.sort_unstable();
    confirm.sort_unstable();
    release.sort_unstable();
    let ops = acquire.len() as u64;
    LeaseWorkloadStats {
        ops,
        elapsed_secs,
        acquire_p50: latency_percentile(&acquire, 0.50),
        acquire_p95: latency_percentile(&acquire, 0.95),
        acquire_p99: latency_percentile(&acquire, 0.99),
        confirm_p50: latency_percentile(&confirm, 0.50),
        confirm_p95: latency_percentile(&confirm, 0.95),
        confirm_p99: latency_percentile(&confirm, 0.99),
        release_p50: latency_percentile(&release, 0.50),
        release_p95: latency_percentile(&release, 0.95),
        release_p99: latency_percentile(&release, 0.99),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manager microbenchmark: cargo +stable test -p tuliprox-session --release -- --ignored --nocapture bench_provider_lease_microbenchmark"]
#[allow(clippy::cast_precision_loss)]
async fn bench_provider_lease_microbenchmark() {
    let manager = Arc::new(ActiveProviderManager::new(
        &create_test_app_config_single_unlimited_provider_pool(),
        &Arc::new(EventManager::new()),
    ));
    let input_name: Arc<str> = "provider_1".intern();

    // Warm-up before measurement so allocator and lock caches are exercised.
    for index in 0..64u16 {
        let addr = SocketAddr::from(([127, 0, 0, 1], 40_000 + index));
        let Some(handle) = manager.acquire_connection_with_grace_for_session(
            &input_name,
            &addr,
            false,
            0,
            ConnectionKind::Normal,
            None,
        ) else {
            continue;
        };
        manager.release_handle(&handle);
    }

    let baseline_rss_kib = resident_set_kib();
    eprintln!("provider lease manager microbenchmark (unlimited provider, in-process, no HTTP bodies)");
    for concurrency in [1usize, 5, 50, 200] {
        let stats = measure_lease_workload(&manager, &input_name, concurrency, 10).await;
        assert_eq!(stats.ops, (concurrency * 10) as u64, "unexpected acquire failures at concurrency {concurrency}");
        let throughput = stats.ops as f64 / stats.elapsed_secs.max(f64::EPSILON);
        let rss_delta_kib = resident_set_kib()
            .zip(baseline_rss_kib)
            .map_or_else(|| "unsupported".to_string(), |(rss, baseline)| rss.saturating_sub(baseline).to_string());
        eprintln!(
            "concurrency={concurrency:>3} ops={:>5} throughput={:>9.1} ops/s | acquire p50/p95/p99={}/{}/{}us | confirm p50/p95/p99={}/{}/{}us | release p50/p95/p99={}/{}/{}us | rss_delta_kib={}",
            stats.ops,
            throughput,
            stats.acquire_p50,
            stats.acquire_p95,
            stats.acquire_p99,
            stats.confirm_p50,
            stats.confirm_p95,
            stats.confirm_p99,
            stats.release_p50,
            stats.release_p95,
            stats.release_p99,
            rss_delta_kib,
        );
    }

    // Soak-style churn: every connection and lease must return to baseline.
    assert_eq!(manager.get_provider_connections_count(), 0);
    let usage = manager.provider_lease_usage(&input_name);
    assert_eq!(usage.total(), 0, "lease table must return to baseline after churn");
}

#[tokio::test]
async fn stale_handle_does_not_cancel_newer_generation_slot() {
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
    let input_name: Arc<str> = "provider_1".intern();
    let addr = SocketAddr::from(([127, 0, 0, 1], 49_001));

    let handle = manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &addr,
            false,
            0,
            ConnectionKind::Normal,
            Some("session-gen"),
        )
        .expect("initial allocation succeeds");

    manager.mark_opening(handle.allocation_id);
    assert!(manager.register_body_owner(handle.allocation_id));

    // Gen 0 has an old completion token
    let old_completion_token = handle.completion_token.clone().expect("completion token present");

    // Renew opening tokens to advance generation to 1
    let (_new_cancel, new_completion, gen) =
        manager.renew_opening_tokens(handle.allocation_id).expect("renewal should succeed");
    assert_eq!(gen, 1);

    // Simulate an old handle belonging to generation 0 whose completion token was cancelled
    old_completion_token.cancel();
    let stale_handle = tuliprox_core::model::ProviderHandle {
        playback_request_id: handle.playback_request_id,
        binding_tag: handle.binding_tag,
        client_id: handle.client_id,
        allocation_id: handle.allocation_id,
        allocation: handle.allocation.clone(),
        cancel_token: handle.cancel_token.clone(),
        completion_token: Some(old_completion_token),
        close_reason: Arc::clone(&handle.close_reason),
        open_generation: 0, // Stale generation
    };

    // Releasing the stale handle must NOT poison/cancel the new generation's completion token
    manager.release_handle(&stale_handle);
    assert!(
        !new_completion.is_cancelled(),
        "new generation completion token must remain uncancelled after stale handle release"
    );
    assert_eq!(manager.get_provider_connections_count(), 1, "slot must still be held by the active new generation");

    // When the real new generation completes, the slot is reaped
    new_completion.cancel();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(manager.get_provider_connections_count(), 0);
}
