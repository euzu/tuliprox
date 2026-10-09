use super::{
    create_bound_hls_test_session, create_unbound_hls_test_session, get_response, overlap_provider_input,
    provider_admission::test_hls_origin_io_context, response_body, runtime_policy_endpoint_fixture,
    single_hls_provider_input, store_test_sources_with_target, test_addr_with_port, test_app_state,
    test_app_state_with_inputs, test_fingerprint, test_fingerprint_with_addr, test_hls_access_context,
    wait_for_provider_connection_count, wait_for_runtime_policy_terminal_plan,
};
use crate::{
    api::model::{
        begin_hls_origin_account_io, finish_hls_origin_account_io, ConnectionKind, HlsAccessLeaseId,
        HlsAccessLeaseState, HlsEffectiveOriginAcquirePolicy, HlsOriginAccountBinding, HlsOriginAccountBindingMode,
        HlsOriginAccountDetachedReason, HlsRuntimeCustomTailReason, ProxySessionId,
    },
    model::{Config, ConfigInput, ConfigTarget, ReverseProxyConfig},
};
use axum::http::{header, StatusCode};
use shared::model::{ConfigTargetDto, ReverseProxyConfigDto, StreamConfigDto};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn hls_origin_account_io_lease_allows_parallel_same_session_origin_work() {
    let input = overlap_provider_input();
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let session = create_bound_hls_test_session(&app_state, &input, "12345", "account-a", 1_000).await;
    let binding = session.read().await.origin_account_binding.clone().expect("binding exists");
    let origin_io = test_hls_origin_io_context(&app_state);

    let first = begin_hls_origin_account_io(&origin_io, &session, &binding)
        .await
        .expect("first same-session origin io acquires provider account");
    wait_for_provider_connection_count(&app_state, 1).await;
    let second = begin_hls_origin_account_io(&origin_io, &session, &binding)
        .await
        .expect("second same-session origin io joins session lease");
    wait_for_provider_connection_count(&app_state, 1).await;
    assert_eq!(
        session
            .read()
            .await
            .origin_account_io_lease
            .as_ref()
            .expect("session provider lease exists")
            .active_io_count
            .load(std::sync::atomic::Ordering::Relaxed),
        2
    );

    finish_hls_origin_account_io(&origin_io, &session, first, true).await;
    wait_for_provider_connection_count(&app_state, 1).await;
    assert_eq!(
        session
            .read()
            .await
            .origin_account_io_lease
            .as_ref()
            .expect("session provider lease remains while second io is active")
            .active_io_count
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );

    finish_hls_origin_account_io(&origin_io, &session, second, true).await;
    wait_for_provider_connection_count(&app_state, 0).await;
    assert!(!app_state
        .active_provider
        .is_provider_reserved_for_other_session(&binding.account_name, Some("other-hls-session")));
}

#[tokio::test]
async fn hls_origin_account_io_lease_blocks_other_hls_sessions_for_same_account() {
    let input = overlap_provider_input();
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let first_session = create_bound_hls_test_session(&app_state, &input, "12345", "account-a", 1_000).await;
    let second_session = create_bound_hls_test_session(&app_state, &input, "67890", "account-a", 1_000).await;
    let first_binding = first_session.read().await.origin_account_binding.clone().expect("binding exists");
    let second_binding = second_session.read().await.origin_account_binding.clone().expect("binding exists");
    let origin_io = test_hls_origin_io_context(&app_state);

    let first = begin_hls_origin_account_io(&origin_io, &first_session, &first_binding)
        .await
        .expect("first session acquires account");
    wait_for_provider_connection_count(&app_state, 1).await;

    assert!(begin_hls_origin_account_io(&origin_io, &second_session, &second_binding).await.is_err());
    wait_for_provider_connection_count(&app_state, 1).await;

    finish_hls_origin_account_io(&origin_io, &first_session, first, true).await;
    wait_for_provider_connection_count(&app_state, 0).await;
}

