use super::{
    begin_hls_origin_account_io_bounded, build_hls_origin_resource_headers, finish_hls_origin_account_io,
    hls_object_body_deadline, run_hls_origin_resource_retry_loop_with_attempt_prepare, CachedSegmentMetadata,
    HlsAccessLeaseStore, HlsBoundAccountAcquireErrorKind, HlsOriginAccountIoLeaseGuard, HlsOriginByteRangeExpectation,
    HlsOriginIoContext, HlsOriginResourceBodyDeadline, HlsOriginResourceClients, HlsOriginResourceFetchError,
    HlsOriginResourceFetchTarget, HlsResourceFetchKind, HlsResourceFetchSource, HlsSegmentCache, HlsSessionHandle,
    MapCacheKey, MapCacheStatus, OriginMapFetchRef, ProxyMapId, SegmentFetchPolicy,
};
use crate::{processing::parser::hls::origin_manifest::ParsedByteRange, utils::content_coding::DecodedHttpResponse};
use arc_swap::ArcSwap;
use axum::http::HeaderMap;
use futures::FutureExt;
use log::debug;
use reqwest::Client;
use std::{fmt, sync::Arc};
use tokio::sync::{OwnedSemaphorePermit, RwLock, Semaphore};

/// Shared context required to schedule EXT-X-MAP origin fetches without holding session locks.
#[derive(Clone)]
pub struct MapFetchContext {
    pub session: HlsSessionHandle,
    pub segment_cache: Arc<HlsSegmentCache>,
    pub headers: HeaderMap,
    pub origin_provider_session_headers: HeaderMap,
    pub client: Client,
    pub no_redirect_client: Client,
    pub use_manual_redirects: bool,
    pub origin_io: Option<HlsOriginIoContext>,
}

#[derive(Clone)]
struct MapFetchSnapshot {
    proxy_map_id: ProxyMapId,
    proxy_map_id_log: String,
    cache_key: MapCacheKey,
    fetch_ref: OriginMapFetchRef,
    origin_work_generation: u64,
}

struct MapFetchCommit {
    content_length: u64,
    generation_valid: bool,
}

#[derive(Clone, Copy)]
struct MapOriginWorkFinish {
    generation_valid: bool,
    refresh_reservation: bool,
}

impl fmt::Debug for MapFetchSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MapFetchSnapshot")
            .field("proxy_map_id", &self.proxy_map_id)
            .field("proxy_map_id_log", &self.proxy_map_id_log)
            .field("cache_key", &self.cache_key)
            .field("fetch_ref", &self.fetch_ref)
            .field("origin_work_generation", &self.origin_work_generation)
            .finish()
    }
}

type MapFetchError = HlsOriginResourceFetchError;

#[derive(Clone)]
struct MapWorkerRuntime {
    global_semaphore: Arc<Semaphore>,
    policy: SegmentFetchPolicy,
}

impl MapWorkerRuntime {
    fn new(policy: SegmentFetchPolicy, global_semaphore: Arc<Semaphore>) -> Self { Self { global_semaphore, policy } }
}

/// Bounded scheduler for live HLS EXT-X-MAP origin fetches.
pub struct HlsMapWorkerPool {
    runtime: ArcSwap<MapWorkerRuntime>,
    access_leases: Arc<RwLock<HlsAccessLeaseStore>>,
    availability_reevaluations: Option<Arc<super::availability_reevaluation::HlsAvailabilityReevaluationCoordinator>>,
}

impl HlsMapWorkerPool {
    pub fn new(policy: SegmentFetchPolicy) -> Self {
        let global_semaphore = Arc::new(Semaphore::new(policy.max_global_segment_fetches));
        Self::with_global_semaphore(policy, global_semaphore)
    }

    pub fn with_global_semaphore(policy: SegmentFetchPolicy, global_semaphore: Arc<Semaphore>) -> Self {
        Self::with_global_semaphore_and_access_leases(
            policy,
            global_semaphore,
            Arc::new(RwLock::new(HlsAccessLeaseStore::default())),
        )
    }

    pub fn with_global_semaphore_and_access_leases(
        policy: SegmentFetchPolicy,
        global_semaphore: Arc<Semaphore>,
        access_leases: Arc<RwLock<HlsAccessLeaseStore>>,
    ) -> Self {
        Self::with_global_semaphore_access_leases_and_availability(
            policy,
            global_semaphore,
            access_leases,
            None,
        )
    }

    pub(crate) fn with_global_semaphore_access_leases_and_availability(
        policy: SegmentFetchPolicy,
        global_semaphore: Arc<Semaphore>,
        access_leases: Arc<RwLock<HlsAccessLeaseStore>>,
        availability_reevaluations: Option<
            Arc<super::availability_reevaluation::HlsAvailabilityReevaluationCoordinator>,
        >,
    ) -> Self {
        Self {
            runtime: ArcSwap::from_pointee(MapWorkerRuntime::new(policy, global_semaphore)),
            access_leases,
            availability_reevaluations,
        }
    }

