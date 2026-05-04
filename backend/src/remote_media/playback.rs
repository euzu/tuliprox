use crate::remote_media::{
    RemoteImageRef, RemoteMediaCatalogClient, RemoteMediaError, RemoteMediaErrorKind, RemoteResourceResponse, RemoteStreamRef,
};
use bytes::Bytes;
use reqwest::{
    header::{
        HeaderMap, ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, LAST_MODIFIED,
    },
    StatusCode,
};
use shared::model::{InputType, PlaylistItemType};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackOrigin {
    Provider,
    LocalLibrary,
    RemoteMedia(RemoteStreamRef),
}

#[derive(Debug, Clone)]
pub struct RemoteProxyResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

pub fn classify_playback_origin(
    input_type: InputType,
    item_type: PlaylistItemType,
    input_name: &Arc<str>,
    item_url: &str,
) -> Result<PlaybackOrigin, RemoteMediaError> {
    if input_type.is_remote_media_server() || item_url.starts_with("remote://") {
        return parse_remote_stream_ref(input_name, item_url).map(PlaybackOrigin::RemoteMedia);
    }

    if matches!(item_type, PlaylistItemType::LocalVideo | PlaylistItemType::LocalSeries) {
        return Ok(PlaybackOrigin::LocalLibrary);
    }

    Ok(PlaybackOrigin::Provider)
}

pub async fn remote_media_stream_response<C>(
    client: &C,
    stream_ref: &RemoteStreamRef,
    range: Option<&str>,
) -> Result<RemoteProxyResponse, RemoteMediaError>
where
    C: RemoteMediaCatalogClient,
{
    let response = client.open_stream(stream_ref, range).await?;
    Ok(remote_resource_to_proxy_response(response))
}

pub async fn remote_media_image_response<C>(
    client: &C,
    image_ref: &RemoteImageRef,
) -> Result<RemoteProxyResponse, RemoteMediaError>
where
    C: RemoteMediaCatalogClient,
{
    let response = client.open_image(image_ref).await?;
    Ok(remote_resource_to_proxy_response(response))
}

fn remote_resource_to_proxy_response(response: RemoteResourceResponse) -> RemoteProxyResponse {
    RemoteProxyResponse {
        status: response.status,
        headers: safe_remote_response_headers(&response.headers),
        body: response.body,
    }
}

pub fn safe_remote_response_headers(headers: &HeaderMap) -> HeaderMap {
    let mut safe = HeaderMap::new();
    for name in [CONTENT_TYPE, CONTENT_LENGTH, CONTENT_RANGE, ACCEPT_RANGES, ETAG, LAST_MODIFIED] {
        if let Some(value) = headers.get(&name) {
            safe.insert(name, value.clone());
        }
    }
    safe
}

pub fn parse_remote_stream_ref(input_name: &Arc<str>, item_url: &str) -> Result<RemoteStreamRef, RemoteMediaError> {
    let Some(rest) = item_url.strip_prefix("remote://") else {
        return Err(RemoteMediaError::new(RemoteMediaErrorKind::RemoteStreamOpenFailed)
            .detail("playlist item is not a remote media URL"));
    };
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() < 3 {
        return Err(RemoteMediaError::new(RemoteMediaErrorKind::RemoteStreamOpenFailed)
            .detail("remote media URL is missing required path parts"));
    }

    match parts[0] {
        "emby" => Ok(RemoteStreamRef::Emby {
            input_name: input_name.clone(),
            server_id: parts[1].into(),
            item_id: parts[2].into(),
            media_source_id: query_value(query, "media_source_id").map(|v| unescape_internal_url_component(&v).into()),
        }),
        "jellyfin" => Ok(RemoteStreamRef::Jellyfin {
            input_name: input_name.clone(),
            server_id: parts[1].into(),
            item_id: parts[2].into(),
            media_source_id: query_value(query, "media_source_id").map(|v| unescape_internal_url_component(&v).into()),
        }),
        "plex" => Ok(RemoteStreamRef::Plex {
            input_name: input_name.clone(),
            server_id: parts[1].into(),
            rating_key: parts[2].into(),
            part_key: query_value(query, "part_key")
                .map(|v| unescape_internal_url_component(&v).into())
                .ok_or_else(|| {
                    RemoteMediaError::new(RemoteMediaErrorKind::NoDirectPlayableRemoteSource)
                        .detail("plex remote URL is missing part_key")
                })?,
        }),
        _ => Err(RemoteMediaError::new(RemoteMediaErrorKind::RemoteStreamOpenFailed)
            .detail("unsupported remote media URL scheme")),
    }
}

fn query_value(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then(|| value.to_string())
    })
}

