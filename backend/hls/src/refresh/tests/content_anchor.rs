use super::*;
use crate::{HlsProxyManager, HlsSessionKey, SegmentCacheStatus, SegmentFetchContext};
use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap as HttpHeaders, StatusCode as HttpStatus, Uri},
    response::{IntoResponse, Response},
    Router,
};
use sha2::Digest;
use std::{
    fmt::Write as _,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

const NAMES: [&str; 12] =
    ["amber", "birch", "cobalt", "dahlia", "ember", "violet", "cedar", "pearl", "silver", "copper", "hazel", "linen"];

fn raw_media(sequence: u64) -> Vec<u8> {
    let mut packet = [0xff; 188];
    packet[..4].copy_from_slice(&[0x47, 0x1f, 0xff, 0x10]);
    let marker = format!("anchor-occurrence-{sequence};");
    packet[4..4 + marker.len()].copy_from_slice(marker.as_bytes());
    [CRITICAL_HANDOFF_TS_BODY, packet.as_slice()].concat()
}

struct OriginState {
    port: u16,
    opaque_names: bool,
    requests: Mutex<Vec<String>>,
    manifest_calls: AtomicUsize,
    pinned_after: AtomicUsize,
    head: AtomicU64,
    fail_probe: AtomicBool,
    gap_a_438: AtomicBool,
    gap_b_438: AtomicUsize,
    gap_b_bad_anchor: AtomicBool,
    corrupt_probe: AtomicBool,
    pause_pinned: AtomicBool,
    pause_sequence: AtomicU64,
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

struct Origin {
    state: Arc<OriginState>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Origin {
    fn drop(&mut self) { self.task.abort(); }
}

impl Origin {
    async fn start(opaque_names: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = Arc::new(OriginState {
            port: listener.local_addr().unwrap().port(),
            opaque_names,
            requests: Mutex::new(Vec::new()),
            manifest_calls: AtomicUsize::new(0),
            pinned_after: AtomicUsize::new(usize::MAX),
            head: AtomicU64::new(434),
            fail_probe: AtomicBool::new(false),
            gap_a_438: AtomicBool::new(false),
            gap_b_438: AtomicUsize::new(0),
            gap_b_bad_anchor: AtomicBool::new(false),
            corrupt_probe: AtomicBool::new(false),
            pause_pinned: AtomicBool::new(false),
            pause_sequence: AtomicU64::new(0),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        });
        let app = Router::new().fallback(serve).with_state(Arc::clone(&state));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { state, task }
    }
    fn base(&self, pinned: bool) -> String {
        format!("http://{}:{}", if pinned { "127.0.0.1" } else { "localhost" }, self.state.port)
    }
    fn fetched(&self, pinned: bool, head: u64) -> FetchedOriginManifest {
        let mut fetched = fetched_manifest(&playlist(&self.state, pinned, head));
        fetched.final_manifest_url = format!("{}/index.m3u8", self.base(pinned));
        fetched.resolved_request_url = fetched.final_manifest_url.clone();
        fetched.redirect_host = Some(if pinned { "127.0.0.1" } else { "localhost" }.into());
        fetched
    }
    fn media_requests(&self) -> Vec<String> {
        self.state.requests.lock().unwrap().iter().filter(|p| p.contains("/media/")).cloned().collect()
    }
}

fn media_name(state: &OriginState, sequence: u64) -> String {
    if state.opaque_names {
        format!("{}.ts", NAMES[usize::try_from(sequence - 432).unwrap()])
    } else {
        format!("{sequence}.ts")
    }
}
fn playlist(state: &OriginState, pinned: bool, head: u64) -> String {
    let host = if pinned { "127.0.0.1" } else { "localhost" };
    let mut body = format!("#EXTM3U\n#EXT-X-TARGETDURATION:8\n#EXT-X-MEDIA-SEQUENCE:{head}\n");
    for seq in head..head + 6 {
        let mode = if !pinned && seq == 438 { state.gap_b_438.load(Ordering::Acquire) } else { 0 };
        let query = if mode == 3 { 2 } else { 1 };
        let duration = if mode == 5 { 7 } else { 8 };
        writeln!(
            body,
            "#EXTINF:{duration},\nhttp://{host}:{}/media/{}?fixture={query}",
            state.port,
            media_name(state, seq)
        )
        .unwrap();
    }
    body
}
async fn serve(State(state): State<Arc<OriginState>>, headers: HttpHeaders, uri: Uri) -> Response {
    let host = headers.get(header::HOST).unwrap().to_str().unwrap();
    state.requests.lock().unwrap().push(format!("{host} {uri}"));
    if uri.path() == "/index.m3u8" {
        let number = state.manifest_calls.fetch_add(1, Ordering::AcqRel);
        if number >= state.pinned_after.load(Ordering::Acquire) {
            return (HttpStatus::FOUND, [(header::LOCATION, format!("http://127.0.0.1:{}/pinned.m3u8", state.port))])
                .into_response();
        }
        return playlist(&state, false, state.head.load(Ordering::Acquire)).into_response();
    }
    if uri.path() == "/pinned.m3u8" {
        return playlist(&state, true, 436).into_response();
    }
    let sequence = (432..444).find(|seq| uri.path() == format!("/media/{}", media_name(&state, *seq)));
    let Some(sequence) = sequence else {
        return HttpStatus::NOT_FOUND.into_response();
    };
    let alternative = host.starts_with("localhost:");
    if !alternative && sequence == 438 && state.gap_a_438.load(Ordering::Acquire) {
        return HttpStatus::NOT_FOUND.into_response();
    }
    if (alternative || state.pause_pinned.load(Ordering::Acquire))
        && state.pause_sequence.load(Ordering::Acquire) == sequence
    {
        state.entered.notify_one();
        state.release.acquire().await.unwrap().forget();
    }
    if alternative && sequence == 437 && state.fail_probe.load(Ordering::Acquire) {
        return HttpStatus::SERVICE_UNAVAILABLE.into_response();
    }
    let payload = raw_media(if alternative && sequence == 437 && state.corrupt_probe.load(Ordering::Acquire) {
        999
    } else if alternative && sequence == 439 && state.gap_b_bad_anchor.load(Ordering::Acquire) {
        998
    } else {
        sequence
    });
    if alternative && sequence == 438 {
        match state.gap_b_438.load(Ordering::Acquire) {
            1 => return HttpStatus::NOT_FOUND.into_response(),
            2 => {
                return Response::builder()
                    .header(header::CONTENT_TYPE, "video/mp2t")
                    .header(header::CONTENT_LENGTH, payload.len() + 188)
                    .body(Body::from_stream(futures::stream::iter([Ok::<_, std::io::Error>(bytes::Bytes::from(
                        payload,
                    ))])))
                    .unwrap();
            }
            4 => {
                let mut payload = payload;
                // Damage the final packet beyond the track-inspection prefix.
                let final_packet = payload.len() - 188;
                payload[final_packet] = 0;
                return ([(header::CONTENT_TYPE, "video/mp2t")], Body::from(payload)).into_response();
            }
            _ => {}
        }
    }
    ([(header::CONTENT_TYPE, "video/mp2t")], Body::from(payload)).into_response()
}

async fn warm(origin: &Origin) -> OriginRefreshRequest {
    let manager = Arc::new(HlsProxyManager::with_private_cache(&shared::model::HlsCacheConfigDto::default()));
    let session = manager.get_or_create_session(HlsSessionKey::new(1, "anchor"), b"fixture", 0).await;
    session.write().await.proxy_next_seq = Some(45);
    let mut request =
        switch_test_request(Arc::clone(&session), Arc::clone(manager.segment_cache()), &origin.base(false));
    request.hls_proxy = manager;
    request.origin_entry = LiveHlsOriginEntry::parse(&format!("{}/index.m3u8", origin.base(false))).unwrap();
    request.retry_policy = no_delay_policy();
    request.now_ms = super::super::current_time_millis();
    request.manifest_commit_requirement = HlsManifestCommitRequirement::CommittedManifestAllowed;
    let fetched = origin.fetched(true, 432);
    let context = manifest_fetch_context(&request, None);
    super::super::commit_initial_fetched_manifest(&request, &context, fetched, None, false).await.unwrap();
    let context = media_context(&request);
    for seq in 45..=50 {
        assert_eq!(
            request
                .segment_worker_pool
                .demand_fetch_and_wait(
                    context.clone(),
                    &crate::HlsSegmentFile { proxy_seq: seq, extension: "ts".into() },
                    request.now_ms
                )
                .await,
            crate::segment_fetcher::SegmentDemandFetchOutcome::Ready
        );
    }
    let mut state = session.write().await;
    state.last_effective_manifest_host = Some("127.0.0.1".into());
    state.origin_control.pinned_host = Some("127.0.0.1".into());
    state.origin_control.origin_epoch = state.origin_epoch;
    state.origin_control.last_media_progress_at_ms = Some(request.now_ms);
    state.mark_authorized_media_access(request.now_ms);
    assert_eq!(state.segments[&50].origin_key.host_local_sequence, 437);
    assert!(matches!(state.segments[&50].status, SegmentCacheStatus::Ready { .. }));
    drop(state);
    origin.state.requests.lock().unwrap().clear();
    request
}

async fn matching_overlap(opaque: bool) {
    let origin = Origin::start(opaque).await;
    let request = warm(&origin).await;
    if opaque {
        let key = request.session.read().await.segments[&50].cache_key.clone();
        request.segment_cache.write_temp_and_commit(&key, std::io::Cursor::new(raw_media(777))).await.unwrap();
    }
    let candidate = origin.fetched(false, 434);
    mark_full_burst_ready_for_switch_staging(&request.session, &candidate).await;
    let result = super::super::commit_manifest_recovery_candidate(
        &request,
        candidate,
        HlsManifestCommitAcceptanceMode::AllowVerifiedContentAnchorHostSwitchCandidate,
    )
    .await;
    assert!(
        result.is_ok(),
        "the same origin occurrence 437 must prove the overlap: {:?}; requests={:?}",
        result.err(),
        origin.media_requests()
    );
    let requests = origin.media_requests();
    assert!(
        requests[0].ends_with(&format!("/media/{}?fixture=1", media_name(&origin.state, 437))),
        "probe must precede continuation: {requests:?}"
    );
    assert!(requests.iter().any(|p| p.ends_with(&format!("/media/{}?fixture=1", media_name(&origin.state, 438)))));
    let state = request.session.read().await;
    assert_eq!(state.segments[&51].origin_key.host_local_sequence, 438);
    assert_eq!(state.segments[&52].origin_key.host_local_sequence, 439);
    assert!(matches!(state.segments[&51].status, SegmentCacheStatus::Ready { .. }));
    println!("names_opaque={opaque} anchor_origin=437 anchor_proxy=50 candidate_index=3 first_continuation=438 requests={requests:?}");
}

#[tokio::test]
async fn content_anchor_matches_reference_sequence_numeric_names() { matching_overlap(false).await; }
#[tokio::test]
async fn content_anchor_matches_reference_sequence_opaque_names_and_original_bytes() { matching_overlap(true).await; }

#[tokio::test]
async fn progressing_pinned_response_commits_in_last_shared_attempt() {
    let origin = Origin::start(false).await;
    let mut request = warm(&origin).await;
    origin.state.corrupt_probe.store(true, Ordering::Release);
    origin.state.pinned_after.store(5, Ordering::Release);
    request.attempt = Some(Arc::new(crate::manifest_fetch::HlsManifestAttemptContext::new(
        request.manifest_commit_requirement,
        true,
        None,
    )));
    let context = manifest_fetch_context(&request, Some(8));
    let result = retry_hls_origin_manifest_recovery_chain(
        &context,
        test_manifest_origin_binding(request.origin_entry.url().clone()),
        None,
        None,
        HlsManifestAcceptanceTrigger::Observe,
        HlsManifestCommitAcceptanceMode::StrictPinnedHost,
        |fetched, mode| super::super::commit_manifest_recovery_candidate(&request, fetched, mode),
    )
    .await;
    assert!(result.is_ok(), "paid pinned progress must not require another burst: {:?}", result.err());
    assert_eq!(request.attempt.as_ref().unwrap().logical_attempts(), 5);
    assert_eq!(request.attempt.as_ref().unwrap().remaining_attempts(), 0);
    assert_eq!(
        request.session.read().await.origin_control.acceptance_episode.as_ref().unwrap().full_bursts_completed,
        1
    );
    assert_eq!(origin.state.manifest_calls.load(Ordering::Acquire), 6);
    assert_eq!(request.session.read().await.origin_seq_highwater, Some(441));
    assert_eq!(
        request.session.read().await.origin_control.acceptance_episode.as_ref().unwrap().state,
        crate::manifest_acceptance::HlsManifestAcceptanceState::Completed
    );
}

#[tokio::test]
async fn resumed_alternative_retries_the_current_reference_after_transport_failure() {
    let origin = Origin::start(false).await;
    let mut request = warm(&origin).await;
    request.retry_policy = RetryPolicy { jitter_max_ms: 0, ..RetryPolicy::default() };
    origin.state.fail_probe.store(true, Ordering::Release);
    request.attempt = Some(Arc::new(crate::manifest_fetch::HlsManifestAttemptContext::new(
        request.manifest_commit_requirement,
        true,
        None,
    )));
    request.session.write().await.origin_refresh.mark_failure(request.now_ms.saturating_sub(100));
    let context = manifest_fetch_context(&request, Some(8));
    let binding = test_manifest_origin_binding(request.origin_entry.url().clone());
    let first = retry_hls_origin_manifest_recovery_chain(
        &context,
        binding,
        None,
        None,
        HlsManifestAcceptanceTrigger::Observe,
        HlsManifestCommitAcceptanceMode::StrictPinnedHost,
        |fetched, mode| super::super::commit_manifest_recovery_candidate(&request, fetched, mode),
    )
    .await;
    assert!(first.is_err());
    {
        let mut state = request.session.write().await;
        state.origin_control.qualification.record_failure();
        state.origin_refresh.mark_failure(super::super::current_time_millis());
        assert_eq!(state.origin_epoch, 0);
        assert!(matches!(state.segments[&50].status, SegmentCacheStatus::Ready { .. }));
    }
    let retry_at = request.session.read().await.origin_refresh.next_fetch_allowed_at_ms;
    let delay = retry_at.saturating_sub(super::super::current_time_millis());
    assert!(!request.session.read().await.origin_refresh.is_due(retry_at - 1));
    assert!(delay > 0, "the existing failed-round backoff must remain in force");
    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    assert!(super::super::current_time_millis() >= retry_at);
    println!("failed_round_attempts=5 existing_backoff_ms={delay} retry_due=true next_manifest_head=435");
    origin.state.fail_probe.store(false, Ordering::Release);
    origin.state.head.store(435, Ordering::Release);
    request.attempt.as_ref().unwrap().finish(true);
    request.attempt = Some(Arc::new(crate::manifest_fetch::HlsManifestAttemptContext::new(
        request.manifest_commit_requirement,
        true,
        None,
    )));
    let before = origin.media_requests().len();
    let context = manifest_fetch_context(&request, Some(8));
    let previous = request.session.read().await.origin_control.acceptance_episode.as_ref().unwrap().clone();
    request.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::RecoveryRequired;
    let second = super::super::short_manifest::resume_qualification(&request, &context).await.unwrap();
    {
        let state = request.session.read().await;
        let episode = state.origin_control.acceptance_episode.as_ref().unwrap();
        assert_eq!(episode.generation, previous.generation);
        assert_eq!(episode.timing(), previous.timing());
        assert_eq!(episode.full_bursts_completed, 1);
        assert_eq!(episode.trigger(), HlsManifestAcceptanceTrigger::RecoveryRequired);
    }
    assert!(second.is_ok(), "a completed full burst must permit the due bound probe: {:?}", second.err());
    assert!(origin.media_requests().len() > before);
    assert!(request.attempt.as_ref().unwrap().logical_attempts() <= 2);
    assert_eq!(request.session.read().await.origin_epoch, 1);
    assert!(request.session.read().await.origin_is_qualified());
    assert_eq!(
        request.session.read().await.origin_control.acceptance_episode.as_ref().unwrap().state,
        crate::manifest_acceptance::HlsManifestAcceptanceState::Completed
    );
}

fn media_context(request: &OriginRefreshRequest) -> SegmentFetchContext {
    SegmentFetchContext {
        session: Arc::clone(&request.session),
        segment_cache: Arc::clone(&request.segment_cache),
        segment_repair: Arc::clone(&request.segment_repair),
        repair_access_lease_id: None,
        headers: HttpHeaders::new(),
        origin_provider_session_headers: HttpHeaders::new(),
        client: request.client.clone(),
        no_redirect_client: request.no_redirect_client.clone(),
        use_manual_redirects: false,
        origin_io: None,
    }
}

fn anchor(session: &crate::HlsSession) -> super::super::origin_probe::HlsOriginContentAnchor {
    super::super::origin_probe::HlsOriginContentEvidence::latest_ready(session).unwrap()
}

async fn discover_438(request: &OriginRefreshRequest, origin: &Origin) {
    let fetched = origin.fetched(true, 433);
    let mut session = request.session.write().await;
    commit_fetched_manifest(&mut session, &fetched, request, request.now_ms).unwrap();
    assert_eq!(session.origin_seq_highwater, Some(438));
}

async fn fetch_438(request: &OriginRefreshRequest) {
    let outcome = request
        .segment_worker_pool
        .demand_fetch_and_wait(
            media_context(request),
            &crate::HlsSegmentFile { proxy_seq: 51, extension: "ts".into() },
            request.now_ms + 1,
        )
        .await;
    assert_eq!(outcome, crate::segment_fetcher::SegmentDemandFetchOutcome::Ready);
}

#[tokio::test]
async fn latest_successful_ready_commit_advances_before_another_client_request() {
    let origin = Origin::start(false).await;
    let request = warm(&origin).await;
    discover_438(&request, &origin).await;
    for status in [
        SegmentCacheStatus::Discovered,
        SegmentCacheStatus::Fetching { priority: crate::SegmentFetchPriority::Demand, started_at_ms: request.now_ms },
        SegmentCacheStatus::FailedRetryable { failed_at_ms: request.now_ms, retry_after_ms: request.now_ms + 1 },
    ] {
        let mut state = request.session.write().await;
        state.segments.get_mut(&51).unwrap().status = status;
        assert_eq!(anchor(&state).origin_key.host_local_sequence, 437);
    }
    request.session.write().await.segments.get_mut(&51).unwrap().status = SegmentCacheStatus::Discovered;
    fetch_438(&request).await;
    let state = request.session.read().await;
    let current = anchor(&state);
    assert_eq!((current.origin_key.host_local_sequence, current.proxy_seq), (438, 51));
    assert_eq!(current.digest, sha2::Sha256::digest(raw_media(438)).as_slice());
    println!("ready_anchor=437/50 discovered_fetching_failed=438 unchanged=true next_successful_commit=438/51");
}

#[tokio::test]
async fn content_anchor_rejects_missing_different_and_noncomparable_references() {
    for case in ["different", "missing", "range", "map", "encryption", "missing-digest", "missing-cache-revision"] {
        let origin = Origin::start(false).await;
        let request = warm(&origin).await;
        let mut candidate = origin.fetched(false, if case == "missing" { 438 } else { 434 });
        if case == "different" {
            origin.state.corrupt_probe.store(true, Ordering::Release);
        }
        if case == "range" {
            candidate.body = candidate.body.replace(
                &format!("{}/media/437.ts?fixture=1", origin.base(false)),
                &format!("#EXT-X-BYTERANGE:188@0\n{}/media/437.ts?fixture=1", origin.base(false)),
            );
        }
        if case == "map" {
            candidate.body = candidate
                .body
                .replace("#EXT-X-MEDIA-SEQUENCE:434", "#EXT-X-MEDIA-SEQUENCE:434\n#EXT-X-MAP:URI=\"init.mp4\"");
        }
        if case == "encryption" {
            candidate.body = candidate.body.replace(
                "#EXT-X-MEDIA-SEQUENCE:434",
                "#EXT-X-MEDIA-SEQUENCE:434\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"",
            );
        }
        if case == "missing-digest" {
            request.session.write().await.origin_content =
                super::super::origin_probe::HlsOriginContentEvidence::default();
        }
        if case == "missing-cache-revision" {
            let key = request.session.read().await.segments[&50].cache_key.clone();
            request.segment_cache.delete(&key).await.unwrap();
        }
        mark_full_burst_ready_for_switch_staging(&request.session, &candidate).await;
        let result = super::super::commit_manifest_recovery_candidate(
            &request,
            candidate,
            HlsManifestCommitAcceptanceMode::AllowVerifiedContentAnchorHostSwitchCandidate,
        )
        .await;
        assert!(result.is_err(), "no false byte proof: {case}");
        let state = request.session.read().await;
        assert_eq!(state.origin_epoch, 0, "{case}");
        assert_eq!(state.proxy_next_seq, Some(51), "{case}");
        assert!(!origin.media_requests().iter().any(|p| p.contains("/438.ts")), "no continuation before proof: {case}");
        println!("negative={case} no_false_commit=true requests={}", origin.media_requests().len());
    }
}

async fn invalidated_probe(pause_sequence: u64, change_anchor: bool) {
    let origin = Origin::start(false).await;
    let request = Arc::new(warm(&origin).await);
    discover_438(&request, &origin).await;
    let candidate = origin.fetched(false, 434);
    mark_full_burst_ready_for_switch_staging(&request.session, &candidate).await;
    origin.state.pause_sequence.store(pause_sequence, Ordering::Release);
    let captured = Arc::clone(&request);
    let work = tokio::spawn(async move {
        super::super::commit_manifest_recovery_candidate(
            &captured,
            candidate,
            HlsManifestCommitAcceptanceMode::AllowVerifiedContentAnchorHostSwitchCandidate,
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), origin.state.entered.notified()).await.unwrap();
    let key = {
        let mut session = request.session.write().await;
        assert_eq!(
            anchor(&session).origin_key.host_local_sequence,
            437,
            "unconfirmed staging never advances the reference"
        );
        assert!(
            session.segments[&50].access.active_readers() > 0,
            "reference stays pinned through continuation staging"
        );
        // Make the old reference otherwise obsolete; an active reader still protects it.
        session.publishable_origin_head_proxy_seq = Some(51);
        session.segments[&50].cache_key.clone()
    };
    let gc = request.hls_proxy.garbage_collector();
    let mut policy = (*gc.policy()).clone();
    policy.cache_duration_ms = 0;
    policy.session_idle_timeout_ms = u64::MAX;
    gc.update_config(policy, gc.rewrite_secret_fingerprint());
    gc.run_once(request.now_ms + 100_000).await.unwrap();
    assert!(request.segment_cache.metadata(&key).await.unwrap().is_some(), "GC retains exact pinned reference file");
    if change_anchor {
        fetch_438(&request).await;
    } else {
        request.session.write().await.activity.origin_work_generation += 1;
    }
    origin.state.release.add_permits(1);
    let result = work.await.unwrap();
    assert!(
        matches!(
            result,
            Err(HlsManifestCommitError::TimelineRejected {
                reason: HlsManifestRejectLogReason::StagedSwitchInvalidated
            })
        ),
        "stale proof: {:?}",
        result.err()
    );
    let state = request.session.read().await;
    assert_eq!(state.origin_epoch, 0);
    assert_eq!(state.proxy_next_seq, Some(52));
    assert_eq!(state.segments[&50].access.active_readers(), 0);
    if change_anchor {
        assert_eq!(anchor(&state).origin_key.host_local_sequence, 438);
    }
    println!(
        "pause_origin={pause_sequence} anchor_advanced={change_anchor} gc_pin_retained=true stale_commit_rejected=true"
    );
}

#[tokio::test]
async fn anchor_advance_during_probe_rejects_the_stale_switch() { invalidated_probe(437, true).await; }
#[tokio::test]
async fn generation_change_during_continuation_keeps_reference_pinned_and_rejects_commit() {
    invalidated_probe(439, false).await;
}

#[tokio::test]
async fn permanent_probe_failure_keeps_followups_reduced_and_bounded() {
    let origin = Origin::start(false).await;
    let mut request = warm(&origin).await;
    origin.state.fail_probe.store(true, Ordering::Release);
    let binding = test_manifest_origin_binding(request.origin_entry.url().clone());
    let mut generation = None;
    for round in 0..4 {
        request.attempt = Some(Arc::new(crate::manifest_fetch::HlsManifestAttemptContext::new(
            request.manifest_commit_requirement,
            true,
            None,
        )));
        let before = origin.state.manifest_calls.load(Ordering::Acquire);
        let context = manifest_fetch_context(&request, Some(8));
        let result = retry_hls_origin_manifest_recovery_chain(
            &context,
            binding.clone(),
            None,
            None,
            HlsManifestAcceptanceTrigger::Observe,
            HlsManifestCommitAcceptanceMode::StrictPinnedHost,
            |fetched, mode| super::super::commit_manifest_recovery_candidate(&request, fetched, mode),
        )
        .await;
        assert!(result.is_err());
        let mut state = request.session.write().await;
        state.origin_control.qualification.record_failure();
        let episode = state.origin_control.acceptance_episode.as_ref().unwrap();
        assert_eq!(*generation.get_or_insert(episode.generation), episode.generation);
        assert_eq!(episode.full_bursts_completed, 1);
        let manifests = origin.state.manifest_calls.load(Ordering::Acquire) - before;
        assert_eq!(manifests, if round == 0 { 6 } else { 1 });
        assert_eq!(request.attempt.as_ref().unwrap().logical_attempts(), if round == 0 { 5 } else { 1 });
        println!(
            "failed_round={round} manifest_requests={manifests} full_bursts=1 origin_epoch={} completion=bounded-held",
            state.origin_epoch
        );
        drop(state);
        request.attempt.as_ref().unwrap().finish(true);
    }
}

#[tokio::test]
async fn generation_change_during_reference_probe_rejects_the_stale_switch() { invalidated_probe(437, false).await; }

mod dedup;

mod gap_repair;
mod origin_updates;