#[tokio::test]
async fn hls_account_overlap_selects_soft_candidate_but_not_hard_active() {
    let app_state = test_app_state();
    let input = ConfigInput { id: 1, name: Arc::from("overlap-input"), ..ConfigInput::default() };
    let session = create_bound_hls_test_session(&app_state, &input, "old", "account-a", 1_000).await;
    {
        let mut session = session.write().await;
        session.target_duration = Some(10);
        session.mark_authorized_media_access(1_000);
    }
    let new_proxy_session_id = ProxySessionId("new-session".to_string());

    let hard_candidate =
        super::super::find_hls_account_overlap_candidate(&app_state, &input, &new_proxy_session_id, 5_000).await;
    assert!(hard_candidate.is_none(), "hard-active sessions must not be overbooked");

    let delayed_candidate =
        super::super::find_hls_account_overlap_candidate(&app_state, &input, &new_proxy_session_id, 12_000).await;
    assert!(delayed_candidate.is_none(), "soft-active candidate must respect the dynamic overlap delay");

    let soft_candidate =
        super::super::find_hls_account_overlap_candidate(&app_state, &input, &new_proxy_session_id, 21_000)
            .await
            .expect("soft-active session can be overbooked");
    assert_eq!(soft_candidate.account_name.as_ref(), "account-a");
    assert_eq!(soft_candidate.last_media_at_ms, 1_000);
    assert_eq!(soft_candidate.soft_overlap_eligible_at_ms, 21_000);
    assert_eq!(soft_candidate.soft_overlap_delay_ms, 20_000);
    assert_eq!(soft_candidate.reclaim_until_ms, 31_000);
}

#[test]
fn hls_soft_overlap_delay_scales_with_tuliprox_target_pressure() {
    assert_eq!(super::super::hls_soft_overlap_delay_ms(10_000, 1, 1), 20_000);
    assert_eq!(super::super::hls_soft_overlap_delay_ms(10_000, 3, 2), 15_000);
    assert_eq!(super::super::hls_soft_overlap_delay_ms(10_000, 4, 2), 10_000);
}

#[tokio::test]
async fn hls_account_overlap_reclaim_preempts_speculative_session() {
    let app_state = test_app_state();
    let input = ConfigInput { id: 1, name: Arc::from("overlap-input"), ..ConfigInput::default() };
    let winner = create_bound_hls_test_session(&app_state, &input, "winner", "account-a", 1_000).await;
    let loser = create_bound_hls_test_session(&app_state, &input, "loser", "account-a", 1_000).await;
    let winner_proxy_session_id = winner.read().await.proxy_session_id.clone();
    let loser_proxy_session_id = loser.read().await.proxy_session_id.clone();
    {
        let mut loser = loser.write().await;
        loser.origin_account_binding = Some(HlsOriginAccountBinding::speculative_from(
            Arc::clone(&input.name),
            Arc::from("account-a"),
            &loser_proxy_session_id,
            winner_proxy_session_id.clone(),
            20_000,
            2_000,
        ));
    }
    let loser_generation = loser.read().await.activity.origin_work_generation;

    super::super::reclaim_hls_account_overlap_if_needed(&app_state, &winner, 10_000).await;

    assert!(app_state.hls.proxy.is_account_overlap_cooling_down(&input.name, &Arc::from("account-a"), 10_000).await);
    assert!(!app_state.hls.proxy.is_account_overlap_cooling_down(&input.name, &Arc::from("account-a"), 25_000).await);
    let loser_binding_mode = loser.read().await.origin_account_binding.as_ref().unwrap().binding_mode.clone();
    assert!(matches!(
        loser_binding_mode,
        HlsOriginAccountBindingMode::Detached { reason: HlsOriginAccountDetachedReason::ReclaimedByOriginalOwner, .. }
    ));
    assert_eq!(loser.read().await.activity.origin_work_generation, loser_generation + 1);
    let winner_binding_mode = winner.read().await.origin_account_binding.as_ref().unwrap().binding_mode.clone();
    assert!(matches!(winner_binding_mode, HlsOriginAccountBindingMode::Active));
}

