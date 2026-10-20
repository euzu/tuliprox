use super::{create_test_app_state, open_media_server_stream_for_input};
use crate::model::{ConfigInput, MediaServerInputConfig};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use futures::StreamExt;
use shared::model::{InputType, MediaServerInputConfigDto, MediaServerLibrarySelector};
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
