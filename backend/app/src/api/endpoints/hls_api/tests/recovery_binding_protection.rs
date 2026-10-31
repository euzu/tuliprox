//! A consumed live window is not evidence of voluntary client inactivity.
use super::{
    create_active_hls_user_session, get_response,
    manifest_entry_freshness::{enable_hls_cache_with_startup_mode, open_manifest_for_new_lease},
    response_body, single_hls_provider_input, spawn_test_binary_origin, test_app_state_with_inputs, test_fingerprint,
    TestBinaryOriginResponse,
};
use axum::http::StatusCode;
use std::{fmt::Write as _, sync::Arc, time::Duration};
use tuliprox_hls::api::{HlsAccessLeaseId, HlsOriginPathCondition};

struct Playback {
    app: Arc<crate::api::model::AppState>,
    input: crate::model::ConfigInput,
    session: tuliprox_hls::api::HlsSessionHandle,
    proxy: tuliprox_hls::api::ProxySessionId,
    id: HlsAccessLeaseId,
    url: String,
    base: u64,
    _origin: super::TestSegmentOrigin,
}

async fn established() -> Playback {
    let origin = spawn_test_binary_origin(Arc::new(|path| {
        let body = if path.ends_with("m3u8") {
            let mut manifest = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:100\n");
            for index in 0..6 {
                writeln!(manifest, "#EXTINF:2,\n{index}.ts").unwrap();
            }
            Arc::from(manifest.into_bytes())
        } else {
            Arc::from(&[0x47; 8192][..])
        };
        TestBinaryOriginResponse::new(StatusCode::OK, body)
    }))
    .await;
    let mut input = single_hls_provider_input("recovery-provider");
    input.url.clone_from(&origin.base_url);
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    enable_hls_cache_with_startup_mode(&app, shared::model::HlsStartupMode::Conservative).await;
    let mut config = app.app_config.config.load().as_ref().clone();
    config.reverse_proxy.as_mut().unwrap().hls_cache.as_mut().unwrap().strip.value = 0;
    app.app_config.config.store(Arc::new(config));
    app.hls.proxy.update_config(&app.app_config).await;
    create_active_hls_user_session(&app).await;
    let url = format!("{}/live/account-user/account-pass/12345.m3u8", origin.base_url);
    let manifest = open_manifest_for_new_lease(&app, &input, &url, "recovering-viewer").await;
    for uri in manifest.lines().filter(|line| !line.is_empty() && !line.starts_with('#')) {
        let response = get_response(Arc::clone(&app), uri, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response_body(response).await.as_ref(), &[0x47; 8192]);
    }
    let key = super::super::build_hls_origin_source(&input, "12345").session_key();
    let session = app.hls.proxy.sessions().get_by_key(&key).await.unwrap();
    let proxy = session.read().await.proxy_session_id.clone();
    let id = HlsAccessLeaseId("recovering-viewer".into());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let lease =
                app.hls.proxy.access_lease_response_snapshot(&id, &proxy, app.hls.proxy.now_ms()).await.unwrap();
            let state = session.read().await;
            if lease.playback_cursor.highest_contiguous_completed_proxy_seq == Some(5)
                && state.activity.active_origin_work_count.load(std::sync::atomic::Ordering::Acquire) == 0
                && state.active_segment_fetches == 0
                && !state.origin_refresh.in_flight
            {
                break;
            }
            drop(state);
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the offered window has been delivered and all jobs have finished");
    let base = app.hls.proxy.now_ms();
    app.hls.proxy.follow_tokio_clock_for_test(base);
    tokio::time::pause();
    Playback { app, input, session, proxy, id, url, base, _origin: origin }
}

impl Playback {
    fn now(&self) -> u64 { self.app.hls.proxy.now_ms() }

    async fn advance_to(&self, offset_ms: u64) {
        tokio::time::advance(Duration::from_millis((self.base + offset_ms).saturating_sub(self.now()))).await;
    }

    async fn sync(&self) -> bool {
        self.app.hls.proxy.sync_recovery_binding_protection(&self.app.active_provider, &self.session, self.now()).await
    }

