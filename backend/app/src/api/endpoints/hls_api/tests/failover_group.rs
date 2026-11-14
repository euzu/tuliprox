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
    model::{
        ConnectFailureReason, FailoverGroupRuntimeConfig, FailureStage, StalkerPlaylistItem, StalkerStreamKind,
        StreamHistoryEventType, UserConnectionPermission,
    },
    utils::Internable,
};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tuliprox_hls::api::{HlsMemberUnhealthyReason, HlsSessionHandle, HlsSessionKey};
use tuliprox_repository::{
    stalker_generation_repository::{save_active_manifest, ClusterFiles, StalkerActiveManifest},
    stalker_repository::{ensure_stalker_storage_path, get_stalker_file_path, persist_stalker_items},
};
use tuliprox_session::{failover_group::CircuitView, MemberBinding};

/// Extracts the proxy session id from the canonical media playlist URI of a single-variant master.
fn proxy_session_id_from_master(master: &str) -> String {
    const MARKER: &str = "/hls/shared/live/";
    let rest = master.split(MARKER).nth(1).expect("canonical manifest path in master");
    rest.split('/').next().expect("proxy session id").to_string()
}

/// Publishes a one-item Stalker live catalog so playback resolution reaches the portal.
pub(in crate::api::endpoints::hls_api) async fn publish_stalker_live_item(
    env: &GroupPlaybackEnv,
    input: &crate::model::ConfigInput,
) {
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
    spawn_mock_master_origin_with_cookie(child_status, None).await
}

