use super::{
    access_lease, finalized_transient_manifest_snapshot, live_media_fixture, publish_manifest_snapshot,
    HlsCriticalHandoffStateAccess, HlsMediaActivityCommitOutcome, HlsProxyManager,
};
use crate::{
    HlsAccessLease, HlsAccessLeaseId, HlsAccessLeaseState, HlsPlaybackFamilyKey, HlsPublishedTransientResourceIds,
    HlsSessionKey, HlsSessionStoreOutcome, TransientResourceKind, TransientResourceRef,
};
use std::sync::Arc;

pub(in crate::manager::tests) async fn wait_until_access_lease_store_is_write_locked(manager: &HlsProxyManager) {
    for _ in 0..1_000 {
        if manager.access_leases.try_write().is_err() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("access lease store was not locked by the reconciliation transaction");
}

#[tokio::test]
async fn critical_handoff_distinguishes_lock_busy_from_acquired_state() {
    let (manager, session, _, _, _) = live_media_fixture("critical-lock-busy").await;
    let lease_guard = manager.access_leases.write().await;

    let busy = manager.with_critical_handoff_state(&session, |_, _| 7_u8).await;

    assert_eq!(busy, HlsCriticalHandoffStateAccess::LockBusy);
    drop(lease_guard);

    let acquired = manager.with_critical_handoff_state(&session, |_, _| 7_u8).await;

    assert_eq!(acquired, HlsCriticalHandoffStateAccess::Acquired(7));
}

#[tokio::test]
async fn stale_session_handle_cannot_mark_recovered_session_with_same_public_id() {
    let (manager, stale_session, proxy_session_id, lease_id, lease_identity) =
        live_media_fixture("session-recovery").await;
    let session_key = stale_session.read().await.key.clone();
    manager.sessions.remove_session(&session_key, &proxy_session_id).await.expect("remove original session");
    let (recovered_session, outcome) = manager.get_or_create_session_with_outcome(session_key, b"secret", 2_000).await;
    assert_eq!(outcome, HlsSessionStoreOutcome::Created);
    assert!(!Arc::ptr_eq(&stale_session, &recovered_session));

    assert_eq!(
        manager
            .mark_authorized_media_access_for_lease_if_identity_matches(
                &stale_session,
                &lease_id,
                &proxy_session_id,
                lease_identity,
                2_100,
            )
            .await,
        HlsMediaActivityCommitOutcome::StaleLeaseIdentity
    );
    assert_eq!(stale_session.read().await.activity.last_authorized_media_at_ms, None);
    assert_eq!(recovered_session.read().await.activity.last_authorized_media_at_ms, None);
}

#[tokio::test]
#[allow(clippy::similar_names, clippy::too_many_lines)]
async fn finalized_signed_url_generations_follow_published_lease_lifetime() {
    let manager = HlsProxyManager::new();
    let published_resource_ids = HlsPublishedTransientResourceIds::default();
    let session = manager.get_or_create_session(HlsSessionKey::new(1, "archive"), b"secret", 0).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_a_id = HlsAccessLeaseId("lease-a".to_string());
    let lease_b_id = HlsAccessLeaseId("lease-b".to_string());
    let resource_a = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "https://cdn.example/archive/segment.ts?token=A",
        b"secret",
        0,
        20,
        Some("ts".to_string()),
    );
    let resource_a_id = resource_a.id.clone();
    let resource_a_uri = format!("/hls/shared/live/{}/lease-a/r/{}.ts", proxy_session_id.0, resource_a_id.0);
    let generation_a = {
        let mut session = session.write().await;
        session.transient.upsert_resources([resource_a]);
        session.transient.replace_manifest_with_semantics(
            format!(
                "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-TARGETDURATION:6\n#EXTINF:6,\n{resource_a_uri}\n#EXT-X-ENDLIST\n"
            ),
            0,
            Some(6_000),
        );
        session.transient.current_finalized_manifest_generation().expect("G1 finalized generation")
    };

    let lease_a = HlsAccessLease::pending(
        lease_a_id.clone(),
        HlsPlaybackFamilyKey::new("alice", "client-a"),
        proxy_session_id.clone(),
        "alice".to_string(),
        "session-a".to_string(),
        1,
        "archive".to_string(),
        1,
        0,
        40,
    );
    manager.prepare_access_lease(lease_a).await;
    assert!(
        publish_manifest_snapshot(
            &manager,
            &lease_a_id,
            &proxy_session_id,
            finalized_transient_manifest_snapshot(0, generation_a, &resource_a_uri),
            1,
        )
        .await
    );

    let resource_b = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "https://cdn.example/archive/segment.ts?token=B",
        b"secret",
        21,
        20,
        Some("ts".to_string()),
    );
    let resource_b_id = resource_b.id.clone();
    assert_ne!(resource_a_id, resource_b_id);
    let resource_b_uri = format!("/hls/shared/live/{}/lease-b/r/{}.ts", proxy_session_id.0, resource_b_id.0);
    let generation_b = {
        let mut session = session.write().await;
        session.transient.upsert_resources([resource_b]);
        session.transient.replace_manifest_with_semantics(
            format!(
                "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-TARGETDURATION:6\n#EXTINF:6,\n{resource_b_uri}\n#EXT-X-ENDLIST\n"
            ),
            21,
            Some(6_000),
        );
        session.transient.current_finalized_manifest_generation().expect("G2 finalized generation")
    };

    assert!(session
        .read()
        .await
        .transient
        .resolve_resource_for_lease(&resource_a_id, &lease_a_id, 0, &published_resource_ids, 21)
        .is_some());
    assert_eq!(session.read().await.transient.finalized_manifest_generation_count(), 2);

    let lease_b = HlsAccessLease::pending(
        lease_b_id.clone(),
        HlsPlaybackFamilyKey::new("bob", "client-b"),
        proxy_session_id.clone(),
        "bob".to_string(),
        "session-b".to_string(),
        1,
        "archive".to_string(),
        1,
        21,
        100,
    );
    manager.prepare_access_lease(lease_b).await;
    assert!(
        publish_manifest_snapshot(
            &manager,
            &lease_b_id,
            &proxy_session_id,
            finalized_transient_manifest_snapshot(21, generation_b, &resource_b_uri),
            22,
        )
        .await
    );
    assert!(session
        .read()
        .await
        .transient
        .resolve_resource_for_lease(&resource_b_id, &lease_b_id, 21, &published_resource_ids, 42)
        .is_some());
    assert!(session
        .read()
        .await
        .transient
        .resolve_resource_for_lease(&resource_a_id, &lease_a_id, 0, &published_resource_ids, 39)
        .is_some());
    assert_eq!(session.read().await.transient.finalized_manifest_lease_binding_count(), 2);
    {
        let session = session.read().await;
        assert_eq!(session.transient.current_manifest_resource_ids().len(), 1);
        assert!(session.transient.resolve_current_resource(&resource_b_id, 42).is_some());
    }

    let expired_a = manager
        .access_lease_response_snapshot(&lease_a_id, &proxy_session_id, 41)
        .await
        .expect("expired lease remains available for lifecycle cleanup");
    assert_eq!(expired_a.state, HlsAccessLeaseState::Expired);
    manager.remove_access_lease(&lease_a_id).await;
    let mut session = session.write().await;
    assert_eq!(session.transient.finalized_manifest_generation_count(), 1);
    assert_eq!(session.transient.finalized_manifest_lease_binding_count(), 1);
    assert!(session.transient.resolve_current_resource(&resource_a_id, 41).is_none());
    session.transient.prune_expired(41);
    assert!(!session.transient.resources.contains_key(&resource_a_id));
    assert!(session.transient.resolve_current_resource(&resource_b_id, 42).is_some());
}

