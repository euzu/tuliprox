use super::{SegmentFetchContext, SegmentFetchPolicy};
use crate::{
    HlsAccessLease, HlsAccessLeaseId, HlsPlaybackFamilyKey, HlsSegmentCache, HlsSegmentFile, HlsSegmentRepairManager,
    HlsSegmentWorkerPool, HlsSessionKey, HlsSessionStore, ProxySessionId, SegmentCacheStatus,
    TransientObjectFetchDecision, TransientObjectFetchToken, TransientResourceId, TransientResourceKind,
    TransientResourceRef,
};
use async_compression::tokio::bufread::{BrotliEncoder, DeflateEncoder, GzipEncoder, ZstdEncoder};
use axum::http::HeaderMap;
use shared::model::HlsSegmentRepairMode;
use std::{
    collections::VecDeque,
    io::Cursor,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::{Mutex, Notify},
};
use tuliprox_core::{model::HlsSegmentRepairConfig, utils::current_time_millis};
use tuliprox_parser::hls::origin_manifest::{parse_origin_media_manifest, OriginManifestParseOutcome};

pub(in crate::segment_fetcher::tests) const BASE_URL: &str = "http://origin.example.com/live/final/index.m3u8";

pub(in crate::segment_fetcher::tests) fn normal_manifest(
    body: &str,
) -> tuliprox_parser::hls::origin_manifest::ParsedOriginManifest {
    match parse_origin_media_manifest(body, BASE_URL) {
        OriginManifestParseOutcome::Normal(manifest) => manifest,
        OriginManifestParseOutcome::TransientPassthrough { reason } => {
            panic!("expected normal manifest: {reason:?}")
        }
    }
}

pub(in crate::segment_fetcher::tests) fn test_segment_repair_manager() -> Arc<HlsSegmentRepairManager> {
    Arc::new(HlsSegmentRepairManager::new(HlsSegmentRepairConfig {
        max_level: HlsSegmentRepairMode::Off,
        apply_to_first_segments: 1,
        max_parallel_repairs: 1,
        ..Default::default()
    }))
}

pub(in crate::segment_fetcher::tests) struct TestSegmentServer {
    pub(in crate::segment_fetcher::tests) base_url: String,
    pub(in crate::segment_fetcher::tests) requests: Arc<Mutex<Vec<String>>>,
    pub(in crate::segment_fetcher::tests) task: tokio::task::JoinHandle<()>,
}

impl Drop for TestSegmentServer {
    fn drop(&mut self) { self.task.abort(); }
}

#[derive(Clone)]
pub(in crate::segment_fetcher::tests) struct TestOriginResponse {
    pub(in crate::segment_fetcher::tests) status: u16,
    pub(in crate::segment_fetcher::tests) headers: Vec<(String, String)>,
    pub(in crate::segment_fetcher::tests) body: Vec<u8>,
}

impl TestOriginResponse {
    pub(in crate::segment_fetcher::tests) fn encoded(content_encoding: &str, body: Vec<u8>) -> Self {
        Self { status: 200, headers: vec![("Content-Encoding".to_string(), content_encoding.to_string())], body }
    }
}

#[derive(Debug, Clone, Copy)]
pub(in crate::segment_fetcher::tests) enum TestContentEncoding {
    Gzip,
    RawDeflate,
    Brotli,
    Zstd,
}

impl TestContentEncoding {
    pub(in crate::segment_fetcher::tests) const fn header_value(self) -> &'static str {
        match self {
            Self::Gzip => "gzip",
            Self::RawDeflate => "deflate",
            Self::Brotli => "br",
            Self::Zstd => "zstd",
        }
    }
}

