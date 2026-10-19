use crate::api::api_utils::tests::group_playback_support::{first_segment_url, GroupPlaybackEnv};
use axum::http::StatusCode;

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
    assert!(!bound.is_empty());
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
