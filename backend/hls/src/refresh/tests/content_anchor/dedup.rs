use super::*;

struct ProbeState {
    port: u16,
    two_hosts: bool,
    manifests: AtomicUsize,
    mode: AtomicUsize,
    pause: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
    requests: Mutex<Vec<String>>,
}

struct ProbeOrigin {
    state: Arc<ProbeState>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for ProbeOrigin {
    fn drop(&mut self) { self.task.abort(); }
}
impl ProbeOrigin {
    async fn start(two_hosts: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = Arc::new(ProbeState {
            port: listener.local_addr().unwrap().port(),
            two_hosts,
            manifests: AtomicUsize::new(0),
            mode: AtomicUsize::new(0),
            pause: AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let app = Router::new().fallback(probe_origin).with_state(Arc::clone(&state));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { state, task }
    }
    fn client(&self, redirects: bool) -> reqwest::Client {
        let address = format!("127.0.0.1:{}", self.state.port).parse().unwrap();
        reqwest::Client::builder()
            .no_proxy()
            .resolve("entry.probe.test", address)
            .resolve("b.probe.test", address)
            .resolve("c.probe.test", address)
            .redirect(if redirects { reqwest::redirect::Policy::default() } else { reqwest::redirect::Policy::none() })
            .build()
            .unwrap()
    }
    fn fetched(&self, host: &str, index: usize, head: u64) -> FetchedOriginManifest {
        let mut body = format!("#EXTM3U\n#EXT-X-TARGETDURATION:8\n#EXT-X-MEDIA-SEQUENCE:{head}\n");
        for seq in head..head + 6 {
            writeln!(body, "#EXTINF:8,\nhttp://{host}:{}/media/{seq}.ts?fixture=1", self.state.port).unwrap();
        }
        let mut fetched = fetched_manifest(&body);
        fetched.final_manifest_url = format!("http://{host}:{}/manifest/{index}.m3u8", self.state.port);
        fetched.resolved_request_url = fetched.final_manifest_url.clone();
        fetched.redirect_host = Some(host.to_string());
        fetched
    }
    fn gets(&self) -> Vec<String> {
        self.state.requests.lock().unwrap().iter().filter(|line| line.contains(".ts")).cloned().collect()
    }
}

async fn probe_origin(State(state): State<Arc<ProbeState>>, headers: HttpHeaders, uri: Uri) -> Response {
    let host = headers.get(header::HOST).unwrap().to_str().unwrap();
    state.requests.lock().unwrap().push(format!("{host} {uri}"));
    if uri.path() == "/entry.m3u8" {
        let index = state.manifests.fetch_add(1, Ordering::AcqRel);
        let host = if state.two_hosts && index >= 7 { "c.probe.test" } else { "b.probe.test" };
        return (
            HttpStatus::FOUND,
            [(header::LOCATION, format!("http://{host}:{}/manifest/{index}.m3u8", state.port))],
        )
            .into_response();
    }
    if std::path::Path::new(uri.path()).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("m3u8")) {
        let mut body = "#EXTM3U\n#EXT-X-TARGETDURATION:8\n#EXT-X-MEDIA-SEQUENCE:434\n".to_string();
        for seq in 434..440 {
            writeln!(body, "#EXTINF:8,\nhttp://{host}/media/{seq}.ts?fixture=1").unwrap();
        }
        return body.into_response();
    }
    if std::path::Path::new(uri.path()).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("ts")) {
        if state.pause.load(Ordering::Acquire) {
            state.entered.notify_one();
            state.release.acquire().await.unwrap().forget();
        }
        let mode = state.mode.load(Ordering::Acquire);
        if mode == 2 {
            return HttpStatus::NOT_FOUND.into_response();
        }
        if mode == 5 {
            return HttpStatus::SERVICE_UNAVAILABLE.into_response();
        }
        let sequence = uri.path().rsplit('/').next().unwrap().strip_suffix(".ts").unwrap().parse::<u64>().unwrap();
        let matching = mode == 1 || (mode == 4 && host.starts_with("b.probe.test:"));
        let marker = if sequence != 437 || matching {
            sequence
        } else if host.starts_with("b.probe.test:") {
            998
        } else {
            999
        };
        let body = raw_media(marker);
        if mode == 3 {
            return Response::builder()
                .status(HttpStatus::OK)
                .header(header::CONTENT_TYPE, "video/mp2t")
                .header(header::CONTENT_LENGTH, body.len() + 188)
                .body(Body::from_stream(futures::stream::iter([Ok::<_, std::io::Error>(bytes::Bytes::from(body))])))
                .unwrap();
        }
        if let Some(range) = headers
            .get(header::RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("bytes="))
        {
            let (start, end) = range.split_once('-').unwrap();
            let start = start.parse::<usize>().unwrap();
            let end = end.parse::<usize>().unwrap();
            return Response::builder()
                .status(HttpStatus::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, "video/mp2t")
                .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{}", body.len()))
                .body(Body::from(body[start..=end].to_vec()))
                .unwrap();
        }
        return ([(header::CONTENT_TYPE, "video/mp2t")], Body::from(body)).into_response();
    }
    HttpStatus::NOT_FOUND.into_response()
}

async fn probe_burst(two_hosts: bool, early_match: bool, qualification: bool) {
    let baseline = Origin::start(false).await;
    let mut request = warm(&baseline).await;
    let origin = ProbeOrigin::start(two_hosts).await;
    if early_match {
        origin.state.mode.store(4, Ordering::Release);
    }
    request.client = origin.client(true);
    request.no_redirect_client = origin.client(false);
    request.origin_entry =
        LiveHlsOriginEntry::parse(&format!("http://entry.probe.test:{}/entry.m3u8", origin.state.port)).unwrap();
    request.manifest_recovery_burst.level = shared::model::HlsManifestRecoveryBurstLevel::Beast;
    let attempt = Arc::new(crate::manifest_fetch::HlsManifestAttemptContext::new(
        request.manifest_commit_requirement,
        true,
        None,
    ));
    for _ in 0..4 {
        assert!(attempt.begin_logical_attempt());
    }
    request.attempt = Some(attempt);
    if qualification {
        let mut session = request.session.write().await;
        session.origin_control.target_duration_qualification =
            Some(crate::short_manifest::HlsTargetDurationQualification::new(&session, shared::model::Secs::new(1)));
    }
    let context = manifest_fetch_context(&request, Some(8));
    let result = retry_hls_origin_manifest_recovery_chain(
        &context,
        test_manifest_origin_binding(request.origin_entry.url().clone()),
        None,
        None,
        HlsManifestAcceptanceTrigger::Observe,
        HlsManifestCommitAcceptanceMode::StrictPinnedHost,
        |fetched, mode| crate::refresh::commit_manifest_recovery_candidate(&request, fetched, mode),
    )
    .await;
    assert_eq!(
        result.is_ok(),
        early_match,
        "only the proven matching content can be accepted: {:?}",
        result.as_ref().err()
    );
    let requests = origin.gets();
    let b = requests.iter().filter(|request| request.starts_with("b.probe.test:")).count();
    let c = requests.iter().filter(|request| request.starts_with("c.probe.test:")).count();
    println!(
        "two_hosts={two_hosts} manifests={} probe_gets={} b_gets={b} c_gets={c}",
        origin.state.manifests.load(Ordering::Acquire),
        requests.len()
    );
    assert_eq!(origin.state.manifests.load(Ordering::Acquire), 12);
    if early_match {
        assert_eq!((b, c), (2, 0), "one reference, one continuation, no speculative C probe");
        assert!(requests[0].contains("/437.ts?"));
        assert!(requests[1].contains("/438.ts?"));
        assert_eq!(request.session.read().await.origin_epoch, 1);
        println!("early_success=true manifest_candidates=12 anchor_gets=1 continuation_gets=1 unneeded_host_gets=0");
    } else {
        assert_eq!((b, c), if two_hosts { (1, 1) } else { (1, 0) }, "one GET per complete origin probe binding");
        let session = request.session.read().await;
        assert_eq!(session.origin_epoch, 0);
        assert_eq!(session.proxy_next_seq, Some(51));
    }
}

#[tokio::test]
async fn twelve_candidates_load_only_the_two_distinct_mismatching_resources() { probe_burst(true, false, true).await; }
#[tokio::test]
async fn twelve_manifest_urls_share_the_same_origin_probe() { probe_burst(false, false, true).await; }

#[tokio::test]
async fn early_success_does_not_probe_the_other_resource() { probe_burst(true, true, true).await; }

#[tokio::test]
async fn mismatching_candidates_continue_without_target_duration_preference() { probe_burst(true, false, false).await; }

use crate::refresh::{
    anchor_probe::{HlsAnchorProbeRoundGuard, ProbeIdentity},
    switch_staging::{alternative_switch_fetch_context, verify_content_anchor},
};
use tuliprox_parser::hls::origin_manifest::ParsedByteRange;

async fn prepared() -> (Origin, ProbeOrigin, OriginRefreshRequest) {
    let baseline = Origin::start(false).await;
    let mut request = warm(&baseline).await;
    let origin = ProbeOrigin::start(false).await;
    request.client = origin.client(true);
    request.no_redirect_client = origin.client(false);
    (baseline, origin, request)
}

async fn verify(request: &OriginRefreshRequest, fetched: &FetchedOriginManifest) -> Result<(), HlsManifestCommitError> {
    mark_full_burst_ready_for_switch_staging(&request.session, fetched).await;
    let _round = HlsAnchorProbeRoundGuard::install(&request.session, 12).await;
    verify_content_anchor(request, fetched).await.map(|_| ())
}

async fn resource(
    request: &OriginRefreshRequest,
    owner: &HlsAnchorProbeRoundGuard,
    url: String,
    range: Option<ParsedByteRange>,
) -> Result<[u8; 32], ()> {
    let context = alternative_switch_fetch_context(request, &fetched_manifest(""));
    let (identity, key) = {
        let session = request.session.read().await;
        let reference = anchor(&session);
        (ProbeIdentity::new(&session, &reference, &context, &url, range)?, reference.cache_key)
    };
    owner
        .0
        .digest(
            identity,
            crate::refresh::anchor_probe::fetch_digest(
                request.clone(),
                context,
                key,
                crate::OriginSegmentFetchRef { resolved_origin_url: url, byte_range: range, valid_until_ms: None },
            ),
        )
        .await
}

#[tokio::test]
async fn overlapping_candidates_share_one_fetch_and_keep_their_own_proofs() {
    let (_baseline, origin, request) = prepared().await;
    origin.state.mode.store(1, Ordering::Release);
    origin.state.pause.store(true, Ordering::Release);
    let fetched = origin.fetched("b.probe.test", 0, 434);
    mark_full_burst_ready_for_switch_staging(&request.session, &fetched).await;
    let owner = HlsAnchorProbeRoundGuard::install(&request.session, 12).await;
    let captured = Arc::new(request);
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..12 {
        let request = Arc::clone(&captured);
        let fetched = fetched.clone();
        tasks.spawn(async move { verify_content_anchor(&request, &fetched).await });
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), origin.state.entered.notified()).await.unwrap();
    assert_eq!(origin.gets().len(), 1);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while captured.session.read().await.segments[&50].access.active_readers() < 12 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    origin.state.release.add_permits(1);
    let mut proofs = Vec::new();
    while let Some(result) = tasks.join_next().await {
        proofs.push(result.unwrap().unwrap());
    }
    assert_eq!(proofs.len(), 12);
    assert_eq!(origin.gets().len(), 1);
    let generation =
        crate::refresh::switch_staging::switch_staging_generation(&*captured.session.read().await).unwrap();
    drop(owner);
    assert!(proofs.iter().all(|proof| !proof.matches(&generation)), "a closed round cannot publish an old proof");
    drop(proofs);
    assert_eq!(captured.session.read().await.segments[&50].access.active_readers(), 0);
    assert!(!captured.segment_cache.has_active_temp_files());
    println!(
        "parallel_waiters=12 probe_gets=1 independent_proofs=12 closed_round_invalidates=true raw_files_released=true"
    );
}

