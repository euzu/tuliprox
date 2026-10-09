use super::{ActiveClientStreamState, BODY_IDLE_TIMEOUT_ERROR_CLASS, DIRECT_BODY_SOCKET_ACTIVITY_TOUCH_SECS};
use crate::{
    api::model::{
        connection_manager::{PROVIDER_END_ERROR, PROVIDER_END_NOT_SET},
        uses_direct_body_idle_timeout, AppState, CustomVideoStreamType, TransportStreamBuffer,
        DIRECT_BODY_IDLE_TIMEOUT_SECS,
    },
    model::ConfigInput,
};
use futures::Future;
use log::{info, warn};
use shared::{
    model::{StreamChannel, VirtualId},
    utils::sanitize_sensitive_info,
};
use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
    task::Context,
};
use tokio_util::sync::CancellationToken;
use tuliprox_session::stream_options::StreamResponseMode;

/// Discriminates which byte-stream the client is consuming at any moment.
/// Stored as `u8` in an `AtomicU8` for lock-free access inside `poll_next`.
/// Lower numeric values correspond to a live or custom stream; `GracePending`
/// (255) is a transient sentinel that parks the poll until the grace task resolves.
///
/// Discriminants are implicit (declaration order) except for the `255` sentinel.
/// The byte mapping lives once, in [`StreamMode::try_from`], and is pinned by
/// `test_stream_mode_byte_values`.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StreamMode {
    /// Forward bytes directly from the upstream provider.
    Inner,
    /// Show the "user connections exhausted" custom video.
    UserExhausted,
    /// Show the "provider connections exhausted" custom video.
    ProviderExhausted,
    /// Show the "channel unavailable" custom video.
    ChannelUnavailable,
    /// Show the provisioning/placeholder custom video while probing for capacity.
    Provisioning,
    /// Show the "low-priority preempted" custom video.
    LowPriorityPreempted,
    /// A recently evicted playback retried while this stream held a grace slot and
    /// every remaining eviction candidate is reentry-protected. The body must end
    /// without painting a user-visible "connections exhausted" error video.
    ReentrySuppressed,
    /// Transient: grace-period check is still in progress; `poll_next` must park.
    GracePending = 255,
}

impl TryFrom<u8> for StreamMode {
    type Error = u8;

    /// Decodes the atomic mode flag. Returns `Err(value)` for a byte that was never a
    /// valid [`StreamMode`]; callers must fail safe instead of inventing a live state
    /// such as `GracePending`.
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Inner),
            1 => Ok(Self::UserExhausted),
            2 => Ok(Self::ProviderExhausted),
            3 => Ok(Self::ChannelUnavailable),
            4 => Ok(Self::Provisioning),
            5 => Ok(Self::LowPriorityPreempted),
            6 => Ok(Self::ReentrySuppressed),
            255 => Ok(Self::GracePending),
            other => Err(other),
        }
    }
}

/// Publishes a new mode to the lock-free flag with release ordering. All writers go
/// through this so the `as u8` cast and the memory ordering live in one place.
pub(super) fn store_stream_mode(flag: &AtomicU8, mode: StreamMode) { flag.store(mode as u8, Ordering::Release); }

/// Holds the optional custom video buffers for each error/placeholder scenario.
/// Using named fields avoids the positional-indexing confusion of a 4-tuple.
pub(super) struct CustomVideoBuffers {
    pub(super) user_exhausted: Option<TransportStreamBuffer>,
    pub(super) provider_exhausted: Option<TransportStreamBuffer>,
    pub(super) unavailable: Option<TransportStreamBuffer>,
    pub(super) provisioning: Option<TransportStreamBuffer>,
    pub(super) low_priority_preempted: Option<TransportStreamBuffer>,
}

pub(super) struct GraceProvisioningInfo {
    pub(super) input: Arc<ConfigInput>,
    pub(super) stop_signal: CancellationToken,
}

#[derive(Clone)]
pub(super) struct TimedStreamContext {
    pub(super) app_state: Arc<AppState>,
    pub(super) duration_secs: u32,
    pub(super) virtual_id: VirtualId,
}

pub(super) struct DirectBodyIdleTimeout {
    pub(super) enabled: bool,
    pub(super) deadline: Option<tokio::time::Instant>,
    pub(super) sleep: Option<Pin<Box<tokio::time::Sleep>>>,
    pub(super) last_socket_activity_touch: Option<tokio::time::Instant>,
}

impl DirectBodyIdleTimeout {
    pub(super) const fn disabled() -> Self {
        Self { enabled: false, deadline: None, sleep: None, last_socket_activity_touch: None }
    }

    pub(super) const fn enabled() -> Self {
        Self { enabled: true, deadline: None, sleep: None, last_socket_activity_touch: None }
    }

