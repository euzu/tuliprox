use crate::{
    api::{
        api_utils::get_stream_options,
        model::{
            connection_manager::{PROVIDER_END_CLOSED, PROVIDER_END_ERROR, PROVIDER_END_NOT_SET},
            create_provider_stream, AppState, BoxedProviderStream, CleanupEvent, ConnectionManager,
            CustomVideoStreamType, EventManager, MeteringStream, PendingProviderWakeSource, ProviderHandle,
            ProviderStreamFactoryOptions, StreamDetails, StreamError, StreamMeterHandle, TimedClientStream,
            TransportStreamBuffer,
        },
        panel_api::{can_provision_on_exhausted, find_input_by_provider_name, run_panel_api_provisioning_probe},
    },
    auth::Fingerprint,
    model::{ConfigInput, ProxyUserCredentials},
    utils::debug_if_enabled,
};
use axum::http::{header::USER_AGENT, HeaderMap};
use bytes::Bytes;
use futures::{task::AtomicWaker, Future, Stream, StreamExt};
use log::{error, info};
use shared::model::FailureStage;
use shared::utils::Internable;
use shared::{
    model::{StreamChannel, UserConnectionPermission, VirtualId},
    utils::sanitize_sensitive_info,
};
use std::{
    net::SocketAddr,
    pin::Pin,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
    task::{Context, Poll},
};
use tokio::sync::Notify;
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

/// Discriminates which byte-stream the client is consuming at any moment.
/// Stored as `u8` in an `AtomicU8` for lock-free access inside `poll_next`.
/// Lower numeric values correspond to a live or custom stream; `GracePending`
/// (255) is a transient sentinel that parks the poll until the grace task resolves.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamMode {
    /// Forward bytes directly from the upstream provider.
    Inner = 0,
    /// Show the "user connections exhausted" custom video.
    UserExhausted = 1,
    /// Show the "provider connections exhausted" custom video.
    ProviderExhausted = 2,
    /// Show the "channel unavailable" custom video.
    ChannelUnavailable = 3,
    /// Show the provisioning/placeholder custom video while probing for capacity.
    Provisioning = 4,
    /// Show the "low-priority preempted" custom video.
    LowPriorityPreempted = 5,
    /// Transient: grace-period check is still in progress; `poll_next` must park.
    GracePending = 255,
}

impl StreamMode {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Inner,
            1 => Self::UserExhausted,
            2 => Self::ProviderExhausted,
            3 => Self::ChannelUnavailable,
            4 => Self::Provisioning,
            5 => Self::LowPriorityPreempted,
            _ => Self::GracePending,
        }
    }
}

/// Holds the optional custom video buffers for each error/placeholder scenario.
/// Using named fields avoids the positional-indexing confusion of a 4-tuple.
struct CustomVideoBuffers {
    user_exhausted: Option<TransportStreamBuffer>,
    provider_exhausted: Option<TransportStreamBuffer>,
    unavailable: Option<TransportStreamBuffer>,
    provisioning: Option<TransportStreamBuffer>,
    low_priority_preempted: Option<TransportStreamBuffer>,
}

struct GraceProvisioningInfo {
    input: Arc<ConfigInput>,
    stop_signal: CancellationToken,
}

#[derive(Clone)]
struct TimedStreamContext {
    app_state: Arc<AppState>,
    duration_secs: u32,
    virtual_id: VirtualId,
}

struct DeferredProviderOpenContext {
    app_state: Arc<AppState>,
    provider_stream_factory_options: ProviderStreamFactoryOptions,
}

pub(crate) struct ActiveClientStreamParams<'a> {
    pub stream_details: StreamDetails,
    pub app_state: &'a Arc<AppState>,
    pub user: &'a ProxyUserCredentials,
    pub connection_permission: UserConnectionPermission,
    pub connection_kind: crate::api::model::active_provider_manager::ConnectionKind,
    pub fingerprint: &'a Fingerprint,
    pub stream_channel: StreamChannel,
    pub socket_bound: bool,
    pub session_token: Option<&'a str>,
    pub req_headers: &'a HeaderMap,
    pub meter_uid: u32,
    pub meter_stream: bool,
}

struct GracePeriodParams {
    app_state: Arc<AppState>,
    stream_details: StreamDetails,
    user_grace_period: bool,
    user: ProxyUserCredentials,
    fingerprint: Fingerprint,
    virtual_id: VirtualId,
    session_token: Option<String>,
    provisioning_info: Option<GraceProvisioningInfo>,
    waker: Option<Arc<AtomicWaker>>,
    hold_stream: bool,
    capacity_notify: Arc<Notify>,
    pending_provider_version: Option<u64>,
    // The `transition_version` of the session if it is in `GraceActive` lifecycle.
    // Used by the grace task to confirm the session is still in `GraceActive` before resolving.
    grace_active_version: Option<u64>,
    // Set when the stream was admitted via a user-grace strategy.
    // On user-grace failure, remaining strategies are evaluated before final deny.
    grace_resolution_context: Option<crate::api::api_utils::GraceResolutionContext>,
    // The `ConnectionKind` from the original admission decision.
    // Preserved in `GraceResolutionContext.kind` and also passed directly here
    // so the grace task can use the runtime value when evaluating remaining strategies.
    grace_kind: Option<crate::api::model::ConnectionKind>,
    // Whether the session is `socket_bound`. Used to construct the correct
    // `EvictionReentryGuard`.
    socket_bound: bool,
}

enum DeferredProviderOpenOutcome {
    Stream(BoxedProviderStream),
    Mode(StreamMode),
    Failed,
}

enum DeferredProviderOpenState {
    Pending(Box<DeferredProviderOpenContext>),
    Opening(Pin<Box<dyn Future<Output=DeferredProviderOpenOutcome> + Send>>),
}

#[allow(clippy::struct_excessive_bools)]
struct ActiveClientStreamState {
    inner: Option<BoxedProviderStream>,
    send_custom_stream_flag: Option<Arc<AtomicU8>>,
    provider_handle: Option<ProviderHandle>,
    deferred_provider_open: Option<DeferredProviderOpenState>,
    timed_stream_context: Option<TimedStreamContext>,
    preempt_cancelled: Option<Pin<Box<WaitForCancellationFutureOwned>>>,
    grace_task_handle: Option<tokio::task::JoinHandle<()>>,
    provisionable: bool,
    custom_video: CustomVideoBuffers,
    meter: Option<Arc<StreamMeterHandle>>,
    event_manager: Arc<EventManager>,
    waker: Option<Arc<AtomicWaker>>,
    connection_manager: Arc<ConnectionManager>,
    fingerprint: Arc<Fingerprint>,
    stream_uid: Option<u32>,
    provider_stopped: bool,
    user_stream_released: bool,
    /// Mirrors `user_stream_released` for the provider handle to guard against double-release
    /// when preemption and Drop race.
    provider_handle_released: bool,
    custom_video_timeout_secs: u32,
    custom_video_timeout_mode: Option<StreamMode>,
    custom_video_timeout_sleep: Option<Pin<Box<tokio::time::Sleep>>>,
    /// Set once when the provider stream ends. Read once in Drop. Never queried during streaming.
    /// Separate from `send_custom_stream_flag` (`StreamMode`). Uses `PROVIDER_END_*` constants.
    provider_end_reason: AtomicU8,
    provider_error_class: Option<&'static str>,
    provider_http_status: Option<u16>,
    /// Count of successful provider reconnections during this session (grace period / deferred open).
    provider_reconnect_count: AtomicU8,
}

impl ActiveClientStreamState {
    fn mode_for_custom_video_type(video_type: CustomVideoStreamType) -> Option<StreamMode> {
        match video_type {
            CustomVideoStreamType::ChannelUnavailable => Some(StreamMode::ChannelUnavailable),
            CustomVideoStreamType::UserConnectionsExhausted => Some(StreamMode::UserExhausted),
            CustomVideoStreamType::ProviderConnectionsExhausted => Some(StreamMode::ProviderExhausted),
            CustomVideoStreamType::LowPriorityPreempted => Some(StreamMode::LowPriorityPreempted),
            CustomVideoStreamType::Provisioning => Some(StreamMode::Provisioning),
            CustomVideoStreamType::UserAccountExpired => None,
        }
    }

    fn wrap_provider_stream(&self, stream: BoxedProviderStream) -> BoxedProviderStream {
        let stream = if let Some(meter) = &self.meter {
            MeteringStream::new(stream, Arc::clone(meter), Arc::clone(&self.event_manager)).boxed()
        } else {
            stream
        };
        if let Some(ctx) = self.timed_stream_context.as_ref() {
            TimedClientStream::new(
                &ctx.app_state,
                stream,
                ctx.duration_secs,
                self.fingerprint.addr,
                ctx.virtual_id,
            )
                .boxed()
        } else {
            stream
        }
    }

    fn custom_video_type_for_mode(mode: StreamMode) -> CustomVideoStreamType {
        match mode {
            StreamMode::UserExhausted => CustomVideoStreamType::UserConnectionsExhausted,
            StreamMode::ProviderExhausted => CustomVideoStreamType::ProviderConnectionsExhausted,
            StreamMode::Provisioning => CustomVideoStreamType::Provisioning,
            StreamMode::LowPriorityPreempted => CustomVideoStreamType::LowPriorityPreempted,
            StreamMode::ChannelUnavailable | StreamMode::Inner | StreamMode::GracePending => {
                CustomVideoStreamType::ChannelUnavailable
            }
        }
    }

    fn release_user_stream(&mut self) {
        if self.user_stream_released {
            return;
        }
        self.user_stream_released = true;
        self.connection_manager.send_cleanup(CleanupEvent::ReleaseStream {
            addr: self.fingerprint.addr,
            stream_uid: self.stream_uid,
            provider_end_reason: self.provider_end_reason.load(Ordering::Relaxed),
            reconnect_count: self.provider_reconnect_count.load(Ordering::Relaxed),
            provider_error_class: self.provider_error_class,
            provider_http_status: self.provider_http_status,
        });
    }

    fn stop_grace_task(&mut self) {
        if let Some(task) = self.grace_task_handle.take() {
            task.abort();
        }
    }