#[tokio::test]
#[allow(clippy::similar_names, clippy::too_many_lines)]
async fn same_lease_retains_every_published_finalized_generation_until_removal() {
    let manager = HlsProxyManager::new();
    let published_resource_ids = HlsPublishedTransientResourceIds::default();
    let session = manager.get_or_create_session(HlsSessionKey::new(1, "archive"), b"secret", 0).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    manager
        .prepare_access_lease(HlsAccessLease::pending(
            lease_id.clone(),
            HlsPlaybackFamilyKey::new("alice", "client-a"),
            proxy_session_id.clone(),
            "alice".to_string(),
            "session-a".to_string(),
            1,
            "archive".to_string(),
            1,
            0,
            100,
        ))
        .await;

    let (resource_a_id, generation_a, resource_a_uri) = {
        let resource = TransientResourceRef::new(
            TransientResourceKind::Segment,
            "https://cdn.example/archive/segment.ts?token=A",
            b"secret",
            0,
            20,
            Some("ts".to_string()),
        );
        let resource_id = resource.id.clone();
        let resource_uri = format!("/hls/shared/live/{}/lease-a/r/{}.ts", proxy_session_id.0, resource_id.0);
        let mut session = session.write().await;
        session.transient.upsert_resources([resource]);
        session.transient.replace_manifest_with_semantics(
            format!(
                "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-TARGETDURATION:6\n#EXTINF:6,\n{resource_uri}\n#EXT-X-ENDLIST\n"
            ),
            0,
            Some(6_000),
        );
        let generation = session.transient.current_finalized_manifest_generation().expect("G1 finalized generation");
        (resource_id, generation, resource_uri)
    };
    assert!(
        publish_manifest_snapshot(
            &manager,
            &lease_id,
            &proxy_session_id,
            finalized_transient_manifest_snapshot(0, generation_a, &resource_a_uri),
            1,
        )
        .await
    );

    let (resource_b_id, generation_b, resource_b_uri) = {
        let resource = TransientResourceRef::new(
            TransientResourceKind::Segment,
            "https://cdn.example/archive/segment.ts?token=B",
            b"secret",
            21,
            20,
            Some("ts".to_string()),
        );
        let resource_id = resource.id.clone();
        let resource_uri = format!("/hls/shared/live/{}/lease-a/r/{}.ts", proxy_session_id.0, resource_id.0);
        let mut session = session.write().await;
        session.transient.upsert_resources([resource]);
        session.transient.replace_manifest_with_semantics(
            format!(
                "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-TARGETDURATION:6\n#EXTINF:6,\n{resource_uri}\n#EXT-X-ENDLIST\n"
            ),
            21,
            Some(6_000),
        );
        let generation = session.transient.current_finalized_manifest_generation().expect("G2 finalized generation");
        (resource_id, generation, resource_uri)
    };
    assert_ne!(resource_a_id, resource_b_id);
    assert!(
        publish_manifest_snapshot(
            &manager,
            &lease_id,
            &proxy_session_id,
            finalized_transient_manifest_snapshot(21, generation_b, &resource_b_uri),
            22,
        )
        .await
    );

    {
        let session = session.read().await;
        assert_eq!(session.transient.finalized_manifest_lease_binding_count(), 2);
        assert!(session
            .transient
            .resolve_resource_for_lease(&resource_a_id, &lease_id, 0, &published_resource_ids, 42)
            .is_some());
        assert!(session
            .transient
            .resolve_resource_for_lease(&resource_b_id, &lease_id, 0, &published_resource_ids, 42)
            .is_some());
    }

    manager.remove_access_lease(&lease_id).await;
    let session = session.read().await;
    assert_eq!(session.transient.finalized_manifest_lease_binding_count(), 0);
    assert_eq!(session.transient.finalized_manifest_generation_count(), 1);
    assert!(session.transient.resolve_current_resource(&resource_a_id, 42).is_none());
    assert!(session.transient.resolve_current_resource(&resource_b_id, 42).is_some());
}

