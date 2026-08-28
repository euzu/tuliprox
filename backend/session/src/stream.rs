use axum::http::StatusCode;
use bytes::Bytes;
use futures::stream::BoxStream;
use shared::{
    defaults::HLS_EXT,
    model::{CustomVideoStreamType, PlaylistItemType, StreamChannel},
};
use std::{collections::HashMap, sync::Arc};
use tokio_util::sync::CancellationToken;
use tuliprox_core::model::{GracePeriodOptions, ProviderHandle, StreamError};
use url::Url;

pub type BoxedProviderStream = BoxStream<'static, Result<Bytes, StreamError>>;
pub type ProviderStreamHeader = Vec<(String, String)>;
pub type ProviderStreamInfo = Option<(ProviderStreamHeader, StatusCode, Option<Url>, Option<CustomVideoStreamType>)>;

pub type ProviderStreamResponse = (Option<BoxedProviderStream>, ProviderStreamInfo);

pub struct ProviderStreamFactoryResponse {
    pub stream: BoxedProviderStream,
    pub info: ProviderStreamInfo,
    pub provider_session_headers: HashMap<String, String>,
}

/// Controls whether a provider stream preserves its origin representation or normalizes it to identity bytes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProviderContentRepresentationMode {
    #[default]
    PreserveOrigin,
    Identity,
}

impl ProviderContentRepresentationMode {
    pub fn for_playback_extension(extension: &str) -> Self {
        if extension.eq_ignore_ascii_case(HLS_EXT) {
            Self::Identity
        } else {
            Self::PreserveOrigin
        }
    }
}

pub fn uses_direct_body_idle_timeout(stream_channel: &StreamChannel) -> bool {
    !stream_channel.shared
        && matches!(
            stream_channel.item_type,
            PlaylistItemType::Video
                | PlaylistItemType::Series
                | PlaylistItemType::LocalVideo
                | PlaylistItemType::LocalSeries
        )
}

type StreamUrl = Arc<str>;
type ProviderName = Arc<str>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderStreamCustomReason {
    ProviderExhausted,
    UnmappedProviderUrl,
}

pub enum ProviderStreamState {
    Custom { response: ProviderStreamResponse, reason: ProviderStreamCustomReason },
    Available(Option<ProviderName>, StreamUrl),
    GracePeriod(Option<ProviderName>, StreamUrl),
}

pub struct StreamDetails {
    pub stream: Option<BoxedProviderStream>,
    pub stream_info: ProviderStreamInfo,
    pub provider_name: Option<Arc<str>>,
    pub request_url: Option<Arc<str>>,
    pub session_headers: Option<HashMap<String, String>>,
    pub provider_session_headers: HashMap<String, String>,
    pub grace_period: GracePeriodOptions,
    pub provider_grace_active: bool,
    pub disable_provider_grace: bool,
    pub reconnect_flag: Option<CancellationToken>,
    pub provider_handle: Option<ProviderHandle>,
    pub content_representation: ProviderContentRepresentationMode,
    /// Set when the stream was admitted via a user-grace strategy. Carried through to
    /// `stream_grace_period` so remaining strategies can be evaluated if the grace fails.
    pub grace_resolution_context: Option<crate::GraceResolutionContext>,
}

/// Manual Clone: stream cannot be cloned so we set it to None on the clone.
/// This is safe because `StreamDetails` is only cloned in contexts where the
/// stream has already been moved out (e.g., constructing grace params).
impl Clone for StreamDetails {
    fn clone(&self) -> Self {
        Self {
            stream: None,
            stream_info: self.stream_info.clone(),
            provider_name: self.provider_name.clone(),
            request_url: self.request_url.clone(),
            session_headers: self.session_headers.clone(),
            provider_session_headers: self.provider_session_headers.clone(),
            grace_period: self.grace_period,
            provider_grace_active: self.provider_grace_active,
            disable_provider_grace: self.disable_provider_grace,
            reconnect_flag: self.reconnect_flag.clone(),
            provider_handle: self.provider_handle.clone(),
            content_representation: self.content_representation,
            grace_resolution_context: self.grace_resolution_context.clone(),
        }
    }
}

impl StreamDetails {
    pub fn from_stream(stream: BoxedProviderStream, grace_period_options: GracePeriodOptions) -> Self {
        Self {
            stream: Some(stream),
            stream_info: None,
            provider_name: None,
            request_url: None,
            session_headers: None,
            provider_session_headers: HashMap::new(),
            grace_period: grace_period_options,
            provider_grace_active: false,
            disable_provider_grace: false,
            reconnect_flag: None,
            provider_handle: None,
            content_representation: ProviderContentRepresentationMode::PreserveOrigin,
            grace_resolution_context: None,
        }
    }
    #[inline]
    pub fn has_stream(&self) -> bool {
        self.stream.is_some()
    }

    #[inline]
    pub fn has_grace_period(&self) -> bool {
        self.grace_period.period_millis > 0
    }

    #[inline]
    pub fn has_deferred_provider_open(&self) -> bool {
        self.stream.is_none()
            && self.provider_grace_active
            && self.grace_period.hold_stream
            && self.provider_handle.is_some()
            && self.provider_name.is_some()
            && self.request_url.is_some()
    }
}

pub struct StreamingStrategy {
    pub provider_handle: Option<ProviderHandle>,
    pub provider_stream_state: ProviderStreamState,
    pub input_headers: Option<HashMap<String, String>>,
}

#[cfg(test)]
mod tests {
    use super::ProviderContentRepresentationMode;

    #[test]
    fn provider_representation_mode_uses_hls_extension_not_playlist_item_type() {
        assert_eq!(
            ProviderContentRepresentationMode::for_playback_extension(".m3u8"),
            ProviderContentRepresentationMode::Identity
        );
        assert_eq!(
            ProviderContentRepresentationMode::for_playback_extension(".M3U8"),
            ProviderContentRepresentationMode::Identity
        );
        for extension in [".ts", ".mp4", ".mkv", ""] {
            assert_eq!(
                ProviderContentRepresentationMode::for_playback_extension(extension),
                ProviderContentRepresentationMode::PreserveOrigin
            );
        }
    }
}
