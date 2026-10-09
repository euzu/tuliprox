use crate::{
    api::model::{AppState, StreamDetails},
    auth::Fingerprint,
    model::ProxyUserCredentials,
};
use axum::http::HeaderMap;
use shared::model::{StreamChannel, UserConnectionPermission};
use std::sync::Arc;

const BODY_IDLE_TIMEOUT_ERROR_CLASS: &str = "body_idle_timeout";

const DIRECT_BODY_SOCKET_ACTIVITY_TOUCH_SECS: u64 = 1;

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

pub(in crate::api) struct ActiveClientStream {
    state: ActiveClientStreamState,
}

#[cfg(test)]
mod tests;

mod admission;
mod error;
mod grace;
mod provider_open;
mod state;
mod stream;

pub(crate) use self::{admission::create_active_client_stream, error::StreamAdmissionError};
use self::{
    admission::{ActiveClientStreamState, GracePeriodParams},
    grace::{resolve_grace_period_provisioning, stream_grace_period},
    provider_open::{create_deferred_provider_open_future, DeferredProviderOpenOutcome, DeferredProviderOpenState},
    state::{
        should_use_direct_body_idle_timeout, store_stream_mode, CustomVideoBuffers, DirectBodyIdleTimeout,
        GraceProvisioningInfo, StreamMode, TimedStreamContext,
    },
    stream::{create_timed_stream_context, wrap_timed_client_stream_if_needed},
};