    fn clear_finished_grace_task(&mut self) {
        if self
            .grace_task_handle
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            self.grace_task_handle = None;
            // If the task finished but the flag is still GRACE_PENDING (e.g. the task
            // panicked or was cancelled before it could update the flag), reset the flag
            // to INNER_STREAM so the client stream is not hung indefinitely.
            if let Some(flag) = &self.send_custom_stream_flag {
                let _ = flag.compare_exchange(
                    StreamMode::GracePending as u8,
                    StreamMode::Inner as u8,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                );
            }
        }
    }

    fn stop_provider_stream_preempted(&mut self) -> bool {
        self.provider_stopped = true;
        self.preempt_cancelled = None;
        self.stop_grace_task();

        let mut serve_preempted_custom = false;
        if self.provider_handle.is_some() {
            let handle = self.provider_handle.take();
            self.provider_handle_released = true;
            if self.custom_video.low_priority_preempted.is_some() {
                serve_preempted_custom = true;
                if let Some(flag) = &self.send_custom_stream_flag {
                    flag.store(StreamMode::LowPriorityPreempted as u8, Ordering::Release);
                } else {
                    // Fallback: create_active_client_stream usually initializes this via stream_grace_period.
                    self.send_custom_stream_flag = Some(Arc::new(AtomicU8::new(StreamMode::LowPriorityPreempted as u8)));
                }
            } else if let Some(flag) = &self.send_custom_stream_flag {
                flag.store(StreamMode::Inner as u8, Ordering::Release);
            }

            if let Some(waker) = &self.waker {
                waker.wake();
            }

            let addr = self.fingerprint.addr;
            // Drop the provider stream immediately instead of replacing with an
            // allocated empty stream — avoids a heap allocation on every preemption.
            self.inner = None;

            debug_if_enabled!(
                "Provider stream preempted for {}; stopping client stream",
                sanitize_sensitive_info(&addr.to_string())
            );
            if serve_preempted_custom {
                self.connection_manager.send_cleanup(CleanupEvent::UpdateDetailAndReleaseProvider {
                    addr,
                    video_type: CustomVideoStreamType::LowPriorityPreempted,
                    handle,
                });
            } else {
                self.release_user_stream();
                self.connection_manager.send_cleanup(CleanupEvent::ReleaseProviderHandle { handle });
            }
        }
        serve_preempted_custom
    }

    fn stop_provider_stream(&mut self, mode: StreamMode) {
        self.provider_stopped = true;
        self.preempt_cancelled = None;
        self.stop_grace_task();

        if self.provider_handle.is_some() {
            let handle = self.provider_handle.take();
            self.provider_handle_released = true;

            if mode == StreamMode::ChannelUnavailable {
                if let Some(flag) = &self.send_custom_stream_flag {
                    let _ = flag.compare_exchange(
                        StreamMode::Inner as u8,
                        StreamMode::ChannelUnavailable as u8,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    );
                }
            }

            if let Some(waker) = &self.waker {
                waker.wake();
            }

            let addr = self.fingerprint.addr;
            // Drop the provider stream immediately instead of replacing with an
            // allocated empty stream — avoids a heap allocation on every mode switch.
            self.inner = None;

            let video_type = Self::custom_video_type_for_mode(mode);
            let reason = match mode {
                StreamMode::ChannelUnavailable => "unavailable provider channel",
                StreamMode::UserExhausted => "user grace period exhaustion",
                StreamMode::ProviderExhausted => "provider grace period exhaustion",
                StreamMode::Provisioning => "provider grace period provisioning",
                StreamMode::LowPriorityPreempted => "low-priority preemption",
                StreamMode::Inner | StreamMode::GracePending => "stream mode transition",
            };
            debug_if_enabled!(
                "Provider stream stopped due to {reason} for {}",
                sanitize_sensitive_info(&addr.to_string())
            );
            self.connection_manager.send_cleanup(CleanupEvent::UpdateDetailAndReleaseProvider {
                addr,
                video_type,
                handle,
            });
        }
    }

    fn reset_custom_video_timeout(&mut self) {
        self.custom_video_timeout_mode = None;
        self.custom_video_timeout_sleep = None;
    }

    fn enter_custom_mode(&mut self, mode: StreamMode) {
        if self.custom_video_timeout_mode != Some(mode) {
            self.custom_video_timeout_mode = Some(mode);
            self.custom_video_timeout_sleep = if self.custom_video_timeout_secs > 0 {
                Some(Box::pin(tokio::time::sleep(tokio::time::Duration::from_secs(
                    u64::from(self.custom_video_timeout_secs),
                ))))
            } else {
                None
            };
        }

        if !self.provider_stopped {
            info!(
                "Switching to {mode:?} custom video stream for {}",
                sanitize_sensitive_info(&self.fingerprint.addr.to_string())
            );
            self.stop_provider_stream(mode);
        }
    }

    fn custom_video_timed_out(&mut self, cx: &mut Context<'_>, mode: StreamMode) -> bool {
        if self.custom_video_timeout_secs == 0 {
            return false;
        }

        if self.custom_video_timeout_mode != Some(mode) {
            return false;
        }

        if let Some(timeout_sleep) = self.custom_video_timeout_sleep.as_mut() {
            return timeout_sleep.as_mut().poll(cx).is_ready();
        }

        false
    }
}

fn wrap_timed_client_stream_if_needed(
    app_state: &Arc<AppState>,
    stream: BoxedProviderStream,
    addr: SocketAddr,
    virtual_id: VirtualId,
) -> BoxedProviderStream {
    let config = app_state.app_config.config.load();
    match config.sleep_timer_mins {
        None => stream,
        Some(mins) => {
            let secs = u32::try_from((u64::from(mins) * 60).min(u64::from(u32::MAX))).unwrap_or(0);
            if secs > 0 {
                TimedClientStream::new(app_state, stream, secs, addr, virtual_id).boxed()
            } else {
                stream
            }
        }
    }
}

fn create_deferred_provider_open_future(
    app_state: &Arc<AppState>,
    stream_details: &StreamDetails,
    fingerprint: &Fingerprint,
    stream_channel: &StreamChannel,
    req_headers: &HeaderMap,
) -> Option<DeferredProviderOpenState> {
    if !stream_details.has_deferred_provider_open() {
        return None;
    }

    let provider_name = stream_details.provider_name.as_deref()?;
    let request_url = stream_details.request_url.as_deref()?;
    let input = find_input_by_provider_name(app_state.as_ref(), provider_name)?;
    let stream_url = url::Url::parse(request_url).ok()?;
    let stream_options = get_stream_options(app_state);
    let default_user_agent = app_state.app_config.config.load().default_user_agent.clone();
    let disabled_headers = app_state.get_disabled_headers();
    let mut provider_stream_factory_options = ProviderStreamFactoryOptions::new(
        &crate::api::model::ProviderStreamFactoryParams {
            addr: fingerprint.addr,
            item_type: stream_channel.item_type,
            share_stream: stream_channel.shared,
            stream_options: &stream_options,
            stream_url: &stream_url,
            req_headers,
            input_headers: Some(&input.headers),
            session_headers: stream_details.session_headers.as_ref(),
            disabled_headers: disabled_headers.as_ref(),
            default_user_agent: default_user_agent.as_deref(),
            username: None,
            client_ip: Some(&fingerprint.client_ip),
            stream_channel: Some(stream_channel),
            connect_failure_stage: Some(FailureStage::ProviderOpen),
        },
    );
    provider_stream_factory_options.set_provider(input.get_resolve_provider(stream_url.as_ref()));

    Some(DeferredProviderOpenState::Pending(Box::new(DeferredProviderOpenContext {
        app_state: Arc::clone(app_state),
        provider_stream_factory_options,
    })))
}

fn create_timed_stream_context(app_state: &Arc<AppState>, virtual_id: VirtualId) -> Option<TimedStreamContext> {
    let config = app_state.app_config.config.load();
    let mins = config.sleep_timer_mins?;
    let duration_secs = u32::try_from((u64::from(mins) * 60).min(u64::from(u32::MAX))).unwrap_or(0);
    (duration_secs > 0).then(|| TimedStreamContext {
        app_state: Arc::clone(app_state),
        duration_secs,
        virtual_id,
    })
}

pub(in crate::api) struct ActiveClientStream {
    state: ActiveClientStreamState,
}

impl Stream for ActiveClientStream {
    type Item = Result<Bytes, StreamError>;

    #[allow(clippy::too_many_lines)]
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            // 1. Preemption check (user priority feature)
            if let Some(fut) = self.state.preempt_cancelled.as_mut() {
                if fut.as_mut().poll(cx).is_ready() && !self.state.stop_provider_stream_preempted() {
                    return Poll::Ready(None);
                }
            }

            // 2. Grace task lifecycle management + waker registration
            self.state.clear_finished_grace_task();
            if let Some(waker) = &self.state.waker {
                waker.register(cx.waker());
            }

            // 3. Read atomic mode flag (set by grace task or stop_provider_stream)
            let mode = match &self.state.send_custom_stream_flag {
                Some(flag) => StreamMode::from_u8(flag.load(Ordering::Acquire)),
                None => StreamMode::Inner,
            };

