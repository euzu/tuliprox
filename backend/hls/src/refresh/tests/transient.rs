use super::{
    build_manifest_refresh_timing, classify_manifest_fetch_failure, commit_fetched_manifest, fetched_manifest,
    key_resource_extension, manifest_fetch_context, record_committed_manifest_success,
    refresh_session_with_origin_body, repeated_transient_manifest, test_acceptance_episode_timing,
    test_manifest_origin_binding, test_origin_refresh_request, test_session, transient_reason_log_fields,
    HlsManifestAcceptanceTrigger, HlsManifestCommitError, HlsManifestCommitProgressEvidence,
    HlsManifestCommitRequirement, HlsManifestFetchFailureKind, HlsManifestFetchFailureSignal, HlsManifestProgress,
    HlsManifestRejectLogReason, OriginManifestFetchError,
};
use crate::{
    HlsAccessLeaseStore, HlsFreshManifestRequiredReason, HlsSegmentWorkerPool, HlsSession, HlsSessionKey,
    HlsSessionMode, TransientPassthroughReason, TransientResourceFile, TransientResourceId,
};
use shared::model::HlsManifestRecoveryBurstLevel;
use std::{fmt::Write, sync::Arc, time::Duration};
use tokio::sync::RwLock;
use url::Url;

#[test]
fn normal_key_extension_is_always_compatible_with_the_transient_route() {
    let extensions = [
        key_resource_extension("https://origin.example/live/key.php?token=secret"),
        key_resource_extension("https://origin.example/live/opaque"),
        key_resource_extension("https://origin.example/live/key.BIN"),
        key_resource_extension("https://origin.example/live/key.key"),
    ];

    assert_eq!(extensions, ["key", "key", "bin", "key"]);
    for extension in extensions {
        let file_name = format!("{}.{}", "abcdefghijklmnop", extension);
        let parsed = TransientResourceFile::parse(&file_name).expect("normalized key route parses");
        assert_eq!(parsed.resource_id, TransientResourceId("abcdefghijklmnop".to_string()));
        assert_eq!(parsed.extension, extension);
    }
}

#[test]
fn hls_cutover_policy_transient_success_preserves_recovery_episode_and_empty_refresh_evidence() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    let burst_plan = HlsManifestRecoveryBurstLevel::Friendly.plan();
    session.origin_control.begin_acceptance_episode(
        1_000,
        burst_plan,
        HlsManifestAcceptanceTrigger::RecoveryRequired,
        &test_acceptance_episode_timing(1_000, burst_plan),
    );
    session.origin_control.acceptance_episode.as_mut().expect("acceptance episode").complete();
    session.origin_refresh.consecutive_empty_refreshes = 2;
    session.origin_refresh.mark_started(10_000);

    let (bookkeeping_timing, applied_interval_ms) = record_committed_manifest_success(
        &mut session,
        HlsManifestCommitProgressEvidence::Transient(build_manifest_refresh_timing(
            None,
            Some(12_000),
            HlsManifestProgress::Advanced,
        )),
        10_000,
        10_100,
    );

    assert_eq!(bookkeeping_timing.progress, HlsManifestProgress::Unchanged);
    assert_eq!(applied_interval_ms, 1_000);
    assert_eq!(session.origin_refresh.consecutive_empty_refreshes, 3);
    assert_eq!(session.origin_refresh.next_fetch_allowed_at_ms, 11_000);
    assert!(session.origin_control.acceptance_episode.is_some());
    assert_eq!(session.origin_control.recovery_samples.p95_ms(), None);
    assert_eq!(session.origin_control.last_origin_response_at_ms, Some(10_100));
}

#[test]
fn transient_reason_log_fields_include_unsupported_tag() {
    let reason = TransientPassthroughReason::UnsupportedTag { tag: "#EXT-X-PART".to_string() };

    assert_eq!(transient_reason_log_fields(&reason), "reason=unsupported_tag tag=#EXT-X-PART");
}

