use crate::{api::model::ProviderContentRepresentationMode, model::ConfigProvider};
use reqwest::header::{HeaderMap, HeaderValue};
use shared::{
    create_bitset,
    model::{FailureStage, PlaylistItemType, StreamChannel},
};
use std::{
    net::SocketAddr,
    sync::{atomic::AtomicU8, Arc},
};
use tokio_util::sync::CancellationToken;
use tuliprox_core::model::AllocationId;
use tuliprox_hls::api::HlsOriginContentCodingObjectKind;
use tuliprox_session::ActiveProviderManager;
use url::Url;

const RETRY_SECONDS: u64 = 5;

const ERR_MAX_RETRY_COUNT: u32 = 5;

/// Describes whether the provider response head is available before Tuliprox commits the client response head.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderResponseHeadAvailability {
    Available,
    Unavailable,
}

#[derive(Clone, Copy, Debug)]
struct ProviderStreamPreparationContext {
    representation: ProviderContentRepresentationMode,
    response_head_availability: ProviderResponseHeadAvailability,
    range_requested: bool,
    hls_content_coding_object_kind: Option<HlsOriginContentCodingObjectKind>,
}

create_bitset!(
    u16,
    ProviderStreamFactoryFlags,
    RetryEnabled,
    InitialRetryLoopEnabled,
    BufferEnabled,
    ShareStream,
    PipeStream,
    RangeRequested,
    PublicDestinationRequired,
    HlsResource,
    FlussonicAudioTracks
);

#[derive(Debug, Clone)]
pub struct ProviderStreamFactoryOptions {
    addr: SocketAddr,
    item_type: PlaylistItemType,
    flags: ProviderStreamFactoryFlagsSet,
    buffer_size: usize,
    buffer_max_bytes: usize,
    url: Url,
    headers: HeaderMap,
    default_user_agent: Option<axum::http::header::HeaderValue>,
    requested_range: Option<HeaderValue>,
    reconnect_flag: CancellationToken,
    provider: Option<Arc<ConfigProvider>>,
    username: Option<String>,
    client_ip: Option<String>,
    user_agent: Option<String>,
    stream_channel: Option<StreamChannel>,
    connect_failure_stage: Option<FailureStage>,
    content_representation: ProviderContentRepresentationMode,
    response_head_availability: ProviderResponseHeadAvailability,
    hls_content_coding_object_kind: Option<HlsOriginContentCodingObjectKind>,
    cancel_token: Option<CancellationToken>,
    completion_token: Option<CancellationToken>,
    close_reason: Option<Arc<AtomicU8>>,
    account: Option<Arc<crate::model::ProviderConfig>>,
}

struct ProviderOpenGuard {
    token: Option<CancellationToken>,
    handed_off: bool,
}

#[derive(Clone)]
pub struct ProviderStreamOpenLifecycle {
    manager: Arc<ActiveProviderManager>,
    allocation_id: AllocationId,
    cancel_token: Option<CancellationToken>,
    completion_token: Option<CancellationToken>,
    close_reason: Option<Arc<AtomicU8>>,
    account: Option<Arc<crate::model::ProviderConfig>>,
}

#[cfg(test)]
mod tests;

mod error;
mod opening;
mod options;
mod request;
mod response;
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub use opening::{create_provider_stream, open_provider_stream_with_lifecycle, ProviderStreamOpen};
pub(crate) use options::ProviderStreamFactoryParams;
#[cfg(test)]
use request::{prepare_client, provider_headers_require_manual_redirects, send_with_manual_redirects};
#[cfg(test)]
use response::normalize_hls_resource_content_type;
#[cfg(test)]
use response::prepare_provider_stream_response;
#[cfg(test)]
use response::prepare_provider_stream_response_with_idle_timeout;
