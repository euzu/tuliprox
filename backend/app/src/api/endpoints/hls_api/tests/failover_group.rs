use crate::{
    api::{
        api_utils::tests::group_playback_support::{first_segment_url, GroupPlaybackEnv},
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
use tuliprox_session::MemberBinding;

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
