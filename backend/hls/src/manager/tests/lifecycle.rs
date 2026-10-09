use super::{
    access_lease, hls_key_readiness_evidence_is_current, live_media_fixture, manifest_snapshot, terminal_plan,
    HlsAccessLeaseDenialMode, HlsAccessLeaseDenialOutcome, HlsLeaseManifestPublicationOutcome,
    HlsLeaseManifestPublicationRejectReason, HlsMediaActivityCommitOutcome, HlsProxyManager, HlsTerminalTailProtection,
};
use crate::{
    terminal_tail::HlsLeasePlaybackMode, HlsAccessLeaseId, HlsAccessLeasePendingDeadline,
    HlsPublishedTransientResourceIds, HlsSessionKey, ProxySessionId,
};
use shared::model::HlsCacheConfigDto;
use std::sync::Arc;
use tuliprox_core::model::HlsCacheConfig;

#[test]
fn key_readiness_evidence_is_valid_at_expiry_and_stale_one_millisecond_later() {
    assert!(hls_key_readiness_evidence_is_current(Some(50_000), 50_000));
    assert!(!hls_key_readiness_evidence_is_current(Some(50_000), 50_001));
    assert!(hls_key_readiness_evidence_is_current(None, u64::MAX));
}

#[tokio::test]
async fn expired_and_denied_lease_identities_cannot_extend_shared_session_activity() {
    let (expired_manager, expired_session, expired_proxy, expired_lease_id, expired_identity) =
        live_media_fixture("expired-media").await;
    {
        let mut leases = expired_manager.access_leases.write().await;
        let mut lease = leases.remove_access_lease(&expired_lease_id).expect("expiring lease");
        lease.pending_deadline = Some(HlsAccessLeasePendingDeadline::Bootstrap { deadline_ms: 2_000 });
        lease.valid_until_ms = 2_000;
        leases.prepare_access_lease(lease);
    }
    assert_eq!(
        expired_manager
            .mark_authorized_media_access_for_lease_if_identity_matches(
                &expired_session,
                &expired_lease_id,
                &expired_proxy,
                expired_identity,
                2_000,
            )
            .await,
        HlsMediaActivityCommitOutcome::StaleLeaseIdentity
    );
    assert_eq!(expired_session.read().await.activity.last_authorized_media_at_ms, None);

    let (denied_manager, denied_session, denied_proxy, denied_lease_id, denied_identity) =
        live_media_fixture("denied-media").await;
    assert!(matches!(
        denied_manager
            .access_leases
            .write()
            .await
            .deny_access_lease(&denied_lease_id, HlsAccessLeaseDenialMode::ImmediateEnd),
        HlsAccessLeaseDenialOutcome::Ended { terminal_release: None }
    ));
    assert_eq!(
        denied_manager
            .mark_authorized_media_access_for_lease_if_identity_matches(
                &denied_session,
                &denied_lease_id,
                &denied_proxy,
                denied_identity,
                2_000,
            )
            .await,
        HlsMediaActivityCommitOutcome::StaleLeaseIdentity
    );
    assert_eq!(denied_session.read().await.activity.last_authorized_media_at_ms, None);
}

#[tokio::test]
async fn manifest_publication_manager_rejects_delayed_source_and_expiry_races() {
    let manager = HlsProxyManager::new();
    let proxy_session_id = ProxySessionId("proxy".to_string());
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    manager.access_leases.write().await.prepare_access_lease(access_lease(&lease_id.0, &proxy_session_id));
    let older_request = manager
        .prepare_access_lease_manifest_publication(&lease_id, &proxy_session_id, 2_000)
        .await
        .expect("older request guard");
    let newer_request = manager
        .prepare_access_lease_manifest_publication(&lease_id, &proxy_session_id, 2_000)
        .await
        .expect("newer request guard");

    assert_eq!(
        manager
            .commit_access_lease_manifest_publication(
                &lease_id,
                &proxy_session_id,
                newer_request,
                manifest_snapshot(20),
                2_100,
            )
            .await,
        HlsLeaseManifestPublicationOutcome::Committed { snapshot_generation: 1 }
    );
    assert_eq!(
        manager
            .commit_access_lease_manifest_publication(
                &lease_id,
                &proxy_session_id,
                older_request,
                manifest_snapshot(10),
                2_200,
            )
            .await,
        HlsLeaseManifestPublicationOutcome::Rejected(HlsLeaseManifestPublicationRejectReason::SourceRegressive)
    );

    let expiry_lease_id = HlsAccessLeaseId("expiry".to_string());
    manager.access_leases.write().await.prepare_access_lease(access_lease(&expiry_lease_id.0, &proxy_session_id));
    let expiry_request = manager
        .prepare_access_lease_manifest_publication(&expiry_lease_id, &proxy_session_id, 2_000)
        .await
        .expect("pre-expiry request guard");
    assert_eq!(
        manager
            .commit_access_lease_manifest_publication(
                &expiry_lease_id,
                &proxy_session_id,
                expiry_request,
                manifest_snapshot(30),
                61_000,
            )
            .await,
        HlsLeaseManifestPublicationOutcome::Rejected(HlsLeaseManifestPublicationRejectReason::LeaseExpired)
    );
}

