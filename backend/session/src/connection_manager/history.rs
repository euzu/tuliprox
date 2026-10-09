use super::{PROVIDER_END_CLOSED, PROVIDER_END_ERROR, PROVIDER_END_PREEMPTED};
use arc_swap::ArcSwapOption;
use shared::model::{CustomVideoStreamType, DisconnectReason, FailureStage, StreamInfo};
use std::{str::FromStr, sync::Arc};
use tuliprox_core::model::{DisconnectQos, PlaybackRequestOutcome, StreamHistoryConfig, StreamHistoryRecord};
use tuliprox_repository::{recover_pending_files, StreamHistoryWriter};

/// Build a new `StreamHistoryWriter` from the given config, running file recovery first.
/// Returns `None` if history is disabled or no config is provided.
pub(super) fn build_history_writer(config: Option<&StreamHistoryConfig>) -> Option<Arc<StreamHistoryWriter>> {
    let cfg = config?;
    if !cfg.stream_history_enabled {
        return None;
    }
    if let Err(e) = recover_pending_files(&cfg.stream_history_directory) {
        log::warn!("Stream history recovery failed: {e}");
    }
    Some(Arc::new(StreamHistoryWriter::new(cfg)))
}

/// Async variant used on hot config reload: recovery I/O and compression run on the blocking
/// pool so a runtime worker is never blocked while streams are being served.
pub(super) async fn build_history_writer_async(
    config: Option<&StreamHistoryConfig>,
) -> Option<Arc<StreamHistoryWriter>> {
    let cfg = config?;
    if !cfg.stream_history_enabled {
        return None;
    }
    let directory = cfg.stream_history_directory.clone();
    match tokio::task::spawn_blocking(move || recover_pending_files(&directory)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => log::warn!("Stream history recovery failed: {e}"),
        Err(join_err) => log::warn!("Stream history recovery task panicked: {join_err}"),
    }
    Some(Arc::new(StreamHistoryWriter::new(cfg)))
}

/// Determine the disconnect reason from the provider-end signal and the stream's current state.
///
/// Priority: If `update_stream_detail` switched the stream to custom-video mode
/// (`provider == "tuliprox"`), the video type takes precedence. The `provider_end_reason`
/// `AtomicU8` disambiguates `ChannelUnavailable` into `ProviderClosed` (EOF) vs `ProviderError` (Err).
///
/// SAFETY: The `channel.title` strings (`channel_unavailable`, `low_priority_preempted`, etc.)
/// are wire-format identifiers shared with Serialize/Deserialize and the REST API.
/// If they ever change, update `CustomVideoStreamType::fmt`/`from_str` and this function together.
pub(super) fn resolve_disconnect_reason(provider_end_reason: u8, stream_info: &StreamInfo) -> DisconnectReason {
    if stream_info.provider.as_ref() == "tuliprox" {
        if let Ok(video_type) = CustomVideoStreamType::from_str(&stream_info.channel.title) {
            match video_type {
                CustomVideoStreamType::LowPriorityPreempted => return DisconnectReason::Preempted,
                CustomVideoStreamType::UserConnectionsExhausted => return DisconnectReason::UserConnectionsExhausted,
                CustomVideoStreamType::ProviderConnectionsExhausted => {
                    return DisconnectReason::ProviderConnectionsExhausted
                }
                CustomVideoStreamType::ChannelUnavailable => {
                    return match provider_end_reason {
                        PROVIDER_END_CLOSED => DisconnectReason::ProviderClosed,
                        _ => DisconnectReason::ProviderError,
                    };
                }
                _ => {}
            }
        }
    }

    match provider_end_reason {
        PROVIDER_END_CLOSED => DisconnectReason::ProviderClosed,
        PROVIDER_END_ERROR => DisconnectReason::ProviderError,
        PROVIDER_END_PREEMPTED => DisconnectReason::Preempted,
        _ => DisconnectReason::ClientClosed,
    }
}

/// Resolves a disconnect reason from the provider-end signal alone, for the path where
/// the user stream claim is already gone and no `StreamInfo` is available to consult.
pub(super) fn resolve_disconnect_reason_from_provider_end(provider_end_reason: u8) -> DisconnectReason {
    match provider_end_reason {
        PROVIDER_END_CLOSED => DisconnectReason::ProviderClosed,
        PROVIDER_END_ERROR => DisconnectReason::ProviderError,
        PROVIDER_END_PREEMPTED => DisconnectReason::Preempted,
        _ => DisconnectReason::ClientClosed,
    }
}

/// Maps a disconnect reason onto the provider-lease outcome policy.
///
/// Only a clean client-side end keeps a reconnect-capable lease alive; provider
/// failures, preemption, kicks and timeouts release capacity immediately.
pub(super) fn playback_outcome_for_reason(reason: DisconnectReason) -> PlaybackRequestOutcome {
    match reason {
        DisconnectReason::ClientClosed
        | DisconnectReason::DayRollover
        | DisconnectReason::Cleanup
        | DisconnectReason::Unknown => PlaybackRequestOutcome::ClientClosed,
        DisconnectReason::Timeout => PlaybackRequestOutcome::TimedOut,
        DisconnectReason::SessionExpired => PlaybackRequestOutcome::SessionExpired,
        DisconnectReason::Shutdown => PlaybackRequestOutcome::ServerShutdown,
        DisconnectReason::ClientKicked => PlaybackRequestOutcome::Kicked,
        DisconnectReason::Preempted => PlaybackRequestOutcome::Preempted,
        DisconnectReason::Provisioning
        | DisconnectReason::UserConnectionsExhausted
        | DisconnectReason::ProviderConnectionsExhausted => PlaybackRequestOutcome::FailedBeforeMedia,
        DisconnectReason::ProviderError
        | DisconnectReason::ProviderClosed
        | DisconnectReason::ServerError
        | DisconnectReason::IntermediateFailures(_) => PlaybackRequestOutcome::ProviderFailed,
    }
}

pub(super) fn emit_connect_record(writer: &ArcSwapOption<StreamHistoryWriter>, info: &StreamInfo) {
    let guard = writer.load();
    let Some(w) = guard.as_ref() else { return };
    w.send_record(StreamHistoryRecord::from_connect(info));
}

pub(super) fn emit_disconnect_record(
    writer: &ArcSwapOption<StreamHistoryWriter>,
    info: &StreamInfo,
    reason: DisconnectReason,
    qos: &DisconnectQos,
    provider_error_class: Option<&str>,
    provider_http_status: Option<u16>,
) {
    let guard = writer.load();
    let Some(w) = guard.as_ref() else { return };
    w.send_record(
        StreamHistoryRecord::from_disconnect(info, reason, qos, resolve_disconnect_failure_stage(info, reason, qos))
            .with_provider_failure(provider_http_status, provider_error_class),
    );
}

pub(super) fn resolve_disconnect_failure_stage(
    info: &StreamInfo,
    reason: DisconnectReason,
    qos: &DisconnectQos,
) -> Option<FailureStage> {
    match reason {
        DisconnectReason::ProviderError | DisconnectReason::ProviderClosed => {
            if !info.channel.shared && qos.first_byte_latency_ms.is_none() {
                Some(FailureStage::FirstByte)
            } else {
                Some(FailureStage::Streaming)
            }
        }
        DisconnectReason::Preempted => Some(FailureStage::Streaming),
        DisconnectReason::SessionExpired => Some(FailureStage::SessionReconnect),
        _ => None,
    }
}
