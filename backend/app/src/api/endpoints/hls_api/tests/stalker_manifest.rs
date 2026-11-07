use super::{single_hls_provider_input, test_app_state_with_inputs, test_fingerprint};
use crate::{
    api::model::{ConnectionKind, PlaybackLeaseRef},
    model::{ConfigInput, PlaybackKind, ProxyUserCredentials},
    processing::parser::hls::HlsManifestSource,
};
use shared::model::{InputType, UserConnectionPermission};
use std::sync::Arc;

const SELECTED: &str = "alias-a";

fn variant(source: HlsManifestSource, origin_provider: Option<&str>) -> super::super::HlsRequestStage<'_> {
    super::super::HlsRequestStage::Variant { source, origin_provider }
}

#[test]
fn stalker_entry_resolves_a_fresh_link_and_legacy_variant_is_rejected() {
    use super::super::StalkerManifestUrl::{Reject, Resolve};
    assert_eq!(super::super::stalker_manifest_url(super::super::HlsRequestStage::Entry, SELECTED), Resolve);
    assert_eq!(super::super::stalker_manifest_url(super::super::HlsRequestStage::LegacyVariant, SELECTED), Reject);
}

#[test]
fn stalker_variant_is_accepted_only_for_its_origin_account() {
    use super::super::StalkerManifestUrl::{Accept, Reject};
    for source in [HlsManifestSource::Entry, HlsManifestSource::Child] {
        assert_eq!(super::super::stalker_manifest_url(variant(source, Some(SELECTED)), SELECTED), Accept);
        assert_eq!(super::super::stalker_manifest_url(variant(source, Some("alias-b")), SELECTED), Reject);
        assert_eq!(super::super::stalker_manifest_url(variant(source, None), SELECTED), Reject);
    }
}

#[test]
fn stalker_hls_stream_requires_a_portal_id_and_maps_the_kind() {
    use shared::model::StalkerStreamKind::{Archive, Live};
    assert_eq!(super::super::stalker_hls_stream("1063", false), Some((1063, Live)));
    assert_eq!(super::super::stalker_hls_stream("1063", true), Some((1063, Archive)));
    assert_eq!(super::super::stalker_hls_stream("news24hd", false), None);
    assert_eq!(super::super::stalker_hls_stream("", false), None);
}

#[tokio::test]
async fn stalker_entry_reservation_without_portal_stream_fails_and_releases_the_slot() {
    let input = ConfigInput { input_type: InputType::Stalker, ..single_hls_provider_input("stalker-provider") };
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let mut user = ProxyUserCredentials::default();
    user.username = "hls-user".to_string();

    let reservation = super::super::try_reserve_hls_entry_origin_account_for_redirect(
        &app_state,
        &test_fingerprint(),
        &user,
        &input,
        12345,
        "http://account.example.com/live/12345.m3u8",
        "hls-session-token",
        "hls-session-owner",
        PlaybackKind::LiveHls,
        super::super::hls_origin_account_reservation_ttl_secs_fallback(),
        UserConnectionPermission::Allowed,
        ConnectionKind::Normal,
        false,
        None,
    )
    .await;

    assert!(reservation.is_none(), "an unresolvable Stalker link must not fall back to the stored URL");
    let handle = app_state.active_provider.acquire_connection_with_lease_for_session(
        &input.name,
        &test_fingerprint().addr,
        false,
        0,
        ConnectionKind::Normal,
        Some(PlaybackLeaseRef::new("hls-other-owner", PlaybackKind::LiveHls)),
    );
    assert!(handle.is_some(), "the failed reservation must release its provider slot");
    app_state.connection_manager.release_provider_handle(handle);
}