fn unescape_internal_url_component(value: &str) -> String {
    let mut result = value.to_string();
    for (encoded, decoded) in [
        ("%2F", "/"),
        ("%3F", "?"),
        ("%26", "&"),
        ("%3D", "="),
        ("%23", "#"),
        ("%25", "%"),
    ] {
        result = result.replace(encoded, decoded).replace(&encoded.to_ascii_lowercase(), decoded);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_media::{
        RemoteEpisode, RemoteLibrary, RemoteLibraryRef, RemoteMovie, RemotePage, RemotePageRequest,
        RemoteServerStatus,
    };
    use reqwest::header::{HeaderValue, AUTHORIZATION};
    use std::sync::{Mutex, Arc as StdArc};

    #[derive(Default)]
    struct MockPlaybackClient {
        seen_range: Mutex<Option<String>>,
        stream_error: Option<RemoteMediaError>,
    }

    impl RemoteMediaCatalogClient for MockPlaybackClient {
        async fn discover(&self) -> Result<RemoteServerStatus, RemoteMediaError> { unreachable!() }
        async fn list_libraries(&self) -> Result<Vec<RemoteLibrary>, RemoteMediaError> { unreachable!() }
        async fn list_movies(
            &self,
            _library: &RemoteLibraryRef,
            _page: RemotePageRequest,
        ) -> Result<RemotePage<RemoteMovie>, RemoteMediaError> {
            unreachable!()
        }
        async fn list_episodes(
            &self,
            _library: &RemoteLibraryRef,
            _page: RemotePageRequest,
        ) -> Result<RemotePage<RemoteEpisode>, RemoteMediaError> {
            unreachable!()
        }

        async fn open_stream(
            &self,
            _stream_ref: &RemoteStreamRef,
            range: Option<&str>,
        ) -> Result<crate::remote_media::RemoteStreamResponse, RemoteMediaError> {
            *self.seen_range.lock().expect("lock") = range.map(ToOwned::to_owned);
            if let Some(error) = self.stream_error.clone() {
                return Err(error);
            }
            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("video/mp4"));
            headers.insert(CONTENT_RANGE, HeaderValue::from_static("bytes 0-1023/2048"));
            headers.insert(CONTENT_LENGTH, HeaderValue::from_static("1024"));
            headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer should-not-leak"));
            Ok(RemoteResourceResponse { status: StatusCode::PARTIAL_CONTENT, headers, body: Bytes::from_static(b"data") })
        }

        async fn open_image(&self, _image_ref: &RemoteImageRef) -> Result<RemoteResourceResponse, RemoteMediaError> {
            Ok(RemoteResourceResponse { status: StatusCode::OK, headers: HeaderMap::new(), body: Bytes::new() })
        }
    }

    #[tokio::test]
    async fn remote_stream_response_forwards_range_and_filters_headers() {
        let client = MockPlaybackClient::default();
        let stream_ref = RemoteStreamRef::Plex {
            input_name: "remote".into(),
            server_id: "server".into(),
            rating_key: "rating".into(),
            part_key: "/library/parts/redacted/file.mkv".into(),
        };

        let response = remote_media_stream_response(&client, &stream_ref, Some("bytes=0-1023"))
            .await
            .expect("stream opens");

        assert_eq!(client.seen_range.lock().expect("lock").as_deref(), Some("bytes=0-1023"));
        assert_eq!(response.status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers.get(CONTENT_RANGE).and_then(|v| v.to_str().ok()), Some("bytes 0-1023/2048"));
        assert!(response.headers.get(AUTHORIZATION).is_none());
    }

    #[test]
    fn classifies_remote_local_and_provider_origins() {
        let input_name = StdArc::<str>::from("remote");
        let remote = classify_playback_origin(
            InputType::Plex,
            PlaylistItemType::Video,
            &input_name,
            "remote://plex/server/rating?part_key=%2Flibrary%2Fparts%2Fredacted%2Ffile.mkv",
        )
        .expect("remote parses");
        assert!(matches!(remote, PlaybackOrigin::RemoteMedia(RemoteStreamRef::Plex { .. })));

        let local = classify_playback_origin(InputType::Library, PlaylistItemType::LocalVideo, &input_name, "file:///tmp/a.mkv")
            .expect("local classifies");
        assert_eq!(local, PlaybackOrigin::LocalLibrary);

        let provider = classify_playback_origin(InputType::M3u, PlaylistItemType::Live, &input_name, "http://example.invalid/live")
            .expect("provider classifies");
        assert_eq!(provider, PlaybackOrigin::Provider);
    }

    #[tokio::test]
    async fn remote_auth_denied_stays_remote_error() {
        let client = MockPlaybackClient {
            stream_error: Some(RemoteMediaError::new(RemoteMediaErrorKind::RemoteAuthDenied)),
            ..MockPlaybackClient::default()
        };
        let stream_ref = RemoteStreamRef::Emby {
            input_name: "remote".into(),
            server_id: "server".into(),
            item_id: "item".into(),
            media_source_id: None,
        };

        let error = remote_media_stream_response(&client, &stream_ref, None)
            .await
            .expect_err("auth denied should fail");

        assert_eq!(error.kind, RemoteMediaErrorKind::RemoteAuthDenied);
    }
}