#[tokio::test]
async fn ext_x_key_manifest_commits_transient_rewrite() {
    let session = refresh_session_with_origin_body(
        "#EXTM3U\n#EXT-X-TARGETDURATION:12\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n#EXTINF:4.0,\nseg.ts\n",
    )
    .await;
    let session = session.read().await;
    let body = session.transient.last_manifest_body.as_ref().expect("transient body");

    assert!(matches!(
        session.mode,
        HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey }
    ));
    assert!(body.contains("/hls/shared/live/"));
    assert!(body.contains("/r/"));
    assert!(!body.contains("/hls/user/"));
    assert_eq!(session.transient.resources.len(), 2);
    assert_eq!(session.target_duration, Some(12));
    assert_eq!(session.account_overlap_timing().target_duration_ms, 12_000);
}

#[tokio::test]
async fn finalized_event_manifest_commits_full_transient_lifecycle() {
    let mut origin_body =
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-MEDIA-SEQUENCE:1\n".to_string();
    for sequence in 1..=8 {
        let _ = write!(origin_body, "#EXTINF:4.0,\n{sequence}.ts\n");
    }
    origin_body.push_str("#EXT-X-ENDLIST\n");
    let origin_body: &'static str = Box::leak(origin_body.into_boxed_str());
    let session = refresh_session_with_origin_body(origin_body).await;
    let session = session.read().await;
    let body = session.transient.last_manifest_body.as_ref().expect("finalized transient body");

    assert!(matches!(
        session.mode,
        HlsSessionMode::TransientPassthrough {
            reason: TransientPassthroughReason::UnsupportedTag { ref tag }
        } if tag == "#EXT-X-PLAYLIST-TYPE"
    ));
    let stored_body_lifecycle = tuliprox_parser::hls::origin_manifest::parse_manifest_semantics(body).lifecycle();
    assert_eq!(session.transient.last_manifest_finalized(), stored_body_lifecycle.is_finalized());
    assert_eq!(
        session.transient.last_manifest_window_policy(),
        tuliprox_parser::hls::origin_manifest::HlsManifestWindowPolicy::PreserveFullManifest
    );
    assert_eq!(session.transient.current_manifest_resource_ids().len(), 8);
    assert_eq!(session.transient.last_manifest_playlist_duration_ms, Some(32_000));
    assert_eq!(session.transient.last_manifest_valid_until_ms(), None);
    assert!(body.contains("#EXT-X-PLAYLIST-TYPE:EVENT"));
    assert!(body.contains("#EXT-X-ENDLIST"));
    assert_eq!(body.matches("/r/").count(), 8);
}

#[test]
fn duplicate_media_units_are_rejected_locally_before_transient_commit() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
    let request = test_origin_refresh_request(test_session());
    let body = repeated_transient_manifest(crate::manifest_limits::MAX_HLS_LEASE_SNAPSHOT_SEGMENTS + 1);

    let result = commit_fetched_manifest(&mut session, &fetched_manifest(&body), &request, 100);

    assert!(matches!(
        result,
        Err(HlsManifestCommitError::LocalRepresentationLimit(violation))
            if violation.kind == crate::manifest_limits::HlsManifestLimitKind::LeaseSnapshotSegments
    ));
    assert!(session.transient.last_manifest_body.is_none());
    let signal = classify_manifest_fetch_failure(&OriginManifestFetchError::LocalRepresentationLimit(
        crate::manifest_limits::HlsManifestLimitViolation::new(
            crate::manifest_limits::HlsManifestLimitKind::LeaseSnapshotSegments,
            crate::manifest_limits::MAX_HLS_LEASE_SNAPSHOT_SEGMENTS + 1,
            crate::manifest_limits::MAX_HLS_LEASE_SNAPSHOT_SEGMENTS,
        ),
    ));
    assert_eq!(signal, HlsManifestFetchFailureSignal::discarded(HlsManifestFetchFailureKind::LocalRepresentationLimit));
}

