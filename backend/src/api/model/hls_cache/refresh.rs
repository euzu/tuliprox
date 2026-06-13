use super::{
    begin_hls_origin_account_io, finish_hls_origin_account_io, safe_origin_log_value, safe_proxy_session_id,
    safe_session_key, sanitized_hls_origin_headers, HlsManifestRenderer, HlsMapWorkerPool, HlsOriginIoContext,
    HlsOriginWorkClass, HlsSegmentCache, HlsSegmentRepairManager, HlsSegmentWorkerPool, HlsSessionHandle,
    HlsSessionMode, MapFetchContext, RenderPolicy, SegmentFetchContext, TransientPassthroughReason,
};
use crate::{
    model::{
        resolve_provider_scheme_url_with_provider_index, AppConfig, ConfigProvider, InputSource,
        ReverseProxyDisabledHeaderConfig, StripConfig,
    },
    processing::parser::hls::{
        origin_manifest::{
            parse_manifest_timing, parse_origin_manifest_timeline, parse_origin_media_manifest,
            OriginManifestParseOutcome, OriginManifestTransientReason, ParsedOriginManifest,
            ParsedOriginManifestTimeline,
        },
        transient_manifest::{TransientManifestRewriter, TransientRewriteOptions},
    },
    utils::request::{download_text_content, download_text_content_with_manual_redirects},
};
use axum::http::{header, HeaderMap, StatusCode};
use log::{debug, info, warn};
use reqwest::Client;
use shared::{model::InputFetchMethod, utils::sanitize_sensitive_info};
use std::{collections::HashMap, fmt, sync::Arc, time::Duration};
use tokio::time::timeout;
use url::Url;

const MAX_MANUAL_REDIRECTS: usize = 10;
const COLD_START_RETRY_AFTER_SECONDS: u64 = 2;
const FIRST_FAILURE_BACKOFF_MS: u64 = 0;
const SECOND_FAILURE_BACKOFF_MS: u64 = 500;
const LATER_FAILURE_BACKOFF_MS: u64 = 1_000;

/// Debounce and singleflight state for one live HLS origin manifest.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct OriginRefreshState {
    pub last_fetch_started_at_ms: Option<u64>,
    pub last_fetch_finished_at_ms: Option<u64>,
    pub next_fetch_allowed_at_ms: u64,
    pub consecutive_failures: u32,
    pub last_success_at_ms: Option<u64>,
    pub last_error_at_ms: Option<u64>,
    pub in_flight: bool,
}

impl OriginRefreshState {
    pub fn is_due(&self, now_ms: u64) -> bool { now_ms >= self.next_fetch_allowed_at_ms && !self.in_flight }

    pub fn mark_started(&mut self, now_ms: u64) {
        self.last_fetch_started_at_ms = Some(now_ms);
        self.in_flight = true;
    }

    pub fn mark_success(&mut self, fetch_started_at_ms: u64, fetch_finished_at_ms: u64, refresh_interval_ms: u64) {
        self.last_fetch_finished_at_ms = Some(fetch_finished_at_ms);
        self.last_success_at_ms = Some(fetch_finished_at_ms);
        self.last_error_at_ms = None;
        self.consecutive_failures = 0;
        self.in_flight = false;
        self.next_fetch_allowed_at_ms = fetch_started_at_ms.saturating_add(refresh_interval_ms);
    }

    pub fn mark_failure(&mut self, failed_at_ms: u64) {
        self.last_fetch_finished_at_ms = Some(failed_at_ms);
        self.last_error_at_ms = Some(failed_at_ms);
        let next_retry_delay_ms = self.next_failure_backoff_ms();
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.in_flight = false;
        self.next_fetch_allowed_at_ms = failed_at_ms.saturating_add(next_retry_delay_ms);
    }

    fn next_failure_backoff_ms(&self) -> u64 {
        match self.consecutive_failures {
            0 => FIRST_FAILURE_BACKOFF_MS,
            1 => SECOND_FAILURE_BACKOFF_MS,
            _ => LATER_FAILURE_BACKOFF_MS,
        }
    }
}

/// Origin manifest entrypoint snapshot for live HLS refreshes.
#[derive(Clone)]
pub struct LiveHlsOriginEntry {
    url: Url,
    provider: Option<Arc<ConfigProvider>>,
}

impl LiveHlsOriginEntry {
    pub fn parse(url: &str) -> Option<Self> { Self::parse_with_provider(url, None) }

    pub fn parse_with_provider(url: &str, provider: Option<Arc<ConfigProvider>>) -> Option<Self> {
        Url::parse(url).ok().map(|url| Self { url, provider })
    }

    pub fn url(&self) -> &Url { &self.url }

    pub fn provider(&self) -> Option<&Arc<ConfigProvider>> { self.provider.as_ref() }

    pub fn to_input_source(&self) -> InputSource {
        InputSource {
            name: Arc::<str>::from("hls-origin"),
            url: self.url.to_string(),
            provider: self.provider.clone(),
            username: None,
            password: None,
            method: InputFetchMethod::GET,
            headers: HashMap::new(),
        }
    }
}

impl fmt::Debug for LiveHlsOriginEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveHlsOriginEntry")
            .field("scheme", &self.url.scheme())
            .field("host", &self.url.host_str().unwrap_or("<missing>"))
            .field("path", &"<redacted>")
            .field("provider", &self.provider.as_ref().map(|provider| provider.name.as_ref()))
            .finish()
    }
}

/// Fixed retry policy for HLS origin manifest refreshes.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RetryPolicy {
    pub delays_ms: [u64; 5],
    pub jitter_max_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self { Self { delays_ms: [0, 100, 250, 500, 750], jitter_max_ms: 100 } }
}

impl RetryPolicy {
    pub fn delay_for_attempt_ms(&self, attempt_index: usize, jitter_ms: u64) -> Option<u64> {
        self.delays_ms.get(attempt_index).map(|base| base.saturating_add(jitter_ms.min(self.jitter_max_ms)))
    }

    fn attempt_count(&self) -> usize { self.delays_ms.len() }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum OriginManifestStatusClass {
    Success,
    Retryable,
    PermanentFailure,
    NonRetryableFailure,
}

pub fn classify_origin_manifest_status(status: StatusCode) -> OriginManifestStatusClass {
    if status.is_success() {
        return OriginManifestStatusClass::Success;
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
        return OriginManifestStatusClass::Retryable;
    }
    if matches!(
        status,
        StatusCode::BAD_REQUEST
            | StatusCode::UNAUTHORIZED
            | StatusCode::FORBIDDEN
            | StatusCode::NOT_FOUND
            | StatusCode::GONE
    ) {
        return OriginManifestStatusClass::PermanentFailure;
    }
    OriginManifestStatusClass::NonRetryableFailure
}

#[derive(Debug)]
pub enum OriginManifestFetchError {
    PermanentStatus(StatusCode),
    RetryableStatus(StatusCode, Option<u64>),
    RetryExhausted,
    NonRetryableStatus(StatusCode),
    Request(String),
    Redirect(String),
    Timeout,
    ProviderUnavailable,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum OriginManifestCommitError {
    TimelineRejected,
}

#[derive(Clone, Eq, PartialEq)]
pub struct FetchedOriginManifest {
    pub body: String,
    pub final_manifest_url: String,
    pub resolved_request_url: String,
    pub redirect_host: Option<String>,
    pub provider_url_index: Option<usize>,
    pub status: StatusCode,
    pub attempts: usize,
}

impl fmt::Debug for FetchedOriginManifest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FetchedOriginManifest")
            .field("body_len", &self.body.len())
            .field("final_manifest_url", &"<redacted>")
            .field("resolved_request_url", &"<redacted>")
            .field("redirect_host", &self.redirect_host)
            .field("provider_url_index", &self.provider_url_index)
            .field("status", &self.status)
            .field("attempts", &self.attempts)
            .finish()
    }
}

#[derive(Clone)]
pub struct OriginRefreshRequest {
    pub app_config: Arc<AppConfig>,
    pub session: HlsSessionHandle,
    pub origin_entry: LiveHlsOriginEntry,
    pub origin_input_source: InputSource,
    pub headers: HeaderMap,
    pub disabled_headers: Option<ReverseProxyDisabledHeaderConfig>,
    pub client: Client,
    pub no_redirect_client: Client,
    pub use_manual_redirects: bool,
    pub segment_cache: Arc<HlsSegmentCache>,
    pub segment_repair: Arc<HlsSegmentRepairManager>,
    pub segment_worker_pool: Arc<HlsSegmentWorkerPool>,
    pub map_worker_pool: Arc<HlsMapWorkerPool>,
    pub origin_manifest_timeout_ms: u64,
    pub strip: StripConfig,
    pub retry_policy: RetryPolicy,
    pub reverse_proxy_rewrite_secret: Vec<u8>,
    pub transient_resource_ttl_ms: u64,
    pub now_ms: u64,
    pub origin_io: Option<HlsOriginIoContext>,
}

pub async fn maybe_trigger_origin_refresh(mut request: OriginRefreshRequest) -> bool {
    let fetch_started_at_ms = request.now_ms;
    if !mark_origin_refresh_started(&mut request, fetch_started_at_ms).await {
        release_preacquired_origin_provider_handle(&request).await;
        return false;
    }

    tokio::spawn(async move {
        refresh_and_commit(request, fetch_started_at_ms).await;
    });
    true
}

pub async fn trigger_origin_refresh_sync(mut request: OriginRefreshRequest) -> bool {
    let fetch_started_at_ms = request.now_ms;
    if !mark_origin_refresh_started(&mut request, fetch_started_at_ms).await {
        release_preacquired_origin_provider_handle(&request).await;
        return false;
    }

    refresh_and_commit(request, fetch_started_at_ms).await;
    true
}

