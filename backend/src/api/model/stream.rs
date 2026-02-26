use crate::{
    api::model::{CustomVideoStreamType, ProviderHandle, StreamError},
    model::GracePeriodOptions,
    tools::atomic_once_flag::AtomicOnceFlag,
};
use axum::http::StatusCode;
use bytes::Bytes;
use futures::stream::BoxStream;
use std::{collections::HashMap, sync::Arc};
use url::Url;

pub type BoxedProviderStream = BoxStream<'static, Result<Bytes, StreamError>>;
pub type ProviderStreamHeader = Vec<(String, String)>;
pub type ProviderStreamInfo = Option<(ProviderStreamHeader, StatusCode, Option<Url>, Option<CustomVideoStreamType>)>;

pub type ProviderStreamResponse = (Option<BoxedProviderStream>, ProviderStreamInfo);

pub type ProviderStreamFactoryResponse = (BoxedProviderStream, ProviderStreamInfo);

type StreamUrl = Arc<str>;
type ProviderName = Arc<str>;

pub enum ProviderStreamState {
    Custom(ProviderStreamResponse),
    Available(Option<ProviderName>, StreamUrl),
    GracePeriod(Option<ProviderName>, StreamUrl),
}

pub struct StreamDetails {
    pub stream: Option<BoxedProviderStream>,
    pub(crate) stream_info: ProviderStreamInfo,
    pub provider_name: Option<Arc<str>>,
    pub request_url: Option<Arc<str>>,
    pub grace_period: GracePeriodOptions,
    pub disable_provider_grace: bool,
    pub reconnect_flag: Option<Arc<AtomicOnceFlag>>,
    pub provider_handle: Option<ProviderHandle>,
}

impl StreamDetails {
    pub fn from_stream(stream: BoxedProviderStream, grace_period_options: GracePeriodOptions) -> Self {
        Self {
            stream: Some(stream),
            stream_info: None,
            provider_name: None,
            request_url: None,
            grace_period: grace_period_options,
            disable_provider_grace: false,
            reconnect_flag: None,
            provider_handle: None,
        }
    }
    #[inline]
    pub fn has_stream(&self) -> bool { self.stream.is_some() }

    #[inline]
    pub fn has_grace_period(&self) -> bool { self.grace_period.period_millis > 0 }
}

pub struct StreamingStrategy {
    pub provider_handle: Option<ProviderHandle>,
    pub provider_stream_state: ProviderStreamState,
    pub input_headers: Option<HashMap<String, String>>,
}
