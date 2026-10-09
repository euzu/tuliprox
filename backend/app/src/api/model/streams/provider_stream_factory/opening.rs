use super::{
    error::{record_provider_open_failure, ProviderStreamRequestFailure},
    options::should_wrap_provider_stream_in_buffer,
    request::get_provider_stream,
    ProviderOpenGuard, ProviderStreamFactoryFlags, ProviderStreamFactoryOptions, ProviderStreamOpenLifecycle,
};
use crate::api::model::{
    create_channel_unavailable_stream, get_response_headers, streams::client_stream::ClientStream,
    CustomVideoStreamType, ProviderStreamFactoryResponse,
};
use futures::StreamExt;
use reqwest::StatusCode;
use shared::model::ConnectFailureReason;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tuliprox_session::{
    stream_ctx::ProviderStreamCtx,
    streams::{ProviderBodyOwner, ProviderBodyOwnerConfig},
    ManagedProviderHandle, ProviderSessionHeaders,
};

/// Result of opening a provider stream.
pub enum ProviderStreamOpen {
    Stream(ProviderStreamFactoryResponse),
    /// A finite HLS resource keeps the upstream error status and its range and retry headers
    /// instead of receiving a fallback body.
    UpstreamStatus {
        status: StatusCode,
        headers: Vec<(String, String)>,
    },
}

#[allow(clippy::too_many_lines)]
pub async fn create_provider_stream(
    ctx: &ProviderStreamCtx,
    client: &reqwest::Client,
    stream_options: ProviderStreamFactoryOptions,
) -> Option<ProviderStreamOpen> {
    let mut open_guard = ProviderOpenGuard { token: stream_options.get_completion_token(), handed_off: false };
    match get_provider_stream(ctx, client, &stream_options).await {
        Ok(Some(ProviderStreamFactoryResponse { stream: init_stream, info, provider_session_headers, .. })) => {
            let continue_signal = stream_options.get_reconnect_flag_clone();
            if let Some((_headers, _status, _response_url, Some(custom_video_type))) = &info {
                let reason = match custom_video_type {
                    CustomVideoStreamType::ChannelUnavailable => Some(ConnectFailureReason::ChannelUnavailable),
                    CustomVideoStreamType::Provisioning => Some(ConnectFailureReason::Provisioning),
                    CustomVideoStreamType::ProviderConnectionsExhausted => {
                        Some(ConnectFailureReason::ProviderConnectionsExhausted)
                    }
                    _ => None,
                };
                if let Some(reason) = reason {
                    record_provider_open_failure(ctx, &stream_options, reason, None, None);
                }
                return Some(ProviderStreamOpen::Stream(ProviderStreamFactoryResponse {
                    stream: ClientStream::new(init_stream, continue_signal, None, stream_options.get_url_as_str())
                        .boxed(),
                    info,
                    provider_session_headers,
                    has_upstream_owner: false,
                }));
            }
            let handle_cancel = stream_options.get_cancel_token();
            let completion_token = stream_options.get_completion_token();
            let close_reason = stream_options.get_close_reason();

            let cancel_token = if let Some(hc) = handle_cancel {
                let combined = CancellationToken::new();
                let c1 = combined.clone();
                let c2 = combined.clone();
                let hc_clone = hc.clone();
                let cs_clone = continue_signal.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        biased;
                        () = hc_clone.cancelled() => {
                            cs_clone.cancel();
                            c1.cancel();
                        }
                        () = cs_clone.cancelled() => {
                            c2.cancel();
                        }
                    }
                });
                combined
            } else {
                continue_signal.clone()
            };

            let body_owner_config = if should_wrap_provider_stream_in_buffer(&stream_options) {
                ProviderBodyOwnerConfig::for_buffered(
                    stream_options.get_buffer_size(),
                    stream_options.get_buffer_max_bytes(),
                )
            } else {
                ProviderBodyOwnerConfig::for_direct_body()
            };

            open_guard.handed_off = true;
            let stream = ProviderBodyOwner::new(
                init_stream.boxed(),
                body_owner_config,
                cancel_token,
                completion_token,
                close_reason,
            )
            .boxed();

            Some(ProviderStreamOpen::Stream(ProviderStreamFactoryResponse {
                stream: ClientStream::new(stream, continue_signal.clone(), None, stream_options.get_url_as_str())
                    .boxed(),
                info,
                provider_session_headers,
                has_upstream_owner: true,
            }))
        }
        Ok(None) => None,
        Err(failure) => {
            let status = failure.status();
            record_provider_open_failure(
                ctx,
                &stream_options,
                ConnectFailureReason::ChannelUnavailable,
                Some(status),
                Some(failure.provider_error_class()),
            );
            if stream_options.flags.contains(ProviderStreamFactoryFlags::HlsResource) {
                let headers = match failure {
                    ProviderStreamRequestFailure::HlsStatus { headers, .. } => headers,
                    ProviderStreamRequestFailure::Status { .. } => Vec::new(),
                };
                return Some(ProviderStreamOpen::UpstreamStatus { status, headers });
            }
            if let (Some(boxed_provider_stream), response_info) = create_channel_unavailable_stream(
                &ctx.app_config,
                &get_response_headers(stream_options.get_headers()),
                StatusCode::OK,
            ) {
                return Some(ProviderStreamOpen::Stream(ProviderStreamFactoryResponse {
                    stream: boxed_provider_stream,
                    info: response_info,
                    provider_session_headers: ProviderSessionHeaders::default(),
                    has_upstream_owner: false,
                }));
            }
            None
        }
    }
}

impl ProviderStreamOpenLifecycle {
    pub fn from_managed(managed: &ManagedProviderHandle) -> Option<Self> {
        let handle = managed.handle()?;
        Some(Self {
            manager: Arc::clone(managed.manager()),
            allocation_id: handle.allocation_id,
            cancel_token: handle.cancel_token.clone(),
            completion_token: handle.completion_token.clone(),
            close_reason: Some(Arc::clone(&handle.close_reason)),
            account: handle.allocation.get_provider_config(),
        })
    }

    pub fn mark_opening(&self) { self.manager.mark_opening(self.allocation_id); }

    pub fn register_body_owner(&self) -> bool { self.manager.register_body_owner(self.allocation_id) }
}

pub async fn open_provider_stream_with_lifecycle(
    ctx: &ProviderStreamCtx,
    client: &reqwest::Client,
    mut stream_options: ProviderStreamFactoryOptions,
    lifecycle: Option<ProviderStreamOpenLifecycle>,
) -> Option<ProviderStreamOpen> {
    if let Some(ref lc) = lifecycle {
        lc.mark_opening();
        stream_options.account.clone_from(&lc.account);
        stream_options.set_provider_handle_tokens(
            lc.cancel_token.clone(),
            lc.completion_token.clone(),
            lc.close_reason.clone(),
        );
    }
    let open = create_provider_stream(ctx, client, stream_options).await?;
    if let (ProviderStreamOpen::Stream(response), Some(lc)) = (&open, lifecycle.as_ref()) {
        if response.has_upstream_owner && !lc.register_body_owner() {
            return None;
        }
    }
    Some(open)
}