#[tokio::test]
async fn round_end_cancels_running_shared_probes_and_releases_the_reference() {
    let (_baseline, origin, request) = prepared().await;
    origin.state.pause.store(true, Ordering::Release);
    let fetched = origin.fetched("b.probe.test", 0, 434);
    mark_full_burst_ready_for_switch_staging(&request.session, &fetched).await;
    let owner = HlsAnchorProbeRoundGuard::install(&request.session, 12).await;
    let captured = Arc::new(request);
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..6 {
        let request = Arc::clone(&captured);
        let fetched = fetched.clone();
        tasks.spawn(async move { verify_content_anchor(&request, &fetched).await.is_err() });
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), origin.state.entered.notified()).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while captured.session.read().await.segments[&50].access.active_readers() < 6 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(owner);
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while let Some(result) = tasks.join_next().await {
            assert!(result.unwrap());
        }
    })
    .await
    .unwrap();
    assert_eq!(origin.gets().len(), 1);
    assert_eq!(captured.session.read().await.segments[&50].access.active_readers(), 0);
    assert!(!captured.segment_cache.has_active_temp_files());
    origin.state.release.add_permits(1);
    println!("cancelled_waiters=6 probe_gets=1 all_waiters_released=true cache_pins=0");
}

#[tokio::test]
async fn full_transport_identity_separates_origin_resources() {
    let (_baseline, origin, mut request) = prepared().await;
    let other_port = ProbeOrigin::start(false).await;
    let owner = HlsAnchorProbeRoundGuard::install(&request.session, 12).await;
    let base = format!("http://b.probe.test:{}/media/437.ts?fixture=1", origin.state.port);
    let targets = [
        base.clone(),
        base.replace("b.probe.test", "c.probe.test"),
        format!("http://b.probe.test:{}/media/437.ts?fixture=1", other_port.state.port),
        base.replace("/media/", "/other/"),
        base.replace("fixture=1", "fixture=2"),
    ];
    for target in targets {
        resource(&request, &owner, target, None).await.unwrap();
    }
    for offset in [0, 188] {
        resource(&request, &owner, base.clone(), Some(ParsedByteRange { offset, length: 188 })).await.unwrap();
    }
    request.headers.insert("accept-language", axum::http::HeaderValue::from_static("en"));
    resource(&request, &owner, base.clone(), None).await.unwrap();
    request.headers.insert("accept-language", axum::http::HeaderValue::from_static("de"));
    resource(&request, &owner, base.clone(), None).await.unwrap();
    let before = origin.gets().len() + other_port.gets().len();
    resource(&request, &owner, base.clone(), None).await.unwrap();
    assert_eq!(origin.gets().len() + other_port.gets().len(), before);
    request.origin_provider_session_headers.insert("cookie", axum::http::HeaderValue::from_static("fixture=a"));
    resource(&request, &owner, base.clone(), None).await.unwrap();
    {
        let mut session = request.session.write().await;
        let mut binding = crate::HlsOriginAccountBinding::new(
            Arc::from("fixture"),
            Arc::from("account-a"),
            &session.proxy_session_id,
            request.now_ms,
        );
        binding.generation = 2;
        session.origin_account_binding = Some(binding);
    }
    resource(&request, &owner, base.clone(), None).await.unwrap();
    request.session.write().await.origin_account_binding.as_mut().unwrap().account_name = Arc::from("account-b");
    resource(&request, &owner, base, None).await.unwrap();
    assert_eq!(origin.gets().len() + other_port.gets().len(), 12);
    assert_eq!(other_port.gets().len(), 1);
    println!("distinct_transport_bindings=12 segment_gets=12 dimensions=host,port,path,query,range,headers,provider-cookie,account duplicates_added_gets=0");
}

