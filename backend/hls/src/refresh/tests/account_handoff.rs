use super::{
    mark_origin_refresh_started_with_outcome, refresh_and_commit, test_origin_refresh_request, test_session,
    HlsOriginRefreshTriggerOutcome,
};
use crate::{HlsCtx, HlsOriginIoContext, HlsSessionHandle};
use std::sync::Arc;
use tuliprox_core::model::{Config, PlaybackKind};
use tuliprox_session::ConnectionKind;

const STARTUP_MANIFEST_202: &str = "#EXTM3U
#EXT-X-VERSION:3
#EXT-X-MEDIA-SEQUENCE:202
#EXT-X-ALLOW-CACHE:YES
#EXT-X-TARGETDURATION:11
#EXTINF:8.960000,
/hls/e436ba69ca3d71ae7fa01ff4e3520a5c/1025125_202.ts
#EXTINF:10.240000,
/hls/634a94edeffb7663ecc730c99396b673/1025125_203.ts
#EXTINF:10.240000,
/hls/7aea19bc02ebac368301e6d538023539/1025125_204.ts
#EXTINF:10.240000,
/hls/5f8a487f200ad89281761f3870e10192/1025125_205.ts
#EXTINF:10.240000,
/hls/8d9f709355939e86eb4d23080ff8684e/1025125_206.ts
#EXTINF:8.960000,
/hls/56e35e98f530ba62afd77fb5d41a2e5f/1025125_207.ts
";

const STARTUP_MANIFEST_203: &str = "#EXTM3U
#EXT-X-VERSION:3
#EXT-X-MEDIA-SEQUENCE:203
#EXT-X-ALLOW-CACHE:YES
#EXT-X-TARGETDURATION:11
#EXTINF:10.240000,
/hls/634a94edeffb7663ecc730c99396b673/1025125_203.ts
#EXTINF:10.240000,
/hls/7aea19bc02ebac368301e6d538023539/1025125_204.ts
#EXTINF:10.240000,
/hls/5f8a487f200ad89281761f3870e10192/1025125_205.ts
#EXTINF:10.240000,
/hls/8d9f709355939e86eb4d23080ff8684e/1025125_206.ts
#EXTINF:8.960000,
/hls/56e35e98f530ba62afd77fb5d41a2e5f/1025125_207.ts
#EXTINF:10.240000,
/hls/71e5f3e7aa278e45b799c0fefbb34afe/1025125_208.ts
";

fn handoff_startup_account(session: &mut crate::HlsSession, generation: u64, now_ms: u64) {
    session.handoff_origin_account(crate::HlsOriginAccountBinding::rebound(
        Arc::from("input"),
        Arc::from(format!("account-{generation}")),
        crate::origin::build_hls_origin_session_owner(&session.proxy_session_id),
        generation,
        now_ms,
    ));
}

#[tokio::test]
async fn published_rebase_continuation_reuses_cached_media_after_admitted_account_restart() {
    let session = test_session();
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.manifest_commit_requirement = super::HlsManifestCommitRequirement::FreshCommitRequired {
        reason: crate::HlsFreshManifestRequiredReason::ExpiredRevalidation,
    };
    let mut current = session.write().await;
    handoff_startup_account(&mut current, 1, 0);
    super::commit_fetched_manifest(&mut current, &super::fetched_manifest(STARTUP_MANIFEST_202), &request, 9_000)
        .unwrap();
    for segment in current.segments.values_mut() {
        segment.status = crate::SegmentCacheStatus::Ready { content_length: 1_024, ready_at_ms: 12_000 };
    }
    current.render_and_store_manifest(12_000).unwrap();
    let epoch = current.origin_epoch;
    let cached = current.segments[&1].clone();
    let owner = crate::origin::build_hls_origin_session_owner(&current.proxy_session_id);
    current.restart_admitted_origin_account(crate::HlsOriginAccountBinding::rebound(
        Arc::from("input"),
        Arc::from("account-2"),
        owner,
        2,
        46_000,
    ));

    let next = super::fetched_manifest(STARTUP_MANIFEST_203);
    super::commit_fetched_manifest(&mut current, &next, &request, 46_000).unwrap();

    assert_eq!(current.live_origin_sequences(), &[1, 2, 3, 4, 5, 6]);
    assert_eq!(current.origin_epoch, epoch);
    assert_eq!(current.origin_seq_highwater, Some(208));
    assert_eq!(current.proxy_next_seq, Some(7));
    assert_eq!(current.segments[&1].cache_key, cached.cache_key);
    assert_eq!(current.segments[&1].status, cached.status);
    assert!(!current.segments[&6].discontinuity_before);
    assert!(current.origin_account_manifest_pending.is_none());
    let anchor = current.origin_account_binding.as_ref().unwrap().startup_protection;
    assert_eq!(anchor.unwrap().anchored_at_ms, 46_000);
    current.render_and_store_manifest(46_000).expect("cached overlap already contains a usable entry window");

    super::commit_fetched_manifest(&mut current, &next, &request, 47_000).unwrap();
    assert_eq!(current.origin_epoch, epoch);
    assert_eq!(current.proxy_next_seq, Some(7));
    assert_eq!(current.origin_account_binding.as_ref().unwrap().startup_protection, anchor);
}

