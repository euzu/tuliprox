use crate::{
    api::{
        api_utils::tests::group_playback_support::{first_segment_url, FakeOriginMode, GroupPlaybackEnv},
        model::{ConnectionKind, CreateUserSessionParams},
    },
    auth::Fingerprint,
};
use axum::{
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use shared::{
    model::{StalkerPlaylistItem, StalkerStreamKind, UserConnectionPermission},
    utils::Internable,
};
use std::{net::SocketAddr, sync::Arc};
use tuliprox_repository::{
    stalker_generation_repository::{save_active_manifest, ClusterFiles, StalkerActiveManifest},
    stalker_repository::{ensure_stalker_storage_path, get_stalker_file_path, persist_stalker_items},
};
use tuliprox_session::{failover_group::CircuitView, MemberBinding};

/// Publishes a one-item Stalker live catalog so playback resolution reaches the portal.
async fn publish_stalker_live_item(env: &GroupPlaybackEnv, input: &crate::model::ConfigInput) {
    let storage =
        ensure_stalker_storage_path(&env.app_state.app_config, &input.name).await.expect("stalker storage path");
    let member = env.group_record.members.iter().find(|member| member.input_name == input.name).expect("member");
    let origin = env.origins.get(input.name.as_ref()).expect("member origin").addr;
    let item = StalkerPlaylistItem {
        stream_id: member.provider_id,
        name: "ZDF HD".intern(),
        stream_kind: StalkerStreamKind::Live,
        cmd: format!("ffmpeg http://{origin}/live/1.ts").intern(),
        ..StalkerPlaylistItem::default()
    };
    persist_stalker_items(&env.app_state.app_config, &storage, StalkerStreamKind::Live, &[item])
        .await
        .expect("persist stalker item");
    let fingerprint = input.stalker.as_ref().expect("stalker config").identity_fingerprint(&input.url);
    let mut manifest = StalkerActiveManifest::empty(fingerprint);
    manifest.live =
        Some(ClusterFiles { generation: 1, data: get_stalker_file_path(&storage, StalkerStreamKind::Live) });
    save_active_manifest(&storage, &manifest).await.expect("publish stalker manifest");
}

#[tokio::test]
async fn stalker_group_member_refresh_resolves_its_own_link_instead_of_replaying_the_stored_one() {
    let env = GroupPlaybackEnv::builder(&["solis"]).hls().stalker("solis").build().await;
    let user = env.app_state.app_config.get_user_credentials("viewer").expect("test user");
    let input = env.app_state.app_config.get_input_by_name(&"solis".intern()).expect("member input");
    publish_stalker_live_item(&env, &input).await;
    let origin = env.origins.get("solis").expect("member origin").addr;
    let member_vid = env.member_vid("solis");
    let addr: SocketAddr = "127.0.0.1:12345".parse().expect("addr");
    let fingerprint = Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), addr);
    let provider: Arc<str> = "solis".intern();
    let stored_url = format!("http://{origin}/live/viewer/secret/101");

    let token = "stalker-group-refresh";
    env.app_state
        .active_users
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: token,
            virtual_id: env.group_vid,
            provider: &provider,
            stream_url: &stored_url,
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    env.app_state
        .active_users
        .bind_member(
            &user.username,
            token,
            MemberBinding {
                group_vid: env.group_vid,
                member_vid,
                member_index: 0,
                input_name: Arc::clone(&input.name),
                member_title: "ZDF HD".intern(),
                provider: Arc::clone(&provider),
            },
        )
        .await;
    let session = env.app_state.active_users.get_and_update_user_session(&user.username, token).await.expect("session");
    // The group entry's own stream context carries the representative member's stream ref.
    let source = super::super::resolve_hls_virtual_source_for_target(&env.app_state, &env.target, env.group_vid)
        .await
        .expect("group stream context");

    let response = super::super::handle_hls_stream_request(
        &fingerprint,
        &env.app_state,
        &user,
        &env.target,
        Some(&session),
        None,
        &session.stream_url,
        None,
        source.stream_context,
        &input,
        &HeaderMap::new(),
        UserConnectionPermission::Allowed,
        Some(ConnectionKind::Normal),
        "/live/viewer/secret/1.m3u8",
        super::super::HlsRequestStage::Entry,
    )
    .await
    .into_response();

    // The refresh must ask the portal for a fresh link of the bound member instead of replaying the stored
    // one. The emulated portal cannot complete a handshake, so the request ends unavailable, but it must
    // have been contacted.
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(env.origin_hits("solis") > 0, "the bound member's Stalker link must be resolved on refresh");
}

