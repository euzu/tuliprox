use crate::{
    frame::{FrameDecoder, FrameValidator},
    protocol::RunId,
    TestkitError,
};
use futures::StreamExt;
use std::collections::{BTreeMap, HashSet};
use url::Url;

const SEGMENT_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const MANIFEST_RELOAD_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
const MANIFEST_RELOAD_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

pub async fn read_segments(
    manifest_url: &str,
    expected_run_id: &RunId,
    expected_marker: u32,
    minimum_frames: u64,
    headers: &BTreeMap<String, String>,
) -> Result<u64, TestkitError> {
    let client = reqwest::Client::builder().http1_only().build()?;
    let base = Url::parse(manifest_url).map_err(|error| TestkitError::Configuration(error.to_string()))?;
    let mut frames = 0;
    let mut validator = FrameValidator::new(expected_run_id, expected_marker);
    // Live manifests are reloaded until enough frames were observed. Segment
    // identities are remembered across snapshots so a reload never replays a
    // segment that was already decoded.
    let mut consumed: HashSet<String> = HashSet::new();
    let deadline = tokio::time::Instant::now() + MANIFEST_RELOAD_DEADLINE;
    loop {
        let manifest = tokio::time::timeout_at(deadline, async {
            request_with_headers(client.get(manifest_url), headers).send().await?.error_for_status()?.text().await
        })
        .await
        .map_err(|_| TestkitError::Protocol("HLS manifest request exceeded overall deadline".to_owned()))??;
        let segments = manifest
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        for segment in segments {
            if !consumed.insert(segment.clone()) {
                continue;
            }
            let segment_url = base.join(&segment).map_err(|error| TestkitError::Configuration(error.to_string()))?;
            let response = tokio::time::timeout_at(deadline, async {
                request_with_headers(client.get(segment_url), headers).send().await?.error_for_status()
            })
            .await
            .map_err(|_| TestkitError::Protocol("HLS segment request exceeded overall deadline".to_owned()))??;
            let mut decoder = FrameDecoder::default();
            let mut body = response.bytes_stream();
            loop {
                let chunk =
                    match tokio::time::timeout_at(deadline, tokio::time::timeout(SEGMENT_IDLE_TIMEOUT, body.next()))
                        .await
                    {
                        Ok(Ok(Some(chunk))) => chunk?,
                        Ok(Ok(None)) => break,
                        Ok(Err(_)) => return Err(TestkitError::Protocol("HLS segment idle timeout".to_owned())),
                        Err(_) => return Err(TestkitError::Protocol("HLS read exceeded overall deadline".to_owned())),
                    };
                for frame in decoder.push(&chunk)? {
                    validator.validate(&frame)?;
                    frames += 1;
                }
            }
            if frames >= minimum_frames {
                return Ok(frames);
            }
        }
        if frames >= minimum_frames {
            return Ok(frames);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(TestkitError::Protocol(format!(
                "HLS produced {frames} valid frames, expected at least {minimum_frames}"
            )));
        }
        if tokio::time::timeout_at(deadline, tokio::time::sleep(MANIFEST_RELOAD_INTERVAL)).await.is_err() {
            return Err(TestkitError::Protocol(format!(
                "HLS produced {frames} valid frames, expected at least {minimum_frames}"
            )));
        }
    }
}

fn request_with_headers(
    mut request: reqwest::RequestBuilder,
    headers: &BTreeMap<String, String>,
) -> reqwest::RequestBuilder {
    for (name, value) in headers {
        request = request.header(name, value);
    }
    request
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{frame::Frame, protocol::OriginStreamId};
    use axum::{
        body::Body,
        extract::OriginalUri,
        http::{HeaderMap, StatusCode},
        response::{IntoResponse, Response},
        routing::get,
        Router,
    };
    use bytes::BytesMut;
    use std::sync::Arc;
    use tokio::{net::TcpListener, sync::Mutex};

    #[test]
    fn manifest_entries_exclude_directives() {
        let entries = "#EXTM3U\n#EXTINF:1,\n0.ts\n"
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect::<Vec<_>>();
        assert_eq!(entries, vec!["0.ts"]);
    }

    #[tokio::test]
    async fn forwards_actor_headers_to_manifest_and_segments() -> Result<(), TestkitError> {
        let run_id = RunId::new("hls-auth");
        let mut payload = BytesMut::new();
        Frame::synthetic(&run_id, &OriginStreamId::new("segment"), 17, 0).encode(&mut payload)?;
        let payload = Arc::new(payload.freeze());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new().fallback(get({
            let payload = Arc::clone(&payload);
            let requests = Arc::clone(&requests);
            move |headers: HeaderMap, uri: OriginalUri| {
                let payload = Arc::clone(&payload);
                let requests = Arc::clone(&requests);
                async move {
                    if headers.get("x-testkit-token").and_then(|value| value.to_str().ok()) != Some("accepted") {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    requests.lock().await.push(uri.path().to_owned());
                    match uri.path() {
                        "/index.m3u8" => "#EXTM3U\n#EXTINF:1,\nsegment.ts\n".into_response(),
                        "/segment.ts" => Response::new(Body::from(payload.as_ref().clone())),
                        _ => StatusCode::NOT_FOUND.into_response(),
                    }
                }
            }
        }));
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let headers = BTreeMap::from([(String::from("x-testkit-token"), String::from("accepted"))]);
        let received = read_segments(&format!("http://{address}/index.m3u8"), &run_id, 17, 1, &headers).await?;
        server.abort();

        assert_eq!(received, 1);
        assert_eq!(requests.lock().await.as_slice(), ["/index.m3u8", "/segment.ts"]);
        Ok(())
    }
}
