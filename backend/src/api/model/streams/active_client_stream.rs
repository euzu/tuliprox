use crate::{
    api::{
        api_utils::get_stream_options,
        model::{
            create_provider_stream, AppState, BoxedProviderStream, CleanupEvent, ConnectionManager,
            CustomVideoStreamType, ProviderHandle, ProviderStreamFactoryOptions, StreamDetails, StreamError,
            TimedClientStream, TransportStreamBuffer,
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
use log::{error, info, trace};
use shared::{
    model::{StreamChannel, UserConnectionPermission, VirtualId},
    utils::sanitize_sensitive_info,
};
use std::{
    pin::Pin,
    net::SocketAddr,
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
    Inner            = 0,
    /// Show the "user connections exhausted" custom video.
    UserExhausted    = 1,
    /// Show the "provider connections exhausted" custom video.
    ProviderExhausted = 2,
    /// Show the "channel unavailable" custom video.
    ChannelUnavailable = 3,
    /// Show the provisioning/placeholder custom video while probing for capacity.
    Provisioning     = 4,
    /// Show the "low-priority preempted" custom video.
    LowPriorityPreempted = 5,
    /// Transient: grace-period check is still in progress; `poll_next` must park.
    GracePending     = 255,
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
    user_exhausted:    Option<TransportStreamBuffer>,
    provider_exhausted: Option<TransportStreamBuffer>,
    unavailable:       Option<TransportStreamBuffer>,
    provisioning:      Option<TransportStreamBuffer>,
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
    pub fingerprint: &'a Fingerprint,
    pub stream_channel: StreamChannel,
    pub session_token: Option<&'a str>,
    pub req_headers: &'a HeaderMap,
    pub meter_uid: u32,
}

struct GracePeriodParams<'a> {
    app_state: &'a Arc<AppState>,
    stream_details: &'a StreamDetails,
    user_grace_period: bool,
    user: &'a ProxyUserCredentials,
    fingerprint: &'a Fingerprint,
    virtual_id: VirtualId,
    provisioning_info: Option<GraceProvisioningInfo>,
    waker: Option<Arc<AtomicWaker>>,
    hold_stream: bool,
    capacity_notify: Arc<Notify>,
}

enum DeferredProviderOpenOutcome {
    Stream(BoxedProviderStream),
    Mode(StreamMode),
    Failed,
}

enum DeferredProviderOpenState {
    Pending(Box<DeferredProviderOpenContext>),
    Opening(Pin<Box<dyn Future<Output = DeferredProviderOpenOutcome> + Send>>),
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
    waker: Option<Arc<AtomicWaker>>,
    connection_manager: Arc<ConnectionManager>,
    fingerprint: Arc<Fingerprint>,
    provider_stopped: bool,
    user_stream_released: bool,
    /// Mirrors `user_stream_released` for the provider handle to guard against double-release
    /// when preemption and Drop race.
    provider_handle_released: bool,
    custom_video_timeout_secs: u32,
    custom_video_timeout_mode: Option<StreamMode>,
    custom_video_timeout_sleep: Option<Pin<Box<tokio::time::Sleep>>>,
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
            disabled_headers: disabled_headers.as_ref(),
            default_user_agent: default_user_agent.as_deref(),
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
                            if self.state.grace_task_handle.is_none() {
                                self.state.stop_provider_stream(StreamMode::ChannelUnavailable);
                                return Poll::Ready(None);
                            }

                            return Poll::Pending;
                        }
                        Some(Poll::Ready(None)) | None => {
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
            self.state.connection_manager.send_cleanup(CleanupEvent::ReleaseStreamAndProviderHandle { addr, handle: handle_for_cleanup });
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
        fingerprint,
        stream_channel,
        session_token,
        req_headers,
        meter_uid,
    } = request;
    if connection_permission == UserConnectionPermission::Exhausted {
        error!("Something is wrong this should not happen");
    }
    let grant_user_grace_period = connection_permission == UserConnectionPermission::GracePeriod;
    let username = user.username.as_str();
    let provider_name = stream_details.provider_name.as_deref().unwrap_or("");

    let user_agent = req_headers.get(USER_AGENT).map(|h| String::from_utf8_lossy(h.as_bytes())).unwrap_or_default();

    let virtual_id = stream_channel.virtual_id;
    let is_shared_source_stream = stream_channel.shared && stream_details.stream.is_some();
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid,
            username,
            max_connections: user.max_connections,
            fingerprint,
            provider: provider_name,
            stream_channel: &stream_channel,
            user_agent,
            session_token,
        })
        .await;
    if let Some((_, _, _m_, Some(cvt))) = stream_details.stream_info.as_ref() {
        app_state.connection_manager.update_stream_detail(&fingerprint.addr, *cvt).await;
    }

    // Shared broadcaster source (first subscriber path): feed provider bytes directly.
    // Grace/custom handling is not needed here because this stream is only the fan-out source.
    if is_shared_source_stream {
        if let Some(stream) = stream_details.stream.take() {
            return wrap_timed_client_stream_if_needed(app_state, stream, fingerprint.addr, virtual_id);
        }
    }

    let provisioning_info = resolve_grace_period_provisioning(app_state, &stream_details);
    let has_provisioning = provisioning_info.is_some();
    let hold_stream = stream_details.grace_period.hold_stream;
    let capacity_notify = app_state.connection_manager.capacity_notified();

    let waker = Arc::new(AtomicWaker::new());
    let (grace_stop_flag, grace_task_handle) = if grant_user_grace_period || stream_details.provider_grace_active {
        stream_grace_period(GracePeriodParams {
            app_state,
            stream_details: &stream_details,
            user_grace_period: grant_user_grace_period,
            user,
            fingerprint,
            virtual_id,
            provisioning_info,
            waker: Some(Arc::clone(&waker)),
            hold_stream,
            capacity_notify,
        })
    } else {
        stream_grace_period(GracePeriodParams {
            app_state,
            stream_details: &stream_details,
            user_grace_period: grant_user_grace_period,
            user,
            fingerprint,
            virtual_id,
            provisioning_info,
            waker: None,
            hold_stream,
            capacity_notify,
        })
    };

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
            user_exhausted:    c.user_connections_exhausted.clone(),
            provider_exhausted: c.provider_connections_exhausted.clone(),
            unavailable:       c.channel_unavailable.clone(),
            provisioning:      c.panel_api_provisioning.clone(),
            low_priority_preempted: c.low_priority_preempted.clone(),
        },
    );

    let deferred_provider_open =
        create_deferred_provider_open_future(app_state, &stream_details, fingerprint, &stream_channel, req_headers);
    let timed_stream_context = deferred_provider_open
        .as_ref()
        .and_then(|_| create_timed_stream_context(app_state, virtual_id));

    let stream: Option<BoxedProviderStream> = match stream_details.stream.take() {
        None => {
            if !stream_details.has_deferred_provider_open() {
                let provider_handle = stream_details.provider_handle.take();
                app_state.connection_manager.release_provider_handle(provider_handle).await;
            }
            None
        }
        Some(stream) => {
            Some(wrap_timed_client_stream_if_needed(app_state, stream, fingerprint.addr, virtual_id))
        }
    };

    let preempt_cancelled = stream_details.provider_handle
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
        provider_handle: stream_details.provider_handle,
        send_custom_stream_flag,
        provisionable: has_provisioning,
        custom_video,
        waker: Some(waker),
        connection_manager: Arc::clone(&app_state.connection_manager),
        fingerprint: Arc::new(fingerprint.clone()),
        provider_stopped: false,
        user_stream_released: false,
        provider_handle_released: false,
        custom_video_timeout_secs,
        custom_video_timeout_mode: None,
        custom_video_timeout_sleep: None,
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
fn stream_grace_period(request: GracePeriodParams<'_>) -> (Option<Arc<AtomicU8>>, Option<tokio::task::JoinHandle<()>>) {
    let GracePeriodParams {
        app_state,
        stream_details,
        user_grace_period,
        user,
        fingerprint,
        virtual_id,
        provisioning_info,
        waker,
        hold_stream,
        capacity_notify,
    } = request;
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

    trace!("grace hold stream {hold_stream}");

    if provider_grace_check.is_some() || user_grace_check.is_some() {
        let stream_strategy_flag = Arc::new(AtomicU8::new(
            if hold_stream { StreamMode::GracePending as u8 } else { StreamMode::Inner as u8 },
        ));
        let stream_strategy_flag_copy = Arc::clone(&stream_strategy_flag);
        let grace_period_millis = stream_details.grace_period.period_millis;

        let user_manager = Arc::clone(&active_users);
        let provider_manager = Arc::clone(&active_provider);
        let connection_manager = Arc::clone(&connection_manager);
        let reconnect_flag = stream_details.reconnect_flag.clone();
        let fingerprint = fingerprint.clone();
        let app_state = Arc::clone(app_state);
        // Safety timeout: if async operations inside the grace task stall, force the flag
        // out of GRACE_PENDING so the client stream is not hung indefinitely.
        // Allow grace_period_millis for the intentional delay plus a 10-second buffer
        // for the async connection checks that follow.
        let grace_task_timeout =
            tokio::time::Duration::from_millis(grace_period_millis.saturating_add(10_000));
        // Clone handles for use in the timeout fallback, in case the inner async block is cancelled.
        let flag_for_fallback = Arc::clone(&stream_strategy_flag_copy);
        let waker_for_fallback = waker.clone();
        let grace_task_handle = tokio::spawn(async move {
            let timed_out = tokio::time::timeout(grace_task_timeout, async move {
                let deadline =
                    tokio::time::Instant::now() + tokio::time::Duration::from_millis(grace_period_millis);
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
                        () = tokio::time::sleep_until(deadline) => break,
                        () = &mut capacity_wait => {}
                    }
                }

                let mut updated = false;
                if let Some((username, max_connections)) = user_grace_check {
                    let active_connections = user_manager.user_connections(&username).await;
                    if active_connections > max_connections {
                        stream_strategy_flag_copy.store(StreamMode::UserExhausted as u8, Ordering::Release);
                        connection_manager
                            .update_stream_detail(&fingerprint.addr, CustomVideoStreamType::UserConnectionsExhausted)
                            .await;
                        // Release the shared stream subscription to stop the subscriber loop
                        connection_manager.shared_stream_manager.release_connection(&fingerprint.addr, true).await;
                        info!("User connections exhausted for active clients: {username}");
                        updated = true;
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
        CustomVideoBuffers, DeferredProviderOpenOutcome, DeferredProviderOpenState, StreamMode, TimedStreamContext,
        GracePeriodParams,
    };
    use crate::{
        api::model::{
            ActiveProviderManager, ActiveUserManager, AppState, CancelTokens, ConnectionManager, CustomVideoStreamType,
            DownloadQueue, EventManager, MetadataUpdateManager, PlaylistStorageState, SharedStreamManager, StreamDetails,
            StreamError, UpdateGuard,
        },
        auth::Fingerprint,
        model::{AppConfig, Config, ConfigInput, GracePeriodOptions, ProcessTargets, ProxyUserCredentials, SourcesConfig},
        utils::{FileLockManager, GeoIp},
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use axum::http::HeaderMap;
    use futures::{pin_mut, StreamExt};
    use reqwest::Client;
    use shared::{
        model::{ConfigPaths, InputFetchMethod, InputType, PlaylistItemType, StreamChannel, UserConnectionPermission, XtreamCluster},
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
    use bytes::Bytes;

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
            ffprobe_available: Arc::default(),
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
        let connection_manager =
            Arc::new(ConnectionManager::new(&active_users, &active_provider, &shared_stream_manager, &event_manager));

        let tokens = CancelTokens::default();
        let metadata_manager = Arc::new(MetadataUpdateManager::new(tokens.metadata.clone()));
        let (manual_update_sender, _) = mpsc::channel::<Arc<ProcessTargets>>(1);

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
            item_type: PlaylistItemType::Live,
            cluster: XtreamCluster::Live,
            group: "Live".intern(),
            title: "Test Channel".intern(),
            url: url.into(),
            shared: false,
            technical: None,
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
            grace_period: GracePeriodOptions {
                period_millis: 100,
                timeout_secs: 0,
                hold_stream: true,
            },
            provider_grace_active: true,
            disable_provider_grace: false,
            reconnect_flag: None,
            provider_handle: Some(provider_handle),
        }
    }

    async fn start_deferred_provider_grace_resolution(
        app_state: &Arc<AppState>,
        provider_name: &Arc<str>,
        deferred_addr: std::net::SocketAddr,
    ) -> (
        Arc<AtomicU8>,
        tokio::task::JoinHandle<()>,
        crate::api::model::ProviderHandle,
    ) {
        let deferred_handle = app_state
            .active_provider
            .acquire_exact_connection_with_grace(provider_name, &deferred_addr, true, 0)
            .await
            .expect("deferred client should receive provider grace allocation");
        let stream_details = create_deferred_provider_grace_details(provider_name, deferred_handle);
        let test_user = create_test_user("grace-user");
        let test_fingerprint = create_test_fingerprint(deferred_addr);
        let (flag, task) = stream_grace_period(GracePeriodParams {
            app_state,
            stream_details: &stream_details,
            user_grace_period: false,
            user: &test_user,
            fingerprint: &test_fingerprint,
            virtual_id: 1,
            provisioning_info: None,
            waker: None,
            hold_stream: true,
            capacity_notify: app_state.connection_manager.capacity_notified(),
        });
        (
            flag.expect("provider grace should install a mode flag"),
            task.expect("provider grace should spawn a grace-resolution task"),
            stream_details.provider_handle.expect("deferred provider handle must be retained during grace"),
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
            waker: None,
            connection_manager,
            fingerprint: Arc::new(Fingerprint::new(
                "fp-key".to_string(),
                "127.0.0.1".to_string(),
                addr,
            )),
            provider_stopped: true,
            user_stream_released: true,
            provider_handle_released: true,
            custom_video_timeout_secs: 5,
            custom_video_timeout_mode: None,
            custom_video_timeout_sleep: None,
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
            .acquire_exact_connection_with_grace(&provider_name, &holder_addr, false, 0)
            .await
            .expect("holder should consume the provider's live capacity");
        let (flag, grace_task, deferred_handle) =
            start_deferred_provider_grace_resolution(&app_state, &provider_name, deferred_addr).await;

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
    async fn test_active_client_stream_deferred_provider_grace_retains_provider_handle_while_grace_pending() {
        let app_state = create_test_app_state();
        let provider_name = "provider_1".intern();
        let holder_addr = "127.0.0.1:55012".parse().unwrap_or_else(|_| unreachable!());
        let deferred_addr = "127.0.0.1:55013".parse().unwrap_or_else(|_| unreachable!());
        let third_addr = "127.0.0.1:55014".parse().unwrap_or_else(|_| unreachable!());

        let holder_handle = app_state
            .active_provider
            .acquire_exact_connection_with_grace(&provider_name, &holder_addr, false, 0)
            .await
            .expect("holder should consume the provider's live capacity");
        let deferred_handle = app_state
            .active_provider
            .acquire_exact_connection_with_grace(&provider_name, &deferred_addr, true, 0)
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
            fingerprint: &test_fingerprint,
            stream_channel: create_test_stream_channel(1, "http://provider-1.example/live/1"),
            session_token: None,
            req_headers: &HeaderMap::default(),
            meter_uid: 0,
        })
        .await;
        pin_mut!(stream);

        assert!(
            matches!(futures::poll!(stream.next()), std::task::Poll::Pending),
            "deferred active-client-stream should park in GracePending while waiting for provider grace resolution"
        );

        let third_handle = app_state
            .active_provider
            .acquire_exact_connection_with_grace(&provider_name, &third_addr, true, 0)
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
            .acquire_exact_connection_with_grace(&provider_name, &holder_addr, false, 0)
            .await
            .expect("holder should consume the provider's live capacity");
        let deferred_handle = app_state
            .active_provider
            .acquire_exact_connection_with_grace(&provider_name, &deferred_addr, true, 0)
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
            fingerprint: &test_fingerprint,
            stream_channel: create_test_shared_stream_channel(1, "http://provider-1.example/live/1"),
            session_token: None,
            req_headers: &HeaderMap::default(),
            meter_uid: 0,
        })
        .await;
        pin_mut!(stream);

        assert!(
            matches!(futures::poll!(stream.next()), std::task::Poll::Pending),
            "shared deferred active-client-stream should stay pending instead of returning an empty stream"
        );

        let third_handle = app_state
            .active_provider
            .acquire_exact_connection_with_grace(&provider_name, &third_addr, true, 0)
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
            waker: None,
            connection_manager,
            fingerprint: Arc::new(Fingerprint::new(
                "fp-timeout".to_string(),
                "127.0.0.1".to_string(),
                addr,
            )),
            provider_stopped: false,
            user_stream_released: true,
            provider_handle_released: true,
            custom_video_timeout_secs: 0,
            custom_video_timeout_mode: None,
            custom_video_timeout_sleep: None,
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

        let holder_handle = app_state
            .active_provider
            .acquire_exact_connection_with_grace(&provider_name, &holder_addr, false, 0)
            .await
            .expect("holder should consume the provider's live capacity");
        let (flag, grace_task, deferred_handle) =
            start_deferred_provider_grace_resolution(&app_state, &provider_name, deferred_addr).await;

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

        app_state.connection_manager.release_provider_handle(Some(holder_handle)).await;
        app_state.connection_manager.release_provider_handle(Some(deferred_handle)).await;
    }
}
