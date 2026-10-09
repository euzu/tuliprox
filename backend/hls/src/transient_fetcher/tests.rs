use super::{
    super::{
        origin_progress::{HlsOriginPathCondition, HlsOriginProgressPhase},
        resource_fetch::take_body_failure_log_attempts,
    },
    fetch_and_commit_hls_transient_origin_response_with_attempt_prepare,
    fetch_hls_transient_origin_response_with_attempt_prepare, hls_transient_object_fetch_failure,
    hls_transient_origin_response, record_temporary_transient_segment_fetch_failure,
    resolve_hls_transient_object_cache_action, HlsLogIdentity, HlsTransientCacheCommitContext,
    HlsTransientDecodedOriginResponse, HlsTransientDirectResponseContext, HlsTransientDirectResponseFinalizer,
    HlsTransientDirectResponseLifecycleContext, HlsTransientDirectStreamOutcome, HlsTransientObjectCacheAction,
    HlsTransientObjectFetchFailure, HlsTransientOriginCacheFetchRequest, HlsTransientOriginFetchRequest,
    HlsTransientOriginIoGuard, HlsTransientResourceLeaseContext,
};
use crate::{
    origin::{HlsOriginAccountBinding, HlsOriginAccountIoLease, HlsOriginAccountIoLeaseGuard, HlsOriginIoContext},
    HlsAccessLeaseId, HlsOriginResourceClients, HlsOriginResourceFetchError, HlsPublishedTransientResourceIds,
    HlsSegmentCache, HlsSegmentFailureObject, HlsSegmentRepairManager, HlsSession, HlsSessionHandle, HlsSessionKey,
    HlsSessionStore, ProxySessionId, SegmentFetchPolicy, TransientObjectCacheKey, TransientObjectCacheStatus,
    TransientObjectFetchDecision, TransientObjectFetchToken, TransientPassthroughState, TransientResourceFile,
    TransientResourceKind, TransientResourceRef,
};
use async_compression::tokio::write::{BrotliEncoder, GzipEncoder, ZstdEncoder};
use axum::{
    body::to_bytes,
    http::{header, HeaderMap, HeaderValue, StatusCode},
};
use futures::FutureExt;
use shared::model::HlsSegmentRepairMode;
use std::{
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{Mutex, RwLock},
};
use tuliprox_core::{model::HlsSegmentRepairConfig, utils::response_compression::should_compress_response};

struct TestOrigin {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TestOrigin {
    fn drop(&mut self) { self.task.abort(); }
}

fn finalized_resource_resolution_fixture(
) -> (HlsSessionHandle, ProxySessionId, HlsAccessLeaseId, TransientResourceFile, HlsPublishedTransientResourceIds) {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "finalized-range"), b"rewrite-secret", 0);
    let proxy_session_id = session.proxy_session_id.clone();
    let access_lease_id = HlsAccessLeaseId("lease".to_string());
    let resource = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "http://origin.example.com/archive/segment.ts",
        b"rewrite-secret",
        0,
        300_000,
        Some("ts".to_string()),
    );
    let resource_file = TransientResourceFile { resource_id: resource.id.clone(), extension: "ts".to_string() };
    session.transient.upsert_resources([resource]);
    let manifest_body = format!(
        "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXTINF:1,\n/hls/shared/live/{}/lease/r/{}.ts\n#EXT-X-ENDLIST\n",
        proxy_session_id.0, resource_file.resource_id.0
    );
    let published_resource_ids = HlsPublishedTransientResourceIds::from_manifest_body(&manifest_body);
    session.transient.replace_manifest_with_semantics(manifest_body, 0, Some(1_000));
    let manifest_generation =
        session.transient.current_finalized_manifest_generation().expect("finalized manifest generation");
    assert!(session.transient.bind_finalized_manifest_generation(super::super::TransientManifestLeaseBinding::new(
        access_lease_id.clone(),
        0,
        manifest_generation,
    )));
    (Arc::new(RwLock::new(session)), proxy_session_id, access_lease_id, resource_file, published_resource_ids)
}

async fn spawn_test_origin(
    status_line: &'static str,
    response_headers: Vec<(&'static str, &'static str)>,
    body: Vec<u8>,
) -> TestOrigin {
    spawn_test_origin_in_chunks(status_line, response_headers, vec![body], Duration::ZERO).await
}

