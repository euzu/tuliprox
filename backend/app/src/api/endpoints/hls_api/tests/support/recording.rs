use super::{
    super::{
        header, hls_api_register, AppState, Arc, AsyncReadExt, AsyncWriteExt, Body, ConfigInput, ConfigTarget,
        ConnectInfo, HashMap, Internable, M3uPlaylistItem, PlaylistItem, Request, Response, ServiceExt, TcpListener,
        VirtualId, XtreamCluster,
    },
    cache_test_m3u_hls_item, path_has_extension, test_addr,
};

/// Origin that only answers requests carrying the configured recording header.
pub(in crate::api::endpoints::hls_api::tests) async fn spawn_recording_header_origin(
) -> (String, Arc<std::sync::Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
    let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = Arc::clone(&requests);
    let task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut chunk = [0_u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => request.extend_from_slice(&chunk[..read]),
                    }
                }
                let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                seen.lock().expect("request log").push(request.clone());
                let (status, body): (&str, &[u8]) = if !request.contains("\r\nx-recording-test: custom\r\n") {
                    ("403 Forbidden", b"")
                } else if path_has_extension(&path, "m3u8") {
                    (
                        "200 OK",
                        b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:4.0,\nseg1.ts\n",
                    )
                } else {
                    ("200 OK", b"segment-bytes")
                };
                let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(body).await;
            });
        }
    });
    (base_url, requests, task)
}

pub(in crate::api::endpoints::hls_api::tests) fn recording_test_router(app_state: &Arc<AppState>) -> axum::Router {
    axum::Router::new()
        .nest(
            "/tuliprox/api/v1",
            crate::api::endpoints::v1_api_playlist::v1_api_playlist_register_public(axum::Router::new()),
        )
        .merge(hls_api_register())
        .merge(crate::api::endpoints::m3u_api::m3u_api_register())
        .merge(crate::api::endpoints::xtream_api::xtream_api_register())
        .with_state(Arc::clone(app_state))
}

pub(in crate::api::endpoints::hls_api::tests) async fn recording_test_response(
    router: &axum::Router,
    uri: &str,
) -> Result<Response<Body>, Box<dyn std::error::Error>> {
    let mut request = Request::builder()
        .uri(uri)
        .header(header::USER_AGENT, shared::model::RECORDING_STREAM_USER_AGENT)
        .body(Body::empty())?;
    request.extensions_mut().insert(ConnectInfo(test_addr()));
    Ok(router.clone().oneshot(request).await?)
}

pub(in crate::api::endpoints::hls_api::tests) fn configure_recording_test_listener(
    app_state: &Arc<AppState>,
    storage_root: &std::path::Path,
) {
    let mut config = (*app_state.app_config.config.load_full()).clone();
    config.api.host = "0.0.0.0".to_string();
    config.api.port = 8901;
    config.storage_dir = storage_root.to_string_lossy().into_owned();
    config.web_ui = Some(crate::model::WebUiConfig::from(&shared::model::WebUiConfigDto {
        path: Some("/tuliprox".to_string()),
        ..Default::default()
    }));
    let mut recording = crate::model::RecordingConfig::from(&shared::model::RecordingConfigDto::default());
    recording.headers = HashMap::from([("X-Recording-Test".to_string(), "custom".to_string())]);
    config.video = Some(crate::model::VideoConfig { extensions: vec![], web_search: None, recording: Some(recording) });
    app_state.app_config.config.store(Arc::new(config));
}

pub(in crate::api::endpoints::hls_api::tests) fn recording_test_url(
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
    input: &ConfigInput,
    cluster: XtreamCluster,
) -> Result<String, Box<dyn std::error::Error>> {
    tuliprox_dvr::recording::recording_url::build_stable_recording_url(
        &app_state.app_config,
        &target.name,
        &input.name,
        12345,
        cluster,
        None,
    )
    .ok_or_else(|| "recording URL missing".into())
}

pub(in crate::api::endpoints::hls_api::tests) async fn cache_recording_test_item(
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
    item: M3uPlaylistItem,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::{
        api::model::{PlaylistStorage, PlaylistXtreamStorage},
        repository::{BPlusTree, VirtualIdRecord},
    };
    if target.has_output(shared::model::TargetType::Xtream) {
        let item = shared::model::XtreamPlaylistItem::from(&PlaylistItem::from(&item));
        let mut mapping = BPlusTree::new();
        mapping.insert(
            item.virtual_id,
            VirtualIdRecord::new(
                item.provider_id,
                item.virtual_id,
                item.item_type,
                VirtualId::new(0),
                shared::model::UUIDType::default(),
            ),
        );
        app_state.playlists.cache_id_mapping(&target.name, mapping).await;
        let mut live = BPlusTree::new();
        let mut vod = BPlusTree::new();
        let mut series = BPlusTree::new();
        match item.item_type.cluster() {
            XtreamCluster::Live => live.insert(item.virtual_id.get(), item),
            XtreamCluster::Video => vod.insert(item.virtual_id.get(), item),
            XtreamCluster::Series => series.insert(item.virtual_id.get(), item),
        }
        app_state
            .playlists
            .cache_playlist(
                &target.name,
                PlaylistStorage::XtreamPlaylist(Box::new(PlaylistXtreamStorage { live, vod, series })),
            )
            .await;
    } else {
        let storage_dir = app_state.app_config.config.load().storage_dir.clone();
        let input_storage = crate::repository::get_input_storage_path(&item.input_name, &storage_dir).await?;
        let path = crate::repository::get_input_m3u_playlist_file_path(&input_storage, &item.input_name);
        let group = shared::model::PlaylistGroup {
            id: 1,
            title: "recording".intern(),
            xtream_cluster: item.item_type.cluster(),
            channels: vec![PlaylistItem::from(&item)],
        };
        crate::repository::persist_input_m3u_playlist(&app_state.app_config, &path, &[group]).await?;
        cache_test_m3u_hls_item(app_state, target, item).await;
    }
    Ok(())
}
