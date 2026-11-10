use super::{
    super::failover_group::{
        probe_member_payload, wait_for_capacity, wait_for_user_capacity, ProbedPayload, PROBE_MIN_BYTES,
    },
    group_playback_support::ts_fixture,
    support::{create_test_app_config, create_test_app_state_for_config},
};
use crate::{
    api::model::{BoxedProviderStream, StreamError},
    model::ProxyUserCredentials,
};
use bytes::Bytes;
use futures::StreamExt;
use std::{sync::Arc, time::Duration};
use tokio::time::Instant;

fn stream_of(chunks: Vec<Bytes>) -> BoxedProviderStream {
    futures::stream::iter(chunks.into_iter().map(Ok::<Bytes, StreamError>)).boxed()
}

async fn collect(stream: BoxedProviderStream) -> Vec<u8> {
    stream
        .fold(Vec::new(), |mut acc, chunk| async move {
            acc.extend_from_slice(&chunk.expect("chunk"));
            acc
        })
        .await
}

fn deadline_in(secs: u64) -> Instant { Instant::now() + Duration::from_secs(secs) }

#[tokio::test]
async fn probe_accepts_ts_split_over_chunks() {
    let ts = ts_fixture();
    let chunks = ts.chunks(100).map(Bytes::copy_from_slice).collect();
    let (stream, payload) = probe_member_payload(stream_of(chunks), false, deadline_in(3)).await.expect("ts accepted");
    assert!(matches!(payload, ProbedPayload::Ts));
    assert_eq!(collect(stream).await, ts);
}

#[tokio::test]
async fn probe_accepts_extm3u_with_text_plain() {
    let body = Bytes::from_static(b"\xEF\xBB\xBF #EXTM3U\n#EXT-X-VERSION:3\nsegment.ts\n");
    let (stream, payload) =
        probe_member_payload(stream_of(vec![body.clone()]), false, deadline_in(3)).await.expect("playlist accepted");
    assert!(matches!(payload, ProbedPayload::Playlist));
    assert_eq!(collect(stream).await, body.to_vec());
}

#[tokio::test]
async fn probe_trusts_adaptive_hint_without_reading() {
    let pending: BoxedProviderStream = futures::stream::pending().boxed();
    let (_stream, payload) = probe_member_payload(pending, true, deadline_in(3)).await.expect("hint accepted");
    assert!(matches!(payload, ProbedPayload::Playlist));
}