#[tokio::test]
async fn lease_reconciliation_cannot_overwrite_a_concurrent_manifest_publication() {
    let manager = Arc::new(HlsProxyManager::new());
    let session = manager.get_or_create_session(HlsSessionKey::new(1, "archive"), b"secret", 0).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let resource = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "https://cdn.example/archive/segment.ts?token=A",
        b"secret",
        0,
        20,
        Some("ts".to_string()),
    );
    let resource_uri = format!("/hls/shared/live/{}/lease-a/r/{}.ts", proxy_session_id.0, resource.id.0);
    let generation = {
        let mut session = session.write().await;
        session.transient.upsert_resources([resource]);
        session.transient.replace_manifest_with_semantics(
            format!(
                "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-TARGETDURATION:6\n#EXTINF:6,\n{resource_uri}\n#EXT-X-ENDLIST\n"
            ),
            0,
            Some(6_000),
        );
        session.transient.current_finalized_manifest_generation().expect("finalized generation")
    };
    manager.prepare_access_lease(access_lease(&lease_id.0, &proxy_session_id)).await;
    let publication_guard = manager
        .prepare_access_lease_manifest_publication(&lease_id, &proxy_session_id, 1)
        .await
        .expect("publication guard");

    let session_guard = session.write().await;
    let reconcile_task = {
        let manager = Arc::clone(&manager);
        let session = Arc::clone(&session);
        let proxy_session_id = proxy_session_id.clone();
        tokio::spawn(
            async move { manager.reconcile_session_access_lease_snapshot(&session, &proxy_session_id, 1).await },
        )
    };
    wait_until_access_lease_store_is_write_locked(&manager).await;
    let publication_task = {
        let manager = Arc::clone(&manager);
        let proxy_session_id = proxy_session_id.clone();
        let lease_id = lease_id.clone();
        tokio::spawn(async move {
            manager
                .commit_access_lease_manifest_publication(
                    &lease_id,
                    &proxy_session_id,
                    publication_guard,
                    finalized_transient_manifest_snapshot(0, generation, &resource_uri),
                    2,
                )
                .await
        })
    };
    drop(session_guard);

    reconcile_task.await.expect("reconciliation completes");
    assert!(publication_task.await.expect("publication completes").is_committed());
    assert_eq!(session.read().await.transient.finalized_manifest_lease_binding_count(), 1);
}