    async fn fail(&self) {
        self.advance_to(1_000).await;
        let mut state = self.session.write().await;
        state.origin_control.path_condition = HlsOriginPathCondition::RetryableFetchFailure;
        state.origin_refresh.mark_failure(self.now());
        drop(state);
        assert!(self.sync().await, "a proven shortage must pause inactivity before its normal window expires");
    }

    async fn lease(&self) -> tuliprox_hls::api::HlsAccessLease {
        self.app.hls.proxy.access_lease_response_snapshot(&self.id, &self.proxy, self.now()).await.unwrap()
    }

    async fn publish_next(&self) {
        let mut state = self.session.write().await;
        let mut next = super::test_segment_entry(
            &self.proxy,
            6,
            tuliprox_hls::api::SegmentCacheStatus::Ready { content_length: 8192, ready_at_ms: self.now() },
        );
        next.duration_ms = 2_000;
        next.media_start_ms = 12_000;
        state.segments.insert(6, next);
        state.publishable_origin_tail_proxy_seq = Some(6);
        state.render_and_store_manifest(self.now()).unwrap();
    }

    async fn apply_publication_recovery(&self, expected: tuliprox_hls::HlsManifestAcceptanceTrigger) {
        use tuliprox_hls::{
            HlsAcceptanceEpisodeTiming, HlsAcceptanceEpisodeTimingInput, HlsOperationTimeoutMs, HlsRecoveryEtaMs,
            HlsRecoveryTimingPolicy,
        };
        // Repeated successful samples have not advanced this consumed window.
        self.session.write().await.origin_control.last_media_progress_at_ms = Some(0);
        let directive = match tuliprox_hls::api::hls_manifest_acceptance_directive_for_session(
            &self.app.hls_ctx(),
            &self.session,
            &self.proxy,
        )
        .await
        {
            tuliprox_hls::api::HlsManifestAcceptanceEvaluationOutcome::Evaluated(directive) => directive,
            other => panic!("publication reserve pressure must evaluate: {other:?}"),
        };
        assert_eq!(directive.trigger, expected);
        if !directive.trigger.starts_episode() {
            return;
        }
        let seed = directive.timing_seed.unwrap();
        let burst_plan = self.app.hls.proxy.manifest_recovery_burst().level.plan();
        let timeout = HlsOperationTimeoutMs::from_millis(self.app.hls.proxy.origin_manifest_timeout_ms());
        let eta = HlsRecoveryEtaMs::from_millis(self.app.hls.proxy.origin_manifest_timeout_ms());
        let mut state = self.session.write().await;
        let timing = HlsAcceptanceEpisodeTiming::from_input(&HlsAcceptanceEpisodeTimingInput {
            started_at_ms: self.now(),
            burst_plan,
            target_duration_ms: seed.target_duration_ms,
            transition_margin: seed.transition_margin,
            workload: seed.workload,
            observed_latency: state.origin_control.recovery_samples.latency_snapshot(),
            required_terminal_media_key: seed.required_terminal_media_key,
            terminal_media_preparation: seed.terminal_media_preparation,
            policy: HlsRecoveryTimingPolicy::new(timeout, timeout, eta, eta),
        });
        assert_eq!(state.origin_control.path_condition, HlsOriginPathCondition::PublicationLate);
        state.origin_control.begin_acceptance_episode(self.now(), burst_plan, directive.trigger, &timing);
    }
}