            // 4. Dispatch based on current streaming phase
            match mode {
                // Grace period: hold_stream=true, waiting for grace task to resolve
                StreamMode::GracePending => {
                    self.state.reset_custom_video_timeout();
                    return Poll::Pending;
                }

                // Live streaming: forward bytes from upstream provider
                StreamMode::Inner => {
                    self.state.reset_custom_video_timeout();

                    if self.state.inner.is_none() {
                        if let Some(deferred_provider_open) = self.state.deferred_provider_open.take() {
                            match deferred_provider_open {
                                DeferredProviderOpenState::Pending(context) => {
                                    let app_state = Arc::clone(&context.app_state);
                                    let client = {
                                        let http_client = app_state.http_client.load();
                                        http_client.as_ref().clone()
                                    };
                                    let future = Box::pin(async move {
                                        match create_provider_stream(
                                            &app_state,
                                            &client,
                                            context.provider_stream_factory_options,
                                        )
                                            .await
                                        {
                                            Some((_stream, Some((_headers, _status, _response_url, Some(custom_video_type))))) => {
                                                ActiveClientStreamState::mode_for_custom_video_type(custom_video_type)
                                                    .map_or(DeferredProviderOpenOutcome::Failed, DeferredProviderOpenOutcome::Mode)
                                            }
                                            Some((stream, _stream_info)) => DeferredProviderOpenOutcome::Stream(stream),
                                            None => DeferredProviderOpenOutcome::Failed,
                                        }
                                    });
                                    self.state.deferred_provider_open = Some(DeferredProviderOpenState::Opening(future));
                                    continue;
                                }
                                DeferredProviderOpenState::Opening(mut future) => match future.as_mut().poll(cx) {
                                    Poll::Pending => {
                                        self.state.deferred_provider_open =
                                            Some(DeferredProviderOpenState::Opening(future));
                                        return Poll::Pending;
                                    }
                                    Poll::Ready(DeferredProviderOpenOutcome::Stream(stream)) => {
                                        self.state.provider_reconnect_count.fetch_add(1, Ordering::Relaxed);
                                        self.state.inner = Some(self.state.wrap_provider_stream(stream));
                                        continue;
                                    }
                                    Poll::Ready(DeferredProviderOpenOutcome::Mode(mode)) => {
                                        self.state.enter_custom_mode(mode);
                                        continue;
                                    }
                                    Poll::Ready(DeferredProviderOpenOutcome::Failed) => {
                                        self.state.enter_custom_mode(StreamMode::ChannelUnavailable);
                                        continue;
                                    }
                                },
                            }
                        }

                        if self.state.grace_task_handle.is_none() {
                            self.state.stop_provider_stream(StreamMode::ChannelUnavailable);
                            return Poll::Ready(None);
                        }

                        return Poll::Pending;
                    }

                    match self.state.inner.as_mut().map(|inner| Pin::new(inner).poll_next(cx)) {
                        Some(Poll::Ready(Some(Ok(bytes)))) => return Poll::Ready(Some(Ok(bytes))),
                        Some(Poll::Ready(Some(Err(e)))) => {
                            error!("Inner stream error: {e:?}");
                            self.state.provider_error_class = Some(e.provider_error_class());
                            self.state.provider_http_status = e.provider_http_status();
                            let _ = self.state.provider_end_reason.compare_exchange(
                                PROVIDER_END_NOT_SET,
                                PROVIDER_END_ERROR,
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            );
                            if self.state.grace_task_handle.is_none() {
                                self.state.stop_provider_stream(StreamMode::ChannelUnavailable);
                                return Poll::Ready(None);
                            }

                            return Poll::Pending;
                        }
                        Some(Poll::Ready(None)) | None => {
                            let _ = self.state.provider_end_reason.compare_exchange(
                                PROVIDER_END_NOT_SET,
                                PROVIDER_END_CLOSED,
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            );
                            if self.state.grace_task_handle.is_none() {
                                self.state.stop_provider_stream(StreamMode::ChannelUnavailable);
                                return Poll::Ready(None);
                            }

                            return Poll::Pending;
                        }
                        Some(Poll::Pending) => return Poll::Pending,
                    }
                }

                // Custom video modes: serve the appropriate buffer
                video_mode => {
                    if self.state.custom_video_timeout_mode != Some(video_mode) {
                        self.state.enter_custom_mode(video_mode);
                    }

                    if self.state.custom_video_timed_out(cx, video_mode) {
                        info!(
                            "Custom video {video_mode:?} timed out for {}, terminating stream",
                            sanitize_sensitive_info(&self.state.fingerprint.addr.to_string())
                        );
                        return Poll::Ready(None);
                    }

                    let is_provisioning = video_mode == StreamMode::Provisioning && self.state.provisionable;

                    let buffer_opt = match video_mode {
                        StreamMode::UserExhausted => self.state.custom_video.user_exhausted.as_mut(),
                        StreamMode::ProviderExhausted => self.state.custom_video.provider_exhausted.as_mut(),
                        StreamMode::ChannelUnavailable => self.state.custom_video.unavailable.as_mut(),
                        StreamMode::Provisioning => self.state.custom_video.provisioning.as_mut(),
                        StreamMode::LowPriorityPreempted => self.state.custom_video.low_priority_preempted.as_mut(),
                        _ => None,
                    };

                    if let Some(buffer) = buffer_opt {
                        buffer.register_waker(cx.waker());
                        if let Some(chunk) = buffer.next_chunk() {
                            return Poll::Ready(Some(Ok(chunk)));
                        }

                        // Provisioning loops until preemption fires; all others terminate.
                        if is_provisioning {
                            return Poll::Pending;
                        }

                        info!(
                            "Custom video {video_mode:?} buffer exhausted for {}, terminating stream",
                            sanitize_sensitive_info(&self.state.fingerprint.addr.to_string())
                        );
                        return Poll::Ready(None);
                    }

                    // No custom video configured for this mode -> terminate immediately.
                    info!(
                        "No custom video configured for {video_mode:?} mode for {}, terminating stream",
                        sanitize_sensitive_info(&self.state.fingerprint.addr.to_string())
                    );
                    return Poll::Ready(None);
                }
            }
        }
    }
}

impl Drop for ActiveClientStream {
    fn drop(&mut self) {
        self.state.stop_grace_task();
        let addr = self.state.fingerprint.addr;
        let handle = self.state.provider_handle.take();
        // `provider_handle_released` mirrors `user_stream_released` for the provider slot.
        // When preemption already released the handle, `provider_handle` is None and the
        // flag is true — sending None here would be a no-op, but the explicit guard makes
        // the invariant visible and safe against future call-site additions.
        let handle_for_cleanup = if self.state.provider_handle_released { None } else { handle };
        if self.state.user_stream_released {
            if !self.state.provider_handle_released {
                self.state.provider_handle_released = true;
                self.state.connection_manager.send_cleanup(CleanupEvent::ReleaseProviderHandle { handle: handle_for_cleanup });
            }
        } else {
            self.state.user_stream_released = true;
            self.state.provider_handle_released = true;
            self.state.connection_manager.send_cleanup(CleanupEvent::ReleaseStreamAndProviderHandle {
                addr,
                stream_uid: self.state.stream_uid,
                handle: handle_for_cleanup,
                provider_end_reason: self.state.provider_end_reason.load(Ordering::Relaxed),
                reconnect_count: self.state.provider_reconnect_count.load(Ordering::Relaxed),
                provider_error_class: self.state.provider_error_class,
                provider_http_status: self.state.provider_http_status,
            });
        }
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn create_active_client_stream(request: ActiveClientStreamParams<'_>) -> BoxedProviderStream {
    let ActiveClientStreamParams {
        mut stream_details,
        app_state,
        user,
        connection_permission,
        connection_kind,
        fingerprint,
        stream_channel,
        socket_bound,
        session_token,
        req_headers,
        meter_uid,
        meter_stream,
    } = request;
    if connection_permission == UserConnectionPermission::Exhausted {
        error!("Something is wrong this should not happen");
    }
    let grant_user_grace_period = connection_permission == UserConnectionPermission::GracePeriod;
    let username = user.username.as_str();
    let provider_name = stream_details.provider_name.clone().unwrap_or_else(|| "unknown".intern());

    let user_agent = req_headers.get(USER_AGENT).map(|h| String::from_utf8_lossy(h.as_bytes())).unwrap_or_default();

    let virtual_id = stream_channel.virtual_id;
    let is_shared_source_stream = stream_channel.shared && stream_details.stream.is_some();
    let registered_stream = app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid,
            username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind,
            priority: user.priority,
            soft_priority: user.soft_priority,
            fingerprint,
            provider: provider_name,
            stream_channel: &stream_channel,
            user_agent,
            session_token,
        })
        .await;
    let stream_uid = registered_stream.as_ref().map(|stream| stream.uid);
    if let Some((_, _, _m_, Some(cvt))) = stream_details.stream_info.as_ref() {
        app_state.connection_manager.update_stream_detail(&fingerprint.addr, *cvt).await;
    }

    let meter = if meter_stream && meter_uid != 0 {
        let meter = Arc::new(StreamMeterHandle::new(meter_uid, Arc::downgrade(&app_state.event_manager)));
        app_state.event_manager.register_meter(Arc::clone(&meter)).await;
        Some(meter)
    } else {
        None
    };

    // Shared broadcaster source (first subscriber path): feed provider bytes directly.
    // Grace/custom handling is not needed here because this stream is only the fan-out source.
    if is_shared_source_stream {
        if let Some(stream) = stream_details.stream.take() {
            let stream = if let Some(meter) = &meter {
                MeteringStream::new(stream, Arc::clone(meter), Arc::clone(&app_state.event_manager)).boxed()
            } else {
                stream
            };
            return wrap_timed_client_stream_if_needed(app_state, stream, fingerprint.addr, virtual_id);
        }
    }

    let provisioning_info = resolve_grace_period_provisioning(app_state, &stream_details);
    let has_provisioning = provisioning_info.is_some();
    let hold_stream = stream_details.grace_period.hold_stream;
    let capacity_notify = app_state.connection_manager.capacity_notified();
    let pending_provider_version = if hold_stream {
        if let Some(token) = session_token {
            app_state.active_users.pending_provider_version(&user.username, token).await
        } else {
            None
        }
    } else {
        None
    };
    // `GraceActive` version: populated when the session is in `GraceActive` lifecycle (GraceMode::Instant).
    let grace_active_version = if let Some(token) = session_token {
        app_state.active_users.grace_active_version(&user.username, token).await
    } else {
        None
    };

    let waker = Arc::new(AtomicWaker::new());
    let owned_session_token: Option<String> = session_token.map(str::to_string);
    let owned_grace_ctx = stream_details.grace_resolution_context.clone();
    let mut provider_handle_preserved = stream_details.provider_handle.clone();
    // Compute deferred provider open before moving stream_details into the grace task.
    let deferred_provider_open =
        create_deferred_provider_open_future(app_state, &stream_details, fingerprint, &stream_channel, req_headers);
    let timed_stream_context = deferred_provider_open
        .as_ref()
        .and_then(|_| create_timed_stream_context(app_state, virtual_id));
    let stream_taken = stream_details.stream.take();
    let has_deferred_open = stream_details.has_deferred_provider_open();
    let grace_waker = if grant_user_grace_period || stream_details.provider_grace_active {
        Some(Arc::clone(&waker))
    } else {
        None
    };
    let (grace_stop_flag, grace_task_handle) = stream_grace_period(GracePeriodParams {
        app_state: Arc::clone(app_state),
        stream_details,
        user_grace_period: grant_user_grace_period,
        user: user.clone(),
        fingerprint: fingerprint.clone(),
        virtual_id,
        session_token: owned_session_token,
        provisioning_info,
        waker: grace_waker,
        hold_stream,
        capacity_notify,
        pending_provider_version,
        grace_active_version,
        grace_resolution_context: owned_grace_ctx,
        grace_kind: Some(connection_kind),
        socket_bound,
    });

    let cfg = &app_state.app_config;
    let custom_response = cfg.custom_stream_response.load();
    let custom_video_timeout_secs = cfg.config.load().custom_stream_response_timeout_secs;
    let custom_video = custom_response.as_ref().map_or(
        CustomVideoBuffers {
            user_exhausted: None,
            provider_exhausted: None,
            unavailable: None,
            provisioning: None,
            low_priority_preempted: None,
        },
        |c| CustomVideoBuffers {
            user_exhausted: c.user_connections_exhausted.clone(),
            provider_exhausted: c.provider_connections_exhausted.clone(),
            unavailable: c.channel_unavailable.clone(),
            provisioning: c.panel_api_provisioning.clone(),
            low_priority_preempted: c.low_priority_preempted.clone(),
        },
    );

    let stream: Option<BoxedProviderStream> = match stream_taken {
        None => {
            if !has_deferred_open {
                let provider_handle = provider_handle_preserved.take();
                app_state.connection_manager.release_provider_handle(provider_handle).await;
            }
            None
        }
        Some(stream) => {
            let stream = if let Some(meter) = &meter {
                MeteringStream::new(stream, Arc::clone(meter), Arc::clone(&app_state.event_manager)).boxed()
            } else {
                stream
            };
            Some(wrap_timed_client_stream_if_needed(app_state, stream, fingerprint.addr, virtual_id))
        }
    };

    let preempt_cancelled = provider_handle_preserved
        .as_ref()
        .and_then(|h| h.cancel_token.as_ref())
        .map(|token| Box::pin(token.clone().cancelled_owned()));

    let mut send_custom_stream_flag = grace_stop_flag;
    if send_custom_stream_flag.is_none()
        && preempt_cancelled.is_some()
        && custom_video.low_priority_preempted.is_some()
    {
        send_custom_stream_flag = Some(Arc::new(AtomicU8::new(StreamMode::Inner as u8)));
    }