pub(in crate::segment_fetcher::tests) async fn encode_test_body(body: &[u8], encoding: TestContentEncoding) -> Vec<u8> {
    let reader = BufReader::new(Cursor::new(body.to_vec()));
    let mut encoded = Vec::new();
    match encoding {
        TestContentEncoding::Gzip => GzipEncoder::new(reader).read_to_end(&mut encoded).await,
        TestContentEncoding::RawDeflate => DeflateEncoder::new(reader).read_to_end(&mut encoded).await,
        TestContentEncoding::Brotli => BrotliEncoder::new(reader).read_to_end(&mut encoded).await,
        TestContentEncoding::Zstd => ZstdEncoder::new(reader).read_to_end(&mut encoded).await,
    }
    .expect("test body encodes");
    encoded
}

pub(in crate::segment_fetcher::tests) async fn spawn_sequence_response_server(
    responses: Vec<TestOriginResponse>,
) -> TestSegmentServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
    let addr = listener.local_addr().expect("local addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
    let task_requests = Arc::clone(&requests);
    let task_responses = Arc::clone(&responses);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let requests = Arc::clone(&task_requests);
            let responses = Arc::clone(&task_responses);
            tokio::spawn(async move {
                use std::fmt::Write as _;

                let mut request = Vec::new();
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let mut chunk = [0_u8; 2048];
                    let Ok(read) = socket.read(&mut chunk).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&chunk[..read]);
                }
                requests.lock().await.push(String::from_utf8_lossy(&request).to_string());
                let response = responses.lock().await.pop_front().unwrap_or(TestOriginResponse {
                    status: 500,
                    headers: Vec::new(),
                    body: Vec::new(),
                });
                let reason = if response.status == 200 { "OK" } else { "Status" };
                let mut head =
                    format!("HTTP/1.1 {} {reason}\r\nContent-Length: {}\r\n", response.status, response.body.len());
                for (name, value) in response.headers {
                    let _ = write!(head, "{name}: {value}\r\n");
                }
                head.push_str("Connection: close\r\n\r\n");
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&response.body).await;
            });
        }
    });

    TestSegmentServer { base_url: format!("http://{addr}"), requests, task }
}

pub(in crate::segment_fetcher::tests) fn temp_cache_files(root: &Path) -> Vec<PathBuf> {
    fn visit(path: &Path, found: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, found);
            } else if path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.contains(".tmp.")) {
                found.push(path);
            }
        }
    }

    let mut found = Vec::new();
    visit(root, &mut found);
    found
}

pub(in crate::segment_fetcher::tests) async fn spawn_segment_server(delay_ms: u64) -> TestSegmentServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
    let addr = listener.local_addr().expect("local addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let task_requests = Arc::clone(&requests);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let requests = Arc::clone(&task_requests);
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 2048];
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
                let request = String::from_utf8_lossy(&buf[..used]).to_string();
                requests.lock().await.push(request.clone());
                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
                let path = request.lines().next().and_then(|line| line.split_whitespace().nth(1)).unwrap_or("/");
                let body = format!("body:{path}");
                let response =
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });

    TestSegmentServer { base_url: format!("http://{addr}"), requests, task }
}

pub(in crate::segment_fetcher::tests) async fn grant_usable_worker_access_lease(
    worker: &HlsSegmentWorkerPool,
    proxy_session_id: &ProxySessionId,
) {
    grant_usable_worker_access_lease_at(worker, proxy_session_id, 10).await;
}

pub(in crate::segment_fetcher::tests) async fn grant_usable_worker_access_lease_at(
    worker: &HlsSegmentWorkerPool,
    proxy_session_id: &ProxySessionId,
    now_ms: u64,
) {
    worker.access_leases().write().await.prepare_access_lease(HlsAccessLease::pending(
        HlsAccessLeaseId("worker-lease".to_string()),
        HlsPlaybackFamilyKey::new("alice", "client-a"),
        proxy_session_id.clone(),
        "alice".to_string(),
        "session-a".to_string(),
        1,
        "12345".to_string(),
        12345,
        now_ms,
        15_000,
    ));
}