#[tokio::test]
async fn hls_account_overlap_promotes_speculative_session_after_soft_window() {
    let app_state = test_app_state();
    let input = ConfigInput { id: 1, name: Arc::from("overlap-input"), ..ConfigInput::default() };
    let displaced = create_bound_hls_test_session(&app_state, &input, "displaced", "account-a", 1_000).await;
    let promoted = create_bound_hls_test_session(&app_state, &input, "promoted", "account-a", 1_000).await;
    let displaced_proxy_session_id = displaced.read().await.proxy_session_id.clone();
    let promoted_proxy_session_id = promoted.read().await.proxy_session_id.clone();
    {
        let mut promoted = promoted.write().await;
        promoted.origin_account_binding = Some(HlsOriginAccountBinding::speculative_from(
            Arc::clone(&input.name),
            Arc::from("account-a"),
            &promoted_proxy_session_id,
            displaced_proxy_session_id,
            20_000,
            2_000,
        ));
    }
    let displaced_generation = displaced.read().await.activity.origin_work_generation;

    super::super::promote_elapsed_hls_account_overlaps(&app_state, 20_001).await;

    let displaced_binding_mode = displaced.read().await.origin_account_binding.as_ref().unwrap().binding_mode.clone();
    assert!(matches!(
        displaced_binding_mode,
        HlsOriginAccountBindingMode::Detached { reason: HlsOriginAccountDetachedReason::SoftWindowElapsed, .. }
    ));
    assert_eq!(displaced.read().await.activity.origin_work_generation, displaced_generation + 1);
    let promoted_binding_mode = promoted.read().await.origin_account_binding.as_ref().unwrap().binding_mode.clone();
    assert!(matches!(promoted_binding_mode, HlsOriginAccountBindingMode::Active));
}

#[tokio::test]
async fn hls_origin_runtime_uses_soft_overlap_before_grace_for_interactive_work() {
    let input = single_hls_provider_input("soft-overlap-input");
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let old_session = create_bound_hls_test_session(&app_state, &input, "old", input.name.as_ref(), 1_000).await;
    let old_proxy_session_id = old_session.read().await.proxy_session_id.clone();
    {
        let mut session = old_session.write().await;
        session.target_duration = Some(10);
        session.mark_authorized_media_access(1_000);
        session.activity.last_delivered_media_at_ms = Some(1_000);
    }
    let old_binding = old_session.read().await.origin_account_binding.clone().expect("binding exists");
    app_state.active_provider.refresh_provider_reservation(&old_binding.account_name, &old_binding.session_owner, 60);
    app_state.active_provider.confirm_playback_activity(&old_binding.session_owner);
    // The binding acquired a reservation without a handle; capture its tag so the
    // preempt path can prove it still targets this exact incarnation.
    let binding_tag = app_state.active_provider.binding_tag_for_owner(&old_binding.session_owner);
    old_session.write().await.origin_account_binding.as_mut().expect("binding exists").provider_binding_tag =
        binding_tag;

    let new_session = create_unbound_hls_test_session(&app_state, &input, "new", 12_000).await;
    let new_proxy_session_id = new_session.read().await.proxy_session_id.clone();
    let prepared_origin = super::super::prepare_hls_origin_runtime(
        &app_state,
        &new_session,
        &input,
        "http://account.example.com/live/account-user/account-pass/new.m3u8",
        "http://account.example.com/live/account-user/account-pass/new.m3u8",
        &new_proxy_session_id,
        &test_fingerprint_with_addr(test_addr_with_port(55201)),
        ConnectionKind::Normal,
        0,
        super::super::HlsOriginWorkKind::Manifest,
        super::super::HlsOriginWorkClass::ManifestInteractive,
        21_000,
    )
    .await
    .expect("interactive work should use soft-active overlap before grace");

    let binding =
        prepared_origin.origin_account_binding_to_store.as_ref().expect("speculative binding should be prepared");
    assert_eq!(binding.account_name, input.name);
    assert!(matches!(
        &binding.binding_mode,
        HlsOriginAccountBindingMode::Speculative {
            displaced_proxy_session_id,
            ..
        } if displaced_proxy_session_id == &old_proxy_session_id
    ));
    assert!(matches!(
        prepared_origin.preacquired_origin_account_handle.as_ref().map(|handle| &handle.allocation),
        Some(super::super::ProviderAllocation::Available(_))
    ));

    app_state.connection_manager.release_provider_handle(prepared_origin.preacquired_origin_account_handle);
}