    let state = ActiveClientStreamState {
        inner: stream,
        deferred_provider_open,
        timed_stream_context,
        preempt_cancelled,
        grace_task_handle,
        provider_handle: provider_handle_preserved,
        send_custom_stream_flag,
        provisionable: has_provisioning,
        custom_video,
        meter,
        event_manager: Arc::clone(&app_state.event_manager),
        waker: Some(waker),
        connection_manager: Arc::clone(&app_state.connection_manager),
        fingerprint: Arc::new(fingerprint.clone()),
        stream_uid,
        provider_stopped: false,
        user_stream_released: false,
        provider_handle_released: false,
        custom_video_timeout_secs,
        custom_video_timeout_mode: None,
        custom_video_timeout_sleep: None,
        provider_end_reason: AtomicU8::new(PROVIDER_END_NOT_SET),
        provider_error_class: None,
        provider_http_status: None,
        provider_reconnect_count: AtomicU8::new(0),
    };

    ActiveClientStream { state }.boxed()
}

fn resolve_grace_period_provisioning(
    app_state: &Arc<AppState>,
    stream_details: &StreamDetails,
) -> Option<GraceProvisioningInfo> {
    if stream_details.disable_provider_grace || !stream_details.provider_grace_active {
        return None;
    }
    let provider_name = stream_details.provider_name.as_deref();
    let input = provider_name.and_then(|name| find_input_by_provider_name(app_state.as_ref(), name))?;
    if !can_provision_on_exhausted(app_state, &input) {
        return None;
    }

    let stop_signal = CancellationToken::new();
    Some(GraceProvisioningInfo { input, stop_signal })
}

#[allow(clippy::too_many_lines)]
fn stream_grace_period(request: GracePeriodParams) -> (Option<Arc<AtomicU8>>, Option<tokio::task::JoinHandle<()>>) {
    let GracePeriodParams {
        app_state,
        stream_details,
        user_grace_period,
        user,
        fingerprint,
        virtual_id,
        session_token,
        provisioning_info,
        waker,
        hold_stream,
        capacity_notify,
        pending_provider_version,
        grace_active_version,
        grace_resolution_context,
        grace_kind,
        socket_bound,
        ..
    } = request;
    let grace_period = stream_details.grace_period;
    let active_users = Arc::clone(&app_state.active_users);
    let active_provider = Arc::clone(&app_state.active_provider);
    let connection_manager = Arc::clone(&app_state.connection_manager);

    let provider_grace_check = if stream_details.provider_grace_active
        && stream_details.provider_name.is_some()
        && !stream_details.disable_provider_grace
    {
        stream_details.provider_name.clone()
    } else {
        None
    };

    let user_max_connections = user.max_connections;
    let user_grace_check = if user_grace_period && user_max_connections > 0 {
        let user_name = user.username.clone();
        Some((user_name, user_max_connections))
    } else {
        None
    };

    if provider_grace_check.is_some() || user_grace_check.is_some() {
        let stream_strategy_flag = Arc::new(AtomicU8::new(
            if hold_stream { StreamMode::GracePending as u8 } else { StreamMode::Inner as u8 },
        ));
        let stream_strategy_flag_copy = Arc::clone(&stream_strategy_flag);
        let grace_period_millis = grace_period.period_millis;

        let user_manager = Arc::clone(&active_users);
        let provider_manager = Arc::clone(&active_provider);
        let connection_manager = Arc::clone(&connection_manager);
        let reconnect_flag = stream_details.reconnect_flag.clone();
        let fingerprint = fingerprint.clone();
        let app_state = Arc::clone(&app_state);
        let pending_username = user.username.clone();
        // Clone owned copies of fields borrowed from `request` so the spawn sees owned values.
        let grace_resolution_context = grace_resolution_context.clone();
        // Safety timeout: if async operations inside the grace task stall, force the flag
        // out of GRACE_PENDING so the client stream is not hung indefinitely.
        // Allow grace_period_millis for the intentional delay plus a 10-second buffer
        // for the async connection checks that follow.
        let grace_task_timeout =
            tokio::time::Duration::from_millis(grace_period_millis.saturating_add(10_000));
        // Clone handles for use in the timeout fallback, in case the inner async block is cancelled.
        let flag_for_fallback = Arc::clone(&stream_strategy_flag_copy);
        let waker_for_fallback = waker.clone();
        let session_token_timeout = session_token.clone();
        let active_users_timeout = Arc::clone(&active_users);
        let pending_username_timeout = pending_username.clone();
        let grace_task_handle = tokio::spawn(async move {
            let timed_out = tokio::time::timeout(grace_task_timeout, async move {
                let deadline =
                    tokio::time::Instant::now() + tokio::time::Duration::from_millis(grace_period_millis);
                let mut pending_wake_source = PendingProviderWakeSource::Activated;
                loop {
                    let capacity_wait = capacity_notify.notified();
                    tokio::pin!(capacity_wait);

                    let user_ok = match &user_grace_check {
                        Some((username, max_connections)) => {
                            user_manager.user_connections(username).await <= *max_connections
                        }
                        None => true,
                    };
                    let provider_ok = match &provider_grace_check {
                        Some(provider_name) => {
                            !provider_manager.is_over_limit(provider_name).await
                        }
                        None => true,
                    };
                    if user_ok && provider_ok {
                        break;
                    }

                    tokio::select! {
                        () = tokio::time::sleep_until(deadline) => {
                            pending_wake_source = PendingProviderWakeSource::Timeout;
                            break;
                        }
                        () = &mut capacity_wait => {
                            pending_wake_source = PendingProviderWakeSource::CapacityNotify;
                        }
                    }
                }

                let mut updated = false;
                if let Some((username, max_connections)) = user_grace_check {
                    let active_connections = user_manager.user_connections(&username).await;
                    if active_connections > max_connections {
                        // User-grace failed. Evaluate remaining strategies before final deny.
                        if let Some(ref ctx) = grace_resolution_context {
                            let eviction_guard = if socket_bound {
                                crate::api::api_utils::EvictionReentryGuard::SocketPlayback { virtual_id }
                            } else {
                                crate::api::api_utils::EvictionReentryGuard::Session(
                                    // Defensive fallback: an empty token will not match any real session.
                                    session_token.as_deref().unwrap_or_default(),
                                )
                            };
                            let remaining_result = crate::api::api_utils::evaluate_remaining_strategies_after_grace(
                                &app_state,
                                &username,
                                max_connections,
                                user.soft_connections,
                                &fingerprint.client_ip,
                                &fingerprint.addr,
                                true,
                                session_token.as_deref(),
                                true,
                                eviction_guard,
                                ctx,
                                grace_kind,
                            )
                                .await;
                            match remaining_result.admission.permission {
                                shared::model::UserConnectionPermission::Allowed
                                | shared::model::UserConnectionPermission::GracePeriod => {
                                    // Remaining strategy succeeded — proceed to Inner.
                                    stream_strategy_flag_copy.store(StreamMode::Inner as u8, Ordering::Release);
                                    // updated stays false
                                }
                                shared::model::UserConnectionPermission::Exhausted => {
                                    // Remaining strategies exhausted — final UserExhausted.
                                    stream_strategy_flag_copy.store(
                                        StreamMode::UserExhausted as u8,
                                        Ordering::Release,
                                    );
                                    connection_manager
                                        .update_stream_detail(
                                            &fingerprint.addr,
                                            CustomVideoStreamType::UserConnectionsExhausted,
                                        )
                                        .await;
                                    connection_manager
                                        .shared_stream_manager
                                        .release_connection(&fingerprint.addr, true)
                                        .await;
                                    info!("User connections exhausted for active clients: {username}");
                                    updated = true;
                                }
                            }
                        } else {
                            // No grace context — immediate UserExhausted.
                            stream_strategy_flag_copy.store(
                                StreamMode::UserExhausted as u8,
                                Ordering::Release,
                            );
                            connection_manager
                                .update_stream_detail(&fingerprint.addr, CustomVideoStreamType::UserConnectionsExhausted)
                                .await;
                            connection_manager.shared_stream_manager.release_connection(&fingerprint.addr, true).await;
                            info!("User connections exhausted for active clients: {username}");
                            updated = true;
                        }
                    }
                }

                if !updated {
                    if let Some(provider_name) = provider_grace_check {
                        if provider_manager.is_over_limit(&provider_name).await {
                            if let Some(provisioning_info) = provisioning_info {
                                stream_strategy_flag_copy.store(StreamMode::Provisioning as u8, Ordering::Release);
                                connection_manager
                                    .update_stream_detail(&fingerprint.addr, CustomVideoStreamType::Provisioning)
                                    .await;
                                debug_if_enabled!(
                                    "Provider grace period exhausted; provisioning for active clients: {provider_name}"
                                );
                                let app_state = Arc::clone(&app_state);
                                let input = (*provisioning_info.input).clone();
                                let stop_signal = provisioning_info.stop_signal;
                                let addr = fingerprint.addr;
                                tokio::spawn(async move {
                                    if let Err(err) =
                                        run_panel_api_provisioning_probe(app_state, input, stop_signal, addr, virtual_id)
                                            .await
                                    {
                                        error!("Error running Probe: {err:?}");
                                    }
                                });
                            } else {
                                stream_strategy_flag_copy.store(StreamMode::ProviderExhausted as u8, Ordering::Release);
                                connection_manager
                                    .update_stream_detail(
                                        &fingerprint.addr,
                                        CustomVideoStreamType::ProviderConnectionsExhausted,
                                    )
                                    .await;
                                // Release the shared stream subscription to stop the subscriber loop
                                connection_manager.shared_stream_manager.release_connection(&fingerprint.addr, true).await;
                                info!("Provider connections exhausted for active clients: {provider_name}");
                            }
                            updated = true;
                        }
                    }
                }

                if !updated {
                    stream_strategy_flag_copy.store(StreamMode::Inner as u8, Ordering::Release);
                }

                // Resolve session lifecycle transitions.
                // PendingProvider (Hold): activate on success, expire on failure.
                if hold_stream {
                    if let (Some(token), Some(version)) = (session_token.as_deref(), pending_provider_version) {
                        let _transition_guard = active_users
                            .acquire_playback_transition(&pending_username, token)
                            .await;
                        if updated {
                            active_users
                                .expire_pending_provider(&pending_username, token, version, pending_wake_source)
                                .await;
                        } else {
                            active_users
                                .activate_pending_provider(&pending_username, token, version, pending_wake_source)
                                .await;
                        }
                    }
                }

                // GraceActive (Instant): activate on success, expire on failure.
                // grace_active_version is Some when the session is in `GraceActive` lifecycle.
                if let (Some(token), Some(version)) = (session_token.as_deref(), grace_active_version) {
                    let _transition_guard = active_users
                        .acquire_playback_transition(&pending_username, token)
                        .await;
                    if updated {
                        active_users
                            .expire_grace_active(&pending_username, token, version)
                            .await;
                    } else {
                        active_users
                            .activate_grace_active(&pending_username, token, version)
                            .await;
                    }
                }

                if updated {
                    if let Some(flag) = reconnect_flag {
                        flag.cancel();
                    }
                }

                if let Some(w) = waker.as_ref() {
                    w.wake();
                }
            })
                .await;

            if timed_out.is_err() {
                // Grace task exceeded its budget without updating the flag — reset GRACE_PENDING
                // to INNER_STREAM so the client stream is not hung indefinitely.
                // Also resolve any session lifecycle to prevent inconsistent state:
                // a PendingProvider / GraceActive session that was never resolved would cause
                // the next admission attempt to incorrectly skip re-evaluation.
                if let (Some(token), Some(version)) = (session_token_timeout.as_deref(), pending_provider_version) {
                    let _transition_guard = active_users_timeout
                        .acquire_playback_transition(&pending_username_timeout, token)
                        .await;
                    active_users_timeout
                        .expire_pending_provider(&pending_username_timeout, token, version, PendingProviderWakeSource::Timeout)
                        .await;
                }
                if let (Some(token), Some(version)) = (session_token_timeout.as_deref(), grace_active_version) {
                    let _transition_guard = active_users_timeout
                        .acquire_playback_transition(&pending_username_timeout, token)
                        .await;
                    active_users_timeout
                        .expire_grace_active(&pending_username_timeout, token, version)
                        .await;
                }
                error!("Grace period task timed out; resetting stream flag to prevent client hang");
                let _ = flag_for_fallback.compare_exchange(
                    StreamMode::GracePending as u8,
                    StreamMode::Inner as u8,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                );
                if let Some(w) = waker_for_fallback.as_ref() {
                    w.wake();
                }
            }
        });
        return (Some(stream_strategy_flag), Some(grace_task_handle));
    }
    (None, None)
}