#[test]
fn transient_commit_generation_exhaustion_is_not_reported_as_manifest_limit() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
    session.set_manifest_commit_generation_for_test(u64::MAX);
    let request = test_origin_refresh_request(test_session());
    let manifest_limit_rejections_before = request.segment_worker_pool.metrics().snapshot().manifest_limit_rejections;
    let fetched = fetched_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n#EXTINF:4,\nseg.ts\n",
    );

    let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);

    assert!(matches!(result, Err(HlsManifestCommitError::CommitGenerationExhausted)));
    assert_eq!(
        request.segment_worker_pool.metrics().snapshot().manifest_limit_rejections,
        manifest_limit_rejections_before
    );
    assert!(session.transient.last_manifest_body.is_none());
}

#[tokio::test]
async fn initial_transient_manifest_without_snapshot_template_is_rejected_locally() {
    let session = test_session();
    session.write().await.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
    let request = test_origin_refresh_request(Arc::clone(&session));
    let fetch_context = manifest_fetch_context(&request);
    let fetched = fetched_manifest(
        "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-MEDIA-SEQUENCE:1\n\
         #EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n#EXTINF:4,\nsegment.ts\n#EXT-X-ENDLIST\n",
    );
    let recovery_binding =
        test_manifest_origin_binding(Url::parse("http://127.0.0.1:9/recovery.m3u8").expect("test recovery URL"));

    let result =
        super::super::commit_initial_fetched_manifest(&request, &fetch_context, fetched, Some(recovery_binding), false)
            .await;

    assert!(matches!(result, Err(OriginManifestFetchError::MalformedTransientRepresentation)));
    assert_eq!(
        classify_manifest_fetch_failure(&OriginManifestFetchError::MalformedTransientRepresentation),
        HlsManifestFetchFailureSignal::discarded(HlsManifestFetchFailureKind::MalformedTransientRepresentation)
    );
    let session = session.read().await;
    assert!(session.transient.last_manifest_body.is_none());
    assert!(session.transient.last_manifest_template().is_none());
    assert!(session.transient.last_manifest_commit_identity().is_none());
    assert_eq!(session.transient.manifest_generation(), 0);
    assert!(session.transient.resources.is_empty());
    assert!(crate::manifest_commit::hls_committed_manifest_body_for_request(
        &session,
        crate::manifest_commit::HlsCachedManifestOptions::initial(Duration::ZERO),
        0,
        100,
    )
    .is_none());
}

#[test]
fn transient_manifest_without_snapshot_template_preserves_deliverable_commit() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
    let request = test_origin_refresh_request(test_session());
    let baseline = fetched_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-MEDIA-SEQUENCE:1\n\
         #EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n#EXTINF:4,\nsegment.ts\n#EXT-X-ENDLIST\n",
    );
    assert!(commit_fetched_manifest(&mut session, &baseline, &request, 100).is_ok());
    session.mark_pending_handoff_discontinuity(7);
    let baseline_body = session.transient.last_manifest_body.clone().expect("baseline manifest");
    let baseline_template = session.transient.last_manifest_template().expect("baseline template");
    let baseline_generation = session.transient.manifest_generation();
    let baseline_finalized_generation =
        session.transient.current_finalized_manifest_generation().expect("baseline finalized generation");
    let baseline_finalized_generation_count = session.transient.finalized_manifest_generation_count();
    let baseline_commit_identity = session.transient.last_manifest_commit_identity().expect("baseline identity");
    let baseline_resources = session.transient.resources.keys().cloned().collect::<std::collections::HashSet<_>>();
    let baseline_manifest_resource_ids = session.transient.current_manifest_resource_ids().clone();
    let baseline_highwater = session.origin_seq_highwater;
    let candidate = fetched_manifest(
        "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-MEDIA-SEQUENCE:2\n\
         #EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"rotated-key.bin\"\n#EXTINF:4,\nreplacement.ts\n#EXT-X-ENDLIST\n",
    );

    let result = commit_fetched_manifest(&mut session, &candidate, &request, 102);

    assert!(matches!(result, Err(HlsManifestCommitError::MalformedTransientRepresentation)));
    assert!(Arc::ptr_eq(
        session.transient.last_manifest_body.as_ref().expect("baseline remains current"),
        &baseline_body
    ));
    assert!(Arc::ptr_eq(
        &session.transient.last_manifest_template().expect("baseline template remains current"),
        &baseline_template
    ));
    assert_eq!(session.transient.manifest_generation(), baseline_generation);
    assert_eq!(session.transient.current_finalized_manifest_generation(), Some(baseline_finalized_generation));
    assert_eq!(session.transient.finalized_manifest_generation_count(), baseline_finalized_generation_count);
    assert_eq!(session.transient.last_manifest_commit_identity(), Some(baseline_commit_identity));
    assert_eq!(
        session.transient.resources.keys().cloned().collect::<std::collections::HashSet<_>>(),
        baseline_resources
    );
    assert_eq!(session.transient.current_manifest_resource_ids(), &baseline_manifest_resource_ids);
    assert_eq!(session.pending_handoff_discontinuity_sequence, Some(7));
    assert_eq!(session.origin_seq_highwater, baseline_highwater);
    assert!(session
        .transient
        .last_manifest_template()
        .zip(session.transient.last_manifest_commit_identity())
        .is_some());
}

