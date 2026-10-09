use super::{
    deterministic_timeline_conflict_from_rejection, manifest_fetch_context, retry_hls_origin_manifest_recovery_chain,
    trigger_origin_refresh_sync, FetchedOriginManifest, HlsAcceptanceEpisodeTiming, HlsAcceptanceEpisodeTimingInput,
    HlsDeterministicTimelineConflict, HlsManifestAcceptanceTrigger, HlsManifestCommitAcceptanceMode,
    HlsManifestCommitError, HlsManifestCommitRequirement, HlsManifestFetchSelection, HlsManifestOriginBinding,
    HlsManifestRejectLogReason, HlsObservedRecoveryLatency, HlsOperationTimeoutMs, HlsPostRefreshRuntime,
    HlsRecoveryEtaMs, HlsRecoveryTimingPolicy, HlsRecoveryWorkload, HlsRecoveryWorkloadEnvelope,
    HlsTerminalMediaPreparationState, HlsTransitionMarginMs, LiveHlsOriginEntry, OriginManifestFetchError,
    OriginRefreshRequest, RetryPolicy,
};
use crate::{
    is_hls_provisioning_gap_segment, is_hls_provisioning_segment, manifest_fetch::fetched_effective_manifest_host,
    CacheAccessState, HlsAccessLease, HlsAccessLeaseId, HlsAccessLeaseTiming, HlsLeaseManifestSegment,
    HlsLeaseManifestSnapshot, HlsManifestAcceptanceDirective, HlsManifestCommitIdentity, HlsManifestDeliveryMode,
    HlsMapWorkerPool, HlsMediaContainer, HlsPlaybackFamilyKey, HlsProxyManager, HlsSegmentCache,
    HlsSegmentRepairManager, HlsSegmentWorkerPool, HlsSession, HlsSessionKey, OriginSegmentKey, RenderedManifest,
    RenderedManifestStoreOutcome, SegmentCacheKey, SegmentCacheStatus, SegmentEntry, SegmentFetchPolicy,
};
use arc_swap::{ArcSwap, ArcSwapOption};
use axum::http::{HeaderMap, StatusCode};
use shared::model::{ConfigPaths, HlsManifestRecoveryBurstLevel, HlsSegmentRepairMode, HlsStripMode};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{Mutex, Notify, RwLock},
};
use tuliprox_core::model::{
    AppConfig, Config, CustomStreamResponse, HlsManifestRecoveryBurstConfig, HlsSegmentRepairConfig, SourcesConfig,
    StripConfig,
};
use tuliprox_mpegts::transport_stream_buffer::TransportStreamBuffer;
use url::Url;

pub(in crate::refresh::tests) async fn retry_test_manifest_recovery_chain(
    request: &OriginRefreshRequest,
    target_url: Url,
    reject_reason: HlsManifestRejectLogReason,
) -> Result<super::super::CommittedOriginManifest, OriginManifestFetchError> {
    let fetch_context = manifest_fetch_context(request);
    retry_hls_origin_manifest_recovery_chain(
        &fetch_context,
        test_manifest_origin_binding(target_url),
        Some(reject_reason),
        None,
        HlsManifestAcceptanceTrigger::RecoveryRequired,
        HlsManifestCommitAcceptanceMode::StrictPinnedHost,
        |fetched, acceptance_mode| super::super::commit_manifest_recovery_candidate(request, fetched, acceptance_mode),
    )
    .await
}

pub(in crate::refresh::tests) fn path_has_extension(path: &str, extension: &str) -> bool {
    std::path::Path::new(path).extension().is_some_and(|actual| actual.eq_ignore_ascii_case(extension))
}

pub(in crate::refresh::tests) fn test_manifest_origin_binding(target_url: Url) -> HlsManifestOriginBinding {
    HlsManifestOriginBinding::new(target_url, None).expect("concrete HTTP test binding")
}

pub(in crate::refresh::tests) fn test_session() -> Arc<RwLock<HlsSession>> {
    Arc::new(RwLock::new(HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0)))
}

pub(in crate::refresh::tests) fn repeated_transient_manifest(segment_count: usize) -> String {
    let mut body = String::from(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n",
    );
    for _ in 0..segment_count {
        body.push_str("#EXTINF:4,\nsame.ts\n");
    }
    body
}

pub(in crate::refresh::tests) fn test_segment_repair_manager() -> Arc<HlsSegmentRepairManager> {
    Arc::new(HlsSegmentRepairManager::new(HlsSegmentRepairConfig {
        max_level: HlsSegmentRepairMode::Off,
        apply_to_first_segments: 1,
        max_parallel_repairs: 1,
        ..Default::default()
    }))
}

pub(in crate::refresh::tests) fn test_app_config() -> Arc<AppConfig> {
    Arc::new(AppConfig {
        config: Arc::new(ArcSwap::from_pointee(Config::default())),
        sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
        hdhomerun: Arc::new(ArcSwapOption::empty()),
        api_proxy: Arc::new(ArcSwapOption::empty()),
        file_locks: Arc::new(tuliprox_core::utils::FileLockManager::default()),
        paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
            home_path: String::new(),
            config_path: String::new(),
            storage_path: String::new(),
            config_file_path: String::new(),
            sources_file_path: String::new(),
            mapping_file_path: None,
            mapping_files_used: None,
            template_file_path: None,
            template_files_used: None,
            api_proxy_file_path: String::new(),
            custom_stream_response_path: None,
        })),
        custom_stream_response: Arc::new(ArcSwapOption::empty()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(tuliprox_core::model::MediaToolCapabilities::default()),
    })
}