async fn mark_origin_refresh_started(request: &mut OriginRefreshRequest, fetch_started_at_ms: u64) -> bool {
    let metrics = Arc::clone(request.segment_worker_pool.metrics());
    {
        let mut session = request.session.write().await;
        if session.is_gc_marked_for_removal() {
            return false;
        }
        if session.origin_refresh.in_flight {
            metrics.record_refresh_skipped();
            let last_fetch_started_at_ms = session
                .origin_refresh
                .last_fetch_started_at_ms
                .map_or_else(|| "<none>".to_string(), |started_at_ms| started_at_ms.to_string());
            let in_flight_for_ms = session.origin_refresh.last_fetch_started_at_ms.map_or_else(
                || "<unknown>".to_string(),
                |started_at_ms| fetch_started_at_ms.saturating_sub(started_at_ms).to_string(),
            );
            debug!(
                "HLS origin manifest refresh skipped: session={} proxy_session_id={} reason=in_flight last_fetch_started_at_ms={} in_flight_for_ms={} now_ms={fetch_started_at_ms}",
                safe_session_key(&session.key),
                safe_proxy_session_id(&session.proxy_session_id),
                last_fetch_started_at_ms,
                in_flight_for_ms
            );
            return false;
        }
        if fetch_started_at_ms < session.origin_refresh.next_fetch_allowed_at_ms {
            metrics.record_refresh_skipped();
            let wait_ms = session.origin_refresh.next_fetch_allowed_at_ms.saturating_sub(fetch_started_at_ms);
            debug!(
                "HLS origin manifest refresh skipped: session={} proxy_session_id={} reason=debounce next_fetch_allowed_at_ms={} now_ms={fetch_started_at_ms} wait_ms={wait_ms}",
                safe_session_key(&session.key),
                safe_proxy_session_id(&session.proxy_session_id),
                session.origin_refresh.next_fetch_allowed_at_ms
            );
            return false;
        }
        if let Some(origin_io) = request.origin_io.as_mut() {
            origin_io.started_generation = Some(session.start_origin_work());
        }
        session.origin_refresh.mark_started(fetch_started_at_ms);
        metrics.record_refresh_started();
        info!(
            "HLS origin manifest refresh started: session={} proxy_session_id={}",
            safe_session_key(&session.key),
            safe_proxy_session_id(&session.proxy_session_id)
        );
    }
    true
}

#[allow(clippy::too_many_lines)]
async fn refresh_and_commit(mut request: OriginRefreshRequest, fetch_started_at_ms: u64) {
    request.headers = sanitized_hls_origin_headers(&request.headers, request.disabled_headers.as_ref());
    let provider_lease = if let Some(origin_io) = request.origin_io.as_ref() {
        let binding = request.session.read().await.origin_account_binding.clone();
        if let Some(binding) = binding {
            if let Ok(guard) = begin_hls_origin_account_io(origin_io, &request.session, &binding).await {
                debug!(
                    "HLS provider session lease joined for manifest refresh: provider={}",
                    sanitize_sensitive_info(binding.account_name.as_ref())
                );
                Some((origin_io.clone(), guard))
            } else {
                touch_refresh_origin_account_binding(&request, false).await;
                let _ = finish_refresh_origin_work(&request, current_time_millis()).await;
                finish_refresh_failure(&request, OriginManifestFetchError::ProviderUnavailable).await;
                return;
            }
        } else {
            None
        }
    } else {
        None
    };
    let result = fetch_and_commit_manifest_with_policy(&mut request).await;
    let fetch_finished_at_ms = current_time_millis();
    let origin_work_state = finish_refresh_origin_work(&request, fetch_finished_at_ms).await;
    if let Some((origin_io, guard)) = provider_lease {
        let binding = guard.binding().clone();
        finish_hls_origin_account_io(
            &origin_io,
            &request.session,
            guard,
            origin_work_state.generation_valid && origin_work_state.refresh_reservation,
        )
        .await;
        debug!(
            "HLS provider session lease released after manifest refresh: provider={}",
            sanitize_sensitive_info(binding.account_name.as_ref())
        );
        touch_refresh_origin_account_binding(
            &request,
            origin_work_state.generation_valid && origin_work_state.refresh_reservation,
        )
        .await;
    }
    let metrics = Arc::clone(request.segment_worker_pool.metrics());
    let (should_wake_segment_scheduler, should_wake_map_scheduler) = {
        let mut session = request.session.write().await;
        match result {
            Ok(CommittedOriginManifest {
                fetched,
                refresh_interval_ms,
                wake_segment_scheduler,
                wake_map_scheduler,
            }) => {
                session.origin_refresh.mark_success(fetch_started_at_ms, fetch_finished_at_ms, refresh_interval_ms);
                metrics.record_refresh_completed();
                for _ in 1..fetched.attempts {
                    metrics.record_refresh_retried();
                }
                info!(
                    "HLS origin manifest refresh completed: session={} proxy_session_id={} final_url={} redirect_host={} status={} attempts={}",
                    safe_session_key(&session.key),
                    safe_proxy_session_id(&session.proxy_session_id),
                    safe_origin_log_value(&fetched.final_manifest_url),
                    fetched.redirect_host.as_deref().unwrap_or("<none>"),
                    fetched.status.as_u16(),
                    fetched.attempts
                );
                if origin_work_state.generation_valid {
                    (wake_segment_scheduler, wake_map_scheduler)
                } else {
                    session.invalidate_queued_origin_work();
                    (false, false)
                }
            }
            Err(err) => {
                session.origin_refresh.mark_failure(fetch_finished_at_ms);
                metrics.record_refresh_failed();
                warn!(
                    "HLS origin manifest refresh completed: session={} proxy_session_id={} result=failed error={}",
                    safe_session_key(&session.key),
                    safe_proxy_session_id(&session.proxy_session_id),
                    safe_origin_log_value(format!("{err:?}"))
                );
                (false, false)
            }
        }
    };

    if should_wake_map_scheduler {
        request
            .map_worker_pool
            .wake_scheduler(
                MapFetchContext {
                    session: Arc::clone(&request.session),
                    segment_cache: Arc::clone(&request.segment_cache),
                    headers: request.headers.clone(),
                    client: request.client.clone(),
                    no_redirect_client: request.no_redirect_client.clone(),
                    use_manual_redirects: request.use_manual_redirects,
                    origin_io: request
                        .origin_io
                        .clone()
                        .map(|origin_io| origin_io.with_grace(HlsOriginWorkClass::Background.allows_grace())),
                },
                fetch_finished_at_ms,
            )
            .await;
    }

    if should_wake_segment_scheduler {
        request
            .segment_worker_pool
            .wake_scheduler(
                SegmentFetchContext {
                    session: request.session,
                    segment_cache: request.segment_cache,
                    segment_repair: request.segment_repair,
                    repair_access_lease_id: None,
                    headers: request.headers,
                    client: request.client,
                    no_redirect_client: request.no_redirect_client,
                    use_manual_redirects: request.use_manual_redirects,
                    origin_io: request
                        .origin_io
                        .map(|origin_io| origin_io.with_grace(HlsOriginWorkClass::Background.allows_grace())),
                },
                fetch_finished_at_ms,
            )
            .await;
    }
}

#[derive(Clone, Copy)]
struct OriginWorkFinishState {
    generation_valid: bool,
    refresh_reservation: bool,
}

async fn finish_refresh_origin_work(request: &OriginRefreshRequest, now_ms: u64) -> OriginWorkFinishState {
    let Some(started_generation) = request.origin_io.as_ref().and_then(|origin_io| origin_io.started_generation) else {
        return OriginWorkFinishState { generation_valid: true, refresh_reservation: false };
    };
    let mut session = request.session.write().await;
    let generation_valid = session.finish_origin_work(started_generation);
    let refresh_reservation = session.should_refresh_origin_reservation(now_ms);
    OriginWorkFinishState { generation_valid, refresh_reservation }
}

async fn finish_refresh_failure(request: &OriginRefreshRequest, err: OriginManifestFetchError) {
    release_preacquired_origin_provider_handle(request).await;
    let fetch_finished_at_ms = current_time_millis();
    let metrics = Arc::clone(request.segment_worker_pool.metrics());
    {
        let mut session = request.session.write().await;
        session.origin_refresh.mark_failure(fetch_finished_at_ms);
        metrics.record_refresh_failed();
        warn!(
            "HLS origin manifest refresh completed: session={} proxy_session_id={} result=failed error={}",
            safe_session_key(&session.key),
            safe_proxy_session_id(&session.proxy_session_id),
            safe_origin_log_value(format!("{err:?}"))
        );
    }
}

async fn release_preacquired_origin_provider_handle(request: &OriginRefreshRequest) {
    let Some(origin_io) = request.origin_io.as_ref() else {
        return;
    };
    let Some(handle) = origin_io.take_preacquired_provider_handle().await else {
        return;
    };
    let binding = request.session.read().await.origin_account_binding.clone();
    if let Some(binding) = binding {
        origin_io.app_state.connection_manager.release_provider_handle(Some(handle)).await;
        debug!(
            "HLS provider handle released after manifest refresh: provider={} reason=refresh-not-started",
            sanitize_sensitive_info(binding.account_name.as_ref())
        );
    } else {
        origin_io.app_state.connection_manager.release_provider_handle(Some(handle)).await;
    }
}

async fn touch_refresh_origin_account_binding(request: &OriginRefreshRequest, reservation_refreshed: bool) {
    let mut session = request.session.write().await;
    if let Some(binding) = session.origin_account_binding.as_mut() {
        let now_ms = current_time_millis();
        binding.last_origin_io_at_ms = Some(now_ms);
        if reservation_refreshed {
            binding.last_reservation_refresh_at_ms = Some(now_ms);
        }
    }
}

struct CommittedOriginManifest {
    fetched: FetchedOriginManifest,
    refresh_interval_ms: u64,
    wake_segment_scheduler: bool,
    wake_map_scheduler: bool,
}

