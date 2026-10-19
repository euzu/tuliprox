use super::{
    mark_origin_refresh_started_with_outcome, refresh_and_commit, test_origin_refresh_request, test_session,
    HlsOriginRefreshTriggerOutcome,
};
use crate::{HlsCtx, HlsOriginIoContext, HlsSessionHandle};
use std::sync::Arc;
use tuliprox_core::model::{Config, PlaybackKind};
use tuliprox_session::ConnectionKind;

pub(super) fn request_with_origin_work(session: &HlsSessionHandle) -> super::OriginRefreshRequest {
    let mut request = test_origin_refresh_request(Arc::clone(session));
    request.origin_io = Some(HlsOriginIoContext {
        ctx: HlsCtx::for_test(Config::default()),
        client_addr: "127.0.0.1:1234".parse().unwrap(),
        allow_grace: false,
        priority: 0,
        connection_kind: ConnectionKind::Normal,
        reservation_ttl_secs: 1,
        preacquired_provider_handle: None,
        started_generation: None,
        prepared_account: None,
        playback_kind: PlaybackKind::LiveHls,
        account_runtime: None,
    });
    request
}

#[tokio::test]
async fn invalidated_refresh_without_successful_handoff_releases_its_singleflight() {
    let session = test_session();
    let mut old = request_with_origin_work(&session);
    assert_eq!(mark_origin_refresh_started_with_outcome(&mut old, 100).await, HlsOriginRefreshTriggerOutcome::Started);
    // A failed account replacement invalidates work but publishes no successor state.
    session.write().await.invalidate_queued_origin_work();
    refresh_and_commit(old, 100).await;
    {
        let current = session.read().await;
        assert!(!current.origin_refresh.in_flight);
        assert_eq!(current.activity.active_origin_work_count.load(std::sync::atomic::Ordering::Acquire), 0);
    }
    let mut retry = request_with_origin_work(&session);
    assert_eq!(
        mark_origin_refresh_started_with_outcome(&mut retry, 100).await,
        HlsOriginRefreshTriggerOutcome::Started
    );
    session.write().await.invalidate_queued_origin_work();
    refresh_and_commit(retry, 100).await;
}

#[tokio::test]
async fn invalidated_refresh_cannot_release_a_successor_started_in_the_same_millisecond() {
    let session = test_session();
    let mut old = request_with_origin_work(&session);
    assert_eq!(mark_origin_refresh_started_with_outcome(&mut old, 100).await, HlsOriginRefreshTriggerOutcome::Started);
    {
        let mut current = session.write().await;
        current.invalidate_queued_origin_work();
        current.origin_refresh = super::OriginRefreshState::default();
    }
    let mut successor = request_with_origin_work(&session);
    assert_eq!(
        mark_origin_refresh_started_with_outcome(&mut successor, 100).await,
        HlsOriginRefreshTriggerOutcome::Started
    );
    let successor_state = session.read().await.origin_refresh.clone();
    refresh_and_commit(old, 100).await;
    {
        let current = session.read().await;
        assert!(current.origin_refresh.in_flight);
        assert!(Arc::ptr_eq(
            current.origin_refresh.refresh_identity.as_ref().unwrap(),
            successor_state.refresh_identity.as_ref().unwrap()
        ));
        assert_eq!(current.activity.active_origin_work_count.load(std::sync::atomic::Ordering::Acquire), 1);
    }
    session.write().await.invalidate_queued_origin_work();
    refresh_and_commit(successor, 100).await;
    assert!(!session.read().await.origin_refresh.in_flight);
}

#[tokio::test]
async fn account_handoff_preserves_cached_ciphertext_key_and_assigns_new_keys_to_uncached_media() {
    let session = test_session();
    let request = test_origin_refresh_request(Arc::clone(&session));
    let fetched = super::fetched_manifest("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:10\n#EXT-X-KEY:METHOD=AES-128,URI=\"shared.key\"\n#EXTINF:4,\n10.ts\n#EXTINF:4,\n11.ts\n#EXTINF:4,\n12.ts\n");
    let mut current = session.write().await;
    super::commit_fetched_manifest(&mut current, &fetched, &request, 100).unwrap();
    let old_key = current.segments[&0].encryption.clone().unwrap().resource_id;
    current.segments.get_mut(&0).unwrap().status =
        crate::SegmentCacheStatus::Ready { content_length: 16, ready_at_ms: 100 };
    let owner = current.proxy_session_id.clone();
    current.handoff_origin_account(crate::HlsOriginAccountBinding::rebound(
        Arc::from("input"),
        Arc::from("new"),
        owner.0,
        1,
        101,
    ));
    let next = super::fetched_manifest("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:13\n#EXT-X-KEY:METHOD=AES-128,URI=\"shared.key\"\n#EXTINF:4,\n13.ts\n#EXTINF:4,\n14.ts\n#EXTINF:4,\n15.ts\n");
    super::commit_fetched_manifest(&mut current, &next, &request, 102).unwrap();
    assert_eq!(current.segments[&0].encryption.as_ref().unwrap().resource_id, old_key);
    let new_key = &current.segments[&3].encryption.as_ref().unwrap().resource_id;
    assert_ne!(new_key, &old_key);
    assert!(current.transient.resources.contains_key(&old_key));
    assert!(current.transient.resources.contains_key(new_key));
}

#[tokio::test]
async fn refresh_start_rejects_a_changed_binding_even_if_the_work_generation_is_unchanged() {
    let session = test_session();
    let mut request = request_with_origin_work(&session);
    {
        let mut current = session.write().await;
        let owner = current.proxy_session_id.0.clone();
        current.replace_origin_account_binding(Some(crate::HlsOriginAccountBinding::rebound(
            Arc::from("input"),
            Arc::from("old"),
            owner.clone(),
            1,
            100,
        )));
        let snapshot = crate::origin_account_runtime::HlsOriginAccountSnapshot::from_session(&current);
        let generation = snapshot.generation;
        request.origin_io.as_mut().unwrap().prepared_account = Some(snapshot);
        current.replace_origin_account_binding(Some(crate::HlsOriginAccountBinding::rebound(
            Arc::from("input"),
            Arc::from("new"),
            owner,
            2,
            100,
        )));
        assert_eq!(current.activity.origin_work_generation, generation);
    }
    assert_eq!(
        mark_origin_refresh_started_with_outcome(&mut request, 100).await,
        HlsOriginRefreshTriggerOutcome::SessionUnavailable
    );
    assert!(!session.read().await.origin_refresh.in_flight);
}