#[tokio::test]
async fn stagnating_successful_manifests_preserve_a_qualified_recovery_between_jobs() {
    let playback = established().await;
    let binding = playback.session.read().await.origin_account_binding.clone().unwrap();
    let actual_media = playback.session.read().await.activity.last_authorized_media_at_ms;
    let mut lease = playback.lease().await;
    lease.playback_cursor.first_segment_started_at_ms = Some(playback.base - 8_000);
    playback.app.hls.proxy.prepare_access_lease(lease).await;
    playback.apply_publication_recovery(tuliprox_hls::HlsManifestAcceptanceTrigger::RecoveryRequired).await;
    assert!(playback.sync().await, "existing reserve-pressure recovery must protect a consumed HTTP-200 window");
    playback.advance_to(3_000).await;
    assert!(playback.sync().await, "qualified recovery must protect the gap after normal HardActive without I/O");
    assert!(super::super::find_hls_account_overlap_candidate(
        &playback.app,
        &playback.input,
        &tuliprox_hls::api::ProxySessionId("publication-competitor".into()),
        playback.now(),
    )
    .await
    .is_none());
    super::super::detach_unprotected_hls_origin_account_bindings(&playback.app, playback.now()).await;
    let state = playback.session.read().await;
    assert!(state.origin_account_binding.as_ref().unwrap().is_active());
    assert_eq!(state.origin_account_binding.as_ref().unwrap().generation, binding.generation);
    assert_eq!(state.activity.last_authorized_media_at_ms, actual_media);
    assert_eq!(state.activity.active_origin_work_count.load(std::sync::atomic::Ordering::Acquire), 0);
    assert_eq!(playback.app.active_provider.provider_lease_usage(&playback.input.name).total(), 1);
}

#[tokio::test]
async fn critical_publication_recovery_protects_the_consumed_window() {
    let playback = established().await;
    let mut lease = playback.lease().await;
    lease.playback_cursor.first_segment_started_at_ms = Some(playback.base - 11_000);
    playback.app.hls.proxy.prepare_access_lease(lease).await;
    playback.apply_publication_recovery(tuliprox_hls::HlsManifestAcceptanceTrigger::Critical).await;
    assert!(playback.sync().await, "Critical acceptance evidence must qualify publication starvation");
}

#[tokio::test]
async fn established_recovery_survives_media_expiry_between_refresh_jobs() {
    let Playback { app, input, session, proxy, url, base, _origin, .. } = established().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    {
        let mut state = session.write().await;
        state.origin_control.path_condition = HlsOriginPathCondition::RetryableFetchFailure;
        state.origin_refresh.mark_failure(base + 1_000);
        state.origin_refresh.consecutive_failures = 5;
    }
    let binding = session.read().await.origin_account_binding.clone().unwrap();
    let established = session.read().await.established_manifest_recovery_binding().unwrap();
    super::super::detach_unprotected_hls_origin_account_bindings(&app, base + 1_000).await;
    tokio::time::advance(Duration::from_secs(7)).await;
    super::super::detach_unprotected_hls_origin_account_bindings(&app, base + 8_000).await;
    let prepared = super::super::prepare_hls_origin_runtime(
        &app,
        &session,
        &input,
        &url,
        &url,
        &proxy,
        &test_fingerprint(),
        tuliprox_session::ConnectionKind::Normal,
        0,
        super::super::HlsOriginWorkKind::Manifest,
        super::super::HlsOriginWorkClass::ManifestInteractive,
        base + 8_000,
    )
    .await
    .unwrap();
    let state = session.read().await;
    assert_eq!(
        state.origin_refresh.consecutive_failures, 5,
        "a server-side shortage must not restart the recovery history"
    );
    assert_eq!(state.origin_account_binding.as_ref().unwrap().generation, binding.generation);
    assert_eq!(state.established_manifest_recovery_binding(), Some(established));
    assert_eq!(app.active_provider.binding_tag_for_owner(&binding.session_owner), binding.provider_binding_tag);
    drop(state);
    app.connection_manager.release_managed_provider_handle(prepared.preacquired_origin_account_handle);
}

#[tokio::test]
async fn recovery_resumes_only_the_remaining_inactivity_window() {
    use tuliprox_hls::api::HlsAccountBindingProtection::{Expired, HardActive, SoftActive};
    let playback = established().await;
    let actual_media = playback.session.read().await.activity.last_authorized_media_at_ms;
    playback.fail().await;
    playback.advance_to(8_000).await;
    assert!(playback.sync().await);
    assert!(matches!(playback.session.read().await.account_binding_protection(playback.now()), HardActive { .. }));
    playback.publish_next().await;
    assert!(!playback.sync().await);
    let expiry = playback.session.read().await.effective_account_binding_activity(playback.now()).unwrap().0 + 6_000;
    assert!(expiry < playback.now() + 6_000, "recovery must not start a full new window");
    assert_eq!(playback.session.read().await.activity.last_authorized_media_at_ms, actual_media);
    playback.advance_to(10_000).await;
    assert!(matches!(playback.session.read().await.account_binding_protection(playback.now()), SoftActive { .. }));
    playback.advance_to(expiry - playback.base + 1).await;
    assert_eq!(playback.session.read().await.account_binding_protection(playback.now()), Expired);
    playback.advance_to(expiry - playback.base + 1_100).await;
    playback.sync().await;
    assert_eq!(playback.app.active_provider.provider_lease_usage(&playback.input.name).total(), 0);
}