#[tokio::test]
async fn lease_reconciliation_cannot_resurrect_a_concurrently_removed_binding() {
    let manager = Arc::new(HlsProxyManager::new());
    let session = manager.get_or_create_session(HlsSessionKey::new(1, "archive"), b"secret", 0).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let resource = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "https://cdn.example/archive/segment.ts?token=A",
        b"secret",
        0,
        20,
        Some("ts".to_string()),
    );
    let resource_uri = format!("/hls/shared/live/{}/lease-a/r/{}.ts", proxy_session_id.0, resource.id.0);
    let generation = {
        let mut session = session.write().await;
        session.transient.upsert_resources([resource]);
        session.transient.replace_manifest_with_semantics(
            format!(
                "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-TARGETDURATION:6\n#EXTINF:6,\n{resource_uri}\n#EXT-X-ENDLIST\n"
            ),
            0,
            Some(6_000),
        );
        session.transient.current_finalized_manifest_generation().expect("finalized generation")
    };
    manager.prepare_access_lease(access_lease(&lease_id.0, &proxy_session_id)).await;
    assert!(
        publish_manifest_snapshot(
            &manager,
            &lease_id,
            &proxy_session_id,
            finalized_transient_manifest_snapshot(0, generation, &resource_uri),
            1,
        )
        .await
    );
    let removal =
        manager.access_leases.read().await.prepare_access_lease_removal(&lease_id).expect("removal preparation");

    let session_guard = session.write().await;
    let reconcile_task = {
        let manager = Arc::clone(&manager);
        let session = Arc::clone(&session);
        let proxy_session_id = proxy_session_id.clone();
        tokio::spawn(
            async move { manager.reconcile_session_access_lease_snapshot(&session, &proxy_session_id, 1).await },
        )
    };
    wait_until_access_lease_store_is_write_locked(&manager).await;
    let removal_task = {
        let manager = Arc::clone(&manager);
        let lease_id = lease_id.clone();
        tokio::spawn(async move { manager.remove_prepared_access_lease(&lease_id, &removal).await })
    };
    drop(session_guard);

    reconcile_task.await.expect("reconciliation completes");
    assert!(removal_task.await.expect("removal completes"));
    assert_eq!(session.read().await.transient.finalized_manifest_lease_binding_count(), 0);
}