#[tokio::test]
async fn transport_failures_are_shared_and_can_retry_after_existing_backoff() {
    let (_baseline, origin, mut request) = prepared().await;
    let fetched = origin.fetched("b.probe.test", 0, 434);
    origin.state.mode.store(2, Ordering::Release);
    mark_full_burst_ready_for_switch_staging(&request.session, &fetched).await;
    let owner = HlsAnchorProbeRoundGuard::install(&request.session, 12).await;
    for _ in 0..12 {
        assert!(verify_content_anchor(&request, &fetched).await.is_err());
    }
    assert_eq!(origin.gets().len(), 1, "404 is shared but proves no byte mismatch");
    drop(owner);
    {
        let mut session = request.session.write().await;
        session.origin_refresh.mark_failure(request.now_ms.saturating_sub(1));
        session.origin_refresh.mark_failure(request.now_ms);
        assert!(!session.origin_refresh.is_due(request.now_ms + 499));
        assert!(session.origin_refresh.is_due(request.now_ms + 500));
    }
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_millis(500)).await;
    tokio::time::resume();
    request.now_ms += 500;
    origin.state.mode.store(1, Ordering::Release);
    assert!(verify(&request, &fetched).await.is_ok());
    assert_eq!(origin.gets().len(), 2);
    assert!(!request.segment_cache.has_active_temp_files());
    println!("transport_failure=404 first_round_waiters=12 first_round_gets=1 backoff_ms=500 next_round_gets=1 result=matching");
}