    pub fn update_config(&self, policy: SegmentFetchPolicy, global_semaphore: Arc<Semaphore>) {
        self.runtime.store(Arc::new(MapWorkerRuntime::new(policy, global_semaphore)));
    }

    pub fn access_leases(&self) -> &Arc<RwLock<HlsAccessLeaseStore>> { &self.access_leases }

    pub async fn wake_scheduler(self: &Arc<Self>, context: MapFetchContext, now_ms: u64) {
        loop {
            let runtime = self.runtime.load_full();
            let Ok(permit) = Arc::clone(&runtime.global_semaphore).try_acquire_owned() else {
                return;
            };
            let Some(snapshot) = self.next_fetch_snapshot(&context, now_ms, &runtime.policy).await else {
                drop(permit);
                return;
            };

            let worker = Arc::clone(self);
            let task_context = context.clone();
            tokio::spawn(async move {
                worker.fetch_one_map(task_context, snapshot, runtime.policy.clone(), permit).await;
            });
        }
    }

    async fn next_fetch_snapshot(
        &self,
        context: &MapFetchContext,
        now_ms: u64,
        policy: &SegmentFetchPolicy,
    ) -> Option<MapFetchSnapshot> {
        let (proxy_session_id, gc_marked_for_removal) = {
            let session = context.session.read().await;
            (session.proxy_session_id.clone(), session.is_gc_marked_for_removal())
        };
        if gc_marked_for_removal {
            return None;
        }
        if !self.access_leases.write().await.has_usable_access_lease_for_session(&proxy_session_id, now_ms) {
            let mut session = context.session.write().await;
            for map in session.maps.values_mut() {
                if matches!(map.status, MapCacheStatus::Queued { .. }) {
                    map.status = MapCacheStatus::Discovered;
                }
            }
            return None;
        }
        let mut session = context.session.write().await;
        if session.is_gc_marked_for_removal() {
            return None;
        }
        if session.active_map_fetches >= policy.max_session_segment_fetches {
            return None;
        }

        let proxy_map_id = session.maps.iter().find_map(|(proxy_map_id, entry)| {
            matches!(entry.status, MapCacheStatus::Discovered | MapCacheStatus::Queued { .. }).then_some(*proxy_map_id)
        })?;

        let origin_work_generation = session.activity.origin_work_generation;
        let entry = session.maps.get_mut(&proxy_map_id)?;
        let fetch_ref = entry.origin_fetch_ref.clone()?;
        if !fetch_ref.is_valid_at(now_ms) {
            return None;
        }
        let cache_key = entry.cache_key.clone();
        entry.status = MapCacheStatus::Fetching { started_at_ms: now_ms };
        session.active_map_fetches = session.active_map_fetches.saturating_add(1);
        let proxy_map_id_log = format!("{:06}", proxy_map_id.0);
        if log::log_enabled!(log::Level::Debug) {
            let identity = super::HlsLogIdentity::from_session(&session);
            debug!(
                "HLS map fetch started: session={} proxy_session={} source=normal resource=map/{}",
                identity.session(),
                identity.proxy_session(),
                proxy_map_id_log
            );
        }

        Some(MapFetchSnapshot { proxy_map_id, proxy_map_id_log, cache_key, fetch_ref, origin_work_generation })
    }