#[tokio::test]
async fn account_handoff_preserves_unpublished_overlap_and_allows_startup_render() {
    let session = test_session();
    let request = test_origin_refresh_request(Arc::clone(&session));
    let first = super::fetched_manifest(STARTUP_MANIFEST_202);
    let second = super::fetched_manifest(STARTUP_MANIFEST_203);
    {
        let mut current = session.write().await;
        handoff_startup_account(&mut current, 1, 0);
        super::commit_fetched_manifest(&mut current, &first, &request, 9_000).unwrap();
        assert!(current.last_rendered_manifest.is_none());
        assert_eq!(
            current.origin_account_binding.as_ref().unwrap().startup_protection.unwrap().reservation_deadline_ms(),
            43_000
        );
        let identity = current.segments[&1].media_resource_identity();
        assert!(identity.is_some());
        // Repeated handoffs before the first media download must retain the same evidence.
        for generation in [2, 3] {
            handoff_startup_account(&mut current, generation, 9_000);
            assert!(current.segments[&1].origin_fetch_ref.is_none());
            assert_eq!(current.segments[&1].media_resource_identity(), identity);
        }
    }
    let context = super::manifest_fetch_context(&request, Some(11));
    super::super::commit_initial_fetched_manifest(&request, &context, second.clone(), None, false)
        .await
        .expect("unchanged overlap must commit without established recovery binding");
    let mut current = session.write().await;
    assert!(current.origin_account_manifest_pending.is_none());
    assert_eq!(current.origin_seq_highwater, Some(208));
    for proxy_seq in 1..=6 {
        let segment = current.segments.get_mut(&proxy_seq).unwrap();
        assert!(segment.origin_fetch_ref.is_some(), "overlap must be downloadable again");
        segment.status = crate::SegmentCacheStatus::Ready { content_length: 1_024, ready_at_ms: 12_000 };
    }
    let rendered = current.render_and_store_manifest(12_000).expect("startup media window must render");
    assert_eq!(rendered.segment_proxy_seqs, vec![1, 2, 3, 4, 5, 6]);
    let deadline = current.origin_account_binding.as_ref().unwrap().startup_protection;
    assert!(deadline.is_some());
    let mut later = second;
    later.body = later.body.replace("TARGETDURATION:11", "TARGETDURATION:22");
    super::commit_fetched_manifest(&mut current, &later, &request, 20_000).unwrap();
    assert_eq!(current.origin_account_binding.as_ref().unwrap().startup_protection, deadline);
}

#[tokio::test]
async fn account_handoff_renews_the_fetch_reference_of_an_unchanged_uncached_map() {
    let session = test_session();
    let request = test_origin_refresh_request(Arc::clone(&session));
    let with_map = |body: &str| {
        body.replace("#EXT-X-TARGETDURATION:11\n", "#EXT-X-TARGETDURATION:11\n#EXT-X-MAP:URI=\"init.mp4\"\n")
    };
    let first = super::fetched_manifest(&with_map(STARTUP_MANIFEST_202));
    let second = super::fetched_manifest(&with_map(STARTUP_MANIFEST_203));
    let mut current = session.write().await;
    super::commit_fetched_manifest(&mut current, &first, &request, 9_000).unwrap();
    let map_id = current.segments[&1].map_ref.unwrap();
    handoff_startup_account(&mut current, 1, 9_000);
    assert!(current.maps[&map_id].origin_fetch_ref.is_none());
    super::commit_fetched_manifest(&mut current, &second, &request, 12_000).unwrap();
    assert_eq!(current.maps.len(), 1);
    assert_eq!(current.segments[&1].map_ref, Some(map_id));
    assert_eq!(
        current.maps[&map_id].origin_fetch_ref.as_ref().unwrap().resolved_origin_url,
        "http://origin.example.com/live/final/init.mp4"
    );
}

#[tokio::test]
async fn account_handoff_rejects_changed_resource_or_byte_range_at_existing_sequence() {
    for body in [
        STARTUP_MANIFEST_203.replace("1025125_203.ts", "different_203.ts"),
        STARTUP_MANIFEST_203.replace(
            "/hls/634a94edeffb7663ecc730c99396b673/1025125_203.ts",
            "#EXT-X-BYTERANGE:512@0\n/hls/634a94edeffb7663ecc730c99396b673/1025125_203.ts",
        ),
    ] {
        let session = test_session();
        let request = test_origin_refresh_request(Arc::clone(&session));
        let mut current = session.write().await;
        super::commit_fetched_manifest(&mut current, &super::fetched_manifest(STARTUP_MANIFEST_202), &request, 9_000)
            .unwrap();
        handoff_startup_account(&mut current, 1, 9_000);
        let result = super::commit_fetched_manifest(&mut current, &super::fetched_manifest(&body), &request, 12_000);
        assert!(matches!(
            result,
            Err(super::HlsManifestCommitError::TimelineRejected {
                reason: super::HlsManifestRejectLogReason::OriginSequenceResourceConflict {
                    existing_proxy_seq: 1,
                    candidate_origin_seq: 203,
                },
            })
        ));
        assert_eq!(current.origin_seq_highwater, Some(207), "a rejected candidate cannot mutate the timeline");
        assert!(current.origin_account_manifest_pending.is_some());
        assert!(current.segments[&1].origin_fetch_ref.is_none());
        assert!(current.origin_account_binding.as_ref().unwrap().startup_media_deadline_ms.is_none());
    }
}

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
    let next = super::fetched_manifest("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:10\n#EXT-X-KEY:METHOD=AES-128,URI=\"shared.key\"\n#EXTINF:4,\n10.ts\n#EXTINF:4,\n11.ts\n#EXTINF:4,\n12.ts\n#EXTINF:4,\n13.ts\n");
    super::commit_fetched_manifest(&mut current, &next, &request, 102).unwrap();
    assert_eq!(current.segments[&0].encryption.as_ref().unwrap().resource_id, old_key);
    assert_ne!(current.segments[&1].encryption.as_ref().unwrap().resource_id, old_key);
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