async fn spawn_test_origin_in_chunks(
    status_line: &'static str,
    response_headers: Vec<(&'static str, &'static str)>,
    body_chunks: Vec<Vec<u8>>,
    inter_chunk_delay: Duration,
) -> TestOrigin {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
    let addr = listener.local_addr().expect("test origin address");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let task_requests = Arc::clone(&requests);
    let body_chunks = Arc::new(body_chunks);
    let content_length = body_chunks.iter().map(Vec::len).sum::<usize>();
    let response_headers = Arc::new(response_headers);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let requests = Arc::clone(&task_requests);
            let body_chunks = Arc::clone(&body_chunks);
            let response_headers = Arc::clone(&response_headers);
            tokio::spawn(async move {
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
                let mut response = format!("HTTP/1.1 {status_line}\r\nContent-Length: {content_length}\r\n");
                for (name, value) in response_headers.iter() {
                    response.push_str(name);
                    response.push_str(": ");
                    response.push_str(value);
                    response.push_str("\r\n");
                }
                response.push_str("Connection: close\r\n\r\n");
                let _ = socket.write_all(response.as_bytes()).await;
                for (index, chunk) in body_chunks.iter().enumerate() {
                    if index > 0 && !inter_chunk_delay.is_zero() {
                        tokio::time::sleep(inter_chunk_delay).await;
                    }
                    if socket.write_all(chunk).await.is_err() {
                        return;
                    }
                }
            });
        }
    });

    TestOrigin { base_url: format!("http://{addr}"), requests, task }
}

async fn spawn_retry_then_body_origin(
    response_headers: Vec<(&'static str, &'static str)>,
    body: Vec<u8>,
) -> TestOrigin {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
    let addr = listener.local_addr().expect("test origin address");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let task_requests = Arc::clone(&requests);
    let attempts = Arc::new(AtomicUsize::new(0));
    let task_attempts = Arc::clone(&attempts);
    let body = Arc::new(body);
    let response_headers = Arc::new(response_headers);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let requests = Arc::clone(&task_requests);
            let attempts = Arc::clone(&task_attempts);
            let body = Arc::clone(&body);
            let response_headers = Arc::clone(&response_headers);
            tokio::spawn(async move {
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
                if attempts.fetch_add(1, Ordering::Relaxed) == 0 {
                    let _ = socket
                        .write_all(
                            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                    return;
                }
                let mut response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n", body.len());
                for (name, value) in response_headers.iter() {
                    response.push_str(name);
                    response.push_str(": ");
                    response.push_str(value);
                    response.push_str("\r\n");
                }
                response.push_str("Connection: close\r\n\r\n");
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.write_all(&body).await;
            });
        }
    });

    TestOrigin { base_url: format!("http://{addr}"), requests, task }
}

async fn spawn_zstd_origin(body: Vec<u8>) -> TestOrigin {
    spawn_test_origin("200 OK", vec![("Content-Encoding", "zstd"), ("Content-Type", "application/octet-stream")], body)
        .await
}

async fn gzip_encode(body: &[u8]) -> Vec<u8> {
    let mut encoder = GzipEncoder::new(Vec::new());
    encoder.write_all(body).await.expect("test body encodes");
    encoder.shutdown().await.expect("test encoder finishes");
    encoder.into_inner()
}

async fn brotli_encode(body: &[u8]) -> Vec<u8> {
    let mut encoder = BrotliEncoder::new(Vec::new());
    encoder.write_all(body).await.expect("test body encodes");
    encoder.shutdown().await.expect("test encoder finishes");
    encoder.into_inner()
}

async fn zstd_encode(body: &[u8]) -> Vec<u8> {
    let mut encoder = ZstdEncoder::new(Vec::new());
    encoder.write_all(body).await.expect("test body encodes");
    encoder.shutdown().await.expect("test encoder finishes");
    encoder.into_inner()
}

async fn fetch_direct_decoded(
    origin_url: String,
    resource_kind: TransientResourceKind,
    range_header: Option<HeaderValue>,
) -> Result<HlsTransientDecodedOriginResponse<Option<HlsTransientOriginIoGuard>>, HlsOriginResourceFetchError> {
    fetch_direct_decoded_with_timeout(origin_url, resource_kind, range_header, 1_000).await
}