#[test]
fn transient_commit_rejects_same_host_backward_manifest_outside_rollover_window() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
    session.origin_seq_highwater = Some(758);
    session.last_effective_manifest_host = Some("origin.example.com".to_string());
    session.mark_authorized_media_access(100);
    let previous_manifest =
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:757\n#EXTINF:4.0,\n/hls/shared/live/session/lease/r/old.ts\n".to_string();
    session.transient.replace_manifest_with_semantics(previous_manifest.clone(), 10, None);
    let request = test_origin_refresh_request(test_session());
    let fetched = fetched_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:12\n#EXT-X-MEDIA-SEQUENCE:226\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n#EXTINF:4.0,\nseg.ts\n",
    );

    let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);

    assert!(matches!(result, Err(HlsManifestCommitError::TimelineRejected { .. })));
    assert_eq!(session.transient.last_manifest_body.as_deref(), Some(previous_manifest.as_str()));
    assert_eq!(session.origin_seq_highwater, Some(758));
}

#[test]
fn transient_commit_rebases_expired_session_highwater() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
    session.target_duration = Some(12);
    session.origin_seq_highwater = Some(758);
    session.last_effective_manifest_host = Some("origin.example.com".to_string());
    session.mark_authorized_media_access(1_000);
    let previous_manifest =
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:757\n#EXTINF:4.0,\n/hls/shared/live/session/lease/r/old.ts\n".to_string();
    session.transient.replace_manifest_with_semantics(previous_manifest.clone(), 10, None);
    let request = test_origin_refresh_request(test_session());
    let fetched = fetched_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:12\n#EXT-X-MEDIA-SEQUENCE:900\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n#EXTINF:4.0,\nseg.ts\n",
    );

    let result = commit_fetched_manifest(&mut session, &fetched, &request, 40_000);

    assert!(result.is_ok());
    assert_ne!(session.transient.last_manifest_body.as_deref(), Some(previous_manifest.as_str()));
    assert_eq!(session.origin_seq_highwater, Some(900));
}

#[test]
fn transient_commit_accepts_monotonic_media_sequence_and_updates_highwater() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
    session.origin_seq_highwater = Some(758);
    session.last_effective_manifest_host = Some("origin.example.com".to_string());
    session.transient.replace_manifest_with_semantics(
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:757\n#EXTINF:4.0,\n/hls/shared/live/session/lease/r/old.ts\n".to_string(),
        10,
        None,
    );
    let request = test_origin_refresh_request(test_session());
    let fetched = fetched_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:759\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n#EXTINF:4.0,\nseg759.ts\n#EXTINF:4.0,\nseg760.ts\n",
    );

    let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);

    assert!(result.is_ok());
    assert_eq!(session.origin_seq_highwater, Some(760));
    assert!(session.transient.last_manifest_body.as_ref().is_some_and(|body| body.contains("/r/")));
}

