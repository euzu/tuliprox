use super::group_playback_support::GroupPlaybackEnv;
use axum::http::StatusCode;
use futures::StreamExt;

#[tokio::test]
async fn range_request_uses_bound_member_input_not_representative() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).with_qos("korath", 2, 95).build().await;
    let initial = env.get_xtream_live(env.group_vid).await;
    assert_eq!(initial.status(), StatusCode::OK);
    assert_eq!(env.bound_member_input("viewer").await, "korath");
    assert_eq!(env.origin_hits("korath"), 1);
    assert_eq!(env.origin_hits("solis"), 0);

    let seek = env.get_xtream_live_with_range(env.group_vid, "bytes=1000-").await;
    assert!(seek.status().is_success());
    assert_eq!(env.bound_member_input("viewer").await, "korath");
    assert_eq!(env.origin_hits("korath"), 2);
    assert_eq!(env.origin_hits("solis"), 0);
}

#[tokio::test]
async fn ts_reconnect_stays_on_member_unless_circuit_open() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).with_qos("korath", 2, 95).build().await;
    let first = env.get_xtream_live(env.group_vid).await;
    assert_eq!(first.status(), StatusCode::OK);
    drop(first);

    // reconnect within session TTL stays on korath
    let second = env.get_xtream_live(env.group_vid).await;
    assert_eq!(second.status(), StatusCode::OK);
    drop(second);
    assert_eq!((env.origin_hits("korath"), env.origin_hits("solis")), (2, 0));
    assert_eq!(env.bound_member_input("viewer").await, "korath");

    // Open circuit on korath -> sticky binding is broken by circuit breaker, fails over to solis
    env.open_circuit("korath");
    let third = env.get_xtream_live(env.group_vid).await;
    assert_eq!(third.status(), StatusCode::OK);
    drop(third);
    assert_eq!(env.origin_hits("korath"), 2);
    assert_eq!(env.origin_hits("solis"), 1);
    assert_eq!(env.bound_member_input("viewer").await, "solis");
}

#[tokio::test]
async fn binding_survives_group_removal_until_stream_end() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).build().await;
    let response = env.get_xtream_live(env.group_vid).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(env.bound_member_input("viewer").await, "solis");
    assert!(env.active_session_vids("viewer").await.contains(&env.group_vid));

    let mut body = response.into_body().into_data_stream();

    // Republish target storage with groups removed while stream is actively running
    env.republish_without_groups().await;

    // Running stream body continues delivering bytes despite group removal from storage
    let chunk = body.next().await.expect("stream continues").expect("bytes");
    assert!(!chunk.is_empty());
    assert_eq!(env.bound_member_input("viewer").await, "solis");

    // New request for the removed group VID sees new state and does not panic (returns 502 Bad Gateway / channel unavailable)
    let after = env.get_xtream_live(env.group_vid).await;
    assert_ne!(after.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(after.status(), StatusCode::BAD_GATEWAY);

    // After body finishes/is dropped, active session lease cleans up
    drop(body);
}