#[tokio::test]
async fn incomplete_bodies_are_not_persisted_as_content_mismatches() {
    let (_baseline, origin, request) = prepared().await;
    let fetched = origin.fetched("b.probe.test", 0, 434);
    origin.state.mode.store(3, Ordering::Release);
    assert!(verify(&request, &fetched).await.is_err());
    let incomplete = origin.gets().len();
    assert!((1..=5).contains(&incomplete));
    origin.state.mode.store(1, Ordering::Release);
    assert!(verify(&request, &fetched).await.is_ok());
    assert_eq!(origin.gets().len(), incomplete + 1);
    println!("incomplete_body_gets={incomplete} transport_policy_limit=5 final_mismatch=false later_matching_gets=1");
}

async fn technical_recommit(request: &OriginRefreshRequest) {
    let mut session = request.session.write().await;
    session.activity.origin_work_generation += 1;
    if let Some(account) = &mut session.origin_account_binding {
        account.generation += 1;
    }
    if let SegmentCacheStatus::Ready { ready_at_ms, .. } = &mut session.segments.get_mut(&50).unwrap().status {
        *ready_at_ms += 1;
    }
    let entry = session.segments[&50].clone();
    let generation = session.activity.origin_work_generation;
    session.origin_content.record(&entry, generation, sha2::Sha256::digest(raw_media(437)).into());
}

