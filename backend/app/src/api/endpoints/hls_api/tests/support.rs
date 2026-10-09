mod app_state;
mod crypto;
mod http;
mod manifest;
mod origins;
mod owner_handoff;
mod recording;
mod runtime_policy;
mod sessions;
mod terminal;

pub(in crate::api::endpoints::hls_api::tests) use self::{
    app_state::{
        cache_test_m3u_hls_item, configure_default_test_server, enable_hls_cache, hls_custom_video_test_user,
        overlap_provider_input, path_has_extension, publish_test_transient_resource_membership,
        single_hls_provider_input, single_variant_master_playlist, store_test_sources_with_target, test_app_state,
        test_app_state_with_hls_proxy, test_app_state_with_hls_proxy_and_inputs, test_app_state_with_inputs,
        test_custom_video_buffer, test_hls_access_context, test_hls_access_context_with, test_hls_entry_stream_context,
        test_hls_input, test_hls_share_target, test_m3u_hls_item, test_m3u_hls_share_target, test_segment_entry,
    },
    crypto::{
        encrypt_test_aes128_cbc_pkcs7, spawn_test_encrypted_hls_origin, test_hls_sequence_iv, AES_TEST_KEY_BYTES,
        AES_TEST_MANIFEST, AES_TEST_PLAINTEXT_SEGMENT,
    },
    http::{
        access_lease_id_from_variant_uri, disable_custom_stream_response, enable_channel_unavailable_custom_response,
        get_response, get_status, hls_proxy_uri, proxy_session_id_from_variant_uri, request_response, response_body,
        single_variant_uri, try_test_hls_cached_manifest_response,
    },
    manifest::{
        legacy_manifest_test_client_headers, legacy_manifest_test_input, manifest_media_sequence, map_hls_map,
        map_ready_segment, map_ready_segment_without_lease, map_segment, map_segment_with_origin_url,
        map_transient_resource, map_transient_resource_with_kind, media_uri_count, normal_manifest,
        normal_manifest_body, publish_ready_test_manifest_for_lease, record_test_normal_manifest_commit,
        regression_origin_manifest, store_normal_manifest_body, transient_manifest_body,
        transient_manifest_body_from_sequence,
    },
    origins::{
        encode_test_manifest, spawn_test_binary_origin, spawn_test_encoded_manifest_origin, spawn_test_segment_origin,
        spawn_test_status_origin, spawn_test_transient_origin,
        spawn_test_transient_origin_with_delayed_binary_response, spawn_test_transient_origin_with_delayed_response,
        spawn_test_transient_origin_with_response, TestBinaryOriginResponse, TestEncodedManifestOrigin,
        TestSegmentOrigin,
    },
    owner_handoff::{publish_owner_handoff_test_manifest, CanonicalOwnerHandoffFixture},
    recording::{
        cache_recording_test_item, configure_recording_test_listener, recording_test_response, recording_test_router,
        recording_test_url, spawn_recording_header_origin,
    },
    runtime_policy::{
        runtime_policy_endpoint_fixture, wait_for_runtime_policy_terminal_plan, RuntimePolicyEndpointFixture,
    },
    sessions::{
        access_lease_session_token, activate_test_hls_access_lease, assert_hls_cache_stream_registered,
        assert_no_hls_cache_stream_registered, create_active_hls_user_session, create_active_hls_user_session_with,
        create_bound_hls_test_session, create_unbound_hls_test_session, grant_hls_proxy_lease,
        hls_session_last_media_at_ms, mark_hls_user_session_exhausted, prepare_pending_test_hls_access_lease,
        test_addr, test_addr_with_port, test_fingerprint, test_fingerprint_with_addr,
        wait_for_provider_connection_count,
    },
    terminal::{terminal_test_asset, terminal_test_plan_shape, terminalize_existing_test_lease},
};