#[tokio::test]
async fn manifest_samples_and_retry_jobs_cannot_roll_the_recovery_deadline() {
    let playback = established().await;
    playback.fail().await;
    let actual_media = playback.session.read().await.activity.last_authorized_media_at_ms;
    let access_expiry = playback.lease().await.valid_until_ms;
    for offset in [2_000, 4_000, 8_000, 10_000] {
        playback.advance_to(offset).await;
        let mut state = playback.session.write().await;
        // A successful unchanged sample is not new client-reachable media.
        state.origin_control.path_condition = HlsOriginPathCondition::ProgressExpected;
        state.origin_refresh.mark_failure(playback.now());
        drop(state);
        assert!(playback.sync().await);
        assert_eq!(playback.lease().await.valid_until_ms, access_expiry);
        assert_eq!(playback.session.read().await.activity.last_authorized_media_at_ms, actual_media);
    }
    playback.advance_to(12_100).await;
    assert!(!playback.sync().await, "the existing media-reserve deadline cannot move with polls or retries");
}

#[tokio::test]
async fn healthy_publication_wait_and_unused_new_media_keep_normal_expiry() {
    for publish_new in [false, true] {
        let playback = established().await;
        if publish_new {
            playback.publish_next().await;
        } else {
            playback.apply_publication_recovery(tuliprox_hls::HlsManifestAcceptanceTrigger::None).await;
        }
        let actual_media = playback.session.read().await.activity.last_authorized_media_at_ms;
        for offset in [1_000, 3_000, 8_000] {
            playback.advance_to(offset).await;
            playback.session.write().await.mark_authorized_manifest_access(playback.now());
            assert!(!playback.sync().await, "playlist activity cannot hold an inactive binding");
        }
        assert_eq!(playback.session.read().await.activity.last_authorized_media_at_ms, actual_media);
        super::super::detach_unprotected_hls_origin_account_bindings(&playback.app, playback.now()).await;
        assert!(playback.session.read().await.origin_account_binding.as_ref().unwrap().is_detached());
        // This test creates two isolated players in one runtime.
        tokio::time::resume();
    }
}

#[tokio::test]
async fn recovery_blocks_soft_overlap_but_keeps_higher_priority_preemption() {
    let playback = established().await;
    playback.advance_to(3_000).await;
    playback.session.write().await.origin_control.path_condition = HlsOriginPathCondition::RetryableFetchFailure;
    assert!(playback.sync().await, "a blockade may start during SoftActive");
    playback.advance_to(8_000).await;
    assert!(playback.sync().await);
    let newcomer = tuliprox_hls::api::ProxySessionId("competitor".into());
    assert!(super::super::find_hls_account_overlap_candidate(
        &playback.app,
        &playback.input,
        &newcomer,
        playback.now()
    )
    .await
    .is_none());
    assert!(super::super::find_hls_origin_policy_preempt_candidate(
        &playback.app,
        &playback.input,
        &newcomer,
        tuliprox_hls::api::HlsEffectiveOriginAcquirePolicy::new(
            tuliprox_session::ConnectionKind::Normal,
            -1,
            playback.now()
        ),
        playback.now()
    )
    .await
    .is_some());
    assert_eq!(
        playback.session.read().await.activity.active_origin_work_count.load(std::sync::atomic::Ordering::Acquire),
        0,
        "recovery reservation must not impersonate in-flight origin work"
    );
    assert_eq!(playback.app.active_provider.provider_lease_usage(&playback.input.name).total(), 1);
    let old_binding = playback.session.read().await.origin_account_binding.clone().unwrap();
    let winner =
        super::create_unbound_hls_test_session(&playback.app, &playback.input, "priority", playback.now()).await;
    let winner_proxy = winner.read().await.proxy_session_id.clone();
    let url = playback.url.replace("12345.m3u8", "priority.m3u8");
    let prepared = super::super::prepare_hls_origin_runtime(
        &playback.app,
        &winner,
        &playback.input,
        &url,
        &url,
        &winner_proxy,
        &test_fingerprint(),
        tuliprox_session::ConnectionKind::Normal,
        -1,
        super::super::HlsOriginWorkKind::Manifest,
        super::super::HlsOriginWorkClass::ManifestInteractive,
        playback.now(),
    )
    .await
    .unwrap();
    let winner_binding = winner.read().await.origin_account_binding.clone().unwrap();
    assert!(
        playback.session.read().await.origin_account_binding.as_ref().unwrap().is_detached(),
        "a higher priority must actually take over the recovery-protected slot"
    );
    assert!(!playback.sync().await);
    playback.app.active_provider.clear_identified_provider_reservation(
        &old_binding.session_owner,
        &old_binding.account_name,
        old_binding.provider_binding_tag,
    );
    assert_eq!(
        playback.app.active_provider.binding_tag_for_owner(&winner_binding.session_owner),
        winner_binding.provider_binding_tag,
        "late cleanup of the recovery victim cannot release the winner's binding"
    );
    playback.app.connection_manager.release_managed_provider_handle(prepared.preacquired_origin_account_handle);
}