#[tokio::test]
async fn stalker_group_member_starts_on_hls_entry_resolving_first_link() {
    let env = GroupPlaybackEnv::builder(&["solis"]).hls().stalker("solis").build().await;
    let input = env.app_state.app_config.get_input_by_name(&"solis".intern()).expect("member input");
    publish_stalker_live_item(&env, &input).await;

    // A Stalker member in the group has an empty URL before start.
    let member = env.group_record.members.iter().find(|m| m.input_name == input.name).expect("member");
    assert!(member.url.is_empty(), "stalker group member starts with empty url");

    // Starting on the HLS entry must resolve the first link for the allocated account.
    let response = env.get_hls_manifest(env.group_vid).await;
    // Emulated portal cannot complete handshake, but portal must have been contacted to resolve the first link.
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(env.origin_hits("solis") > 0, "the Stalker portal must be contacted to resolve first link on start");
}

#[tokio::test]
async fn hls_entry_selects_member_with_tiers() {
    let env = GroupPlaybackEnv::new(&["solis", "korath"]).hls().build().await;
    let _blocker = env.hold("solis", 0);
    let manifest = env.get_hls_manifest(env.group_vid).await;
    assert_eq!(manifest.status(), StatusCode::OK);
    assert_eq!(env.origin_hits("solis"), 0);
    assert_eq!(env.origin_hits("korath"), 1);
    assert_eq!(env.bound_member_input("viewer").await, "korath");
}

#[tokio::test]
async fn segment_routing_resolves_member_vid_to_member_input() {
    let env = GroupPlaybackEnv::new(&["solis", "korath"]).hls().with_qos("korath", 2, 95).build().await;
    let manifest = env.get_hls_manifest_text(env.group_vid).await;
    let segment_url = first_segment_url(&manifest);
    assert!(segment_url.contains(&env.member_vid("korath").to_string()));
    let segment = env.get(&segment_url).await;
    assert_eq!(segment.status(), StatusCode::OK);
    assert_eq!(env.origin_hits("solis"), 0);
    assert_eq!(env.origin_hits("korath"), 1);
    assert_eq!(env.bound_member_input("viewer").await, "korath");
}

#[tokio::test]
async fn manifest_refresh_never_reselects_even_with_open_circuit() {
    let env = GroupPlaybackEnv::new(&["solis", "korath"]).hls().build().await;
    let first = env.get_hls_manifest_text(env.group_vid).await;
    let bound = env.bound_member_input("viewer").await;
    assert_ne!(bound, "");
    env.open_circuit(&bound);
    let second = env.refresh_hls_manifest(&first).await;
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(env.bound_member_input("viewer").await, bound);
}

#[tokio::test]
async fn hls_cache_entry_uses_selected_member_input() {
    let env = GroupPlaybackEnv::new(&["solis", "korath"]).hls_cache().with_qos("korath", 2, 95).build().await;
    let manifest = env.get_hls_manifest(env.group_vid).await;
    assert!(manifest.status().is_success() || manifest.status().is_redirection(), "status {}", manifest.status());
    let body = axum::body::to_bytes(manifest.into_body(), 1024 * 1024).await.expect("body");
    // The variant request makes the shared origin fetch its upstream playlist.
    let _variant = env.get(&first_segment_url(&String::from_utf8_lossy(&body))).await;
    assert_eq!(env.origin_hits("solis"), 0);
    assert!(env.origin_hits("korath") > 0);
}

