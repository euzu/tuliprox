use super::*;

#[test]
fn test_channel_unavailable_with_eof_maps_to_provider_closed() {
    let info = make_stream_info("tuliprox", "channel_unavailable");
    let reason = resolve_disconnect_reason(PROVIDER_END_CLOSED, &info);
    assert_eq!(reason, DisconnectReason::ProviderClosed);
}

#[test]
fn test_channel_unavailable_with_err_maps_to_provider_error() {
    let info = make_stream_info("tuliprox", "channel_unavailable");
    let reason = resolve_disconnect_reason(PROVIDER_END_ERROR, &info);
    assert_eq!(reason, DisconnectReason::ProviderError);
}

#[test]
fn test_channel_unavailable_without_atomic_maps_to_provider_error() {
    let info = make_stream_info("tuliprox", "channel_unavailable");
    let reason = resolve_disconnect_reason(PROVIDER_END_NOT_SET, &info);
    assert_eq!(reason, DisconnectReason::ProviderError);
}