    async fn fetch_one_map(
        self: Arc<Self>,
        context: MapFetchContext,
        snapshot: MapFetchSnapshot,
        policy: SegmentFetchPolicy,
        permit: OwnedSemaphorePermit,
    ) {
        let result = fetch_map_into_cache(&context, &snapshot, &policy).await;
        let finished_at_ms = current_time_millis();
        let fetch_succeeded = result.is_ok();
        let (generation_valid, evidence_changed_for) = {
            let mut session = context.session.write().await;
            session.active_map_fetches = session.active_map_fetches.saturating_sub(1);
            let generation_valid = session.activity.origin_work_generation == snapshot.origin_work_generation
                && result.as_ref().map_or(true, |commit| commit.generation_valid)
                && session.maps.get(&snapshot.proxy_map_id).is_some_and(|entry| {
                    entry.cache_key == snapshot.cache_key && matches!(entry.status, MapCacheStatus::Fetching { .. })
                });
            if generation_valid && result.is_err() {
                session.origin_control.path_condition =
                    super::origin_progress::HlsOriginPathCondition::SegmentReadinessFailure;
            }
            if let Some(entry) = session.maps.get_mut(&snapshot.proxy_map_id) {
                if generation_valid {
                    match result {
                        Ok(commit) => {
                            let content_length = commit.content_length;
                            entry.status = MapCacheStatus::Ready { content_length, ready_at_ms: finished_at_ms };
                            if log::log_enabled!(log::Level::Debug) {
                                let identity = super::HlsLogIdentity::from_session(&session);
                                debug!(
                                    "HLS map cached: session={} proxy_session={} source=normal resource=map/{} content_length={content_length}",
                                    identity.session(),
                                    identity.proxy_session(),
                                    snapshot.proxy_map_id_log
                                );
                            }
                            session.advance_media_readiness_generation();
                        }
                        Err(err) => {
                            if err.retryable_failure() {
                                entry.status = MapCacheStatus::FailedRetryable {
                                    failed_at_ms: finished_at_ms,
                                    retry_after_ms: 1_000,
                                };
                            } else {
                                entry.status = MapCacheStatus::FailedPermanent {
                                    failed_at_ms: finished_at_ms,
                                    status: err.permanent_status(),
                                };
                            }
                        }
                    }
                } else if entry.cache_key == snapshot.cache_key
                    && matches!(entry.status, MapCacheStatus::Fetching { .. })
                {
                    entry.status = MapCacheStatus::Discovered;
                }
            }
            if fetch_succeeded && generation_valid {
                if let Err(err) = session.render_and_store_manifest(finished_at_ms) {
                    if log::log_enabled!(log::Level::Debug) {
                        let identity = super::HlsLogIdentity::from_session(&session);
                        debug!(
                            "HLS manifest render deferred after map readiness: session={} proxy_session={} resource=map/{} error={err:?}",
                            identity.session(),
                            identity.proxy_session(),
                            snapshot.proxy_map_id_log
                        );
                    }
                }
            }
            (
                generation_valid,
                generation_valid.then(|| session.proxy_session_id.clone()),
            )
        };
        if fetch_succeeded && !generation_valid {
            if let Err(err) = context.segment_cache.delete(&snapshot.cache_key).await {
                debug!("HLS stale map cache cleanup failed: resource=map/{} error={err}", snapshot.proxy_map_id_log);
            }
        }
        if let (Some(coordinator), Some(proxy_session_id)) = (
            self.availability_reevaluations.as_ref(),
            evidence_changed_for.as_ref(),
        ) {
            coordinator.notify_session_evidence_changed(proxy_session_id);
        }
        drop(permit);
        if generation_valid {
            self.schedule_wake(context, finished_at_ms);
        }
    }

    fn schedule_wake(self: &Arc<Self>, context: MapFetchContext, now_ms: u64) {
        let worker = Arc::clone(self);
        tokio::spawn(async move {
            worker.wake_scheduler(context, now_ms).await;
        });
    }
}

impl Default for HlsMapWorkerPool {
    fn default() -> Self { Self::new(SegmentFetchPolicy::default()) }
}

async fn fetch_map_into_cache(
    context: &MapFetchContext,
    snapshot: &MapFetchSnapshot,
    policy: &SegmentFetchPolicy,
) -> Result<MapFetchCommit, MapFetchError> {
    fetch_map_with_retries_into_cache(context, snapshot, policy).await
}

struct MapOriginAttemptGuard {
    started_generation: Option<u64>,
    provider_lease: Option<(HlsOriginIoContext, HlsOriginAccountIoLeaseGuard)>,
}

async fn prepare_map_origin_attempt(
    context: MapFetchContext,
    policy: SegmentFetchPolicy,
) -> Result<MapOriginAttemptGuard, MapFetchError> {
    let started_generation = start_map_origin_work(&context).await;
    let binding =
        if context.origin_io.is_some() { context.session.read().await.origin_account_binding.clone() } else { None };
    let provider_lease = if let (Some(origin_io), Some(binding)) = (context.origin_io.as_ref(), binding.as_ref()) {
        if binding.is_detached() {
            finish_map_origin_work(&context, started_generation).await;
            touch_map_origin_account_binding(&context, false).await;
            return Err(MapFetchError::ProviderUnavailable(HlsBoundAccountAcquireErrorKind::Detached));
        }
        let guard = match begin_hls_origin_account_io_bounded(
            origin_io,
            &context.session,
            binding,
            hls_object_body_deadline(policy.origin_segment_timeout_ms),
        )
        .await
        {
            Ok(guard) => guard,
            Err(err) => {
                finish_map_origin_work(&context, started_generation).await;
                touch_map_origin_account_binding(&context, false).await;
                return Err(MapFetchError::ProviderUnavailable(err));
            }
        };
        Some((origin_io.clone(), guard))
    } else {
        None
    };
    Ok(MapOriginAttemptGuard { started_generation, provider_lease })
}

async fn finish_map_origin_attempt(context: MapFetchContext, guard: MapOriginAttemptGuard) -> MapOriginWorkFinish {
    finish_map_origin_io(&context, guard.started_generation, guard.provider_lease).await
}

