//! A deliberate member handoff admits the new member as a fresh baseline without
//! content-anchor equality, even when the new member's media sequence is below or
//! overlapping the retained highwater and both members share the same origin host.

use super::*;
use crate::{
    api::HlsOriginSourceKind, HlsFreshManifestRequiredReason, HlsOriginAccountBinding, HlsOriginSource, HlsProxyManager,
};
use axum::{body::Body, extract::State, http::Uri, response::Response, routing::any, Router};
use shared::model::{HlsCacheConfigDto, HlsStartupConfigDto, HlsStartupMode};
use std::{fmt::Write as _, sync::Arc};

fn media(marker: u8) -> Vec<u8> {
    let mut bytes = CRITICAL_HANDOFF_TS_BODY.to_vec();
    bytes.extend_from_slice(&[0x47, 0x1f, 0xff, 0x10]);
    bytes.extend_from_slice(&[marker; 184]);
    bytes
}

#[derive(Clone)]
struct OriginState {
    baseline_bytes: Arc<Vec<u8>>,
}

struct Origin {
    base: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Origin {
    fn drop(&mut self) { self.task.abort(); }
}

impl Origin {
    async fn start() -> Self {
        let state = OriginState { baseline_bytes: Arc::new(media(1)) };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .fallback(any(|State(state): State<OriginState>, uri: Uri| async move {
                if path_has_extension(uri.path(), "m3u8") {
                    return Response::new(Body::empty());
                }
                // The first member serves READY media; the new member's media is
                // deliberately unavailable so a pre-fix probe cannot complete.
                if uri.path().contains("new-member") {
                    return Response::builder().status(404).body(Body::empty()).unwrap();
                }
                Response::new(Body::from(state.baseline_bytes.as_ref().clone()))
            }))
            .with_state(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { base, task }
    }
}

fn member_manifest(base: &str, namespace: &str, media_sequence: u64, count: u64) -> FetchedOriginManifest {
    let mut body = format!("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:{media_sequence}\n");
    for seq in 0..count {
        writeln!(body, "#EXTINF:4,\n{base}/{namespace}/{}.ts", media_sequence + seq).unwrap();
    }
    let mut fetched = fetched_manifest(&body);
    fetched.final_manifest_url = format!("{base}/index.m3u8");
    fetched.resolved_request_url = fetched.final_manifest_url.clone();
    fetched.redirect_host = Some("127.0.0.1".into());
    fetched
}

async fn warm_baseline(origin: &Origin) -> OriginRefreshRequest {
    let manager = Arc::new(HlsProxyManager::with_private_cache(&HlsCacheConfigDto {
        startup: HlsStartupConfigDto { mode: HlsStartupMode::FirstReady, ..Default::default() },
        ..Default::default()
    }));
    let session = manager.get_or_create_session(crate::HlsSessionKey::new(1, "12345"), b"secret", 0).await;
    let mut request = switch_test_request(Arc::clone(&session), Arc::clone(manager.segment_cache()), &origin.base);
    request.hls_proxy = manager;
    let fetched = member_manifest(&origin.base, "old-member", 0, 5);
    let context = manifest_fetch_context(&request, None);
    super::super::commit_initial_fetched_manifest(&request, &context, fetched, None, false).await.unwrap();
    let context = crate::SegmentFetchContext {
        session: Arc::clone(&session),
        segment_cache: Arc::clone(&request.segment_cache),
        segment_repair: Arc::clone(&request.segment_repair),
        repair_access_lease_id: None,
        headers: axum::http::HeaderMap::new(),
        origin_provider_session_headers: axum::http::HeaderMap::new(),
        client: request.client.clone(),
        no_redirect_client: request.no_redirect_client.clone(),
        use_manual_redirects: false,
        origin_io: None,
    };
    for seq in 0..5 {
        assert_eq!(
            request
                .segment_worker_pool
                .demand_fetch_and_wait(
                    context.clone(),
                    &crate::HlsSegmentFile { proxy_seq: seq, extension: "ts".into() },
                    100
                )
                .await,
            crate::segment_fetcher::SegmentDemandFetchOutcome::Ready
        );
    }
    assert_eq!(session.read().await.origin_seq_highwater, Some(4));
    request
}

async fn handoff_to_new_member(request: &mut OriginRefreshRequest) {
    {
        let mut session = request.session.write().await;
        let source =
            HlsOriginSource::new(2, Arc::from("input-b"), "other-stream", HlsOriginSourceKind::DirectMediaPlaylist);
        let binding = HlsOriginAccountBinding::new(
            Arc::from("input-b"),
            Arc::from("account-b"),
            &session.proxy_session_id.clone(),
            super::super::current_time_millis(),
        );
        session.handoff_origin_member(source, binding);
    }
    request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired {
        reason: HlsFreshManifestRequiredReason::ProvisioningHandoff,
    };
    request.fresh_manifest_requirement_generation = request
        .session
        .read()
        .await
        .fresh_manifest_commit_requirement_generation(HlsFreshManifestRequiredReason::ProvisioningHandoff);
}

async fn assert_fresh_baseline_commit(origin: &Origin, media_sequence: u64) {
    let mut request = warm_baseline(origin).await;
    let (epoch, proxy_next) = {
        let session = request.session.read().await;
        (session.origin_epoch, session.proxy_next_seq)
    };
    handoff_to_new_member(&mut request).await;
    let fetched = member_manifest(&origin.base, "new-member", media_sequence, 3);
    let result = super::super::origin_probe::commit_pinned_manifest(
        &request,
        &fetched,
        request.manifest_commit_requirement.acceptance_mode(),
    )
    .await;
    assert!(
        result.is_ok(),
        "member handoff below the old highwater must commit as a fresh baseline without probing media: {:?}",
        result.as_ref().err()
    );
    let session = request.session.read().await;
    assert_eq!(session.origin_epoch, epoch + 1, "the handoff must start a new origin epoch");
    assert_eq!(
        session.origin_seq_highwater,
        Some(media_sequence + 2),
        "the fresh baseline highwater is the new member's tail"
    );
    let first_proxy = proxy_next.expect("retained proxy sequence");
    assert!(
        session.segments[&first_proxy].discontinuity_before,
        "the first new-member segment must follow a discontinuity"
    );
    assert_eq!(session.origin_source.input_name.as_ref(), "input-b");
    assert!(session.fresh_manifest_commit_required.is_none(), "the provisioning handoff requirement must be cleared");
}

#[tokio::test]
async fn member_handoff_below_highwater_commits_a_fresh_baseline() {
    let origin = Origin::start().await;
    assert_fresh_baseline_commit(&origin, 0).await;
}

#[tokio::test]
async fn member_handoff_overlapping_highwater_commits_a_fresh_baseline() {
    let origin = Origin::start().await;
    assert_fresh_baseline_commit(&origin, 3).await;
}