#[tokio::test]
async fn hls_entry_first_member_404_fails_over_to_second_member() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).hls().origin_status("solis", 404).build().await;
    let response = env.get_hls_manifest(env.group_vid).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(env.bound_member_input("viewer").await, "korath");
    assert_eq!(env.origin_hits("solis"), 1);
    assert_eq!(env.origin_hits("korath"), 1);
    assert!(matches!(env.circuit("solis"), CircuitView::Counting(1)));
}

#[tokio::test]
async fn hls_entry_first_member_401_fails_over_to_second_member() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).hls().origin_status("solis", 401).build().await;
    let response = env.get_hls_manifest(env.group_vid).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(env.bound_member_input("viewer").await, "korath");
    assert_eq!(env.origin_hits("solis"), 1);
    assert_eq!(env.origin_hits("korath"), 1);
    // 401 is Auth failure: counts for member circuit; input circuit stays closed with only 1 member on solis
    assert!(matches!(env.circuit("solis"), CircuitView::Counting(1)));
    assert!(matches!(env.input_circuit("solis"), CircuitView::Closed));
}

#[tokio::test]
async fn hls_entry_first_member_429_fails_over_to_second_member() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).hls().origin_status("solis", 429).build().await;
    let response = env.get_hls_manifest(env.group_vid).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(env.bound_member_input("viewer").await, "korath");
    assert!(env.origin_hits("solis") >= 1);
    assert_eq!(env.origin_hits("korath"), 1);
    // 429 is Capacity failure: does not open member circuit
    assert!(matches!(env.circuit("solis"), CircuitView::Closed));
    assert!(matches!(env.input_circuit("solis"), CircuitView::Closed));
}

#[tokio::test]
async fn hls_entry_first_member_500_fails_over_to_second_member() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).hls().origin_status("solis", 500).build().await;
    let response = env.get_hls_manifest(env.group_vid).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(env.bound_member_input("viewer").await, "korath");
    assert!(env.origin_hits("solis") >= 1);
    assert_eq!(env.origin_hits("korath"), 1);
    // 500 is Stream failure: counts for member circuit, not input circuit
    assert!(matches!(env.circuit("solis"), CircuitView::Counting(1)));
    assert!(matches!(env.input_circuit("solis"), CircuitView::Closed));
}

#[tokio::test]
async fn hls_entry_first_member_empty_body_fails_over_to_second_member() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .hls()
        .origin_mode("solis", FakeOriginMode::EmptyBody)
        .build()
        .await;
    let response = env.get_hls_manifest(env.group_vid).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(env.bound_member_input("viewer").await, "korath");
    assert_eq!(env.origin_hits("solis"), 1);
    assert_eq!(env.origin_hits("korath"), 1);
    // Empty body is Stream failure
    assert!(matches!(env.circuit("solis"), CircuitView::Counting(1)));
    assert!(matches!(env.input_circuit("solis"), CircuitView::Closed));
}

#[tokio::test]
async fn hls_entry_first_member_non_hls_fails_over_to_second_member() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .hls()
        .origin_body("solis", "text/plain", b"invalid playlist body".to_vec())
        .build()
        .await;
    let response = env.get_hls_manifest(env.group_vid).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(env.bound_member_input("viewer").await, "korath");
    assert_eq!(env.origin_hits("solis"), 1);
    assert_eq!(env.origin_hits("korath"), 1);
    // Non-HLS body is Stream failure
    assert!(matches!(env.circuit("solis"), CircuitView::Counting(1)));
    assert!(matches!(env.input_circuit("solis"), CircuitView::Closed));
}

