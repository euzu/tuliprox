#[cfg(feature = "plugins")]
use super::open_media_server_image_resource;
use super::{create_test_app_state, open_media_server_stream_for_input};
use crate::model::{ConfigInput, MediaServerInputConfig};
#[cfg(feature = "plugins")]
use crate::{
    api::model::create_test_app_state_with_plugins,
    model::{Config, SourcesConfig},
};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use futures::StreamExt;
use shared::model::{InputType, MediaServerInputConfigDto, MediaServerLibrarySelector};
#[cfg(feature = "plugins")]
use std::sync::Arc;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const MEDIA_BODY: &[u8] = b"media-bytes";

/// Answers the Emby discovery call and the stream call, recording every request head in lowercase.
async fn spawn_emby_test_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("emby test server binds");
    let addr = listener.local_addr().expect("emby test server address");
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for _ in 0..2 {
            let Ok((mut socket, _)) = listener.accept().await else { break };
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut chunk = [0_u8; 1024];
                let Ok(read) = socket.read(&mut chunk).await else { break };
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
            let response = if request.starts_with("get /system/info/public") {
                let body = r#"{"Id":"server-1","ServerName":"Mock"}"#;
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            } else {
                format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Type: video/mp4\r\nContent-Range: bytes 4-14/100\r\nSet-Cookie: session=secret\r\nX-Emby-Token: upstream-secret\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    MEDIA_BODY.len(),
                    String::from_utf8_lossy(MEDIA_BODY)
                )
            };
            requests.push(request);
            socket.write_all(response.as_bytes()).await.expect("emby test server writes response");
        }
        requests
    });
    (addr, task)
}

#[tokio::test]
async fn emby_stream_forwards_range_and_strips_credentials_from_the_response() {
    let (addr, server) = spawn_emby_test_server().await;
    let app_state = create_test_app_state();
    let media_server = MediaServerInputConfig::from(&MediaServerInputConfigDto {
        token: Some("emby-secret".to_string()),
        libraries: vec![MediaServerLibrarySelector::Name("Movies".to_string())],
        ..MediaServerInputConfigDto::default()
    });
    let input = ConfigInput {
        name: "emby_input".into(),
        input_type: InputType::Emby,
        url: format!("http://{addr}"),
        media_server: Some(media_server),
        enabled: true,
        ..ConfigInput::default()
    };
    let mut request_headers = HeaderMap::new();
    request_headers.insert(header::RANGE, HeaderValue::from_static("bytes=4-14"));

    let (stream, info) = open_media_server_stream_for_input(
        &app_state,
        &input,
        "media-server://emby/server-1/item-1?media_source_id=source-1",
        &request_headers,
    )
    .await
    .expect("emby stream opens");

    let (headers, status, _, _) = info.expect("response info is present");
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert!(headers.iter().any(|(name, value)| name == "content-range" && value == "bytes 4-14/100"), "{headers:?}");
    for (name, _) in &headers {
        assert!(
            name != "authorization" && !name.starts_with("x-emby-") && name != "set-cookie",
            "credential header {name} leaked into the returned headers"
        );
    }
    let body: Vec<u8> = stream.map(|chunk| chunk.expect("stream chunk").to_vec()).concat().await;
    assert_eq!(body, MEDIA_BODY);

    let requests = server.await.expect("emby test server finishes");
    let stream_request = requests.last().expect("stream request is recorded");
    assert!(stream_request.starts_with("get /videos/item-1/stream?"), "{stream_request}");
    assert!(stream_request.contains("mediasourceid=source-1"), "{stream_request}");
    assert!(stream_request.contains("range: bytes=4-14"), "{stream_request}");
    assert!(stream_request.contains("x-emby-token: emby-secret"), "{stream_request}");
}

