use super::{store_stream_mode, ActiveClientStreamState, StreamMode};
use crate::{
    api::{
        model::{
            connection_manager::PROVIDER_END_PREEMPTED, AppState, BoxedProviderStream, CleanupEvent,
            CustomVideoStreamType, MeteringStream, ProviderStreamFactoryOptions, StreamDetails, TimedClientStream,
        },
        panel_api::find_input_by_provider_name,
    },
    auth::Fingerprint,
    utils::debug_if_enabled,
};
use axum::http::HeaderMap;
use futures::{Future, StreamExt};
use shared::{
    model::{FailureStage, StreamChannel},
    utils::sanitize_sensitive_info,
};
use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
};
use tuliprox_session::stream_options::{get_stream_options, StreamResponseMode};

pub(super) struct DeferredProviderOpenContext {
    pub(super) app_state: Arc<AppState>,
    pub(super) provider_stream_factory_options: ProviderStreamFactoryOptions,
}

pub(super) enum DeferredProviderOpenOutcome {
    Stream(BoxedProviderStream),
    Mode(StreamMode),
    Failed,
}

pub(super) enum DeferredProviderOpenState {
    Pending(Box<DeferredProviderOpenContext>),
    Opening(Pin<Box<dyn Future<Output = DeferredProviderOpenOutcome> + Send>>),
}

impl ActiveClientStreamState {
    pub(super) fn wrap_provider_stream(&self, stream: BoxedProviderStream) -> BoxedProviderStream {
        let stream = if let Some(meter) = &self.meter {
            MeteringStream::new(stream, Arc::clone(meter), Arc::clone(&self.event_manager)).boxed()
        } else {
            stream
        };
        if let Some(ctx) = self.timed_stream_context.as_ref() {
            TimedClientStream::new(
                &ctx.app_state.app_config,
                &ctx.app_state.connection_manager,
                stream,
                ctx.duration_secs,
                self.fingerprint.addr,
                ctx.virtual_id,
                self.stream_uid,
            )
            .boxed()
        } else {
            stream
        }
    }

    pub(super) fn stop_provider_stream_preempted(&mut self) -> bool {
        if self.response_mode == StreamResponseMode::HlsResource {
            self.provider_end_reason.store(PROVIDER_END_PREEMPTED, Ordering::Relaxed);
            self.provider_error_class = Some("preempted");
            self.terminate_quietly();
            return false;
        }
        self.provider_stopped = true;
        self.preempt_cancelled = None;
        self.stop_grace_task();
        self.provider_end_reason.store(PROVIDER_END_PREEMPTED, Ordering::Relaxed);
        self.provider_error_class = Some("preempted");

        let is_superseded = self
            .provider_handle
            .as_ref()
            .and_then(|m| m.handle())
            .is_some_and(|h| h.get_close_reason() == tuliprox_core::model::ProviderCloseReason::Superseded);

        let mut serve_preempted_custom = false;
        if self.provider_handle.is_some() {
            let managed = self.provider_handle.take();
            self.provider_handle_released = true;
            // Synchronous provider-slot release; the custom-video detail update is a
            // separate, best-effort UI effect.
            drop(managed);
            if !is_superseded && self.custom_video.low_priority_preempted.is_some() {
                serve_preempted_custom = true;
                if let Some(flag) = &self.send_custom_stream_flag {
                    store_stream_mode(flag, StreamMode::LowPriorityPreempted);
                } else {
                    // Fallback: create_active_client_stream usually initializes this via stream_grace_period.
                    self.send_custom_stream_flag =
                        Some(Arc::new(AtomicU8::new(StreamMode::LowPriorityPreempted as u8)));
                }
            } else if let Some(flag) = &self.send_custom_stream_flag {
                store_stream_mode(flag, StreamMode::Inner);
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
                    stream_uid: self.stream_uid,
                    video_type: CustomVideoStreamType::LowPriorityPreempted,
                    handle: None,
                });
            } else {
                self.release_user_stream();
            }
        }
        serve_preempted_custom
    }

    pub(super) fn stop_provider_stream(&mut self, mode: StreamMode) {
        self.provider_stopped = true;
        self.preempt_cancelled = None;
        self.stop_grace_task();

        if self.provider_handle.is_some() {
            // Synchronous provider-slot release; the custom-video detail update is a
            // separate, best-effort UI effect.
            self.release_provider_handle_and_detach_body();

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
            let reason = match mode {
                StreamMode::ChannelUnavailable => "unavailable provider channel",
                StreamMode::UserExhausted => "user grace period exhaustion",
                StreamMode::ProviderExhausted => "provider grace period exhaustion",
                StreamMode::Provisioning => "provider grace period provisioning",
                StreamMode::LowPriorityPreempted => "low-priority preemption",
                StreamMode::ReentrySuppressed => "reentry suppression",
                StreamMode::Inner | StreamMode::GracePending => "stream mode transition",
            };
            debug_if_enabled!(
                "Provider stream stopped due to {reason} for {}",
                sanitize_sensitive_info(&addr.to_string())
            );
            if let Some(video_type) = Self::custom_video_type_for_mode(mode) {
                self.connection_manager.send_cleanup(CleanupEvent::UpdateDetailAndReleaseProvider {
                    addr,
                    stream_uid: self.stream_uid,
                    video_type,
                    handle: None,
                });
            } else {
                // No custom video for this mode: release the request without publishing
                // a detail update so no error clip is shown.
                self.release_user_stream();
            }
        }
    }
}

pub(super) fn create_deferred_provider_open_future(
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
    let stream_options = get_stream_options(&app_state.app_config, stream_details.response_mode);
    let default_user_agent = app_state.app_config.config.load().default_user_agent.clone();
    let disabled_headers = app_state.get_disabled_headers();
    let mut provider_stream_factory_options =
        ProviderStreamFactoryOptions::new(&crate::api::model::ProviderStreamFactoryParams {
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
            content_representation: stream_details.content_representation,
        })
        .for_deferred_open();
    if stream_details.provider_handle.is_some() {
        if let Some(stream_index) = stream_details.user_agent_stream_index {
            provider_stream_factory_options.apply_user_agent_stream_index(stream_index);
        }
    }
    provider_stream_factory_options.set_provider(input.get_resolve_provider(stream_url.as_ref()));
    provider_stream_factory_options.apply_input_options(&input);

    Some(DeferredProviderOpenState::Pending(Box::new(DeferredProviderOpenContext {
        app_state: Arc::clone(app_state),
        provider_stream_factory_options,
    })))
}
