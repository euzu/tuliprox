use super::{
    group_playback_support::{ts_fixture, GroupPlaybackEnv},
    support::FakeOriginMode,
};
use shared::{model::AdmissionStrategy, utils::Internable};
use std::{net::SocketAddr, sync::Arc};
use tuliprox_session::failover_group::CircuitView;

/// Occupies `input`'s own account so its next allocation must use an alias.
fn hold_input_account(env: &GroupPlaybackEnv, input: &str) {
    let held = env.app_state.active_provider.acquire_exact_connection_with_grace(
        &input.intern(),
        &SocketAddr::from(([127, 0, 0, 1], 201)),
        false,
        0,
        crate::api::model::ConnectionKind::Normal,
    );
    assert!(held.is_some(), "setup should occupy {input}'s own account");
}

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
async fn group_member_alias_lease_plays_the_alias_url() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .max_connections("solis", 1)
        .alias("solis", "solis_alias")
        .with_qos("solis", 1, 95)
        .build()
        .await;
    hold_input_account(&env, "solis");

    assert_eq!(env.get_xtream_live(env.group_vid).await.status(), 200);
    assert_eq!(env.origin_hits("solis"), 0, "the input account URL must not be used while the alias holds the lease");
    assert_eq!(env.origin_hits("solis_alias"), 1, "the member URL must be mapped to the allocated alias account");
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

#[tokio::test]
async fn group_skips_member_in_maintenance() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).with_qos("solis", 1, 95).build().await;
    env.app_state.maintenance.add("solis", None, 1).await.expect("maintenance");
    assert_eq!(env.get_xtream_live(env.group_vid).await.status(), 200);
    assert_eq!((env.origin_hits("solis"), env.origin_hits("korath")), (0, 1));
}

#[tokio::test]
async fn group_with_all_members_in_maintenance_returns_maintenance() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).build().await;
    env.app_state.maintenance.add("solis", None, 1).await.expect("maintenance");
    env.app_state.maintenance.add("korath", None, 1).await.expect("maintenance");
    let response = env.get_xtream_live(env.group_vid).await;
    assert_ne!(response.status(), 200);
    assert_eq!((env.origin_hits("solis"), env.origin_hits("korath")), (0, 0));
    assert_eq!(env.slots_in_use(), 0);
}

#[tokio::test]
async fn start_budget_bounds_all_attempts() {
    let blocked = Arc::new(tokio::sync::Notify::new());
    let failover_groups = shared::model::FailoverGroupsConfig {
        start: shared::model::FailoverGroupStartConfig { timeout_secs: 2, ..Default::default() },
        ..Default::default()
    };
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .origin_mode("solis", FakeOriginMode::BlockFirstChunk(Arc::clone(&blocked)))
        .origin_mode("korath", FakeOriginMode::BlockFirstChunk(Arc::clone(&blocked)))
        .with_failover_groups(failover_groups)
        .build()
        .await;
    let started = std::time::Instant::now();
    let response = env.get_xtream_live(env.group_vid).await;
    let elapsed = started.elapsed();
    blocked.notify_waiters();
    assert!(elapsed < std::time::Duration::from_millis(2_500), "start took {elapsed:?}");
    drop(response);
    // A failed open releases its slot once the dropped provider body completes.
    let released = async {
        while env.slots_in_use() != 0 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };
    assert!(tokio::time::timeout(std::time::Duration::from_secs(2), released).await.is_ok());
}

#[tokio::test]
async fn xtream_timeshift_routes_to_archive_member_and_builds_timeshift_url() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .without_plain_channel()
        // solis has no archive; korath has 7 days archive
        .with_member_archives(&[None, Some(7)])
        .build()
        .await;

    let now_minus_1_day = (chrono::Utc::now() - chrono::Duration::days(1)).format("%Y-%m-%d:%H-%M").to_string();
    let res = env.get_xtream_timeshift(env.group_vid, "60", &now_minus_1_day).await;
    assert_eq!(res.status(), 200);

    // Must route to korath (the only member covering the requested archive window)
    assert_eq!(env.origin_hits("solis"), 0);
    assert_eq!(env.origin_hits("korath"), 1);

    // Upstream request must hit the timeshift path with korath's stream id, NOT live/101
    let last_request = env.last_request_path("korath");
    assert!(last_request.contains("/timeshift/"), "Upstream must receive timeshift request, got: {last_request}");
    assert!(last_request.contains(&env.member_stream_id("korath")), "Upstream must receive member stream id");
}

#[tokio::test]
async fn start_failover_to_second_archive_member_when_first_fails() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .without_plain_channel()
        // solis has archive but returns 503; korath has archive and succeeds
        .with_member_archives(&[Some(7), Some(7)])
        .origin_status("solis", 503)
        .build()
        .await;

    let now_minus_1_day = (chrono::Utc::now() - chrono::Duration::days(1)).format("%Y-%m-%d:%H-%M").to_string();
    let res = env.get_xtream_timeshift(env.group_vid, "60", &now_minus_1_day).await;
    assert_eq!(res.status(), 200);

    // Must have tried solis first, failed, and then opened korath
    assert_eq!(env.origin_hits("solis"), 1);
    assert_eq!(env.origin_hits("korath"), 1);
}