async fn finish_map_origin_io(
    context: &MapFetchContext,
    started_generation: Option<u64>,
    provider_lease: Option<(HlsOriginIoContext, HlsOriginAccountIoLeaseGuard)>,
) -> MapOriginWorkFinish {
    let origin_work = finish_map_origin_work(context, started_generation).await;
    if let Some((origin_io, guard)) = provider_lease {
        finish_hls_origin_account_io(
            &origin_io,
            &context.session,
            guard,
            origin_work.generation_valid && origin_work.refresh_reservation,
        )
        .await;
        touch_map_origin_account_binding(context, origin_work.generation_valid && origin_work.refresh_reservation)
            .await;
    }
    origin_work
}

async fn start_map_origin_work(context: &MapFetchContext) -> Option<u64> {
    context.origin_io.as_ref()?;
    let mut session = context.session.write().await;
    Some(session.start_origin_work())
}

async fn finish_map_origin_work(context: &MapFetchContext, started_generation: Option<u64>) -> MapOriginWorkFinish {
    let Some(started_generation) = started_generation else {
        return MapOriginWorkFinish { generation_valid: true, refresh_reservation: false };
    };
    let mut session = context.session.write().await;
    let generation_valid = session.finish_origin_work(started_generation);
    let refresh_reservation = session.should_refresh_origin_reservation(current_time_millis());
    MapOriginWorkFinish { generation_valid, refresh_reservation }
}

