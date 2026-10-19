use super::*;
use crate::{manifest_acceptance::HlsManifestAcceptanceEpisodeStatus, SegmentCacheStatus};
use shared::model::HlsManifestRecoveryBurstLevel;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::Mutex;

const BASELINE: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/reduced-retry/baseline.m3u8"));
const CONFLICT: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/reduced-retry/conflict.m3u8"));
const REPAIRED: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/reduced-retry/repaired.m3u8"));

async fn commit_baseline(request: &OriginRefreshRequest, context: &HlsOriginManifestFetchContext) {
    let baseline =
        fetch_hls_origin_manifest_request(HlsOriginManifestFetchRequest::initial_global_policy(context)).await.unwrap();
    {
        let mut s = request.session.write().await;
        let now_ms = tuliprox_core::utils::current_time_millis();
        commit_fetched_manifest(&mut s, &baseline, request, now_ms).unwrap();
        s.activity.last_authorized_media_at_ms = Some(now_ms);
        for segment in s.segments.values_mut() {
            segment.status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: now_ms };
        }
        s.render_and_store_manifest(now_ms).unwrap();
        assert_eq!(s.origin_seq_highwater, Some(1005));
        assert!(s.established_manifest_recovery_binding().is_some());
    }
}

async fn conflict_then_reduced_retry(permanent_conflict: bool) {
    let recovery = Arc::new(AtomicBool::new(false));
    let requests = Arc::new(AtomicUsize::new(0));
    let (phase, count) = (Arc::clone(&recovery), Arc::clone(&requests));
    let server = spawn_test_origin(Arc::new(move |_| {
        let body = if !phase.load(Ordering::Acquire) {
            BASELINE
        } else {
            let n = count.fetch_add(1, Ordering::AcqRel) + 1;
            eprintln!("recovery HTTP request={n} status=200 permanent={permanent_conflict}");
            if n <= 12 || permanent_conflict {
                CONFLICT
            } else {
                REPAIRED
            }
        };
        (200, Vec::new(), body.to_string())
    }))
    .await;
    let session = test_session();
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    let url = format!("{}/index.m3u8", server.base_url);
    request.origin_entry = LiveHlsOriginEntry::parse(&url).unwrap();
    request.manifest_recovery_burst.level = HlsManifestRecoveryBurstLevel::Beast;
    let context = manifest_fetch_context(&request);
    commit_baseline(&request, &context).await;
    recovery.store(true, Ordering::Release);
    let callbacks = Arc::new(Mutex::new(Vec::new()));
    let observations = Arc::clone(&callbacks);
    let request_ref = &request;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        retry_hls_origin_manifest_recovery_chain(
            &context,
            test_manifest_origin_binding(url::Url::parse(&url).unwrap()),
            None,
            None,
            HlsManifestAcceptanceTrigger::RecoveryRequired,
            HlsManifestCommitAcceptanceMode::StrictPinnedHost,
            move |fetched, mode| {
                let observations = Arc::clone(&observations);
                async move {
                    let before = {
                        let s = request_ref.session.read().await;
                        let score =
                            score_manifest_recovery_candidate(&s, &fetched, &manifest_fetch_context(request_ref))
                                .unwrap();
                        assert_eq!(
                            score.quality.host_relation,
                            crate::manifest_fetch::HlsManifestOriginRelation::SameRedirectHost
                        );
                        assert_eq!(score.quality.sequence_relation, HlsManifestSequenceRelation::PlausibleForward);
                        eprintln!(
                            "selected candidate host={:?} sequence={:?}",
                            score.quality.host_relation, score.quality.sequence_relation
                        );
                        s.origin_control.acceptance_episode.clone().unwrap()
                    };
                    let outcome = super::super::commit_manifest_recovery_candidate(request_ref, fetched, mode).await;
                    let label = match &outcome {
                        Ok(_) => "committed".to_string(),
                        Err(HlsManifestCommitError::TimelineRejected { reason }) => reason.status_label().to_string(),
                        Err(error) => format!("{error:?}"),
                    };
                    eprintln!(
                        "real commit callback={} generation={} full_bursts={} timing={:?}",
                        label,
                        before.generation.0,
                        before.full_bursts_completed,
                        before.timing()
                    );
                    observations.lock().await.push((label, before));
                    outcome
                }
            },
        ),
    )
    .await
    .unwrap();
    let records = callbacks.lock().await;
    let s = session.read().await;
    let episode = s.origin_control.acceptance_episode.as_ref().unwrap();
    eprintln!(
        "recovery result={:?} requests={} callbacks={} episode={episode:?}",
        result.as_ref().map(|_| ()),
        requests.load(Ordering::Acquire),
        records.len()
    );
    assert!(
        records[0].0.contains("origin-sequence-resource-conflict"),
        "first commit must encounter the real conflict: {}",
        records[0].0
    );
    if permanent_conflict {
        assert!(result.is_err());
        assert_eq!(requests.load(Ordering::Acquire), 16);
        assert_eq!(records.len(), 5, "all reduced candidates must reach the real commit guard");
        assert!(records.iter().all(|(label, _)| label.contains("origin-sequence-resource-conflict")));
        assert!(matches!(
            crate::manifest_acceptance::manifest_acceptance_episode_status(
                Some(episode),
                episode.generation,
                request.now_ms
            ),
            HlsManifestAcceptanceEpisodeStatus::FullBurstExhausted { .. }
        ));
        assert_eq!(s.origin_seq_highwater, Some(1005));
    } else {
        assert!(
            result.is_ok(),
            "repaired single-candidate retry must commit; requests={} callbacks={}",
            requests.load(Ordering::Acquire),
            records.len()
        );
        assert_eq!(requests.load(Ordering::Acquire), 13);
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].0, "committed");
        assert_eq!(s.origin_seq_highwater, Some(1008));
    }
    assert_eq!(episode.full_bursts_completed, 1);
    for (_, before) in records.iter() {
        assert_eq!(before.generation, episode.generation);
        assert_eq!(before.timing(), episode.timing());
        assert_eq!(before.full_bursts_completed, 1);
    }
}