#[tokio::test]
async fn hls_entry_first_member_timeout_fails_over_to_second_member() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .hls()
        .origin_mode("solis", FakeOriginMode::Silent)
        .build()
        .await;
    // Set short origin manifest timeout (50ms) to keep test fast
    env.app_state.hls.proxy.update_origin_preferences(
        shared::model::Secs::new(2),
        shared::model::HlsSegmentTimeout::default(),
        50,
    );
    let response = env.get_hls_manifest(env.group_vid).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(env.bound_member_input("viewer").await, "korath");
    assert_eq!(env.origin_hits("solis"), 1);
    assert_eq!(env.origin_hits("korath"), 1);
    // Timeout is Transport failure: counts for member circuit; input circuit stays closed with only 1 member on solis
    assert!(matches!(env.circuit("solis"), CircuitView::Counting(1)));
    assert!(matches!(env.input_circuit("solis"), CircuitView::Closed));
}

#[tokio::test]
async fn hls_entry_all_members_fail_returns_service_unavailable() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .hls()
        .origin_status("solis", 404)
        .origin_status("korath", 500)
        .build()
        .await;
    let response = env.get_hls_manifest(env.group_vid).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(env.origin_hits("solis") >= 1);
    assert!(env.origin_hits("korath") >= 1);
}

#[tokio::test]
async fn hls_entry_healthy_first_member_makes_exactly_one_manifest_request() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).hls().build().await;
    let response = env.get_hls_manifest(env.group_vid).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(env.bound_member_input("viewer").await, "solis");
    // The probe download is the delivered manifest; no second download of the same URL.
    assert_eq!(env.origin_hits("solis"), 1);
    assert_eq!(env.origin_hits("korath"), 0);
}

async fn spawn_mock_master_origin(child_status: StatusCode) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("binds");
    let addr = listener.local_addr().expect("address");
    let task = tokio::spawn(async move {
        for _ in 0..10 {
            let Ok((mut socket, _)) = listener.accept().await else { break };
            let mut request = Vec::new();
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let mut buf = [0u8; 1024];
                let Ok(n) = socket.read(&mut buf).await else { break };
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
            }
            let req_str = String::from_utf8_lossy(&request);
            if req_str.contains("child.m3u8") {
                if child_status == StatusCode::OK {
                    let body = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\nseg1.ts\n";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                } else {
                    let resp = format!("HTTP/1.1 {child_status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    let _ = socket.write_all(resp.as_bytes()).await;
                }
                let _ = socket.shutdown().await;
            } else {
                let body = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1280000\nchild.m3u8\n";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        }
    });
    (addr, task)
}

#[tokio::test]
async fn hls_entry_master_with_child_followed_and_delivered() {
    let (addr, _task) = spawn_mock_master_origin(StatusCode::OK).await;
    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).hls().m3u("solis").build().await;

    let target_path =
        crate::repository::get_target_storage_path(&env.app_state.app_config.config.load(), &env.target.name)
            .expect("target storage path");
    let mut group = (*env.group_record).clone();
    group.members[0].url = Arc::from(format!("http://{addr}/master.m3u8"));
    tuliprox_repository::FailoverGroupStore::write(&target_path, &[group], &[], None)
        .expect("write group with master url");
    env.app_state.playlists.bump_failover_group_generation(&env.target.name);

    let response = env.get_hls_manifest(env.group_vid).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(env.bound_member_input("viewer").await, "solis");
    assert_eq!(env.origin_hits("korath"), 0);

    let body = GroupPlaybackEnv::response_text(response).await;
    assert!(body.contains("#EXT-X-STREAM-INF"), "master playlist delivered to client: {body}");
}