#[tokio::test]
async fn hls_origin_runtime_normal_policy_preempts_active_soft_hls_binding() {
    let input = single_hls_provider_input("policy-preempt-input");
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let soft_session = create_bound_hls_test_session(&app_state, &input, "soft", input.name.as_ref(), 1_000).await;
    let soft_generation = {
        let mut session = soft_session.write().await;
        session.target_duration = Some(10);
        session.mark_authorized_media_access(10_000);
        session.activity.last_delivered_media_at_ms = Some(10_000);
        session.reconcile_effective_origin_acquire_policy(
            Some(HlsEffectiveOriginAcquirePolicy::new(ConnectionKind::Soft, 0, 10_000)),
            10_000,
        );
        session.activity.origin_work_generation
    };
    let soft_binding = soft_session.read().await.origin_account_binding.clone().expect("soft binding exists");
    app_state.active_provider.refresh_provider_reservation(&soft_binding.account_name, &soft_binding.session_owner, 60);
    app_state.active_provider.confirm_playback_activity(&soft_binding.session_owner);
    // The binding acquired a reservation without a handle; capture its tag so the
    // preempt path can prove it still targets this exact incarnation.
    let binding_tag = app_state.active_provider.binding_tag_for_owner(&soft_binding.session_owner);
    soft_session.write().await.origin_account_binding.as_mut().expect("binding exists").provider_binding_tag =
        binding_tag;

    let normal_session = create_unbound_hls_test_session(&app_state, &input, "normal", 10_500).await;
    let normal_proxy_session_id = normal_session.read().await.proxy_session_id.clone();
    let prepared_origin = super::super::prepare_hls_origin_runtime(
        &app_state,
        &normal_session,
        &input,
        "http://account.example.com/live/account-user/account-pass/normal.m3u8",
        "http://account.example.com/live/account-user/account-pass/normal.m3u8",
        &normal_proxy_session_id,
        &test_fingerprint_with_addr(test_addr_with_port(55231)),
        ConnectionKind::Normal,
        0,
        super::super::HlsOriginWorkKind::Manifest,
        super::super::HlsOriginWorkClass::ManifestInteractive,
        10_500,
    )
    .await
    .expect("normal HLS policy should preempt active soft HLS binding");

    let new_binding = prepared_origin
        .origin_account_binding_to_store
        .as_ref()
        .expect("preempting session should receive active binding");
    assert_eq!(new_binding.account_name, input.name);
    assert!(matches!(new_binding.binding_mode, HlsOriginAccountBindingMode::Active));
    assert!(matches!(
        prepared_origin.preacquired_origin_account_handle.as_ref().map(|handle| &handle.allocation),
        Some(super::super::ProviderAllocation::Available(_))
    ));
    let soft_session = soft_session.read().await;
    assert_eq!(soft_session.activity.origin_work_generation, soft_generation + 1);
    assert!(matches!(
        soft_session.origin_account_binding.as_ref().map(|binding| &binding.binding_mode),
        Some(HlsOriginAccountBindingMode::Detached {
            reason: HlsOriginAccountDetachedReason::PreemptedByHigherPriority,
            ..
        })
    ));
    drop(soft_session);

    app_state.connection_manager.release_provider_handle(prepared_origin.preacquired_origin_account_handle);
}

#[tokio::test]
async fn preempted_origin_runtime_commits_low_priority_tail_without_redirect_or_origin_fetch() {
    let fixture = runtime_policy_endpoint_fixture(true).await;
    let input = single_hls_provider_input("runtime-preempted-input");
    let session = fixture
        .app_state
        .hls
        .proxy
        .sessions()
        .get_by_proxy_session_id(&fixture.proxy_session_id)
        .await
        .expect("runtime policy session");
    let now_ms = super::super::current_time_millis();
    let mut binding = HlsOriginAccountBinding::new(
        Arc::clone(&input.name),
        Arc::from("preempted-account"),
        &fixture.proxy_session_id,
        now_ms,
    );
    binding.detach(HlsOriginAccountDetachedReason::PreemptedByHigherPriority, now_ms);
    session.write().await.replace_origin_account_binding(Some(binding));
    let origin_refresh_before = session.read().await.origin_refresh.clone();
    let origin = super::super::HlsCacheManifestOrigin {
        raw_request_url: "http://account.example.com/live/account-user/account-pass/12345.m3u8",
        session_entry_url: super::super::HlsOriginEntryUrl::direct_http(
            "http://account.example.com/live/account-user/account-pass/12345.m3u8",
        ),
        input: &input,
        origin_source: super::super::build_hls_origin_source(&input, "12345"),
    };
    let context = test_hls_access_context(fixture.proxy_session_id.clone(), fixture.lease_id.clone());

    let result = super::super::prepare_hls_canonical_manifest_origin_runtime(
        &fixture.app_state,
        &session,
        &context,
        &origin,
        &fixture.proxy_session_id,
        &fixture.lease_id,
        HlsAccessLeaseState::Activated,
        &test_fingerprint(),
        None,
        now_ms,
    )
    .await;
    let Err(response) = result else {
        panic!("detached origin binding must resolve to the lease-bound policy tail");
    };

    assert!(!response.headers().contains_key(header::LOCATION));
    let plan = wait_for_runtime_policy_terminal_plan(&fixture).await;
    assert_eq!(plan.reason, HlsRuntimeCustomTailReason::LowPriorityPreempted);
    assert_eq!(plan.segment_duration_ms, 10_027);
    assert_eq!(session.read().await.origin_refresh, origin_refresh_before);
    let replay = get_response(Arc::clone(&fixture.app_state), &fixture.manifest_uri, None).await;
    assert_eq!(replay.status(), StatusCode::OK);
    assert!(!replay.headers().contains_key(header::LOCATION));
    let body = String::from_utf8(response_body(replay).await.to_vec()).expect("preemption manifest utf8");
    assert!(body.contains("/terminal/"));
    assert!(!body.contains("/cvs/hls/"));
    assert!(body.ends_with("#EXT-X-ENDLIST\n"));
}

