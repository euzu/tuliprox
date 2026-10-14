use super::group_playback_support::{ts_fixture, GroupPlaybackEnv};
use shared::model::AdmissionStrategy;
use tuliprox_session::channel_group::CircuitView;

#[tokio::test]
async fn xtream_group_playback_uses_member_input() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .with_qos("korath", 2, 95)
        .with_qos("solis", 1, 20)
        .build()
        .await;
    let response = env.get_xtream_live(env.group_vid).await;
    assert_eq!(response.status(), 200);
    assert_eq!(env.origin_hits("korath"), 1);
    assert_eq!(env.origin_hits("solis"), 0);
}

#[tokio::test]
async fn start_failover_moves_to_next_member_on_open_failure() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).origin_status("solis", 503).build().await;
    let response = env.get_xtream_live(env.group_vid).await;
    assert_eq!(response.status(), 200);
    assert_eq!(env.origin_hits("korath"), 1);
    assert!(matches!(env.circuit("solis"), CircuitView::Counting(1)));
}

#[tokio::test]
async fn capacity_rejection_moves_to_other_input_without_circuit() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).origin_status("solis", 429).build().await;
    let response = env.get_xtream_live(env.group_vid).await;
    assert_eq!(response.status(), 200);
    assert_eq!(env.origin_hits("korath"), 1);
    assert!(matches!(env.circuit("solis"), CircuitView::Closed));
}

#[tokio::test]
async fn non_ts_first_chunk_fails_over() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .origin_body("solis", "video/mp2t", b"Channel offline, please try later".to_vec())
        .build()
        .await;
    assert_eq!(env.get_xtream_live(env.group_vid).await.status(), 200);
    assert_eq!(env.origin_hits("korath"), 1);
    assert!(matches!(env.circuit("solis"), CircuitView::Counting(1)));
}

#[tokio::test]
async fn ts_with_leading_garbage_is_accepted() {
    let mut body = vec![0u8; 17];
    body.extend(ts_fixture());
    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).origin_body("solis", "video/mp2t", body).build().await;
    assert_eq!(env.get_xtream_live(env.group_vid).await.status(), 200);
    assert_eq!(env.origin_hits("korath"), 0);
}

#[tokio::test]
async fn auth_failure_counts_for_member_and_input() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).origin_status("solis", 401).build().await;
    assert_eq!(env.get_xtream_live(env.group_vid).await.status(), 200);
    assert_eq!(env.origin_hits("korath"), 1);
    assert!(matches!(env.circuit("solis"), CircuitView::Counting(1)));
    assert!(matches!(env.input_circuit("solis"), CircuitView::Closed)); // one member only, input stays closed
}

#[tokio::test]
async fn member_vid_request_plays_group_preferring_that_member() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).with_qos("korath", 2, 95).build().await;
    let response = env.get_xtream_live(env.member_vid("solis")).await;
    assert_eq!(response.status(), 200);
    assert_eq!((env.origin_hits("solis"), env.origin_hits("korath")), (1, 0)); // preferred despite lower QoS
}

#[tokio::test]
async fn favourite_copy_plays_as_group() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).with_favourite().with_qos("korath", 2, 95).build().await;
    assert_eq!(env.get_xtream_live(env.favourite_vid()).await.status(), 200);
    assert_eq!(env.origin_hits("korath"), 1);
}

#[tokio::test]
async fn non_group_playback_is_unchanged() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).build().await;
    assert_eq!(env.get_xtream_live(env.plain_vid).await.status(), 200);
    assert_eq!(env.origin_hits("solis"), 1);
}

#[tokio::test]
async fn m3u_group_playback_uses_member_input() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).with_qos("korath", 2, 95).build().await;
    assert_eq!(env.get_m3u_live(env.group_vid).await.status(), 200);
    assert_eq!(env.origin_hits("solis"), 0);
}

#[tokio::test]
async fn stalker_member_url_resolved_after_selection() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).stalker("solis").with_qos("korath", 2, 95).build().await;
    assert_eq!(env.get_xtream_live(env.group_vid).await.status(), 200);
    assert_eq!(env.origin_hits("solis"), 0); // no Stalker create_link against the representative
}

#[tokio::test]
async fn admission_uses_group_vid_for_eviction() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .user_limit(1)
        .admission(vec![AdmissionStrategy::EvictUserSameIpOldest])
        .build()
        .await;
    let first = env.get_xtream_live(env.group_vid).await;
    assert_eq!(first.status(), 200);
    let second = env.get_xtream_live(env.plain_vid).await;
    assert_eq!(second.status(), 200);
    assert_eq!(env.active_session_vids("viewer").await, vec![env.plain_vid]); // group session evicted by its group vid
}

#[tokio::test]
async fn grace_hold_selects_member_only_after_hold_passes() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .user_limit(1)
        .admission(vec![AdmissionStrategy::GraceHoldStream])
        .build()
        .await;
    let _first = env.get_xtream_live(env.plain_vid).await;
    let slots_before = env.slots_in_use();
    let rejected = env.get_xtream_live(env.group_vid).await; // first stream still active → hold fails
    assert_ne!(rejected.status(), 200);
    assert_eq!(env.slots_in_use(), slots_before); // no member slot was taken during the hold
}
