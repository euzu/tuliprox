use super::{
    super::{
        AppState, Arc, HlsAccessLease, HlsAccessLeaseId, HlsManifestCommitRequirement, HlsPlaybackFamilyKey,
        HlsSessionHandle, HlsSessionKey, HlsStripMode, ProxySessionId, SegmentCacheStatus, StripConfig,
    },
    enable_hls_cache, normal_manifest, test_app_state, test_fingerprint,
};

pub(in crate::api::endpoints::hls_api::tests) struct CanonicalOwnerHandoffFixture {
    pub(in crate::api::endpoints::hls_api::tests) app_state: Arc<AppState>,
    pub(in crate::api::endpoints::hls_api::tests) session: HlsSessionHandle,
    pub(in crate::api::endpoints::hls_api::tests) proxy_session_id: ProxySessionId,
    pub(in crate::api::endpoints::hls_api::tests) leases: Vec<(HlsAccessLeaseId, u64)>,
    pub(in crate::api::endpoints::hls_api::tests) strip: StripConfig,
}

impl CanonicalOwnerHandoffFixture {
    pub(in crate::api::endpoints::hls_api::tests) async fn new(lease_ids: &[&str]) -> Self {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let session = app_state
            .hls
            .proxy
            .get_or_create_session(HlsSessionKey::new(1, "owner-handoff"), &app_state.get_encrypt_secret(), 100)
            .await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let issued_at_ms = super::super::super::current_time_millis();
        let mut leases = Vec::with_capacity(lease_ids.len());
        for lease_id in lease_ids {
            let lease_id = HlsAccessLeaseId((*lease_id).to_string());
            app_state
                .hls
                .proxy
                .prepare_access_lease(HlsAccessLease::pending(
                    lease_id.clone(),
                    HlsPlaybackFamilyKey::new("hls-user", test_fingerprint().key),
                    proxy_session_id.clone(),
                    "hls-user".to_string(),
                    format!("{}-session", lease_id.0),
                    1,
                    "owner-handoff".to_string(),
                    12345,
                    issued_at_ms,
                    60_000,
                ))
                .await;
            leases.push((lease_id, issued_at_ms));
        }
        Self {
            app_state,
            session,
            proxy_session_id,
            leases,
            strip: StripConfig { mode: HlsStripMode::Segments, value: 0 },
        }
    }

    pub(in crate::api::endpoints::hls_api::tests) async fn safe_session(&self) -> String {
        let session = self.session.read().await;
        super::super::super::safe_session_key(&session.key)
    }

    pub(in crate::api::endpoints::hls_api::tests) fn handoff_context(
        &self,
        lease_index: usize,
        safe_session: String,
        request_deadline_ms: u64,
    ) -> super::super::super::HlsCanonicalOwnerHandoffContext<'_> {
        let (lease_id, issued_at_ms) = &self.leases[lease_index];
        super::super::super::HlsCanonicalOwnerHandoffContext {
            app_state: &self.app_state,
            proxy_session_id: &self.proxy_session_id,
            access_lease_id: lease_id,
            expected_lease_issued_at_ms: Some(*issued_at_ms),
            strip: &self.strip,
            server_path: None,
            manifest_commit_requirement: HlsManifestCommitRequirement::CommittedManifestAllowed,
            manifest_boundary_rendered_at_ms: 0,
            bandwidth_learning: super::super::super::HlsRuntimeBandwidthLearningContext::Disabled,
            request_deadline_ms,
            safe_session,
        }
    }
}

pub(in crate::api::endpoints::hls_api::tests) async fn publish_owner_handoff_test_manifest(session: &HlsSessionHandle) {
    let now_ms = super::super::super::current_time_millis();
    let manifest = normal_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n\
             #EXTINF:4.0,\n1.ts\n#EXTINF:4.0,\n2.ts\n#EXTINF:4.0,\n3.ts\n",
    );
    let mut session = session.write().await;
    session.apply_origin_manifest(&manifest).expect("owner handoff manifest maps");
    for segment in session.segments.values_mut() {
        segment.status = SegmentCacheStatus::Ready { content_length: 1_000, ready_at_ms: now_ms };
    }
    session.advance_media_readiness_generation();
    session.render_and_store_manifest(now_ms).expect("owner handoff manifest renders");
    session.mark_authorized_media_access(now_ms);
}
