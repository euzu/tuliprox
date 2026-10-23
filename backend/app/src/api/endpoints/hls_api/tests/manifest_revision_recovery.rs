use super::{normal_manifest, response_body, test_app_state_with_hls_proxy, try_test_hls_cached_manifest_response};
use crate::api::model::{
    HlsAccessLease, HlsAccessLeaseId, HlsAccessLeaseState, HlsPlaybackFamilyKey, HlsProxyManager, HlsSessionKey,
    SegmentCacheStatus,
};
use axum::http::{HeaderMap, StatusCode};
use shared::model::{HlsCacheConfigDto, HlsStartupMode};
use std::{fmt::Write as _, sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tuliprox_core::{
    model::{HlsCacheConfig, StripConfig},
    utils::current_time_millis,
};
use tuliprox_hls::api::SegmentFetchContext;

#[tokio::test]
#[allow(clippy::too_many_lines, reason = "One deterministic held-body scheduling and HTTP publication scenario.")]
async fn retained_playlist_request_is_released_by_successor_headers_while_bodies_stay_open() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("origin");
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let (headers, header_gate) = tokio::sync::watch::channel(false);
    let (bodies, body_gate) = tokio::sync::watch::channel(false);
    let origin_task = tokio::spawn(async move {
        let mut children = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                socket = listener.accept() => {
                    let (socket, _) = socket.unwrap();
                    let mut headers = header_gate.clone();
                    let mut bodies = body_gate.clone();
                    children.spawn(async move {
                        let mut socket = BufReader::new(socket);
                        let mut request = String::new();
                        socket.read_line(&mut request).await.unwrap();
                        let head = request.contains("/103.ts ");
                        loop {
                            let mut line = String::new();
                            socket.read_line(&mut line).await.unwrap();
                            if line == "\r\n" || line.is_empty() { break; }
                        }
                        let mut socket = socket.into_inner();
                        if !head {
                            while !*headers.borrow_and_update() { headers.changed().await.unwrap(); }
                        }
                        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\n").await.unwrap();
                        if head { socket.write_all(b"segment-").await.unwrap(); }
                        while !*bodies.borrow_and_update() { bodies.changed().await.unwrap(); }
                        socket.write_all(if head { b"body".as_slice() } else { b"segment-body".as_slice() }).await.unwrap();
                    });
                }
                _ = children.join_next(), if !children.is_empty() => {}
            }
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let mut dto = HlsCacheConfigDto {
        cache_path: Some(directory.path().to_string_lossy().into_owned()),
        max_concurrent_segment_fetches_per_session: 3,
        ..Default::default()
    };
    dto.startup.mode = HlsStartupMode::Optimistic;
    let manager = Arc::new(HlsProxyManager::with_hls_cache_config(&HlsCacheConfig::from(&dto)));
    let app = test_app_state_with_hls_proxy(manager.clone());
    let session =
        manager.get_or_create_session(HlsSessionKey::new(1, "retained-render"), b"secret", current_time_millis()).await;
    let manifest = |first: u64| {
        let mut body = format!("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:{first}\n");
        for seq in first..first + 3 {
            writeln!(body, "#EXTINF:4,\n{origin}/{seq}.ts").unwrap();
        }
        normal_manifest(&body)
    };
    let old_rendered_at = current_time_millis().saturating_sub(1000);
    let (old_head, new_head) = {
        let mut session = session.write().await;
        let startup = session.startup.take();
        session.apply_origin_manifest(&manifest(100)).unwrap();
        for entry in session.segments.values_mut() {
            entry.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: old_rendered_at };
        }
        let old_head = session.render_and_store_manifest(old_rendered_at).unwrap().first_proxy_seq;
        session.apply_origin_manifest(&manifest(103)).unwrap();
        session.startup = startup;
        let new_head = session.publishable_origin_head_proxy_seq.unwrap();
        while session.segment_prefetch_queue.pop_next().is_some() {}
        let candidates = session
            .segments
            .iter()
            .filter(|(_, entry)| !matches!(entry.status, SegmentCacheStatus::Ready { .. }))
            .map(|(seq, _)| *seq)
            .collect::<Vec<_>>();
        for seq in candidates {
            session.segments.get_mut(&seq).unwrap().status = SegmentCacheStatus::Discovered;
            assert!(session.queue_segment_fetch_candidate(
                seq,
                tuliprox_hls::api::SegmentFetchPriority::Demand,
                current_time_millis()
            ));
        }
        (old_head, new_head)
    };
    let session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("waiting-reused-session".into());
    manager
        .prepare_access_lease(HlsAccessLease::pending(
            lease_id.clone(),
            HlsPlaybackFamilyKey::new("test-user", "render-gate"),
            session_id,
            "test-user".into(),
            "render-session".into(),
            1,
            "retained-render".into(),
            1,
            current_time_millis(),
            60_000,
        ))
        .await;
    let client = reqwest::Client::new();
    manager
        .segment_worker_pool()
        .wake_scheduler(
            SegmentFetchContext {
                session: session.clone(),
                segment_cache: manager.segment_cache().clone(),
                segment_repair: manager.segment_repair().clone(),
                repair_access_lease_id: None,
                headers: HeaderMap::new(),
                origin_provider_session_headers: HeaderMap::new(),
                client: client.clone(),
                no_redirect_client: client,
                use_manual_redirects: false,
                origin_io: None,
            },
            current_time_millis(),
        )
        .await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if session
                .read()
                .await
                .startup
                .as_ref()
                .unwrap()
                .revisions
                .values()
                .any(|revision| revision.revision().prefix_available.load(std::sync::atomic::Ordering::Acquire) > 0)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("head prefix");
    assert_eq!(session.read().await.last_rendered_manifest.as_ref().unwrap().first_proxy_seq, old_head);
    let request = {
        let app = app.clone();
        let session = session.clone();
        let lease_id = lease_id.clone();
        tokio::spawn(async move {
            try_test_hls_cached_manifest_response(
                &app,
                &session,
                &lease_id,
                HlsAccessLeaseState::Pending,
                &StripConfig { mode: shared::model::HlsStripMode::Segments, value: 0 },
                None,
                super::HlsCachedManifestOptions::initial(Duration::from_secs(2))
                    .requiring_newer_manifest(old_rendered_at),
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(!request.is_finished());
    headers.send_replace(true);
    let response = tokio::time::timeout(Duration::from_secs(1), request).await.unwrap().unwrap().expect("playlist");
    let answered_at = current_time_millis();
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
    assert!(body.contains(&format!("{new_head:06}.ts")));
    let state = session.read().await;
    let rendered = state.last_rendered_manifest.as_ref().unwrap();
    assert!(answered_at.saturating_sub(rendered.rendered_at_ms) <= 50, "release exceeded two poll intervals");
    assert!(state.startup.as_ref().unwrap().revisions.values().all(|revision| !revision.revision().is_complete()));
    assert!(!*bodies.borrow());
    drop(state);
    bodies.send_replace(true);
    origin_task.abort();
}
