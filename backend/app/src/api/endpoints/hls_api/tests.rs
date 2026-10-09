use super::{
    super::hls_terminal_response::{
        hls_manifest_terminal_preflight, hls_terminal_endpoint_action, hls_terminal_failed_closed_response,
        HlsManifestTerminalPreflight, HlsTerminalEndpointAction,
    },
    apply_hls_user_agent_stream_index, build_hls_manifest_request_headers, hls_api_register,
    hls_availability_reevaluation_registration_failure_response, hls_canonical_owner_registration,
    hls_temporary_resource_unavailable_response, m3u_archive_epg_reference_ts,
    m3u_catchup_epg_reference_from_session_token, resolve_leaked_hls_relative_origin, HlsCanonicalOwnerRegistration,
    HlsCanonicalOwnerRegistrationFailure, HlsCanonicalOwnerRegistrationKind,
};
use crate::{
    api::model::{
        build_terminal_tail_plan, build_transient_resource_id,
        hls_cache::initial_strip::{HlsInitialStripOutcome, HlsInitialStripSkipReason},
        prepare_terminal_base_evidence, prepared_terminal_bundle_key, snapshot_terminal_media_asset,
        ActiveProviderManager, ActiveUserManager, AppState, CacheAccessState, CancelTokens, ConnectionKind,
        ConnectionManager, CreateUserSessionParams, EventManager, HlsAccessContext, HlsAccessLease, HlsAccessLeaseId,
        HlsAccessLeaseState, HlsAccessLeaseTiming, HlsLeaseManifestSegment, HlsLeaseManifestSnapshot,
        HlsLeasePlaybackMode, HlsManifestCommitIdentity, HlsManifestCommitRequirement, HlsManifestDeliveryMode,
        HlsMediaContainer, HlsOriginAccountBinding, HlsPlaybackFamilyKey, HlsPreparedTerminalBundleState,
        HlsProxyManager, HlsPublishedTransientResourceIds, HlsRuntimeCustomTailAssetIdentity, HlsSession,
        HlsSessionHandle, HlsSessionKey, HlsSessionMode, HlsTerminalAssetIdentity, HlsTerminalBaseMediaState,
        HlsTerminalBaseProtection, HlsTerminalBaseSegmentAvailability, HlsTerminalMediaAsset,
        HlsTerminalTailBuildInput, HlsTerminalTailGeneration, HlsTerminalTailPlan, HlsTerminalTailProtection,
        MapCacheStatus, MetadataUpdateManager, OriginSegmentFetchRef, OriginSegmentKey, PlaylistStorageState,
        ProxyMapId, ProxySessionId, RenderedManifest, SegmentCacheKey, SegmentCacheStatus, SegmentEntry,
        SharedStreamManager, TransientResourceKind, TransientResourceRef, TransportStreamBuffer,
        HLS_TERMINAL_TAIL_SEGMENT_COUNT,
    },
    auth::Fingerprint,
    model::{
        ApiProxyConfig, ApiProxyServerInfo, AppConfig, Config, ConfigInput, ConfigSource, ConfigTarget,
        CustomStreamResponse, ProxyUserCredentials, ReverseProxyConfig, SourcesConfig, StripConfig, TargetUser,
    },
    processing::parser::hls::{
        get_hls_session_token_and_url_from_token,
        origin_manifest::{parse_origin_media_manifest, OriginManifestParseOutcome},
    },
    repository::GeoIp,
};
use aes::{cipher::Block, Aes128};
use arc_swap::{ArcSwap, ArcSwapOption};
use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{header, HeaderMap, HeaderValue, Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use http_body_util::BodyExt;
use shared::{
    model::{
        provider_saturation::build_group_lookup, ConfigPaths, ConfigTargetDto, ConfigTargetOptions,
        ConfigTargetShareLiveStreams, HlsCacheConfigDto, HlsStripMode, InputType, M3uPlaylistItem, M3uTargetOutputDto,
        PlaylistItem, PlaylistItemHeader, PlaylistItemType, ReverseProxyConfigDto, StreamConfigDto, TargetOutputDto,
        UserConnectionPermission, VirtualId, XtreamCluster, XtreamTargetOutputDto,
    },
    utils::Internable,
};
use std::{
    collections::HashMap,
    fmt::Write as _,
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::RwLock,
};
use tower::ServiceExt;
use tuliprox_hls::api::{
    derive_hls_lease_manifest_snapshot, hls_manifest_commit_requirement, HlsAccessLeasePendingDeadline,
    HlsCachedManifestOptions, HlsLeaseManifestSnapshotInput, LiveHlsOriginEntry,
};

mod owner_token;

mod archive;
mod bandwidth;
mod encrypted_media;
mod entry_identity;
mod headers;
mod lease_lifecycle;
mod manifest_startup;
mod observability;
mod owner_handoff;
mod provider_admission;
mod runtime_policy;
mod segment_responses;
mod support;
mod terminal_resources;
mod terminal_tail;
mod transient_resources;

use self::support::{
    access_lease_id_from_variant_uri, access_lease_session_token, activate_test_hls_access_lease,
    assert_hls_cache_stream_registered, assert_no_hls_cache_stream_registered, cache_recording_test_item,
    cache_test_m3u_hls_item, configure_default_test_server, configure_recording_test_listener,
    create_active_hls_user_session, create_active_hls_user_session_with, create_bound_hls_test_session,
    create_unbound_hls_test_session, disable_custom_stream_response, enable_channel_unavailable_custom_response,
    enable_hls_cache, encode_test_manifest, encrypt_test_aes128_cbc_pkcs7, get_response, get_status,
    hls_custom_video_test_user, hls_proxy_uri, hls_session_last_media_at_ms, legacy_manifest_test_client_headers,
    legacy_manifest_test_input, manifest_media_sequence, map_hls_map, map_ready_segment,
    map_ready_segment_without_lease, map_segment, map_segment_with_origin_url, map_transient_resource,
    map_transient_resource_with_kind, mark_hls_user_session_exhausted, media_uri_count, normal_manifest,
    normal_manifest_body, overlap_provider_input, path_has_extension, prepare_pending_test_hls_access_lease,
    proxy_session_id_from_variant_uri, publish_owner_handoff_test_manifest, publish_ready_test_manifest_for_lease,
    record_test_normal_manifest_commit, recording_test_response, recording_test_router, recording_test_url,
    regression_origin_manifest, request_response, response_body, runtime_policy_endpoint_fixture,
    single_hls_provider_input, single_variant_master_playlist, single_variant_uri, spawn_recording_header_origin,
    spawn_test_binary_origin, spawn_test_encoded_manifest_origin, spawn_test_encrypted_hls_origin,
    spawn_test_segment_origin, spawn_test_status_origin, spawn_test_transient_origin,
    spawn_test_transient_origin_with_delayed_binary_response, spawn_test_transient_origin_with_delayed_response,
    spawn_test_transient_origin_with_response, store_normal_manifest_body, store_test_sources_with_target,
    terminal_test_asset, terminal_test_plan_shape, terminalize_existing_test_lease, test_addr, test_addr_with_port,
    test_app_state, test_app_state_with_hls_proxy, test_app_state_with_hls_proxy_and_inputs,
    test_app_state_with_inputs, test_custom_video_buffer, test_fingerprint, test_fingerprint_with_addr,
    test_hls_access_context, test_hls_access_context_with, test_hls_entry_stream_context, test_hls_input,
    test_hls_sequence_iv, test_hls_share_target, test_m3u_hls_item, test_m3u_hls_share_target, test_segment_entry,
    transient_manifest_body, transient_manifest_body_from_sequence, try_test_hls_cached_manifest_response,
    wait_for_provider_connection_count, wait_for_runtime_policy_terminal_plan, CanonicalOwnerHandoffFixture,
    RuntimePolicyEndpointFixture, TestBinaryOriginResponse, TestSegmentOrigin, AES_TEST_KEY_BYTES, AES_TEST_MANIFEST,
    AES_TEST_PLAINTEXT_SEGMENT,
};

mod provider_admission_account_overlap;
mod provider_admission_provider_binding;
mod provider_admission_provider_identity;
mod provider_admission_provider_lease_release;

mod terminal_tail_recovery_before_cutover;
mod terminal_tail_terminal_cutover;
mod terminal_tail_terminal_fallback;
mod terminal_tail_terminal_preflight;
