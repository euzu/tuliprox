use super::{
    ActiveClientStream, ActiveClientStreamState, DeferredProviderOpenOutcome, DeferredProviderOpenState, StreamMode,
    TimedStreamContext,
};
use crate::api::model::{
    connection_manager::{PROVIDER_END_CLOSED, PROVIDER_END_ERROR, PROVIDER_END_NOT_SET},
    open_provider_stream_with_lifecycle, AppState, BoxedProviderStream, ProviderStreamOpen,
    ProviderStreamOpenLifecycle, StreamError, TimedClientStream,
};
use bytes::Bytes;
use futures::{Future, Stream, StreamExt};
use log::{error, info};
use shared::{model::VirtualId, utils::sanitize_sensitive_info};
use std::{
    net::SocketAddr,
    pin::Pin,
    sync::{atomic::Ordering, Arc},
    task::{Context, Poll},
};

impl ActiveClientStreamState {
    /// Drops the provider handle at most once (releasing the provider slot
    /// synchronously) and detaches the provider body stream. The stream claim itself
    /// is released separately through [`Self::release_user_stream`].
    pub(super) fn release_provider_handle_and_detach_body(&mut self) {
        self.inner = None;
        if !self.provider_handle_released {
            self.provider_handle_released = true;
            drop(self.provider_handle.take());
        }
    }

    pub(super) fn release_user_stream(&mut self) {
        if self.user_stream_released {
            return;
        }
        self.user_stream_released = true;
        if let Some(cleanup) = self.request_cleanup.as_mut() {
            cleanup.finish(
                self.lease_request_id,
                self.provider_end_reason.load(Ordering::Relaxed),
                self.provider_reconnect_count.load(Ordering::Relaxed),
                self.provider_error_class,
                self.provider_http_status,
            );
        }
    }

    /// Confirms the playback lease exactly once, when the first real provider media
    /// bytes reach the client. Only a confirmed lease may reserve provider capacity
    /// against other playbacks, so an abandoned or custom-video-only start does not
    /// block unrelated clients behind the same reverse proxy.
    fn confirm_lease_once(&mut self) {
        if self.lease_confirmed {
            return;
        }
        let Some(owner) = self.lease_owner.take() else {
            return;
        };
        self.lease_confirmed = true;
        if let Some(media_started) = &self.media_started {
            media_started.store(true, Ordering::Release);
        }
        let request_id = self.lease_request_id.or_else(|| {
            self.provider_handle.as_ref().and_then(|managed| managed.handle()).and_then(|h| h.playback_request_id)
        });
        // Confirm synchronously through the broker: a first-byte confirmation is a
        // mandatory capacity transition and must not be dropped by queue pressure.
        let provider_manager = &self.connection_manager.provider_manager;
        if let Some(request_id) = request_id {
            provider_manager.confirm_identified_playback_activity(&owner, request_id);
        } else {
            provider_manager.confirm_playback_activity(&owner);
        }
    }

    pub(super) fn release_stream_and_provider_handle_once(&mut self) {
        self.stop_grace_task();
        self.release_provider_handle_and_detach_body();
        self.release_user_stream();
    }

    fn mark_direct_body_progress(&mut self) {
        if self.direct_body_idle_timeout.mark_progress() {
            self.connection_manager.touch_direct_body_activity(&self.fingerprint.addr);
        }
    }

    /// Ends the response body without switching to a custom error video. Used for a
    /// suppressed reentry retry, where a visible "connections exhausted" video would be
    /// misleading: the request was declined by the reentry guard, not by a real limit.
    pub(super) fn terminate_quietly(&mut self) {
        self.provider_stopped = true;
        self.preempt_cancelled = None;
        self.stop_grace_task();
        self.release_provider_handle_and_detach_body();
        self.release_user_stream();
    }
}

pub(super) fn wrap_timed_client_stream_if_needed(
    app_state: &Arc<AppState>,
    stream: BoxedProviderStream,
    addr: SocketAddr,
    virtual_id: VirtualId,
    stream_uid: Option<u32>,
) -> BoxedProviderStream {
    let config = app_state.app_config.config.load();
    match config.sleep_timer_mins {
        None => stream,
        Some(mins) => {
            let secs = u32::try_from((u64::from(mins) * 60).min(u64::from(u32::MAX))).unwrap_or(0);
            if secs > 0 {
                TimedClientStream::new(
                    &app_state.app_config,
                    &app_state.connection_manager,
                    stream,
                    secs,
                    addr,
                    virtual_id,
                    stream_uid,
                )
                .boxed()
            } else {
                stream
            }
        }
    }
}

