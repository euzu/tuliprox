use super::{
    super::{
        build_transient_resource_id, header, parse_origin_media_manifest, AppState, Arc, HashMap, HeaderMap,
        HeaderValue, HlsAccessLeaseId, HlsLeaseManifestSegment, HlsLeaseManifestSnapshot, HlsManifestCommitIdentity,
        HlsManifestDeliveryMode, HlsMediaContainer, HlsSession, HlsSessionKey, HlsSessionMode, MapCacheStatus,
        OriginManifestParseOutcome, ProxyMapId, ProxySessionId, RenderedManifest, SegmentCacheStatus,
        TransientResourceKind, TransientResourceRef,
    },
    grant_hls_proxy_lease, test_segment_entry, TestEncodedManifestOrigin,
};
use std::fmt::Write as _;

pub(in crate::api::endpoints::hls_api::tests) fn transient_manifest_body(proxy_session_id: &str) -> String {
    transient_manifest_body_from_sequence(proxy_session_id, 100, 6)
}

pub(in crate::api::endpoints::hls_api::tests) fn transient_manifest_body_from_sequence(
    proxy_session_id: &str,
    first_sequence: u64,
    count: usize,
) -> String {
    let mut body = format!("#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXT-X-MEDIA-SEQUENCE:{first_sequence}\n");
    for index in 0..count {
        let sequence = first_sequence.saturating_add(u64::try_from(index).expect("test sequence index fits u64"));
        body.push_str("#EXTINF:10.0,\n");
        let _ = writeln!(
            body,
            "/hls/shared/live/{proxy_session_id}/{}/r/seg{sequence}.ts",
            crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER
        );
    }
    body
}

pub(in crate::api::endpoints::hls_api::tests) fn media_uri_count(body: &str) -> usize {
    body.lines().filter(|line| !line.is_empty() && !line.starts_with('#')).count()
}

pub(in crate::api::endpoints::hls_api::tests) fn normal_manifest_body(proxy_session_id: &str) -> String {
    normal_manifest_body_from_sequence(proxy_session_id, 0, 6)
}

pub(in crate::api::endpoints::hls_api::tests) fn normal_manifest_body_from_sequence(
    proxy_session_id: &str,
    first_sequence: u64,
    count: usize,
) -> String {
    let mut body = format!("#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXT-X-MEDIA-SEQUENCE:{first_sequence}\n");
    for index in 0..count {
        let sequence = first_sequence.saturating_add(u64::try_from(index).expect("test sequence index fits u64"));
        body.push_str("#EXTINF:10.0,\n");
        let _ = writeln!(
            body,
            "/hls/shared/live/{proxy_session_id}/{}/{sequence:06}.ts",
            crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER
        );
    }
    body
}

pub(in crate::api::endpoints::hls_api::tests) fn store_normal_manifest_body(
    session: &mut HlsSession,
    body: String,
    rendered_at_ms: u64,
) {
    store_normal_manifest_body_range(session, body, 0, 6, rendered_at_ms);
}

pub(in crate::api::endpoints::hls_api::tests) fn record_test_normal_manifest_commit(
    session: &mut HlsSession,
    rendered_at_ms: u64,
) {
    let identity = session
        .next_manifest_commit_identity(rendered_at_ms)
        .expect("test manifest commit generation remains available");
    session.record_normal_manifest_commit_identity(identity);
}

pub(in crate::api::endpoints::hls_api::tests) fn store_normal_manifest_body_range(
    session: &mut HlsSession,
    body: String,
    first_proxy_seq: u64,
    count: usize,
    rendered_at_ms: u64,
) {
    let last_proxy_seq =
        first_proxy_seq.saturating_add(u64::try_from(count.saturating_sub(1)).expect("test count fits u64"));
    let proxy_session_id = session.proxy_session_id.clone();
    for proxy_seq in first_proxy_seq..=last_proxy_seq {
        let mut entry = test_segment_entry(
            &proxy_session_id,
            proxy_seq,
            SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: rendered_at_ms },
        );
        entry.duration_ms = 10_000;
        session.segments.insert(proxy_seq, entry);
    }
    session.advance_media_readiness_generation();
    session.last_rendered_manifest = Some(RenderedManifest {
        body,
        first_proxy_seq,
        last_proxy_seq,
        playlist_duration_ms: 60_000,
        valid_until_ms: rendered_at_ms.saturating_add(60_000),
        render_gap_segments: 0,
        rendered_at_ms,
        discontinuity_sequence: 0,
        target_duration_ms: 10_000,
        segment_proxy_seqs: (first_proxy_seq..=last_proxy_seq).collect(),
    });
    record_test_normal_manifest_commit(session, rendered_at_ms);
}