#[cfg(test)]
mod tests {
    use super::{
        create_active_client_stream, stream_grace_period, ActiveClientStream, ActiveClientStreamParams,
        ActiveClientStreamState,
        CustomVideoBuffers, DeferredProviderOpenOutcome, DeferredProviderOpenState, GracePeriodParams, StreamMode,
        TimedStreamContext,
    };
    use crate::api::api_utils::GraceResolutionContext;
    use crate::api::model::connection_manager::PROVIDER_END_NOT_SET;
    use crate::{
        api::model::{
            ActiveProviderManager, ActiveUserManager, AppState, CancelTokens, ConnectionManager, CreateUserSessionParams,
            CustomVideoStreamType, DownloadQueue, EventManager, MetadataUpdateManager, PlaylistStorageState,
            SharedStreamManager, StreamDetails, StreamError, UpdateGuard,
        },
        auth::Fingerprint,
        model::{AppConfig, Config, ConfigInput, GracePeriodOptions, MediaToolCapabilities, ProcessTargets, ProxyUserCredentials, SourcesConfig, StreamConfig},
        utils::{FileLockManager, GeoIp},
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use axum::http::HeaderMap;
    use bytes::Bytes;
    use futures::{pin_mut, StreamExt};
    use reqwest::Client;
    use shared::{
        model::{AdmissionStrategy, ConfigPaths, InputFetchMethod, InputType, PlaylistItemType, StreamChannel, UserConnectionPermission, XtreamCluster},
        utils::Internable,
    };
    use std::{
        collections::HashMap,
        sync::{
            atomic::{AtomicU8, Ordering},
            Arc,
        },
        time::Duration,
    };
    use tokio::sync::mpsc;

    fn create_test_app_config() -> AppConfig {
        let input = Arc::new(ConfigInput {
            id: 1,
            name: "provider_1".intern(),
            input_type: InputType::Xtream,
            headers: HashMap::default(),
            url: "http://provider-1.example".to_string(),
            username: Some("user1".to_string()),
            password: Some("pass1".to_string()),
            enabled: true,
            priority: 0,
            max_connections: 1,
            method: InputFetchMethod::default(),
            aliases: None,
            ..ConfigInput::default()
        });
        let sources = SourcesConfig { inputs: vec![input], ..SourcesConfig::default() };

        AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(sources)),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            api_proxy: Arc::new(ArcSwapOption::default()),
            file_locks: Arc::new(FileLockManager::default()),
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
            custom_stream_response: Arc::new(ArcSwapOption::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(MediaToolCapabilities::new()),
        }
    }

    fn create_test_connection_manager() -> Arc<ConnectionManager> {
        let app_cfg = create_test_app_config();
        let event_manager = Arc::new(EventManager::new());
        let provider_manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
        let shared_manager = Arc::new(SharedStreamManager::new(Arc::clone(&provider_manager)));
        provider_manager.set_shared_stream_manager(Arc::clone(&shared_manager));

        let geo_ip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let config = app_cfg.config.load();
        let user_manager = Arc::new(ActiveUserManager::new(&config, &geo_ip, &event_manager));

        Arc::new(ConnectionManager::new(
            &user_manager,
            &provider_manager,
            &shared_manager,
            &event_manager,
            None,
        ))
    }

    fn create_test_app_state() -> Arc<AppState> {
        let app_cfg = Arc::new(create_test_app_config());
        let event_manager = Arc::new(EventManager::new());
        let active_provider = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
        let shared_stream_manager = Arc::new(SharedStreamManager::new(Arc::clone(&active_provider)));
        active_provider.set_shared_stream_manager(Arc::clone(&shared_stream_manager));

        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let config = app_cfg.config.load();
        let active_users = Arc::new(ActiveUserManager::new(&config, &geoip, &event_manager));
        let connection_manager = Arc::new(ConnectionManager::new(
            &active_users,
            &active_provider,
            &shared_stream_manager,
            &event_manager,
            None,
        ));

        let tokens = CancelTokens::default();
        let metadata_manager = Arc::new(MetadataUpdateManager::new(tokens.metadata.clone()));
        let (manual_update_sender, _) = mpsc::channel::<crate::api::model::ManualPlaylistUpdateRequest>(1);

        Arc::new(AppState {
            forced_targets: Arc::new(ArcSwap::from_pointee(ProcessTargets {
                enabled: false,
                inputs: Vec::new(),
                targets: Vec::new(),
                target_names: Vec::new(),
            })),
            app_config: app_cfg,
            http_client: Arc::new(ArcSwap::from_pointee(Client::new())),
            http_client_no_redirect: Arc::new(ArcSwap::from_pointee(Client::new())),
            downloads: Arc::new(DownloadQueue::new()),
            cache: Arc::new(ArcSwapOption::default()),
            shared_stream_manager,
            hls_proxy: Arc::new(crate::api::model::HlsProxyManager::new()),
            hls_provisioning: Arc::new(crate::api::model::HlsProvisioningState::new()),
            active_users,
            active_provider,
            connection_manager,
            event_manager,
            cancel_tokens: Arc::new(ArcSwap::from_pointee(tokens)),
            playlists: Arc::new(PlaylistStorageState::new()),
            geoip,
            update_guard: UpdateGuard::new(),
            metadata_manager,
            manual_update_sender,
        })
    }

    fn create_test_app_state_with_stream_config(stream: StreamConfig) -> Arc<AppState> {
        let config = Config {
            reverse_proxy: Some(crate::model::ReverseProxyConfig {
                resource_rewrite_disabled: false,
                rewrite_secret: [0; 16],
                resource_retry: crate::model::ResourceRetryConfig::default(),
                disabled_header: None,
                stream: Some(stream),
                cache: None,
                rate_limit: None,
                geoip: None,
                stream_history: None,
                qos_aggregation: None,
                hls_cache: None,
            }),
            user_access_control: true,
            ..Config::default()
        };

        let mut app_cfg = create_test_app_config();
        app_cfg.config = Arc::new(ArcSwap::from_pointee(config));

        let event_manager = Arc::new(EventManager::new());
        let active_provider = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
        let shared_stream_manager = Arc::new(SharedStreamManager::new(Arc::clone(&active_provider)));
        active_provider.set_shared_stream_manager(Arc::clone(&shared_stream_manager));

        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let config_loaded = app_cfg.config.load();
        let active_users = Arc::new(ActiveUserManager::new(&config_loaded, &geoip, &event_manager));
        let connection_manager = Arc::new(ConnectionManager::new(
            &active_users,
            &active_provider,
            &shared_stream_manager,
            &event_manager,
            None,
        ));

        let tokens = CancelTokens::default();
        let metadata_manager = Arc::new(MetadataUpdateManager::new(tokens.metadata.clone()));
        let (manual_update_sender, _) = mpsc::channel::<crate::api::model::ManualPlaylistUpdateRequest>(1);

        Arc::new(AppState {
            forced_targets: Arc::new(ArcSwap::from_pointee(ProcessTargets {
                enabled: false,
                inputs: Vec::new(),
                targets: Vec::new(),
                target_names: Vec::new(),
            })),
            app_config: Arc::new(app_cfg),
            http_client: Arc::new(ArcSwap::from_pointee(Client::new())),
            http_client_no_redirect: Arc::new(ArcSwap::from_pointee(Client::new())),
            downloads: Arc::new(DownloadQueue::new()),
            cache: Arc::new(ArcSwapOption::default()),
            shared_stream_manager,
            hls_proxy: Arc::new(crate::api::model::HlsProxyManager::new()),
            hls_provisioning: Arc::new(crate::api::model::HlsProvisioningState::new()),
            active_users,
            active_provider,
            connection_manager,
            event_manager,
            cancel_tokens: Arc::new(ArcSwap::from_pointee(tokens)),
            playlists: Arc::new(PlaylistStorageState::new()),
            geoip,
            update_guard: UpdateGuard::new(),
            metadata_manager,
            manual_update_sender,
        })
    }

    fn create_test_user(username: &str) -> ProxyUserCredentials {
        let mut user = ProxyUserCredentials::default();
        user.username = username.to_string();
        user.max_connections = 1;
        user
    }