pub(in crate::segment_fetcher::tests) async fn fetch_context_with_access_lease(
    server: &TestSegmentServer,
    temp_dir: &tempfile::TempDir,
    policy: &SegmentFetchPolicy,
    grant_access_lease: bool,
) -> (Arc<HlsSegmentWorkerPool>, SegmentFetchContext, HlsSegmentFile) {
    let store = HlsSessionStore::new();
    let session = store.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", 0).await;
    {
        let mut session = session.write().await;
        session.configure_segment_prefetch_queue(policy.max_prefetch_queue_depth);
        session.proxy_next_seq = Some(1);
        session
            .apply_origin_manifest(&normal_manifest(&format!(
                "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:4.0,\n{}/1.ts\n#EXTINF:4.0,\n{}/2.ts\n#EXTINF:4.0,\n{}/3.ts\n",
                server.base_url, server.base_url, server.base_url
            )))
            .expect("manifest maps");
        session.queue_manifest_prefetch_candidates(10);
    }
    let worker = Arc::new(HlsSegmentWorkerPool::new(policy.clone()));
    if grant_access_lease {
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        grant_usable_worker_access_lease(&worker, &proxy_session_id).await;
    }
    let context = SegmentFetchContext {
        session: Arc::clone(&session),
        segment_cache: Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path())),
        segment_repair: test_segment_repair_manager(),
        repair_access_lease_id: None,
        headers: HeaderMap::new(),
        origin_provider_session_headers: HeaderMap::new(),
        client: reqwest::Client::new(),
        no_redirect_client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client builds"),
        use_manual_redirects: true,
        origin_io: None,
    };
    (worker, context, HlsSegmentFile { proxy_seq: 1, extension: "ts".to_string() })
}

pub(in crate::segment_fetcher::tests) async fn fetch_context(
    server: &TestSegmentServer,
    temp_dir: &tempfile::TempDir,
    policy: &SegmentFetchPolicy,
) -> (Arc<HlsSegmentWorkerPool>, SegmentFetchContext, HlsSegmentFile) {
    fetch_context_with_access_lease(server, temp_dir, policy, true).await
}

pub(in crate::segment_fetcher::tests) async fn encrypted_fetch_context(
    server: &TestSegmentServer,
    temp_dir: &tempfile::TempDir,
    policy: &SegmentFetchPolicy,
) -> (Arc<HlsSegmentWorkerPool>, SegmentFetchContext, HlsSegmentFile) {
    let store = HlsSessionStore::new();
    let session = store.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", 0).await;
    let key_uri = format!("{}/key.key", server.base_url);
    let key_resource = TransientResourceRef::new(
        TransientResourceKind::Key,
        &key_uri,
        b"secret",
        0,
        u64::MAX,
        Some("key".to_string()),
    );
    let mut manifest = normal_manifest(&format!(
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXT-X-KEY:METHOD=AES-128,URI=\"{key_uri}\"\n\
         #EXTINF:4.0,\n{}/1.ts\n#EXTINF:4.0,\n{}/2.ts\n#EXTINF:4.0,\n{}/3.ts\n",
        server.base_url, server.base_url, server.base_url
    ));
    for encryption in manifest.segments.iter_mut().filter_map(|segment| segment.encryption.as_mut()) {
        encryption.proxy_resource_id = Some(key_resource.id.0.clone());
        encryption.proxy_resource_extension = Some("key".to_string());
    }
    {
        let mut session = session.write().await;
        session.configure_segment_prefetch_queue(policy.max_prefetch_queue_depth);
        session.proxy_next_seq = Some(1);
        session.transient.upsert_resources([key_resource]);
        session.apply_origin_manifest(&manifest).expect("encrypted manifest maps");
    }
    let worker = Arc::new(HlsSegmentWorkerPool::new(policy.clone()));
    let context = SegmentFetchContext {
        session: Arc::clone(&session),
        segment_cache: Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path())),
        segment_repair: test_segment_repair_manager(),
        repair_access_lease_id: None,
        headers: HeaderMap::new(),
        origin_provider_session_headers: HeaderMap::new(),
        client: reqwest::Client::new(),
        no_redirect_client: reqwest::Client::new(),
        use_manual_redirects: false,
        origin_io: None,
    };
    (worker, context, HlsSegmentFile { proxy_seq: 1, extension: "ts".to_string() })
}