#[tokio::test]
async fn complete_content_mismatch_remains_final_across_rounds_and_technical_revisions() {
    let (_baseline, origin, mut request) = prepared().await;
    {
        let mut session = request.session.write().await;
        session.origin_account_binding = Some(crate::HlsOriginAccountBinding::new(
            Arc::from("fixture"),
            Arc::from("account-a"),
            &session.proxy_session_id,
            request.now_ms,
        ));
    }
    for (index, head) in [434, 435, 436, 437].into_iter().enumerate() {
        if index > 0 {
            technical_recommit(&request).await;
            origin.state.mode.store(1, Ordering::Release);
        }
        let fetched = origin.fetched("b.probe.test", index, head);
        mark_full_burst_ready_for_switch_staging(&request.session, &fetched).await;
        request.attempt = Some(Arc::new(crate::manifest_fetch::HlsManifestAttemptContext::new(
            request.manifest_commit_requirement,
            true,
            None,
        )));
        let result = crate::refresh::commit_manifest_recovery_candidate(
            &request,
            fetched,
            HlsManifestCommitAcceptanceMode::AllowVerifiedContentAnchorHostSwitchCandidate,
        )
        .await;
        assert!(result.is_err(), "a later matching body cannot undo the proven mismatch");
        assert_eq!(origin.gets().len(), 1);
        let session = request.session.read().await;
        assert_eq!(session.origin_epoch, 0);
        assert_eq!(session.proxy_next_seq, Some(51));
        println!("recovery_round={index} acceptance_generation={} origin_generation={} manifest_head={head} total_probe_gets=1 decision=final-mismatch",session.origin_control.acceptance_generation.0,session.activity.origin_work_generation);
    }
}