/// Master origin; with `cookie`, the master sets it and the child answers 403 without it.
async fn spawn_mock_master_origin_with_cookie(
    child_status: StatusCode,
    cookie: Option<&'static str>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
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
            let has_cookie = cookie.is_none_or(|c| req_str.to_ascii_lowercase().contains(&format!("cookie: {c}")));
            if req_str.contains("child.m3u8") {
                if child_status == StatusCode::OK && has_cookie {
                    let body = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\nseg1.ts\n";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                } else {
                    let status = if has_cookie { child_status } else { StatusCode::FORBIDDEN };
                    let resp = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    let _ = socket.write_all(resp.as_bytes()).await;
                }
                let _ = socket.shutdown().await;
            } else {
                let body = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1280000\nchild.m3u8\n";
                let set_cookie = cookie.map(|c| format!("Set-Cookie: {c}; Path=/\r\n")).unwrap_or_default();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\n{set_cookie}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
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
async fn hls_entry_master_cookie_is_forwarded_to_same_origin_variant_probe() {
    let (addr, _task) = spawn_mock_master_origin_with_cookie(StatusCode::OK, Some("tok=abc")).await;
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

#[tokio::test]
async fn two_group_viewers_share_one_live_hls_session() {
    let env = GroupPlaybackEnv::new(&["solis"]).hls_cache().build().await;
    let first_master = env.get_hls_manifest_text(env.group_vid).await;
    let first_proxy = proxy_session_id_from_master(&first_master);
    let _variant = env.get(&first_segment_url(&first_master)).await;
    let origin_hits_after_first = env.origin_hits("solis");
    assert!(origin_hits_after_first > 0);

    let second_response = env.get_hls_manifest_from(env.group_vid, 12346).await;
    assert!(second_response.status().is_success() || second_response.status().is_redirection());
    let second_master = GroupPlaybackEnv::response_text(second_response).await;
    assert_eq!(proxy_session_id_from_master(&second_master), first_proxy);
    assert_eq!(env.origin_hits("solis"), origin_hits_after_first);
}

#[tokio::test]
async fn viewer_via_member_vid_joins_existing_group_session() {
    let env = GroupPlaybackEnv::new(&["solis", "korath"]).hls_cache().with_qos("korath", 2, 95).build().await;
    let first_master = env.get_hls_manifest_text(env.group_vid).await;
    let first_proxy = proxy_session_id_from_master(&first_master);
    let _variant = env.get(&first_segment_url(&first_master)).await;
    let origin_hits_after_first = env.origin_hits("korath");
    assert!(origin_hits_after_first > 0);

    let member_vid = env.member_vid("korath");
    let second_response = env.get_hls_manifest_from(member_vid, 12346).await;
    assert!(second_response.status().is_success() || second_response.status().is_redirection());
    let second_master = GroupPlaybackEnv::response_text(second_response).await;
    assert_eq!(proxy_session_id_from_master(&second_master), first_proxy);
    assert_eq!(env.origin_hits("korath"), origin_hits_after_first);
}

#[tokio::test]
async fn reusable_group_session_skips_probe_and_non_reusable_probe_runs() {
    let env = GroupPlaybackEnv::new(&["solis"]).hls_cache().build().await;
    let first_master = env.get_hls_manifest_text(env.group_vid).await;
    let _variant = env.get(&first_segment_url(&first_master)).await;
    let origin_hits_after_first = env.origin_hits("solis");
    assert!(origin_hits_after_first > 0);

    let reusable_response = env.get_hls_manifest_from(env.group_vid, 12346).await;
    assert!(reusable_response.status().is_success() || reusable_response.status().is_redirection());
    assert_eq!(env.origin_hits("solis"), origin_hits_after_first);

    let group_key = tuliprox_hls::api::HlsSessionKey::for_group(env.target.id, env.group_vid);
    let session = env.app_state.hls.proxy.sessions().get_by_key(&group_key).await.expect("group session");
    session.write().await.origin_refresh.consecutive_failures = 1;

    let non_reusable_response = env.get_hls_manifest_from(env.group_vid, 12347).await;
    assert!(non_reusable_response.status().is_success() || non_reusable_response.status().is_redirection());
    assert!(env.origin_hits("solis") > origin_hits_after_first, "non-reusable group session must probe origin");
}

#[tokio::test]
async fn hls_cache_group_session_registers_selected_member_identity() {
    let env = GroupPlaybackEnv::new(&["solis", "korath"]).hls_cache().with_qos("korath", 2, 95).build().await;
    let manifest = env.get_hls_manifest(env.group_vid).await;
    assert!(manifest.status().is_success() || manifest.status().is_redirection(), "status {}", manifest.status());
    let body = axum::body::to_bytes(manifest.into_body(), 1024 * 1024).await.expect("body");
    let variant = env.get(&first_segment_url(&String::from_utf8_lossy(&body))).await;
    assert_eq!(variant.status(), StatusCode::OK);
    let variant_body = axum::body::to_bytes(variant.into_body(), 1024 * 1024).await.expect("variant body");
    let segment = env.get(&first_segment_url(&String::from_utf8_lossy(&variant_body))).await;
    assert_eq!(segment.status(), StatusCode::OK);

    let streams = env.app_state.active_users.active_streams().await;
    let stream = streams.iter().find(|stream| stream.channel.shared).expect("shared HLS stream");
    let member = env.group_record.members.iter().find(|m| m.input_name.as_ref() == "korath").expect("korath member");
    assert_eq!(stream.channel.input_name.as_ref(), "korath");
    assert_eq!(stream.channel.provider_id, member.provider_id);
    assert_eq!(stream.channel.virtual_id, member.member_vid.get());
    assert_eq!(stream.channel.group_virtual_id, Some(env.group_vid));
}

#[tokio::test]
async fn hls_entry_probe_failure_writes_connect_failed_for_failed_member() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .hls()
        .with_stream_history()
        .origin_status("solis", 404)
        .build()
        .await;
    let response = env.get_hls_manifest(env.group_vid).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(env.bound_member_input("viewer").await, "korath");

    let writer = env.app_state.connection_manager.history_writer().load_full().expect("history writer enabled");
    let records = writer.get_current_batch().await.expect("history batch");
    let member = env.group_record.members.iter().find(|m| m.input_name.as_ref() == "solis").expect("solis member");
    let connect_failed = records
        .iter()
        .find(|record| {
            record.event_type == StreamHistoryEventType::ConnectFailed && record.input_name.as_deref() == Some("solis")
        })
        .expect("ConnectFailed record for solis");
    assert_eq!(connect_failed.provider_id, Some(member.provider_id));
    assert_eq!(connect_failed.virtual_id, Some(member.member_vid.get()));
    assert_eq!(connect_failed.connect_failure_reason, Some(ConnectFailureReason::ChannelUnavailable));
    assert_eq!(connect_failed.failure_stage, Some(FailureStage::ProviderOpen));
}

#[tokio::test]
async fn legacy_hls_group_session_registers_selected_member_identity() {
    let env =
        GroupPlaybackEnv::new(&["solis", "korath"]).hls().with_qos("korath", 2, 95).with_stream_history().build().await;
    let manifest = env.get_hls_manifest_text(env.group_vid).await;
    let variant = env.get(&first_segment_url(&manifest)).await;
    assert_eq!(variant.status(), StatusCode::OK);
    let variant_body = axum::body::to_bytes(variant.into_body(), 1024 * 1024).await.expect("variant body");
    let segment_uri = first_segment_url(&String::from_utf8_lossy(&variant_body));
    let segment = env.get(&segment_uri).await;
    assert_eq!(segment.status(), StatusCode::OK, "segment uri: {segment_uri}");

    let writer = env.app_state.connection_manager.history_writer().load_full().expect("history writer enabled");
    let records = writer.get_current_batch().await.expect("history batch");
    let member = env.group_record.members.iter().find(|m| m.input_name.as_ref() == "korath").expect("korath member");
    let connect = records
        .iter()
        .find(|record| {
            record.event_type == StreamHistoryEventType::Connect && record.input_name.as_deref() == Some("korath")
        })
        .unwrap_or_else(|| {
            panic!(
                "Connect record for korath; records={:?}",
                records
                    .iter()
                    .map(|r| (r.event_type, r.input_name.as_deref(), r.provider_id, r.virtual_id))
                    .collect::<Vec<_>>()
            )
        });
    assert_eq!(connect.provider_id, Some(member.provider_id));
    assert_eq!(connect.virtual_id, Some(member.member_vid.get()));
}

async fn wait_for_bound_member(session: &HlsSessionHandle, input: &str) {
    for _ in 0..500 {
        let session = session.read().await;
        if session.origin_source.input_name.as_ref() == input && session.origin_account_manifest_pending.is_none() {
            return;
        }
        drop(session);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let session = session.read().await;
    let bound = session.origin_source.input_name.to_string();
    panic!(
        "session never bound to {input}; still bound to {bound}, pending={:?}",
        session.origin_account_manifest_pending
    );
}

#[tokio::test]
async fn member_handoff_moves_group_session_to_next_member() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .hls_cache()
        .origin_mode("solis", FakeOriginMode::HlsAdvancing)
        .origin_mode("korath", FakeOriginMode::HlsAdvancing)
        .build()
        .await;

    let first_master = env.get_hls_manifest_text(env.group_vid).await;
    let first_proxy = proxy_session_id_from_master(&first_master);
    let variant_uri = first_segment_url(&first_master);
    let first_variant = env.get(&variant_uri).await;
    assert_eq!(first_variant.status(), StatusCode::OK);
    assert!(env.origin_hits("solis") > 0);

    let group_key = HlsSessionKey::for_group(env.target.id, env.group_vid);
    let session = env.app_state.hls.proxy.sessions().get_by_key(&group_key).await.expect("group session");
    let runtime = session.read().await.origin_account_runtime.clone().expect("origin runtime");
    runtime.notify_member_unhealthy(&session, HlsMemberUnhealthyReason::ManifestFailure);

    wait_for_bound_member(&session, "korath").await;

    let session = session.read().await;
    assert_eq!(session.origin_source.input_name.as_ref(), "korath");
    assert_eq!(session.origin_account_binding.as_ref().map(|binding| binding.input_name.as_ref()), Some("korath"));
    assert!(session.origin_account_binding.as_ref().is_some_and(tuliprox_hls::api::HlsOriginAccountBinding::is_active));
    drop(session);

    assert!(env.origin_hits("korath") > 0, "the new member origin must be fetched");
    assert_eq!(
        proxy_session_id_from_master(&env.get_hls_manifest_text(env.group_vid).await),
        first_proxy,
        "the client must keep its proxy session id"
    );
}

#[tokio::test]
async fn member_handoff_releases_old_reservation_without_leak() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .hls_cache()
        .origin_mode("solis", FakeOriginMode::HlsAdvancing)
        .origin_mode("korath", FakeOriginMode::HlsAdvancing)
        .build()
        .await;

    let first_master = env.get_hls_manifest_text(env.group_vid).await;
    let variant_uri = first_segment_url(&first_master);
    let first_variant = env.get(&variant_uri).await;
    assert_eq!(first_variant.status(), StatusCode::OK);

    let group_key = HlsSessionKey::for_group(env.target.id, env.group_vid);
    let session = env.app_state.hls.proxy.sessions().get_by_key(&group_key).await.expect("group session");
    let runtime = session.read().await.origin_account_runtime.clone().expect("origin runtime");
    runtime.notify_member_unhealthy(&session, HlsMemberUnhealthyReason::ManifestFailure);
    wait_for_bound_member(&session, "korath").await;

    // The old member's reservation must be released and the new member must not double-acquire.
    for _ in 0..100 {
        if env.app_state.active_provider.get_provider_connections_count() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        env.app_state.active_provider.get_provider_connections_count(),
        0,
        "the member handoff must release the old reservation without leaking a provider connection"
    );
    let connections = env.app_state.active_provider.active_connections();
    assert!(
        connections.as_ref().is_none_or(|connections| connections.get("solis").copied().unwrap_or(0) == 0),
        "solis must no longer hold a provider connection: {connections:?}"
    );
}

#[tokio::test]
async fn member_handoff_concurrent_notifications_cause_one_switch() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .hls_cache()
        .origin_mode("solis", FakeOriginMode::HlsAdvancing)
        .origin_mode("korath", FakeOriginMode::HlsAdvancing)
        .build()
        .await;

    let first_master = env.get_hls_manifest_text(env.group_vid).await;
    let variant_uri = first_segment_url(&first_master);
    let first_variant = env.get(&variant_uri).await;
    assert_eq!(first_variant.status(), StatusCode::OK);

    let group_key = HlsSessionKey::for_group(env.target.id, env.group_vid);
    let session = env.app_state.hls.proxy.sessions().get_by_key(&group_key).await.expect("group session");
    let runtime = session.read().await.origin_account_runtime.clone().expect("origin runtime");

    // Back-to-back notifications must coalesce into a single handoff.
    runtime.notify_member_unhealthy(&session, HlsMemberUnhealthyReason::ManifestFailure);
    runtime.notify_member_unhealthy(&session, HlsMemberUnhealthyReason::ManifestFailure);
    wait_for_bound_member(&session, "korath").await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let session = session.read().await;
    assert_eq!(session.origin_source.input_name.as_ref(), "korath", "exactly one switch must land on korath");
    assert!(session.origin_account_binding.as_ref().is_some_and(tuliprox_hls::api::HlsOriginAccountBinding::is_active));
    drop(session);
    assert_eq!(
        env.app_state.active_provider.get_provider_connections_count(),
        0,
        "coalesced notifications must not drive a second switch that leaks or re-acquires"
    );
}