pub(in crate::api::endpoints::hls_api::tests) async fn publish_ready_test_manifest_for_lease(
    app_state: &Arc<AppState>,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    target_duration_ms: u64,
) {
    let session =
        app_state.hls.proxy.sessions().get_by_proxy_session_id(proxy_session_id).await.expect("test session exists");
    let (proxy_seq, duration_ms) = {
        let session = session.read().await;
        session
            .segments
            .iter()
            .find_map(|(proxy_seq, entry)| {
                matches!(entry.status, SegmentCacheStatus::Ready { .. }).then_some((*proxy_seq, entry.duration_ms))
            })
            .expect("READY test segment")
    };
    let now_ms = super::super::super::current_time_millis();
    let publication_guard = app_state
        .hls
        .proxy
        .prepare_access_lease_manifest_publication(access_lease_id, proxy_session_id, now_ms)
        .await
        .expect("test lease accepts publication");
    let snapshot = HlsLeaseManifestSnapshot {
        startup_revisions: None,
        delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
        source_commit_identity: HlsManifestCommitIdentity::new(now_ms),
        uri_materialization: None,
        finalized_transient_manifest_generation: None,
        snapshot_generation: 0,
        delivered_at_ms: now_ms,
        first_proxy_seq: proxy_seq,
        last_proxy_seq: proxy_seq,
        visible_segments: Arc::from([HlsLeaseManifestSegment {
            proxy_seq,
            duration_ms,
            uri: format!("/hls/shared/live/{}/{}/{proxy_seq:06}.ts", proxy_session_id.0, access_lease_id.0).into(),
            discontinuity_before: false,
            map_ref_ready: true,
            encryption: None,
        }]),
        discontinuity_sequence: 0,
        target_duration_ms: target_duration_ms.max(duration_ms),
        playlist_duration_ms: duration_ms,
        last_visible_media_end_ms: duration_ms,
        active_map: None,
        active_encryption: None,
        container: HlsMediaContainer::MpegTs,
    };
    assert!(app_state
        .hls
        .proxy
        .commit_access_lease_manifest_publication(
            access_lease_id,
            proxy_session_id,
            publication_guard,
            snapshot,
            now_ms,
        )
        .await
        .is_committed());
}

pub(in crate::api::endpoints::hls_api::tests) fn regression_origin_manifest(
    first_sequence: u64,
    segment_count: usize,
) -> Vec<u8> {
    let mut manifest =
        format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:12\n#EXT-X-MEDIA-SEQUENCE:{first_sequence}\n");
    for offset in 0..segment_count {
        let sequence = first_sequence.saturating_add(u64::try_from(offset).unwrap_or(u64::MAX));
        let _ = writeln!(&mut manifest, "#EXTINF:12.0,\n{sequence}.ts");
    }
    manifest.into_bytes()
}

pub(in crate::api::endpoints::hls_api::tests) fn normal_manifest(
    body: &str,
) -> crate::processing::parser::hls::origin_manifest::ParsedOriginManifest {
    match parse_origin_media_manifest(body, "http://origin.example.com/live/final/index.m3u8") {
        OriginManifestParseOutcome::Normal(manifest) => manifest,
        OriginManifestParseOutcome::TransientPassthrough { reason } => {
            panic!("expected normal manifest: {reason:?}")
        }
    }
}

pub(in crate::api::endpoints::hls_api::tests) async fn map_segment(
    app_state: &Arc<AppState>,
    proxy_seq: u64,
    extension: &str,
) -> String {
    map_segment_with_origin_url(app_state, proxy_seq, extension, &format!("{proxy_seq}.{extension}")).await
}

pub(in crate::api::endpoints::hls_api::tests) async fn map_segment_with_origin_url(
    app_state: &Arc<AppState>,
    proxy_seq: u64,
    _extension: &str,
    origin_url: &str,
) -> String {
    let session = app_state
        .hls
        .proxy
        .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
        .await;
    let manifest =
        normal_manifest(&format!("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:{proxy_seq}\n#EXTINF:4.0,\n{origin_url}\n"));
    let mut session = session.write().await;
    session.proxy_next_seq = Some(proxy_seq);
    session.apply_origin_manifest(&manifest).expect("manifest should map");
    session.proxy_session_id.0.clone()
}

pub(in crate::api::endpoints::hls_api::tests) async fn map_ready_segment(
    app_state: &Arc<AppState>,
    proxy_seq: u64,
    extension: &str,
    body: &[u8],
) -> String {
    let proxy_session_id = map_ready_segment_without_lease(app_state, proxy_seq, extension, body).await;
    grant_hls_proxy_lease(app_state, &proxy_session_id).await;
    proxy_session_id
}

