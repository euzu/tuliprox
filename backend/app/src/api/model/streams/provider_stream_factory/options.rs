use super::{
    request::{merge_provider_request_headers, same_origin},
    ProviderResponseHeadAvailability, ProviderStreamFactoryFlags, ProviderStreamFactoryFlagsSet,
    ProviderStreamFactoryOptions,
};
use crate::{
    api::model::{get_header_filter_for_item_type, ProviderContentRepresentationMode},
    model::{ConfigProvider, ReverseProxyDisabledHeaderConfig},
    utils::request::{get_request_headers, preview_request_target_for_logging},
};
use reqwest::header::{HeaderMap, HeaderValue, RANGE};
use shared::{
    model::{FailureStage, PlaylistItemType, StreamChannel, StreamInfo},
    utils::is_sanitize_sensitive_info_enabled,
};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{atomic::AtomicU8, Arc},
};
use tokio_util::sync::CancellationToken;
use tuliprox_core::utils::request_headers::get_headers_from_request;
use tuliprox_hls::api::HlsOriginContentCodingObjectKind;
use tuliprox_session::stream_options::{StreamOptions, StreamResponseMode};
use url::Url;

pub(crate) struct ProviderStreamFactoryParams<'a> {
    pub addr: SocketAddr,
    pub item_type: PlaylistItemType,
    pub share_stream: bool,
    pub stream_options: &'a StreamOptions,
    pub stream_url: &'a Url,
    pub req_headers: &'a HeaderMap,
    pub input_headers: Option<&'a HashMap<String, String>>,
    pub session_headers: Option<&'a HashMap<String, String>>,
    pub disabled_headers: Option<&'a ReverseProxyDisabledHeaderConfig>,
    pub default_user_agent: Option<&'a str>,
    pub username: Option<&'a str>,
    pub client_ip: Option<&'a str>,
    pub stream_channel: Option<&'a StreamChannel>,
    pub connect_failure_stage: Option<FailureStage>,
    pub content_representation: ProviderContentRepresentationMode,
}

impl ProviderStreamFactoryOptions {
    pub(crate) fn new(request: &ProviderStreamFactoryParams<'_>) -> Self {
        let ProviderStreamFactoryParams {
            addr,
            item_type,
            share_stream,
            stream_options,
            stream_url,
            req_headers,
            input_headers,
            session_headers,
            disabled_headers,
            default_user_agent,
            username,
            client_ip,
            stream_channel,
            connect_failure_stage,
            content_representation,
        } = request;
        let buffer_size = if stream_options.buffer_enabled { stream_options.buffer_size } else { 0 };
        let buffer_max_bytes = stream_options.buffer_max_bytes;
        let user_agent = req_headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string);
        let filter_header = if stream_options.response_mode == StreamResponseMode::HlsResource {
            None
        } else {
            get_header_filter_for_item_type(*item_type)
        };
        let mut req_headers = get_headers_from_request(req_headers, &filter_header);
        let requested_range = req_headers.remove(RANGE.as_str()).and_then(|value| HeaderValue::from_bytes(&value).ok());

        let merged_input_headers = merge_provider_request_headers(*input_headers, *session_headers);

        // We merge configured input headers with the headers from the request.
        let mut headers = get_request_headers(
            merged_input_headers.as_ref(),
            Some(&req_headers),
            *disabled_headers,
            *default_user_agent,
        );
        crate::utils::request::overlay_upstream_user_agent(
            &mut headers,
            stream_channel.as_ref().and_then(|channel| channel.upstream_user_agent.as_deref()),
            *disabled_headers,
        );

