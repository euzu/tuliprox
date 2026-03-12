use crate::{
    api::{
        model::{
            AppState, BoxedProviderStream, CleanupEvent, ConnectionManager, CustomVideoStreamType, ProviderHandle,
            StreamDetails, StreamError, TimedClientStream, TransportStreamBuffer,
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
use log::{debug, error, info};
use shared::{
    model::{StreamChannel, UserConnectionPermission, VirtualId},
    utils::sanitize_sensitive_info,
};
use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
    task::{Context, Poll},
};
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

#[allow(clippy::struct_excessive_bools)]
struct ActiveClientStreamState {
    inner: BoxedProviderStream,
    send_custom_stream_flag: Option<Arc<AtomicU8>>,
    provider_handle: Option<ProviderHandle>,
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
        self.release_user_stream();

        let mut serve_preempted_custom = false;
        if self.provider_handle.is_some() {
            let handle = self.provider_handle.take();
            self.provider_handle_released = true;
            if self.custom_video.low_priority_preempted.is_some() {
                serve_preempted_custom = true;
                if let Some(flag) = &self.send_custom_stream_flag {
                    flag.store(StreamMode::LowPriorityPreempted as u8, Ordering::Release);
                } else {
                    self.send_custom_stream_flag = Some(Arc::new(AtomicU8::new(StreamMode::LowPriorityPreempted as u8)));
                }
            } else if let Some(flag) = &self.send_custom_stream_flag {
                flag.store(StreamMode::Inner as u8, Ordering::Release);
            }

            if let Some(waker) = &self.waker {
                waker.wake();
            }

            let addr = self.fingerprint.addr;
            self.inner = futures::stream::empty::<Result<Bytes, StreamError>>().boxed();

            debug_if_enabled!(
                "Provider stream preempted for {}; stopping client stream",
                sanitize_sensitive_info(&addr.to_string())
            );
            self.connection_manager.send_cleanup(CleanupEvent::UpdateDetailAndReleaseProvider {
                addr,
                video_type: CustomVideoStreamType::LowPriorityPreempted,
                handle,
            });
        }
        serve_preempted_custom
    }

    fn stop_provider_stream(&mut self, mode: StreamMode) {
        self.provider_stopped = true;
        self.preempt_cancelled = None;
        self.stop_grace_task();
        self.release_user_stream();

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
            self.inner = futures::stream::empty::<Result<Bytes, StreamError>>().boxed();

            let video_type = Self::custom_video_type_for_mode(mode);
            debug_if_enabled!(
                "Provider stream stopped due to grace period or unavailable provider channel for {}",
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

pub(in crate::api) struct ActiveClientStream {
    state: ActiveClientStreamState,
}

impl Stream for ActiveClientStream {
    type Item = Result<Bytes, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // 1. Preemption check (user priority feature)
        if let Some(fut) = self.state.preempt_cancelled.as_mut() {
            if fut.as_mut().poll(cx).is_ready() {
                if !self.state.stop_provider_stream_preempted() {
                    return Poll::Ready(None);
                }
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
                Poll::Pending
            }

            // Live streaming: forward bytes from upstream provider
            StreamMode::Inner => {
                self.state.reset_custom_video_timeout();
                match Pin::new(&mut self.state.inner).poll_next(cx) {
                    Poll::Ready(Some(Ok(bytes))) => Poll::Ready(Some(Ok(bytes))),
                    Poll::Ready(Some(Err(e))) => {
                        error!("Inner stream error: {e:?}");
                        if self.state.grace_task_handle.is_none() {
                            self.state.stop_provider_stream(StreamMode::ChannelUnavailable);
                            Poll::Ready(None)
                        } else {
                            // Grace task is still resolving; keep stream alive until it decides
                            Poll::Pending
                        }
                    }
                    Poll::Ready(None) => {
                        if self.state.grace_task_handle.is_none() {
                            self.state.stop_provider_stream(StreamMode::ChannelUnavailable);
                            Poll::Ready(None)
                        } else {
                            // Grace task is still resolving; keep stream alive until it decides
                            Poll::Pending
                        }
                    }
                    Poll::Pending => Poll::Pending,
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
                    StreamMode::UserExhausted     => self.state.custom_video.user_exhausted.as_mut(),
                    StreamMode::ProviderExhausted => self.state.custom_video.provider_exhausted.as_mut(),
                    StreamMode::ChannelUnavailable => self.state.custom_video.unavailable.as_mut(),
                    StreamMode::Provisioning      => self.state.custom_video.provisioning.as_mut(),
                    StreamMode::LowPriorityPreempted => self.state.custom_video.low_priority_preempted.as_mut(),
                    _ => None,
                };

                if let Some(buffer) = buffer_opt {
                    buffer.register_waker(cx.waker());
                    match buffer.next_chunk() {
                        Some(chunk) => Poll::Ready(Some(Ok(chunk))),
                        None => {
                            // Provisioning loops until preemption fires; all others terminate.
                            if is_provisioning {
                                Poll::Pending
                            } else {
                                info!(
                                    "Custom video {video_mode:?} buffer exhausted for {}, terminating stream",
                                    sanitize_sensitive_info(&self.state.fingerprint.addr.to_string())
                                );
                                Poll::Ready(None)
                            }
                        }
                    }
                } else {
                    // No custom video configured for this mode -> terminate immediately.
                    info!(
                        "No custom video configured for {video_mode:?} mode for {}, terminating stream",
                        sanitize_sensitive_info(&self.state.fingerprint.addr.to_string())
                    );
                    Poll::Ready(None)
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

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) async fn create_active_client_stream(
    mut stream_details: StreamDetails,
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    connection_permission: UserConnectionPermission,
    fingerprint: &Fingerprint,
    stream_channel: StreamChannel,
    session_token: Option<&str>,
    req_headers: &HeaderMap,
) -> BoxedProviderStream {
    if connection_permission == UserConnectionPermission::Exhausted {
        error!("Something is wrong this should not happen");
    }
    let grant_user_grace_period = connection_permission == UserConnectionPermission::GracePeriod;
    let username = user.username.as_str();
    let provider_name = stream_details.provider_name.as_ref().map_or_else(String::new, ToString::to_string);

    let user_agent = req_headers.get(USER_AGENT).map(|h| String::from_utf8_lossy(h.as_bytes())).unwrap_or_default();

    let virtual_id = stream_channel.virtual_id;
    let is_shared_source_stream = stream_channel.shared && stream_details.request_url.is_some();
    app_state
        .connection_manager
        .update_connection(
            username,
            user.max_connections,
            fingerprint,
            &provider_name,
            stream_channel,
            user_agent,
            session_token,
        )
        .await;
    if let Some((_, _, _m_, Some(cvt))) = stream_details.stream_info.as_ref() {
        app_state.connection_manager.update_stream_detail(&fingerprint.addr, *cvt).await;
    }

    // Shared broadcaster source (first subscriber path): feed provider bytes directly.
    // Grace/custom handling is not needed here because this stream is only the fan-out source.
    if is_shared_source_stream {
        let stream = match stream_details.stream.take() {
            None => {
                let provider_handle = stream_details.provider_handle.take();
                app_state.connection_manager.release_provider_handle(provider_handle).await;
                futures::stream::empty::<Result<Bytes, StreamError>>().boxed()
            }
            Some(stream) => {
                let config = app_state.app_config.config.load();
                match config.sleep_timer_mins {
                    None => stream,
                    Some(mins) => {
                        let secs = u32::try_from((u64::from(mins) * 60).min(u64::from(u32::MAX))).unwrap_or(0);
                        if secs > 0 {
                            TimedClientStream::new(app_state, stream, secs, fingerprint.addr, virtual_id).boxed()
                        } else {
                            stream
                        }
                    }
                }
            }
        };
        return stream;
    }

    let provisioning_info = resolve_grace_period_provisioning(app_state, &stream_details);
    let has_provisioning = provisioning_info.is_some();
    let hold_stream = stream_details.grace_period.hold_stream;

    let waker = Arc::new(AtomicWaker::new());
    let (grace_stop_flag, grace_task_handle) = if grant_user_grace_period || stream_details.provider_grace_active {
        stream_grace_period(
            app_state,
            &stream_details,
            grant_user_grace_period,
            user,
            fingerprint,
            virtual_id,
            provisioning_info,
            Some(Arc::clone(&waker)),
            hold_stream,
        )
    } else {
        stream_grace_period(
            app_state,
            &stream_details,
            grant_user_grace_period,
            user,
            fingerprint,
            virtual_id,
            provisioning_info,
            None,
            hold_stream,
        )
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

    let stream = match stream_details.stream.take() {
        None => {
            let provider_handle = stream_details.provider_handle.take();
            app_state.connection_manager.release_provider_handle(provider_handle).await;
            futures::stream::empty::<Result<Bytes, StreamError>>().boxed()
        }
        Some(stream) => {
            let config = app_state.app_config.config.load();
            match config.sleep_timer_mins {
                None => stream,
                Some(mins) => {
                    let secs = u32::try_from((u64::from(mins) * 60).min(u64::from(u32::MAX))).unwrap_or(0);
                    if secs > 0 {
                        TimedClientStream::new(app_state, stream, secs, fingerprint.addr, virtual_id).boxed()
                    } else {
                        stream
                    }
                }
            }
        }
    };

    let preempt_cancelled = stream_details.provider_handle
        .as_ref()
        .and_then(|h| h.cancel_token.as_ref())
        .map(|token| Box::pin(token.clone().cancelled_owned()));

    let state = ActiveClientStreamState {
        inner: stream,
        preempt_cancelled,
        grace_task_handle,
        provider_handle: stream_details.provider_handle,
        send_custom_stream_flag: grace_stop_flag,
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

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn stream_grace_period(
    app_state: &Arc<AppState>,
    stream_details: &StreamDetails,
    user_grace_period: bool,
    user: &ProxyUserCredentials,
    fingerprint: &Fingerprint,
    virtual_id: VirtualId,
    provisioning_info: Option<GraceProvisioningInfo>,
    waker: Option<Arc<AtomicWaker>>,
    hold_stream: bool,
) -> (Option<Arc<AtomicU8>>, Option<tokio::task::JoinHandle<()>>) {
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

    debug!("hold stream {hold_stream}");

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
                tokio::time::sleep(tokio::time::Duration::from_millis(grace_period_millis)).await;

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
    use super::{ActiveClientStreamState, StreamMode};
    use crate::api::model::CustomVideoStreamType;

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
}