async fn fetch_direct_decoded_with_timeout(
    origin_url: String,
    resource_kind: TransientResourceKind,
    range_header: Option<HeaderValue>,
    origin_segment_timeout_ms: u64,
) -> Result<HlsTransientDecodedOriginResponse<Option<HlsTransientOriginIoGuard>>, HlsOriginResourceFetchError> {
    let resource =
        TransientResourceRef::new(resource_kind, origin_url, b"rewrite-secret", 10, 60_000, Some("bin".to_string()));
    let request = HlsTransientOriginFetchRequest {
        resolved_origin_uri: resource.resolved_origin_uri.clone(),
        origin_headers: HeaderMap::new(),
        origin_provider_session_headers: HeaderMap::new(),
        range_header,
        resource_file: TransientResourceFile { resource_id: resource.id, extension: "bin".to_string() },
        resource_kind,
        clients: HlsOriginResourceClients {
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("no-redirect client builds"),
            use_manual_redirects: false,
        },
        policy: SegmentFetchPolicy {
            origin_segment_timeout_ms,
            retry_delays_ms: [0; 5],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        },
        log_identity: HlsLogIdentity::for_test("direct-transient-content-session", "direct-transient-proxy-session"),
    };
    fetch_hls_transient_origin_response_with_attempt_prepare(request, |_| async { Ok(None) }.boxed()).await
}

fn direct_client_response(
    response: HlsTransientDecodedOriginResponse<Option<HlsTransientOriginIoGuard>>,
    resource_kind: TransientResourceKind,
) -> axum::response::Response {
    TestDirectResponseFixture::new(resource_kind, "http://127.0.0.1/origin").response(response)
}

struct TestDirectResponseFixture {
    session: HlsSessionHandle,
    resource: TransientResourceRef,
    policy: SegmentFetchPolicy,
    log_identity: HlsLogIdentity,
}

impl TestDirectResponseFixture {
    fn new(resource_kind: TransientResourceKind, origin_url: impl Into<String>) -> Self {
        let resource = TransientResourceRef::new(
            resource_kind,
            origin_url,
            b"rewrite-secret",
            tuliprox_core::utils::current_time_millis(),
            60_000,
            Some("bin".to_string()),
        );
        let policy =
            SegmentFetchPolicy { retry_delays_ms: [0; 5], retry_jitter_max_ms: 0, ..SegmentFetchPolicy::default() };
        let mut session = HlsSession::new(HlsSessionKey::new(1, "direct-test"), b"rewrite-secret", 10);
        session.transient.upsert_resources([resource.clone()]);
        let log_identity = HlsLogIdentity::from_session(&session);
        Self { session: Arc::new(RwLock::new(session)), resource, policy, log_identity }
    }

    fn response(
        &self,
        response: HlsTransientDecodedOriginResponse<Option<HlsTransientOriginIoGuard>>,
    ) -> axum::response::Response {
        hls_transient_origin_response(response, self.response_context())
    }

    fn response_context(&self) -> HlsTransientDirectResponseContext {
        HlsTransientDirectResponseContext {
            session: Arc::clone(&self.session),
            resource: self.resource.clone(),
            policy: self.policy.clone(),
            now_ms: 10,
            log_identity: self.log_identity.clone(),
        }
    }

    fn lifecycle_context(&self) -> HlsTransientDirectResponseLifecycleContext {
        HlsTransientDirectResponseLifecycleContext {
            session: Arc::clone(&self.session),
            resource: self.resource.clone(),
            policy: self.policy.clone(),
        }
    }

    async fn seed_segment_failure(&self) {
        assert!(
            !record_temporary_transient_segment_fetch_failure(&self.session, &self.resource, &self.policy, 9).await
        );
    }

    async fn segment_failure_count(&self) -> u32 {
        self.session.read().await.segment_failure_tracker.consecutive_temporary_failures
    }