        let default_user_agent = default_user_agent
            .and_then(|ua| {
                let trimmed = ua.trim();
                (!trimmed.is_empty()).then_some(trimmed)
            })
            .and_then(|ua| axum::http::header::HeaderValue::from_str(ua).ok());
        let url = (*stream_url).clone();
        let mut flags = ProviderStreamFactoryFlagsSet::new();
        if stream_options.stream_retry {
            flags.set(ProviderStreamFactoryFlags::RetryEnabled);
            if !item_type.is_live_adaptive() && stream_options.response_mode != StreamResponseMode::HlsResource {
                flags.set(ProviderStreamFactoryFlags::InitialRetryLoopEnabled);
            }
        }
        if stream_options.pipe_provider_stream {
            flags.set(ProviderStreamFactoryFlags::PipeStream);
        }
        if stream_options.buffer_enabled {
            flags.set(ProviderStreamFactoryFlags::BufferEnabled);
        }
        if stream_options.response_mode == StreamResponseMode::HlsResource {
            flags.set(ProviderStreamFactoryFlags::HlsResource);
        }
        if *share_stream {
            flags.set(ProviderStreamFactoryFlags::ShareStream);
        }
        if requested_range.is_some() {
            flags.set(ProviderStreamFactoryFlags::RangeRequested);
        }