#[test]
fn fresh_revalidation_rebases_transient_manifest_on_the_pinned_host() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
    session.origin_seq_highwater = Some(1_000);
    session.last_effective_manifest_host = Some("origin.example.com".to_string());
    let mut request = test_origin_refresh_request(test_session());
    request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired {
        reason: HlsFreshManifestRequiredReason::ExpiredRevalidation,
    };
    let fetched = fetched_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:10\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n#EXTINF:4.0,\nseg10.ts\n",
    );

    let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);

    assert!(result.is_ok());
    assert_eq!(session.origin_seq_highwater, Some(10));
    assert_eq!(session.last_effective_manifest_host.as_deref(), Some("origin.example.com"));
    assert!(session.transient.last_manifest_body.as_ref().is_some_and(|body| body.contains("/r/")));
}

#[tokio::test]
async fn unsupported_tag_manifest_commits_transient_rewrite() {
    let session = refresh_session_with_origin_body(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-PART:DURATION=1.0,URI=\"part.m4s\"\n\
         #EXTINF:4.0,\nseg.ts\n",
    )
    .await;
    let session = session.read().await;

    assert!(matches!(
        session.mode,
        HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::UnsupportedTag { .. } }
    ));
    assert!(session.transient.last_manifest_body.is_some());
}

#[tokio::test]
async fn parser_unsupported_feature_manifest_commits_transient_rewrite() {
    let session = refresh_session_with_origin_body(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-BYTERANGE:10\n#EXTINF:4.0,\nseg.ts\n",
    )
    .await;
    let session = session.read().await;

    assert!(matches!(
        session.mode,
        HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ParserUnsupportedFeature { .. } }
    ));
    assert!(session.transient.last_manifest_body.is_some());
}

#[test]
fn fast_start_transient_transition_is_allowed_only_before_publication() {
    for mode in [shared::model::HlsStartupMode::FirstReady, shared::model::HlsStartupMode::Progressive] {
        for published in [false, true] {
            let mut session = HlsSession::new(HlsSessionKey::new(1, "transition"), b"secret", 0);
            let config = tuliprox_core::model::HlsStartupConfig { mode, ..Default::default() };
            let worker = Arc::new(HlsSegmentWorkerPool::default());
            session.startup = Some(crate::HlsSessionStartup {
                config: config.clone(),
                first_data_timeout_ms: 10000,
                worker: Arc::downgrade(&worker),
                access_leases: Arc::new(RwLock::new(HlsAccessLeaseStore::default())),
                revisions: std::collections::BTreeMap::new(),
                policy_fixed: true,
                store: Arc::new(crate::SegmentRevisionStore::default()),
                budget: crate::ProgressiveBudgetManager::new(config),
            });
            if published {
                session.published_live_origin_baseline = Some(crate::session::HlsPublishedLiveOriginBaseline {
                    evidence_proxy_seq: 0,
                    origin_epoch: 0,
                    rendered_at_ms: 10,
                });
            }
            let before = session.published_live_origin_baseline;
            let request = test_origin_refresh_request(test_session());
            let fetched = fetched_manifest("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n#EXTINF:4,\n1.ts\n");
            let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);
            if published {
                assert!(matches!(
                    result,
                    Err(HlsManifestCommitError::TimelineRejected {
                        reason: HlsManifestRejectLogReason::StartupRepresentationChange,
                    })
                ));
                assert!(matches!(session.mode, HlsSessionMode::NormalCacheTimeline));
                assert!(session.startup.is_some());
                assert_eq!(session.published_live_origin_baseline, before);
                assert!(session.transient.last_manifest_body.is_none());
                assert!(session.transient.resources.is_empty());
            } else {
                assert!(result.is_ok());
                assert!(matches!(session.mode, HlsSessionMode::TransientPassthrough { .. }));
                assert!(session.startup.is_none());
                assert!(session.transient.last_manifest_body.is_some());
            }
        }
    }
}