pub(in crate::refresh::tests) fn test_deterministic_timeline_conflict() -> HlsDeterministicTimelineConflict {
    let fetched = fetched_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:490\n\
         #EXTINF:4,\n490.ts\n#EXTINF:4,\n480.ts\n#EXTINF:4,\n491.ts\n",
    );
    deterministic_timeline_conflict_from_rejection(
        &fetched,
        &HlsManifestRejectLogReason::PublishedResourceReplay {
            previous_proxy_tail: Some(2),
            existing_proxy_seq: 0,
            candidate_position: 1,
            candidate_origin_seq: 491,
            resource_key: super::super::super::resource_identity::HlsMediaResourceIdentity::from_url(
                "http://origin.example.com/live/final/480.ts",
                None,
            )
            .semantic_key(),
            decision: super::super::super::timeline::HlsResourceReplayDecision::RejectContradictoryOrder,
        },
    )
    .expect("published replay rejection produces deterministic evidence")
}

pub(in crate::refresh::tests) async fn refresh_session_with_origin_body(body: &'static str) -> Arc<RwLock<HlsSession>> {
    let session = test_session();
    let server = spawn_test_origin(Arc::new(move |_path| (200, Vec::new(), body.to_string()))).await;
    let entry = LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url))
        .expect("valid origin entry");
    let request = OriginRefreshRequest {
        app_config: test_app_config(),
        session: Arc::clone(&session),
        origin_entry: entry.clone(),
        headers: HeaderMap::new(),
        origin_provider_session_headers: HeaderMap::new(),
        client: reqwest::Client::new(),
        no_redirect_client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client builds"),
        use_manual_redirects: false,
        segment_cache: Arc::new(HlsSegmentCache::new()),
        hls_proxy: Arc::new(HlsProxyManager::new()),
        segment_repair: test_segment_repair_manager(),
        segment_worker_pool: Arc::new(HlsSegmentWorkerPool::default()),
        map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
        origin_manifest_timeout_ms: 2_000,
        manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
        strip: StripConfig { mode: HlsStripMode::Segments, value: 3 },
        retry_policy: no_delay_policy(),
        reverse_proxy_rewrite_secret: b"secret".to_vec(),
        transient_resource_ttl_ms: 300_000,
        manifest_commit_requirement: HlsManifestCommitRequirement::CommittedManifestAllowed,
        fresh_manifest_requirement_generation: None,
        acceptance_directive: HlsManifestAcceptanceDirective::none(),
        access_lease_id: None,
        disabled_headers: None,
        now_ms: 100,
        origin_io: None,
        post_refresh_runtime: None,
    };

    assert!(trigger_origin_refresh_sync(request).await);
    {
        let session = session.read().await;
        assert!(
            !session.segments.is_empty() || session.transient.last_manifest_body.is_some(),
            "synchronous controlled origin work must commit origin state: mode={:?} path_condition={:?} failures={}",
            session.mode,
            session.origin_control.path_condition,
            session.origin_refresh.consecutive_failures
        );
    }
    session
}

pub(in crate::refresh::tests) struct TestOriginServer {
    pub(in crate::refresh::tests) base_url: String,
    pub(in crate::refresh::tests) requests: Arc<Mutex<Vec<String>>>,
    pub(in crate::refresh::tests) raw_requests: Arc<Mutex<Vec<String>>>,
    pub(in crate::refresh::tests) task: tokio::task::JoinHandle<()>,
}

pub(in crate::refresh::tests) type TestOriginHandler =
    Arc<dyn Fn(String) -> (u16, Vec<(&'static str, String)>, String) + Send + Sync>;

impl Drop for TestOriginServer {
    fn drop(&mut self) { self.task.abort(); }
}

pub(in crate::refresh::tests) async fn spawn_test_origin(handler: TestOriginHandler) -> TestOriginServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
    let addr = listener.local_addr().expect("local addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_for_task = Arc::clone(&requests);
    let raw_requests = Arc::new(Mutex::new(Vec::new()));
    let raw_requests_for_task = Arc::clone(&raw_requests);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let requests = Arc::clone(&requests_for_task);
            let raw_requests = Arc::clone(&raw_requests_for_task);
            let handler = Arc::clone(&handler);
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 4096];
                let mut used = 0_usize;
                loop {
                    let Ok(read) = socket.read(&mut buf[used..]).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    used += read;
                    if used >= 4 && buf[..used].windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                    if used == buf.len() {
                        return;
                    }
                }
                let request = String::from_utf8_lossy(&buf[..used]).into_owned();
                let path =
                    request.lines().next().and_then(|line| line.split_whitespace().nth(1)).unwrap_or("/").to_string();
                requests.lock().await.push(path.clone());
                raw_requests.lock().await.push(request);
                let (status, headers, body) = handler(path);
                let reason = match status {
                    200 => "OK",
                    302 => "Found",
                    404 => "Not Found",
                    407 => "Proxy Authentication Required",
                    500 => "Internal Server Error",
                    _ => "Status",
                };
                let mut response =
                    format!("HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
                for (name, value) in headers {
                    response.push_str(name);
                    response.push_str(": ");
                    response.push_str(&value);
                    response.push_str("\r\n");
                }
                response.push_str("\r\n");
                response.push_str(&body);
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    TestOriginServer { base_url: format!("http://{addr}"), requests, raw_requests, task }
}

pub(in crate::refresh::tests) fn request_header_value<'a>(request: &'a str, expected_name: &str) -> Option<&'a str> {
    request.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case(expected_name).then_some(value.trim())
    })
}

pub(in crate::refresh::tests) fn no_delay_policy() -> RetryPolicy {
    RetryPolicy { delays_ms: [0, 0, 0, 0, 0], jitter_max_ms: 0 }
}

pub(in crate::refresh::tests) fn test_recovery_timing_policy(expected_eta_ms: u64) -> HlsRecoveryTimingPolicy {
    HlsRecoveryTimingPolicy::new(
        HlsOperationTimeoutMs::from_millis(2_000),
        HlsOperationTimeoutMs::from_millis(2_000),
        HlsRecoveryEtaMs::from_millis(expected_eta_ms),
        HlsRecoveryEtaMs::from_millis(expected_eta_ms),
    )
}