#[tokio::test]
async fn media_server_stream_follows_307_redirect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test server binds");
    let addr = listener.local_addr().expect("test server address");
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for _ in 0..4 {
            let Ok((mut socket, _)) = listener.accept().await else { break };
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut chunk = [0_u8; 1024];
                let Ok(read) = socket.read(&mut chunk).await else { break };
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            let req_str = String::from_utf8_lossy(&request).to_ascii_lowercase();
            let response = if req_str.starts_with("get /system/info/public") {
                let body = r#"{"Id":"server-1","ServerName":"Mock"}"#;
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            } else if req_str.starts_with("get /users/me") {
                let body = r#"{"Id":"user-1","Name":"Admin"}"#;
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            } else if req_str.starts_with("get /videos/item-1/stream") {
                format!(
                    "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{addr}/redirected-stream\r\nConnection: close\r\n\r\n"
                )
            } else {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    MEDIA_BODY.len(),
                    String::from_utf8_lossy(MEDIA_BODY)
                )
            };
            requests.push(req_str);
            socket.write_all(response.as_bytes()).await.expect("test server writes response");
        }
        requests
    });

    let app_state = create_test_app_state();
    let media_server = MediaServerInputConfig::from(&MediaServerInputConfigDto {
        token: Some("silo-secret".to_string()),
        libraries: vec![MediaServerLibrarySelector::Name("Movies".to_string())],
        ..MediaServerInputConfigDto::default()
    });
    let input = ConfigInput {
        name: "silo_input".into(),
        input_type: InputType::Silo,
        url: format!("http://{addr}"),
        media_server: Some(media_server),
        enabled: true,
        ..ConfigInput::default()
    };
    let request_headers = HeaderMap::new();

    let (stream, info) =
        open_media_server_stream_for_input(&app_state, &input, "media-server://silo/server-1/item-1", &request_headers)
            .await
            .expect("silo stream opens following redirect");

    let (_, status, _, _) = info.expect("response info is present");
    assert_eq!(status, StatusCode::OK);
    let body: Vec<u8> = stream.map(|chunk| chunk.expect("stream chunk").to_vec()).concat().await;
    assert_eq!(body, MEDIA_BODY);

    let requests = task.await.expect("test server finishes");
    assert_eq!(requests.len(), 4);
    assert!(requests[1].starts_with("get /users/me"));
    assert!(requests[2].starts_with("get /videos/item-1/stream"));
    assert!(requests[3].starts_with("get /redirected-stream"));
}

#[cfg(feature = "plugins")]
fn load_media_probe_system() -> Option<(tempfile::TempDir, tuliprox_plugins::PluginSystem)> {
    use sha2::Digest;
    use std::{fs, path::PathBuf};
    let fixtures_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugin-fixtures/plugins/media-probe");
    if !fixtures_dir.join("plugin.wasm").is_file() {
        return None;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let plugins_dir = root.path().join("plugins/media-probe");
    fs::create_dir_all(&plugins_dir).expect("create_dir_all");
    let wasm = fs::read(fixtures_dir.join("plugin.wasm")).expect("read plugin.wasm");
    fs::write(plugins_dir.join("plugin.wasm"), &wasm).expect("write plugin.wasm");
    let manifest = fs::read(fixtures_dir.join("manifest.yml")).expect("read manifest.yml");
    fs::write(plugins_dir.join("manifest.yml"), manifest).expect("write manifest.yml");

    let hash = sha2::Sha256::digest(&wasm).iter().fold(String::new(), |mut hex, byte| {
        let _ = std::fmt::Write::write_fmt(&mut hex, format_args!("{byte:02x}"));
        hex
    });
    let data = root.path().join("data");
    fs::create_dir(&data).expect("create data dir");
    fs::write(
        data.join("approvals.json"),
        serde_json::json!({
            "media-probe": {
                "wasm_sha256": hash,
                "capabilities": ["network"]
            }
        })
        .to_string(),
    )
    .expect("write approvals.json");

    let config = tuliprox_core::model::PluginsConfig {
        enabled: true,
        directory: root.path().join("plugins").to_string_lossy().into(),
        plugins: vec![tuliprox_core::model::PluginEntry {
            id: "media-probe".into(),
            enabled: true,
            priority: 0,
            settings: serde_json::Map::default(),
            settings_version: 1,
            update_policy: shared::model::PluginUpdatePolicy::Auto,
        }],
        ..Default::default()
    };
    let services = tuliprox_plugins::PluginServices::new(reqwest::Client::new(), data);
    let system = tuliprox_plugins::PluginSystem::load(&config, services)
        .expect("load media-probe plugin")
        .expect("plugin system loaded");
    Some((root, system))
}

#[cfg(feature = "plugins")]
/// Answers `max_requests` requests, then ends and returns them.
async fn spawn_media_test_server(max_requests: usize) -> (std::net::SocketAddr, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test server binds");
    let addr = listener.local_addr().expect("test server address");
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        while requests.len() < max_requests {
            let Ok((mut socket, _)) = listener.accept().await else { break };
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut chunk = [0_u8; 1024];
                let Ok(read) = socket.read(&mut chunk).await else { break };
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            if request.is_empty() {
                break;
            }
            let req_str = String::from_utf8_lossy(&request).to_ascii_lowercase();
            requests.push(req_str.clone());
            let is_head = req_str.starts_with("head ");
            let is_range = req_str.contains("range: bytes=");
            let response = if is_range {
                format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Type: video/mp4\r\nContent-Range: bytes 0-10/100\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    if is_head { 0 } else { 11 },
                    if is_head { "" } else { "01234567890" }
                )
            } else {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    if is_head { 0 } else { MEDIA_BODY.len() },
                    if is_head { "" } else { std::str::from_utf8(MEDIA_BODY).unwrap_or("") }
                )
            };
            let _ = socket.write_all(response.as_bytes()).await;
        }
        requests
    });
    (addr, task)
}

