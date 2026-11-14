//! The refresh flow reports an unhealthy group member only for classified, repeated failures.

use super::{
    bind_refresh_request_to_app_state, manifest_body, spawn_test_origin, test_origin_refresh_request,
    trigger_origin_refresh_sync, LiveHlsOriginEntry,
};
use crate::{
    member_health::test_support::RecordingMemberRuntime, origin_account_runtime::HlsMemberUnhealthyReason,
    HlsSessionHandle, HlsSessionKey,
};
use std::sync::{
    atomic::{AtomicU16, Ordering},
    Arc,
};
use tuliprox_core::{model::Config, utils::current_time_millis};

struct Fixture {
    ctx: crate::HlsCtx,
    session: HlsSessionHandle,
    runtime: Arc<RecordingMemberRuntime>,
}

async fn fixture(with_policy: bool) -> Fixture {
    let ctx = crate::HlsCtx::for_test(Config::default());
    let (session, _) = ctx
        .hls_proxy
        .get_or_create_session_with_outcome(HlsSessionKey::new(1, "member-health"), b"secret", current_time_millis())
        .await;
    let runtime = RecordingMemberRuntime::new(with_policy.then(RecordingMemberRuntime::default_policy));
    session.write().await.origin_account_runtime = Some(runtime.clone());
    Fixture { ctx, session, runtime }
}

/// Runs one refresh against an origin answering with the status in `status` (200 serves a manifest).
async fn refresh_once(fixture: &Fixture, origin: &str) {
    let mut request =
        bind_refresh_request_to_app_state(test_origin_refresh_request(Arc::clone(&fixture.session)), &fixture.ctx);
    request.origin_entry =
        LiveHlsOriginEntry::parse(&format!("{origin}/live/user/pass/12345.m3u8")).expect("origin entry");
    request.now_ms = current_time_millis();
    // The failure backoff is not under test here.
    fixture.session.write().await.origin_refresh.next_fetch_allowed_at_ms = 0;
    trigger_origin_refresh_sync(request).await;
}

async fn status_origin(status: Arc<AtomicU16>) -> super::TestOriginServer {
    spawn_test_origin(Arc::new(move |_path| match status.load(Ordering::Acquire) {
        200 => (200, Vec::new(), manifest_body()),
        other => (other, Vec::new(), String::new()),
    }))
    .await
}

#[tokio::test]
async fn repeated_hard_manifest_failures_notify_the_member_unhealthy() {
    let fixture = fixture(true).await;
    let origin = status_origin(Arc::new(AtomicU16::new(404))).await;

    refresh_once(&fixture, &origin.base_url).await;
    fixture.runtime.assert_quiet("one failure must not switch");
    refresh_once(&fixture, &origin.base_url).await;
    assert_eq!(fixture.runtime.reasons(), vec![HlsMemberUnhealthyReason::ManifestFailure]);
}

#[tokio::test]
async fn one_retryable_manifest_failure_does_not_notify_and_success_restarts_the_count() {
    let fixture = fixture(true).await;
    let status = Arc::new(AtomicU16::new(503));
    let origin = status_origin(Arc::clone(&status)).await;

    refresh_once(&fixture, &origin.base_url).await;
    status.store(200, Ordering::Release);
    refresh_once(&fixture, &origin.base_url).await;
    status.store(404, Ordering::Release);
    refresh_once(&fixture, &origin.base_url).await;
    fixture.runtime.assert_quiet("a success between failures restarts the hard-failure count");
}

#[tokio::test]
async fn sessions_without_a_group_policy_never_notify() {
    let fixture = fixture(false).await;
    let origin = status_origin(Arc::new(AtomicU16::new(404))).await;

    for _ in 0..4 {
        refresh_once(&fixture, &origin.base_url).await;
    }
    fixture.runtime.assert_quiet("");
}

#[tokio::test]
async fn stalled_publication_is_reported_on_a_successful_refresh() {
    let fixture = fixture(true).await;
    let origin = status_origin(Arc::new(AtomicU16::new(200))).await;

    refresh_once(&fixture, &origin.base_url).await;
    fixture.runtime.assert_quiet("");
    {
        // The origin keeps answering with the same manifest: no media progress for a long time.
        let mut session = fixture.session.write().await;
        let stale = current_time_millis().saturating_sub(60_000);
        session.origin_control.last_media_progress_at_ms = Some(stale);
    }
    refresh_once(&fixture, &origin.base_url).await;
    assert_eq!(fixture.runtime.reasons(), vec![HlsMemberUnhealthyReason::Stall]);
}

#[tokio::test]
async fn handoff_resets_the_failure_count_of_the_previous_member() {
    let fixture = fixture(true).await;
    let origin = status_origin(Arc::new(AtomicU16::new(404))).await;

    refresh_once(&fixture, &origin.base_url).await;
    {
        let mut session = fixture.session.write().await;
        let binding = crate::HlsOriginAccountBinding::new(
            Arc::from("input"),
            Arc::from("account"),
            &session.proxy_session_id.clone(),
            0,
        );
        let source = session.origin_source.clone();
        session.handoff_origin_member(source, binding);
    }
    refresh_once(&fixture, &origin.base_url).await;
    fixture.runtime.assert_quiet("the new member starts without the old member's failures");
}