pub(super) fn create_timed_stream_context(
    app_state: &Arc<AppState>,
    virtual_id: VirtualId,
) -> Option<TimedStreamContext> {
    let config = app_state.app_config.config.load();
    let mins = config.sleep_timer_mins?;
    let duration_secs = u32::try_from((u64::from(mins) * 60).min(u64::from(u32::MAX))).unwrap_or(0);
    (duration_secs > 0).then(|| TimedStreamContext { app_state: Arc::clone(app_state), duration_secs, virtual_id })
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
            let Some(mode) = (match &self.state.send_custom_stream_flag {
                Some(flag) => StreamMode::try_from(flag.load(Ordering::Acquire)).ok(),
                None => Some(StreamMode::Inner),
            }) else {
                // An unknown mode can only come from a corrupted flag. Never map it
                // onto a live state such as `GracePending`; end the body safely.
                debug_assert!(false, "unknown stream mode flag");
                error!(
                    "Unknown stream mode for {}, terminating stream",
                    sanitize_sensitive_info(&self.state.fingerprint.addr.to_string())
                );
                self.state.terminate_quietly();
                return Poll::Ready(None);
            };

            // Dispatch based on the current streaming phase.
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
                                        let http_client = app_state.http_clients.default.load();
                                        http_client.as_ref().clone()
                                    };
                                    let lifecycle = self
                                        .state
                                        .provider_handle
                                        .as_ref()
                                        .and_then(ProviderStreamOpenLifecycle::from_managed);
                                    let future = Box::pin(async move {
                                        match open_provider_stream_with_lifecycle(
                                            &app_state.provider_stream_ctx(),
                                            &client,
                                            context.provider_stream_factory_options,
                                            lifecycle,
                                        )
                                        .await
                                        {
                                            Some(ProviderStreamOpen::Stream(response))
                                                if matches!(response.info, Some((_, _, _, Some(_)))) =>
                                            {
                                                let Some((_headers, _status, _response_url, Some(custom_video_type))) =
                                                    response.info
                                                else {
                                                    return DeferredProviderOpenOutcome::Failed;
                                                };
                                                ActiveClientStreamState::mode_for_custom_video_type(custom_video_type)
                                                    .map_or(
                                                        DeferredProviderOpenOutcome::Failed,
                                                        DeferredProviderOpenOutcome::Mode,
                                                    )
                                            }
                                            Some(ProviderStreamOpen::Stream(response)) => {
                                                DeferredProviderOpenOutcome::Stream(response.stream)
                                            }
                                            Some(ProviderStreamOpen::UpstreamStatus { .. }) | None => {
                                                DeferredProviderOpenOutcome::Failed
                                            }
                                        }
                                    });
                                    self.state.deferred_provider_open =
                                        Some(DeferredProviderOpenState::Opening(future));
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
                                        self.state.mark_direct_body_progress();
                                        continue;
                                    }
                                    Poll::Ready(DeferredProviderOpenOutcome::Mode(mode)) => {
                                        if !self.state.enter_custom_mode(mode) {
                                            return Poll::Ready(None);
                                        }
                                        continue;
                                    }
                                    Poll::Ready(DeferredProviderOpenOutcome::Failed) => {
                                        if !self.state.enter_custom_mode(StreamMode::ChannelUnavailable) {
                                            return Poll::Ready(None);
                                        }
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
                        Some(Poll::Ready(Some(Ok(bytes)))) => {
                            if !bytes.is_empty() {
                                self.state.confirm_lease_once();
                            }
                            self.state.mark_direct_body_progress();
                            return Poll::Ready(Some(Ok(bytes)));
                        }
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
                        Some(Poll::Pending) => {
                            if self.state.direct_body_idle_timeout.poll_expired(cx) {
                                self.state.stop_direct_body_idle_timeout();
                                return Poll::Ready(None);
                            }
                            return Poll::Pending;
                        }
                    }
                }

                // Quiet termination: the reentry guard declined a retry of a recently
                // evicted playback. End the body without a user-visible error video.
                StreamMode::ReentrySuppressed => {
                    info!(
                        "Suppressing reentry retry for {}, terminating stream",
                        sanitize_sensitive_info(&self.state.fingerprint.addr.to_string())
                    );
                    self.state.terminate_quietly();
                    return Poll::Ready(None);
                }

                // Custom video modes: serve the appropriate buffer
                video_mode => {
                    if self.state.custom_video_timeout_mode != Some(video_mode)
                        && !self.state.enter_custom_mode(video_mode)
                    {
                        return Poll::Ready(None);
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
    fn drop(&mut self) { self.state.release_stream_and_provider_handle_once(); }
}