async fn fetch_and_commit_manifest_with_policy(
    request: &mut OriginRefreshRequest,
) -> Result<CommittedOriginManifest, OriginManifestFetchError> {
    let mut target_url = None;
    let mut target_provider_url_index = None;
    let attempts = request.retry_policy.attempt_count();
    let mut last_error = OriginManifestFetchError::RetryExhausted;

    for attempt_index in 0..attempts {
        let delay_ms = {
            let jitter = if request.retry_policy.jitter_max_ms == 0 {
                0
            } else {
                fastrand::u64(0..=request.retry_policy.jitter_max_ms)
            };
            request.retry_policy.delay_for_attempt_ms(attempt_index, jitter).unwrap_or_default()
        };
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }

        let fetch_result = if let Some(target) = target_url.as_ref() {
            fetch_origin_manifest_for_hls_reject_attempt(request, target, target_provider_url_index, attempt_index)
                .await
        } else {
            fetch_origin_manifest_with_global_policy(request, &request.origin_input_source).await
        };

        let mut fetched = match fetch_result {
            Ok(fetched) => fetched.with_attempts(attempt_index + 1),
            Err(err)
                if target_url.is_some()
                    && is_hls_retryable_manifest_reject_fetch_error(&err)
                    && attempt_index + 1 < attempts =>
            {
                log_origin_refresh_retry_scheduled(
                    &request.origin_entry,
                    attempt_index,
                    next_retry_delay_ms(&request.retry_policy, attempt_index, None),
                    format!("reason=timeline-rejected error={}", safe_origin_log_value(format!("{err:?}"))),
                );
                last_error = err;
                continue;
            }
            Err(err) => return Err(err),
        };
        if fetched.provider_url_index.is_none() {
            fetched.provider_url_index = target_provider_url_index;
        }

        let commit_result = {
            let mut session = request.session.write().await;
            commit_fetched_manifest(&mut session, &fetched, request, current_time_millis())
        };

        match commit_result {
            Ok((refresh_interval_ms, wake_segment_scheduler, wake_map_scheduler)) => {
                return Ok(CommittedOriginManifest {
                    fetched,
                    refresh_interval_ms,
                    wake_segment_scheduler,
                    wake_map_scheduler,
                });
            }
            Err(OriginManifestCommitError::TimelineRejected) => {
                target_url = Url::parse(&fetched.resolved_request_url).ok();
                target_provider_url_index = fetched.provider_url_index;
                if attempt_index + 1 == attempts {
                    return Err(OriginManifestFetchError::RetryExhausted);
                }
                log_origin_refresh_retry_scheduled(
                    &request.origin_entry,
                    attempt_index,
                    next_retry_delay_ms(&request.retry_policy, attempt_index, None),
                    "error=timeline-rejected",
                );
                last_error = OriginManifestFetchError::RetryExhausted;
            }
        }
    }

    Err(last_error)
}

fn is_hls_retryable_manifest_reject_fetch_error(err: &OriginManifestFetchError) -> bool {
    matches!(
        err,
        OriginManifestFetchError::RetryableStatus(_, _)
            | OriginManifestFetchError::Request(_)
            | OriginManifestFetchError::Redirect(_)
            | OriginManifestFetchError::Timeout
            | OriginManifestFetchError::RetryExhausted
    )
}

fn commit_fetched_manifest(
    session: &mut super::HlsSession,
    fetched: &FetchedOriginManifest,
    request: &OriginRefreshRequest,
    fetch_finished_at_ms: u64,
) -> Result<(u64, bool, bool), OriginManifestCommitError> {
    let existing_transient_reason = match &session.mode {
        HlsSessionMode::TransientPassthrough { reason } => Some(reason.clone()),
        HlsSessionMode::NormalCacheTimeline => None,
    };

    match (existing_transient_reason, parse_origin_media_manifest(&fetched.body, &fetched.final_manifest_url)) {
        (None, OriginManifestParseOutcome::Normal(manifest)) => {
            let result = commit_normal_manifest(
                session,
                &manifest,
                fetched.redirect_host.as_deref(),
                fetched.provider_url_index,
                request,
                &fetched.resolved_request_url,
                fetch_finished_at_ms,
            );
            result.map(|refresh_interval_ms| (refresh_interval_ms, true, true))
        }
        (Some(reason), _) => {
            let timeline = validate_transient_manifest_continuity_for_commit(
                session,
                &fetched.body,
                fetched.redirect_host.as_deref(),
            )?;
            Ok((
                commit_transient_manifest(
                    session,
                    &fetched.body,
                    &fetched.final_manifest_url,
                    &fetched.resolved_request_url,
                    fetched.redirect_host.as_deref(),
                    fetched.provider_url_index,
                    &request.headers,
                    reason,
                    &request.reverse_proxy_rewrite_secret,
                    request.transient_resource_ttl_ms,
                    fetch_finished_at_ms,
                    timeline,
                ),
                false,
                false,
            ))
        }
        (None, OriginManifestParseOutcome::TransientPassthrough { reason }) => {
            let timeline = validate_transient_manifest_continuity_for_commit(
                session,
                &fetched.body,
                fetched.redirect_host.as_deref(),
            )?;
            request.segment_worker_pool.metrics().record_transient_switch();
            Ok((
                commit_transient_manifest(
                    session,
                    &fetched.body,
                    &fetched.final_manifest_url,
                    &fetched.resolved_request_url,
                    fetched.redirect_host.as_deref(),
                    fetched.provider_url_index,
                    &request.headers,
                    map_transient_reason(reason),
                    &request.reverse_proxy_rewrite_secret,
                    request.transient_resource_ttl_ms,
                    fetch_finished_at_ms,
                    timeline,
                ),
                false,
                false,
            ))
        }
    }
}

fn validate_origin_manifest_continuity_for_commit(
    session: &super::HlsSession,
    redirect_host: Option<&str>,
    timeline: ParsedOriginManifestTimeline,
) -> Result<(), OriginManifestCommitError> {
    if can_use_redirect_manifest(
        session.origin_seq_highwater,
        session.last_redirect_host.as_deref(),
        redirect_host,
        timeline.origin_manifest_sequence,
        timeline.origin_manifest_segment_cnt,
    ) {
        return Ok(());
    }

    warn!(
        "HLS origin manifest rejected: session={} proxy_session_id={} reason=redirect-or-sequence-check media_sequence={} segments={}",
        safe_session_key(&session.key),
        safe_proxy_session_id(&session.proxy_session_id),
        timeline.origin_manifest_sequence,
        timeline.origin_manifest_segment_cnt
    );
    Err(OriginManifestCommitError::TimelineRejected)
}

fn validate_transient_manifest_continuity_for_commit(
    session: &super::HlsSession,
    body: &str,
    redirect_host: Option<&str>,
) -> Result<ParsedOriginManifestTimeline, OriginManifestCommitError> {
    let timeline = parse_origin_manifest_timeline(body).map_err(|reason| {
        warn!(
            "HLS origin manifest rejected: session={} proxy_session_id={} reason=malformed-transient-timeline error={}",
            safe_session_key(&session.key),
            safe_proxy_session_id(&session.proxy_session_id),
            safe_origin_log_value(format!("{reason:?}"))
        );
        OriginManifestCommitError::TimelineRejected
    })?;
    validate_origin_manifest_continuity_for_commit(session, redirect_host, timeline)?;

    Ok(timeline)
}

#[allow(clippy::too_many_arguments)]
fn commit_transient_manifest(
    session: &mut super::HlsSession,
    body: &str,
    final_manifest_url: &str,
    resolved_request_url: &str,
    redirect_host: Option<&str>,
    provider_url_index: Option<usize>,
    request_headers: &HeaderMap,
    reason: TransientPassthroughReason,
    reverse_proxy_rewrite_secret: &[u8],
    transient_resource_ttl_ms: u64,
    rendered_at_ms: u64,
    timeline: ParsedOriginManifestTimeline,
) -> u64 {
    let was_normal = matches!(session.mode, HlsSessionMode::NormalCacheTimeline);
    let reason_log_fields = transient_reason_log_fields(&reason);
    session.mode = HlsSessionMode::TransientPassthrough { reason };
    if was_normal {
        info!(
            "HLS session switched to transient passthrough: session={} proxy_session_id={} {}",
            safe_session_key(&session.key),
            safe_proxy_session_id(&session.proxy_session_id),
            reason_log_fields
        );
    }
    session.origin_request_headers = request_headers.clone();
    session.transient.set_resource_ttl_ms(transient_resource_ttl_ms);
    session.transient.prune_expired(rendered_at_ms);
    if let Some(redirect_host) = redirect_host {
        session.last_redirect_host = Some(redirect_host.to_string());
    }
    session.last_successful_manifest_target_url = Some(resolved_request_url.to_string());
    session.last_successful_manifest_provider_url_index = provider_url_index;

    let handoff_discontinuity_sequence = session.take_pending_handoff_discontinuity_sequence();
    let rewritten = if handoff_discontinuity_sequence.is_some() {
        TransientManifestRewriter::rewrite_with_options(
            body,
            final_manifest_url,
            &session.proxy_session_id,
            reverse_proxy_rewrite_secret,
            rendered_at_ms,
            transient_resource_ttl_ms,
            TransientRewriteOptions { handoff_discontinuity_sequence },
        )
    } else {
        TransientManifestRewriter::rewrite(
            body,
            final_manifest_url,
            &session.proxy_session_id,
            reverse_proxy_rewrite_secret,
            rendered_at_ms,
            transient_resource_ttl_ms,
        )
    };
    session.transient.upsert_resources(rewritten.resources);
    session.transient.replace_manifest(rewritten.body, rendered_at_ms);
    if let Some(highwater) = timeline.origin_highwater() {
        session.origin_seq_highwater =
            Some(session.origin_seq_highwater.map_or(highwater, |current| current.max(highwater)));
    }

    let timing = parse_manifest_timing(body);
    if let Some(target_duration_ms) = timing.target_duration_ms {
        if let Ok(target_duration_secs) = u32::try_from(target_duration_ms / 1_000) {
            session.target_duration = Some(target_duration_secs);
        }
    }
    let target_duration_ms =
        timing.target_duration_ms.or_else(|| session.target_duration.map(|duration| u64::from(duration) * 1_000));
    log_and_compute_manifest_refresh_interval(session, timing.last_segment_duration_ms, target_duration_ms)
}

