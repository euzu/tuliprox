use super::{
    begin_hls_origin_account_io, finish_hls_origin_account_io, force_identity_without_range,
    hls_session_object_body_deadline, safe_origin_log_value, scrub_hls_origin_headers, CachedSegmentMetadata,
    HlsAccessLeaseStore, HlsOriginAccountIoLeaseGuard, HlsOriginIoContext, HlsSegmentCache, HlsSessionHandle,
    MapCacheKey, MapCacheStatus, OriginMapFetchRef, ProxyMapId, SegmentFetchPolicy,
};
use crate::processing::parser::hls::origin_manifest::ParsedByteRange;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use arc_swap::ArcSwap;
use futures::TryStreamExt;
use log::{debug, warn};
use reqwest::Client;
use shared::utils::sanitize_sensitive_info;
use std::{fmt, io, sync::Arc, time::Duration};
use tokio::{
    sync::{OwnedSemaphorePermit, RwLock, Semaphore},
};
use tokio_util::io::StreamReader;
use url::Url;

const MAX_MANUAL_REDIRECTS: usize = 10;

/// Shared context required to schedule EXT-X-MAP origin fetches without holding session locks.
#[derive(Clone)]
pub struct MapFetchContext {
    pub session: HlsSessionHandle,
    pub segment_cache: Arc<HlsSegmentCache>,
    pub headers: HeaderMap,
    pub client: Client,
    pub no_redirect_client: Client,
    pub use_manual_redirects: bool,
    pub origin_io: Option<HlsOriginIoContext>,
}

#[derive(Clone)]
struct MapFetchSnapshot {
    proxy_map_id: ProxyMapId,
    cache_key: MapCacheKey,
    fetch_ref: OriginMapFetchRef,
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
            .field("cache_key", &self.cache_key)
            .field("fetch_ref", &self.fetch_ref)
            .finish()
    }
}

#[derive(Debug)]
enum MapFetchError {
    PermanentStatus(StatusCode),
    RetryableStatus(StatusCode),
    RetryExhausted,
    NonRetryableStatus(StatusCode),
    Request(String),
    Redirect,
    Timeout,
    InvalidOriginUrl,
    InvalidByteRange,
    UnexpectedByteRangeStatus,
    Cache,
    ProviderUnavailable,
}

#[derive(Clone)]
struct MapWorkerRuntime {
    global_semaphore: Arc<Semaphore>,
    policy: SegmentFetchPolicy,
}

impl MapWorkerRuntime {
    fn new(policy: SegmentFetchPolicy, global_semaphore: Arc<Semaphore>) -> Self {
        Self { global_semaphore, policy }
    }
}