pub(in crate::api::endpoints::hls_api::tests) async fn map_ready_segment_without_lease(
    app_state: &Arc<AppState>,
    proxy_seq: u64,
    extension: &str,
    body: &[u8],
) -> String {
    let proxy_session_id = map_segment(app_state, proxy_seq, extension).await;
    let session = app_state
        .hls
        .proxy
        .sessions()
        .get_by_proxy_session_id(&ProxySessionId(proxy_session_id.clone()))
        .await
        .expect("session should exist");
    let cache_key = {
        let session = session.read().await;
        session.segments.get(&proxy_seq).expect("segment should be mapped").cache_key.clone()
    };
    let metadata = app_state
        .hls
        .proxy
        .segment_cache()
        .write_bytes_and_commit(&cache_key, body)
        .await
        .expect("cache commit should succeed");
    {
        let mut session = session.write().await;
        session.segments.get_mut(&proxy_seq).expect("segment should be mapped").status =
            SegmentCacheStatus::Ready { content_length: metadata.size, ready_at_ms: 200 };
    }
    proxy_session_id
}

pub(in crate::api::endpoints::hls_api::tests) async fn map_hls_map(
    app_state: &Arc<AppState>,
    body: &[u8],
    grant_lease: bool,
) -> String {
    let session = app_state
        .hls
        .proxy
        .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
        .await;
    let manifest = normal_manifest("#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n000123.m4s\n");
    let proxy_session_id = {
        let mut session = session.write().await;
        session.apply_origin_manifest(&manifest).expect("manifest should map");
        session.proxy_session_id.0.clone()
    };
    let session = app_state
        .hls
        .proxy
        .sessions()
        .get_by_proxy_session_id(&ProxySessionId(proxy_session_id.clone()))
        .await
        .expect("session should exist");
    let cache_key = {
        let session = session.read().await;
        session.maps.get(&ProxyMapId(0)).expect("map should be mapped").cache_key.clone()
    };
    let metadata = app_state
        .hls
        .proxy
        .segment_cache()
        .write_bytes_and_commit(&cache_key, body)
        .await
        .expect("map cache commit should succeed");
    {
        let mut session = session.write().await;
        session.maps.get_mut(&ProxyMapId(0)).expect("map should be mapped").status =
            MapCacheStatus::Ready { content_length: metadata.size, ready_at_ms: 200 };
    }
    if grant_lease {
        grant_hls_proxy_lease(app_state, &proxy_session_id).await;
    }
    proxy_session_id
}

pub(in crate::api::endpoints::hls_api::tests) async fn map_transient_resource(
    app_state: &Arc<AppState>,
    origin_url: &str,
    extension: &str,
    grant_lease: bool,
) -> (String, String) {
    map_transient_resource_with_kind(app_state, origin_url, extension, grant_lease, TransientResourceKind::Segment)
        .await
}

pub(in crate::api::endpoints::hls_api::tests) async fn map_transient_resource_with_kind(
    app_state: &Arc<AppState>,
    origin_url: &str,
    extension: &str,
    grant_lease: bool,
    kind: TransientResourceKind,
) -> (String, String) {
    static TRANSIENT_STREAM_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let secret = b"rewrite-secret";
    let now_ms = super::super::super::current_time_millis();
    let stream_ref =
        format!("transient-{}", TRANSIENT_STREAM_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let session = app_state.hls.proxy.get_or_create_session(HlsSessionKey::new(1, &stream_ref), secret, now_ms).await;
    let resource_id = build_transient_resource_id(origin_url, secret);
    let proxy_session_id = {
        let mut session = session.write().await;
        session.mode =
            HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
        session.transient.upsert_resources([TransientResourceRef::new(
            kind,
            origin_url,
            secret,
            now_ms,
            300_000,
            Some(extension.to_string()),
        )]);
        session.proxy_session_id.0.clone()
    };
    if grant_lease {
        grant_hls_proxy_lease(app_state, &proxy_session_id).await;
    }
    (proxy_session_id, resource_id.0)
}

pub(in crate::api::endpoints::hls_api::tests) fn manifest_media_sequence(body: &str) -> u64 {
    body.lines()
        .find_map(|line| line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:"))
        .and_then(|value| value.parse().ok())
        .expect("media sequence")
}

pub(in crate::api::endpoints::hls_api::tests) fn legacy_manifest_test_input(
    origin: &TestEncodedManifestOrigin,
) -> crate::model::InputSource {
    crate::model::InputSource {
        name: Arc::from("legacy-content-coding-test"),
        url: format!("{}/manifest.m3u8", origin.base_url),
        provider: None,
        username: None,
        password: None,
        method: shared::model::InputFetchMethod::GET,
        headers: HashMap::from([("Accept-Encoding".to_string(), "gzip".to_string())]),
    }
}

pub(in crate::api::endpoints::hls_api::tests) fn legacy_manifest_test_client_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));
    headers
}