#[tokio::test]
async fn genuinely_changed_bindings_are_independent_but_old_mismatch_is_retained() {
    let (_baseline, origin, request) = prepared().await;
    let original = origin.fetched("b.probe.test", 0, 434);
    assert!(verify(&request, &original).await.is_err());
    assert_eq!(origin.gets().len(), 1);
    origin.state.mode.store(1, Ordering::Release);
    let other = origin.fetched("c.probe.test", 1, 434);
    assert!(verify(&request, &other).await.is_ok());
    assert_eq!(origin.gets().len(), 2);
    let mut query = original.clone();
    query.body = query.body.replace("fixture=1", "fixture=2");
    assert!(verify(&request, &query).await.is_ok());
    assert_eq!(origin.gets().len(), 3);
    {
        let entry = request.session.read().await.segments[&50].clone();
        let bytes = raw_media(998);
        let metadata = request
            .segment_cache
            .write_temp_and_commit(&entry.cache_key, std::io::Cursor::new(bytes.clone()))
            .await
            .unwrap();
        let mut session = request.session.write().await;
        session.segments.get_mut(&50).unwrap().status =
            SegmentCacheStatus::Ready { content_length: metadata.size, ready_at_ms: request.now_ms + 1 };
        let entry = session.segments[&50].clone();
        let generation = session.activity.origin_work_generation;
        session.origin_content.record(&entry, generation, sha2::Sha256::digest(bytes).into());
    }
    origin.state.mode.store(0, Ordering::Release); // Original resource has stable 998 bytes again, matching the new anchor.
    assert!(verify(&request, &original).await.is_ok());
    assert_eq!(origin.gets().len(), 4);
    let old = raw_media(437);
    let key = request.session.read().await.segments[&50].cache_key.clone();
    request.segment_cache.write_temp_and_commit(&key, std::io::Cursor::new(old)).await.unwrap();
    technical_recommit(&request).await;
    assert!(verify(&request, &original).await.is_err());
    assert_eq!(origin.gets().len(), 4);
    println!("original_mismatch_gets=1 other_host_gets=1 other_query_gets=1 changed_confirmed_content_gets=1 restored_old_binding_gets=0");
}