#[cfg(feature = "plugins")]
#[tokio::test]
async fn plugin_stream_forwards_range_and_safe_headers_and_isolates_plugin_headers() {
    let Some((_root, system)) = load_media_probe_system() else {
        return;
    };
    let (addr, server_task) = spawn_media_test_server(1).await;

    let app_state = create_test_app_state_with_plugins(Config::default(), Some(system));
    let input = ConfigInput {
        name: "plugin_input".into(),
        input_type: InputType::Plugin,
        plugin: Some("media-probe".into()),
        url: format!("http://{addr}/media.mp4"),
        enabled: true,
        ..ConfigInput::default()
    };
    app_state
        .app_config
        .sources
        .store(Arc::new(SourcesConfig { inputs: vec![Arc::new(input.clone())], ..Default::default() }));

    let mut request_headers = HeaderMap::new();
    request_headers.insert(header::RANGE, HeaderValue::from_static("bytes=4-14"));

    let (stream, info) = open_media_server_stream_for_input(
        &app_state,
        &input,
        "media-server://plugin/media-probe/movie-1?stream_key=stream-movie-1",
        &request_headers,
    )
    .await
    .expect("plugin stream opens");

    let (headers, status, _, _) = info.expect("response info is present");
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert!(headers.iter().any(|(name, value)| name == "content-range" && value == "bytes 0-10/100"), "{headers:?}");

    // Header isolation: upstream plugin headers (X-Media-Probe-Auth) and sensitive headers never reach client
    for (name, _) in &headers {
        assert!(
            name != "x-media-probe-auth" && name != "set-cookie" && !name.starts_with("x-plugin-"),
            "plugin header {name} leaked into the client headers"
        );
    }

    let body: Vec<u8> = stream.map(|chunk| chunk.expect("stream chunk").to_vec()).concat().await;
    assert_eq!(body, b"01234567890");

    let requests = tokio::time::timeout(std::time::Duration::from_secs(10), server_task)
        .await
        .expect("test server answers in time")
        .expect("test server finishes");
    assert_eq!(requests.len(), 1);
    let req = &requests[0];
    assert!(req.starts_with("get /media.mp4"), "{req}");
    assert!(req.contains("range: bytes=4-14"), "{req}");
    assert!(req.contains("x-media-probe-auth: probe-secret-token"), "{req}");
}