#[tokio::test]
async fn hls_entry_master_with_failing_child_fails_over() {
    let (addr, _task) = spawn_mock_master_origin(StatusCode::NOT_FOUND).await;
    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).hls().m3u("solis").build().await;

    let target_path =
        crate::repository::get_target_storage_path(&env.app_state.app_config.config.load(), &env.target.name)
            .expect("target storage path");
    let mut group = (*env.group_record).clone();
    group.members[0].url = Arc::from(format!("http://{addr}/master.m3u8"));
    tuliprox_repository::FailoverGroupStore::write(&target_path, &[group], &[], None)
        .expect("write group with master url");
    env.app_state.playlists.bump_failover_group_generation(&env.target.name);

    let response = env.get_hls_manifest(env.group_vid).await;
    assert_eq!(response.status(), StatusCode::OK);
    // Solis master had failing child, so it failed over to korath!
    assert_eq!(env.bound_member_input("viewer").await, "korath");
    assert_eq!(env.origin_hits("korath"), 1);
    assert!(matches!(env.circuit("solis"), CircuitView::Counting(1)));
}

#[tokio::test]
async fn hls_entry_max_attempts_respected() {
    use shared::model::{FailoverGroupStartConfig, FailoverGroupsConfig};
    let failover_groups = FailoverGroupsConfig {
        start: FailoverGroupStartConfig { max_attempts: 1, timeout_secs: 5 },
        ..Default::default()
    };
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .hls()
        .with_failover_groups(failover_groups)
        .origin_status("solis", 404)
        .build()
        .await;
    let response = env.get_hls_manifest(env.group_vid).await;
    // max_attempts was 1: solis failed, stopped without attempting korath
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(env.origin_hits("solis"), 1);
    assert_eq!(env.origin_hits("korath"), 0);
}

#[tokio::test]
async fn hls_cache_entry_first_member_404_fails_over_to_second_member() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).hls_cache().origin_status("solis", 404).build().await;
    let response = env.get_hls_manifest(env.group_vid).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(env.origin_hits("solis"), 1);
    assert_eq!(env.origin_hits("korath"), 1);
    assert!(matches!(env.circuit("solis"), CircuitView::Counting(1)));
}

#[tokio::test]
async fn hls_entry_handle_released_after_url_build_failure() {
    let env = GroupPlaybackEnv::builder(&["solis"]).hls().stalker("solis").build().await;
    let input = env.app_state.app_config.get_input_by_name(&"solis".intern()).expect("member input");
    publish_stalker_live_item(&env, &input).await;

    assert_eq!(env.slots_in_use(), 0, "initial slots in use must be 0");

    let response = env.get(&format!("/live/viewer/secret/{}.m3u8?utc=1700000000&lutc=1700003600", env.group_vid)).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    assert_eq!(env.slots_in_use(), 0, "provider handle must be released on URL resolution failure");
}

#[tokio::test]
async fn hls_entry_stalker_url_resolution_hang_bounded_by_deadline() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .hls()
        .stalker("solis")
        .origin_mode("solis", FakeOriginMode::Silent)
        .build()
        .await;
    let input = env.app_state.app_config.get_input_by_name(&"solis".intern()).expect("member input");
    publish_stalker_live_item(&env, &input).await;

    // Set short origin manifest timeout (50ms) to bound the attempt deadline
    env.app_state.hls.proxy.update_origin_preferences(
        shared::model::Secs::new(2),
        shared::model::HlsSegmentTimeout::default(),
        50,
    );

    let start = tokio::time::Instant::now();
    let response = env.get_hls_manifest(env.group_vid).await;
    let elapsed = start.elapsed();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(env.bound_member_input("viewer").await, "korath");
    assert_eq!(env.origin_hits("korath"), 1);
    assert!(elapsed < std::time::Duration::from_secs(2), "probe URL resolution must be bounded by attempt deadline");
}

#[tokio::test]
async fn hls_entry_group_probe_request_carries_user_agent_stream_index() {
    let env = GroupPlaybackEnv::builder(&["solis"]).hls().user_agent_stream_index("solis").build().await;
    let response = env.get_hls_manifest(env.group_vid).await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = env.last_request_headers("solis");
    let ua = headers.get(axum::http::header::USER_AGENT).expect("user-agent header").to_str().expect("valid utf-8");
    assert!(ua.ends_with(" 1"), "probe user agent should carry stream index suffix: {ua}");
}