#[tokio::test]
async fn m3u_catchup_resolves_group_vid_and_streams_archive() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .without_plain_channel()
        .m3u("korath")
        .with_member_archives(&[None, Some(7)])
        .build()
        .await;

    let utc = (chrono::Utc::now() - chrono::Duration::hours(2)).timestamp();
    let res = env.get_m3u_catchup(env.group_vid, utc, 3600).await;
    assert_eq!(res.status(), 200);

    assert_eq!(env.origin_hits("solis"), 0);
    assert_eq!(env.origin_hits("korath"), 1);
    let last_request = env.last_request_path("korath");
    assert!(last_request.contains("/catchup"), "Upstream must receive catchup request, got: {last_request}");
}

#[tokio::test]
async fn failover_group_short_epg_emits_has_archive_when_second_member_has_archive() {
    let now = chrono::Utc::now().timestamp();
    let mut programme =
        shared::model::EpgProgramme::new(now - 3600, now + 3600, shared::utils::Internable::intern("zdf.de"));
    programme.title = Some(shared::utils::Internable::intern("Heute"));
    let epg = crate::model::Epg {
        priority: 0,
        logo_override: false,
        attributes: None,
        children: vec![Arc::new(shared::model::EpgChannel {
            id: shared::utils::Internable::intern("zdf.de"),
            title: Some(shared::utils::Internable::intern("ZDF")),
            icon: None,
            programmes: vec![programme],
        })],
        channel_sources: None,
        input_name: None,
    };

    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .without_plain_channel()
        .m3u("solis")
        // solis has no archive; korath has 7 days archive
        .with_member_archives(&[None, Some(7)])
        .with_epg(epg, "zdf.de")
        .build()
        .await;

    let res = env.get_xtream_short_epg(env.group_vid).await;
    assert_eq!(res.status(), 200);
    let body = GroupPlaybackEnv::response_text(res).await;
    let json: serde_json::Value = serde_json::from_str(&body).expect("valid json");

    let listings = json["epg_listings"].as_array().expect("epg listings");
    assert!(!listings.is_empty(), "must have epg listings");
    assert_eq!(listings[0]["has_archive"], 1, "Short EPG must advertise archive for group");
}

#[tokio::test]
async fn archive_member_order_ranks_capable_members_and_caps_by_max_attempts() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .without_plain_channel()
        .with_member_archives(&[None, Some(7)])
        .build()
        .await;

    let now_ts = chrono::Utc::now().timestamp();
    let start_ts = now_ts - 3600;
    let order = crate::api::api_utils::failover_group::archive_member_order(
        &env.app_state,
        &env.target,
        &env.group_record,
        start_ts,
    );
    // korath (index 1) has 7 days archive covering start_ts; solis (index 0) has none
    assert_eq!(order, vec![1]);
}

#[tokio::test]
async fn timeshift_failover_skips_candidate_returning_custom_video_and_cleans_up() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .without_plain_channel()
        .channel_unavailable_video()
        .origin_status("solis", 503)
        .with_member_archives(&[Some(7), Some(7)])
        .build()
        .await;

    let now_minus_1_day = (chrono::Utc::now() - chrono::Duration::days(1)).format("%Y-%m-%d:%H-%M").to_string();
    let res = env.get_xtream_timeshift(env.group_vid, "60", &now_minus_1_day).await;
    assert_eq!(res.status(), 200);

    // solis returned 503, which channel_unavailable_video wrapped into a 200 custom video.
    // Finding 3: Outcome decides failure from stream details (not HTTP 200), so failover continues.
    assert_eq!(env.origin_hits("solis"), 1);
    assert_eq!(env.origin_hits("korath"), 1);

    // Finding 2: solis's session/lease was terminated before korath was opened; only korath's session remains.
    assert_eq!(env.active_session_vids("viewer").await, vec![env.group_vid]);
}

#[tokio::test]
async fn xtream_catchup_table_virtual_id_resolves_group_and_plays() {
    use axum::http::Request;
    use tower::ServiceExt;

    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .without_plain_channel()
        .with_member_archives(&[Some(7), Some(7)])
        .origin_body(
            "solis",
            "application/json",
            br#"{"epg_listings":[{"id":"999","title":"Program 1","start":"2026-01-01 12:00:00","end":"2026-01-01 13:00:00"}]}"#.to_vec(),
        )
        .build()
        .await;

    let uri = format!(
        "/player_api.php?username=viewer&password=secret&action=get_simple_data_table&stream_id={}",
        env.group_vid
    );
    let mut req = Request::builder().uri(uri).body(axum::body::Body::empty()).expect("request builder");
    req.extensions_mut().insert(axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))));
    let res = env.router.clone().oneshot(req).await.expect("endpoint response");
    assert_eq!(res.status(), 200);

    let body = GroupPlaybackEnv::response_text(res).await;
    let json: serde_json::Value = serde_json::from_str(&body).expect("valid json");
    let listings = json["epg_listings"].as_array().expect("epg listings array");
    assert_eq!(listings.len(), 1);
    let mapped_id_str = listings[0]["id"].as_str().expect("mapped program id string");
    let mapped_id: u32 = mapped_id_str.parse().expect("parsed program virtual id");

    // Play the timeshift stream using the program's mapped virtual id.
    // solis returns application/json (fails TS chunk check), so failover proceeds to korath.
    let now_minus_1_day = (chrono::Utc::now() - chrono::Duration::days(1)).format("%Y-%m-%d:%H-%M").to_string();
    let stream_res = env.get_xtream_timeshift(mapped_id, "60", &now_minus_1_day).await;
    assert_eq!(stream_res.status(), 200);

    // Group was resolved from program virtual id; member solis was routed for timeshift
    assert_eq!(env.origin_hits("solis"), 2);
    let last_request = env.last_request_path("solis");
    assert!(last_request.contains("/timeshift/"), "Upstream must receive timeshift request, got: {last_request}");
}