pub(in crate::refresh::tests) fn test_acceptance_episode_timing(
    started_at_ms: u64,
    burst_plan: shared::model::HlsManifestRecoveryBurstPlan,
) -> HlsAcceptanceEpisodeTiming {
    HlsAcceptanceEpisodeTiming::from_input(&HlsAcceptanceEpisodeTimingInput {
        started_at_ms,
        burst_plan,
        target_duration_ms: 4_000,
        transition_margin: HlsTransitionMarginMs::from_millis(4_000),
        workload: HlsRecoveryWorkload::clear_fetch(),
        observed_latency: HlsObservedRecoveryLatency::default(),
        required_terminal_media_key: None,
        terminal_media_preparation: HlsTerminalMediaPreparationState::Failed { key: None },
        policy: test_recovery_timing_policy(2_000),
    })
}

pub(in crate::refresh::tests) fn test_switch_staging_acceptance_episode_timing(
    started_at_ms: u64,
    burst_plan: shared::model::HlsManifestRecoveryBurstPlan,
) -> HlsAcceptanceEpisodeTiming {
    HlsAcceptanceEpisodeTiming::from_input(&HlsAcceptanceEpisodeTimingInput {
        started_at_ms,
        burst_plan,
        target_duration_ms: 4_000,
        transition_margin: HlsTransitionMarginMs::from_millis(4_000),
        workload: HlsRecoveryWorkloadEnvelope::acceptance_policy().ceiling(),
        observed_latency: HlsObservedRecoveryLatency::default(),
        required_terminal_media_key: None,
        terminal_media_preparation: HlsTerminalMediaPreparationState::Failed { key: None },
        policy: test_recovery_timing_policy(2_000),
    })
}

pub(in crate::refresh::tests) fn test_origin_refresh_request(session: Arc<RwLock<HlsSession>>) -> OriginRefreshRequest {
    let entry =
        LiveHlsOriginEntry::parse("http://origin.example.com/live/user/pass/12345.m3u8").expect("valid origin entry");
    OriginRefreshRequest {
        app_config: test_app_config(),
        session,
        origin_entry: entry.clone(),
        headers: HeaderMap::new(),
        origin_provider_session_headers: HeaderMap::new(),
        client: reqwest::Client::new(),
        no_redirect_client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client builds"),
        use_manual_redirects: false,
        segment_cache: Arc::new(HlsSegmentCache::new()),
        hls_proxy: Arc::new(HlsProxyManager::new()),
        segment_repair: test_segment_repair_manager(),
        segment_worker_pool: Arc::new(HlsSegmentWorkerPool::default()),
        map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
        origin_manifest_timeout_ms: 2_000,
        manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
        strip: StripConfig { mode: HlsStripMode::Segments, value: 0 },
        retry_policy: no_delay_policy(),
        reverse_proxy_rewrite_secret: b"secret".to_vec(),
        transient_resource_ttl_ms: 300_000,
        manifest_commit_requirement: HlsManifestCommitRequirement::CommittedManifestAllowed,
        fresh_manifest_requirement_generation: None,
        acceptance_directive: HlsManifestAcceptanceDirective::none(),
        access_lease_id: None,
        disabled_headers: None,
        now_ms: 100,
        origin_io: None,
        post_refresh_runtime: None,
    }
}

pub(in crate::refresh::tests) fn bind_refresh_request_to_app_state(
    mut request: OriginRefreshRequest,
    ctx: &crate::HlsCtx,
) -> OriginRefreshRequest {
    request.segment_cache = Arc::clone(ctx.hls_proxy.segment_cache());
    request.hls_proxy = Arc::clone(&ctx.hls_proxy);
    request.segment_repair = Arc::clone(ctx.hls_proxy.segment_repair());
    request.segment_worker_pool = Arc::clone(ctx.hls_proxy.segment_worker_pool());
    request.map_worker_pool = Arc::clone(ctx.hls_proxy.map_worker_pool());
    request.post_refresh_runtime = Some(HlsPostRefreshRuntime { ctx: ctx.downgrade() });
    request
}

pub(in crate::refresh::tests) fn post_refresh_live_manifest_snapshot() -> HlsLeaseManifestSnapshot {
    HlsLeaseManifestSnapshot {
        startup_revisions: None,
        delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
        source_commit_identity: HlsManifestCommitIdentity::new(1),
        uri_materialization: None,
        finalized_transient_manifest_generation: None,
        snapshot_generation: 1,
        delivered_at_ms: 1,
        first_proxy_seq: 0,
        last_proxy_seq: 2,
        visible_segments: Arc::from([
            HlsLeaseManifestSegment {
                proxy_seq: 0,
                duration_ms: 4_000,
                uri: "0.ts".into(),
                discontinuity_before: false,
                map_ref_ready: true,
                encryption: None,
            },
            HlsLeaseManifestSegment {
                proxy_seq: 1,
                duration_ms: 4_000,
                uri: "1.ts".into(),
                discontinuity_before: false,
                map_ref_ready: true,
                encryption: None,
            },
            HlsLeaseManifestSegment {
                proxy_seq: 2,
                duration_ms: 4_000,
                uri: "2.ts".into(),
                discontinuity_before: false,
                map_ref_ready: true,
                encryption: None,
            },
        ]),
        discontinuity_sequence: 0,
        target_duration_ms: 4_000,
        playlist_duration_ms: 12_000,
        last_visible_media_end_ms: 12_000,
        active_map: None,
        active_encryption: None,
        container: HlsMediaContainer::MpegTs,
    }
}

pub(in crate::refresh::tests) fn fetched_manifest(body: &str) -> FetchedOriginManifest {
    FetchedOriginManifest {
        body: body.to_string(),
        final_manifest_url: "http://origin.example.com/live/final/index.m3u8".to_string(),
        resolved_request_url: "http://origin.example.com/live/user/pass/12345.m3u8".to_string(),
        redirect_host: Some("origin.example.com".to_string()),
        provider_url_index: None,
        provider_session_headers: HeaderMap::new(),
        status: StatusCode::OK,
        attempts: 1,
        candidate_requests: 1,
        selection: HlsManifestFetchSelection::Initial,
    }
}