pub(in crate::segment_fetcher::tests) async fn shared_key_fetch_and_wait(
    context: &SegmentFetchContext,
    now_ms: u64,
) -> (TransientObjectFetchToken, Arc<Notify>, TransientResourceId, String) {
    let mut session = context.session.write().await;
    let resource = session
        .transient
        .resources
        .values()
        .find(|resource| resource.kind == TransientResourceKind::Key)
        .cloned()
        .expect("encrypted fixture key resource");
    let extension = resource.file_ext_hint.clone().expect("key resource extension");
    let proxy_session_id = session.proxy_session_id.clone();
    let cache_duration_ms = session.transient.resource_ttl_ms;
    let fetch_token =
        match session.transient.begin_object_fetch(&proxy_session_id, &resource, &extension, now_ms, cache_duration_ms)
        {
            TransientObjectFetchDecision::Fetch(token) => token,
            TransientObjectFetchDecision::Ready | TransientObjectFetchDecision::Wait(_) => {
                panic!("first shared-key decision must own the fetch")
            }
        };
    let notifier =
        match session.transient.begin_object_fetch(&proxy_session_id, &resource, &extension, now_ms, cache_duration_ms)
        {
            TransientObjectFetchDecision::Wait(notifier) => notifier,
            TransientObjectFetchDecision::Ready | TransientObjectFetchDecision::Fetch(_) => {
                panic!("second shared-key decision must wait for the existing fetch")
            }
        };
    (*fetch_token, notifier, resource.id, extension)
}

pub(in crate::segment_fetcher::tests) async fn commit_test_key_ready(
    context: &SegmentFetchContext,
    token: &TransientObjectFetchToken,
    now_ms: u64,
) {
    assert!(context.session.write().await.commit_transient_object_ready_if_current(
        TransientResourceKind::Key,
        token,
        "application/octet-stream".to_string(),
        16,
        now_ms,
        u64::MAX,
    ));
}

pub(in crate::segment_fetcher::tests) async fn clear_scheduled_prefetch(
    context: &SegmentFetchContext,
    policy: &SegmentFetchPolicy,
) {
    let mut session = context.session.write().await;
    session.segment_prefetch_queue = crate::SegmentPrefetchQueue::new(policy.max_prefetch_queue_depth);
    for segment in session.segments.values_mut() {
        if !matches!(segment.status, SegmentCacheStatus::Ready { .. }) {
            segment.status = SegmentCacheStatus::Discovered;
        }
    }
}

pub(in crate::segment_fetcher::tests) async fn committed_segment(
    context: &SegmentFetchContext,
    proxy_seq: u64,
) -> (crate::SegmentCacheKey, Vec<u8>) {
    let cache_key = context.session.read().await.segments.get(&proxy_seq).expect("segment").cache_key.clone();
    let metadata =
        context.segment_cache.metadata(&cache_key).await.expect("cache metadata reads").expect("cache object exists");
    let bytes = tokio::fs::read(metadata.path).await.expect("cache object reads");
    (cache_key, bytes)
}

pub(in crate::segment_fetcher::tests) async fn install_startup(
    context: &SegmentFetchContext,
    worker: &Arc<HlsSegmentWorkerPool>,
    mode: shared::model::HlsStartupMode,
) {
    let config = tuliprox_core::model::HlsStartupConfig { mode, ..Default::default() };
    let budget = crate::ProgressiveBudgetManager::new(config.clone());
    context.session.write().await.startup = Some(crate::HlsSessionStartup {
        config,
        first_data_timeout_ms: 10_000,
        worker: Arc::downgrade(worker),
        access_leases: Arc::clone(worker.access_leases()),
        revisions: std::collections::BTreeMap::new(),
        policy_fixed: false,
        store: Arc::new(crate::SegmentRevisionStore::default()),
        budget,
    });
}

