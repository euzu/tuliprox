use super::{
    create_test_app_state_for_config, create_test_fingerprint, create_test_live_channel,
    create_test_provider_app_config, create_test_provider_app_state, stream_response,
};
use crate::{
    api::{api_utils::blocks_new_playback, model::AppState},
    model::{ConfigInput, ConfigInputAlias, ProxyUserCredentials, SourcesConfig, RECORDING_PROXY_USERNAME},
};
use shared::utils::Internable;
use std::sync::Arc;

/// The body of the function starting with `signature`, up to its closing brace in column 0,
/// so an order check cannot match code of a neighbouring function.
fn function_body<'a>(source: &'a str, signature: &str) -> &'a str {
    let start = source.find(signature).expect("function present");
    let body = &source[start..];
    &body[..body.find("\n}\n").expect("function end")]
}

fn viewer() -> ProxyUserCredentials {
    let mut user = ProxyUserCredentials::default();
    user.username = "viewer".to_string();
    user
}

fn provider_state() -> (Arc<AppState>, Arc<ConfigInput>) {
    let app_state = create_test_app_state_for_config(Arc::new(create_test_provider_app_config()));
    let input = Arc::clone(&app_state.app_config.sources.load().inputs[0]);
    (app_state, input)
}

#[tokio::test]
async fn input_not_in_maintenance_is_not_gated() {
    let (app_state, input) = provider_state();
    assert!(!blocks_new_playback(&app_state, &input, &viewer(), false));
}

#[tokio::test]
async fn input_in_maintenance_is_gated() {
    let (app_state, input) = provider_state();
    app_state.input_maintenance.add(&input.name, None, 1).await.unwrap();
    assert!(blocks_new_playback(&app_state, &input, &viewer(), false));
}

#[tokio::test]
async fn preacquired_handle_is_not_gated() {
    let (app_state, input) = provider_state();
    app_state.input_maintenance.add(&input.name, None, 1).await.unwrap();
    assert!(!blocks_new_playback(&app_state, &input, &viewer(), true));
}

#[tokio::test]
async fn recording_proxy_user_is_not_gated() {
    let (app_state, input) = provider_state();
    app_state.input_maintenance.add(&input.name, None, 1).await.unwrap();
    let mut recorder = ProxyUserCredentials::default();
    recorder.username = RECORDING_PROXY_USERNAME.to_string();
    recorder.t_is_api_user = true;
    assert!(recorder.is_recording_proxy_user());
    assert!(!blocks_new_playback(&app_state, &input, &recorder, false));
}

#[tokio::test]
async fn aliased_input_is_gated_by_canonical_name() {
    let (app_state, provider) = provider_state();
    let input = Arc::new(ConfigInput {
        aliases: Some(vec![ConfigInputAlias {
            id: 2,
            name: "provider_1_alias".intern(),
            url: "http://127.0.0.1:2".to_string(),
            username: Some("user-b".to_string()),
            password: Some("pass-b".to_string()),
            priority: 1,
            max_connections: 3,
            exp_date: None,
            enabled: true,
            stalker: None,
        }]),
        ..(*provider).clone()
    });
    app_state
        .app_config
        .sources
        .store(Arc::new(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    app_state.input_maintenance.add(&input.name, None, 1).await.unwrap();
    assert!(!app_state.input_maintenance.is_in_maintenance("provider_1_alias"), "aliases are never keys");
    assert!(blocks_new_playback(&app_state, &input, &viewer(), false));
}

#[test]
fn gate_runs_before_shared_stream_join() {
    // A new viewer must not join a running provider stream of an input in maintenance.
    let body = function_body(
        include_str!("../provider_acquire.rs"),
        "pub(crate) async fn stream_response_with_provider_handle(",
    );
    let gate = body.find("if blocks_new_playback(").expect("gate present");
    let join = body.find("try_shared_stream_response_if_any(").expect("shared join present");
    assert!(gate < join, "maintenance gate must run before the shared stream join");
}

#[test]
fn redirect_defers_maintenance_inputs_to_the_gated_proxy_path() {
    // Redirect mode would send the client straight to the provider; for an input in
    // maintenance it must fall through to the proxied path, where gate 1 answers.
    let body = function_body(include_str!("../strategy.rs"), "pub async fn redirect_response<");
    let gate = body.find("if blocks_new_playback(").expect("gate present");
    let redirect = body.find("Some(redirect(").expect("redirect response present");
    assert!(gate < redirect, "maintenance must be checked before any redirect is built");
}

/// One MPEG-TS packet whose payload marks it as the maintenance clip.
fn maintenance_packet() -> Vec<u8> {
    let mut packet = vec![0xff_u8; 188];
    packet[0] = 0x47;
    packet[1] = 0x01;
    packet[2] = 0x00;
    packet[3] = 0x10;
    packet[4..9].copy_from_slice(b"MAINT");
    packet
}

fn maintenance_clip_target() -> Arc<crate::model::ConfigTarget> {
    Arc::new(crate::model::ConfigTarget {
        curation: None,
        id: 1,
        enabled: true,
        name: "test".to_string(),
        options: None,
        sort: None,
        filter: shared::foundation::Filter::default().into(),
        output: Vec::new(),
        rename: None,
        mapping_ids: None,
        mapping: Arc::new(arc_swap::ArcSwapOption::default()),
        favourites: None,
        processing_order: shared::model::ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
    })
}

#[tokio::test]
async fn new_ts_stream_for_input_in_maintenance_gets_the_clip_without_provider_slot() {
    use axum::{http::HeaderMap, response::IntoResponse};
    use http_body_util::BodyExt;

    let app_state = create_test_provider_app_state();
    let mut cfg = (**app_state.app_config.config.load()).clone();
    cfg.custom_stream_response_enabled = true;
    app_state.app_config.config.store(Arc::new(cfg));
    app_state.app_config.custom_stream_response.store(Some(Arc::new(crate::model::CustomStreamResponse {
        channel_unavailable: None,
        user_connections_exhausted: None,
        provider_connections_exhausted: None,
        low_priority_preempted: None,
        user_account_expired: None,
        panel_api_provisioning: None,
        hls_session_or_lease_expired: None,
        input_maintenance: Some(crate::api::model::TransportStreamBuffer::new(maintenance_packet())),
        panel_api_provisioning_hls_segments: Vec::new(),
    })));
    let input = provider_state_input(&app_state);
    app_state.input_maintenance.add(&input.name, None, 1).await.unwrap();
    // Unreachable: a provider connect attempt would fail instead of answering with the clip.
    let stream_url = "http://127.0.0.1:1/live/user/pass/42.ts";

    let response = stream_response(
        &create_test_fingerprint("127.0.0.1:55201".parse().unwrap()),
        &app_state,
        "maintenance-session",
        None,
        create_test_live_channel(stream_url),
        stream_url,
        None,
        &HeaderMap::default(),
        &input,
        &maintenance_clip_target(),
        &viewer(),
        shared::model::UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        false,
        None,
    )
    .await
    .into_response();

    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 0, "no provider slot was taken");
    let body = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut body = response.into_body();
        let frame = body.frame().await;
        frame.and_then(Result::ok).and_then(|frame| frame.into_data().ok()).unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    assert!(body.windows(5).any(|window| window == b"MAINT"), "the maintenance clip is served");
}

fn provider_state_input(app_state: &AppState) -> Arc<ConfigInput> {
    Arc::clone(&app_state.app_config.sources.load().inputs[0])
}