fn commit_normal_manifest(
    session: &mut super::HlsSession,
    manifest: &ParsedOriginManifest,
    redirect_host: Option<&str>,
    provider_url_index: Option<usize>,
    request: &OriginRefreshRequest,
    resolved_request_url: &str,
    rendered_at_ms: u64,
) -> Result<u64, OriginManifestCommitError> {
    validate_origin_manifest_continuity_for_commit(
        session,
        redirect_host,
        ParsedOriginManifestTimeline {
            origin_manifest_sequence: manifest.origin_manifest_sequence,
            origin_manifest_segment_cnt: manifest.origin_manifest_segment_cnt,
        },
    )?;

    let segment_durations = manifest.segments.iter().map(|segment| segment.duration_ms).collect::<Vec<_>>();
    session.render_policy = RenderPolicy::from_strip_config(&request.strip, &segment_durations);
    session.apply_origin_manifest(manifest).map_err(|_| OriginManifestCommitError::TimelineRejected)?;
    session.origin_request_headers = request.headers.clone();
    session.queue_map_fetch_candidates(rendered_at_ms);
    let backpressure = request.segment_worker_pool.classify_backpressure_for_session(session);
    let queue_report = session.queue_manifest_fetch_candidates(rendered_at_ms, backpressure.allows_prefetch());
    request.segment_worker_pool.metrics().record_prefetch_queued(queue_report.prefetch_queued);
    request.segment_worker_pool.metrics().record_prefetch_skipped(queue_report.prefetch_skipped);
    if queue_report.prefetch_queued > 0 {
        debug!(
            "HLS segment queued for prefetch: session={} proxy_session_id={} count={}",
            safe_session_key(&session.key),
            safe_proxy_session_id(&session.proxy_session_id),
            queue_report.prefetch_queued
        );
    }
    if queue_report.prefetch_skipped > 0 {
        debug!(
            "HLS segment queued for prefetch skipped by backpressure: session={} proxy_session_id={} count={} state={backpressure:?}",
            safe_session_key(&session.key),
            safe_proxy_session_id(&session.proxy_session_id),
            queue_report.prefetch_skipped
        );
    }
    if let Some(redirect_host) = redirect_host {
        session.last_redirect_host = Some(redirect_host.to_string());
    }
    session.last_successful_manifest_target_url = Some(resolved_request_url.to_string());
    session.last_successful_manifest_provider_url_index = provider_url_index;
    match HlsManifestRenderer::render(session, rendered_at_ms) {
        Ok(rendered) => {
            let segment_count = rendered.segment_proxy_seqs.len();
            let render_gap_segments = rendered.render_gap_segments;
            session.store_rendered_manifest(rendered);
            request.segment_worker_pool.metrics().record_manifest_rendered();
            info!(
                "HLS manifest rendered: session={} proxy_session_id={} media_sequence={} segments={} render_gap_segments={}",
                safe_session_key(&session.key),
                safe_proxy_session_id(&session.proxy_session_id),
                manifest.origin_manifest_sequence,
                segment_count,
                render_gap_segments
            );
        }
        Err(err) => {
            request.segment_worker_pool.metrics().record_manifest_render_skipped();
            debug!(
                "HLS manifest render skipped: session={} proxy_session_id={} reason={err:?}",
                safe_session_key(&session.key),
                safe_proxy_session_id(&session.proxy_session_id)
            );
        }
    }

    let last_segment_duration_ms = manifest.segments.last().map(|segment| segment.duration_ms);
    let target_duration_ms = manifest.target_duration.map(|duration| u64::from(duration) * 1_000);
    Ok(log_and_compute_manifest_refresh_interval(session, last_segment_duration_ms, target_duration_ms))
}

fn log_and_compute_manifest_refresh_interval(
    session: &super::HlsSession,
    last_segment_duration_ms: Option<u64>,
    target_duration_ms: Option<u64>,
) -> u64 {
    let refresh_interval_ms = compute_origin_refresh_interval_ms(last_segment_duration_ms, target_duration_ms);
    let timing_source = if last_segment_duration_ms.is_some() {
        "last_segment_duration"
    } else if target_duration_ms.is_some() {
        "target_duration"
    } else {
        "fallback"
    };
    debug!(
        "HLS manifest timing parsed: session={} target_duration={} last_segment_duration={} next_refresh_in_s={} source={}",
        safe_session_key(&session.key),
        format_optional_millis_as_seconds(target_duration_ms),
        format_optional_millis_as_seconds(last_segment_duration_ms),
        format_millis_as_seconds(refresh_interval_ms),
        timing_source
    );
    refresh_interval_ms
}

fn format_optional_millis_as_seconds(value_ms: Option<u64>) -> String {
    value_ms.map_or_else(|| "none".to_string(), format_millis_as_seconds)
}

fn format_millis_as_seconds(value_ms: u64) -> String {
    let seconds = value_ms / 1_000;
    let millis = value_ms % 1_000;
    format!("{seconds}.{millis:03}")
}

pub fn can_use_redirect_manifest(
    session_seq_highwater: Option<u64>,
    last_redirect_host: Option<&str>,
    redirect_host: Option<&str>,
    origin_manifest_sequence: u64,
    origin_manifest_segment_cnt: usize,
) -> bool {
    let Some(highwater) = session_seq_highwater else {
        return true;
    };
    if redirect_host.is_some() && redirect_host == last_redirect_host {
        return true;
    }
    let segment_cnt = u64::try_from(origin_manifest_segment_cnt).unwrap_or(u64::MAX);
    if segment_cnt == 0 {
        return false;
    }
    let Some(next_highwater) = origin_manifest_sequence.checked_add(segment_cnt.saturating_sub(1)) else {
        return false;
    };
    next_highwater > highwater && origin_manifest_sequence <= highwater.saturating_add(1)
}

fn map_transient_reason(reason: OriginManifestTransientReason) -> TransientPassthroughReason {
    match reason {
        OriginManifestTransientReason::ExtXKey => TransientPassthroughReason::ExtXKey,
        OriginManifestTransientReason::UnsupportedTag { tag } => TransientPassthroughReason::UnsupportedTag { tag },
        OriginManifestTransientReason::ParserUnsupportedFeature { feature } => {
            TransientPassthroughReason::ParserUnsupportedFeature { feature }
        }
    }
}

fn transient_reason_log_fields(reason: &TransientPassthroughReason) -> String {
    match reason {
        TransientPassthroughReason::ExtXKey => "reason=ext_x_key".to_string(),
        TransientPassthroughReason::UnsupportedTag { tag } => format!("reason=unsupported_tag tag={tag}"),
        TransientPassthroughReason::ParserUnsupportedFeature { feature } => {
            format!("reason=parser_unsupported_feature feature={feature}")
        }
    }
}

pub fn compute_origin_refresh_interval_ms(
    last_segment_duration_ms: Option<u64>,
    target_duration_ms: Option<u64>,
) -> u64 {
    last_segment_duration_ms.or(target_duration_ms).map_or(2_000, |duration_ms| duration_ms / 2).clamp(1_000, 6_000)
}

pub fn cold_start_retry_after_seconds() -> u64 { COLD_START_RETRY_AFTER_SECONDS }

#[cfg(test)]
pub async fn refresh_from_live_hls_entrypoint_with_retries(
    origin_entry: &LiveHlsOriginEntry,
    headers: &HeaderMap,
    client: &Client,
    no_redirect_client: &Client,
    use_manual_redirects: bool,
    origin_manifest_timeout_ms: u64,
    retry_policy: &RetryPolicy,
) -> Result<FetchedOriginManifest, OriginManifestFetchError> {
    let mut retry_after_delay_ms = None;
    let attempts = retry_policy.attempt_count();

    for attempt_index in 0..attempts {
        let delay_ms = retry_after_delay_ms.take().unwrap_or_else(|| {
            let jitter =
                if retry_policy.jitter_max_ms == 0 { 0 } else { fastrand::u64(0..=retry_policy.jitter_max_ms) };
            retry_policy.delay_for_attempt_ms(attempt_index, jitter).unwrap_or_default()
        });
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }

        let fetch_result = timeout(
            Duration::from_millis(origin_manifest_timeout_ms.max(1)),
            fetch_origin_manifest_once(
                origin_entry.url(),
                headers,
                client,
                no_redirect_client,
                use_manual_redirects,
                None,
            ),
        )
        .await
        .map_err(|_| OriginManifestFetchError::Timeout);

        match fetch_result {
            Ok(Ok(fetched)) => return Ok(fetched.with_attempts(attempt_index + 1)),
            Ok(Err(OriginManifestFetchError::PermanentStatus(status))) => {
                return Err(OriginManifestFetchError::PermanentStatus(status));
            }
            Ok(Err(OriginManifestFetchError::NonRetryableStatus(status))) => {
                return Err(OriginManifestFetchError::NonRetryableStatus(status));
            }
            Ok(Err(OriginManifestFetchError::RetryableStatus(status, retry_after_ms))) => {
                if attempt_index + 1 == attempts {
                    return Err(OriginManifestFetchError::RetryableStatus(status, retry_after_ms));
                }
                log_origin_refresh_retry_scheduled(
                    origin_entry,
                    attempt_index,
                    next_retry_delay_ms(retry_policy, attempt_index, retry_after_ms),
                    format!("status={}", status.as_u16()),
                );
                retry_after_delay_ms = retry_after_ms;
            }
            Ok(Err(OriginManifestFetchError::Request(err))) => {
                if attempt_index + 1 == attempts {
                    return Err(OriginManifestFetchError::Request(err));
                }
                log_origin_refresh_retry_scheduled(
                    origin_entry,
                    attempt_index,
                    next_retry_delay_ms(retry_policy, attempt_index, None),
                    format!("error={}", safe_origin_log_value(&err)),
                );
            }
            Ok(Err(err @ (OriginManifestFetchError::Redirect(_) | OriginManifestFetchError::Timeout))) => {
                if attempt_index + 1 == attempts {
                    return Err(err);
                }
                log_origin_refresh_retry_scheduled(
                    origin_entry,
                    attempt_index,
                    next_retry_delay_ms(retry_policy, attempt_index, None),
                    format!("error={}", safe_origin_log_value(format!("{err:?}"))),
                );
            }
            Ok(Err(OriginManifestFetchError::RetryExhausted)) => return Err(OriginManifestFetchError::RetryExhausted),
            Ok(Err(OriginManifestFetchError::ProviderUnavailable)) => {
                return Err(OriginManifestFetchError::ProviderUnavailable);
            }
            Err(OriginManifestFetchError::Timeout) => {
                if attempt_index + 1 == attempts {
                    return Err(OriginManifestFetchError::Timeout);
                }
                log_origin_refresh_retry_scheduled(
                    origin_entry,
                    attempt_index,
                    next_retry_delay_ms(retry_policy, attempt_index, None),
                    "error=timeout",
                );
            }
            Err(err) => return Err(err),
        }
    }

    Err(OriginManifestFetchError::RetryExhausted)
}

fn next_retry_delay_ms(retry_policy: &RetryPolicy, attempt_index: usize, retry_after_ms: Option<u64>) -> u64 {
    retry_after_ms.unwrap_or_else(|| retry_policy.delays_ms.get(attempt_index + 1).copied().unwrap_or_default())
}

fn log_origin_refresh_retry_scheduled(
    origin_entry: &LiveHlsOriginEntry,
    attempt_index: usize,
    delay_ms: u64,
    detail: impl AsRef<str>,
) {
    warn!(
        "HLS origin manifest refresh retry scheduled: origin_entry={} attempt={} {} delay_ms={delay_ms}",
        safe_origin_log_value(origin_entry.url().as_str()),
        attempt_index + 1,
        detail.as_ref()
    );
}