#[tokio::test]
async fn reduced_retry_commits_repaired_same_host_manifest_after_real_resource_conflict() {
    conflict_then_reduced_retry(false).await;
}

#[tokio::test]
async fn reduced_retry_permanent_resource_conflict_exhausts_existing_budget() {
    conflict_then_reduced_retry(true).await;
}

async fn superseded_reduced_response_preserves_successor(account_handoff: bool, after_selection: bool) {
    use super::support::{spawn_test_origin_with_response_gate, TestOriginResponseGate};
    use crate::short_manifest::{HlsOriginQualification, HlsOriginQualificationBasis};
    use tokio::sync::Notify;

    let requests = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&requests);
    let gate = Arc::new(TestOriginResponseGate {
        request_number: 14, // Baseline plus twelve burst samples, then the reduced retry.
        entered: Notify::new(),
        release: Notify::new(),
    });
    let server = spawn_test_origin_with_response_gate(
        Arc::new(move |_| {
            let n = count.fetch_add(1, Ordering::AcqRel);
            let body = if n == 0 {
                BASELINE
            } else if n <= 12 {
                CONFLICT
            } else {
                REPAIRED
            };
            (200, Vec::new(), body.to_string())
        }),
        if after_selection { None } else { Some(Arc::clone(&gate)) },
    )
    .await;
    let session = test_session();
    let mut request = super::account_handoff::request_with_origin_work(&session);
    request.origin_io.as_mut().unwrap().started_generation = Some(session.read().await.activity.origin_work_generation);
    let url = format!("{}/index.m3u8", server.base_url);
    request.origin_entry = LiveHlsOriginEntry::parse(&url).unwrap();
    request.manifest_recovery_burst.level = HlsManifestRecoveryBurstLevel::Beast;
    let context = manifest_fetch_context(&request);
    commit_baseline(&request, &context).await;
    let baseline = session.read().await.origin_to_proxy.clone();
    let callbacks = Arc::new(AtomicUsize::new(0));
    let observed_callbacks = Arc::clone(&callbacks);
    let binding = test_manifest_origin_binding(url::Url::parse(&url).unwrap());
    let retry_binding = binding.clone();
    let commit_gate = Arc::clone(&gate);
    let recovery = tokio::spawn(async move {
        retry_hls_origin_manifest_recovery_chain(
            &context,
            binding,
            None,
            None,
            HlsManifestAcceptanceTrigger::RecoveryRequired,
            HlsManifestCommitAcceptanceMode::StrictPinnedHost,
            |fetched, mode| {
                let number = observed_callbacks.fetch_add(1, Ordering::AcqRel) + 1;
                let gate = Arc::clone(&commit_gate);
                let request_ref = &request;
                async move {
                    if after_selection && number == 2 {
                        gate.entered.notify_one();
                        gate.release.notified().await;
                    }
                    super::super::commit_manifest_recovery_candidate(request_ref, fetched, mode).await
                }
            },
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified()).await.unwrap();
    let successor = {
        let mut s = session.write().await;
        let old = s.origin_control.acceptance_episode.as_ref().unwrap().clone();
        if after_selection {
            assert!(old.exhaustion_reason().is_none());
            assert_eq!(old.state, crate::manifest_acceptance::HlsManifestAcceptanceState::Committing);
        } else {
            assert!(old.exhaustion_reason().is_some());
        }
        if account_handoff {
            let owner = s.proxy_session_id.0.clone();
            s.handoff_origin_account(crate::HlsOriginAccountBinding::rebound(
                Arc::from("input"),
                Arc::from("account-b"),
                owner,
                1,
                200,
            ));
        } else {
            s.invalidate_queued_origin_work();
        }
        let now = tuliprox_core::utils::current_time_millis();
        let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
        let generation = s.origin_control.begin_acceptance_episode(
            now,
            plan,
            HlsManifestAcceptanceTrigger::RecoveryRequired,
            &test_acceptance_episode_timing(now, plan),
        );
        assert_ne!(generation, old.generation);
        s.origin_control.qualification = HlsOriginQualification::Checking {
            basis: HlsOriginQualificationBasis::capture(&s),
            generation,
            target: retry_binding,
        };
        s.origin_control.acceptance_episode.clone().unwrap()
    };
    gate.release.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), recovery).await.unwrap().unwrap();
    {
        let s = session.read().await;
        let actual = s.origin_control.acceptance_episode.as_ref().unwrap();
        eprintln!("REVIEW: after_selection={after_selection} handoff={account_handoff} result={:?} callbacks={} requests={} expected_generation={} actual_generation={} expected_state={:?} actual_state={:?} successor_unchanged={}",
            result.as_ref().map(|_| ()), callbacks.load(Ordering::Acquire), requests.load(Ordering::Acquire),
            successor.generation.0, actual.generation.0, successor.state, actual.state, actual == &successor);
        assert!(actual == &successor, "old commit must leave the successor episode unchanged");
    }
    assert!(matches!(
        result,
        Err(OriginManifestFetchError::RecoveryUnavailable {
            reason: HlsManifestRecoveryUnavailableReason::BindingSuperseded
        })
    ));
    assert_eq!(
        callbacks.load(Ordering::Acquire),
        if after_selection { 2 } else { 1 },
        "old response must not reach another commit"
    );
    assert_eq!(requests.load(Ordering::Acquire), 14, "old chain stops before another attempt");
    let s = session.read().await;
    assert_eq!(s.origin_to_proxy, baseline);
    assert_eq!(
        s.origin_control.acceptance_episode.as_ref(),
        Some(&successor),
        "old work must leave the pending successor unchanged"
    );
    assert_eq!(s.origin_control.qualification.generation(), Some(successor.generation));
}

#[tokio::test]
async fn reduced_retry_held_response_cannot_mutate_successor_after_account_handoff() {
    superseded_reduced_response_preserves_successor(true, false).await;
}

#[tokio::test]
async fn reduced_retry_held_response_cannot_mutate_successor_after_work_generation_change() {
    superseded_reduced_response_preserves_successor(false, false).await;
}

#[tokio::test]
async fn review_selected_commit_cannot_mutate_successor_after_account_handoff() {
    superseded_reduced_response_preserves_successor(true, true).await;
}

#[tokio::test]
async fn review_selected_commit_cannot_mutate_successor_after_work_generation_change() {
    superseded_reduced_response_preserves_successor(false, true).await;
}