#[tokio::test]
async fn one_terminal_viewer_preserves_the_other_and_the_last_one_releases_capacity() {
    let playback = established().await;
    let mut second = playback.lease().await;
    second.lease_id = HlsAccessLeaseId("second-established-viewer".into());
    playback.app.hls.proxy.prepare_access_lease(second.clone()).await;
    playback.fail().await;
    playback.advance_to(8_000).await;
    let actual_media = playback.session.read().await.activity.last_authorized_media_at_ms.unwrap();
    assert!(playback.sync().await);
    let effective_media = playback.session.read().await.effective_account_binding_activity(playback.now()).unwrap().0;
    assert_eq!(effective_media - actual_media, 7_000, "overlapping viewer blockades count only once");
    let mut first = playback.lease().await;
    first.playback_mode = tuliprox_hls::api::HlsLeasePlaybackMode::Ended;
    playback.app.hls.proxy.prepare_access_lease(first).await;
    assert!(playback.sync().await);
    assert_eq!(playback.app.active_provider.provider_lease_usage(&playback.input.name).total(), 1);
    second.playback_mode = tuliprox_hls::api::HlsLeasePlaybackMode::Ended;
    playback.app.hls.proxy.prepare_access_lease(second).await;
    assert!(!playback.sync().await);
    assert!(playback.session.read().await.origin_account_binding.as_ref().unwrap().is_detached());
    assert_eq!(playback.app.active_provider.provider_lease_usage(&playback.input.name).total(), 0);
}

#[tokio::test]
async fn lease_expiry_ends_recovery_without_extending_or_reviving_authorization() {
    let playback = established().await;
    let mut lease = playback.lease().await;
    lease.valid_until_ms = playback.base + 5_000;
    playback.app.hls.proxy.prepare_access_lease(lease).await;
    playback.fail().await;
    playback.advance_to(5_001).await;
    assert!(!playback.sync().await);
    assert_eq!(playback.lease().await.state, tuliprox_hls::api::HlsAccessLeaseState::Expired);
    assert!(playback.session.read().await.origin_account_binding.as_ref().unwrap().is_detached());
    assert_eq!(playback.app.active_provider.provider_lease_usage(&playback.input.name).total(), 0);
    assert!(!playback.sync().await);
}

#[tokio::test]
async fn a_binding_successor_does_not_inherit_paused_inactivity() {
    let playback = established().await;
    playback.fail().await;
    playback.advance_to(8_000).await;
    assert!(playback.sync().await);
    let mut state = playback.session.write().await;
    let mut successor = state.origin_account_binding.clone().unwrap();
    successor.generation += 1;
    successor.pinned_at_ms = playback.now();
    state.restart_admitted_origin_account(successor);
    let actual_media = state.activity.last_authorized_media_at_ms.unwrap();
    assert_eq!(state.effective_account_binding_activity(playback.now()).unwrap().0, actual_media);
    assert!(!state.origin_binding_recovery_protected(playback.now()));
}

