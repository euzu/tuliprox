use crate::{
    api::{
        model::{is_custom_video_stream_enabled, AppState, CustomVideoStreamType, TransportStreamBuffer},
        panel_api::{can_provision_on_exhausted, try_provision_account_on_exhausted},
    },
    model::{ConfigInput, CustomStreamResponse, ProxyUserCredentials},
};
use axum::{
    body::Body,
    http::{header, StatusCode},
    response::IntoResponse,
};
use dashmap::DashMap;
use log::{debug, error};
use shared::{defaults::CUSTOM_VIDEO_PREFIX, utils::sanitize_sensitive_info};
use std::sync::Arc;
use tuliprox_hls::api::{
    build_hls_standalone_custom_plan, HlsAccessLease, HlsAccessLeaseId, HlsRuntimeCustomTailReason,
    HlsStandaloneCustomAccess, ProxySessionId,
};

const PROVISIONING_HLS_TARGET_DURATION_SECS: u64 = 2;
const PROVISIONING_HLS_EXTINF: &str = "2.000000";
pub(crate) const CUSTOM_VIDEO_HLS_PROVISIONING_SEGMENT_COUNT: usize =
    shared::defaults::PANEL_API_PROVISIONING_HLS_SEGMENT_COUNT;
const HLS_PROVISIONING_COMPLETED_REDIRECT_WINDOW_MS: u64 = 60_000;
const HLS_PROVISIONING_STALE_MARKER_MS: u64 = 5 * 60_000;
const HLS_CUSTOM_VIDEO_ROUTE_KIND: &str = "hls";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HlsProvisioningStatus {
    InProgress,
    Ready,
    ProviderExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HlsProvisioningJobGroup {
    last_started_at_ms: u64,
    running_jobs: usize,
    ready_slots: usize,
    recent_failed_jobs: usize,
    last_ready_at_ms: Option<u64>,
    last_failed_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct HlsProvisioningConsumerKey {
    input_name: Arc<str>,
    virtual_id: u32,
}

impl HlsProvisioningConsumerKey {
    fn new(input_name: Arc<str>, virtual_id: u32) -> Self {
        Self { input_name, virtual_id }
    }
}

#[derive(Debug, Clone)]
struct HlsProvisioningConsumer {
    created_at_ms: u64,
    last_seen_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct HlsProvisioningHandoffKey {
    input_name: Arc<str>,
    virtual_id: u32,
    proxy_session_id: Option<String>,
    access_lease_id: Option<String>,
}

impl HlsProvisioningHandoffKey {
    fn new(
        input_name: Arc<str>,
        virtual_id: u32,
        proxy_session_id: Option<&ProxySessionId>,
        access_lease_id: Option<&HlsAccessLeaseId>,
    ) -> Self {
        Self {
            input_name,
            virtual_id,
            proxy_session_id: proxy_session_id.map(|id| id.0.clone()),
            access_lease_id: access_lease_id.map(|id| id.0.clone()),
        }
    }
}

#[derive(Debug, Default)]
pub struct HlsProvisioningState {
    jobs: DashMap<Arc<str>, HlsProvisioningJobGroup>,
    consumers: DashMap<HlsProvisioningConsumerKey, HlsProvisioningConsumer>,
    handoffs: DashMap<HlsProvisioningHandoffKey, u64>,
}

impl HlsProvisioningState {
    pub(in crate::api) fn new() -> Self {
        Self::default()
    }

    fn desired_jobs_for_waiting_consumers(consumer_count: usize) -> usize {
        if consumer_count == 0 {
            0
        } else {
            consumer_count.div_ceil(2)
        }
    }

    fn waiting_consumer_count(&self, input_name: &Arc<str>, now_ms: u64) -> usize {
        self.prune(now_ms);
        self.consumers.iter().filter(|entry| &entry.key().input_name == input_name).count()
    }

    fn start_jobs_for_waiting_consumers(&self, input_name: &Arc<str>, now_ms: u64) -> usize {
        let desired_jobs = Self::desired_jobs_for_waiting_consumers(self.waiting_consumer_count(input_name, now_ms));
        self.start_jobs_until(Arc::clone(input_name), desired_jobs, now_ms)
    }

    fn start_jobs_until(&self, input_name: Arc<str>, desired_jobs: usize, now_ms: u64) -> usize {
        self.prune(now_ms);
        if desired_jobs == 0 {
            return 0;
        }
        if let Some(mut group) = self.jobs.get_mut(&input_name) {
            let counted_jobs =
                group.running_jobs.saturating_add(group.ready_slots).saturating_add(group.recent_failed_jobs);
            if counted_jobs >= desired_jobs {
                return 0;
            }
            let jobs_to_start = desired_jobs - counted_jobs;
            group.running_jobs = group.running_jobs.saturating_add(jobs_to_start);
            group.last_started_at_ms = now_ms;
            jobs_to_start
        } else {
            self.jobs.insert(
                input_name,
                HlsProvisioningJobGroup {
                    last_started_at_ms: now_ms,
                    running_jobs: desired_jobs,
                    ready_slots: 0,
                    recent_failed_jobs: 0,
                    last_ready_at_ms: None,
                    last_failed_at_ms: None,
                },
            );
            desired_jobs
        }
    }

    fn mark_job_ready(&self, input_name: Arc<str>, ready_at_ms: u64) {
        if let Some(mut group) = self.jobs.get_mut(&input_name) {
            group.running_jobs = group.running_jobs.saturating_sub(1);
            group.ready_slots = group.ready_slots.saturating_add(1);
            group.last_ready_at_ms = Some(ready_at_ms);
        } else {
            self.jobs.insert(
                input_name,
                HlsProvisioningJobGroup {
                    last_started_at_ms: ready_at_ms,
                    running_jobs: 0,
                    ready_slots: 1,
                    recent_failed_jobs: 0,
                    last_ready_at_ms: Some(ready_at_ms),
                    last_failed_at_ms: None,
                },
            );
        }
    }

    fn mark_job_provider_exhausted(&self, input_name: Arc<str>, failed_at_ms: u64) {
        if let Some(mut group) = self.jobs.get_mut(&input_name) {
            group.running_jobs = group.running_jobs.saturating_sub(1);
            group.recent_failed_jobs = group.recent_failed_jobs.saturating_add(1);
            group.last_failed_at_ms = Some(failed_at_ms);
        } else {
            self.jobs.insert(
                input_name,
                HlsProvisioningJobGroup {
                    last_started_at_ms: failed_at_ms,
                    running_jobs: 0,
                    ready_slots: 0,
                    recent_failed_jobs: 1,
                    last_ready_at_ms: None,
                    last_failed_at_ms: Some(failed_at_ms),
                },
            );
        }
    }

    pub(in crate::api) fn touch_consumer(&self, input_name: Arc<str>, virtual_id: u32, now_ms: u64) {
        self.prune(now_ms);
        let key = HlsProvisioningConsumerKey::new(input_name, virtual_id);
        if let Some(mut consumer) = self.consumers.get_mut(&key) {
            consumer.last_seen_at_ms = now_ms;
        } else {
            self.consumers.insert(key, HlsProvisioningConsumer { created_at_ms: now_ms, last_seen_at_ms: now_ms });
        }
    }

    fn job_status(&self, input_name: &Arc<str>, now_ms: u64) -> Option<HlsProvisioningStatus> {
        self.prune(now_ms);
        let group = self.jobs.get(input_name)?;
        if group.ready_slots > 0 {
            return Some(HlsProvisioningStatus::Ready);
        }
        if group.running_jobs > 0 {
            return Some(HlsProvisioningStatus::InProgress);
        }
        if group.recent_failed_jobs > 0 {
            return Some(HlsProvisioningStatus::ProviderExhausted);
        }
        None
    }

    pub(in crate::api) fn consumer_status(
        &self,
        input_name: &Arc<str>,
        virtual_id: u32,
        now_ms: u64,
    ) -> Option<HlsProvisioningStatus> {
        self.prune(now_ms);
        let key = HlsProvisioningConsumerKey::new(Arc::clone(input_name), virtual_id);
        self.consumers.get(&key)?;
        self.job_status(input_name, now_ms)
    }

    pub(in crate::api) fn has_consumer(&self, input_name: &Arc<str>, virtual_id: u32, now_ms: u64) -> bool {
        self.prune(now_ms);
        let key = HlsProvisioningConsumerKey::new(Arc::clone(input_name), virtual_id);
        self.consumers.contains_key(&key)
    }

    pub(in crate::api) fn clear_consumer(&self, input_name: &Arc<str>, virtual_id: u32) {
        self.consumers.remove(&HlsProvisioningConsumerKey::new(Arc::clone(input_name), virtual_id));
    }

    pub(in crate::api) fn mark_handoff_once(
        &self,
        input_name: &Arc<str>,
        virtual_id: u32,
        proxy_session_id: Option<&ProxySessionId>,
        access_lease_id: Option<&HlsAccessLeaseId>,
        now_ms: u64,
    ) -> bool {
        self.prune(now_ms);
        let key = HlsProvisioningHandoffKey::new(Arc::clone(input_name), virtual_id, proxy_session_id, access_lease_id);
        if self.handoffs.contains_key(&key) {
            return false;
        }
        self.handoffs.insert(key, now_ms);
        true
    }

    pub(in crate::api) fn take_ready_slot_for_consumer(
        &self,
        input_name: &Arc<str>,
        virtual_id: u32,
        now_ms: u64,
    ) -> bool {
        self.prune(now_ms);
        let key = HlsProvisioningConsumerKey::new(Arc::clone(input_name), virtual_id);
        if !self.consumers.contains_key(&key) {
            return false;
        }
        let Some(mut group) = self.jobs.get_mut(input_name) else {
            return false;
        };
        if group.ready_slots == 0 {
            return false;
        }
        group.ready_slots -= 1;
        drop(group);
        self.consumers.remove(&key);
        true
    }

    fn prune(&self, now_ms: u64) {
        self.jobs.retain(|_, group| {
            if let Some(ready_at_ms) = group.last_ready_at_ms {
                if ready_at_ms.saturating_add(HLS_PROVISIONING_COMPLETED_REDIRECT_WINDOW_MS) < now_ms {
                    group.ready_slots = 0;
                    group.last_ready_at_ms = None;
                }
            }
            if let Some(failed_at_ms) = group.last_failed_at_ms {
                if failed_at_ms.saturating_add(HLS_PROVISIONING_STALE_MARKER_MS) < now_ms {
                    group.recent_failed_jobs = 0;
                    group.last_failed_at_ms = None;
                }
            }
            let latest_reference_ms = [Some(group.last_started_at_ms), group.last_ready_at_ms, group.last_failed_at_ms]
                .into_iter()
                .flatten()
                .max()
                .unwrap_or_default();
            group.running_jobs > 0
                || group.ready_slots > 0
                || group.recent_failed_jobs > 0
                || latest_reference_ms.saturating_add(HLS_PROVISIONING_STALE_MARKER_MS) >= now_ms
        });
        self.consumers.retain(|_, consumer| {
            consumer.last_seen_at_ms.max(consumer.created_at_ms).saturating_add(HLS_PROVISIONING_STALE_MARKER_MS)
                >= now_ms
        });
        self.handoffs.retain(|_, marked_at_ms| {
            marked_at_ms.saturating_add(HLS_PROVISIONING_COMPLETED_REDIRECT_WINDOW_MS) >= now_ms
        });
    }
}

use tuliprox_core::utils::current_time_millis;

fn hls_response(hls_content: String) -> axum::response::Response {
    match axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
        .header(header::CACHE_CONTROL, "no-store, no-cache, must-revalidate")
        .body(Body::from(hls_content))
    {
        Ok(response) => response,
        Err(err) => {
            error!("Failed to build HLS response: {}", sanitize_sensitive_info(err.to_string().as_str()));
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub(crate) fn hls_custom_video_type_configured(app_state: &Arc<AppState>, video_type: CustomVideoStreamType) -> bool {
    if !is_custom_video_stream_enabled(&app_state.app_config) {
        return false;
    }
    let custom_stream_response = app_state.app_config.custom_stream_response.load();
    custom_stream_response.as_ref().and_then(|response| custom_video_asset(response, video_type)).is_some()
}

fn custom_video_asset(
    response: &CustomStreamResponse,
    video_type: CustomVideoStreamType,
) -> Option<&TransportStreamBuffer> {
    match video_type {
        CustomVideoStreamType::ChannelUnavailable => response.channel_unavailable.as_ref(),
        CustomVideoStreamType::UserConnectionsExhausted => response.user_connections_exhausted.as_ref(),
        CustomVideoStreamType::ProviderConnectionsExhausted => response.provider_connections_exhausted.as_ref(),
        CustomVideoStreamType::LowPriorityPreempted => response.low_priority_preempted.as_ref(),
        CustomVideoStreamType::UserAccountExpired => response.user_account_expired.as_ref(),
        CustomVideoStreamType::Provisioning => response.panel_api_provisioning.as_ref(),
        CustomVideoStreamType::HlsSessionOrLeaseExpired => response.hls_session_or_lease_expired.as_ref(),
    }
}

pub(crate) fn hls_panel_provisioning_segment_route_name(index: usize) -> String {
    format!("provisioning_{index:03}.ts")
}

pub(crate) fn parse_hls_panel_provisioning_segment_route_name(stream_type: &str) -> Option<usize> {
    let raw = stream_type.strip_suffix(".ts").unwrap_or(stream_type);
    let index = raw.strip_prefix("provisioning_")?.parse::<usize>().ok()?;
    (index < CUSTOM_VIDEO_HLS_PROVISIONING_SEGMENT_COUNT).then_some(index)
}

fn hls_panel_provisioning_segment_url(base_url: &str, user: &ProxyUserCredentials, index: usize) -> String {
    format!(
        "{}/{CUSTOM_VIDEO_PREFIX}/{HLS_CUSTOM_VIDEO_ROUTE_KIND}/{}/{}/{}",
        base_url.trim_end_matches('/'),
        user.username,
        user.password,
        hls_panel_provisioning_segment_route_name(index)
    )
}

pub(crate) fn hls_panel_provisioning_manifest_path(user: &ProxyUserCredentials, virtual_id: u32) -> String {
    format!(
        "/{CUSTOM_VIDEO_PREFIX}/{HLS_CUSTOM_VIDEO_ROUTE_KIND}/{}/{}/provisioning.m3u8?id={virtual_id}",
        user.username, user.password
    )
}

pub(crate) fn hls_provisioning_discontinuity_sequence(_now_ms: u64) -> u64 {
    0
}

fn build_hls_panel_provisioning_manifest_body(mut segment_url: impl FnMut(usize) -> String) -> String {
    let media_sequence = 0;
    let mut playlist = format!(
        "#EXTM3U\n\
         #EXT-X-VERSION:3\n\
         #EXT-X-TARGETDURATION:{PROVISIONING_HLS_TARGET_DURATION_SECS}\n\
         #EXT-X-MEDIA-SEQUENCE:{media_sequence}\n\
         #EXT-X-INDEPENDENT-SEGMENTS\n"
    );
    for index in 0..CUSTOM_VIDEO_HLS_PROVISIONING_SEGMENT_COUNT {
        let video_url = segment_url(index);
        playlist.push_str("#EXTINF:");
        playlist.push_str(PROVISIONING_HLS_EXTINF);
        playlist.push_str(",\n");
        playlist.push_str(&video_url);
        playlist.push('\n');
    }
    playlist
}

#[cfg(test)]
pub(crate) fn build_hls_custom_video_manifest_body(
    base_url: &str,
    user: &ProxyUserCredentials,
    video_type: CustomVideoStreamType,
) -> Option<String> {
    if video_type != CustomVideoStreamType::Provisioning {
        return None;
    }

    Some(build_hls_panel_provisioning_manifest_body(|index| hls_panel_provisioning_segment_url(base_url, user, index)))
}

pub(crate) async fn hls_custom_video_manifest_response_with_virtual_id(
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    video_type: CustomVideoStreamType,
    fallback_status: StatusCode,
    _virtual_id: Option<u32>,
) -> axum::response::Response {
    hls_custom_video_manifest_response_with_access(app_state, user, video_type, fallback_status, None).await
}

pub(crate) async fn hls_custom_video_manifest_response_for_access_lease(
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    video_type: CustomVideoStreamType,
    fallback_status: StatusCode,
    lease: &HlsAccessLease,
) -> axum::response::Response {
    let access = HlsStandaloneCustomAccess::for_shared_lease(
        lease.lease_id.clone(),
        lease.proxy_session_id.clone(),
        lease.username.clone(),
        lease.issued_at_ms,
        lease.valid_until_ms,
    );
    hls_custom_video_manifest_response_with_access(app_state, user, video_type, fallback_status, Some(access)).await
}

async fn hls_custom_video_manifest_response_with_access(
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    video_type: CustomVideoStreamType,
    fallback_status: StatusCode,
    access: Option<HlsStandaloneCustomAccess>,
) -> axum::response::Response {
    if !hls_custom_video_type_configured(app_state, video_type) {
        return fallback_status.into_response();
    }
    let Some(server_info) = app_state.app_config.get_user_server_info(user) else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };

    let base_url = server_info.get_base_url();
    let manifest = if video_type == CustomVideoStreamType::Provisioning {
        build_hls_panel_provisioning_manifest_body(|index| hls_panel_provisioning_segment_url(&base_url, user, index))
    } else {
        let Some(reason) = HlsRuntimeCustomTailReason::from_video_type(video_type) else {
            return fallback_status.into_response();
        };
        let access = access.unwrap_or_else(|| HlsStandaloneCustomAccess::for_user(user.username.clone()));
        let Ok(plan) =
            build_hls_standalone_custom_plan(&app_state.hls_ctx(), &base_url, access, reason, current_time_millis())
                .await
        else {
            return fallback_status.into_response();
        };
        plan.manifest_body.to_string()
    };
    hls_response(manifest)
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct HlsPanelProvisioningRedirectPaths<'a> {
    pub waiting_manifest_path: Option<&'a str>,
}

pub(crate) async fn try_hls_panel_provisioning_manifest_response(
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    input: &ConfigInput,
    virtual_id: u32,
    redirect_paths: HlsPanelProvisioningRedirectPaths<'_>,
    server_path: Option<&str>,
    fallback_status: StatusCode,
) -> Option<axum::response::Response> {
    let now_ms = current_time_millis();
    app_state.hls_provisioning.touch_consumer(Arc::clone(&input.name), virtual_id, now_ms);
    let provisioning_enabled = can_provision_on_exhausted(app_state.as_ref(), input);
    if provisioning_enabled {
        start_hls_panel_provisioning_once(app_state, input);
    }
    let status = if let Some(status) = app_state.hls_provisioning.consumer_status(&input.name, virtual_id, now_ms) {
        status
    } else if provisioning_enabled {
        HlsProvisioningStatus::InProgress
    } else {
        return None;
    };

    match status {
        HlsProvisioningStatus::InProgress | HlsProvisioningStatus::Ready => {
            // A ready provisioning job only means that credentials were persisted/probed.
            // Stream handoff must happen in a route that can reserve a runtime origin account first.
            if !hls_custom_video_type_configured(app_state, CustomVideoStreamType::Provisioning) {
                return Some(fallback_status.into_response());
            }
            let server_path = server_path
                .map(str::to_string)
                .or_else(|| app_state.app_config.get_user_server_info(user).and_then(|server| server.path));
            let manifest_path = redirect_paths
                .waiting_manifest_path
                .map_or_else(|| hls_panel_provisioning_manifest_path(user, virtual_id), str::to_string);
            Some(hls_virtual_entry_redirect_response(&manifest_path, server_path.as_deref()))
        }
        HlsProvisioningStatus::ProviderExhausted => Some(
            hls_custom_video_manifest_response_with_virtual_id(
                app_state,
                user,
                CustomVideoStreamType::ProviderConnectionsExhausted,
                fallback_status,
                Some(virtual_id),
            )
            .await,
        ),
    }
}

pub(crate) fn start_hls_panel_provisioning_once(app_state: &Arc<AppState>, input: &ConfigInput) -> bool {
    if !can_provision_on_exhausted(app_state.as_ref(), input) {
        return false;
    }
    let key = Arc::clone(&input.name);
    let now_ms = current_time_millis();
    let jobs_to_start = app_state.hls_provisioning.start_jobs_for_waiting_consumers(&key, now_ms);
    if jobs_to_start == 0 {
        return false;
    }
    for job_index in 0..jobs_to_start {
        let app_state = Arc::clone(app_state);
        let key = Arc::clone(&key);
        tokio::spawn(async move {
            debug!(
                "HLS panel provisioning started: input={} job_index={}",
                sanitize_sensitive_info(key.as_ref()),
                job_index
            );
            let outcome = try_provision_account_on_exhausted(&app_state, &key).await;
            let ready = outcome.is_some();
            let finished_at_ms = current_time_millis();
            if ready {
                app_state.hls_provisioning.mark_job_ready(Arc::clone(&key), finished_at_ms);
            } else {
                app_state.hls_provisioning.mark_job_provider_exhausted(Arc::clone(&key), finished_at_ms);
            }
            debug!(
                "HLS panel provisioning completed: input={} outcome={} ready={}",
                sanitize_sensitive_info(key.as_ref()),
                outcome.as_ref().map_or("unchanged", |outcome| outcome.kind_label()),
                ready
            );
        });
    }
    true
}

fn prefixed_hls_entry_path(original_hls_entry_path: &str, server_path: Option<&str>) -> String {
    let path = if original_hls_entry_path.starts_with('/') {
        original_hls_entry_path.to_string()
    } else {
        format!("/{original_hls_entry_path}")
    };
    let Some(server_path) = server_path.map(str::trim).filter(|path| !path.is_empty() && *path != "/") else {
        return path;
    };
    format!("/{}/{}", server_path.trim_matches('/'), path.trim_start_matches('/'))
}

pub(crate) fn hls_virtual_entry_redirect_response(
    original_hls_entry_path: &str,
    server_path: Option<&str>,
) -> axum::response::Response {
    match axum::response::Response::builder()
        .status(StatusCode::TEMPORARY_REDIRECT)
        .header(header::LOCATION, prefixed_hls_entry_path(original_hls_entry_path, server_path))
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::empty())
    {
        Ok(response) => response,
        Err(err) => {
            error!("Failed to build HLS provisioning redirect: {}", sanitize_sensitive_info(err.to_string().as_str()));
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input_name() -> Arc<str> {
        Arc::<str>::from("cdn-test")
    }

    #[test]
    fn hls_provisioning_state_tracks_in_progress_ready_and_provider_exhausted() {
        let state = HlsProvisioningState::new();
        let input = input_name();

        state.touch_consumer(Arc::clone(&input), 57, 900);
        assert_eq!(state.start_jobs_for_waiting_consumers(&input, 1_000), 1);
        assert_eq!(state.start_jobs_for_waiting_consumers(&input, 1_100), 0);
        assert_eq!(state.job_status(&input, 1_200), Some(HlsProvisioningStatus::InProgress));

        state.mark_job_ready(Arc::clone(&input), 2_000);
        assert_eq!(state.job_status(&input, 2_100), Some(HlsProvisioningStatus::Ready));
        assert!(state.take_ready_slot_for_consumer(&input, 57, 2_200));

        state.touch_consumer(Arc::clone(&input), 59, 2_900);
        assert_eq!(state.start_jobs_for_waiting_consumers(&input, 2_950), 1);
        state.mark_job_provider_exhausted(Arc::clone(&input), 3_000);
        assert_eq!(state.job_status(&input, 3_100), Some(HlsProvisioningStatus::ProviderExhausted));
    }

    #[test]
    fn hls_provisioning_ready_expires_after_redirect_window() {
        let state = HlsProvisioningState::new();
        let input = input_name();
        let ready_at_ms = 10_000;

        state.mark_job_ready(Arc::clone(&input), ready_at_ms);
        assert_eq!(
            state.job_status(&input, ready_at_ms + HLS_PROVISIONING_COMPLETED_REDIRECT_WINDOW_MS),
            Some(HlsProvisioningStatus::Ready)
        );
        assert_eq!(state.job_status(&input, ready_at_ms + HLS_PROVISIONING_COMPLETED_REDIRECT_WINDOW_MS + 1), None);
    }

    #[test]
    fn hls_provisioning_handoff_marker_is_idempotent_per_shared_session_lease() {
        let state = HlsProvisioningState::new();
        let input = input_name();
        let proxy_session_id = ProxySessionId("shared-session".to_string());
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());

        assert!(state.mark_handoff_once(&input, 57, Some(&proxy_session_id), Some(&access_lease_id), 1_000));
        assert!(!state.mark_handoff_once(&input, 57, Some(&proxy_session_id), Some(&access_lease_id), 1_100));

        let next_access_lease_id = HlsAccessLeaseId("next-access-lease".to_string());
        assert!(state.mark_handoff_once(&input, 57, Some(&proxy_session_id), Some(&next_access_lease_id), 1_200));
    }

    #[test]
    fn hls_provisioning_handoff_marker_expires_after_redirect_window() {
        let state = HlsProvisioningState::new();
        let input = input_name();
        let proxy_session_id = ProxySessionId("shared-session".to_string());
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let marked_at_ms = 10_000;

        assert!(state.mark_handoff_once(&input, 57, Some(&proxy_session_id), Some(&access_lease_id), marked_at_ms));
        assert!(!state.mark_handoff_once(
            &input,
            57,
            Some(&proxy_session_id),
            Some(&access_lease_id),
            marked_at_ms + HLS_PROVISIONING_COMPLETED_REDIRECT_WINDOW_MS
        ));
        assert!(state.mark_handoff_once(
            &input,
            57,
            Some(&proxy_session_id),
            Some(&access_lease_id),
            marked_at_ms + HLS_PROVISIONING_COMPLETED_REDIRECT_WINDOW_MS + 1
        ));
    }

    #[test]
    fn hls_provisioning_segment_route_names_are_bounded() {
        assert_eq!(hls_panel_provisioning_segment_route_name(0), "provisioning_000.ts");
        assert_eq!(hls_panel_provisioning_segment_route_name(5), "provisioning_005.ts");
        assert_eq!(parse_hls_panel_provisioning_segment_route_name("provisioning_000.ts"), Some(0));
        assert_eq!(parse_hls_panel_provisioning_segment_route_name("provisioning_005.ts"), Some(5));
        assert_eq!(parse_hls_panel_provisioning_segment_route_name("provisioning_006.ts"), None);
        assert_eq!(parse_hls_panel_provisioning_segment_route_name("provisioning.ts"), None);
    }

    #[test]
    fn hls_provisioning_consumers_are_tracked_per_virtual_id() {
        let state = HlsProvisioningState::new();
        let input = input_name();

        state.touch_consumer(Arc::clone(&input), 57, 1_100);
        assert_eq!(state.start_jobs_for_waiting_consumers(&input, 1_150), 1);

        assert_eq!(state.consumer_status(&input, 57, 1_200), Some(HlsProvisioningStatus::InProgress));
        assert_eq!(state.consumer_status(&input, 59, 1_200), None);

        state.touch_consumer(Arc::clone(&input), 59, 1_300);
        state.mark_job_ready(Arc::clone(&input), 2_000);
        assert_eq!(state.consumer_status(&input, 57, 2_100), Some(HlsProvisioningStatus::Ready));
        assert_eq!(state.consumer_status(&input, 59, 2_100), Some(HlsProvisioningStatus::Ready));

        assert!(state.take_ready_slot_for_consumer(&input, 59, 2_150));
        assert_eq!(state.consumer_status(&input, 59, 2_200), None);
        assert_eq!(state.consumer_status(&input, 57, 2_200), None);
        assert_eq!(state.job_status(&input, 2_200), None);
    }

    #[test]
    fn hls_provisioning_stale_provider_exhausted_job_can_restart() {
        let state = HlsProvisioningState::new();
        let input = input_name();

        state.touch_consumer(Arc::clone(&input), 57, 900);
        assert_eq!(state.start_jobs_for_waiting_consumers(&input, 1_000), 1);
        state.mark_job_provider_exhausted(Arc::clone(&input), 2_000);
        assert_eq!(state.job_status(&input, 2_100), Some(HlsProvisioningStatus::ProviderExhausted));

        let after_stale = 2_000 + HLS_PROVISIONING_STALE_MARKER_MS + 1;
        assert_eq!(state.job_status(&input, after_stale), None);
        state.touch_consumer(Arc::clone(&input), 57, after_stale + 1);
        assert_eq!(state.start_jobs_for_waiting_consumers(&input, after_stale + 2), 1);
    }

    #[test]
    fn hls_provisioning_starts_one_job_for_each_two_waiting_consumers() {
        let state = HlsProvisioningState::new();
        let input = input_name();

        state.touch_consumer(Arc::clone(&input), 57, 1_000);
        assert_eq!(state.start_jobs_for_waiting_consumers(&input, 1_010), 1);

        state.touch_consumer(Arc::clone(&input), 59, 1_100);
        assert_eq!(state.start_jobs_for_waiting_consumers(&input, 1_110), 0);

        state.touch_consumer(Arc::clone(&input), 61, 1_200);
        assert_eq!(state.start_jobs_for_waiting_consumers(&input, 1_210), 1);

        state.touch_consumer(Arc::clone(&input), 63, 1_300);
        assert_eq!(state.start_jobs_for_waiting_consumers(&input, 1_310), 0);
    }

    #[test]
    fn hls_provisioning_recent_failures_count_against_backpressure_target() {
        let state = HlsProvisioningState::new();
        let input = input_name();

        state.touch_consumer(Arc::clone(&input), 57, 1_000);
        assert_eq!(state.start_jobs_for_waiting_consumers(&input, 1_010), 1);
        state.mark_job_provider_exhausted(Arc::clone(&input), 2_000);
        assert_eq!(state.start_jobs_for_waiting_consumers(&input, 2_010), 0);

        state.touch_consumer(Arc::clone(&input), 59, 2_100);
        assert_eq!(state.start_jobs_for_waiting_consumers(&input, 2_110), 0);

        state.touch_consumer(Arc::clone(&input), 61, 2_200);
        assert_eq!(state.start_jobs_for_waiting_consumers(&input, 2_210), 1);
    }
}