pub(in crate::refresh::tests) fn host_from_base_url(base_url: &str) -> String {
    url::Url::parse(base_url).expect("base url").host_str().expect("host").to_string()
}

pub(in crate::refresh::tests) fn manifest_body() -> String {
    "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nseg.ts\n".to_string()
}

pub(in crate::refresh::tests) fn three_segment_manifest_body(media_sequence: u64) -> String {
    format!(
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:{media_sequence}\n#EXT-X-TARGETDURATION:4\n\
         #EXTINF:4.0,\n{media_sequence}.ts\n\
         #EXTINF:4.0,\n{}.ts\n\
         #EXTINF:4.0,\n{}.ts\n",
        media_sequence.saturating_add(1),
        media_sequence.saturating_add(2)
    )
}

pub(in crate::refresh::tests) async fn publish_ready_test_manifest(
    session: &Arc<RwLock<HlsSession>>,
    rendered_at_ms: u64,
) {
    let mut session = session.write().await;
    for segment in session.segments.values_mut() {
        segment.status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: rendered_at_ms };
    }
    session.render_and_store_manifest(rendered_at_ms).expect("test live manifest publishes");
}

pub(in crate::refresh::tests) const SWITCH_MAP_BODY: &[u8] = b"complete-switch-map";

pub(in crate::refresh::tests) const SWITCH_SEGMENT_BODY: &[u8] = b"complete-switch-segment-body";

pub(in crate::refresh::tests) const CRITICAL_HANDOFF_TS_BODY: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"));

pub(in crate::refresh::tests) const CRITICAL_HANDOFF_MANIFEST_BODY: &[u8] = b"#EXTM3U\n\
    #EXT-X-MEDIA-SEQUENCE:900\n\
    #EXT-X-TARGETDURATION:4\n\
    #EXTINF:4.0,\nfirst.ts\n\
    #EXTINF:4.0,\nsecond.ts\n";

pub(in crate::refresh::tests) struct ControlledSwitchOriginServer {
    pub(in crate::refresh::tests) base_url: String,
    pub(in crate::refresh::tests) requests: Arc<Mutex<Vec<String>>>,
    pub(in crate::refresh::tests) segment_prefix_written: Arc<Notify>,
    pub(in crate::refresh::tests) release_segment_body: Arc<Notify>,
    pub(in crate::refresh::tests) task: tokio::task::JoinHandle<()>,
}

impl Drop for ControlledSwitchOriginServer {
    fn drop(&mut self) { self.task.abort(); }
}

pub(in crate::refresh::tests) async fn await_controlled_switch_segment_prefix<T: std::fmt::Debug>(
    origin: &ControlledSwitchOriginServer,
    task: &mut tokio::task::JoinHandle<T>,
    task_name: &str,
) {
    tokio::select! {
        () = origin.segment_prefix_written.notified() => {}
        result = &mut *task => {
            panic!("{task_name} completed before controlled segment staging reached the network boundary: {result:?}");
        }
        () = tokio::time::sleep(Duration::from_secs(10)) => {
            panic!("{task_name} did not reach the controlled segment staging boundary before the test deadline");
        }
    }
}

pub(in crate::refresh::tests) struct CriticalEmergencyOriginServer {
    pub(in crate::refresh::tests) base_url: String,
    pub(in crate::refresh::tests) manifest_requests: Arc<AtomicUsize>,
    pub(in crate::refresh::tests) segment_requests: Arc<AtomicUsize>,
    pub(in crate::refresh::tests) segment_prefix_written: Arc<Notify>,
    pub(in crate::refresh::tests) release_segment_body: Arc<Notify>,
    pub(in crate::refresh::tests) task: tokio::task::JoinHandle<()>,
}

impl Drop for CriticalEmergencyOriginServer {
    fn drop(&mut self) { self.task.abort(); }
}

pub(in crate::refresh::tests) async fn read_test_request_path(socket: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buf = vec![0_u8; 4_096];
    let mut used = 0_usize;
    loop {
        let read = socket.read(&mut buf[used..]).await.ok()?;
        if read == 0 {
            return None;
        }
        used = used.saturating_add(read);
        if used >= 4 && buf[..used].windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if used == buf.len() {
            return None;
        }
    }
    String::from_utf8_lossy(&buf[..used])
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .map(str::to_string)
}

pub(in crate::refresh::tests) async fn write_test_response(
    socket: &mut tokio::net::TcpStream,
    status: u16,
    body: &[u8],
) {
    let reason = if status == 200 { "OK" } else { "Not Found" };
    let head = format!("HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
    let _ = socket.write_all(head.as_bytes()).await;
    let _ = socket.write_all(body).await;
}

pub(in crate::refresh::tests) async fn spawn_controlled_switch_origin() -> ControlledSwitchOriginServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("controlled switch origin binds");
    let addr = listener.local_addr().expect("controlled switch origin address");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_for_task = Arc::clone(&requests);
    let segment_prefix_written = Arc::new(Notify::new());
    let segment_prefix_written_for_task = Arc::clone(&segment_prefix_written);
    let release_segment_body = Arc::new(Notify::new());
    let release_segment_body_for_task = Arc::clone(&release_segment_body);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let requests = Arc::clone(&requests_for_task);
            let segment_prefix_written = Arc::clone(&segment_prefix_written_for_task);
            let release_segment_body = Arc::clone(&release_segment_body_for_task);
            tokio::spawn(async move {
                let Some(path) = read_test_request_path(&mut socket).await else {
                    return;
                };
                requests.lock().await.push(path.clone());
                match path.as_str() {
                    "/live/final/init.mp4" => write_test_response(&mut socket, 200, SWITCH_MAP_BODY).await,
                    "/live/final/first.ts" => {
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            SWITCH_SEGMENT_BODY.len()
                        );
                        if socket.write_all(head.as_bytes()).await.is_err() {
                            return;
                        }
                        let split = SWITCH_SEGMENT_BODY.len() / 2;
                        if socket.write_all(&SWITCH_SEGMENT_BODY[..split]).await.is_err() {
                            return;
                        }
                        segment_prefix_written.notify_one();
                        release_segment_body.notified().await;
                        let _ = socket.write_all(&SWITCH_SEGMENT_BODY[split..]).await;
                    }
                    _ => write_test_response(&mut socket, 404, &[]).await,
                }
            });
        }
    });
    ControlledSwitchOriginServer {
        base_url: format!("http://{addr}"),
        requests,
        segment_prefix_written,
        release_segment_body,
        task,
    }
}