    pub(super) fn mark_progress(&mut self) -> bool {
        if !self.enabled {
            return false;
        }

        let now = tokio::time::Instant::now();
        self.deadline = Some(now + tokio::time::Duration::from_secs(DIRECT_BODY_IDLE_TIMEOUT_SECS));
        self.sleep = None;

        let should_touch_socket = self.last_socket_activity_touch.is_none_or(|last_touch| {
            now.duration_since(last_touch) >= tokio::time::Duration::from_secs(DIRECT_BODY_SOCKET_ACTIVITY_TOUCH_SECS)
        });
        if should_touch_socket {
            self.last_socket_activity_touch = Some(now);
        }
        should_touch_socket
    }

    pub(super) fn poll_expired(&mut self, cx: &mut Context<'_>) -> bool {
        if !self.enabled {
            return false;
        }

        let deadline = *self.deadline.get_or_insert_with(|| {
            tokio::time::Instant::now() + tokio::time::Duration::from_secs(DIRECT_BODY_IDLE_TIMEOUT_SECS)
        });
        if tokio::time::Instant::now() >= deadline {
            return true;
        }

        self.sleep.get_or_insert_with(|| Box::pin(tokio::time::sleep_until(deadline)));
        self.sleep.as_mut().is_some_and(|sleep| sleep.as_mut().poll(cx).is_ready())
    }
}

impl ActiveClientStreamState {
    pub(super) fn mode_for_custom_video_type(video_type: CustomVideoStreamType) -> Option<StreamMode> {
        match video_type {
            CustomVideoStreamType::ChannelUnavailable => Some(StreamMode::ChannelUnavailable),
            CustomVideoStreamType::UserConnectionsExhausted => Some(StreamMode::UserExhausted),
            CustomVideoStreamType::ProviderConnectionsExhausted => Some(StreamMode::ProviderExhausted),
            CustomVideoStreamType::LowPriorityPreempted => Some(StreamMode::LowPriorityPreempted),
            CustomVideoStreamType::Provisioning => Some(StreamMode::Provisioning),
            CustomVideoStreamType::UserAccountExpired | CustomVideoStreamType::HlsSessionOrLeaseExpired => None,
        }
    }

    /// Maps a mode to the custom video it serves, if any. `ReentrySuppressed` and the
    /// transparent forwarding modes deliberately have no custom video: a suppressed
    /// reentry must never surface a user-visible error clip.
    pub(super) fn custom_video_type_for_mode(mode: StreamMode) -> Option<CustomVideoStreamType> {
        match mode {
            StreamMode::UserExhausted => Some(CustomVideoStreamType::UserConnectionsExhausted),
            StreamMode::ProviderExhausted => Some(CustomVideoStreamType::ProviderConnectionsExhausted),
            StreamMode::Provisioning => Some(CustomVideoStreamType::Provisioning),
            StreamMode::LowPriorityPreempted => Some(CustomVideoStreamType::LowPriorityPreempted),
            StreamMode::ChannelUnavailable => Some(CustomVideoStreamType::ChannelUnavailable),
            StreamMode::Inner | StreamMode::GracePending | StreamMode::ReentrySuppressed => None,
        }
    }

    pub(super) fn stop_direct_body_idle_timeout(&mut self) {
        self.provider_stopped = true;
        self.preempt_cancelled = None;
        self.inner = None;
        self.provider_error_class = Some(BODY_IDLE_TIMEOUT_ERROR_CLASS);
        self.provider_http_status = None;
        let _ = self.provider_end_reason.compare_exchange(
            PROVIDER_END_NOT_SET,
            PROVIDER_END_ERROR,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        warn!(
            "Direct body stream idle timeout after {DIRECT_BODY_IDLE_TIMEOUT_SECS}s for {}, terminating stream",
            sanitize_sensitive_info(&self.fingerprint.addr.to_string())
        );
        self.release_stream_and_provider_handle_once();
    }

    pub(super) fn reset_custom_video_timeout(&mut self) {
        self.custom_video_timeout_mode = None;
        self.custom_video_timeout_sleep = None;
    }

    /// Returns whether the custom state can serve a body; finite HLS objects terminate on entry.
    pub(super) fn enter_custom_mode(&mut self, mode: StreamMode) -> bool {
        if self.response_mode == StreamResponseMode::HlsResource {
            self.terminate_quietly();
            return false;
        }
        if self.custom_video_timeout_mode != Some(mode) {
            self.custom_video_timeout_mode = Some(mode);
            self.custom_video_timeout_sleep = if self.custom_video_timeout_secs > 0 {
                Some(Box::pin(tokio::time::sleep(tokio::time::Duration::from_secs(u64::from(
                    self.custom_video_timeout_secs,
                )))))
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
        true
    }

    pub(super) fn custom_video_timed_out(&mut self, cx: &mut Context<'_>, mode: StreamMode) -> bool {
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

pub(super) fn should_use_direct_body_idle_timeout(stream_channel: &StreamChannel) -> bool {
    uses_direct_body_idle_timeout(stream_channel)
}