#[tokio::test]
async fn hls_entry_group_probe_request_user_agent_stream_index_follows_member_input_not_target_input() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .hls()
        .origin_status("solis", 404)
        .user_agent_stream_index("korath")
        .build()
        .await;
    let response = env.get_hls_manifest(env.group_vid).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(env.bound_member_input("viewer").await, "korath");

    let solis_headers = env.last_request_headers("solis");
    let solis_ua =
        solis_headers.get(axum::http::header::USER_AGENT).expect("solis user-agent").to_str().expect("valid utf-8");
    assert!(!solis_ua.ends_with(" 1"), "solis probe must not have stream index suffix: {solis_ua}");

    let korath_headers = env.last_request_headers("korath");
    let korath_ua =
        korath_headers.get(axum::http::header::USER_AGENT).expect("korath user-agent").to_str().expect("valid utf-8");
    assert!(korath_ua.ends_with(" 1"), "korath probe must carry stream index suffix: {korath_ua}");
}

#[tokio::test]
async fn hls_entry_group_probe_request_skips_stream_index_when_selected_member_input_has_no_flag() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .hls()
        .user_agent_stream_index("solis")
        .origin_status("solis", 404)
        .build()
        .await;
    let response = env.get_hls_manifest(env.group_vid).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(env.bound_member_input("viewer").await, "korath");

    let solis_headers = env.last_request_headers("solis");
    let solis_ua =
        solis_headers.get(axum::http::header::USER_AGENT).expect("solis user-agent").to_str().expect("valid utf-8");
    assert!(solis_ua.ends_with(" 1"), "solis probe carries stream index suffix: {solis_ua}");

    let korath_headers = env.last_request_headers("korath");
    let korath_ua =
        korath_headers.get(axum::http::header::USER_AGENT).expect("korath user-agent").to_str().expect("valid utf-8");
    assert!(!korath_ua.ends_with(" 1"), "korath probe must not have stream index suffix: {korath_ua}");
}

#[tokio::test]
async fn stalker_group_member_first_link_rejected_fresh_link_retried_and_served() {
    let env = GroupPlaybackEnv::builder(&["solis"])
        .hls()
        .stalker("solis")
        .stalker_retry_origin("solis", "/fresh/live.m3u8")
        .build()
        .await;
    let input = env.app_state.app_config.get_input_by_name(&"solis".intern()).expect("member input");
    publish_stalker_live_item(&env, &input).await;

    let response = env.get_hls_manifest(env.group_vid).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(env.bound_member_input("viewer").await, "solis");
    assert!(matches!(env.circuit("solis"), CircuitView::Closed));
    let body = GroupPlaybackEnv::response_text(response).await;
    assert!(body.contains("#EXT-X-STREAM-INF"), "fresh manifest delivered: {body}");
}

#[tokio::test]
async fn second_viewer_on_same_member_makes_no_extra_origin_manifest_request() {
    let env = GroupPlaybackEnv::new(&["solis"]).hls_cache().build().await;
    let manifest = env.get_hls_manifest(env.group_vid).await;
    assert!(manifest.status().is_success() || manifest.status().is_redirection());
    let body = axum::body::to_bytes(manifest.into_body(), 1024 * 1024).await.expect("body");
    let _variant = env.get(&first_segment_url(&String::from_utf8_lossy(&body))).await;
    let origin_hits_after_first = env.origin_hits("solis");
    assert!(origin_hits_after_first > 0);

    let manifest2 = env.get_hls_manifest_from(env.group_vid, 12346).await;
    assert!(manifest2.status().is_success() || manifest2.status().is_redirection());
    assert_eq!(env.origin_hits("solis"), origin_hits_after_first);
}