    async fn wait_for_segment_failure_count(&self, expected: u32) {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if self.segment_failure_count().await == expected {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("segment failure state reaches expected value");
    }

    async fn last_segment_failure(&self) -> Option<HlsSegmentFailureObject> {
        self.session.read().await.segment_failure_tracker.last_failed_object.clone()
    }
}

struct TestTransientCacheFixture {
    temp_dir: tempfile::TempDir,
    segment_cache: Arc<HlsSegmentCache>,
    segment_repair: Arc<HlsSegmentRepairManager>,
    session: HlsSessionHandle,
    log_identity: HlsLogIdentity,
    resource: TransientResourceRef,
    resource_file: TransientResourceFile,
    lookup_key: TransientObjectCacheKey,
    cache_key: TransientObjectCacheKey,
    fetch_token: TransientObjectFetchToken,
}

impl TestTransientCacheFixture {
    async fn new(origin_url: String) -> Self {
        let now_ms = tuliprox_core::utils::current_time_millis();
        let resource = TransientResourceRef::new(
            TransientResourceKind::Segment,
            origin_url,
            b"rewrite-secret",
            now_ms,
            60_000,
            Some("ts".to_string()),
        );
        let resource_file = TransientResourceFile { resource_id: resource.id.clone(), extension: "ts".to_string() };
        let store = HlsSessionStore::new();
        let session = store.get_or_create_session(HlsSessionKey::new(1, "1"), b"rewrite-secret", 10).await;
        let (proxy_session_id, log_identity) = {
            let session = session.read().await;
            (session.proxy_session_id.clone(), HlsLogIdentity::from_session(&session))
        };
        let lookup_key = TransientPassthroughState::transient_object_key(
            &proxy_session_id,
            &resource.id,
            resource_file.extension.clone(),
        );
        let (resource, fetch_token) = {
            let mut session = session.write().await;
            session.transient.upsert_resources([resource]);
            let resource = session
                .transient
                .resources
                .get(&resource_file.resource_id)
                .expect("registered transient resource")
                .clone();
            match session.transient.begin_object_fetch(&proxy_session_id, &resource, "ts", now_ms, 60_000) {
                TransientObjectFetchDecision::Fetch(fetch_token) => (resource, *fetch_token),
                TransientObjectFetchDecision::Ready | TransientObjectFetchDecision::Wait(_) => {
                    panic!("new transient resource should start a cache fetch")
                }
            }
        };
        let cache_key = fetch_token.cache_key().clone();
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let segment_cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
        let segment_repair = Arc::new(HlsSegmentRepairManager::new(HlsSegmentRepairConfig {
            max_level: HlsSegmentRepairMode::Off,
            apply_to_first_segments: 1,
            max_parallel_repairs: 1,
            ..Default::default()
        }));
        Self {
            temp_dir,
            segment_cache,
            segment_repair,
            session,
            log_identity,
            resource,
            resource_file,
            lookup_key,
            cache_key,
            fetch_token,
        }
    }

    fn request(&self, range_header: Option<HeaderValue>) -> HlsTransientOriginCacheFetchRequest {
        HlsTransientOriginCacheFetchRequest {
            fetch: HlsTransientOriginFetchRequest {
                resolved_origin_uri: self.resource.resolved_origin_uri.clone(),
                origin_headers: HeaderMap::new(),
                origin_provider_session_headers: HeaderMap::new(),
                range_header,
                resource_file: self.resource_file.clone(),
                resource_kind: self.resource.kind,
                clients: HlsOriginResourceClients {
                    client: reqwest::Client::new(),
                    no_redirect_client: reqwest::Client::builder()
                        .redirect(reqwest::redirect::Policy::none())
                        .build()
                        .expect("no-redirect client builds"),
                    use_manual_redirects: false,
                },
                policy: SegmentFetchPolicy {
                    origin_segment_timeout_ms: 1_000,
                    retry_delays_ms: [0; 5],
                    retry_jitter_max_ms: 0,
                    ..SegmentFetchPolicy::default()
                },
                log_identity: self.log_identity.clone(),
            },
            commit: HlsTransientCacheCommitContext {
                segment_cache: Arc::clone(&self.segment_cache),
                segment_repair: Arc::clone(&self.segment_repair),
                session: Arc::clone(&self.session),
                proxy_session_id: ProxySessionId(String::from("test-proxy-session")),
                log_identity: self.log_identity.clone(),
                access_lease_id: HlsAccessLeaseId("transient-content-coding-test".to_string()),
                resource: self.resource.clone(),
                resource_file: self.resource_file.clone(),
                fetch_token: self.fetch_token.clone(),
                cache_duration_ms: 60_000,
            },
        }
    }
}

struct DropCounter(Arc<AtomicUsize>);

impl Drop for DropCounter {
    fn drop(&mut self) { self.0.fetch_add(1, Ordering::Relaxed); }
}

mod cache;
mod decoding;
mod lifecycle;