#[tokio::test]
async fn disjoint_blockades_exclude_only_their_union_without_resetting_the_deadline() {
    let playback = established().await;
    playback.fail().await;
    playback.advance_to(4_000).await;
    playback.publish_next().await;
    assert!(!playback.sync().await);
    playback.advance_to(6_000).await;
    {
        let mut state = playback.session.write().await;
        state.segments.get_mut(&6).unwrap().status = tuliprox_hls::api::SegmentCacheStatus::FailedPermanent {
            failed_at_ms: playback.now(),
            status: Some(StatusCode::PROXY_AUTHENTICATION_REQUIRED),
        };
    }
    assert!(playback.sync().await);
    playback.advance_to(8_000).await;
    playback.sync().await;
    let state = playback.session.read().await;
    assert_eq!(
        state.effective_account_binding_activity(playback.now()).unwrap().0
            - state.activity.last_authorized_media_at_ms.unwrap(),
        5_000
    );
    drop(state);
    playback.advance_to(12_100).await;
    assert!(!playback.sync().await);
}

#[tokio::test]
async fn a_new_cache_object_does_not_end_the_blockade_without_its_map_or_key() {
    for map in [true, false] {
        let playback = established().await;
        playback.fail().await;
        playback.advance_to(8_000).await;
        playback.publish_next().await;
        {
            let mut state = playback.session.write().await;
            let next = state.segments.get_mut(&6).unwrap();
            if map {
                next.map_ref = Some(tuliprox_hls::api::ProxyMapId(999));
            } else {
                next.encryption = Some(tuliprox_hls::api::HlsSegmentEncryption {
                    resource_id: tuliprox_hls::api::TransientResourceId("missing-key".into()),
                    resource_extension: "key".into(),
                    iv: None,
                    key_format: None,
                    key_format_versions: None,
                });
            }
        }
        assert!(playback.sync().await, "unavailable MAP/key dependencies are not playable media");
        {
            let mut state = playback.session.write().await;
            let next = state.segments.get_mut(&6).unwrap();
            next.map_ref = None;
            next.encryption = None;
        }
        assert!(!playback.sync().await);
        tokio::time::resume();
    }
}

#[tokio::test]
async fn a_late_failure_cannot_revive_an_already_expired_binding() {
    let playback = established().await;
    playback.advance_to(8_000).await;
    playback.session.write().await.origin_control.path_condition = HlsOriginPathCondition::RetryableFetchFailure;
    assert!(!playback.sync().await);
    assert_eq!(playback.app.active_provider.provider_lease_usage(&playback.input.name).total(), 0);
}

#[tokio::test]
async fn superseded_session_completion_and_expiry_cannot_change_its_replacement() {
    let playback = established().await;
    playback.fail().await;
    playback.advance_to(8_000).await;
    let manager = &playback.app.hls.proxy;
    let key = playback.session.read().await.key.clone();
    manager.sessions().remove_session(&key, &playback.proxy).await.unwrap();
    assert_eq!(
        playback.app.active_provider.provider_lease_usage(&playback.input.name).total(),
        0,
        "a removed session must drop its recovery right even while an old task still owns its handle"
    );
    let replacement = manager.get_or_create_session(key, &playback.app.get_encrypt_secret(), playback.now()).await;
    assert!(!Arc::ptr_eq(&replacement, &playback.session));
    assert!(!playback.sync().await, "a late old completion must reject the superseded session incarnation");
    manager
        .handle_lifecycle_event(
            &playback.app.active_users,
            &playback.app.active_provider,
            tuliprox_hls::api::HlsLifecycleEvent {
                key: tuliprox_hls::api::HlsLifecycleEventKey::OriginBindingProtection {
                    proxy_session_id: playback.proxy.clone(),
                },
                due_at_ms: playback.now(),
            },
            playback.now(),
        )
        .await;
    assert!(replacement.read().await.origin_account_binding.is_none());
    assert!(replacement.read().await.effective_account_binding_activity(playback.now()).is_none());
}