#[tokio::test]
async fn member_handoff_single_member_group_does_not_loop() {
    let env = GroupPlaybackEnv::new(&["solis"]).hls_cache().build().await;

    let first_master = env.get_hls_manifest_text(env.group_vid).await;
    let variant_uri = first_segment_url(&first_master);
    let first_variant = env.get(&variant_uri).await;
    assert_eq!(first_variant.status(), StatusCode::OK);

    let group_key = HlsSessionKey::for_group(env.target.id, env.group_vid);
    let session = env.app_state.hls.proxy.sessions().get_by_key(&group_key).await.expect("group session");
    let runtime = session.read().await.origin_account_runtime.clone().expect("origin runtime");
    runtime.notify_member_unhealthy(&session, HlsMemberUnhealthyReason::ManifestFailure);

    for _ in 0..100 {
        if env.app_state.active_provider.get_provider_connections_count() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        env.app_state.active_provider.get_provider_connections_count(),
        0,
        "single-member handoff must release the reservation instead of looping"
    );
    assert_eq!(session.read().await.origin_source.input_name.as_ref(), "solis");
}

#[tokio::test]
async fn member_handoff_single_item_session_is_noop() {
    let env = GroupPlaybackEnv::new(&["solis"]).hls_cache().build().await;

    let plain_master = env.get_hls_manifest_text(env.plain_vid).await;
    let variant_uri = first_segment_url(&plain_master);
    let variant = env.get(&variant_uri).await;
    assert_eq!(variant.status(), StatusCode::OK);

    let proxy = tuliprox_hls::api::ProxySessionId(proxy_session_id_from_master(&plain_master));
    let session = env.app_state.hls.proxy.sessions().get_by_proxy_session_id(&proxy).await.expect("plain session");
    let before = session.read().await.origin_source.clone();
    let runtime = session.read().await.origin_account_runtime.clone().expect("origin runtime");
    runtime.notify_member_unhealthy(&session, HlsMemberUnhealthyReason::ManifestFailure);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let after = session.read().await.origin_source.clone();
    assert_eq!(before, after, "single-item sessions must ignore member-unhealthy notifications");
}