pub(in crate::refresh::tests) async fn spawn_critical_emergency_origin() -> CriticalEmergencyOriginServer {
    spawn_critical_emergency_origin_with_control(false).await
}

pub(in crate::refresh::tests) async fn spawn_critical_emergency_origin_with_control(
    pause_segment: bool,
) -> CriticalEmergencyOriginServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("critical emergency origin binds");
    let addr = listener.local_addr().expect("critical emergency origin address");
    let manifest_requests = Arc::new(AtomicUsize::new(0));
    let manifest_requests_for_task = Arc::clone(&manifest_requests);
    let segment_requests = Arc::new(AtomicUsize::new(0));
    let segment_requests_for_task = Arc::clone(&segment_requests);
    let segment_prefix_written = Arc::new(Notify::new());
    let segment_prefix_written_for_task = Arc::clone(&segment_prefix_written);
    let release_segment_body = Arc::new(Notify::new());
    let release_segment_body_for_task = Arc::clone(&release_segment_body);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let manifest_requests = Arc::clone(&manifest_requests_for_task);
            let segment_requests = Arc::clone(&segment_requests_for_task);
            let segment_prefix_written = Arc::clone(&segment_prefix_written_for_task);
            let release_segment_body = Arc::clone(&release_segment_body_for_task);
            tokio::spawn(async move {
                let Some(path) = read_test_request_path(&mut socket).await else {
                    return;
                };
                if path_has_extension(&path, "m3u8") {
                    let request_index = manifest_requests.fetch_add(1, Ordering::SeqCst);
                    if request_index == 0 {
                        write_test_response(&mut socket, 200, CRITICAL_HANDOFF_MANIFEST_BODY).await;
                    } else {
                        write_test_response(&mut socket, 407, b"retryable manifest failure").await;
                    }
                } else if path.ends_with("/first.ts") {
                    segment_requests.fetch_add(1, Ordering::SeqCst);
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        CRITICAL_HANDOFF_TS_BODY.len()
                    );
                    if socket.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    let split = CRITICAL_HANDOFF_TS_BODY.len() / 2;
                    if socket.write_all(&CRITICAL_HANDOFF_TS_BODY[..split]).await.is_err() {
                        return;
                    }
                    segment_prefix_written.notify_one();
                    if pause_segment {
                        release_segment_body.notified().await;
                    }
                    let _ = socket.write_all(&CRITICAL_HANDOFF_TS_BODY[split..]).await;
                } else {
                    write_test_response(&mut socket, 404, &[]).await;
                }
            });
        }
    });
    CriticalEmergencyOriginServer {
        base_url: format!("http://{addr}"),
        manifest_requests,
        segment_requests,
        segment_prefix_written,
        release_segment_body,
        task,
    }
}

pub(in crate::refresh::tests) fn switch_manifest_body(include_map: bool) -> String {
    let map = if include_map { "#EXT-X-MAP:URI=\"init.mp4\"\n" } else { "" };
    format!(
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:500\n#EXT-X-TARGETDURATION:4\n{map}#EXTINF:4.0,\nfirst.ts\n#EXTINF:4.0,\nsecond.ts\n"
    )
}

pub(in crate::refresh::tests) fn switch_fetched_manifest(base_url: &str, include_map: bool) -> FetchedOriginManifest {
    FetchedOriginManifest {
        body: switch_manifest_body(include_map),
        final_manifest_url: format!("{base_url}/live/final/index.m3u8"),
        resolved_request_url: format!("{base_url}/live/user/pass/12345.m3u8"),
        redirect_host: None,
        provider_url_index: None,
        provider_session_headers: HeaderMap::new(),
        status: StatusCode::OK,
        attempts: 1,
        candidate_requests: 1,
        selection: HlsManifestFetchSelection::Initial,
    }
}

pub(in crate::refresh::tests) fn switch_test_request(
    session: Arc<RwLock<HlsSession>>,
    segment_cache: Arc<HlsSegmentCache>,
    base_url: &str,
) -> OriginRefreshRequest {
    let mut request = test_origin_refresh_request(session);
    request.origin_entry =
        LiveHlsOriginEntry::parse(&format!("{base_url}/live/user/pass/12345.m3u8")).expect("switch origin entry");
    request.client = reqwest::Client::builder().no_proxy().build().expect("switch client");
    request.no_redirect_client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("switch no-redirect client");
    request.hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(segment_cache.cache_path(), 300));
    request.segment_cache = segment_cache;
    request.segment_worker_pool = Arc::new(HlsSegmentWorkerPool::new(SegmentFetchPolicy {
        origin_segment_timeout_ms: 2_000,
        retry_delays_ms: [0; 5],
        retry_jitter_max_ms: 0,
        ..SegmentFetchPolicy::default()
    }));
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Friendly };
    request
}