#[tokio::test]
async fn probe_rejects_garbage() {
    let body = Bytes::from(vec![b'x'; PROBE_MIN_BYTES * 2]);
    assert!(probe_member_payload(stream_of(vec![body]), false, deadline_in(3)).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn probe_rejects_endless_empty_chunks_within_deadline() {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(3);
    let empty: BoxedProviderStream = futures::stream::repeat_with(|| Ok::<Bytes, StreamError>(Bytes::new())).boxed();
    assert!(probe_member_payload(empty, false, deadline).await.is_err());
    assert!(Instant::now() <= deadline);
}

#[tokio::test(start_paused = true)]
async fn probe_times_out_on_silent_stream() {
    let deadline = deadline_in(3);
    let silent: BoxedProviderStream = futures::stream::pending().boxed();
    assert!(probe_member_payload(silent, false, deadline).await.is_err());
    assert!(Instant::now() >= deadline);
}

#[tokio::test]
async fn probe_puts_read_chunks_back_in_order() {
    let ts = ts_fixture();
    let mut tail = ts.clone();
    tail.extend_from_slice(&ts);
    let chunks = vec![
        Bytes::copy_from_slice(&ts[..10]),
        Bytes::new(),
        Bytes::copy_from_slice(&ts[10..]),
        Bytes::from(ts.clone()),
    ];
    let (stream, _) = probe_member_payload(stream_of(chunks), false, deadline_in(3)).await.expect("ts accepted");
    assert_eq!(collect(stream).await, tail);
}

fn viewer(max_connections: u32) -> ProxyUserCredentials {
    let mut user = ProxyUserCredentials::default();
    user.username = "viewer".to_string();
    user.max_connections = max_connections;
    user
}

#[tokio::test]
async fn wait_for_user_capacity_returns_immediately_below_limit() {
    let app_state = create_test_app_state_for_config(Arc::new(create_test_app_config()));
    assert!(wait_for_user_capacity(&app_state, &viewer(1), deadline_in(2)).await);
}

#[tokio::test]
async fn wait_for_user_capacity_times_out() {
    let app_state = create_test_app_state_for_config(Arc::new(create_test_app_config()));
    let started = Instant::now();
    assert!(!wait_for_user_capacity(&app_state, &viewer(0), started + Duration::from_millis(300)).await);
    assert!(started.elapsed() >= Duration::from_millis(300));
}

#[tokio::test]
async fn wait_for_capacity_rechecks_on_notification() {
    let notify = Arc::new(tokio::sync::Notify::new());
    let checks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let started = Instant::now();
    let waiter = {
        let notify = Arc::clone(&notify);
        let checks = Arc::clone(&checks);
        tokio::spawn(async move {
            wait_for_capacity(&notify, deadline_in(5), || {
                let checks = Arc::clone(&checks);
                async move { checks.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 1 }
            })
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    notify.notify_waiters();
    assert!(waiter.await.expect("waiter"));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn refresh_member_keys_keeps_only_group_member_probes() {
    let env = super::group_playback_support::GroupPlaybackEnv::builder(&["solis", "korath"]).build().await;
    let member = env.group_record.members[0].clone();
    let mut stranger = member.clone();
    stranger.input_stream_id = "not-a-member".into();
    assert_eq!(crate::api::tasks::refresh_member_keys(&env.app_state).await, env.group_record.members.len());
    let video = Some(r#"{"codec_name":"h264","width":1920,"height":1080}"#);
    let health = &env.app_state.member_health;
    health.patch_probe(&tuliprox_session::failover_group::member_key(&stranger), video, 4_000);
    health.patch_probe(&tuliprox_session::failover_group::member_key(&member), video, 4_000);
    let target: Arc<str> = env.target.name.as_str().into();
    let now = std::time::Instant::now();
    assert!(health.health(&target, &stranger, now).probe.is_none());
    assert!(health.health(&target, &member, now).probe.is_some());
}

#[tokio::test]
async fn probe_rejects_short_text_body_with_sync_byte() {
    let body = Bytes::from_static(b"Gone. Please try again later.");
    assert!(probe_member_payload(stream_of(vec![body]), false, deadline_in(3)).await.is_err());
}

#[tokio::test]
async fn prefixed_stream_passes_inner_after_head() {
    use super::super::failover_group::PrefixedStream;
    let head = vec![Bytes::from_static(b"ab"), Bytes::from_static(b"c")];
    let inner = stream_of(vec![Bytes::from_static(b"de")]);
    let stream = PrefixedStream::new(head, inner).boxed();
    assert_eq!(collect(stream).await, b"abcde");
}

async fn wait_for(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while !condition() {
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

#[tokio::test]
async fn member_key_task_refreshes_on_sources_change_and_stops_on_cancel() {
    let env = super::group_playback_support::GroupPlaybackEnv::builder(&["solis", "korath"]).build().await;
    let member = env.group_record.members[0].clone();
    let key = tuliprox_session::failover_group::member_key(&member);
    let target: Arc<str> = env.target.name.as_str().into();
    let health = Arc::clone(&env.app_state.member_health);
    let video = Some(r#"{"codec_name":"h264","width":1920,"height":1080}"#);
    let cancel = tokio_util::sync::CancellationToken::new();
    let task = crate::api::tasks::exec_failover_group_member_keys(&env.app_state, &cancel);
    // The startup refresh registers the members.
    assert!(
        wait_for(Duration::from_secs(2), || {
            health.patch_probe(&key, video, 1_000);
            health.health(&target, &member, std::time::Instant::now()).probe.is_some()
        })
        .await
    );
    health.set_member_keys(std::collections::HashSet::new());
    env.app_state
        .event_manager
        .send_event(shared::model::EventMessage::ConfigChange(shared::model::ConfigType::Sources));
    assert!(
        wait_for(Duration::from_secs(2), || {
            health.patch_probe(&key, video, 1_000);
            health.health(&target, &member, std::time::Instant::now()).probe.is_some()
        })
        .await,
        "a sources change refreshes the member keys"
    );
    cancel.cancel();
    assert!(tokio::time::timeout(Duration::from_secs(2), task).await.is_ok(), "the task stops on cancel");
}