#[tokio::test]
async fn hls_origin_policy_preemption_rejects_soft_request_against_active_normal_binding() {
    let input = single_hls_provider_input("policy-no-preempt-input");
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let normal_session = create_bound_hls_test_session(&app_state, &input, "normal", input.name.as_ref(), 1_000).await;
    {
        let mut session = normal_session.write().await;
        session.target_duration = Some(10);
        session.mark_authorized_media_access(10_000);
        session.reconcile_effective_origin_acquire_policy(
            Some(HlsEffectiveOriginAcquirePolicy::new(ConnectionKind::Normal, 0, 10_000)),
            10_000,
        );
    }
    let normal_binding = normal_session.read().await.origin_account_binding.clone().expect("normal binding exists");
    app_state.active_provider.refresh_provider_reservation(
        &normal_binding.account_name,
        &normal_binding.session_owner,
        60,
    );
    app_state.active_provider.confirm_playback_activity(&normal_binding.session_owner);
    let soft_session = create_unbound_hls_test_session(&app_state, &input, "soft", 10_500).await;
    let soft_proxy_session_id = soft_session.read().await.proxy_session_id.clone();

    let result = super::super::prepare_hls_origin_policy_preempt_runtime(
        &app_state,
        &soft_session,
        &input,
        "http://account.example.com/live/account-user/account-pass/soft.m3u8",
        "http://account.example.com/live/account-user/account-pass/soft.m3u8",
        &soft_proxy_session_id,
        &test_fingerprint_with_addr(test_addr_with_port(55232)),
        ConnectionKind::Soft,
        -100,
        crate::model::PlaybackKind::LiveHls,
        10_500,
    )
    .await;

    assert!(result.is_err());
    assert!(matches!(
        normal_session.read().await.origin_account_binding.as_ref().map(|binding| &binding.binding_mode),
        Some(HlsOriginAccountBindingMode::Active)
    ));
    assert!(app_state
        .active_provider
        .is_provider_reserved_for_other_session(&normal_binding.account_name, Some("unrelated-session")));
}

#[tokio::test]
async fn hls_origin_runtime_uses_grace_as_interactive_fallback_after_overlap_fails() {
    let input = single_hls_provider_input("grace-input");
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let occupied = app_state
        .active_provider
        .acquire_connection_with_grace_for_session(
            &input.name,
            &test_addr_with_port(55211),
            false,
            0,
            ConnectionKind::Normal,
            Some("external-owner"),
        )
        .expect("test should occupy the only provider account");
    let session = create_unbound_hls_test_session(&app_state, &input, "12345", 2_000).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();

    let prepared_origin = super::super::prepare_hls_origin_runtime(
        &app_state,
        &session,
        &input,
        "http://account.example.com/live/account-user/account-pass/12345.m3u8",
        "http://account.example.com/live/account-user/account-pass/12345.m3u8",
        &proxy_session_id,
        &test_fingerprint_with_addr(test_addr_with_port(55212)),
        ConnectionKind::Normal,
        0,
        super::super::HlsOriginWorkKind::Manifest,
        super::super::HlsOriginWorkClass::ManifestInteractive,
        2_000,
    )
    .await
    .expect("interactive work can use grace when normal acquire and overlap fail");

    assert!(matches!(
        prepared_origin.preacquired_origin_account_handle.as_ref().map(|handle| &handle.allocation),
        Some(super::super::ProviderAllocation::GracePeriod(_))
    ));
    assert_eq!(
        prepared_origin
            .origin_account_binding_to_store
            .as_ref()
            .expect("grace binding should still bind the selected account")
            .account_name
            .as_ref(),
        input.name.as_ref()
    );

    app_state.connection_manager.release_provider_handle(prepared_origin.preacquired_origin_account_handle);
    app_state.connection_manager.release_provider_handle(Some(occupied));
}