pub(in crate::refresh::tests) async fn prepare_cross_host_baseline(session: &Arc<RwLock<HlsSession>>) {
    let body =
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:500\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nold500.ts\n#EXTINF:4.0,\nold501.ts\n";
    let tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(manifest) =
        tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(
            body,
            "http://previous.example.com/live/user/pass/12345.m3u8",
        )
    else {
        panic!("baseline manifest parses as normal timeline");
    };
    let mut session = session.write().await;
    session
        .apply_origin_manifest_for_host(&manifest, crate::timeline::effective_origin_host_id("previous.example.com"))
        .expect("baseline timeline commits");
    session.last_effective_manifest_host = Some("previous.example.com".to_string());
    session.origin_control.pinned_host = Some("previous.example.com".to_string());
    session.origin_control.origin_epoch = session.origin_epoch;
}

pub(in crate::refresh::tests) async fn install_published_recovery_binding(
    session: &Arc<RwLock<HlsSession>>,
    request_url: &str,
    provider_url_index: Option<usize>,
) {
    let binding = HlsManifestOriginBinding::new(
        Url::parse(request_url).expect("published recovery request URL"),
        provider_url_index,
    )
    .expect("published recovery binding");
    let mut session = session.write().await;
    session.origin_control.manifest_origin_binding = Some(binding);
    session.origin_control.record_media_progress(1, 4_000);
    let evidence_proxy_seq = session
        .segments
        .iter()
        .find_map(|(proxy_seq, segment)| {
            (!is_hls_provisioning_segment(segment) && !is_hls_provisioning_gap_segment(segment)).then_some(*proxy_seq)
        })
        .unwrap_or_else(|| {
            let proxy_seq = session.proxy_next_seq.unwrap_or(0);
            let origin_sequence = session.origin_seq_highwater.unwrap_or(proxy_seq);
            let origin_epoch = session.origin_epoch;
            let cache_key = SegmentCacheKey::new(session.proxy_session_id.clone(), proxy_seq, "ts");
            session.segments.insert(
                proxy_seq,
                SegmentEntry {
                    origin_key: OriginSegmentKey {
                        origin_epoch,
                        effective_host_id: 0,
                        host_local_sequence: origin_sequence,
                        host_local_index: 0,
                    },
                    proxy_seq,
                    duration_ms: 4_000,
                    proxy_file_ext: "ts".to_string(),
                    content_type: "video/mp2t".to_string(),
                    cache_key,
                    discontinuity_before: false,
                    program_date_time: None,
                    daterange_tags_before: Vec::new(),
                    origin_byte_range: None,
                    map_ref: None,
                    encryption: None,
                    origin_fetch_ref: None,
                    status: SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 1 },
                    last_rendered_at_ms: None,
                    access: Arc::new(CacheAccessState::new()),
                },
            );
            session.publishable_origin_head_proxy_seq.get_or_insert(proxy_seq);
            session.publishable_origin_tail_proxy_seq = Some(proxy_seq);
            session.proxy_next_seq = Some(proxy_seq.saturating_add(1));
            proxy_seq
        });
    let discontinuity_sequence = session.discontinuity_sequence;
    let outcome = session.store_rendered_manifest(RenderedManifest {
        body: "#EXTM3U\n".to_string(),
        first_proxy_seq: evidence_proxy_seq,
        last_proxy_seq: evidence_proxy_seq,
        discontinuity_sequence,
        target_duration_ms: 4_000,
        playlist_duration_ms: 4_000,
        valid_until_ms: 10_000,
        render_gap_segments: 0,
        rendered_at_ms: 1,
        segment_proxy_seqs: vec![evidence_proxy_seq],
    });
    assert_eq!(outcome, RenderedManifestStoreOutcome::Stored);
    assert!(session.published_live_origin_baseline.is_some());
}

pub(in crate::refresh::tests) async fn commit_ready_baseline_snapshot(
    session: &Arc<RwLock<HlsSession>>,
    cache: &HlsSegmentCache,
    now_ms: u64,
) -> HlsLeaseManifestSnapshot {
    let cache_keys = {
        let session = session.read().await;
        session.segments.values().map(|segment| segment.cache_key.clone()).collect::<Vec<_>>()
    };
    for cache_key in &cache_keys {
        cache.write_bytes_and_commit(cache_key, CRITICAL_HANDOFF_TS_BODY).await.expect("baseline TS object commits");
    }

    let (visible_segments, discontinuity_sequence, target_duration_ms) = {
        let mut session = session.write().await;
        for segment in session.segments.values_mut() {
            assert!(segment.map_ref.is_none());
            assert!(segment.encryption.is_none());
            segment.status = SegmentCacheStatus::Ready {
                content_length: u64::try_from(CRITICAL_HANDOFF_TS_BODY.len()).unwrap_or(u64::MAX),
                ready_at_ms: now_ms,
            };
        }
        let visible_segments = session
            .segments
            .values()
            .map(|segment| HlsLeaseManifestSegment {
                proxy_seq: segment.proxy_seq,
                duration_ms: segment.duration_ms,
                uri: format!("/hls/test/{:06}.ts", segment.proxy_seq).into(),
                discontinuity_before: segment.discontinuity_before,
                map_ref_ready: true,
                encryption: None,
            })
            .collect::<Vec<_>>();
        (
            visible_segments,
            session.discontinuity_sequence,
            session.target_duration.map_or(4_000, |seconds| u64::from(seconds).saturating_mul(1_000)),
        )
    };
    let first_proxy_seq = visible_segments.first().expect("baseline head").proxy_seq;
    let last_proxy_seq = visible_segments.last().expect("baseline tail").proxy_seq;
    let playlist_duration_ms =
        visible_segments.iter().fold(0_u64, |duration_ms, segment| duration_ms.saturating_add(segment.duration_ms));
    HlsLeaseManifestSnapshot {
        startup_revisions: None,
        delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
        source_commit_identity: HlsManifestCommitIdentity::new(now_ms),
        uri_materialization: None,
        finalized_transient_manifest_generation: None,
        snapshot_generation: 0,
        delivered_at_ms: now_ms,
        first_proxy_seq,
        last_proxy_seq,
        visible_segments: Arc::from(visible_segments),
        discontinuity_sequence,
        target_duration_ms,
        playlist_duration_ms,
        last_visible_media_end_ms: playlist_duration_ms,
        active_map: None,
        active_encryption: None,
        container: HlsMediaContainer::MpegTs,
    }
}