/// Bounded scheduler for live HLS EXT-X-MAP origin fetches.
pub struct HlsMapWorkerPool {
    runtime: ArcSwap<MapWorkerRuntime>,
    access_leases: Arc<RwLock<HlsAccessLeaseStore>>,
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
        Self {
            runtime: ArcSwap::from_pointee(MapWorkerRuntime::new(policy, global_semaphore)),
            access_leases,
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
        if !self
            .access_leases
            .write()
            .await
            .has_usable_access_lease_for_session(&proxy_session_id, now_ms)
        {
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

        let entry = session.maps.get_mut(&proxy_map_id)?;
        let fetch_ref = entry.origin_fetch_ref.clone()?;
        if !fetch_ref.is_valid_at(now_ms) {
            return None;
        }
        let cache_key = entry.cache_key.clone();
        entry.status = MapCacheStatus::Fetching { started_at_ms: now_ms };
        session.active_map_fetches = session.active_map_fetches.saturating_add(1);
        debug!("HLS map fetch started: proxy_map_id={}", proxy_map_id.0);

        Some(MapFetchSnapshot { proxy_map_id, cache_key, fetch_ref })
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
        let generation_valid = result.as_ref().map_or(true, |commit| commit.generation_valid);
        let fetch_succeeded = result.is_ok();
        {
            let mut session = context.session.write().await;
            session.active_map_fetches = session.active_map_fetches.saturating_sub(1);
            if let Some(entry) = session.maps.get_mut(&snapshot.proxy_map_id) {
                match result {
                    Ok(commit) => {
                        let content_length = commit.content_length;
                        entry.status = MapCacheStatus::Ready { content_length, ready_at_ms: finished_at_ms };
                        debug!(
                            "HLS map cached: proxy_map_id={} content_length={content_length}",
                            snapshot.proxy_map_id.0
                        );
                    }
                    Err(_) => {
                        entry.status = MapCacheStatus::Failed { failed_at_ms: finished_at_ms };
                    }
                }
            }
            if fetch_succeeded && generation_valid {
                let _ = session.render_and_store_manifest(finished_at_ms);
            }
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
    let started_generation = start_map_origin_work(context).await;
    let binding =
        if context.origin_io.is_some() { context.session.read().await.origin_account_binding.clone() } else { None };
    let provider_lease = if let (Some(origin_io), Some(binding)) = (context.origin_io.as_ref(), binding.as_ref()) {
        if binding.is_detached() {
            let _ = finish_map_origin_work(context, started_generation).await;
            touch_map_origin_account_binding(context, false).await;
            return Err(MapFetchError::ProviderUnavailable);
        }
        let Ok(guard) = begin_hls_origin_account_io(origin_io, &context.session, binding).await else {
            let _ = finish_map_origin_work(context, started_generation).await;
            touch_map_origin_account_binding(context, false).await;
            return Err(MapFetchError::ProviderUnavailable);
        };
        Some((origin_io.clone(), guard))
    } else {
        None
    };

    let metadata = match fetch_map_with_retries_into_cache(context, snapshot, policy).await {
        Ok(metadata) => metadata,
        Err(err) => {
            finish_map_origin_io(context, started_generation, provider_lease).await;
            return Err(err);
        }
    };
    let origin_work = finish_map_origin_io(context, started_generation, provider_lease).await;
    Ok(MapFetchCommit {
        content_length: metadata.size,
        generation_valid: origin_work.generation_valid,
    })
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
        touch_map_origin_account_binding(context, origin_work.generation_valid && origin_work.refresh_reservation).await;
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
        return MapOriginWorkFinish {
            generation_valid: true,
            refresh_reservation: false,
        };
    };
    let mut session = context.session.write().await;
    let generation_valid = session.finish_origin_work(started_generation);
    let refresh_reservation = session.should_refresh_origin_reservation(current_time_millis());
    MapOriginWorkFinish {
        generation_valid,
        refresh_reservation,
    }
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

async fn fetch_map_with_retries_into_cache(
    context: &MapFetchContext,
    snapshot: &MapFetchSnapshot,
    policy: &SegmentFetchPolicy,
) -> Result<CachedSegmentMetadata, MapFetchError> {
    for attempt_index in 0..policy.retry_delays_ms.len() {
        let jitter = if policy.retry_jitter_max_ms == 0 { 0 } else { fastrand::u64(0..=policy.retry_jitter_max_ms) };
        let delay_ms = policy.retry_delays_ms[attempt_index].saturating_add(jitter);
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }

        let fetch_result = fetch_map_attempt_into_cache(context, snapshot, policy).await;

        match fetch_result {
            Ok(metadata) => return Ok(metadata),
            Err(MapFetchError::PermanentStatus(status)) => return Err(MapFetchError::PermanentStatus(status)),
            Err(MapFetchError::NonRetryableStatus(status)) => {
                return Err(MapFetchError::NonRetryableStatus(status));
            }
            Err(MapFetchError::RetryableStatus(status)) if attempt_index + 1 == policy.retry_delays_ms.len() => {
                return Err(MapFetchError::RetryableStatus(status));
            }
            Err(MapFetchError::RetryableStatus(status)) => {
                warn!(
                    "HLS map fetch retry scheduled: origin_url={} attempt={} status={} delay_ms={}",
                    safe_origin_log_value(&snapshot.fetch_ref.resolved_origin_url),
                    attempt_index + 1,
                    status.as_u16(),
                    policy.retry_delays_ms.get(attempt_index + 1).copied().unwrap_or_default()
                );
            }
            Err(MapFetchError::Request(err)) if attempt_index + 1 == policy.retry_delays_ms.len() => {
                return Err(MapFetchError::Request(err));
            }
            Err(err @ (MapFetchError::Redirect | MapFetchError::Timeout))
                if attempt_index + 1 == policy.retry_delays_ms.len() =>
            {
                return Err(err);
            }
            Err(err @ (MapFetchError::InvalidOriginUrl | MapFetchError::InvalidByteRange)) => return Err(err),
            Err(MapFetchError::ProviderUnavailable) => return Err(MapFetchError::ProviderUnavailable),
            Err(MapFetchError::Timeout) => {
                warn!(
                    "HLS origin object fetch timed out: session={} kind=map map_id={} deadline_ms={}",
                    safe_origin_log_value(&context.session.read().await.proxy_session_id.0),
                    snapshot.proxy_map_id.0,
                    hls_session_object_body_deadline(&context.session, policy.origin_segment_timeout_ms)
                        .await
                        .as_millis()
                );
            }
            Err(_) => {}
        }
    }

    Err(MapFetchError::RetryExhausted)
}

async fn fetch_map_attempt_into_cache(
    context: &MapFetchContext,
    snapshot: &MapFetchSnapshot,
    policy: &SegmentFetchPolicy,
) -> Result<CachedSegmentMetadata, MapFetchError> {
    let response = fetch_map_once(context, &snapshot.fetch_ref).await?;
    if snapshot.fetch_ref.byte_range.is_some() && response.status() != StatusCode::PARTIAL_CONTENT {
        return Err(MapFetchError::UnexpectedByteRangeStatus);
    }
    if snapshot.fetch_ref.byte_range.is_none() && response.status() == StatusCode::PARTIAL_CONTENT {
        return Err(MapFetchError::UnexpectedByteRangeStatus);
    }
    let deadline = hls_session_object_body_deadline(&context.session, policy.origin_segment_timeout_ms).await;
    let stream_reader = StreamReader::new(response.bytes_stream().map_err(io::Error::other));
    context
        .segment_cache
        .write_temp_and_commit_with_timeout(&snapshot.cache_key, stream_reader, deadline)
        .await
        .map_err(|err| {
            if err.kind() == io::ErrorKind::TimedOut {
                MapFetchError::Timeout
            } else {
                MapFetchError::Cache
            }
        })
}

async fn fetch_map_once(
    context: &MapFetchContext,
    fetch_ref: &OriginMapFetchRef,
) -> Result<reqwest::Response, MapFetchError> {
    let url = Url::parse(&fetch_ref.resolved_origin_url).map_err(|_| MapFetchError::InvalidOriginUrl)?;
    let headers = build_map_origin_headers(&context.headers, fetch_ref.byte_range)?;
    if context.use_manual_redirects {
        fetch_map_with_manual_redirects(&url, headers, &context.no_redirect_client).await
    } else {
        let response = context
            .client
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(|err| MapFetchError::Request(sanitize_sensitive_info(err.to_string().as_str()).to_string()))?;
        classify_map_response(response)
    }
}

async fn fetch_map_with_manual_redirects(
    entry_url: &Url,
    headers: HeaderMap,
    client: &Client,
) -> Result<reqwest::Response, MapFetchError> {
    let mut current_url = entry_url.clone();
    let mut current_headers = headers;
    let mut remaining_redirects = MAX_MANUAL_REDIRECTS;

    loop {
        let response = client
            .get(current_url.clone())
            .headers(current_headers.clone())
            .send()
            .await
            .map_err(|err| MapFetchError::Request(sanitize_sensitive_info(err.to_string().as_str()).to_string()))?;
        if !response.status().is_redirection() {
            return classify_map_response(response);
        }
        if remaining_redirects == 0 {
            return Err(MapFetchError::Redirect);
        }
        let response_url = response.url().clone();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or(MapFetchError::Redirect)?;
        let next_url =
            response_url.join(location).or_else(|_| Url::parse(location)).map_err(|_| MapFetchError::Redirect)?;
        if !same_origin(&response_url, &next_url) {
            strip_sensitive_headers_for_cross_origin_redirect(&mut current_headers);
        }
        current_url = next_url;
        remaining_redirects = remaining_redirects.saturating_sub(1);
    }
}

fn classify_map_response(response: reqwest::Response) -> Result<reqwest::Response, MapFetchError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    if status.is_server_error()
        || matches!(
            status,
            StatusCode::PROXY_AUTHENTICATION_REQUIRED
                | StatusCode::REQUEST_TIMEOUT
                | StatusCode::TOO_EARLY
                | StatusCode::TOO_MANY_REQUESTS
        )
    {
        return Err(MapFetchError::RetryableStatus(status));
    }
    if matches!(
        status,
        StatusCode::BAD_REQUEST
            | StatusCode::UNAUTHORIZED
            | StatusCode::FORBIDDEN
            | StatusCode::NOT_FOUND
            | StatusCode::GONE
    ) {
        return Err(MapFetchError::PermanentStatus(status));
    }
    Err(MapFetchError::NonRetryableStatus(status))
}

fn build_map_origin_headers(
    source_headers: &HeaderMap,
    byte_range: Option<ParsedByteRange>,
) -> Result<HeaderMap, MapFetchError> {
    let mut headers = source_headers.clone();
    scrub_hls_origin_headers(&mut headers, None);
    force_identity_without_range(&mut headers);
    if let Some(byte_range) = byte_range {
        let end = byte_range
            .offset
            .checked_add(byte_range.length)
            .and_then(|end_exclusive| end_exclusive.checked_sub(1))
            .ok_or(MapFetchError::InvalidByteRange)?;
        let range_value = format!("bytes={}-{}", byte_range.offset, end);
        let range_value = HeaderValue::from_str(&range_value).map_err(|_| MapFetchError::InvalidByteRange)?;
        headers.insert(header::RANGE, range_value);
    }
    Ok(headers)
}

fn same_origin(lhs: &Url, rhs: &Url) -> bool {
    lhs.scheme().eq_ignore_ascii_case(rhs.scheme())
        && lhs.host_str() == rhs.host_str()
        && lhs.port_or_known_default() == rhs.port_or_known_default()
}

fn strip_sensitive_headers_for_cross_origin_redirect(headers: &mut HeaderMap) {
    scrub_hls_origin_headers(headers, None);
}

fn current_time_millis() -> u64 { chrono::Utc::now().timestamp_millis().try_into().unwrap_or_default() }

#[cfg(test)]
mod tests {
    use super::{build_map_origin_headers, HlsMapWorkerPool, MapFetchContext};
    use crate::{
        api::model::{
            HlsAccessLease, HlsAccessLeaseId, HlsPlaybackFamilyKey, HlsSegmentCache, HlsSessionKey, HlsSessionStore,
            MapCacheStatus, ProxySessionId, SegmentFetchPolicy,
        },
        processing::parser::hls::origin_manifest::{
            parse_origin_media_manifest, OriginManifestParseOutcome, ParsedByteRange,
        },
    };
    use axum::http::{header, HeaderMap};
    use std::{sync::Arc, time::Duration};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
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
        let headers = build_map_origin_headers(&source_headers, Some(ParsedByteRange { offset: 10, length: 5 }))
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
        assert!(cache.metadata(&map.cache_key).await.expect("metadata").is_some());
        server.await.expect("server joins");
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
        assert!(matches!(
            session.maps.values().next().expect("map").status,
            MapCacheStatus::Discovered
        ));
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