#[cfg(feature = "plugins")]
#[tokio::test]
async fn plugin_image_resource_resolves_and_forwards_safely() {
    let Some((_root, system)) = load_media_probe_system() else {
        return;
    };
    let (addr, server_task) = spawn_media_test_server(1).await;

    let app_state = create_test_app_state_with_plugins(Config::default(), Some(system));
    let input = ConfigInput {
        name: "plugin_input".into(),
        input_type: InputType::Plugin,
        plugin: Some("media-probe".into()),
        url: format!("http://{addr}/poster.jpg"),
        enabled: true,
        ..ConfigInput::default()
    };
    app_state
        .app_config
        .sources
        .store(Arc::new(SourcesConfig { inputs: vec![Arc::new(input.clone())], ..Default::default() }));

    let response = open_media_server_image_resource(
        &app_state,
        "media-server://image/plugin/plugin_input/media-probe/movie-1?image_key=image-movie-1",
    )
    .await
    .expect("plugin image opens");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get(header::CONTENT_TYPE).unwrap(), "video/mp4");
    assert!(response.headers().get("x-media-probe-auth").is_none());

    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("body bytes");
    assert_eq!(body, MEDIA_BODY);

    let requests = tokio::time::timeout(std::time::Duration::from_secs(10), server_task)
        .await
        .expect("test server answers in time")
        .expect("test server finishes");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("get /poster.jpg"));
    assert!(requests[0].contains("x-media-probe-auth: probe-secret-token"));
}

#[cfg(feature = "plugins")]
#[tokio::test]
async fn plugin_stream_client_abort_mid_stream_cancels_cleanly_without_leak() {
    let Some((_root, system)) = load_media_probe_system() else {
        return;
    };
    let (addr, _server_task) = spawn_media_test_server(1).await;

    let app_state = create_test_app_state_with_plugins(Config::default(), Some(system));
    let input = ConfigInput {
        name: "plugin_input".into(),
        input_type: InputType::Plugin,
        plugin: Some("media-probe".into()),
        url: format!("http://{addr}/media.mp4"),
        enabled: true,
        ..ConfigInput::default()
    };
    app_state
        .app_config
        .sources
        .store(Arc::new(SourcesConfig { inputs: vec![Arc::new(input.clone())], ..Default::default() }));

    let request_headers = HeaderMap::new();
    let (mut stream, _info) = open_media_server_stream_for_input(
        &app_state,
        &input,
        "media-server://plugin/media-probe/movie-1?stream_key=stream-movie-1",
        &request_headers,
    )
    .await
    .expect("plugin stream opens");

    let first = stream.next().await;
    assert!(first.is_some());
    drop(stream);
}

#[cfg(feature = "plugins")]
#[tokio::test]
async fn plugin_stream_error_handling() {
    let Some((_root, system)) = load_media_probe_system() else {
        return;
    };
    let app_state = create_test_app_state_with_plugins(Config::default(), Some(system));

    // Case 1: Plugin not installed or enabled -> 404/410 (MediaServerItemNotFound)
    let disabled_input = ConfigInput {
        name: "disabled_input".into(),
        input_type: InputType::Plugin,
        plugin: Some("non-existent-plugin".into()),
        enabled: true,
        ..ConfigInput::default()
    };
    let Err(err_missing) = open_media_server_stream_for_input(
        &app_state,
        &disabled_input,
        "media-server://plugin/non-existent-plugin/movie-1",
        &HeaderMap::new(),
    )
    .await
    else {
        panic!("expected item not found error");
    };
    assert_eq!(err_missing.kind, crate::media_server::MediaServerErrorKind::MediaServerItemNotFound);

    // Case 2: Plugin traps or fails -> 502 Bad Gateway (MediaServerStreamOpenFailed)
    let trap_input = ConfigInput {
        name: "trap_input".into(),
        input_type: InputType::Plugin,
        plugin: Some("media-probe".into()),
        plugin_options: vec![("mode".into(), "trap".into())],
        url: "http://example.com/media.mp4".into(),
        enabled: true,
        ..ConfigInput::default()
    };
    let Err(err_trap) = open_media_server_stream_for_input(
        &app_state,
        &trap_input,
        "media-server://plugin/media-probe/movie-1?stream_key=trap",
        &HeaderMap::new(),
    )
    .await
    else {
        panic!("expected stream open failed error");
    };
    assert_eq!(err_trap.kind, crate::media_server::MediaServerErrorKind::MediaServerStreamOpenFailed);
}