pub(in crate::segment_fetcher::tests) async fn prefix_origin(
    auto_complete_head: bool,
) -> std::io::Result<(TestSegmentServer, tokio::sync::watch::Sender<bool>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    let (release, released) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        let mut children = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((socket, _)) = accepted else { break; };
                    let recorded = Arc::clone(&recorded);
                    let mut released = released.clone();
                    children.spawn(async move {
                        let mut reader = BufReader::new(socket);
                        let mut request = String::new();
                        tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut request).await?;
                        let head = request.starts_with("GET /1.ts ");
                        recorded.lock().await.push(request);
                        loop {
                            let mut line = String::new();
                            tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line).await?;
                            if line == "\r\n" || line.is_empty() { break; }
                        }
                        let mut socket = reader.into_inner();
                        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\nsegment-").await?;
                        if !(auto_complete_head && head) {
                            while !*released.borrow_and_update() {
                                released.changed().await.map_err(std::io::Error::other)?;
                            }
                        }
                        socket.write_all(b"body").await?;
                        socket.shutdown().await?;
                        Ok::<_, std::io::Error>(())
                    });
                }
                _ = children.join_next(), if !children.is_empty() => {}
            }
        }
    });
    Ok((TestSegmentServer { base_url, requests, task }, release))
}

pub(in crate::segment_fetcher::tests) type StartupSoakResources<'a> = (
    &'a tempfile::TempDir,
    &'a Arc<crate::SegmentRevisionStore>,
    &'a Arc<crate::ProgressiveBudgetManager>,
    &'a Arc<HlsSegmentCache>,
);