impl FetchedOriginManifest {
    fn with_attempts(mut self, attempts: usize) -> Self {
        self.attempts = attempts;
        self
    }
}

async fn fetch_origin_manifest_once(
    entry_url: &Url,
    headers: &HeaderMap,
    client: &Client,
    no_redirect_client: &Client,
    use_manual_redirects: bool,
    provider_url_index: Option<usize>,
) -> Result<FetchedOriginManifest, OriginManifestFetchError> {
    if use_manual_redirects {
        fetch_origin_manifest_with_manual_redirects(entry_url, headers, no_redirect_client, provider_url_index).await
    } else {
        let response = client.get(entry_url.clone()).headers(headers.clone()).send().await.map_err(|err| {
            OriginManifestFetchError::Request(sanitize_sensitive_info(err.to_string().as_str()).to_string())
        })?;
        response_to_fetched_manifest(response, provider_url_index, entry_url.clone()).await
    }
}

async fn fetch_origin_manifest_with_manual_redirects(
    entry_url: &Url,
    headers: &HeaderMap,
    client: &Client,
    provider_url_index: Option<usize>,
) -> Result<FetchedOriginManifest, OriginManifestFetchError> {
    let mut current_url = entry_url.clone();
    let mut current_headers = headers.clone();
    let mut remaining_redirects = MAX_MANUAL_REDIRECTS;

    loop {
        let response =
            client.get(current_url.clone()).headers(current_headers.clone()).send().await.map_err(|err| {
                OriginManifestFetchError::Request(sanitize_sensitive_info(err.to_string().as_str()).to_string())
            })?;
        if !response.status().is_redirection() {
            return response_to_fetched_manifest(response, provider_url_index, entry_url.clone()).await;
        }
        if remaining_redirects == 0 {
            return Err(OriginManifestFetchError::Redirect("too many redirects".to_string()));
        }
        let response_url = response.url().clone();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| OriginManifestFetchError::Redirect("redirect missing location".to_string()))?;
        let next_url = response_url
            .join(location)
            .or_else(|_| Url::parse(location))
            .map_err(|_| OriginManifestFetchError::Redirect("redirect location invalid".to_string()))?;

        if !same_origin(&response_url, &next_url) {
            strip_sensitive_headers_for_cross_origin_redirect(&mut current_headers);
        }
        current_url = next_url;
        remaining_redirects = remaining_redirects.saturating_sub(1);
    }
}

async fn fetch_origin_manifest_for_hls_reject_attempt(
    request: &OriginRefreshRequest,
    target_url: &Url,
    provider_url_index: Option<usize>,
    attempt_index: usize,
) -> Result<FetchedOriginManifest, OriginManifestFetchError> {
    debug!(
        "HLS origin manifest request started: reason=timeline-rejected attempt={} request_target={}",
        attempt_index + 1,
        safe_origin_log_value(target_url.as_str())
    );
    timeout(
        Duration::from_millis(request.origin_manifest_timeout_ms.max(1)),
        fetch_origin_manifest_once(
            target_url,
            &request.headers,
            &request.client,
            &request.no_redirect_client,
            request.use_manual_redirects,
            provider_url_index,
        ),
    )
    .await
    .map_err(|_| OriginManifestFetchError::Timeout)?
}

async fn fetch_origin_manifest_with_global_policy(
    request: &OriginRefreshRequest,
    input_source: &InputSource,
) -> Result<FetchedOriginManifest, OriginManifestFetchError> {
    let account_binding = {
        let session = request.session.read().await;
        if session.origin_account_binding.is_some() {
            "present"
        } else {
            "absent"
        }
    };
    debug!(
        "HLS origin manifest request started: account_binding={account_binding} origin_entry={}",
        safe_origin_log_value(input_source.url.as_str())
    );
    let download_result = timeout(Duration::from_millis(request.origin_manifest_timeout_ms.max(1)), async {
        if request.use_manual_redirects {
            download_text_content_with_manual_redirects(
                &request.app_config,
                &request.no_redirect_client,
                input_source,
                Some(&request.headers),
                None,
                false,
                MAX_MANUAL_REDIRECTS,
            )
            .await
        } else {
            download_text_content(
                &request.app_config,
                &request.client,
                input_source,
                Some(&request.headers),
                None,
                false,
            )
            .await
        }
    })
    .await
    .map_err(|_| OriginManifestFetchError::Timeout)?;
    let (body, final_manifest_url) = download_result.map_err(|err| {
        OriginManifestFetchError::Request(sanitize_sensitive_info(err.to_string().as_str()).to_string())
    })?;
    let provider_url_index = input_source.get_provider().map(|provider| provider.get_current_index());
    let resolved_request_url =
        resolved_hls_manifest_request_url_from_input(input_source, provider_url_index, request.origin_entry.url());
    fetched_manifest_from_downloaded_text(body, final_manifest_url, &resolved_request_url, provider_url_index)
}

fn fetched_manifest_from_downloaded_text(
    body: String,
    final_manifest_url: String,
    resolved_request_url: &Url,
    provider_url_index: Option<usize>,
) -> Result<FetchedOriginManifest, OriginManifestFetchError> {
    let final_url = Url::parse(final_manifest_url.as_str()).map_err(|err| {
        OriginManifestFetchError::Request(sanitize_sensitive_info(err.to_string().as_str()).to_string())
    })?;
    debug!(
        "HLS origin manifest response received: request_target={} final_target={} status=200",
        safe_origin_log_value(resolved_request_url.as_str()),
        safe_origin_log_value(final_url.as_str())
    );
    Ok(FetchedOriginManifest {
        body,
        final_manifest_url,
        resolved_request_url: resolved_request_url.to_string(),
        redirect_host: final_url.host_str().map(str::to_string),
        provider_url_index,
        status: StatusCode::OK,
        attempts: 1,
    })
}

async fn response_to_fetched_manifest(
    response: reqwest::Response,
    provider_url_index: Option<usize>,
    resolved_request_url: Url,
) -> Result<FetchedOriginManifest, OriginManifestFetchError> {
    let status = response.status();
    debug!(
        "HLS origin manifest response received: request_target={} final_target={} status={}",
        safe_origin_log_value(resolved_request_url.as_str()),
        safe_origin_log_value(response.url().as_str()),
        status.as_u16()
    );
    match classify_origin_manifest_status(status) {
        OriginManifestStatusClass::Success => {
            let final_url = response.url().clone();
            let redirect_host = final_url.host_str().map(str::to_string);
            let body = response.text().await.map_err(|err| {
                OriginManifestFetchError::Request(sanitize_sensitive_info(err.to_string().as_str()).to_string())
            })?;
            Ok(FetchedOriginManifest {
                body,
                final_manifest_url: final_url.to_string(),
                resolved_request_url: resolved_request_url.to_string(),
                redirect_host,
                provider_url_index,
                status,
                attempts: 1,
            })
        }
        OriginManifestStatusClass::Retryable => {
            Err(OriginManifestFetchError::RetryableStatus(status, retry_after_delay_ms(response.headers())))
        }
        OriginManifestStatusClass::PermanentFailure => Err(OriginManifestFetchError::PermanentStatus(status)),
        OriginManifestStatusClass::NonRetryableFailure => Err(OriginManifestFetchError::NonRetryableStatus(status)),
    }
}

fn resolved_hls_manifest_request_url_from_input(
    input_source: &InputSource,
    provider_url_index: Option<usize>,
    fallback_url: &Url,
) -> Url {
    let fallback = || Url::parse(input_source.url.as_str()).unwrap_or_else(|_| fallback_url.clone());
    let (Some(provider), Some(provider_url_index)) = (input_source.get_provider(), provider_url_index) else {
        return fallback();
    };
    match resolve_provider_scheme_url_with_provider_index(
        input_source.url.as_str(),
        Some(Arc::clone(provider)),
        provider_url_index,
    ) {
        Ok((_provider, resolved_url)) => Url::parse(resolved_url.as_ref()).unwrap_or_else(|err| {
            debug!(
                "HLS provider URL resolution returned invalid URL: error={} origin={}",
                sanitize_sensitive_info(err.to_string().as_str()),
                safe_origin_log_value(input_source.url.as_str())
            );
            fallback()
        }),
        Err(err) => {
            debug!(
                "HLS provider URL resolution failed: error={} origin={}",
                sanitize_sensitive_info(err.to_string().as_str()),
                safe_origin_log_value(input_source.url.as_str())
            );
            fallback()
        }
    }
}

fn same_origin(lhs: &Url, rhs: &Url) -> bool {
    lhs.scheme().eq_ignore_ascii_case(rhs.scheme())
        && lhs.host_str() == rhs.host_str()
        && lhs.port_or_known_default() == rhs.port_or_known_default()
}

fn strip_sensitive_headers_for_cross_origin_redirect(headers: &mut HeaderMap) {
    super::scrub_hls_origin_headers(headers, None);
}

pub fn retry_after_delay_ms(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(|seconds| seconds.saturating_mul(1_000))
}

fn current_time_millis() -> u64 { chrono::Utc::now().timestamp_millis().try_into().unwrap_or_default() }

#[cfg(test)]
mod tests {
    use super::{
        can_use_redirect_manifest, classify_origin_manifest_status, commit_fetched_manifest,
        compute_origin_refresh_interval_ms, format_millis_as_seconds, format_optional_millis_as_seconds,
        refresh_from_live_hls_entrypoint_with_retries, resolved_hls_manifest_request_url_from_input,
        retry_after_delay_ms, transient_reason_log_fields, FetchedOriginManifest, LiveHlsOriginEntry,
        OriginManifestFetchError, OriginManifestStatusClass, OriginRefreshRequest, OriginRefreshState, RetryPolicy,
    };
    use crate::{
        api::model::{
            maybe_trigger_origin_refresh, HlsMapWorkerPool, HlsSegmentCache, HlsSegmentRepairManager,
            HlsSegmentWorkerPool, HlsSession, HlsSessionKey, HlsSessionMode, TransientPassthroughReason,
        },
        model::{
            AppConfig, Config, ConfigProvider, HlsSegmentRepairConfig, HlsSegmentRepairMode,
            ReverseProxyDisabledHeaderConfig,
            SourcesConfig, StripConfig, StripMode,
        },
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
    use shared::model::{ConfigPaths, ConfigProviderDto, ProviderUrlSelectionPolicy};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::{Mutex, RwLock},
    };