async fn touch_map_origin_account_binding(context: &MapFetchContext, reservation_refreshed: bool) {
    let mut session = context.session.write().await;
    if let Some(binding) = session.origin_account_binding.as_mut() {
        let now_ms = current_time_millis();
        binding.last_origin_io_at_ms = Some(now_ms);
        if reservation_refreshed {
            binding.last_reservation_refresh_at_ms = Some(now_ms);
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn fetch_map_with_retries_into_cache(
    context: &MapFetchContext,
    snapshot: &MapFetchSnapshot,
    policy: &SegmentFetchPolicy,
) -> Result<MapFetchCommit, MapFetchError> {
    let headers = build_map_origin_headers(
        &context.headers,
        &context.origin_provider_session_headers,
        snapshot.fetch_ref.byte_range,
    )?;
    let target = HlsOriginResourceFetchTarget {
        kind: HlsResourceFetchKind::Map,
        source: HlsResourceFetchSource::Normal,
        object_id: snapshot.proxy_map_id_log.clone(),
        origin_url: snapshot.fetch_ref.resolved_origin_url.clone(),
        headers,
        byte_range_expectation: if snapshot.fetch_ref.byte_range.is_some() {
            HlsOriginByteRangeExpectation::PartialContent
        } else {
            HlsOriginByteRangeExpectation::FullObject
        },
    };
    let clients = HlsOriginResourceClients {
        client: context.client.clone(),
        no_redirect_client: context.no_redirect_client.clone(),
        use_manual_redirects: context.use_manual_redirects,
    };
    let log_identity = {
        let session = context.session.read().await;
        super::HlsLogIdentity::from_session(&session)
    };
    let context = context.clone();
    let snapshot = snapshot.clone();
    let policy_for_prepare = policy.clone();
    let prepare_context = context.clone();
    let cleanup_context = context.clone();
    run_hls_origin_resource_retry_loop_with_attempt_prepare(
        target,
        clients,
        policy,
        &log_identity,
        move |_attempt| {
            let context = prepare_context.clone();
            let policy = policy_for_prepare.clone();
            async move { prepare_map_origin_attempt(context, policy).await }.boxed()
        },
        move |guard| {
            let context = cleanup_context.clone();
            async move {
                finish_map_origin_attempt(context, guard).await;
            }
            .boxed()
        },
        move |response, _attempt, body_deadline, guard| {
            let context = context.clone();
            let snapshot = snapshot.clone();
            async move {
                let commit_result = commit_map_response_into_cache(&context, &snapshot, response, body_deadline).await;
                let origin_work = finish_map_origin_attempt(context, guard).await;
                commit_result.map(|metadata| MapFetchCommit {
                    content_length: metadata.size,
                    generation_valid: origin_work.generation_valid,
                })
            }
            .boxed()
        },
    )
    .await
}

async fn commit_map_response_into_cache(
    context: &MapFetchContext,
    snapshot: &MapFetchSnapshot,
    response: DecodedHttpResponse,
    body_deadline: HlsOriginResourceBodyDeadline,
) -> Result<CachedSegmentMetadata, MapFetchError> {
    context
        .segment_cache
        .write_temp_and_commit_with_deadline(&snapshot.cache_key, response.body, body_deadline.deadline())
        .await
        .map_err(|err| MapFetchError::cache_body(&err))
}

fn build_map_origin_headers(
    source_headers: &HeaderMap,
    provider_session_headers: &HeaderMap,
    byte_range: Option<ParsedByteRange>,
) -> Result<HeaderMap, MapFetchError> {
    build_hls_origin_resource_headers(source_headers, provider_session_headers, byte_range)
}

fn current_time_millis() -> u64 { chrono::Utc::now().timestamp_millis().try_into().unwrap_or_default() }

#[cfg(test)]
mod tests {
    use super::{
        super::{
            availability_reevaluation::{
                HlsAvailabilityReevaluationCoordinator, HlsAvailabilityReevaluationMode,
                HlsAvailabilityReevaluationObservation, HlsAvailabilityReevaluationOwnerKey,
                HlsAvailabilityReevaluationRegistration,
            },
            lease::HlsAvailabilityEvidenceGeneration,
            session_store::HlsSessionIncarnation,
        },
        super::origin_progress::{HlsOriginPathCondition, HlsOriginProgressPhase},
        build_map_origin_headers, HlsMapWorkerPool, MapFetchContext,
    };
    use crate::{
        api::model::{
            HlsAccessLease, HlsAccessLeaseId, HlsAccessLeaseStore, HlsPlaybackFamilyKey, HlsSegmentCache,
            HlsSessionKey, HlsSessionStore, MapCacheStatus, ProxySessionId, SegmentFetchPolicy,
        },
        processing::parser::hls::origin_manifest::{
            parse_origin_media_manifest, OriginManifestParseOutcome, ParsedByteRange,
        },
    };
    use axum::http::{header, HeaderMap};
    use flate2::{write::GzEncoder, Compression};
    use std::{io::Write, sync::Arc, time::Duration};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::{oneshot, Notify, RwLock, Semaphore},
    };

    fn normal_manifest(
        body: &str,
        base_url: &str,
    ) -> crate::processing::parser::hls::origin_manifest::ParsedOriginManifest {
        match parse_origin_media_manifest(body, base_url) {
            OriginManifestParseOutcome::Normal(manifest) => manifest,
            OriginManifestParseOutcome::TransientPassthrough { reason } => {
                panic!("expected normal manifest: {reason:?}")
            }
        }
    }

    async fn grant_usable_map_access_lease(worker: &HlsMapWorkerPool, proxy_session_id: &ProxySessionId) {
        worker.access_leases().write().await.prepare_access_lease(HlsAccessLease::pending(
            HlsAccessLeaseId("map-lease".to_string()),
            HlsPlaybackFamilyKey::new("alice", "client-a"),
            proxy_session_id.clone(),
            "alice".to_string(),
            "session-a".to_string(),
            1,
            "12345".to_string(),
            12345,
            1,
            15_000,
        ));
    }

    #[test]
    fn map_origin_headers_apply_byterange() {
        let mut source_headers = HeaderMap::new();
        source_headers.insert(header::AUTHORIZATION, "Bearer secret".parse().expect("header value"));
        source_headers.insert(header::COOKIE, "sid=secret".parse().expect("header value"));
        source_headers.insert(header::HOST, "origin.example.com".parse().expect("header value"));
        source_headers.insert(header::RANGE, "bytes=0-".parse().expect("header value"));
        let headers = build_map_origin_headers(
            &source_headers,
            &HeaderMap::new(),
            Some(ParsedByteRange { offset: 10, length: 5 }),
        )
        .expect("headers should build");

        assert_eq!(headers.get(header::RANGE).expect("range"), "bytes=10-14");
        assert_eq!(headers.get(header::ACCEPT_ENCODING).expect("encoding"), "identity");
        assert!(!headers.contains_key(header::AUTHORIZATION));
        assert!(!headers.contains_key(header::COOKIE));
        assert!(!headers.contains_key(header::HOST));
    }

    #[tokio::test]
    async fn map_fetch_writes_cache_and_sets_ready_after_commit() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
        let addr = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0_u8; 2048];
            let read = socket.read(&mut buf).await.expect("request reads");
            let request = String::from_utf8_lossy(&buf[..read]);
            let body = if request.to_ascii_lowercase().contains("range: bytes=10-14") { "map!!" } else { "bad" };
            let status = if body == "map!!" { "206 Partial Content" } else { "200 OK" };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Range: bytes 10-14/20\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.expect("response writes");
        });
        let base_url = format!("http://{addr}/live/index.m3u8");
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"5@10\"\n#EXTINF:4.0,\n1.m4s\n",
            &base_url,
        );
        let store = HlsSessionStore::new();
        let session = store.get_or_create_session(HlsSessionKey::new(1, "1"), b"secret", 0).await;
        {
            let mut session = session.write().await;
            session.apply_origin_manifest(&manifest).expect("manifest maps");
            session.queue_map_fetch_candidates(1);
        }

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path().to_path_buf()));
        let worker = Arc::new(HlsMapWorkerPool::new(SegmentFetchPolicy {
            retry_jitter_max_ms: 0,
            origin_segment_timeout_ms: 1_000,
            ..SegmentFetchPolicy::default()
        }));
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        grant_usable_map_access_lease(&worker, &proxy_session_id).await;
        worker
            .wake_scheduler(
                MapFetchContext {
                    session: Arc::clone(&session),
                    segment_cache: Arc::clone(&cache),
                    headers: HeaderMap::new(),
                    origin_provider_session_headers: HeaderMap::new(),
                    client: reqwest::Client::new(),
                    no_redirect_client: reqwest::Client::new(),
                    use_manual_redirects: false,
                    origin_io: None,
                },
                2,
            )
            .await;

        for _ in 0..50 {
            if matches!(
                session.read().await.maps.values().next().map(|map| &map.status),
                Some(MapCacheStatus::Ready { .. })
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let session_read = session.read().await;
        let map = session_read.maps.values().next().expect("map");
        assert!(matches!(map.status, MapCacheStatus::Ready { content_length: 5, .. }));
        assert_eq!(session_read.activity.media_readiness_generation, 1);
        assert!(cache.metadata(&map.cache_key).await.expect("metadata").is_some());
        server.await.expect("server joins");
    }

    #[tokio::test]
    async fn invalidated_generation_discards_late_map_completion_without_sleep() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
        let addr = listener.local_addr().expect("local addr");
        let (request_seen_tx, request_seen_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
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
            let _ = request_seen_tx.send(());
            if release_rx.await.is_err() {
                return;
            }
            let body = b"controlled-map";
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(body).await;
        });
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n1.m4s\n",
            &format!("http://{addr}/live/index.m3u8"),
        );
        let store = HlsSessionStore::new();
        let session = store.get_or_create_session(HlsSessionKey::new(1, "1"), b"secret", 0).await;
        {
            let mut session = session.write().await;
            session.apply_origin_manifest(&manifest).expect("manifest maps");
            session.queue_map_fetch_candidates(1);
        }
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path().to_path_buf()));
        let policy = SegmentFetchPolicy { retry_jitter_max_ms: 0, ..SegmentFetchPolicy::default() };
        let worker = Arc::new(HlsMapWorkerPool::new(policy.clone()));
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        grant_usable_map_access_lease(&worker, &proxy_session_id).await;
        let context = MapFetchContext {
            session: Arc::clone(&session),
            segment_cache: Arc::clone(&cache),
            headers: HeaderMap::new(),
            origin_provider_session_headers: HeaderMap::new(),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::new(),
            use_manual_redirects: false,
            origin_io: None,
        };
        let snapshot = worker.next_fetch_snapshot(&context, 2, &policy).await.expect("map snapshot");
        let permit = Arc::new(Semaphore::new(1)).acquire_owned().await.expect("test permit");
        let fetch = tokio::spawn(Arc::clone(&worker).fetch_one_map(context.clone(), snapshot, policy, permit));

        request_seen_rx.await.expect("origin request starts");
        session.write().await.invalidate_queued_origin_work();
        release_tx.send(()).expect("origin response released");
        fetch.await.expect("map fetch joins");

        let cache_key = {
            let session = session.read().await;
            assert_eq!(session.active_map_fetches, 0);
            let map = session.maps.values().next().expect("map remains mapped");
            assert!(matches!(map.status, MapCacheStatus::Discovered));
            assert_eq!(session.origin_control.path_condition, HlsOriginPathCondition::ProgressExpected);
            assert_eq!(session.activity.media_readiness_generation, 0);
            map.cache_key.clone()
        };
        assert!(cache.metadata(&cache_key).await.expect("metadata reads").is_none());
        server.await.expect("server joins");
    }

    #[tokio::test]
    async fn current_required_map_failure_degrades_availability_without_terminalizing_the_lease() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
        let addr = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
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
            let _ = socket.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
        });
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n1.m4s\n",
            &format!("http://{addr}/live/index.m3u8"),
        );
        let store = HlsSessionStore::new();
        let session = store.get_or_create_session(HlsSessionKey::new(1, "1"), b"secret", 0).await;
        {
            let mut session = session.write().await;
            session.apply_origin_manifest(&manifest).expect("manifest maps");
            session.queue_map_fetch_candidates(1);
        }
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path().to_path_buf()));
        let policy = SegmentFetchPolicy { retry_delays_ms: [0; 5], retry_jitter_max_ms: 0, ..Default::default() };
        let availability = Arc::new(HlsAvailabilityReevaluationCoordinator::with_capacity(1));
        let worker = Arc::new(HlsMapWorkerPool::with_global_semaphore_access_leases_and_availability(
            policy.clone(),
            Arc::new(Semaphore::new(1)),
            Arc::new(RwLock::new(HlsAccessLeaseStore::default())),
            Some(Arc::clone(&availability)),
        ));
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        grant_usable_map_access_lease(&worker, &proxy_session_id).await;
        let release_owner = Arc::new(Notify::new());
        let owner_finished = Arc::new(Notify::new());
        let task_release_owner = Arc::clone(&release_owner);
        let task_owner_finished = Arc::clone(&owner_finished);
        assert_eq!(
            availability.register(
                HlsAvailabilityReevaluationOwnerKey {
                    session_incarnation: HlsSessionIncarnation::for_test(1),
                    proxy_session_id: proxy_session_id.clone(),
                    origin_progress_generation: 0,
                    media_readiness_generation: 0,
                    availability_evidence_generation: HlsAvailabilityEvidenceGeneration::for_test(1),
                },
                HlsAvailabilityReevaluationMode::RecoveryPressure,
                move |_| async move {
                    task_release_owner.notified().await;
                    task_owner_finished.notify_one();
                },
            ),
            HlsAvailabilityReevaluationRegistration::Scheduled
        );
        let mut observer = availability.observe_owner(&proxy_session_id).expect("availability owner observation");
        let mut evidence_change = Box::pin(observer.changed());
        assert!(matches!(futures::poll!(evidence_change.as_mut()), std::task::Poll::Pending));
        let context = MapFetchContext {
            session: Arc::clone(&session),
            segment_cache: cache,
            headers: HeaderMap::new(),
            origin_provider_session_headers: HeaderMap::new(),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::new(),
            use_manual_redirects: false,
            origin_io: None,
        };
        let snapshot = worker.next_fetch_snapshot(&context, 2, &policy).await.expect("map snapshot");
        let permit = Arc::new(Semaphore::new(1)).acquire_owned().await.expect("test permit");

        Arc::clone(&worker).fetch_one_map(context, snapshot, policy, permit).await;

        assert_eq!(
            futures::poll!(evidence_change.as_mut()),
            std::task::Poll::Ready(HlsAvailabilityReevaluationObservation::EvidenceChanged)
        );
        release_owner.notify_one();
        owner_finished.notified().await;
        let session = session.read().await;
        assert!(matches!(
            session.maps.values().next().expect("required map").status,
            MapCacheStatus::FailedPermanent { .. }
        ));
        assert_eq!(session.origin_control.path_condition, HlsOriginPathCondition::SegmentReadinessFailure);
        assert_eq!(session.activity.media_readiness_generation, 0);
        assert!(!matches!(
            session.origin_control.progress_phase,
            HlsOriginProgressPhase::Terminal | HlsOriginProgressPhase::TerminalPartial
        ));
        server.await.expect("server joins");
    }

    #[tokio::test]
    async fn gzip_fmp4_map_fetch_commits_identity_representation() {
        const FMP4_MAP: &[u8] = b"\0\0\0\x18ftypisom\0\0\x02\0isomiso2";

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(FMP4_MAP).expect("gzip fixture should encode");
        let encoded_map = encoder.finish().expect("gzip fixture should finish");
        let encoded_len = encoded_map.len();

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
        let addr = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0_u8; 2048];
            let read = socket.read(&mut buf).await.expect("request reads");
            let request = String::from_utf8_lossy(&buf[..read]);
            assert!(
                request.to_ascii_lowercase().contains("accept-encoding: identity"),
                "map origin request must enforce identity"
            );
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Encoding: gzip\r\nContent-Length: {encoded_len}\r\nConnection: close\r\n\r\n"
            );
            socket.write_all(headers.as_bytes()).await.expect("response headers write");
            socket.write_all(&encoded_map).await.expect("response body writes");
        });
        let base_url = format!("http://{addr}/live/index.m3u8");
        let manifest = normal_manifest("#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n1.m4s\n", &base_url);
        let store = HlsSessionStore::new();
        let session = store.get_or_create_session(HlsSessionKey::new(1, "1"), b"secret", 0).await;
        {
            let mut session = session.write().await;
            session.apply_origin_manifest(&manifest).expect("manifest maps");
            session.queue_map_fetch_candidates(1);
        }

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path().to_path_buf()));
        let worker = Arc::new(HlsMapWorkerPool::new(SegmentFetchPolicy {
            retry_jitter_max_ms: 0,
            origin_segment_timeout_ms: 1_000,
            ..SegmentFetchPolicy::default()
        }));
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        grant_usable_map_access_lease(&worker, &proxy_session_id).await;
        worker
            .wake_scheduler(
                MapFetchContext {
                    session: Arc::clone(&session),
                    segment_cache: Arc::clone(&cache),
                    headers: HeaderMap::new(),
                    origin_provider_session_headers: HeaderMap::new(),
                    client: reqwest::Client::new(),
                    no_redirect_client: reqwest::Client::new(),
                    use_manual_redirects: false,
                    origin_io: None,
                },
                2,
            )
            .await;

        for _ in 0..50 {
            if matches!(
                session.read().await.maps.values().next().map(|map| &map.status),
                Some(MapCacheStatus::Ready { .. })
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let cache_key = {
            let session = session.read().await;
            let map = session.maps.values().next().expect("map");
            assert!(matches!(
                &map.status,
                MapCacheStatus::Ready { content_length, .. }
                    if *content_length == u64::try_from(FMP4_MAP.len()).expect("fixture length fits u64")
            ));
            map.cache_key.clone()
        };
        let metadata = cache.metadata(&cache_key).await.expect("metadata reads").expect("map is committed");
        let cached_map = tokio::fs::read(metadata.path).await.expect("cached map reads");
        assert_eq!(cached_map, FMP4_MAP);
        assert!(!cached_map.starts_with(&[0x1f, 0x8b]));
        assert!(!cache.has_active_temp_files());
        server.await.expect("server joins");
    }

    #[tokio::test]
    async fn map_fetch_snapshot_uses_concrete_final_map_fetch_url() {
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"5@10\"\n#EXTINF:4.0,\n1.m4s\n",
            "https://cdn.example.net/live/redirected/playlist.m3u8",
        );
        let store = HlsSessionStore::new();
        let session = store.get_or_create_session(HlsSessionKey::new(1, "1"), b"secret", 0).await;
        {
            let mut session = session.write().await;
            session.apply_origin_manifest(&manifest).expect("manifest maps");
            session.queue_map_fetch_candidates(1);
        }

        let worker = Arc::new(HlsMapWorkerPool::new(SegmentFetchPolicy::default()));
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        grant_usable_map_access_lease(&worker, &proxy_session_id).await;
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let context = MapFetchContext {
            session: Arc::clone(&session),
            segment_cache: Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path().to_path_buf())),
            headers: HeaderMap::new(),
            origin_provider_session_headers: HeaderMap::new(),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::new(),
            use_manual_redirects: false,
            origin_io: None,
        };

        let snapshot =
            worker.next_fetch_snapshot(&context, 2, &SegmentFetchPolicy::default()).await.expect("map snapshot");

        assert_eq!(snapshot.fetch_ref.resolved_origin_url, "https://cdn.example.net/live/redirected/init.mp4");
        assert_eq!(snapshot.fetch_ref.byte_range, Some(ParsedByteRange { length: 5, offset: 10 }));
    }

    #[tokio::test]
    async fn map_fetch_without_usable_access_lease_resets_queue_without_origin_request() {
        let base_url = "http://origin.example.com/live/index.m3u8";
        let manifest = normal_manifest("#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n1.m4s\n", base_url);
        let store = HlsSessionStore::new();
        let session = store.get_or_create_session(HlsSessionKey::new(1, "1"), b"secret", 0).await;
        {
            let mut session = session.write().await;
            session.apply_origin_manifest(&manifest).expect("manifest maps");
            session.queue_map_fetch_candidates(1);
        }

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let worker = Arc::new(HlsMapWorkerPool::new(SegmentFetchPolicy::default()));
        worker
            .wake_scheduler(
                MapFetchContext {
                    session: Arc::clone(&session),
                    segment_cache: Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path().to_path_buf())),
                    headers: HeaderMap::new(),
                    origin_provider_session_headers: HeaderMap::new(),
                    client: reqwest::Client::new(),
                    no_redirect_client: reqwest::Client::new(),
                    use_manual_redirects: false,
                    origin_io: None,
                },
                2,
            )
            .await;

        let session = session.read().await;
        assert_eq!(session.active_map_fetches, 0);
        assert!(matches!(session.maps.values().next().expect("map").status, MapCacheStatus::Discovered));
    }

    #[tokio::test]
    async fn map_fetch_is_blocked_for_gc_marked_session() {
        let base_url = "http://origin.example.com/live/index.m3u8";
        let manifest = normal_manifest("#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n1.m4s\n", base_url);
        let store = HlsSessionStore::new();
        let session = store.get_or_create_session(HlsSessionKey::new(1, "1"), b"secret", 0).await;
        {
            let mut session = session.write().await;
            session.apply_origin_manifest(&manifest).expect("manifest maps");
            session.queue_map_fetch_candidates(1);
            session.mark_for_gc_removal();
        }

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let worker = Arc::new(HlsMapWorkerPool::new(SegmentFetchPolicy::default()));
        worker
            .wake_scheduler(
                MapFetchContext {
                    session: Arc::clone(&session),
                    segment_cache: Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path().to_path_buf())),
                    headers: HeaderMap::new(),
                    origin_provider_session_headers: HeaderMap::new(),
                    client: reqwest::Client::new(),
                    no_redirect_client: reqwest::Client::new(),
                    use_manual_redirects: false,
                    origin_io: None,
                },
                2,
            )
            .await;

        let session = session.read().await;
        assert_eq!(session.active_map_fetches, 0);
        assert!(matches!(session.maps.values().next().expect("map").status, MapCacheStatus::Queued { .. }));
    }
}
