use super::{CleanupEvent, OwnedRequestCleanup, RegisteredPlaybackRequest, PROVIDER_END_NOT_SET};
use shared::model::{StreamChannel, StreamInfo};
use std::{borrow::Cow, sync::Arc};
use tuliprox_core::model::Fingerprint;

pub struct ConnectionParams<'a> {
    pub meter_uid: u32,
    pub username: &'a str,
    pub max_connections: u32,
    pub soft_connections: u16,
    pub connection_kind: crate::active_provider_manager::ConnectionKind,
    pub priority: i8,
    pub soft_priority: i8,
    pub fingerprint: &'a Fingerprint,
    pub provider: Arc<str>,
    pub stream_channel: &'a StreamChannel,
    pub user_agent: Cow<'a, str>,
    pub session_token: Option<&'a str>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionHistoryMode {
    EmitConnect,
    RefreshOnly,
}

impl OwnedRequestCleanup {
    /// Releases the reserved cleanup right without emitting any release.
    pub(super) fn disarm(&mut self) {
        self.finished = true;
        self.permit = None;
    }

    /// Releases the request claim exactly once with the actual provider outcome.
    #[allow(clippy::too_many_arguments)]
    pub fn finish(
        &mut self,
        request_id: Option<tuliprox_core::model::PlaybackRequestId>,
        provider_end_reason: u8,
        reconnect_count: u8,
        provider_error_class: Option<&'static str>,
        provider_http_status: Option<u16>,
    ) {
        if self.finished {
            return;
        }
        self.finished = true;
        let Some(permit) = self.permit.take() else {
            return;
        };
        permit.send(CleanupEvent::ReleaseStream {
            request_id,
            owner: self.owner.clone(),
            addr: self.addr,
            stream_uid: Some(self.request_uid),
            provider_end_reason,
            reconnect_count,
            provider_error_class,
            provider_http_status,
        });
    }
}

impl Drop for OwnedRequestCleanup {
    fn drop(&mut self) {
        // Conservative fallback for a body that is dropped without an explicit finish:
        // never invent a successful EOF, but still finish the provider request with its
        // captured identity so an abandoned start does not linger until the startup TTL.
        self.finish(self.provider_request_id, PROVIDER_END_NOT_SET, 0, None, None);
    }
}

/// Why a playback request was rejected before a body could be created. Carried on
/// `RegisteredPlaybackRequest` so the caller can surface a non-success HTTP response
/// instead of silently dropping the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionRejectionReason {
    CleanupReceiverClosed,
    CleanupAdmissionTimeout,
    RegistrationFailed,
}

impl std::fmt::Display for ConnectionRejectionReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CleanupReceiverClosed => write!(f, "cleanup receiver closed"),
            Self::CleanupAdmissionTimeout => write!(f, "cleanup admission timed out"),
            Self::RegistrationFailed => write!(f, "connection registration failed"),
        }
    }
}

impl std::error::Error for ConnectionRejectionReason {}

impl std::fmt::Debug for RegisteredPlaybackRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegisteredPlaybackRequest")
            .field("request_uid", &self.request_uid)
            .field("display_stream", &self.display_stream)
            .field("rejection", &self.rejection)
            .finish_non_exhaustive()
    }
}

impl RegisteredPlaybackRequest {
    #[inline]
    pub fn new(request_uid: u32, display_stream: Option<StreamInfo>) -> Self {
        Self { request_uid, display_stream, cleanup: None, rejection: None }
    }

    #[inline]
    pub(super) fn rejected(request_uid: u32, reason: ConnectionRejectionReason) -> Self {
        Self { request_uid, display_stream: None, cleanup: None, rejection: Some(reason) }
    }

    #[inline]
    pub(super) fn with_cleanup(
        request_uid: u32,
        display_stream: Option<StreamInfo>,
        cleanup: Option<OwnedRequestCleanup>,
    ) -> Self {
        Self { request_uid, display_stream, cleanup, rejection: None }
    }

    /// Marks the request as taken over by a path without a body (compatibility API or
    /// shared source), disabling rollback on drop.
    #[inline]
    pub fn commit(&mut self) {
        if let Some(mut cleanup) = self.cleanup.take() {
            cleanup.disarm();
        }
    }

    /// Transfers the guaranteed cleanup right into the body. The body owns the returned
    /// value and must finish it exactly once with the real provider outcome.
    #[inline]
    pub fn into_body_cleanup(&mut self) -> Option<OwnedRequestCleanup> { self.cleanup.take() }

    #[inline]
    pub fn display_uid(&self) -> Option<u32> { self.display_stream.as_ref().map(|s| s.uid) }

    #[inline]
    pub fn rejection_reason(&self) -> Option<ConnectionRejectionReason> { self.rejection }

    #[inline]
    pub fn into_display_stream(self) -> Option<StreamInfo> { self.display_stream }
}