    fn test_session() -> Arc<RwLock<HlsSession>> {
        Arc::new(RwLock::new(HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0)))
    }

    fn test_segment_repair_manager() -> Arc<HlsSegmentRepairManager> {
        Arc::new(HlsSegmentRepairManager::new(HlsSegmentRepairConfig {
            max_level: HlsSegmentRepairMode::Off,
            apply_to_first_segments: 1,
            max_parallel_repairs: 1,
            ..Default::default()
        }))
    }

    fn test_app_config() -> Arc<AppConfig> {
        Arc::new(AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
            hdhomerun: Arc::new(ArcSwapOption::empty()),
            api_proxy: Arc::new(ArcSwapOption::empty()),
            file_locks: Arc::new(crate::utils::FileLockManager::default()),
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
            media_tools: Arc::new(crate::model::MediaToolCapabilities::default()),
        })
    }

    #[test]
    fn origin_refresh_state_starts_only_when_due_and_not_in_flight() {
        let mut state = OriginRefreshState { next_fetch_allowed_at_ms: 100, ..OriginRefreshState::default() };
        assert!(!state.is_due(99));
        assert!(state.is_due(100));
        state.mark_started(100);
        assert!(!state.is_due(101));
    }

    #[test]
    fn origin_refresh_failure_backoff_ramps_and_success_resets_counter() {
        let mut state = OriginRefreshState::default();

        state.mark_started(1_000);
        state.mark_failure(1_100);
        assert_eq!(state.consecutive_failures, 1);
        assert_eq!(state.last_error_at_ms, Some(1_100));
        assert_eq!(state.next_fetch_allowed_at_ms, 1_100);
        assert!(state.is_due(1_100));

        state.mark_started(1_200);
        state.mark_failure(1_300);
        assert_eq!(state.consecutive_failures, 2);
        assert_eq!(state.next_fetch_allowed_at_ms, 1_800);
        assert!(!state.is_due(1_799));
        assert!(state.is_due(1_800));

        state.mark_started(1_900);
        state.mark_failure(2_000);
        assert_eq!(state.consecutive_failures, 3);
        assert_eq!(state.next_fetch_allowed_at_ms, 3_000);

        state.mark_started(3_100);
        state.mark_success(3_100, 3_200, 10_000);
        assert_eq!(state.consecutive_failures, 0);
        assert_eq!(state.last_error_at_ms, None);
        assert_eq!(state.next_fetch_allowed_at_ms, 13_100);

        state.mark_started(13_100);
        state.mark_failure(13_200);
        assert_eq!(state.consecutive_failures, 1);
        assert_eq!(state.next_fetch_allowed_at_ms, 13_200);
    }

    #[test]
    fn status_classification_matches_hls_retry_policy() {
        for status in [
            StatusCode::PROXY_AUTHENTICATION_REQUIRED,
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::TOO_EARLY,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
        ] {
            assert_eq!(classify_origin_manifest_status(status), OriginManifestStatusClass::Retryable);
        }
        for status in [
            StatusCode::BAD_REQUEST,
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
            StatusCode::GONE,
        ] {
            assert_eq!(classify_origin_manifest_status(status), OriginManifestStatusClass::PermanentFailure);
        }
    }

    #[test]
    fn retry_after_header_is_parsed_as_milliseconds() {
        let mut headers = HeaderMap::new();
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("3"));
        assert_eq!(retry_after_delay_ms(&headers), Some(3_000));
    }

    #[test]
    fn resolved_hls_manifest_request_url_uses_provider_index_locally() {
        let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
            name: "demo".into(),
            urls: vec!["http://provider-a.example".into(), "http://provider-b.example".into()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::default(),
            dns: None,
        }));
        let provider_entry =
            LiveHlsOriginEntry::parse_with_provider("provider://demo/live/u/p/1.m3u8", Some(provider)).unwrap();

        let resolved = resolved_hls_manifest_request_url_from_input(
            &provider_entry.to_input_source(),
            Some(1),
            provider_entry.url(),
        );
        assert_eq!(resolved.as_str(), "http://provider-b.example/live/u/p/1.m3u8");
        assert!(!resolved.as_str().contains("provider://"));

        let direct_entry = LiveHlsOriginEntry::parse("http://origin.example/live/u/p/1.m3u8").unwrap();
        assert_eq!(
            resolved_hls_manifest_request_url_from_input(&direct_entry.to_input_source(), Some(1), direct_entry.url())
                .as_str(),
            "http://origin.example/live/u/p/1.m3u8"
        );
    }

    #[test]
    fn manifest_timing_log_values_are_seconds_or_none() {
        assert_eq!(format_optional_millis_as_seconds(Some(4_500)), "4.500");
        assert_eq!(format_optional_millis_as_seconds(None), "none");
        assert_eq!(format_millis_as_seconds(2_000), "2.000");
    }

    #[test]
    fn refresh_interval_uses_half_duration_with_clamp() {
        assert_eq!(compute_origin_refresh_interval_ms(Some(8_000), None), 4_000);
        assert_eq!(compute_origin_refresh_interval_ms(Some(500), None), 1_000);
        assert_eq!(compute_origin_refresh_interval_ms(Some(20_000), None), 6_000);
        assert_eq!(compute_origin_refresh_interval_ms(None, None), 2_000);
    }

    #[test]
    fn transient_reason_log_fields_include_unsupported_tag() {
        let reason = TransientPassthroughReason::UnsupportedTag { tag: "#EXT-X-PART".to_string() };

        assert_eq!(transient_reason_log_fields(&reason), "reason=unsupported_tag tag=#EXT-X-PART");
    }

    #[test]
    fn redirect_manifest_acceptance_matches_concept() {
        assert!(can_use_redirect_manifest(None, None, Some("origin.example.com"), 10, 6));
        assert!(can_use_redirect_manifest(
            Some(15),
            Some("redirect-a.example.com"),
            Some("redirect-a.example.com"),
            0,
            6
        ));
        assert!(can_use_redirect_manifest(
            Some(15),
            Some("redirect-a.example.com"),
            Some("redirect-b.example.com"),
            11,
            6
        ));
        assert!(can_use_redirect_manifest(
            Some(15),
            Some("redirect-a.example.com"),
            Some("redirect-b.example.com"),
            16,
            6
        ));
        assert!(!can_use_redirect_manifest(
            Some(15),
            Some("redirect-a.example.com"),
            Some("redirect-b.example.com"),
            17,
            6
        ));
        assert!(!can_use_redirect_manifest(
            Some(15),
            Some("redirect-a.example.com"),
            Some("redirect-b.example.com"),
            10,
            6
        ));
        assert!(!can_use_redirect_manifest(
            Some(15),
            Some("redirect-a.example.com"),
            Some("redirect-b.example.com"),
            16,
            0
        ));
    }

    #[tokio::test]
    async fn concurrent_maybe_trigger_origin_refresh_starts_singleflight_once() {
        let session = test_session();
        let entry =
            LiveHlsOriginEntry::parse("http://127.0.0.1:9/live/user/pass/12345.m3u8").expect("valid origin entry");
        let client = reqwest::Client::new();
        let no_redirect_client =
            reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().expect("client builds");
        let request = OriginRefreshRequest {
            app_config: test_app_config(),
            session: Arc::clone(&session),
            origin_entry: entry.clone(),
            origin_input_source: entry.to_input_source(),
            headers: HeaderMap::new(),
            client,
            no_redirect_client,
            use_manual_redirects: false,
            segment_cache: Arc::new(HlsSegmentCache::new()),
            segment_repair: test_segment_repair_manager(),
            segment_worker_pool: Arc::new(HlsSegmentWorkerPool::default()),
            map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
            origin_manifest_timeout_ms: 1,
            strip: StripConfig { mode: StripMode::Segments, value: 0 },
            retry_policy: RetryPolicy { delays_ms: [0, 0, 0, 0, 0], jitter_max_ms: 0 },
            reverse_proxy_rewrite_secret: b"secret".to_vec(),
            transient_resource_ttl_ms: 300_000,
            disabled_headers: None,
            now_ms: 100,
            origin_io: None,
        };

        let mut handles = Vec::new();
        for _ in 0..8 {
            let request = request.clone();
            handles.push(tokio::spawn(async move { maybe_trigger_origin_refresh(request).await }));
        }

        let started = futures::future::join_all(handles)
            .await
            .into_iter()
            .filter(|result| result.as_ref().is_ok_and(|started| *started))
            .count();
        assert_eq!(started, 1);
    }

    async fn refresh_session_with_origin_body(body: &'static str) -> Arc<RwLock<HlsSession>> {
        let session = test_session();
        let server = spawn_test_origin(Arc::new(move |_path| (200, Vec::new(), body.to_string()))).await;
        let entry = LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url))
            .expect("valid origin entry");
        let request = OriginRefreshRequest {
            app_config: test_app_config(),
            session: Arc::clone(&session),
            origin_entry: entry.clone(),
            origin_input_source: entry.to_input_source(),
            headers: HeaderMap::new(),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("client builds"),
            use_manual_redirects: false,
            segment_cache: Arc::new(HlsSegmentCache::new()),
            segment_repair: test_segment_repair_manager(),
            segment_worker_pool: Arc::new(HlsSegmentWorkerPool::default()),
            map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
            origin_manifest_timeout_ms: 2_000,
            strip: StripConfig { mode: StripMode::Segments, value: 0 },
            retry_policy: no_delay_policy(),
            reverse_proxy_rewrite_secret: b"secret".to_vec(),
            transient_resource_ttl_ms: 300_000,
            disabled_headers: None,
            now_ms: 100,
            origin_io: None,
        };

        assert!(maybe_trigger_origin_refresh(request).await);
        for _ in 0..50 {
            if session.read().await.transient.last_manifest_body.is_some() {
                return session;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        session
    }

    #[tokio::test]
    async fn refresh_stores_headers_after_hls_origin_policy() {
        let session = test_session();
        let server = spawn_test_origin(Arc::new(|_path| {
            (200, Vec::new(), "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nseg.ts\n".to_string())
        }))
        .await;
        let entry = LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url))
            .expect("valid origin entry");
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer secret"));
        headers.insert(header::COOKIE, HeaderValue::from_static("sid=secret"));
        headers.insert(HeaderName::from_static("proxy-authorization"), HeaderValue::from_static("Basic secret"));
        headers.insert(header::HOST, HeaderValue::from_static("proxy.example.com"));
        headers.insert(HeaderName::from_static("x-blocked"), HeaderValue::from_static("blocked"));
        headers.insert(HeaderName::from_static("cf-ray"), HeaderValue::from_static("cf"));
        headers.insert(header::ACCEPT_LANGUAGE, HeaderValue::from_static("de"));

        let request = OriginRefreshRequest {
            app_config: test_app_config(),
            session: Arc::clone(&session),
            origin_entry: entry.clone(),
            origin_input_source: entry.to_input_source(),
            headers,
            disabled_headers: Some(ReverseProxyDisabledHeaderConfig {
                referer_header: false,
                x_header: true,
                cloudflare_header: true,
                custom_header: Vec::new(),
            }),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("client builds"),
            use_manual_redirects: false,
            segment_cache: Arc::new(HlsSegmentCache::new()),
            segment_repair: test_segment_repair_manager(),
            segment_worker_pool: Arc::new(HlsSegmentWorkerPool::default()),
            map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
            origin_manifest_timeout_ms: 2_000,
            strip: StripConfig { mode: StripMode::Segments, value: 0 },
            retry_policy: no_delay_policy(),
            reverse_proxy_rewrite_secret: b"secret".to_vec(),
            transient_resource_ttl_ms: 300_000,
            now_ms: 100,
            origin_io: None,
        };

        assert!(maybe_trigger_origin_refresh(request).await);
        for _ in 0..50 {
            if session.read().await.last_rendered_manifest.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let session = session.read().await;
        assert!(!session.origin_request_headers.contains_key(header::AUTHORIZATION));
        assert!(!session.origin_request_headers.contains_key(header::COOKIE));
        assert!(!session.origin_request_headers.contains_key("proxy-authorization"));
        assert!(!session.origin_request_headers.contains_key(header::HOST));
        assert!(!session.origin_request_headers.contains_key("x-blocked"));
        assert!(!session.origin_request_headers.contains_key("cf-ray"));
        assert_eq!(session.origin_request_headers.get(header::ACCEPT_LANGUAGE).expect("language"), "de");
    }

    #[tokio::test]
    async fn ext_x_key_manifest_commits_transient_rewrite() {
        let session = refresh_session_with_origin_body(
            "#EXTM3U\n#EXT-X-TARGETDURATION:12\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4.0,\nseg.ts\n",
        )
        .await;
        let session = session.read().await;
        let body = session.transient.last_manifest_body.as_ref().expect("transient body");

        assert!(matches!(
            session.mode,
            HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey }
        ));
        assert!(body.contains("/proxy/hls/live/"));
        assert!(body.contains("/r/"));
        assert!(!body.contains("/hls/user/"));
        assert_eq!(session.transient.resources.len(), 2);
        assert_eq!(session.target_duration, Some(12));
        assert_eq!(session.account_overlap_timing().target_duration_ms, 12_000);
    }

    #[test]
    fn transient_commit_accepts_same_redirect_host_even_when_media_sequence_moves_backward() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
        session.origin_seq_highwater = Some(758);
        session.last_redirect_host = Some("origin.example.com".to_string());
        let previous_manifest =
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:757\n#EXTINF:4.0,\n/proxy/hls/live/session/lease/r/old.ts\n".to_string();
        session.transient.replace_manifest(previous_manifest.clone(), 10);
        let request = test_origin_refresh_request(test_session());
        let fetched = fetched_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:226\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4.0,\nseg.ts\n",
        );

        let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);

        assert!(result.is_ok());
        assert_ne!(session.transient.last_manifest_body.as_deref(), Some(previous_manifest.as_str()));
        assert_eq!(session.origin_seq_highwater, Some(758));
        assert!(session.transient.last_manifest_body.as_ref().is_some_and(|body| body.contains("/r/")));
    }

    #[test]
    fn transient_commit_accepts_monotonic_media_sequence_and_updates_highwater() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
        session.origin_seq_highwater = Some(758);
        session.last_redirect_host = Some("origin.example.com".to_string());
        session.transient.replace_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:757\n#EXTINF:4.0,\n/proxy/hls/live/session/lease/r/old.ts\n".to_string(),
            10,
        );
        let request = test_origin_refresh_request(test_session());
        let fetched = fetched_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:759\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4.0,\nseg759.ts\n#EXTINF:4.0,\nseg760.ts\n",
        );

        let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);

        assert!(result.is_ok());
        assert_eq!(session.origin_seq_highwater, Some(760));
        assert!(session.transient.last_manifest_body.as_ref().is_some_and(|body| body.contains("/r/")));
    }

    #[test]
    fn transient_commit_with_different_redirect_host_accepts_manifest_that_extends_highwater() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
        session.origin_seq_highwater = Some(758);
        session.last_redirect_host = Some("previous.example.com".to_string());
        let request = test_origin_refresh_request(test_session());
        let fetched = fetched_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:758\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4.0,\nseg758.ts\n#EXTINF:4.0,\nseg759.ts\n",
        );

        let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);

        assert!(result.is_ok());
        assert_eq!(session.origin_seq_highwater, Some(759));
        assert!(session.transient.last_manifest_body.as_ref().is_some_and(|body| body.contains("/r/")));
    }

    #[tokio::test]
    async fn unsupported_tag_manifest_commits_transient_rewrite() {
        let session = refresh_session_with_origin_body("#EXTM3U\n#EXT-X-PART:DURATION=1.0,URI=\"part.m4s\"\n").await;
        let session = session.read().await;

        assert!(matches!(
            session.mode,
            HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::UnsupportedTag { .. } }
        ));
        assert!(session.transient.last_manifest_body.is_some());
    }

    #[tokio::test]
    async fn parser_unsupported_feature_manifest_commits_transient_rewrite() {
        let session = refresh_session_with_origin_body("#EXTM3U\n#EXT-X-BYTERANGE:10\n#EXTINF:4.0,\nseg.ts\n").await;
        let session = session.read().await;

        assert!(matches!(
            session.mode,
            HlsSessionMode::TransientPassthrough {
                reason: TransientPassthroughReason::ParserUnsupportedFeature { .. }
            }
        ));
        assert!(session.transient.last_manifest_body.is_some());
    }

    struct TestOriginServer {
        base_url: String,
        requests: Arc<Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }

    type TestOriginHandler = Arc<dyn Fn(String) -> (u16, Vec<(&'static str, String)>, String) + Send + Sync>;

    impl Drop for TestOriginServer {
        fn drop(&mut self) { self.task.abort(); }
    }

    async fn spawn_test_origin(handler: TestOriginHandler) -> TestOriginServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
        let addr = listener.local_addr().expect("local addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_task = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let requests = Arc::clone(&requests_for_task);
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
                    let request = String::from_utf8_lossy(&buf[..used]);
                    let path = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    requests.lock().await.push(path.clone());
                    let (status, headers, body) = handler(path);
                    let reason = match status {
                        200 => "OK",
                        302 => "Found",
                        404 => "Not Found",
                        407 => "Proxy Authentication Required",
                        500 => "Internal Server Error",
                        _ => "Status",
                    };
                    let mut response = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
                        body.len()
                    );
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
        TestOriginServer { base_url: format!("http://{addr}"), requests, task }
    }

    fn no_delay_policy() -> RetryPolicy { RetryPolicy { delays_ms: [0, 0, 0, 0, 0], jitter_max_ms: 0 } }

    fn test_origin_refresh_request(session: Arc<RwLock<HlsSession>>) -> OriginRefreshRequest {
        let entry = LiveHlsOriginEntry::parse("http://origin.example.com/live/user/pass/12345.m3u8")
            .expect("valid origin entry");
        OriginRefreshRequest {
            app_config: test_app_config(),
            session,
            origin_entry: entry.clone(),
            origin_input_source: entry.to_input_source(),
            headers: HeaderMap::new(),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("client builds"),
            use_manual_redirects: false,
            segment_cache: Arc::new(HlsSegmentCache::new()),
            segment_repair: test_segment_repair_manager(),
            segment_worker_pool: Arc::new(HlsSegmentWorkerPool::default()),
            map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
            origin_manifest_timeout_ms: 2_000,
            strip: StripConfig { mode: StripMode::Segments, value: 0 },
            retry_policy: no_delay_policy(),
            reverse_proxy_rewrite_secret: b"secret".to_vec(),
            transient_resource_ttl_ms: 300_000,
            disabled_headers: None,
            now_ms: 100,
            origin_io: None,
        }
    }

    fn fetched_manifest(body: &str) -> FetchedOriginManifest {
        FetchedOriginManifest {
            body: body.to_string(),
            final_manifest_url: "http://origin.example.com/live/final/index.m3u8".to_string(),
            resolved_request_url: "http://origin.example.com/live/user/pass/12345.m3u8".to_string(),
            redirect_host: Some("origin.example.com".to_string()),
            provider_url_index: None,
            status: StatusCode::OK,
            attempts: 1,
        }
    }

    fn manifest_body() -> String { "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nseg.ts\n".to_string() }

    #[tokio::test]
    async fn manifest_retry_starts_at_entrypoint_after_redirect_failure() {
        let redirect_hits = Arc::new(AtomicUsize::new(0));
        let redirect_hits_for_handler = Arc::clone(&redirect_hits);
        let server = spawn_test_origin(Arc::new(move |path| {
            if path == "/live/user/pass/12345.m3u8" {
                return (302, vec![("Location", "/live/play/once/12345".to_string())], String::new());
            }
            if path == "/live/play/once/12345" {
                let hit = redirect_hits_for_handler.fetch_add(1, Ordering::SeqCst);
                if hit < 2 {
                    return (500, Vec::new(), "fail".to_string());
                }
                return (200, Vec::new(), manifest_body());
            }
            (404, Vec::new(), String::new())
        }))
        .await;
        let entry =
            LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");
        let no_redirect_client =
            reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().expect("client builds");

        let fetched = refresh_from_live_hls_entrypoint_with_retries(
            &entry,
            &HeaderMap::new(),
            &reqwest::Client::new(),
            &no_redirect_client,
            true,
            2_000,
            &no_delay_policy(),
        )
        .await
        .expect("refresh eventually succeeds");

        assert_eq!(fetched.attempts, 3);
        assert_eq!(
            *server.requests.lock().await,
            vec![
                "/live/user/pass/12345.m3u8",
                "/live/play/once/12345",
                "/live/user/pass/12345.m3u8",
                "/live/play/once/12345",
                "/live/user/pass/12345.m3u8",
                "/live/play/once/12345"
            ]
        );
    }

    #[tokio::test]
    async fn retryable_407_retries_until_success() {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_for_handler = Arc::clone(&hits);
        let server = spawn_test_origin(Arc::new(move |_path| {
            let hit = hits_for_handler.fetch_add(1, Ordering::SeqCst);
            if hit < 2 {
                return (407, Vec::new(), "retry".to_string());
            }
            (200, Vec::new(), manifest_body())
        }))
        .await;
        let entry =
            LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");

        let fetched = refresh_from_live_hls_entrypoint_with_retries(
            &entry,
            &HeaderMap::new(),
            &reqwest::Client::new(),
            &reqwest::Client::new(),
            false,
            2_000,
            &no_delay_policy(),
        )
        .await
        .expect("refresh eventually succeeds");

        assert_eq!(fetched.attempts, 3);
        assert_eq!(server.requests.lock().await.len(), 3);
    }

    #[tokio::test]
    async fn permanent_404_does_not_retry() {
        let server = spawn_test_origin(Arc::new(|_path| (404, Vec::new(), "missing".to_string()))).await;
        let entry =
            LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");

        let err = refresh_from_live_hls_entrypoint_with_retries(
            &entry,
            &HeaderMap::new(),
            &reqwest::Client::new(),
            &reqwest::Client::new(),
            false,
            2_000,
            &no_delay_policy(),
        )
        .await
        .expect_err("404 is permanent");

        assert!(matches!(err, OriginManifestFetchError::PermanentStatus(StatusCode::NOT_FOUND)));
        assert_eq!(server.requests.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn provider_failover_status_does_not_count_as_hls_retry() {
        let first = spawn_test_origin(Arc::new(|_path| (407, Vec::new(), "rotate".to_string()))).await;
        let second = spawn_test_origin(Arc::new(|_path| (200, Vec::new(), manifest_body()))).await;
        let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
            name: "demo".into(),
            urls: vec![first.base_url.as_str().into(), second.base_url.as_str().into()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::RestartFromFirst,
            dns: None,
        }));
        let session = test_session();
        let entry = LiveHlsOriginEntry::parse_with_provider(
            "provider://demo/live/user/pass/12345.m3u8",
            Some(Arc::clone(&provider)),
        )
        .expect("provider entry url");
        let request = OriginRefreshRequest {
            app_config: test_app_config(),
            session: Arc::clone(&session),
            origin_entry: entry.clone(),
            origin_input_source: entry.to_input_source(),
            headers: HeaderMap::new(),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("client builds"),
            use_manual_redirects: false,
            segment_cache: Arc::new(HlsSegmentCache::new()),
            segment_repair: test_segment_repair_manager(),
            segment_worker_pool: Arc::new(HlsSegmentWorkerPool::default()),
            map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
            origin_manifest_timeout_ms: 2_000,
            strip: StripConfig { mode: StripMode::Segments, value: 0 },
            retry_policy: no_delay_policy(),
            reverse_proxy_rewrite_secret: b"secret".to_vec(),
            transient_resource_ttl_ms: 300_000,
            disabled_headers: None,
            now_ms: 100,
            origin_io: None,
        };

        assert!(maybe_trigger_origin_refresh(request).await);
        for _ in 0..50 {
            if session.read().await.last_successful_manifest_provider_url_index == Some(1) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let first_requests = first.requests.lock().await;
        let second_requests = second.requests.lock().await;
        let first_manifest_requests =
            first_requests.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
        let second_manifest_requests =
            second_requests.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
        assert_eq!(first_manifest_requests, 1);
        assert_eq!(second_manifest_requests, 1);
        assert_eq!(provider.get_current_index(), 1);
        let session = session.read().await;
        assert_eq!(session.last_successful_manifest_provider_url_index, Some(1));
        assert!(session
            .last_successful_manifest_target_url
            .as_ref()
            .is_some_and(|url| { url.starts_with(second.base_url.as_str()) }));
    }

    #[tokio::test]
    async fn timeline_reject_uses_hls_retry_against_successful_target() {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_for_handler = Arc::clone(&hits);
        let server = spawn_test_origin(Arc::new(move |_path| {
            let hit = hits_for_handler.fetch_add(1, Ordering::SeqCst);
            if hit == 0 {
                return (
                    200,
                    Vec::new(),
                    "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:102\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n102.ts\n".to_string(),
                );
            }
            (
                200,
                Vec::new(),
                "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:101\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n101.ts\n#EXTINF:4.0,\n102.ts\n".to_string(),
            )
        }))
        .await;
        let session = test_session();
        {
            let mut session = session.write().await;
            session.origin_seq_highwater = Some(100);
            session.last_redirect_host = Some("other-origin.example.com".to_string());
            session.last_successful_manifest_target_url =
                Some(format!("{}/live/user/pass/12345.m3u8", server.base_url));
        }
        let entry =
            LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");
        let request = OriginRefreshRequest {
            app_config: test_app_config(),
            session: Arc::clone(&session),
            origin_entry: entry.clone(),
            origin_input_source: entry.to_input_source(),
            headers: HeaderMap::new(),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("client builds"),
            use_manual_redirects: false,
            segment_cache: Arc::new(HlsSegmentCache::new()),
            segment_repair: test_segment_repair_manager(),
            segment_worker_pool: Arc::new(HlsSegmentWorkerPool::default()),
            map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
            origin_manifest_timeout_ms: 2_000,
            strip: StripConfig { mode: StripMode::Segments, value: 0 },
            retry_policy: no_delay_policy(),
            reverse_proxy_rewrite_secret: b"secret".to_vec(),
            transient_resource_ttl_ms: 300_000,
            disabled_headers: None,
            now_ms: 100,
            origin_io: None,
        };

        assert!(maybe_trigger_origin_refresh(request).await);
        for _ in 0..50 {
            if session.read().await.origin_seq_highwater == Some(102) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let manifest_requests =
            server.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
        assert_eq!(manifest_requests, 2);
        assert_eq!(session.read().await.origin_seq_highwater, Some(102));
    }

    #[tokio::test]
    async fn timeline_reject_after_provider_failover_retries_resolved_url_without_provider_chain() {
        let first = spawn_test_origin(Arc::new(|_path| (407, Vec::new(), "rotate".to_string()))).await;
        let second_hits = Arc::new(AtomicUsize::new(0));
        let second_hits_for_handler = Arc::clone(&second_hits);
        let second = spawn_test_origin(Arc::new(move |_path| {
            let hit = second_hits_for_handler.fetch_add(1, Ordering::SeqCst);
            if hit == 0 {
                return (
                    200,
                    Vec::new(),
                    "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:102\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n102.ts\n".to_string(),
                );
            }
            (
                200,
                Vec::new(),
                "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:101\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n101.ts\n#EXTINF:4.0,\n102.ts\n".to_string(),
            )
        }))
        .await;
        let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
            name: "demo".into(),
            urls: vec![first.base_url.as_str().into(), second.base_url.as_str().into()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::RestartFromFirst,
            dns: None,
        }));
        let session = test_session();
        {
            session.write().await.origin_seq_highwater = Some(100);
        }
        let entry = LiveHlsOriginEntry::parse_with_provider(
            "provider://demo/live/user/pass/12345.m3u8",
            Some(Arc::clone(&provider)),
        )
        .expect("provider entry url");
        let request = OriginRefreshRequest {
            app_config: test_app_config(),
            session: Arc::clone(&session),
            origin_entry: entry.clone(),
            origin_input_source: entry.to_input_source(),
            headers: HeaderMap::new(),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("client builds"),
            use_manual_redirects: false,
            segment_cache: Arc::new(HlsSegmentCache::new()),
            segment_repair: test_segment_repair_manager(),
            segment_worker_pool: Arc::new(HlsSegmentWorkerPool::default()),
            map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
            origin_manifest_timeout_ms: 2_000,
            strip: StripConfig { mode: StripMode::Segments, value: 0 },
            retry_policy: no_delay_policy(),
            reverse_proxy_rewrite_secret: b"secret".to_vec(),
            transient_resource_ttl_ms: 300_000,
            disabled_headers: None,
            now_ms: 100,
            origin_io: None,
        };

        assert!(maybe_trigger_origin_refresh(request).await);
        for _ in 0..50 {
            if session.read().await.origin_seq_highwater == Some(102) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let first_manifest_requests =
            first.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
        let second_manifest_requests =
            second.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
        assert_eq!(first_manifest_requests, 1);
        assert_eq!(second_manifest_requests, 2);
        assert_eq!(session.read().await.origin_seq_highwater, Some(102));
    }

    #[tokio::test]
    async fn timeline_reject_fetch_errors_use_single_hls_policy_attempts() {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_for_handler = Arc::clone(&hits);
        let server = spawn_test_origin(Arc::new(move |_path| {
            let hit = hits_for_handler.fetch_add(1, Ordering::SeqCst);
            if hit == 0 {
                return (
                    200,
                    Vec::new(),
                    "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:102\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n102.ts\n".to_string(),
                );
            }
            (407, Vec::new(), "retry".to_string())
        }))
        .await;
        let session = test_session();
        {
            session.write().await.origin_seq_highwater = Some(100);
        }
        let entry =
            LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");
        let request = OriginRefreshRequest {
            app_config: test_app_config(),
            session: Arc::clone(&session),
            origin_entry: entry.clone(),
            origin_input_source: entry.to_input_source(),
            headers: HeaderMap::new(),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("client builds"),
            use_manual_redirects: false,
            segment_cache: Arc::new(HlsSegmentCache::new()),
            segment_repair: test_segment_repair_manager(),
            segment_worker_pool: Arc::new(HlsSegmentWorkerPool::default()),
            map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
            origin_manifest_timeout_ms: 2_000,
            strip: StripConfig { mode: StripMode::Segments, value: 0 },
            retry_policy: no_delay_policy(),
            reverse_proxy_rewrite_secret: b"secret".to_vec(),
            transient_resource_ttl_ms: 300_000,
            disabled_headers: None,
            now_ms: 100,
            origin_io: None,
        };

        assert!(maybe_trigger_origin_refresh(request).await);
        for _ in 0..50 {
            if hits.load(Ordering::SeqCst) >= 5 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert_eq!(hits.load(Ordering::SeqCst), 5);
        assert_eq!(session.read().await.origin_seq_highwater, Some(100));
    }
}
