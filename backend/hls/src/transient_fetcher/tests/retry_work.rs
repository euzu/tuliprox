use super::*;
use crate::HlsOriginRetryWork;
use tuliprox_core::model::PlaybackKind;
use tuliprox_session::{ConnectionKind, PlaybackLeaseRef};

#[tokio::test]
async fn direct_body_keeps_capacity_until_eof_drop_or_deadline_for_segments_maps_and_keys() {
    for kind in [TransientResourceKind::Segment, TransientResourceKind::Map, TransientResourceKind::Key] {
        for outcome in 0..3 {
            assert_direct_work_lifecycle(kind, outcome).await;
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "Keep body ownership, controlled expiry and provider cleanup in one scenario."
)]
async fn assert_direct_work_lifecycle(kind: TransientResourceKind, outcome: u8) {
    let origin = spawn_test_origin("200 OK", vec![], vec![b'x'; 512 * 1024]).await;
    let url = format!("{}/resource.bin", origin.base_url);
    let mut fixture = TestDirectResponseFixture::new(kind, url.clone());
    fixture.policy.timeouts = crate::HlsOriginTimeoutPolicy::fixed(10_000, 10_000);
    let (io, _) = crate::origin_attempt::tests::bound_origin_io(&fixture.session).await;
    let manager = Arc::clone(&io.ctx.active_provider);
    let binding = fixture.session.read().await.origin_account_binding.clone().unwrap();
    let client_addr = io.client_addr;
    let acquire = |owner: &str| {
        manager.acquire_exact_connection_with_lease_for_session(
            &binding.account_name,
            &client_addr,
            false,
            0,
            ConnectionKind::Normal,
            Some(PlaybackLeaseRef::new(owner, PlaybackKind::LiveHls)),
        )
    };
    let foreign = acquire("other-viewer").unwrap();
    let own = acquire(&binding.session_owner).unwrap();
    let tag = manager.reserve_hls_start(&binding.session_owner, &own, Duration::from_secs(7)).unwrap();
    manager.release_handle(&own);
    fixture.session.write().await.origin_account_binding.as_mut().unwrap().provider_binding_tag = Some(tag);
    let work = HlsOriginRetryWork::for_transient(&fixture.session, &manager, &fixture.policy, kind).await;
    let other_work = if outcome == 2 {
        HlsOriginRetryWork::for_transient(&fixture.session, &manager, &fixture.policy, kind).await
    } else {
        None
    };
    let count = Arc::clone(&fixture.session.read().await.activity.active_origin_work_count);
    let generation = fixture.session.read().await.activity.origin_work_generation;
    let prepare_session = Arc::clone(&fixture.session);
    let request = HlsTransientOriginFetchRequest {
        resolved_origin_uri: url,
        origin_headers: HeaderMap::new(),
        origin_provider_session_headers: HeaderMap::new(),
        range_header: None,
        resource_file: TransientResourceFile { resource_id: fixture.resource.id.clone(), extension: "bin".into() },
        resource_kind: kind,
        clients: HlsOriginResourceClients {
            retry_work: work,
            session: Some(Arc::clone(&fixture.session)),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::new(),
            use_manual_redirects: false,
        },
        policy: fixture.policy.clone(),
        log_identity: fixture.log_identity.clone(),
    };
    let decoded = fetch_hls_transient_origin_response_with_attempt_prepare(request, move |attempt| {
        let session = Arc::clone(&prepare_session);
        let io = io.clone();
        async move {
            attempt.protect_origin_work(&io.ctx.active_provider, &session, generation, false).await?;
            let binding = session.read().await.origin_account_binding.clone().unwrap();
            let lease = crate::begin_hls_origin_account_io_bounded(
                &io,
                &session,
                &binding,
                Duration::from_millis(attempt.timeout.effective_ms),
            )
            .await
            .map_err(HlsOriginResourceFetchError::ProviderUnavailable)?;
            attempt.protect_origin_work(&io.ctx.active_provider, &session, generation, true).await?;
            let started = session.write().await.start_origin_work();
            let counter = Arc::clone(&session.read().await.activity.active_origin_work_count);
            Ok(Some(HlsTransientOriginIoGuard::new(session, counter, io, lease, started)))
        }
        .boxed()
    })
    .await
    .unwrap();
    let response = fixture.response(decoded);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(8)).await;
    tokio::time::resume();
    assert_eq!(manager.binding_tag_for_owner(&binding.session_owner), Some(tag));
    assert!(acquire("competitor").is_none(), "unpolled body retains the last slot beyond normal expiry");
    assert!(count.load(Ordering::Acquire) > 0);
    match outcome {
        0 => assert_eq!(to_bytes(response.into_body(), usize::MAX).await.unwrap().len(), 512 * 1024),
        1 => drop(response),
        _ => {
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(3)).await;
            tokio::time::resume();
            assert!(to_bytes(response.into_body(), usize::MAX).await.is_err());
        }
    }
    let retained = usize::from(other_work.is_some());
    tokio::time::timeout(Duration::from_secs(2), async {
        while count.load(Ordering::Acquire) != retained || manager.get_provider_connections_count() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("body completion releases its own work and I/O handle");
    assert_eq!(fixture.session.read().await.activity.origin_work_generation, generation);
    assert!(fixture.session.read().await.activity.last_delivered_media_at_ms.is_none());
    if other_work.is_some() {
        assert!(acquire("competitor").is_none(), "one failed body preserves other shared work");
        assert_eq!(manager.binding_tag_for_owner(&binding.session_owner), Some(tag));
    }
    drop(other_work);
    assert_eq!(count.load(Ordering::Acquire), 0);
    let competitor = acquire("competitor").expect("no lease or GC wait after the last body completes");
    assert_eq!(origin.requests.lock().await.len(), 1, "no retry after headers or exposed bytes");
    manager.release_handle(&competitor);
    manager.release_handle(&foreign);
}