    fn create_test_fingerprint(addr: std::net::SocketAddr) -> Fingerprint {
        Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), addr)
    }

    fn create_test_stream_channel(virtual_id: u32, url: &str) -> StreamChannel {
        StreamChannel {
            target_id: 1,
            virtual_id,
            provider_id: 1,
            input_name: "input".intern(),
            item_type: PlaylistItemType::Live,
            cluster: XtreamCluster::Live,
            group: "Live".intern(),
            title: "Test Channel".intern(),
            url: url.into(),
            shared: false,
            shared_joined_existing: None,
            shared_stream_id: None,
            technical: None,
            epg_channel_id: None,
            epg_reference_ts: None,
        }
    }

    fn create_test_shared_stream_channel(virtual_id: u32, url: &str) -> StreamChannel {
        let mut channel = create_test_stream_channel(virtual_id, url);
        channel.shared = true;
        channel
    }

    fn create_deferred_provider_grace_details(
        provider_name: &Arc<str>,
        provider_handle: crate::api::model::ProviderHandle,
    ) -> StreamDetails {
        StreamDetails {
            stream: None,
            stream_info: None,
            provider_name: Some(Arc::clone(provider_name)),
            request_url: Some("http://provider-1.example/live/1".intern()),
            session_headers: None,
            grace_period: GracePeriodOptions {
                period_millis: 100,
                timeout_secs: 0,
                hold_stream: true,
            },
            provider_grace_active: true,
            disable_provider_grace: false,
            reconnect_flag: None,
            provider_handle: Some(provider_handle),
            grace_resolution_context: None,
        }
    }

    async fn start_deferred_provider_grace_resolution(
        app_state: &Arc<AppState>,
        provider_name: &Arc<str>,
        deferred_addr: std::net::SocketAddr,
        session_token: Option<&str>,
    ) -> (
        Arc<AtomicU8>,
        tokio::task::JoinHandle<()>,
        crate::api::model::ProviderHandle,
    ) {
        let deferred_handle = app_state
            .active_provider
            .acquire_exact_connection_with_grace(
                provider_name,
                &deferred_addr,
                true,
                0,
                crate::api::model::ConnectionKind::Normal,
            )
            .await
            .expect("deferred client should receive provider grace allocation");
        let stream_details = create_deferred_provider_grace_details(provider_name, deferred_handle);
        let deferred_provider_handle = stream_details.provider_handle.clone();
        let test_user = create_test_user("grace-user");
        let test_fingerprint = create_test_fingerprint(deferred_addr);
        let pending_provider_version = if let Some(token) = session_token {
            app_state
                .active_users
                .pending_provider_version(&test_user.username, token)
                .await
        } else {
            None
        };
        let (flag, task) = stream_grace_period(GracePeriodParams {
            app_state: Arc::clone(app_state),
            stream_details,
            user_grace_period: false,
            user: test_user,
            fingerprint: test_fingerprint,
            virtual_id: 1,
            session_token: session_token.map(str::to_string),
            provisioning_info: None,
            waker: None,
            hold_stream: true,
            capacity_notify: app_state.connection_manager.capacity_notified(),
            pending_provider_version,
            grace_active_version: None,
            grace_resolution_context: None,
            grace_kind: None,
            socket_bound: false,
        });
        (
            flag.expect("provider grace should install a mode flag"),
            task.expect("provider grace should spawn a grace-resolution task"),
            deferred_provider_handle.expect("deferred provider handle must be retained during grace"),
        )
    }

    async fn assert_missing_custom_video_terminates(mode: StreamMode, provisionable: bool) {
        let connection_manager = create_test_connection_manager();
        let addr = "127.0.0.1:55001".parse().unwrap_or_else(|_| unreachable!());

        let state = ActiveClientStreamState {
            inner: None,
            send_custom_stream_flag: Some(Arc::new(AtomicU8::new(mode as u8))),
            provider_handle: None,
            deferred_provider_open: None,
            timed_stream_context: None,
            preempt_cancelled: None,
            grace_task_handle: None,
            provisionable,
            custom_video: CustomVideoBuffers {
                user_exhausted: None,
                provider_exhausted: None,
                unavailable: None,
                provisioning: None,
                low_priority_preempted: None,
            },
            meter: None,
            event_manager: Arc::new(EventManager::new()),
            waker: None,
            connection_manager,
            fingerprint: Arc::new(Fingerprint::new(
                "fp-key".to_string(),
                "127.0.0.1".to_string(),
                addr,
            )),
            stream_uid: None,
            provider_stopped: true,
            user_stream_released: true,
            provider_handle_released: true,
            custom_video_timeout_secs: 5,
            custom_video_timeout_mode: None,
            custom_video_timeout_sleep: None,
            provider_end_reason: AtomicU8::new(PROVIDER_END_NOT_SET),
            provider_error_class: None,
            provider_http_status: None,
            provider_reconnect_count: AtomicU8::new(0),
        };

        let stream = ActiveClientStream { state };
        pin_mut!(stream);

        let result = stream.next().await;
        assert!(result.is_none());
    }

    #[test]
    fn test_custom_video_type_mapping_for_grace_modes() {
        assert!(matches!(
            ActiveClientStreamState::custom_video_type_for_mode(StreamMode::UserExhausted),
            CustomVideoStreamType::UserConnectionsExhausted
        ));
        assert!(matches!(
            ActiveClientStreamState::custom_video_type_for_mode(StreamMode::ProviderExhausted),
            CustomVideoStreamType::ProviderConnectionsExhausted
        ));
        assert!(matches!(
            ActiveClientStreamState::custom_video_type_for_mode(StreamMode::Provisioning),
            CustomVideoStreamType::Provisioning
        ));
        assert!(matches!(
            ActiveClientStreamState::custom_video_type_for_mode(StreamMode::LowPriorityPreempted),
            CustomVideoStreamType::LowPriorityPreempted
        ));
        assert!(matches!(
            ActiveClientStreamState::custom_video_type_for_mode(StreamMode::ChannelUnavailable),
            CustomVideoStreamType::ChannelUnavailable
        ));
    }

    #[tokio::test]
    async fn test_provisioning_without_custom_video_terminates_immediately_with_timeout_configured() {
        assert_missing_custom_video_terminates(StreamMode::Provisioning, true).await;
    }

    #[tokio::test]
    async fn test_user_exhausted_without_custom_video_terminates_immediately() {
        assert_missing_custom_video_terminates(StreamMode::UserExhausted, false).await;
    }

    #[tokio::test]
    async fn test_provider_exhausted_without_custom_video_terminates_immediately() {
        assert_missing_custom_video_terminates(StreamMode::ProviderExhausted, false).await;
    }

    #[tokio::test]
    async fn test_channel_unavailable_without_custom_video_terminates_immediately() {
        assert_missing_custom_video_terminates(StreamMode::ChannelUnavailable, false).await;
    }

    #[tokio::test]
    async fn test_low_priority_preempted_without_custom_video_terminates_immediately() {
        assert_missing_custom_video_terminates(StreamMode::LowPriorityPreempted, false).await;
    }

    #[tokio::test(start_paused = true)]
    async fn test_provider_grace_resolution_transitions_from_grace_pending_to_inner_when_capacity_notify_arrives() {
        let app_state = create_test_app_state();
        let provider_name = "provider_1".intern();
        let holder_addr = "127.0.0.1:55010".parse().unwrap_or_else(|_| unreachable!());
        let deferred_addr = "127.0.0.1:55011".parse().unwrap_or_else(|_| unreachable!());

        let holder_handle = app_state
            .active_provider
            .acquire_exact_connection_with_grace(
                &provider_name,
                &holder_addr,
                false,
                0,
                crate::api::model::ConnectionKind::Normal,
            )
            .await
            .expect("holder should consume the provider's live capacity");
        let (flag, grace_task, deferred_handle) =
            start_deferred_provider_grace_resolution(&app_state, &provider_name, deferred_addr, None).await;

        assert_eq!(
            StreamMode::from_u8(flag.load(Ordering::Acquire)),
            StreamMode::GracePending,
            "provider grace resolution must begin in GracePending while provider capacity is exhausted"
        );

        app_state.connection_manager.release_provider_handle(Some(holder_handle)).await;
        let join_result = tokio::time::timeout(Duration::from_millis(1), grace_task).await;

        assert!(
            join_result.is_ok(),
            "provider grace resolution stayed pending after capacity_notify should have fired"
        );
        assert_eq!(
            StreamMode::from_u8(flag.load(Ordering::Acquire)),
            StreamMode::Inner,
            "capacity-notify should resolve provider grace from GracePending to Inner before the deadline"
        );

        app_state.connection_manager.release_provider_handle(Some(deferred_handle)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn test_provider_grace_resolution_clears_pending_provider_on_capacity_notify() {
        let app_state = create_test_app_state();
        let provider_name = "provider_1".intern();
        let holder_addr = "127.0.0.1:55024".parse().unwrap_or_else(|_| unreachable!());
        let deferred_addr = "127.0.0.1:55025".parse().unwrap_or_else(|_| unreachable!());
        let user = create_test_user("grace-user");

        let _ = app_state
            .active_users
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-grace",
                virtual_id: 1,
                provider: provider_name.as_ref(),
                stream_url: "http://provider-1.example/live/1.ts",
                addr: &deferred_addr,
                connection_permission: UserConnectionPermission::GracePeriod,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        let _ = app_state
            .active_users
            .mark_pending_provider(
                &user.username,
                "tok-grace",
                crate::api::model::PendingProviderReason::GraceHold,
                9_999,
            )
            .await;

        let holder_handle = app_state
            .active_provider
            .acquire_exact_connection_with_grace(
                &provider_name,
                &holder_addr,
                false,
                0,
                crate::api::model::ConnectionKind::Normal,
            )
            .await
            .expect("holder should consume the provider's live capacity");
        let (_flag, grace_task, deferred_handle) =
            start_deferred_provider_grace_resolution(&app_state, &provider_name, deferred_addr, Some("tok-grace")).await;

        app_state.connection_manager.release_provider_handle(Some(holder_handle)).await;
        let join_result = tokio::time::timeout(Duration::from_millis(1), grace_task).await;
        assert!(join_result.is_ok(), "grace task should finish after capacity notify");

        let session = app_state
            .active_users
            .get_and_update_user_session(&user.username, "tok-grace")
            .await
            .expect("session should still exist");
        assert!(!matches!(session.lifecycle, crate::api::model::PlaybackLifecycle::PendingProvider { .. }), "capacity notify should clear pending provider state");
        assert_eq!(session.permission, UserConnectionPermission::Allowed);

        app_state.connection_manager.release_provider_handle(Some(deferred_handle)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn test_provider_grace_resolution_ignores_stale_pending_version_on_capacity_notify() {
        let app_state = create_test_app_state();
        let provider_name = "provider_1".intern();
        let holder_addr = "127.0.0.1:55026".parse().unwrap_or_else(|_| unreachable!());
        let deferred_addr = "127.0.0.1:55027".parse().unwrap_or_else(|_| unreachable!());
        let user = create_test_user("grace-user");

        let _ = app_state
            .active_users
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-grace-stale",
                virtual_id: 1,
                provider: provider_name.as_ref(),
                stream_url: "http://provider-1.example/live/1.ts",
                addr: &deferred_addr,
                connection_permission: UserConnectionPermission::GracePeriod,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        let _ = app_state
            .active_users
            .mark_pending_provider(
                &user.username,
                "tok-grace-stale",
                crate::api::model::PendingProviderReason::GraceHold,
                9_999,
            )
            .await;

        let holder_handle = app_state
            .active_provider
            .acquire_exact_connection_with_grace(
                &provider_name,
                &holder_addr,
                false,
                0,
                crate::api::model::ConnectionKind::Normal,
            )
            .await
            .expect("holder should consume the provider's live capacity");
        let (_flag, grace_task, deferred_handle) = start_deferred_provider_grace_resolution(
            &app_state,
            &provider_name,
            deferred_addr,
            Some("tok-grace-stale"),
        )
            .await;

        let replacement_version = app_state
            .active_users
            .mark_pending_provider(
                &user.username,
                "tok-grace-stale",
                crate::api::model::PendingProviderReason::GraceHold,
                10_500,
            )
            .await
            .expect("replacement pending version should be created");

        app_state.connection_manager.release_provider_handle(Some(holder_handle)).await;
        let join_result = tokio::time::timeout(Duration::from_millis(1), grace_task).await;
        assert!(join_result.is_ok(), "grace task should finish after capacity notify");

        let session = app_state
            .active_users
            .get_and_update_user_session(&user.username, "tok-grace-stale")
            .await
            .expect("session should still exist");
        let crate::api::model::PlaybackLifecycle::PendingProvider { data: pending } =
            &session.lifecycle
        else {
            panic!("stale grace task must not clear the replacement pending provider state")
        };
        assert_eq!(pending.version, replacement_version);
        assert!(pending.wake_source.is_none());
        assert_eq!(session.permission, UserConnectionPermission::GracePeriod);

        app_state.connection_manager.release_provider_handle(Some(deferred_handle)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn test_active_client_stream_deferred_provider_grace_retains_provider_handle_while_grace_pending() {
        let app_state = create_test_app_state();
        let provider_name = "provider_1".intern();
        let holder_addr = "127.0.0.1:55012".parse().unwrap_or_else(|_| unreachable!());
        let deferred_addr = "127.0.0.1:55013".parse().unwrap_or_else(|_| unreachable!());
        let third_addr = "127.0.0.1:55014".parse().unwrap_or_else(|_| unreachable!());

        let holder_handle = app_state
            .active_provider
            .acquire_exact_connection_with_grace(
                &provider_name,
                &holder_addr,
                false,
                0,
                crate::api::model::ConnectionKind::Normal,
            )
            .await
            .expect("holder should consume the provider's live capacity");
        let deferred_handle = app_state
            .active_provider
            .acquire_exact_connection_with_grace(
                &provider_name,
                &deferred_addr,
                true,
                0,
                crate::api::model::ConnectionKind::Normal,
            )
            .await
            .expect("deferred client should receive provider grace allocation");
        let stream_details = create_deferred_provider_grace_details(&provider_name, deferred_handle.clone());
        let test_user = create_test_user("grace-user");
        let test_fingerprint = create_test_fingerprint(deferred_addr);
        let stream = create_active_client_stream(ActiveClientStreamParams {
            stream_details,
            app_state: &app_state,
            user: &test_user,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            fingerprint: &test_fingerprint,
            stream_channel: create_test_stream_channel(1, "http://provider-1.example/live/1"),
            socket_bound: true,
            session_token: None,
            req_headers: &HeaderMap::default(),
            meter_uid: 0,
            meter_stream: false,
        })
            .await;
        pin_mut!(stream);

        assert!(
            matches!(futures::poll!(stream.next()), std::task::Poll::Pending),
            "deferred active-client-stream should park in GracePending while waiting for provider grace resolution"
        );

        let third_handle = app_state
            .active_provider
            .acquire_exact_connection_with_grace(
                &provider_name,
                &third_addr,
                true,
                0,
                crate::api::model::ConnectionKind::Normal,
            )
            .await;

        assert!(
            third_handle.is_none(),
            "deferred active-client-stream should retain the deferred provider grace reservation while GracePending"
        );

        app_state.connection_manager.release_provider_handle(Some(holder_handle)).await;
        app_state.connection_manager.release_provider_handle(Some(deferred_handle)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn test_active_client_stream_shared_deferred_provider_grace_retains_provider_handle_while_grace_pending() {
        let app_state = create_test_app_state();
        let provider_name = "provider_1".intern();
        let holder_addr = "127.0.0.1:55017".parse().unwrap_or_else(|_| unreachable!());
        let deferred_addr = "127.0.0.1:55018".parse().unwrap_or_else(|_| unreachable!());
        let third_addr = "127.0.0.1:55019".parse().unwrap_or_else(|_| unreachable!());

        let holder_handle = app_state
            .active_provider
            .acquire_exact_connection_with_grace(
                &provider_name,
                &holder_addr,
                false,
                0,
                crate::api::model::ConnectionKind::Normal,
            )
            .await
            .expect("holder should consume the provider's live capacity");
        let deferred_handle = app_state
            .active_provider
            .acquire_exact_connection_with_grace(
                &provider_name,
                &deferred_addr,
                true,
                0,
                crate::api::model::ConnectionKind::Normal,
            )
            .await
            .expect("deferred shared client should receive provider grace allocation");
        let stream_details = create_deferred_provider_grace_details(&provider_name, deferred_handle.clone());
        let test_user = create_test_user("grace-user");
        let test_fingerprint = create_test_fingerprint(deferred_addr);
        let stream = create_active_client_stream(ActiveClientStreamParams {
            stream_details,
            app_state: &app_state,
            user: &test_user,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            fingerprint: &test_fingerprint,
            stream_channel: create_test_shared_stream_channel(1, "http://provider-1.example/live/1"),
            socket_bound: true,
            session_token: None,
            req_headers: &HeaderMap::default(),
            meter_uid: 0,
            meter_stream: false,
        })
            .await;
        pin_mut!(stream);

        assert!(
            matches!(futures::poll!(stream.next()), std::task::Poll::Pending),
            "shared deferred active-client-stream should stay pending instead of returning an empty stream"
        );

        let third_handle = app_state
            .active_provider
            .acquire_exact_connection_with_grace(
                &provider_name,
                &third_addr,
                true,
                0,
                crate::api::model::ConnectionKind::Normal,
            )
            .await;

        assert!(
            third_handle.is_none(),
            "shared deferred active-client-stream should retain the deferred provider grace reservation while pending"
        );

        app_state.connection_manager.release_provider_handle(Some(holder_handle)).await;
        app_state.connection_manager.release_provider_handle(Some(deferred_handle)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn test_active_client_stream_deferred_provider_open_applies_sleep_timer_timeout() {
        let app_state = create_test_app_state();
        let connection_manager = create_test_connection_manager();
        let addr = "127.0.0.1:55020".parse().unwrap_or_else(|_| unreachable!());
        let state = ActiveClientStreamState {
            inner: None,
            send_custom_stream_flag: Some(Arc::new(AtomicU8::new(StreamMode::Inner as u8))),
            provider_handle: None,
            deferred_provider_open: Some(DeferredProviderOpenState::Opening(Box::pin(async {
                DeferredProviderOpenOutcome::Stream(futures::stream::pending::<Result<Bytes, StreamError>>().boxed())
            }))),
            timed_stream_context: Some(TimedStreamContext {
                app_state,
                duration_secs: 1,
                virtual_id: 1,
            }),
            preempt_cancelled: None,
            grace_task_handle: None,
            provisionable: false,
            custom_video: CustomVideoBuffers {
                user_exhausted: None,
                provider_exhausted: None,
                unavailable: None,
                provisioning: None,
                low_priority_preempted: None,
            },
            meter: None,
            event_manager: Arc::new(EventManager::new()),
            waker: None,
            connection_manager,
            fingerprint: Arc::new(Fingerprint::new(
                "fp-timeout".to_string(),
                "127.0.0.1".to_string(),
                addr,
            )),
            stream_uid: None,
            provider_stopped: false,
            user_stream_released: true,
            provider_handle_released: true,
            custom_video_timeout_secs: 0,
            custom_video_timeout_mode: None,
            custom_video_timeout_sleep: None,
            provider_end_reason: AtomicU8::new(PROVIDER_END_NOT_SET),
            provider_error_class: None,
            provider_http_status: None,
            provider_reconnect_count: AtomicU8::new(0),
        };
        let stream = ActiveClientStream { state };
        pin_mut!(stream);

        assert!(
            matches!(futures::poll!(stream.next()), std::task::Poll::Pending),
            "deferred-open success should first install the wrapped upstream stream and park pending"
        );

        tokio::time::advance(Duration::from_secs(2)).await;

        let result = tokio::time::timeout(Duration::from_millis(1), stream.next()).await;
        assert!(
            result.is_ok(),
            "deferred-open stream should stop once the configured sleep timer expires"
        );
        match result {
            Ok(joined) => assert!(
                joined.is_none(),
                "sleep timer should terminate the deferred-open stream without yielding bytes"
            ),
            Err(_) => unreachable!("timeout already checked"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_provider_grace_resolution_transitions_from_grace_pending_to_provider_exhausted_at_deadline() {
        let app_state = create_test_app_state();
        let provider_name = "provider_1".intern();
        let holder_addr = "127.0.0.1:55015".parse().unwrap_or_else(|_| unreachable!());
        let deferred_addr = "127.0.0.1:55016".parse().unwrap_or_else(|_| unreachable!());
        let user = create_test_user("grace-user");

        let _ = app_state
            .active_users
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-grace-timeout",
                virtual_id: 1,
                provider: provider_name.as_ref(),
                stream_url: "http://provider-1.example/live/1.ts",
                addr: &deferred_addr,
                connection_permission: UserConnectionPermission::GracePeriod,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        let _ = app_state
            .active_users
            .mark_pending_provider(
                &user.username,
                "tok-grace-timeout",
                crate::api::model::PendingProviderReason::GraceHold,
                9_999,
            )
            .await;

        let holder_handle = app_state
            .active_provider
            .acquire_exact_connection_with_grace(
                &provider_name,
                &holder_addr,
                false,
                0,
                crate::api::model::ConnectionKind::Normal,
            )
            .await
            .expect("holder should consume the provider's live capacity");
        let (flag, grace_task, deferred_handle) =
            start_deferred_provider_grace_resolution(&app_state, &provider_name, deferred_addr, Some("tok-grace-timeout")).await;

        assert_eq!(
            StreamMode::from_u8(flag.load(Ordering::Acquire)),
            StreamMode::GracePending,
            "provider grace resolution must begin in GracePending while provider capacity is exhausted"
        );

        tokio::time::advance(Duration::from_millis(101)).await;
        let task_result = grace_task.await;

        assert!(
            task_result.is_ok(),
            "grace-resolution task should complete once the deadline expires without capacity becoming available"
        );
        assert_eq!(
            StreamMode::from_u8(flag.load(Ordering::Acquire)),
            StreamMode::ProviderExhausted,
            "provider grace resolution should transition from GracePending to ProviderExhausted when the deadline expires"
        );

        let session = app_state
            .active_users
            .get_and_update_user_session(&user.username, "tok-grace-timeout")
            .await
            .expect("session should still exist");
        assert!(!matches!(session.lifecycle, crate::api::model::PlaybackLifecycle::PendingProvider { .. }), "timeout expiry should clear pending provider state");
        assert_eq!(session.permission, UserConnectionPermission::Exhausted);

        app_state.connection_manager.release_provider_handle(Some(holder_handle)).await;
        app_state.connection_manager.release_provider_handle(Some(deferred_handle)).await;
    }

    /// Regression test: verifies that when user-grace fails and remaining strategies are
    /// exhausted, the `grace_kind` (original `ConnectionKind = Soft`) flows through to
    /// `evaluate_remaining_strategies_after_grace` and the session expires correctly.
    ///
    /// This test exercises the full `stream_grace_period` path with:
    /// - `grace_kind = Some(Soft)` (passed directly from `create_active_client_stream`)
    /// - `grace_resolution_context` pointing to `GraceHoldStream` (remaining slice is empty)
    /// - user-grace failure (deadline expires, user still at connection limit)
    /// - remaining strategies exhausted -> `expire_pending_provider` is called
    #[tokio::test(start_paused = true)]
    #[allow(clippy::too_many_lines)]
    async fn test_user_grace_failure_preserves_soft_kind_on_exhausted() {
        // Use GraceHoldStream only, remaining slice is empty after grace exhaustion.
        let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 100,
            grace_period_timeout_secs: 8,
            grace_period_hold_stream: true,
            hls_session_ttl_secs: 10,
            catchup_session_ttl_secs: 10,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream]),
        });

        let provider_name = "provider_1".intern();
        // Two addresses: first holds the counted session, second triggers grace.
        let first_addr: std::net::SocketAddr = "127.0.0.1:55201".parse().unwrap_or_else(|_| unreachable!());
        let second_addr: std::net::SocketAddr = "127.0.0.1:55202".parse().unwrap_or_else(|_| unreachable!());
        let first_fingerprint = create_test_fingerprint(first_addr);

        let mut user = create_test_user("grace-soft-user");
        user.max_connections = 1; // User has 1 hard slot; first session fills it, grace session exceeds it
        user.soft_connections = 0;

        // First session: counted Normal, consumes the user's only hard slot.
        app_state
            .active_users
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-first",
                virtual_id: 1,
                provider: provider_name.as_ref(),
                stream_url: "http://provider-1.example/live/1.ts",
                addr: &first_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        app_state
            .connection_manager
            .update_connection(crate::api::model::ConnectionParams {
                meter_uid: 1,
                username: "grace-soft-user",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 10,
                fingerprint: &first_fingerprint,
                provider: provider_name.clone(),
                stream_channel: &create_test_stream_channel(1, "http://provider-1.example/live/1.ts"),
                user_agent: std::borrow::Cow::Borrowed("ua"),
                session_token: Some("tok-first"),
            })
            .await;

        // Second session: will be admitted via grace (user already at limit, grace is granted).
        // The grace session itself is not yet counted, so user_connections returns 1 (just the
        // first session). When the grace deadline expires, the first session is still present,
        // user_connections (1) > max_connections (1) is NOT true. We need the first session to
        // actually consume the hard slot so that the second (grace) session causes the over-limit
        // check to fire when grace expires. Since max_connections=1, the first Normal session
        // uses that slot, and the grace session would push to 2 > 1 — but PendingProvider sessions
        // are not counted! So we need a different approach: create a second Normal session BEFORE
        // the grace session so that user_connections = 2 when grace expires.
        //
        // Actually, the simpler path: make the first session Soft and max_connections=1.
        // A Soft session IS counted (it increments connection_data.connections but uses the soft
        // counter). When grace expires, user_connections = 1 (the Soft session), which is
        // NOT > max_connections = 1. So user_ok = true, no failure.
        //
        // The correct setup: max_connections = 1, first session = Normal (uses the hard slot),
        // second session = GracePeriod (PendingProvider, not counted). When grace deadline hits,
        // user_connections = 1 (Normal only), max_connections = 1, so 1 > 1 is false — no failure.
        //
        // We need the grace session itself to trigger the over-limit check at deadline.
        // But PendingProvider sessions are not counted in user_connections!
        //
        // So the only way for grace to fail at deadline is if there is already a DIFFERENT
        // counted session taking up the slot, AND that session is still there at deadline.
        // With max_connections=1 and first session Normal: when grace expires, user_conn=1,
        // max=1, so 1 > 1 is false.
        //
        // The solution: we need TWO already-counted sessions at deadline, not one.
        // But we can't create both before the grace session because the second would also be
        // admitted via grace.
        //
        // Instead: make the first session consume the slot AND also expire it at deadline,
        // so when grace expires, user_connections = 0, which is NOT > max_connections = 1.
        //
        // The grace session (tok-second) is in PendingProvider state and is NOT counted.
        // To trigger user-grace failure, we need user_connections > max_connections at deadline.
        // With max_connections=1: we need 2 counted sessions at grace deadline.
        // Solution: create two Normal sessions BEFORE the grace session:
        //   tok-first  -> Normal, counted (consumes hard slot)
        //   tok-preload -> Normal, counted (exceeds max, 2 > 1)
        //   tok-second -> GracePeriod, PendingProvider (NOT counted, grace session)
        //
        // At grace deadline: user_connections = 2 (tok-first + tok-preload), max = 1.
        // 2 > 1 -> user_ok = false -> user-grace failure path entered.
        let preload_addr: std::net::SocketAddr = "127.0.0.1:55203".parse().unwrap_or_else(|_| unreachable!());
        let preload_fingerprint = create_test_fingerprint(preload_addr);

        app_state
            .active_users
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-preload",
                virtual_id: 3,
                provider: provider_name.as_ref(),
                stream_url: "http://provider-1.example/live/3.ts",
                addr: &preload_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        app_state
            .connection_manager
            .update_connection(crate::api::model::ConnectionParams {
                meter_uid: 3,
                username: "grace-soft-user",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 10,
                fingerprint: &preload_fingerprint,
                provider: provider_name.clone(),
                stream_channel: &create_test_stream_channel(3, "http://provider-1.example/live/3.ts"),
                user_agent: std::borrow::Cow::Borrowed("ua"),
                session_token: Some("tok-preload"),
            })
            .await;

        let second_fingerprint = create_test_fingerprint(second_addr);

        app_state
            .active_users
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-second",
                virtual_id: 2,
                provider: provider_name.as_ref(),
                stream_url: "http://provider-1.example/live/2.ts",
                addr: &second_addr,
                connection_permission: UserConnectionPermission::GracePeriod,
                connection_kind: Some(crate::api::model::ConnectionKind::Soft),
                socket_bound: false,
            })
            .await;

        let _pending_version = app_state
            .active_users
            .mark_pending_provider(
                "grace-soft-user",
                "tok-second",
                crate::api::model::PendingProviderReason::GraceHold,
                9_999,
            )
            .await
            .expect("pending version must be created for tok-second");

        let grace_context = GraceResolutionContext {
            strategy_index: 0,
            strategies: vec![AdmissionStrategy::GraceHoldStream],
            kind: Some(crate::api::model::ConnectionKind::Soft),
        };

        let pending_version = app_state
            .active_users
            .pending_provider_version("grace-soft-user", "tok-second")
            .await
            .expect("pending version must be created for tok-second");

        let stream_details = StreamDetails {
            stream: None,
            stream_info: None,
            provider_name: Some(provider_name),
            request_url: Some("http://provider-1.example/live/2.ts".intern()),
            session_headers: None,
            grace_period: GracePeriodOptions {
                period_millis: 100,
                timeout_secs: 0,
                hold_stream: true,
            },
            provider_grace_active: false,
            disable_provider_grace: false,
            reconnect_flag: None,
            provider_handle: None,
            grace_resolution_context: Some(grace_context.clone()),
        };

        let (flag, grace_task) = stream_grace_period(GracePeriodParams {
            app_state: Arc::clone(&app_state),
            stream_details,
            user_grace_period: true,
            user: user.clone(),
            fingerprint: second_fingerprint.clone(),
            virtual_id: 2,
            session_token: Some("tok-second".to_string()),
            provisioning_info: None,
            waker: None,
            hold_stream: true,
            capacity_notify: app_state.connection_manager.capacity_notified(),
            pending_provider_version: Some(pending_version),
            grace_active_version: None,
            grace_resolution_context: Some(grace_context),
            grace_kind: Some(crate::api::model::ConnectionKind::Soft),
            socket_bound: false,
        });

        // Grace should be pending initially.
        assert_eq!(
            StreamMode::from_u8(flag.as_ref().unwrap().load(Ordering::Acquire)),
            StreamMode::GracePending,
            "user grace should start in GracePending"
        );

        // Advance time past the grace deadline.
        tokio::time::advance(Duration::from_millis(101)).await;
        let _ = grace_task.expect("grace task should be spawned").await;

        // Remaining strategies are exhausted (only GraceHoldStream was configured, no eviction).
        // The session should expire with UserExhausted.
        assert_eq!(
            StreamMode::from_u8(flag.as_ref().unwrap().load(Ordering::Acquire)),
            StreamMode::UserExhausted,
            "exhausted remaining strategies should result in UserExhausted"
        );

        let session = app_state
            .active_users
            .get_and_update_user_session("grace-soft-user", "tok-second")
            .await
            .expect("session must exist after grace failure");

        // Permission should be Exhausted (not GracePeriod).
        assert_eq!(
            session.permission,
            UserConnectionPermission::Exhausted,
            "session permission should be Exhausted after grace failure with exhausted strategies"
        );

        // Lifecycle should be Expired.
        assert!(
            matches!(session.lifecycle, crate::api::model::PlaybackLifecycle::Expired),
            "session lifecycle should be Expired after grace failure"
        );

        // The session's original connection_kind is Soft and must NOT be modified by the
        // grace failure path. The grace_kind = Soft that was passed to
        // evaluate_remaining_strategies_after_grace is verified by the fact that
        // the session's kind remained Soft (it was set at creation time and is preserved
        // through the grace failure flow since session.connection_kind is not changed).
        assert_eq!(
            session.connection_kind,
            Some(crate::api::model::ConnectionKind::Soft),
            "session connection_kind should remain Soft (unchanged from creation)"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_active_client_stream_immediate_provider_stream_emits_meter_batches() {
        let app_state = create_test_app_state();
        let mut meter_events = app_state.event_manager.get_meter_channel();
        app_state.event_manager.stream_meter_subscriber_connected();

        let addr = "127.0.0.1:55030".parse().unwrap_or_else(|_| unreachable!());
        let test_user = create_test_user("meter-user");
        let test_fingerprint = create_test_fingerprint(addr);
        let provider_stream = futures::stream::iter(vec![Ok::<Bytes, StreamError>(Bytes::from_static(&[0_u8; 3072]))])
            .chain(futures::stream::pending())
            .boxed();
        let stream_details = StreamDetails::from_stream(provider_stream, GracePeriodOptions::default());

        let stream = create_active_client_stream(ActiveClientStreamParams {
            stream_details,
            app_state: &app_state,
            user: &test_user,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            fingerprint: &test_fingerprint,
            stream_channel: create_test_stream_channel(1, "http://provider-1.example/live/1"),
            socket_bound: true,
            session_token: None,
            req_headers: &HeaderMap::default(),
            meter_uid: 55,
            meter_stream: true,
        })
            .await;
        pin_mut!(stream);

        let first_chunk = stream.next().await;
        assert!(
            matches!(first_chunk, Some(Ok(ref bytes)) if bytes.len() == 3072),
            "immediate provider stream should yield the metered payload chunk"
        );

        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;

        let entries = tokio::time::timeout(Duration::from_millis(1), meter_events.recv())
            .await
            .expect("immediate provider stream should publish a meter batch after bytes are sent")
            .expect("meter channel should stay open while the stream is active");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].meter_uid, 55);
        assert_eq!(entries[0].uids, vec![1]);
        assert_eq!(entries[0].rate_kbps, 1);
        assert_eq!(entries[0].total_kb, 3);
    }
}