#[tokio::test]
async fn failover_disabled_installs_no_member_health_policy() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .hls_cache()
        .with_failover(FailoverGroupRuntimeConfig { enabled: false, ..Default::default() })
        .origin_mode("solis", FakeOriginMode::HlsAdvancing)
        .build()
        .await;

    let master = env.get_hls_manifest_text(env.group_vid).await;
    let _variant = env.get(&first_segment_url(&master)).await;

    let group_key = HlsSessionKey::for_group(env.target.id, env.group_vid);
    let session = env.app_state.hls.proxy.sessions().get_by_key(&group_key).await.expect("group session");
    let runtime = session.read().await.origin_account_runtime.clone().expect("origin runtime");
    let session_guard = session.read().await;
    assert!(
        runtime.member_health_policy(&session_guard).is_none(),
        "failover disabled must install no member-health policy"
    );
}

fn media_sequence(manifest: &str) -> u64 {
    manifest
        .lines()
        .find_map(|line| line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:"))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or_else(|| panic!("media sequence in manifest: {manifest}"))
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.expect("body");
    String::from_utf8_lossy(&bytes).into_owned()
}

fn origin_segment_hits(env: &GroupPlaybackEnv, input: &str) -> usize {
    env.origins
        .get(input)
        .map_or(0, |origin| origin.requests.lock().expect("mutex").iter().filter(|r| r.contains(".ts")).count())
}

/// Segment URIs of a manifest, each flagged with whether a bare `EXT-X-DISCONTINUITY` tag precedes it.
fn manifest_segments(manifest: &str) -> Vec<(String, bool)> {
    let mut discontinuity = false;
    let mut segments = Vec::new();
    for line in manifest.lines().map(str::trim) {
        if line == "#EXT-X-DISCONTINUITY" {
            discontinuity = true;
        } else if !line.is_empty() && !line.starts_with('#') {
            segments.push((line.to_string(), discontinuity));
            discontinuity = false;
        }
    }
    segments
}

/// Polls the canonical media playlist once and requests every segment it lists, returning the manifest.
async fn poll_and_fetch_segments(env: &GroupPlaybackEnv, variant_uri: &str) -> String {
    let response = env.get(variant_uri).await;
    assert_eq!(response.status(), StatusCode::OK);
    let manifest = body_text(response).await;
    for (segment_uri, _) in manifest_segments(&manifest) {
        let segment = env.get(&segment_uri).await;
        assert_eq!(segment.status(), StatusCode::OK, "segment {segment_uri}");
        let _ = body_text(segment).await;
    }
    manifest
}

#[tokio::test]
async fn member_handoff_keeps_one_valid_timeline_across_members() {
    // The members share a host but publish unrelated media sequences, like two independent providers.
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .hls_cache()
        .origin_mode("solis", FakeOriginMode::HlsAdvancing)
        .origin_mode("korath", FakeOriginMode::HlsAdvancingFrom(5000))
        .build()
        .await;

    let first_master = env.get_hls_manifest_text(env.group_vid).await;
    let variant_uri = first_segment_url(&first_master);
    let mut manifests = Vec::new();
    for _ in 0..3 {
        manifests.push(poll_and_fetch_segments(&env, &variant_uri).await);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(origin_segment_hits(&env, "solis") > 0, "segments must come from solis before the handoff");
    assert_eq!(origin_segment_hits(&env, "korath"), 0);
    let pre_handoff_segments: Vec<String> =
        manifests.iter().flat_map(|manifest| manifest_segments(manifest)).map(|(uri, _)| uri).collect();

    let group_key = HlsSessionKey::for_group(env.target.id, env.group_vid);
    let session = env.app_state.hls.proxy.sessions().get_by_key(&group_key).await.expect("group session");
    let runtime = session.read().await.origin_account_runtime.clone().expect("origin runtime");
    runtime.notify_member_unhealthy(&session, HlsMemberUnhealthyReason::SegmentFailure);
    wait_for_bound_member(&session, "korath").await;

    let mut boundary = None;
    for _ in 0..100 {
        let manifest = poll_and_fetch_segments(&env, &variant_uri).await;
        boundary = manifest_segments(&manifest)
            .into_iter()
            .find(|(uri, discontinuity)| *discontinuity && !pre_handoff_segments.contains(uri))
            .map(|(uri, _)| uri);
        manifests.push(manifest);
        if boundary.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let sequences: Vec<u64> = manifests.iter().map(|manifest| media_sequence(manifest)).collect();
    assert!(
        sequences.windows(2).all(|pair| pair[0] <= pair[1]),
        "EXT-X-MEDIA-SEQUENCE must never decrease: {sequences:?}"
    );
    assert!(
        boundary.is_some(),
        "a discontinuity must precede the first segment of the new member; last manifests: {:#?}",
        manifests.iter().rev().take(3).collect::<Vec<_>>()
    );
    // The boundary segment itself comes from the new member's origin.
    assert!(origin_segment_hits(&env, "korath") > 0, "segments must be served from the new member origin");
}

/// Faults of a member origin that can be switched on while a session runs.
#[derive(Clone, Default)]
struct OriginFaults {
    manifest_status: Arc<std::sync::atomic::AtomicU16>,
    segment_status: Arc<std::sync::atomic::AtomicU16>,
}

impl OriginFaults {
    fn mode(&self, start: usize) -> FakeOriginMode {
        FakeOriginMode::HlsAdvancingWithFault {
            start,
            manifest_status: Arc::clone(&self.manifest_status),
            segment_status: Arc::clone(&self.segment_status),
        }
    }

    fn fail_manifests(&self, status: u16) { self.manifest_status.store(status, std::sync::atomic::Ordering::Release); }

    fn fail_segments(&self, status: u16) { self.segment_status.store(status, std::sync::atomic::Ordering::Release); }
}

/// Runtime failover settings with the no-data timeout cut to the smallest value the config allows.
fn fast_failover() -> shared::model::FailoverGroupRuntimeConfig {
    shared::model::FailoverGroupRuntimeConfig { no_data_timeout_secs: 1, ..Default::default() }
}

/// Polls the canonical playlist once and requests its segments. A faulty origin may make either fail, so
/// statuses are not asserted; the playlist text is returned when the poll succeeded.
async fn poll_tolerant(env: &GroupPlaybackEnv, variant_uri: &str) -> Option<String> {
    let response = env.get(variant_uri).await;
    if response.status() != StatusCode::OK {
        return None;
    }
    let manifest = body_text(response).await;
    for (segment_uri, _) in manifest_segments(&manifest) {
        let _ = body_text(env.get(&segment_uri).await).await;
    }
    Some(manifest)
}

/// Keeps polling the same canonical playlist until a segment of a new member follows a discontinuity.
/// Returns every playlist seen and the boundary segment. The client never asks for a new entry.
async fn poll_until_boundary(
    env: &GroupPlaybackEnv,
    variant_uri: &str,
    known_segments: &[String],
) -> (Vec<String>, Option<String>) {
    let mut manifests = Vec::new();
    for _ in 0..80 {
        if let Some(manifest) = poll_tolerant(env, variant_uri).await {
            let boundary = manifest_segments(&manifest)
                .into_iter()
                .find(|(uri, discontinuity)| *discontinuity && !known_segments.contains(uri))
                .map(|(uri, _)| uri);
            manifests.push(manifest);
            if boundary.is_some() {
                return (manifests, boundary);
            }
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    (manifests, None)
}

/// Plays three healthy rounds from the first member and returns the canonical playlist uri and the
/// segment uris seen so far.
async fn play_healthy_rounds(env: &GroupPlaybackEnv) -> (String, Vec<String>, Vec<String>) {
    let first_master = env.get_hls_manifest_text(env.group_vid).await;
    let variant_uri = first_segment_url(&first_master);
    let mut manifests = Vec::new();
    for _ in 0..3 {
        manifests.push(poll_and_fetch_segments(env, &variant_uri).await);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(origin_segment_hits(env, "solis") > 0, "segments must come from solis before the fault");
    let segments = manifests.iter().flat_map(|manifest| manifest_segments(manifest)).map(|(uri, _)| uri).collect();
    (variant_uri, segments, manifests)
}

async fn assert_switched_to_korath(
    env: &GroupPlaybackEnv,
    mut manifests: Vec<String>,
    later: Vec<String>,
    boundary: Option<String>,
) {
    manifests.extend(later);
    let sequences: Vec<u64> = manifests.iter().map(|manifest| media_sequence(manifest)).collect();
    assert!(
        sequences.windows(2).all(|pair| pair[0] <= pair[1]),
        "EXT-X-MEDIA-SEQUENCE must never decrease: {sequences:?}"
    );
    assert!(
        boundary.is_some(),
        "the session must switch to korath behind a discontinuity; last playlists: {:#?}",
        manifests.iter().rev().take(3).collect::<Vec<_>>()
    );
    assert!(origin_segment_hits(env, "korath") > 0, "segments must be served from the new member origin");
    let group_key = HlsSessionKey::for_group(env.target.id, env.group_vid);
    let session = env.app_state.hls.proxy.sessions().get_by_key(&group_key).await.expect("group session");
    assert_eq!(session.read().await.origin_source.input_name.as_ref(), "korath");
}

#[tokio::test]
async fn failing_manifests_switch_the_running_session_to_the_next_member() {
    let faults = OriginFaults::default();
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .hls_cache()
        .with_failover(fast_failover())
        .origin_mode("solis", faults.mode(0))
        .origin_mode("korath", FakeOriginMode::HlsAdvancingFrom(5000))
        .build()
        .await;
    let (variant_uri, known_segments, manifests) = play_healthy_rounds(&env).await;

    faults.fail_manifests(500);
    let (later, boundary) = poll_until_boundary(&env, &variant_uri, &known_segments).await;

    assert_switched_to_korath(&env, manifests, later, boundary).await;
}

#[tokio::test]
async fn failing_segments_switch_the_running_session_to_the_next_member() {
    let faults = OriginFaults::default();
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .hls_cache()
        .with_failover(fast_failover())
        .origin_mode("solis", faults.mode(0))
        .origin_mode("korath", FakeOriginMode::HlsAdvancingFrom(5000))
        .build()
        .await;
    let (variant_uri, known_segments, manifests) = play_healthy_rounds(&env).await;

    faults.fail_segments(404);
    let (later, boundary) = poll_until_boundary(&env, &variant_uri, &known_segments).await;

    assert_switched_to_korath(&env, manifests, later, boundary).await;
}

#[tokio::test]
async fn single_item_session_keeps_its_origin_when_the_origin_fails() {
    let faults = OriginFaults::default();
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .hls_cache()
        .with_failover(fast_failover())
        .origin_mode("solis", faults.mode(0))
        .origin_mode("korath", FakeOriginMode::HlsAdvancingFrom(5000))
        .build()
        .await;
    let plain_master = env.get_hls_manifest_text(env.plain_vid).await;
    let variant_uri = first_segment_url(&plain_master);
    for _ in 0..2 {
        assert!(poll_tolerant(&env, &variant_uri).await.is_some());
    }
    let proxy = tuliprox_hls::api::ProxySessionId(proxy_session_id_from_master(&plain_master));
    let session = env.app_state.hls.proxy.sessions().get_by_proxy_session_id(&proxy).await.expect("plain session");
    let before = session.read().await.origin_source.clone();

    faults.fail_manifests(500);
    faults.fail_segments(404);
    for _ in 0..25 {
        let _ = poll_tolerant(&env, &variant_uri).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    assert_eq!(session.read().await.origin_source, before, "a single-item session never changes its origin");
    assert_eq!(env.origin_hits("korath"), 0, "no other member may be contacted for a single item");
}

fn lease_total(env: &GroupPlaybackEnv, input: &str) -> usize {
    env.app_state.active_provider.provider_lease_usage(&input.intern()).total()
}

#[tokio::test]
async fn legacy_manifest_failure_switches_to_next_member_with_rebase() {
    let faults = OriginFaults::default();
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .hls()
        .origin_mode("solis", faults.mode(0))
        .origin_mode("korath", FakeOriginMode::HlsAdvancingFrom(5000))
        .build()
        .await;

    let master = env.get_hls_manifest_text(env.group_vid).await;
    assert_eq!(env.bound_member_input("viewer").await, "solis");
    let variant_uri = first_segment_url(&master);
    let first = env.get(&variant_uri).await;
    assert_eq!(first.status(), StatusCode::OK);
    let last_solis_seq = media_sequence(&body_text(first).await);
    assert_eq!(env.origin_hits("korath"), 0);

    faults.fail_manifests(500);
    let switched = env.get(&variant_uri).await;
    assert_eq!(switched.status(), StatusCode::OK, "this poll must be answered with the new member's manifest");
    let switched_body = body_text(switched).await;

    assert_eq!(env.bound_member_input("viewer").await, "korath");
    let switched_seq = media_sequence(&switched_body);
    assert!(switched_seq > last_solis_seq, "the media sequence must continue: {switched_seq} > {last_solis_seq}");
    assert!(
        manifest_segments(&switched_body).iter().any(|(_, discontinuity)| *discontinuity),
        "the switch must insert a discontinuity: {switched_body}"
    );
    assert!(env.origin_hits("korath") > 0);
    assert_eq!(lease_total(&env, "solis"), 0, "the old member's reservation must be released");
    assert_eq!(lease_total(&env, "korath"), 1, "the new member must hold exactly one reservation");
}

#[tokio::test]
async fn legacy_segment_failure_switches_on_next_manifest_poll() {
    let faults = OriginFaults::default();
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .hls()
        .origin_mode("solis", faults.mode(0))
        .origin_mode("korath", FakeOriginMode::HlsAdvancingFrom(5000))
        .build()
        .await;

    let master = env.get_hls_manifest_text(env.group_vid).await;
    let variant_uri = first_segment_url(&master);
    let first = env.get(&variant_uri).await;
    assert_eq!(first.status(), StatusCode::OK);
    let segment_uri = first_segment_url(&body_text(first).await);
    assert_eq!(env.bound_member_input("viewer").await, "solis");

    // A segment of the bound member fails hard; this request keeps its status.
    faults.fail_segments(404);
    let segment = env.get(&segment_uri).await;
    assert_eq!(segment.status(), StatusCode::NOT_FOUND, "the failing segment keeps its own status");

    // The next manifest poll switches without serving the failed member again.
    let switched = env.get(&variant_uri).await;
    assert_eq!(switched.status(), StatusCode::OK, "the next poll must be answered with the new member");
    assert_eq!(env.bound_member_input("viewer").await, "korath");
    assert!(env.origin_hits("korath") > 0);
    assert_eq!(lease_total(&env, "solis"), 0, "the old member's reservation must be released");
    assert_eq!(lease_total(&env, "korath"), 1, "the new member must hold exactly one reservation");
}

#[tokio::test]
async fn legacy_single_item_session_keeps_its_origin_on_failure() {
    let faults = OriginFaults::default();
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .hls()
        .origin_mode("solis", faults.mode(0))
        .origin_mode("korath", FakeOriginMode::HlsAdvancingFrom(5000))
        .build()
        .await;

    let plain_master = env.get_hls_manifest_text(env.plain_vid).await;
    let variant_uri = first_segment_url(&plain_master);
    assert_eq!(env.get(&variant_uri).await.status(), StatusCode::OK);

    faults.fail_manifests(500);
    let response = env.get(&variant_uri).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND, "a single-item legacy session keeps its terminal response");
    assert_eq!(env.origin_hits("korath"), 0, "no other member may be contacted for a single item");
}

#[tokio::test]
async fn legacy_budget_exhaustion_keeps_existing_error() {
    let solis_faults = OriginFaults::default();
    let korath_faults = OriginFaults::default();
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .hls()
        .with_failover(shared::model::FailoverGroupRuntimeConfig { max_switches: 1, ..Default::default() })
        .origin_mode("solis", solis_faults.mode(0))
        .origin_mode("korath", korath_faults.mode(5000))
        .build()
        .await;

    let master = env.get_hls_manifest_text(env.group_vid).await;
    let variant_uri = first_segment_url(&master);
    assert_eq!(env.get(&variant_uri).await.status(), StatusCode::OK);

    // One switch is allowed: solis -> korath.
    solis_faults.fail_manifests(500);
    let switched = env.get(&variant_uri).await;
    assert_eq!(switched.status(), StatusCode::OK);
    assert_eq!(env.bound_member_input("viewer").await, "korath");

    // The second failure exhausts the switch budget and keeps the terminal response.
    korath_faults.fail_manifests(500);
    let response = env.get(&variant_uri).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND, "budget exhaustion keeps the existing terminal response");
}

#[tokio::test]
async fn legacy_stable_member_segment_failure_retries_then_switches() {
    let faults = OriginFaults::default();
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .hls()
        .with_failover(shared::model::FailoverGroupRuntimeConfig {
            stable_after_secs: 1,
            same_source_retries: 1,
            ..Default::default()
        })
        .origin_mode("solis", faults.mode(0))
        .origin_mode("korath", FakeOriginMode::HlsAdvancingFrom(5000))
        .build()
        .await;

    let master = env.get_hls_manifest_text(env.group_vid).await;
    let variant_uri = first_segment_url(&master);
    let first = env.get(&variant_uri).await;
    assert_eq!(first.status(), StatusCode::OK);
    let segment_uri = first_segment_url(&body_text(first).await);

    // Let the member count as stable (`stable_after_secs: 1`).
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // First segment failure: the stable member gets one same-member retry, so the next poll
    // still serves solis (no switch yet).
    faults.fail_segments(404);
    assert_eq!(env.get(&segment_uri).await.status(), StatusCode::NOT_FOUND);
    let poll = env.get(&variant_uri).await;
    assert_eq!(poll.status(), StatusCode::OK, "the stable member is retried before switching");
    assert_eq!(env.bound_member_input("viewer").await, "solis");

    // Second segment failure: the same-member retry is spent, so the group switches.
    let segment_uri = first_segment_url(&body_text(poll).await);
    assert_eq!(env.get(&segment_uri).await.status(), StatusCode::NOT_FOUND);
    let switched = env.get(&variant_uri).await;
    assert_eq!(switched.status(), StatusCode::OK);
    assert_eq!(env.bound_member_input("viewer").await, "korath");
}

#[tokio::test]
async fn legacy_single_member_group_keeps_reservation_after_segment_failure() {
    let faults = OriginFaults::default();
    let env = GroupPlaybackEnv::builder(&["solis"]).hls().origin_mode("solis", faults.mode(0)).build().await;

    let master = env.get_hls_manifest_text(env.group_vid).await;
    let variant_uri = first_segment_url(&master);
    let first = env.get(&variant_uri).await;
    assert_eq!(first.status(), StatusCode::OK);
    let segment_uri = first_segment_url(&body_text(first).await);
    assert_eq!(lease_total(&env, "solis"), 1);

    // A segment failure of the only member marks it failed, but there is no member to switch to.
    faults.fail_segments(404);
    assert_eq!(env.get(&segment_uri).await.status(), StatusCode::NOT_FOUND);

    let poll = env.get(&variant_uri).await;
    assert_eq!(poll.status(), StatusCode::OK);
    assert_eq!(env.bound_member_input("viewer").await, "solis");
    assert_eq!(lease_total(&env, "solis"), 1, "a one-member group must keep its reservation");
}

#[tokio::test]
async fn legacy_discontinuity_sequence_advances_after_boundary_leaves_window() {
    let faults = OriginFaults::default();
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .hls()
        .origin_mode("solis", faults.mode(0))
        .origin_mode("korath", FakeOriginMode::HlsAdvancingFrom(5000))
        .build()
        .await;

    let master = env.get_hls_manifest_text(env.group_vid).await;
    let variant_uri = first_segment_url(&master);
    assert_eq!(env.get(&variant_uri).await.status(), StatusCode::OK);

    faults.fail_manifests(500);
    let switched = env.get(&variant_uri).await;
    assert_eq!(switched.status(), StatusCode::OK);
    let switched_body = body_text(switched).await;
    assert!(switched_body.lines().any(|line| line == "#EXT-X-DISCONTINUITY-SEQUENCE:0"), "{switched_body}");
    assert!(switched_body.lines().any(|line| line == "#EXT-X-DISCONTINUITY"), "{switched_body}");

    // The next poll of the new member has slid past the boundary: the discontinuity sequence
    // advances and the boundary itself is gone.
    let next = env.get(&variant_uri).await;
    assert_eq!(next.status(), StatusCode::OK);
    let next_body = body_text(next).await;
    assert!(next_body.lines().any(|line| line == "#EXT-X-DISCONTINUITY-SEQUENCE:1"), "{next_body}");
    assert!(!next_body.lines().any(|line| line == "#EXT-X-DISCONTINUITY"), "{next_body}");
}

#[tokio::test]
async fn legacy_two_sequential_switches_keep_monotonic_sequence() {
    let solis_faults = OriginFaults::default();
    let korath_faults = OriginFaults::default();
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .hls()
        .origin_mode("solis", solis_faults.mode(0))
        .origin_mode("korath", korath_faults.mode(5000))
        .build()
        .await;

    let master = env.get_hls_manifest_text(env.group_vid).await;
    let variant_uri = first_segment_url(&master);
    let first = env.get(&variant_uri).await;
    assert_eq!(first.status(), StatusCode::OK);
    let mut last_seq = media_sequence(&body_text(first).await);

    // solis -> korath
    solis_faults.fail_manifests(500);
    let switched = env.get(&variant_uri).await;
    assert_eq!(switched.status(), StatusCode::OK);
    let seq = media_sequence(&body_text(switched).await);
    assert!(seq > last_seq, "the sequence continues across the first switch: {seq} > {last_seq}");
    last_seq = seq;

    // korath -> solis (solis recovered)
    solis_faults.fail_manifests(0);
    korath_faults.fail_manifests(500);
    let switched = env.get(&variant_uri).await;
    assert_eq!(switched.status(), StatusCode::OK);
    let seq = media_sequence(&body_text(switched).await);
    assert!(seq > last_seq, "the sequence continues across the second switch: {seq} > {last_seq}");
}

#[tokio::test]
async fn legacy_concurrent_failing_polls_probe_next_member_once() {
    let faults = OriginFaults::default();
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .hls()
        .origin_mode("solis", faults.mode(0))
        .origin_mode("korath", FakeOriginMode::HlsAdvancingFrom(5000))
        .build()
        .await;

    let master = env.get_hls_manifest_text(env.group_vid).await;
    let variant_uri = first_segment_url(&master);
    assert_eq!(env.get(&variant_uri).await.status(), StatusCode::OK);

    faults.fail_manifests(500);
    let uri = variant_uri.clone();
    let (a, b) = tokio::join!(env.get(&variant_uri), env.get(&uri));
    assert!(
        a.status().is_success() || b.status().is_success(),
        "one poll must serve the new member: {} {}",
        a.status(),
        b.status()
    );
    assert_eq!(env.origin_hits("korath"), 1, "the next member must be probed exactly once");
}

#[tokio::test]
async fn legacy_removed_bound_member_does_not_panic() {
    let faults = OriginFaults::default();
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .hls()
        .origin_mode("solis", faults.mode(0))
        .origin_mode("korath", FakeOriginMode::HlsAdvancingFrom(5000))
        .build()
        .await;

    let user = env.app_state.app_config.get_user_credentials("viewer").expect("test user");
    let input = env.app_state.app_config.get_input_by_name(&"solis".intern()).expect("member input");
    let addr: SocketAddr = "127.0.0.1:12345".parse().expect("addr");
    let fingerprint = Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), addr);
    let provider: Arc<str> = "solis".intern();
    let token = "legacy-removed-member";

    env.app_state
        .active_users
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: token,
            virtual_id: env.group_vid,
            provider: &provider,
            stream_url: &format!("http://{}/live/101", env.origins.get("solis").expect("origin").addr),
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    // A stale binding whose member is no longer in the group and whose index is out of bounds.
    env.app_state
        .active_users
        .bind_member(
            &user.username,
            token,
            MemberBinding {
                group_vid: env.group_vid,
                member_vid: 999_999,
                member_index: 999,
                input_name: Arc::clone(&input.name),
                member_title: "ZDF HD".intern(),
                provider: Arc::clone(&provider),
            },
        )
        .await;
    let session = env.app_state.active_users.get_and_update_user_session(&user.username, token).await.expect("session");
    let source = super::super::resolve_hls_virtual_source_for_target(&env.app_state, &env.target, env.group_vid)
        .await
        .expect("group stream context");

    // The removed member cannot resolve a playlist URL; the request must fail without panicking.
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
        super::super::HlsRequestStage::LegacyVariant,
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}