        Self {
            item_type: *item_type,
            addr: *addr,
            flags,
            buffer_size,
            buffer_max_bytes,
            reconnect_flag: CancellationToken::new(),
            url,
            headers,
            default_user_agent,
            requested_range,
            provider: None,
            username: username.map(ToString::to_string),
            client_ip: client_ip.map(ToString::to_string),
            user_agent,
            stream_channel: stream_channel.cloned(),
            connect_failure_stage: *connect_failure_stage,
            content_representation: *content_representation,
            response_head_availability: ProviderResponseHeadAvailability::Available,
            hls_content_coding_object_kind: match *content_representation {
                ProviderContentRepresentationMode::Identity => Some(HlsOriginContentCodingObjectKind::Other),
                ProviderContentRepresentationMode::PreserveOrigin => None,
            },
            cancel_token: None,
            completion_token: None,
            close_reason: None,
            account: None,
        }
    }

    pub fn set_provider_handle_tokens(
        &mut self,
        cancel_token: Option<CancellationToken>,
        completion_token: Option<CancellationToken>,
        close_reason: Option<Arc<AtomicU8>>,
    ) {
        self.cancel_token = cancel_token;
        self.completion_token = completion_token;
        self.close_reason = close_reason;
    }

    pub fn get_cancel_token(&self) -> Option<CancellationToken> { self.cancel_token.clone() }

    pub fn get_completion_token(&self) -> Option<CancellationToken> { self.completion_token.clone() }

    pub fn get_close_reason(&self) -> Option<Arc<AtomicU8>> { self.close_reason.clone() }

    pub fn set_provider(&mut self, provider: Option<Arc<ConfigProvider>>) { self.provider = provider; }

    pub fn apply_user_agent_stream_index(&mut self, stream_index: u64) {
        crate::utils::request::append_user_agent_stream_index(&mut self.headers, stream_index);
    }

    pub fn require_public_destination(&mut self) {
        self.flags.set(ProviderStreamFactoryFlags::PublicDestinationRequired);
    }

    /// Applies per-input stream options that the generic factory parameters do not carry.
    pub fn apply_input_options(&mut self, input: &crate::model::ConfigInput) {
        if input.has_flag(crate::model::ConfigInputFlags::FlussonicHlsAudioTracks) {
            self.flags.set(ProviderStreamFactoryFlags::FlussonicAudioTracks);
        }
    }

    pub fn get_provider(&self) -> Option<&Arc<ConfigProvider>> { self.provider.as_ref() }

    #[inline]
    pub(super) fn is_piped(&self) -> bool { self.flags.contains(ProviderStreamFactoryFlags::PipeStream) }

    #[inline]
    pub(super) fn is_buffer_enabled(&self) -> bool { self.flags.contains(ProviderStreamFactoryFlags::BufferEnabled) }

    #[inline]
    pub(crate) fn get_buffer_size(&self) -> usize { self.buffer_size }

    #[inline]
    pub(crate) fn get_buffer_max_bytes(&self) -> usize { self.buffer_max_bytes }

    #[inline]
    pub fn get_reconnect_flag_clone(&self) -> CancellationToken { self.reconnect_flag.clone() }

    #[inline]
    pub fn cancel_reconnect(&self) { self.reconnect_flag.cancel(); }

    #[inline]
    pub fn get_url(&self) -> &Url { &self.url }

    #[inline]
    pub fn get_url_as_str(&self) -> &str { self.url.as_str() }

    #[inline]
    pub(super) fn get_item_type(&self) -> PlaylistItemType { self.item_type }

    #[inline]
    pub fn should_retry_provider_request(&self) -> bool {
        self.flags.contains(ProviderStreamFactoryFlags::RetryEnabled)
    }

    pub(super) fn requires_public_destination(&self) -> bool {
        self.flags.contains(ProviderStreamFactoryFlags::PublicDestinationRequired)
    }

    #[inline]
    pub fn should_retry_initial_open_loop(&self) -> bool {
        self.flags.contains(ProviderStreamFactoryFlags::InitialRetryLoopEnabled)
    }

    #[inline]
    pub fn get_headers(&self) -> &HeaderMap { &self.headers }

    #[inline]
    pub fn get_requested_range(&self) -> Option<&HeaderValue> { self.requested_range.as_ref() }

    #[inline]
    pub fn should_continue(&self) -> bool { !self.reconnect_flag.is_cancelled() }

    #[inline]
    pub fn was_range_requested(&self) -> bool { self.flags.contains(ProviderStreamFactoryFlags::RangeRequested) }

    pub(super) fn get_log_url(&self) -> std::borrow::Cow<'_, str> {
        if is_sanitize_sensitive_info_enabled() {
            return std::borrow::Cow::Borrowed(self.url.as_str());
        }

        std::borrow::Cow::Owned(preview_request_target_for_logging(&self.url, self.provider.as_ref()))
    }

    pub(super) fn build_connect_failed_stream_info(&self, provider_name: Arc<str>) -> Option<StreamInfo> {
        let username = self.username.as_deref()?;
        let client_ip = self.client_ip.as_deref()?;
        let stream_channel = self.stream_channel.clone()?;
        Some(StreamInfo::new(shared::model::StreamInfoParams {
            uid: 0,
            meter_uid: 0,
            username,
            addr: &self.addr,
            client_ip,
            provider: provider_name,
            stream_channel,
            user_agent: self.user_agent.clone().unwrap_or_default(),
            country_code: None,
            session_token: None,
        }))
    }

    pub(super) fn get_connect_failure_stage(&self) -> Option<FailureStage> { self.connect_failure_stage }

    #[inline]
    pub(crate) fn content_representation(&self) -> ProviderContentRepresentationMode { self.content_representation }

    /// Converts an open delayed until body polling to the only representation safe without an origin response head.
    pub(crate) fn for_deferred_open(mut self) -> Self {
        self.content_representation = ProviderContentRepresentationMode::Identity;
        self.response_head_availability = ProviderResponseHeadAvailability::Unavailable;
        self
    }

    #[cfg(test)]
    pub(crate) fn response_head_is_available(&self) -> bool {
        matches!(self.response_head_availability, ProviderResponseHeadAvailability::Available)
    }

    #[cfg(test)]
    pub(crate) fn hls_content_coding_object_kind(&self) -> Option<HlsOriginContentCodingObjectKind> {
        self.hls_content_coding_object_kind
    }
}

pub(super) fn should_wrap_provider_stream_in_buffer(stream_options: &ProviderStreamFactoryOptions) -> bool {
    !stream_options.is_piped()
        && !stream_options.flags.contains(ProviderStreamFactoryFlags::ShareStream)
        && stream_options.is_buffer_enabled()
}

// fn get_host_and_optional_port(url: &Url) -> Option<String> {
//     let host = url.host_str()?;
//     match url.port() {
//         Some(port) => Some(format!("{host}:{port}")),
//         None => Some(host.to_string()),
//     }
// }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ProviderRequestCredentialState {
    OriginalOrigin,
    Scrubbed,
}

impl ProviderRequestCredentialState {
    pub(super) fn observe_target(&mut self, original_url: &Url, target_url: &Url) {
        if !same_origin(original_url, target_url) {
            *self = Self::Scrubbed;
        }
    }
}