#[tokio::test]
async fn hls_provider_exhausted_grace_hold_waits_for_grace_period_before_retry() {
    let input = single_hls_provider_input("provider-grace-hold-input");
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    app_state.app_config.config.store(Arc::new(Config {
        reverse_proxy: Some(ReverseProxyConfig::from(&ReverseProxyConfigDto {
            stream: Some(StreamConfigDto {
                grace_period_millis: 20,
                grace_period_timeout_secs: 10,
                grace_period_hold_stream: true,
                ..Default::default()
            }),
            ..Default::default()
        })),
        ..Default::default()
    }));
    let session = create_unbound_hls_test_session(&app_state, &input, "provider-grace-session", 1_000).await;
    let access_lease_id = HlsAccessLeaseId("provider-grace-lease".to_string());
    let strip = app_state.hls.proxy.strip();
    let resolution = super::super::hls_provider_connections_exhausted_manifest_resolution(
        &app_state,
        &session,
        "hls-user",
        &input,
        59,
        &access_lease_id,
        HlsAccessLeaseState::Activated,
        &strip,
        None,
        true,
    );

    let resolution =
        tokio::time::timeout(Duration::from_millis(500), resolution).await.expect("grace hold deadline should wake");

    assert!(matches!(resolution, super::super::HlsProviderExhaustedResolution::RetryAcquire));
}

#[tokio::test]
async fn provider_grace_expiry_commits_lease_bound_provider_exhausted_tail() {
    let fixture = runtime_policy_endpoint_fixture(true).await;
    let input = single_hls_provider_input("runtime-provider-grace-input");
    store_test_sources_with_target(
        &fixture.app_state,
        input.clone(),
        ConfigTarget::from(&ConfigTargetDto { id: 1, name: "default".to_string(), ..Default::default() }),
    );
    let current = fixture.app_state.app_config.config.load();
    fixture.app_state.app_config.config.store(Arc::new(Config {
        reverse_proxy: Some(ReverseProxyConfig::from(&ReverseProxyConfigDto {
            stream: Some(StreamConfigDto {
                grace_period_millis: 20,
                grace_period_timeout_secs: 10,
                grace_period_hold_stream: true,
                ..Default::default()
            }),
            ..Default::default()
        })),
        ..current.as_ref().clone()
    }));
    let strip = fixture.app_state.hls.proxy.strip();
    let session = fixture
        .app_state
        .hls
        .proxy
        .sessions()
        .get_by_proxy_session_id(&fixture.proxy_session_id)
        .await
        .expect("runtime provider session");

    let grace = super::super::hls_provider_connections_exhausted_manifest_resolution(
        &fixture.app_state,
        &session,
        "hls-user",
        &input,
        12345,
        &fixture.lease_id,
        HlsAccessLeaseState::Activated,
        &strip,
        None,
        true,
    )
    .await;
    assert!(matches!(grace, super::super::HlsProviderExhaustedResolution::RetryAcquire));

    let response = super::super::hls_provider_connections_exhausted_manifest_resolution(
        &fixture.app_state,
        &session,
        "hls-user",
        &input,
        12345,
        &fixture.lease_id,
        HlsAccessLeaseState::Activated,
        &strip,
        None,
        false,
    )
    .await;
    let super::super::HlsProviderExhaustedResolution::Response(response) = response else {
        panic!("expired grace must resolve to a finite response");
    };
    assert!(!response.headers().contains_key(header::LOCATION));

    let plan = wait_for_runtime_policy_terminal_plan(&fixture).await;
    assert_eq!(plan.reason, HlsRuntimeCustomTailReason::ProviderConnectionsExhausted);
    assert_eq!(plan.segment_duration_ms, 10_027);
    let replay = super::super::hls_provider_connections_exhausted_manifest_resolution(
        &fixture.app_state,
        &session,
        "hls-user",
        &input,
        12345,
        &fixture.lease_id,
        HlsAccessLeaseState::Denied,
        &strip,
        None,
        false,
    )
    .await;
    let super::super::HlsProviderExhaustedResolution::Response(replay) = replay else {
        panic!("committed provider tail must replay");
    };
    assert_eq!(replay.status(), StatusCode::OK);
    assert!(!replay.headers().contains_key(header::LOCATION));
    let body = String::from_utf8(response_body(replay).await.to_vec()).expect("provider terminal manifest utf8");
    assert!(body.contains("/terminal/"));
    assert!(!body.contains("/cvs/hls/"));
    assert!(body.ends_with("#EXT-X-ENDLIST\n"));
}

