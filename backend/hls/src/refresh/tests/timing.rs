use super::{
    compute_origin_refresh_interval_ms, critical_handoff_app_config, format_millis_as_seconds,
    format_optional_millis_as_seconds, origin_highwater_policy_limit, test_origin_refresh_request, test_session,
};
use crate::HlsPreparedTerminalBundleState;

#[test]
fn manifest_highwater_policy_limit_uses_target_duration_fallback() {
    assert_eq!(origin_highwater_policy_limit(60, None), Some(4));
    assert_eq!(origin_highwater_policy_limit(61, None), Some(5));
    assert_eq!(origin_highwater_policy_limit(60, Some(12)), Some(5));
}

#[test]
fn manifest_timing_log_values_are_seconds_or_none() {
    assert_eq!(format_optional_millis_as_seconds(Some(4_500)), "4.500");
    assert_eq!(format_optional_millis_as_seconds(None), "none");
    assert_eq!(format_millis_as_seconds(2_000), "2.000");
}

#[test]
fn refresh_interval_uses_half_reference_duration_without_upper_clamp() {
    assert_eq!(compute_origin_refresh_interval_ms(Some(8_000), None), 4_000);
    assert_eq!(compute_origin_refresh_interval_ms(Some(500), None), 1_000);
    assert_eq!(compute_origin_refresh_interval_ms(Some(20_000), None), 10_000);
    assert_eq!(compute_origin_refresh_interval_ms(None, None), 2_000);
}

#[tokio::test]
async fn hls_prepared_terminal_bundle_early_refresh_hook_starts_singleflight_for_known_target_duration() {
    let mut request = test_origin_refresh_request(test_session());
    request.app_config = critical_handoff_app_config();
    let terminal_response = request.app_config.custom_stream_response.load_full();
    let asset = terminal_response
        .as_ref()
        .and_then(|response| response.channel_unavailable.as_ref())
        .and_then(|buffer| super::super::snapshot_terminal_media_asset(buffer).ok())
        .expect("terminal refresh asset");
    let target_duration_ms = asset.duration_ms().saturating_add(1_000);
    let key = super::super::super::prepared_terminal_bundle::prepared_terminal_bundle_key(
        &asset,
        target_duration_ms,
        super::super::HLS_TERMINAL_TAIL_SEGMENT_COUNT,
    );

    super::super::start_refresh_terminal_bundle_preparation(&request, target_duration_ms);
    let state =
        request.hls_proxy.wait_for_prepared_terminal_bundle(key).await.expect("early terminal preparation completes");

    assert!(matches!(
        state,
        HlsPreparedTerminalBundleState::Ready { bundle } if bundle.key == key
    ));
}