pub(in crate::segment_fetcher::tests) async fn cold_startup_round(
    mode: shared::model::HlsStartupMode,
    shared: Option<StartupSoakResources<'_>>,
) -> std::io::Result<()> {
    use futures::StreamExt;
    let started = tokio::time::Instant::now();
    let wall_started_at_ms = current_time_millis();
    let (server, release) = prefix_origin(mode == shared::model::HlsStartupMode::FirstReady).await?;
    let directory = tempfile::tempdir()?;
    let policy = SegmentFetchPolicy { max_session_segment_fetches: 3, ..Default::default() };
    let fixture_dir = shared.map_or(&directory, |state| state.0);
    let (worker, mut context, _) = fetch_context_with_access_lease(&server, fixture_dir, &policy, false).await;
    install_startup(&context, &worker, mode).await;
    if let Some((_, store, budget, cache)) = shared {
        context.segment_cache = Arc::clone(cache);
        let mut session = context.session.write().await;
        if let Some(startup) = &mut session.startup {
            startup.store = Arc::clone(store);
            startup.budget = Arc::clone(budget);
        }
    }
    let session_id = context.session.read().await.proxy_session_id.clone();
    grant_usable_worker_access_lease_at(&worker, &session_id, current_time_millis()).await;
    worker.wake_scheduler(context.clone(), current_time_millis()).await;
    let publication = tokio::time::timeout(Duration::from_secs(3), wait_for_cold_publication(&context, mode)).await;
    let publication = match publication {
        Ok(publication) => publication?,
        Err(_) => {
            return Err(
                startup_timeout_diagnostic(&context, &server, "cold publication", started, wall_started_at_ms).await
            )
        }
    };
    let timeline = crate::media_reserve::HlsReadyTimelineSnapshot { units: Arc::from([]) };
    let admission = crate::media_reserve::evaluate_startup_admission(crate::media_reserve::HlsStartupAdmissionInput {
        manifest: &publication,
        ready_timeline: &timeline,
        origin_state: crate::media_reserve::HlsStartupAdmissionOriginState::Healthy,
        recovery_trigger_budget: crate::recovery_timing::HlsRecoveryTriggerBudgetMs::from_millis(4000),
    });
    assert_eq!(admission.decision, crate::media_reserve::HlsStartupAdmissionDecision::Admit);
    let evidence = publication.startup_revisions.as_ref().ok_or_else(|| std::io::Error::other("evidence"))?;
    let head = evidence.revisions.get(&1).ok_or_else(|| std::io::Error::other("head"))?;
    let successor = evidence.revisions.get(&2).ok_or_else(|| std::io::Error::other("successor"))?;
    assert!(matches!(*successor.revision().subscribe().borrow(), crate::SegmentRevisionState::Pending));
    let mut stalled = crate::progressive_startup::revision_body(head.clone(), Duration::from_millis(20), 0, None);
    let aborted = crate::progressive_startup::revision_body(head.clone(), Duration::from_secs(3), 0, None);
    drop(aborted);
    let mut body = crate::progressive_startup::revision_body(head.clone(), Duration::from_secs(3), 0, None);
    let first = body.next().await.transpose()?.ok_or_else(|| std::io::Error::other("first data"))?;
    assert!(first.starts_with(b"segment-"));
    assert_eq!(server.requests.lock().await.len(), 3);
    assert_eq!(
        successor
            .revision()
            .wait_complete(tokio::time::Instant::now() + Duration::from_millis(25))
            .await
            .err()
            .map(|error| error.kind()),
        Some(std::io::ErrorKind::TimedOut)
    );
    assert!(stalled.next().await.is_some_and(|chunk| chunk.is_err()));
    release.send(true).map_err(std::io::Error::other)?;
    let mut received = first.to_vec();
    while let Some(chunk) = body.next().await {
        received.extend_from_slice(&chunk?);
    }
    assert_eq!(received, b"segment-body");
    successor.revision().wait_complete(tokio::time::Instant::now() + Duration::from_secs(3)).await?;
    assert_eq!(server.requests.lock().await.len(), 3);
    if tokio::time::timeout(Duration::from_secs(3), async {
        while context.session.read().await.active_segment_fetches != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_err()
    {
        return Err(
            startup_timeout_diagnostic(&context, &server, "producer completion", started, wall_started_at_ms).await
        );
    }
    Ok(())
}

pub(in crate::segment_fetcher::tests) async fn wait_for_cold_publication(
    context: &SegmentFetchContext,
    mode: shared::model::HlsStartupMode,
) -> std::io::Result<crate::HlsLeaseManifestSnapshot> {
    loop {
        let mut session = context.session.write().await;
        if let Ok(rendered) = session.render_and_store_manifest(current_time_millis()) {
            let head = session.startup.as_ref().and_then(|startup| startup.revisions.get(&1)).cloned();
            if mode == shared::model::HlsStartupMode::Progressive
                || head.as_ref().is_some_and(|head| {
                    matches!(*head.revision().subscribe().borrow(), crate::SegmentRevisionState::Complete(_))
                })
            {
                let snapshot = crate::manifest_snapshot::derive_hls_lease_manifest_snapshot(
                    &crate::manifest_snapshot::HlsLeaseManifestSnapshotInput::NormalCacheTimeline {
                        session: &session,
                        committed_body: &rendered.body,
                        materialized_body: &rendered.body,
                        stripped_tail_segments: 0,
                    },
                    current_time_millis(),
                )
                .map_err(|error| std::io::Error::other(format!("{error:?}")))?
                .ok_or_else(|| std::io::Error::other("publication snapshot"))?;
                return Ok::<_, std::io::Error>(snapshot);
            }
        }
        drop(session);
        tokio::task::yield_now().await;
    }
}

pub(in crate::segment_fetcher::tests) async fn startup_timeout_diagnostic(
    context: &SegmentFetchContext,
    server: &TestSegmentServer,
    phase: &str,
    started: tokio::time::Instant,
    wall_started_at_ms: u64,
) -> std::io::Error {
    let origin_requests = server.requests.lock().await.len();
    let session = context.session.read().await;
    let statuses = session.segments.iter().map(|(seq, entry)| (*seq, entry.status.clone())).collect::<Vec<_>>();
    let revisions = session.startup.as_ref().map(|startup| {
        startup
            .revisions
            .iter()
            .map(|(seq, owner)| (*seq, owner.revision().subscribe().borrow().clone()))
            .collect::<Vec<_>>()
    });
    std::io::Error::new(std::io::ErrorKind::TimedOut, format!(
        "{phase}: monotonic_ms={} wall_ms={} active={} origin_requests={} statuses={statuses:?} revisions={revisions:?}",
        started.elapsed().as_millis(), current_time_millis().saturating_sub(wall_started_at_ms),
        session.active_segment_fetches, origin_requests
    ))
}

pub(in crate::segment_fetcher::tests) async fn prefix_drop_rounds() -> std::io::Result<()> {
    use futures::StreamExt;
    for drop_body in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let (release, released) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut chunk = [0u8; 2048];
                let count = socket.read(&mut chunk).await?;
                if count == 0 {
                    return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "incomplete test request"));
                }
                request.extend_from_slice(&chunk[..count]);
            }
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\nsegment-").await?;
            let _ = released.await;
            if !drop_body {
                socket.write_all(b"body").await?;
            }
            Ok::<_, std::io::Error>(())
        });
        let server = TestSegmentServer {
            base_url: format!("http://{addr}"),
            requests: Arc::new(Mutex::new(Vec::new())),
            task: tokio::spawn(async {}),
        };
        let directory = tempfile::tempdir()?;
        let policy = SegmentFetchPolicy { retry_delays_ms: [0; 5], retry_jitter_max_ms: 0, ..Default::default() };
        let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
        install_startup(&context, &worker, shared::model::HlsStartupMode::Progressive).await;
        let snapshot = worker
            .next_fetch_snapshot(&context, 10, &policy)
            .await
            .ok_or_else(|| std::io::Error::other("fetch snapshot"))?;
        let fill_context = context.clone();
        let fill = tokio::spawn(async move {
            super::super::fetch_segment_with_retries_into_cache(&fill_context, &snapshot, &policy).await
        });
        let revision = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let found = context
                    .session
                    .read()
                    .await
                    .startup
                    .as_ref()
                    .and_then(|startup| startup.revisions.get(&1))
                    .cloned();
                if let Some(revision) = found.filter(|revision| revision.revision().is_publishable()) {
                    return revision;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(std::io::Error::other)?;
        let mut body =
            crate::progressive_startup::revision_body(revision.clone(), std::time::Duration::from_secs(2), 0, None);
        let prefix = tokio::time::timeout(std::time::Duration::from_secs(1), body.next())
            .await
            .map_err(std::io::Error::other)?
            .ok_or_else(|| std::io::Error::other("no progressive prefix"))??;
        assert_eq!(prefix, b"segment-".as_slice());
        assert!(!fill.is_finished());
        release.send(()).map_err(|()| std::io::Error::other("origin release"))?;
        task.await.map_err(std::io::Error::other)??;
        if drop_body {
            assert!(body.next().await.is_some_and(|chunk| chunk.is_err()));
            assert!(matches!(*revision.revision().subscribe().borrow(), crate::SegmentRevisionState::Failed));
            fill.abort();
            let _ = fill.await;
        } else {
            assert_eq!(body.next().await.transpose()?.as_deref(), Some(b"body".as_slice()));
            assert!(body.next().await.is_none());
            assert!(fill.await.map_err(std::io::Error::other)?.is_ok());
        }
    }
    Ok(())
}