#[tokio::test]
async fn retryable_errors_share_the_bounded_transport_policy() {
    let (_baseline, origin, mut request) = prepared().await;
    request.segment_worker_pool =
        Arc::new(crate::HlsSegmentWorkerPool::new(crate::segment_fetcher::SegmentFetchPolicy {
            retry_delays_ms: [0; 5],
            retry_jitter_max_ms: 0,
            ..Default::default()
        }));
    origin.state.mode.store(5, Ordering::Release);
    let fetched = origin.fetched("b.probe.test", 0, 434);
    mark_full_burst_ready_for_switch_staging(&request.session, &fetched).await;
    let owner = HlsAnchorProbeRoundGuard::install(&request.session, 12).await;
    for _ in 0..12 {
        assert!(verify_content_anchor(&request, &fetched).await.is_err());
    }
    assert_eq!(origin.gets().len(), 5, "one shared producer, existing five-attempt policy");
    drop(owner);
    origin.state.mode.store(1, Ordering::Release);
    assert!(verify(&request, &fetched).await.is_ok());
    assert_eq!(origin.gets().len(), 6);
    assert!(!request.segment_cache.has_active_temp_files());
    println!("retryable_status=503 duplicate_candidates=12 transport_attempts=5 later_round_gets=1");
}

async fn stale_probe(mismatch: bool) {
    let (_baseline, origin, request) = prepared().await;
    origin.state.mode.store(usize::from(!mismatch), Ordering::Release);
    origin.state.pause.store(true, Ordering::Release);
    let fetched = origin.fetched("b.probe.test", 0, 434);
    mark_full_burst_ready_for_switch_staging(&request.session, &fetched).await;
    let owner = HlsAnchorProbeRoundGuard::install(&request.session, 12).await;
    let captured = request.clone();
    let candidate = fetched.clone();
    let task = tokio::spawn(async move {
        crate::refresh::commit_manifest_recovery_candidate(
            &captured,
            candidate,
            HlsManifestCommitAcceptanceMode::AllowVerifiedContentAnchorHostSwitchCandidate,
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), origin.state.entered.notified()).await.unwrap();
    request.session.write().await.activity.origin_work_generation += 1;
    origin.state.pause.store(false, Ordering::Release);
    origin.state.release.add_permits(1);
    let result = task.await.unwrap();
    assert!(matches!(
        result,
        Err(HlsManifestCommitError::TimelineRejected { reason: HlsManifestRejectLogReason::StagedSwitchInvalidated })
    ));
    drop(owner);
    assert_eq!(origin.gets().len(), 1);
    let session = request.session.read().await;
    assert_eq!(session.origin_epoch, 0);
    assert_eq!(session.proxy_next_seq, Some(51));
    drop(session);
    origin.state.mode.store(1, Ordering::Release);
    assert_eq!(verify(&request, &fetched).await.is_err(), mismatch);
    assert_eq!(origin.gets().len(), if mismatch { 1 } else { 2 });
    assert!(!request.segment_cache.has_active_temp_files());
    println!(
        "stale_probe_mismatch={mismatch} stale_commit_rejected=true later_matching_gets={} epoch_unchanged=true",
        usize::from(!mismatch)
    );
}
#[tokio::test]
async fn stale_matching_probe_cannot_commit() { stale_probe(false).await; }
#[tokio::test]
async fn stale_full_mismatch_remains_semantically_final() { stale_probe(true).await; }

#[tokio::test]
async fn other_host_switch_modes_cannot_bypass_a_final_mismatch() {
    let (_baseline, origin, request) = prepared().await;
    let fetched = origin.fetched("b.probe.test", 0, 434);
    assert!(verify(&request, &fetched).await.is_err());
    origin.state.mode.store(1, Ordering::Release);
    for mode in [
        HlsManifestCommitAcceptanceMode::AllowHeldHostSwitchCandidate,
        HlsManifestCommitAcceptanceMode::AllowVerifiedEmergencyHostSwitchCandidate,
    ] {
        mark_full_burst_ready_for_switch_staging(&request.session, &fetched).await;
        assert!(crate::refresh::commit_manifest_recovery_candidate(&request, fetched.clone(), mode).await.is_err());
    }
    assert_eq!(origin.gets().len(), 1);
    assert_eq!(request.session.read().await.origin_epoch, 0);
    println!("final_mismatch_bypasses=0 held_extra_gets=0 emergency_extra_gets=0 epoch_unchanged=true");
}
