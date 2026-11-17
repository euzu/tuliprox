use crate::{
    api::{
        endpoints::v1_api_playlist::{
            imdb_update_result_to_response,
            routes::{v1_api_playlist_register_protected, v1_api_playlist_register_with_permissions},
        },
        model::create_test_app_state,
    },
    auth::create_jwt_web_user,
    processing::imdb::ImdbUpdateError,
};
use async_compression::tokio::write::GzipEncoder;
use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use shared::model::{ImdbUpdateResponseDto, Permission, WebAuthConfigDto, WebUiConfigDto};
use std::sync::Arc;
use tokio::{io::AsyncWriteExt, net::TcpListener};
use tower::ServiceExt;
use tuliprox_core::model::{Config, ImdbConfig, MetadataUpdateConfig};
use tuliprox_repository::get_imdb_ratings_path;

async fn compress_to_gzip(content: &str) -> Vec<u8> {
    let mut encoder = GzipEncoder::new(Vec::new());
    encoder.write_all(content.as_bytes()).await.expect("compress gzip");
    encoder.shutdown().await.expect("shutdown gzip");
    encoder.into_inner()
}

async fn start_test_server(body: Vec<u8>, status_code: u16) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind test server");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;

            let response_headers = format!(
                "HTTP/1.1 {} OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
                status_code,
                body.len()
            );
            let _ = socket.write_all(response_headers.as_bytes()).await;
            let _ = socket.write_all(&body).await;
        }
    });
    (format!("http://{addr}"), handle)
}

fn make_config(storage_dir: &str, url: &str, enabled: bool) -> Config {
    let mut config = Config { storage_dir: storage_dir.to_string(), ..Default::default() };
    config.metadata_update = Some(MetadataUpdateConfig {
        imdb: ImdbConfig { enabled, url: url.to_string(), ..Default::default() },
        ..Default::default()
    });
    config
}

#[tokio::test]
async fn test_imdb_update_result_to_response_mapping() {
    // Ok -> 200 with JSON { "rows": 42 }
    let resp = imdb_update_result_to_response(Ok(42));
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.expect("bytes");
    let dto: ImdbUpdateResponseDto = serde_json::from_slice(&bytes).expect("deserialize");
    assert_eq!(dto.rows, 42);

    // Disabled -> 400
    let resp = imdb_update_result_to_response(Err(ImdbUpdateError::Disabled));
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // AlreadyRunning -> 409
    let resp = imdb_update_result_to_response(Err(ImdbUpdateError::AlreadyRunning));
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // DownloadFailed -> 502
    let resp = imdb_update_result_to_response(Err(ImdbUpdateError::DownloadFailed("network error".to_string())));
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

    // ProcessFailed -> 500
    let resp = imdb_update_result_to_response(Err(ImdbUpdateError::ProcessFailed("disk error".to_string())));
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);

    // Rejected -> 500
    let resp = imdb_update_result_to_response(Err(ImdbUpdateError::Rejected("too few rows".to_string())));
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn test_imdb_update_disabled_returns_400() {
    let temp = tempfile::tempdir().expect("tempdir");
    let storage_dir = temp.path().to_str().expect("path");
    let config = make_config(storage_dir, "", false);
    let app_state = create_test_app_state(config);
    let router = v1_api_playlist_register_protected(Router::new()).with_state(app_state);

    let req = Request::builder().uri("/imdb/update").method("GET").body(Body::empty()).expect("request");
    let response = router.oneshot(req).await.expect("response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_imdb_update_already_running_returns_409() {
    let temp = tempfile::tempdir().expect("tempdir");
    let storage_dir = temp.path().to_str().expect("path");
    let config = make_config(storage_dir, "https://example.com/ratings.tsv.gz", true);
    let app_state = create_test_app_state(config);
    let path = get_imdb_ratings_path(storage_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create storage dir");
    }

    // Hold write lock before invoking update
    let _lock = app_state.app_config.file_locks.write_lock(&path).await;

    let router = v1_api_playlist_register_protected(Router::new()).with_state(app_state);
    let req = Request::builder().uri("/imdb/update").method("GET").body(Body::empty()).expect("request");
    let response = router.oneshot(req).await.expect("response");
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn test_imdb_update_success_returns_200_and_row_count() {
    let tsv_content = "\
tconst\taverageRating\tnumVotes
tt0000001\t5.7\t2112
tt0000002\t8.1\t1500
tt0000003\t7.0\t3000
";
    let gzip_body = compress_to_gzip(tsv_content).await;
    let (server_url, _handle) = start_test_server(gzip_body, 200).await;

    let temp = tempfile::tempdir().expect("tempdir");
    let storage_dir = temp.path().to_str().expect("path");
    let config = make_config(storage_dir, &format!("{server_url}/ratings.tsv.gz"), true);
    let app_state = create_test_app_state(config);

    let router = v1_api_playlist_register_protected(Router::new()).with_state(app_state);
    let req = Request::builder().uri("/imdb/update").method("GET").body(Body::empty()).expect("request");
    let response = router.oneshot(req).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("bytes");
    let dto: ImdbUpdateResponseDto = serde_json::from_slice(&bytes).expect("deserialize");
    assert_eq!(dto.rows, 3);
}

#[tokio::test]
async fn test_imdb_update_permissions() {
    let temp = tempfile::tempdir().expect("tempdir");
    let storage_dir = temp.path().to_str().expect("path");
    let mut config = make_config(storage_dir, "", false);
    let web_ui = WebUiConfigDto {
        auth: Some(WebAuthConfigDto {
            enabled: true,
            issuer: "test".to_string(),
            secret: "test-secret-must-be-long-enough-32-bytes!!".to_string(),
            ..WebAuthConfigDto::default()
        }),
        ..WebUiConfigDto::default()
    };
    config.web_ui = Some((&web_ui).into());

    let web_auth = config.web_ui.as_ref().unwrap().auth.as_ref().unwrap();
    let reader_token = create_jwt_web_user(
        web_auth,
        "reader",
        Permission::PlaylistRead.into(),
        0,
        shared::model::UserId::from("web:reader"),
    )
    .expect("token");
    let writer_token = create_jwt_web_user(
        web_auth,
        "writer",
        Permission::PlaylistWrite.into(),
        0,
        shared::model::UserId::from("web:writer"),
    )
    .expect("token");

    let app_state = create_test_app_state(config);
    let router =
        v1_api_playlist_register_with_permissions(Router::new(), &app_state).with_state(Arc::clone(&app_state));

    // No auth -> 401
    let req = Request::builder().uri("/imdb/update").method("GET").body(Body::empty()).expect("request");
    let response = router.clone().oneshot(req).await.expect("response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // PlaylistRead -> 403
    let req = Request::builder()
        .uri("/imdb/update")
        .method("GET")
        .header("authorization", format!("Bearer {reader_token}"))
        .body(Body::empty())
        .expect("request");
    let response = router.clone().oneshot(req).await.expect("response");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // PlaylistWrite -> reaches handler (IMDb disabled -> 400)
    let req = Request::builder()
        .uri("/imdb/update")
        .method("GET")
        .header("authorization", format!("Bearer {writer_token}"))
        .body(Body::empty())
        .expect("request");
    let response = router.clone().oneshot(req).await.expect("response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