pub(in crate::refresh::tests) async fn prepare_active_critical_handoff_lease(
    session: &Arc<RwLock<HlsSession>>,
    cache: &HlsSegmentCache,
    now_ms: u64,
) -> (Arc<HlsProxyManager>, HlsAccessLeaseId, HlsAccessLease) {
    let manifest_snapshot = commit_ready_baseline_snapshot(session, cache, now_ms).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("critical-emergency-handoff".to_string());
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(cache.cache_path(), 300));
    hls_proxy
        .prepare_access_lease(HlsAccessLease::pending(
            lease_id.clone(),
            HlsPlaybackFamilyKey::new("critical-user", "critical-client"),
            proxy_session_id.clone(),
            "critical-user".to_string(),
            "critical-session".to_string(),
            1,
            "12345".to_string(),
            12345,
            now_ms,
            120_000,
        ))
        .await;
    assert!(hls_proxy
        .activate_access_lease(
            &lease_id,
            &proxy_session_id,
            now_ms,
            HlsAccessLeaseTiming { active_window_ms: 120_000, valid_window_ms: 120_000 },
        )
        .await
        .is_activated());
    let publication_guard = hls_proxy
        .prepare_access_lease_manifest_publication(&lease_id, &proxy_session_id, now_ms)
        .await
        .expect("active lease accepts authoritative manifest publication");
    assert!(hls_proxy
        .commit_access_lease_manifest_publication(
            &lease_id,
            &proxy_session_id,
            publication_guard,
            manifest_snapshot,
            now_ms,
        )
        .await
        .is_committed());
    let lease = hls_proxy
        .access_lease_response_snapshot(&lease_id, &proxy_session_id, now_ms)
        .await
        .expect("critical live lease snapshot");
    (hls_proxy, lease_id, lease)
}

pub(in crate::refresh::tests) fn critical_handoff_app_config() -> Arc<AppConfig> {
    let app_config = test_app_config();
    app_config.custom_stream_response.store(Some(Arc::new(CustomStreamResponse {
        channel_unavailable: Some(TransportStreamBuffer::new(CRITICAL_HANDOFF_TS_BODY.to_vec())),
        user_connections_exhausted: None,
        provider_connections_exhausted: None,
        low_priority_preempted: None,
        user_account_expired: None,
        panel_api_provisioning: None,
        hls_session_or_lease_expired: None,
        panel_api_provisioning_hls_segments: Vec::new(),
    })));
    app_config
}

pub(in crate::refresh::tests) fn candidate_handoff_preview(
    session: &HlsSession,
    body: &str,
    now_ms: u64,
) -> (
    tuliprox_parser::hls::origin_manifest::ParsedOriginManifest,
    Vec<crate::TransientResourceRef>,
    crate::timeline::HlsOriginHandoffPreview,
) {
    let final_url = "https://candidate.example/live/index.m3u8";
    let tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(mut manifest) =
        tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(body, final_url)
    else {
        panic!("candidate fixture must parse as a normal manifest");
    };
    let key_resources =
        super::super::commit::materialize_normal_key_resources(&mut manifest, b"secret", now_ms, 60_000);
    let preview = session
        .preview_origin_handoff_manifest(&manifest, crate::timeline::effective_origin_host_id("candidate.example"), 0)
        .expect("candidate handoff preview");
    (manifest, key_resources, preview)
}

pub(in crate::refresh::tests) async fn mark_full_burst_ready_for_switch_staging(
    session: &Arc<RwLock<HlsSession>>,
    fetched: &FetchedOriginManifest,
) {
    let candidate_workload = {
        let session = session.read().await;
        let (_, key_resources, preview) = candidate_handoff_preview(&session, &fetched.body, 100);
        let first_segment = preview.segments.first().expect("switch candidate recovery segment");
        let required_map =
            first_segment.map_ref.and_then(|map_id| preview.maps.iter().find(|map| map.proxy_map_id == map_id));
        super::super::switch_staging::handoff_preview_recovery_workload(
            &session,
            first_segment,
            required_map,
            &key_resources,
            100,
        )
    };
    let mut session = session.write().await;
    let burst_plan = HlsManifestRecoveryBurstLevel::Friendly.plan();
    session.origin_control.begin_acceptance_episode(
        100,
        burst_plan,
        HlsManifestAcceptanceTrigger::RecoveryRequired,
        &test_switch_staging_acceptance_episode_timing(100, burst_plan),
    );
    let episode = session.origin_control.acceptance_episode.as_mut().expect("acceptance episode");
    episode.record_full_burst();
    episode.state = super::super::super::manifest_acceptance::HlsManifestAcceptanceState::StagingSwitchSegment;
    let effective_host = fetched_effective_manifest_host(fetched);
    let identity = super::super::super::manifest_acceptance::HlsManifestRecoveryCandidateIdentity::from_candidate(
        0,
        effective_host.as_deref(),
        &fetched.body,
    );
    assert_eq!(
        episode.select_candidate(episode.generation, identity),
        super::super::super::manifest_acceptance::HlsRecoveryWorkloadBindingUpdate::Applied
    );
    let mut binding_probe = episode.clone();
    assert_eq!(
        binding_probe.bind_selected_candidate(episode.generation, identity, candidate_workload),
        super::super::super::manifest_acceptance::HlsRecoveryWorkloadBindingUpdate::Applied,
        "switch-staging fixture must admit the selected candidate before network staging"
    );
}