#[tokio::test]
async fn rolling_manifest_publication_uses_lease_store_when_session_is_gc_marked() {
    let manager = HlsProxyManager::new();
    let session = manager.get_or_create_session(HlsSessionKey::new(1, "rolling"), b"secret", 1_000).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    manager.access_leases.write().await.prepare_access_lease(access_lease(&lease_id.0, &proxy_session_id));
    let publication = manager
        .prepare_access_lease_manifest_publication(&lease_id, &proxy_session_id, 2_000)
        .await
        .expect("rolling publication guard");
    let published_resource_ids = HlsPublishedTransientResourceIds::from_manifest_body(
        "#EXTM3U\n#EXTINF:6,\n/hls/shared/live/proxy/lease-a/r/resource.ts\n",
    );
    session.write().await.mark_for_gc_removal();

    assert_eq!(
        manager
            .commit_access_lease_manifest_publication_with_resources(
                &lease_id,
                &proxy_session_id,
                publication,
                manifest_snapshot(20),
                published_resource_ids.clone(),
                2_100,
            )
            .await,
        HlsLeaseManifestPublicationOutcome::Committed { snapshot_generation: 1 }
    );
    let lease = manager
        .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_100)
        .await
        .expect("published rolling lease");
    assert_eq!(lease.published_transient_resource_ids(), &published_resource_ids);
}

#[tokio::test]
async fn stale_lease_removal_preparation_cannot_release_replacement_protection() {
    let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
    let manager = HlsProxyManager::with_hls_cache_config(&config);
    let (session, _) =
        manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("replacement-protection".to_string());
    let old_plan = terminal_plan(7, &proxy_session_id, &lease_id);
    let mut old_lease = access_lease(&lease_id.0, &proxy_session_id);
    old_lease.playback_mode = HlsLeasePlaybackMode::TerminalTail(Arc::clone(&old_plan));
    manager.access_leases.write().await.prepare_access_lease(old_lease);
    assert_eq!(
        session.write().await.install_terminal_tail_protection(
            lease_id.clone(),
            HlsTerminalTailProtection {
                generation: old_plan.generation,
                base_proxy_seqs: Arc::clone(&old_plan.protected_base_proxy_seqs),
                key_bindings: old_plan.key_bindings(),
            },
        ),
        super::super::super::session::HlsTerminalTailProtectionInstall::Installed
    );
    let preparation = manager
        .access_leases
        .read()
        .await
        .prepare_access_lease_removal(&lease_id)
        .expect("old lease removal preparation");

    let replacement_plan = terminal_plan(8, &proxy_session_id, &lease_id);
    let mut replacement = access_lease(&lease_id.0, &proxy_session_id);
    replacement.issued_at_ms = 2_000;
    replacement.playback_mode = HlsLeasePlaybackMode::TerminalTail(Arc::clone(&replacement_plan));
    assert!(manager.access_leases.write().await.prepare_access_lease(replacement));
    assert_eq!(
        session.write().await.install_terminal_tail_protection(
            lease_id.clone(),
            HlsTerminalTailProtection {
                generation: replacement_plan.generation,
                base_proxy_seqs: Arc::clone(&replacement_plan.protected_base_proxy_seqs),
                key_bindings: replacement_plan.key_bindings(),
            },
        ),
        super::super::super::session::HlsTerminalTailProtectionInstall::Installed
    );

    assert!(!manager.remove_prepared_access_lease(&lease_id, &preparation).await);
    assert_eq!(
        manager
            .access_leases
            .write()
            .await
            .response_snapshot(&lease_id, &proxy_session_id, 2_000)
            .map(|lease| lease.issued_at_ms),
        Some(2_000)
    );
    assert_eq!(
        session.read().await.terminal_tail_protection(&lease_id).map(|protection| protection.generation),
        Some(replacement_plan.generation)
    );
}
