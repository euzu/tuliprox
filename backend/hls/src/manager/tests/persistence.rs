use super::{access_lease, finalized_transient_manifest_snapshot, HlsLeaseManifestPublicationOutcome, HlsProxyManager};
use crate::{
    HlsAccessLeaseId, HlsPublishedTransientResourceIds, HlsSessionKey, TransientResourceKind, TransientResourceRef,
};
use std::sync::Arc;

#[tokio::test]
async fn finalized_manifest_publication_blocks_session_replacement_until_binding_is_committed() {
    let manager = Arc::new(HlsProxyManager::new());
    let session_key = HlsSessionKey::new(1, "archive-publication-race");
    let session = manager.get_or_create_session(session_key.clone(), b"secret", 1_000).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let resource = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "https://cdn.example/archive/segment.ts",
        b"secret",
        1_000,
        1,
        Some("ts".to_string()),
    );
    let resource_id = resource.id.clone();
    let resource_uri = format!("/hls/shared/live/{}/lease-a/r/{}.ts", proxy_session_id.0, resource_id.0);
    let manifest_body = format!("#EXTM3U\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXTINF:6,\n{resource_uri}\n#EXT-X-ENDLIST\n");
    let generation = {
        let mut session = session.write().await;
        session.transient.upsert_resources([resource]);
        session.transient.replace_manifest_with_semantics(manifest_body.clone(), 1_000, Some(6_000));
        session.transient.current_finalized_manifest_generation().expect("finalized generation")
    };
    manager.prepare_access_lease(access_lease(&lease_id.0, &proxy_session_id)).await;
    let publication = manager
        .prepare_access_lease_manifest_publication(&lease_id, &proxy_session_id, 2_000)
        .await
        .expect("publication guard");
    let published_resource_ids = HlsPublishedTransientResourceIds::from_manifest_body(&manifest_body);
    let lease_store_guard = manager.hold_access_lease_store_for_test().await;
    let publication_task = {
        let manager = Arc::clone(&manager);
        let lease_id = lease_id.clone();
        let proxy_session_id = proxy_session_id.clone();
        let published_resource_ids = published_resource_ids.clone();
        tokio::spawn(async move {
            manager
                .commit_access_lease_manifest_publication_with_resources(
                    &lease_id,
                    &proxy_session_id,
                    publication,
                    finalized_transient_manifest_snapshot(1_000, generation, &resource_uri),
                    published_resource_ids,
                    2_000,
                )
                .await
        })
    };

    let mut index_guard_observed = false;
    for _ in 0..1_000 {
        if manager.sessions.index_write_is_blocked_for_test() {
            index_guard_observed = true;
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(index_guard_observed, "publication did not acquire the session-index guard");
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(25),
            manager.sessions.remove_session(&session_key, &proxy_session_id),
        )
        .await
        .is_err(),
        "session replacement completed before finalized publication"
    );

    drop(lease_store_guard);
    assert_eq!(
        publication_task.await.expect("publication task"),
        HlsLeaseManifestPublicationOutcome::Committed { snapshot_generation: 1 }
    );
    let current_session =
        manager.sessions.get_by_proxy_session_id(&proxy_session_id).await.expect("current session remains indexed");
    assert!(Arc::ptr_eq(&current_session, &session));
    assert!(current_session
        .read()
        .await
        .transient
        .resolve_resource_for_lease(&resource_id, &lease_id, 1_000, &published_resource_ids, 2_000)
        .is_some());
}