pub(in crate::refresh::tests) async fn assert_incompatible_switch_is_rejected_before_timeline_commit(
    baseline_body: &str,
    candidate_body: &str,
    expected_reason: HlsManifestRejectLogReason,
) {
    let session = test_session();
    let tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(baseline) =
        tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(
            baseline_body,
            "http://previous.example.com/live/index.m3u8",
        )
    else {
        panic!("baseline parses as normal timeline");
    };
    {
        let mut session = session.write().await;
        session
            .apply_origin_manifest_for_host(
                &baseline,
                crate::timeline::effective_origin_host_id("previous.example.com"),
            )
            .expect("baseline timeline commits");
        session.last_effective_manifest_host = Some("previous.example.com".to_string());
        session.origin_control.pinned_host = Some("previous.example.com".to_string());
        session.origin_control.origin_epoch = session.origin_epoch;
    }
    let temp_dir = tempfile::tempdir().expect("switch cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let base_url = "http://127.0.0.1:9";
    let request = switch_test_request(Arc::clone(&session), Arc::clone(&cache), base_url);
    let fetched = FetchedOriginManifest {
        body: candidate_body.to_string(),
        final_manifest_url: format!("{base_url}/live/final/index.m3u8"),
        resolved_request_url: format!("{base_url}/live/user/pass/12345.m3u8"),
        redirect_host: None,
        provider_url_index: None,
        provider_session_headers: HeaderMap::new(),
        status: StatusCode::OK,
        attempts: 1,
        candidate_requests: 1,
        selection: HlsManifestFetchSelection::Initial,
    };
    mark_full_burst_ready_for_switch_staging(&session, &fetched).await;
    let before = {
        let session = session.read().await;
        (session.origin_epoch, session.proxy_next_seq, session.segments.len(), session.maps.len())
    };

    let result = super::super::commit_manifest_recovery_candidate(
        &request,
        fetched,
        HlsManifestCommitAcceptanceMode::AllowHeldHostSwitchCandidate,
    )
    .await;

    assert!(matches!(
        result,
        Err(HlsManifestCommitError::TimelineRejected { reason }) if reason == expected_reason
    ));
    let session = session.read().await;
    assert_eq!((session.origin_epoch, session.proxy_next_seq, session.segments.len(), session.maps.len()), before);
    drop(session);
    assert!(!cache.has_active_temp_files());
}

#[derive(Clone, Copy)]
pub(in crate::refresh::tests) enum StaleSwitchGeneration {
    Acceptance,
    Progress,
    PinnedHostRecovered,
}

pub(in crate::refresh::tests) async fn assert_stale_switch_generation_rejects_commit(
    stale_generation: StaleSwitchGeneration,
) {
    let temp_dir = tempfile::tempdir().expect("switch cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let origin = spawn_controlled_switch_origin().await;
    let session = test_session();
    prepare_cross_host_baseline(&session).await;
    let fetched = switch_fetched_manifest(&origin.base_url, true);
    mark_full_burst_ready_for_switch_staging(&session, &fetched).await;
    let effective_host_id = crate::timeline::effective_origin_host_id(&host_from_base_url(&origin.base_url));
    let (baseline_epoch, baseline_proxy_next, staged_segment_key, staged_map_key) = {
        let tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(manifest) =
            tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(
                &fetched.body,
                &fetched.final_manifest_url,
            )
        else {
            panic!("switch manifest parses as normal timeline");
        };
        let session = session.read().await;
        let preview = session.preview_origin_handoff_manifest(&manifest, effective_host_id, 0).expect("switch preview");
        (
            session.origin_epoch,
            session.proxy_next_seq,
            preview.segments.first().expect("first staged segment").cache_key.clone(),
            preview.maps.first().expect("staged map").cache_key.clone(),
        )
    };
    let request = switch_test_request(Arc::clone(&session), Arc::clone(&cache), &origin.base_url);
    let cleanup_manager = Arc::clone(&request.hls_proxy);
    let mut commit_task = tokio::spawn(async move {
        super::super::commit_manifest_recovery_candidate(
            &request,
            fetched,
            HlsManifestCommitAcceptanceMode::AllowHeldHostSwitchCandidate,
        )
        .await
        .map(|_| ())
    });

    await_controlled_switch_segment_prefix(&origin, &mut commit_task, "stale-generation switch commit").await;
    {
        let mut session = session.write().await;
        match stale_generation {
            StaleSwitchGeneration::Acceptance => {
                let episode = session.origin_control.acceptance_episode.as_mut().expect("active acceptance episode");
                episode.generation = super::super::super::manifest_acceptance::HlsManifestAcceptanceGeneration(
                    episode.generation.0.saturating_add(1),
                );
            }
            StaleSwitchGeneration::Progress => {
                session.origin_control.progress_generation =
                    session.origin_control.progress_generation.saturating_add(1);
            }
            StaleSwitchGeneration::PinnedHostRecovered => {
                session.origin_control.pinned_host = Some("recovered.example.com".to_string());
                session.last_effective_manifest_host = Some("recovered.example.com".to_string());
            }
        }
    }
    origin.release_segment_body.notify_one();

    let result = commit_task.await.expect("stale switch task");
    assert!(matches!(
        result,
        Err(HlsManifestCommitError::TimelineRejected { reason: HlsManifestRejectLogReason::StagedSwitchInvalidated })
    ));
    {
        let session = session.read().await;
        assert_eq!(session.origin_epoch, baseline_epoch);
        assert_eq!(session.proxy_next_seq, baseline_proxy_next);
        assert!(session.segments.values().all(|segment| segment.origin_key.origin_epoch == baseline_epoch));
    }
    assert_eq!(cache.metadata(&staged_segment_key).await.expect("stale segment metadata"), None);
    assert_eq!(cache.metadata(&staged_map_key).await.expect("stale map metadata"), None);
    assert_eq!(cleanup_manager.cache_deletion_queue_usage(), (0, 0));
    assert!(!cache.has_active_temp_files());
}
