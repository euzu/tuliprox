use super::*;
use crate::{
    api::model::{
        ActiveProviderManager, ActiveUserManager, ConnectionKind, ConnectionManager, EventManager, SharedStreamManager,
    },
    model::ConfigInput,
    repository::GeoIp,
};
use shared::model::{InputFetchMethod, InputType};
use tuliprox_session::{stream_ctx::ProviderStreamCtx, ManagedProviderHandle};

const SEGMENT_BODY: &[u8] = b"finite hls segment payload that must arrive untruncated";

fn single_slot_app_config() -> Arc<AppConfig> {
    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: "http://provider-1.example".to_string(),
        username: Some("user1".to_string()),
        password: Some("pass1".to_string()),
        enabled: true,
        priority: 0,
        max_connections: 1,
        method: InputFetchMethod::default(),
        aliases: None,
        ..ConfigInput::default()
    });
    let base = test_app_config();
    base.sources.store(Arc::new(SourcesConfig { inputs: vec![input], ..SourcesConfig::default() }));
    base
}

fn stream_ctx(app_config: &Arc<AppConfig>) -> ProviderStreamCtx {
    let event_manager = Arc::new(EventManager::new());
    let active_provider = Arc::new(ActiveProviderManager::new(app_config, &event_manager));
    let shared_stream_manager = Arc::new(SharedStreamManager::new(Arc::clone(&active_provider)));
    active_provider.set_shared_stream_manager(&shared_stream_manager);
    let geo_ip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let active_users = Arc::new(ActiveUserManager::new(&app_config.config.load(), &geo_ip, &event_manager));
    let connection_manager =
        Arc::new(ConnectionManager::new(&active_users, &active_provider, &shared_stream_manager, &event_manager, None));
    ProviderStreamCtx {
        active_provider,
        app_config: Arc::clone(app_config),
        connection_manager,
        http_clients: Arc::default(),
    }
}

async fn spawn_segment_origin() -> Url {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("segment origin binds");
    let addr = listener.local_addr().expect("segment origin address");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("segment origin accepts");
        let _ = read_http_request(&mut socket).await;
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            SEGMENT_BODY.len()
        );
        socket.write_all(head.as_bytes()).await.expect("segment head");
        socket.write_all(SEGMENT_BODY).await.expect("segment body");
        socket.shutdown().await.expect("segment origin closes");
    });
    Url::parse(&format!("http://{addr}/hls/000003.ts")).expect("segment url")
}

fn segment_options(stream_url: &Url, response_mode: StreamResponseMode) -> ProviderStreamFactoryOptions {
    let stream_options = StreamOptions {
        stream_retry: false,
        buffer_enabled: false,
        buffer_size: 0,
        buffer_max_bytes: 0,
        pipe_provider_stream: false,
        response_mode,
    };
    ProviderStreamFactoryOptions::new(&ProviderStreamFactoryParams {
        addr: "127.0.0.1:8080".parse().unwrap(),
        item_type: PlaylistItemType::Live,
        share_stream: false,
        stream_options: &stream_options,
        stream_url,
        req_headers: &HeaderMap::new(),
        input_headers: None,
        session_headers: None,
        disabled_headers: None,
        default_user_agent: None,
        username: None,
        client_ip: None,
        stream_channel: None,
        connect_failure_stage: None,
        content_representation: ProviderContentRepresentationMode::Identity,
    })
}

/// Opens one response through the lifecycle path and waits until the upstream body ended.
async fn open_and_let_upstream_finish(
    ctx: &ProviderStreamCtx,
    response_mode: StreamResponseMode,
) -> (ManagedProviderHandle, BoxedProviderStream) {
    let handle = ctx
        .active_provider
        .acquire_connection_with_grace_for_session(
            &"provider_1".intern(),
            &"127.0.0.1:50100".parse().unwrap(),
            false,
            0,
            ConnectionKind::Normal,
            None,
        )
        .expect("slot available");
    let managed = ManagedProviderHandle::new(Arc::clone(&ctx.active_provider), handle);
    let lifecycle = ProviderStreamOpenLifecycle::from_managed(&managed).expect("lifecycle");
    let stream_url = spawn_segment_origin().await;
    let open = open_provider_stream_with_lifecycle(
        ctx,
        &reqwest::Client::new(),
        segment_options(&stream_url, response_mode),
        Some(lifecycle),
    )
    .await
    .expect("provider stream opens");
    let ProviderStreamOpen::Stream(ProviderStreamFactoryResponse { stream, .. }) = open else {
        panic!("expected a provider stream");
    };
    // The consumer has not read yet; the upstream body ends and the owner task completes on its own.
    tokio::time::timeout(
        Duration::from_secs(2),
        managed.handle().unwrap().completion_token.as_ref().unwrap().cancelled(),
    )
    .await
    .expect("body owner completes on upstream EOF");
    (managed, stream)
}

#[tokio::test]
async fn hls_resource_frees_slot_on_upstream_eof_and_still_delivers_all_bytes() {
    let ctx = stream_ctx(&single_slot_app_config());
    let (managed, stream) = open_and_let_upstream_finish(&ctx, StreamResponseMode::HlsResource).await;

    // The EOF release runs in a separately spawned task; wait for it instead of sleeping.
    tokio::time::timeout(Duration::from_secs(2), async {
        while ctx.active_provider.get_provider_connections_count() != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("EOF release frees the slot");
    assert_eq!(ctx.active_provider.get_provider_connections_count(), 0, "slot is free before the client reads");
    assert_eq!(collect_stream(stream).await.expect("buffered segment streams"), SEGMENT_BODY);

    drop(managed);
    assert_eq!(ctx.active_provider.get_provider_connections_count(), 0, "late handle drop is a no-op");
}

#[tokio::test]
async fn regular_stream_keeps_slot_until_handle_is_released() {
    let ctx = stream_ctx(&single_slot_app_config());
    let (managed, stream) = open_and_let_upstream_finish(&ctx, StreamResponseMode::Stream).await;

    assert_eq!(ctx.active_provider.get_provider_connections_count(), 1, "non-HLS streams hold the slot");
    assert_eq!(collect_stream(stream).await.expect("segment streams"), SEGMENT_BODY);
    assert_eq!(ctx.active_provider.get_provider_connections_count(), 1);

    drop(managed);
    assert_eq!(ctx.active_provider.get_provider_connections_count(), 0);
}
