use super::*;

#[test]
fn test_preempted_from_custom_video_detail() {
    let info = make_stream_info("tuliprox", "low_priority_preempted");
    let reason = resolve_disconnect_reason(PROVIDER_END_NOT_SET, &info);
    assert_eq!(reason, DisconnectReason::Preempted);
}

#[test]
fn test_provider_preempted_atomic_maps_to_preempted() {
    let info = make_stream_info("some_provider", "Some Channel");
    let reason = resolve_disconnect_reason(PROVIDER_END_PREEMPTED, &info);
    assert_eq!(reason, DisconnectReason::Preempted);

    let reason_direct = resolve_disconnect_reason_from_provider_end(PROVIDER_END_PREEMPTED);
    assert_eq!(reason_direct, DisconnectReason::Preempted);
    assert_eq!(playback_outcome_for_reason(reason_direct), PlaybackRequestOutcome::Preempted);
}

#[test]
fn test_user_exhausted_custom_video_maps_to_user_connections_exhausted() {
    let info = make_stream_info("tuliprox", "user_connections_exhausted");
    let reason = resolve_disconnect_reason(PROVIDER_END_NOT_SET, &info);
    assert_eq!(reason, DisconnectReason::UserConnectionsExhausted);
}

#[test]
fn test_provider_exhausted_custom_video_maps_to_provider_connections_exhausted() {
    let info = make_stream_info("tuliprox", "provider_connections_exhausted");
    let reason = resolve_disconnect_reason(PROVIDER_END_NOT_SET, &info);
    assert_eq!(reason, DisconnectReason::ProviderConnectionsExhausted);
}