#[tokio::test]
async fn hls_origin_runtime_background_skips_soft_overlap_and_grace() {
    let input = single_hls_provider_input("background-input");
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let old_session = create_bound_hls_test_session(&app_state, &input, "old", input.name.as_ref(), 1_000).await;
    {
        let mut session = old_session.write().await;
        session.target_duration = Some(10);
        session.mark_authorized_media_access(1_000);
        session.activity.last_delivered_media_at_ms = Some(1_000);
    }
    let old_binding = old_session.read().await.origin_account_binding.clone().expect("binding exists");
    app_state.active_provider.refresh_provider_reservation(&old_binding.account_name, &old_binding.session_owner, 60);
    app_state.active_provider.confirm_playback_activity(&old_binding.session_owner);
    let new_session = create_unbound_hls_test_session(&app_state, &input, "new", 12_000).await;
    let new_proxy_session_id = new_session.read().await.proxy_session_id.clone();

    let result = super::super::prepare_hls_origin_runtime(
        &app_state,
        &new_session,
        &input,
        "http://account.example.com/live/account-user/account-pass/new.m3u8",
        "http://account.example.com/live/account-user/account-pass/new.m3u8",
        &new_proxy_session_id,
        &test_fingerprint_with_addr(test_addr_with_port(55221)),
        ConnectionKind::Normal,
        0,
        super::super::HlsOriginWorkKind::Segment,
        super::super::HlsOriginWorkClass::Background,
        12_000,
    )
    .await;

    assert_eq!(
        result.err(),
        Some(super::super::HlsOriginRuntimeAcquireError::NoAccountAvailable {
            reason: super::super::HlsOriginRuntimeNoAccountReason::ProviderConnectionsExhausted
        })
    );
    let old_session = old_session.read().await;
    assert!(matches!(
        old_session.origin_account_binding.as_ref().expect("old binding remains").binding_mode,
        HlsOriginAccountBindingMode::Active
    ));
    assert!(new_session.read().await.origin_account_binding.is_none());
}

#[test]
fn hls_detached_origin_binding_reclaimed_by_owner_maps_to_preempted_no_account_reason() {
    let proxy_session_id = ProxySessionId("preempted-session".to_string());
    let mut binding =
        HlsOriginAccountBinding::new(Arc::from("input-a"), Arc::from("account-a"), &proxy_session_id, 1_000);
    binding.detach(HlsOriginAccountDetachedReason::ReclaimedByOriginalOwner, 2_000);

    assert_eq!(
        super::super::hls_no_account_reason_for_binding(Some(&binding)),
        super::super::HlsOriginRuntimeNoAccountReason::OriginBindingPreempted
    );
}

#[test]
fn hls_detached_origin_binding_soft_window_elapsed_maps_to_exhausted_no_account_reason() {
    let proxy_session_id = ProxySessionId("soft-window-session".to_string());
    let mut binding =
        HlsOriginAccountBinding::new(Arc::from("input-a"), Arc::from("account-a"), &proxy_session_id, 1_000);
    binding.detach(HlsOriginAccountDetachedReason::SoftWindowElapsed, 2_000);

    assert_eq!(
        super::super::hls_no_account_reason_for_binding(Some(&binding)),
        super::super::HlsOriginRuntimeNoAccountReason::ProviderConnectionsExhausted
    );
}
