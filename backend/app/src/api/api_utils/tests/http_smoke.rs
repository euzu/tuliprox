use super::{
    create_test_app_state_for_config, create_test_fingerprint, create_test_provider_app_config,
    force_provider_stream_response, load_test_channel, load_test_session, load_test_user, spawn_load_test_origin,
    validate_load_response, ForceStreamRequestContext,
};
use crate::{
    api::model::AppState,
    model::{ConfigInput, SourcesConfig},
};
use arc_swap::ArcSwap;
use axum::{
    extract::{ConnectInfo, State},
    http::HeaderMap,
    response::IntoResponse,
    routing::get,
    Router,
};
use http_body_util::BodyExt;
use shared::{model::InputType, utils::Internable};
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use tokio::net::TcpListener;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_provider_lease_smoke_test() {
    const TS_PACKET: [u8; 188] = [0x47; 188];
    let origin_addr = spawn_load_test_origin(&TS_PACKET).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_smoke".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_addr}"),
        enabled: true,
        priority: 0,
        max_connections: 0,
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app = create_test_app_state_for_config(Arc::new(config));

    for concurrency in [1usize, 2] {
        let rounds = 5;
        let mut tasks = tokio::task::JoinSet::new();
        for task_idx in 0..concurrency {
            let app = Arc::clone(&app);
            let input = Arc::clone(&input);
            let user = load_test_user(&format!("smoke-user-{task_idx}"));
            tasks.spawn(async move {
                for r in 0..rounds {
                    let addr = SocketAddr::from((
                        [127, 0, 0, 1],
                        48_000 + u16::try_from(task_idx).expect("smoke task index fits a port offset"),
                    ));
                    let fingerprint = create_test_fingerprint(addr);
                    let token = format!("smoke-{task_idx}-{r}");
                    let session = load_test_session(&input, origin_addr, &token, addr);
                    let channel = load_test_channel(&input, origin_addr);

                    let response = force_provider_stream_response(
                        &fingerprint,
                        &app,
                        &session,
                        channel,
                        ForceStreamRequestContext {
                            req_headers: &HeaderMap::new(),
                            input: &input,
                            user: &user,
                            session_reservation_ttl_secs: 0,
                            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
                        },
                        None,
                    )
                    .await
                    .into_response();

                    let status = response.status();
                    let body = response.into_body().collect().await.expect("smoke body collects").to_bytes();
                    validate_load_response(status, &body, &TS_PACKET).expect("smoke response must match the TS packet");
                }
            });
        }
        while let Some(res) = tasks.join_next().await {
            res.expect("smoke task must succeed");
        }
    }

    for _ in 0..50 {
        if app.active_provider.get_provider_connections_count() == 0
            && app.active_provider.provider_lease_usage(&input.name).total() == 0
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    assert_eq!(app.active_provider.get_provider_connections_count(), 0, "smoke slots baseline");
    assert_eq!(app.active_provider.provider_lease_usage(&input.name).total(), 0, "smoke lease baseline");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_provider_lease_downstream_smoke_test() {
    #[derive(Clone)]
    struct DownstreamState {
        app: Arc<AppState>,
        input: Arc<ConfigInput>,
        origin_addr: SocketAddr,
    }

    async fn stream_handler(
        State(state): State<DownstreamState>,
        ConnectInfo(addr): ConnectInfo<SocketAddr>,
    ) -> axum::response::Response {
        let fingerprint = create_test_fingerprint(addr);
        let user = load_test_user("downstream-user");
        let session = load_test_session(&state.input, state.origin_addr, "downstream-token", addr);
        let channel = load_test_channel(&state.input, state.origin_addr);
        force_provider_stream_response(
            &fingerprint,
            &state.app,
            &session,
            channel,
            ForceStreamRequestContext {
                req_headers: &HeaderMap::new(),
                input: &state.input,
                user: &user,
                session_reservation_ttl_secs: 0,
                content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
            },
            None,
        )
        .await
        .into_response()
    }

    const TS_PACKET: [u8; 188] = [0x47; 188];
    let origin_addr = spawn_load_test_origin(&TS_PACKET).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_downstream".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_addr}"),
        enabled: true,
        priority: 0,
        max_connections: 0,
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app = create_test_app_state_for_config(Arc::new(config));

    let router = Router::new().route("/stream", get(stream_handler)).with_state(DownstreamState {
        app: Arc::clone(&app),
        input: Arc::clone(&input),
        origin_addr,
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("downstream listener binds");
    let downstream_addr = listener.local_addr().expect("downstream address");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await;
    });

    let client = reqwest::Client::new();
    for concurrency in [1usize, 2] {
        let mut tasks = tokio::task::JoinSet::new();
        for task_idx in 0..concurrency {
            let client = client.clone();
            let url = format!("http://{downstream_addr}/stream");
            tasks.spawn(async move {
                for round in 0..3 {
                    let response = client.get(&url).send().await.expect("downstream response");
                    let status = response.status();
                    let body = response.bytes().await.expect("downstream body");
                    validate_load_response(status, &body, &TS_PACKET)
                        .unwrap_or_else(|err| panic!("downstream task {task_idx} round {round}: {err}"));
                }
            });
        }
        while let Some(res) = tasks.join_next().await {
            res.expect("downstream task must succeed");
        }
    }

    for _ in 0..50 {
        if app.active_provider.get_provider_connections_count() == 0
            && app.active_provider.provider_lease_usage(&input.name).total() == 0
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(app.active_provider.get_provider_connections_count(), 0, "downstream slots baseline");
    assert_eq!(app.active_provider.provider_lease_usage(&input.name).total(), 0, "downstream lease baseline");

    server.abort();
}
