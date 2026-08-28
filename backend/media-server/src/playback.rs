use crate::{
    BoxedMediaServerStream, MediaServerCatalogClient, MediaServerError, MediaServerErrorKind, MediaServerImageRef,
    MediaServerResourceResponse, MediaServerStreamRef, MediaServerStreamResponse,
};
use bytes::Bytes;
use futures::{stream, StreamExt};
use reqwest::{
    header::{HeaderMap, ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, LAST_MODIFIED},
    StatusCode,
};
use shared::model::{InputType, PlaylistItemType};
use std::{fmt, sync::Arc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackOrigin {
    Provider,
    LocalLibrary,
    MediaServer(MediaServerStreamRef),
}

pub struct MediaServerProxyResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: BoxedMediaServerStream,
}

impl fmt::Debug for MediaServerProxyResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MediaServerProxyResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("body", &"<stream>")
            .finish()
    }
}

pub fn classify_playback_origin(
    input_type: InputType,
    item_type: PlaylistItemType,
    input_name: &Arc<str>,
    item_url: &str,
) -> Result<PlaybackOrigin, MediaServerError> {
    if input_type.is_media_server() || item_url.starts_with("media-server://") {
        return parse_media_server_stream_ref(input_name, item_url).map(PlaybackOrigin::MediaServer);
    }

    if matches!(item_type, PlaylistItemType::LocalVideo | PlaylistItemType::LocalSeries) {
        return Ok(PlaybackOrigin::LocalLibrary);
    }

    Ok(PlaybackOrigin::Provider)
}

pub async fn media_server_stream_response<C>(
    client: &C,
    stream_ref: &MediaServerStreamRef,
    range: Option<&str>,
) -> Result<MediaServerProxyResponse, MediaServerError>
where
    C: MediaServerCatalogClient,
{
    let response = client.open_stream(stream_ref, range).await?;
    Ok(media_server_stream_to_proxy_response(response))
}

pub async fn media_server_image_response<C>(
    client: &C,
    image_ref: &MediaServerImageRef,
) -> Result<MediaServerProxyResponse, MediaServerError>
where
    C: MediaServerCatalogClient,
{
    let response = client.open_image(image_ref).await?;
    Ok(media_server_resource_to_proxy_response(response))
}

fn media_server_resource_to_proxy_response(response: MediaServerResourceResponse) -> MediaServerProxyResponse {
    MediaServerProxyResponse {
        status: response.status,
        headers: safe_media_server_response_headers(&response.headers),
        body: single_chunk_stream(response.body),
    }
}

fn media_server_stream_to_proxy_response(response: MediaServerStreamResponse) -> MediaServerProxyResponse {
    MediaServerProxyResponse {
        status: response.status,
        headers: safe_media_server_response_headers(&response.headers),
        body: response.body,
    }
}

fn single_chunk_stream(body: Bytes) -> BoxedMediaServerStream {
    stream::once(async move { Ok::<Bytes, MediaServerError>(body) }).boxed()
}

pub fn safe_media_server_response_headers(headers: &HeaderMap) -> HeaderMap {
    let mut safe = HeaderMap::new();
    for name in [CONTENT_TYPE, CONTENT_LENGTH, CONTENT_RANGE, ACCEPT_RANGES, ETAG, LAST_MODIFIED] {
        if let Some(value) = headers.get(&name) {
            safe.insert(name, value.clone());
        }
    }
    safe
}

pub fn parse_media_server_stream_ref(
    input_name: &Arc<str>,
    item_url: &str,
) -> Result<MediaServerStreamRef, MediaServerError> {
    let Some(rest) = item_url.strip_prefix("media-server://") else {
        return Err(MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
            .detail("playlist item is not a media server URL"));
    };
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let parts: Vec<String> = path.split('/').map(unescape_internal_url_component).collect();
    if parts.len() < 3 {
        return Err(MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
            .detail("media server URL is missing required path parts"));
    }

    match parts[0].as_str() {
        "unavailable" => Err(MediaServerError::new(MediaServerErrorKind::NoDirectPlayableMediaServerSource)
            .detail("media server item has no direct playable source")),
        "emby" => Ok(MediaServerStreamRef::Emby {
            input_name: input_name.clone(),
            server_id: Arc::<str>::from(parts[1].as_str()),
            item_id: Arc::<str>::from(parts[2].as_str()),
            media_source_id: query_value(query, "media_source_id").map(Arc::<str>::from),
        }),
        "jellyfin" => Ok(MediaServerStreamRef::Jellyfin {
            input_name: input_name.clone(),
            server_id: Arc::<str>::from(parts[1].as_str()),
            item_id: Arc::<str>::from(parts[2].as_str()),
            media_source_id: query_value(query, "media_source_id").map(Arc::<str>::from),
        }),
        "plex" => Ok(MediaServerStreamRef::Plex {
            input_name: input_name.clone(),
            server_id: Arc::<str>::from(parts[1].as_str()),
            rating_key: Arc::<str>::from(parts[2].as_str()),
            part_key: query_value(query, "part_key").map(Arc::<str>::from).ok_or_else(|| {
                MediaServerError::new(MediaServerErrorKind::NoDirectPlayableMediaServerSource)
                    .detail("plex media-server URL is missing part_key")
            })?,
        }),
        _ => Err(MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
            .detail("unsupported media server URL scheme")),
    }
}

pub fn parse_media_server_image_ref(resource_url: &str) -> Result<MediaServerImageRef, MediaServerError> {
    let Some(rest) = resource_url.strip_prefix("media-server://image/") else {
        return Err(MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
            .detail("resource URL is not a media server image URL"));
    };
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let parts: Vec<String> = path.split('/').map(unescape_internal_url_component).collect();
    if parts.len() < 4 {
        return Err(MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
            .detail("media server image URL is missing required path parts"));
    }

    match parts[0].as_str() {
        "emby" => Ok(MediaServerImageRef::Emby {
            input_name: Arc::<str>::from(parts[1].as_str()),
            server_id: Arc::<str>::from(parts[2].as_str()),
            item_id: Arc::<str>::from(parts[3].as_str()),
            image_kind: query_value(query, "image_kind").map(Arc::<str>::from).ok_or_else(|| {
                MediaServerError::new(MediaServerErrorKind::MediaServerItemNotFound)
                    .detail("emby media-server image URL is missing image_kind")
            })?,
            tag: query_value(query, "tag").map(Arc::<str>::from),
        }),
        "jellyfin" => Ok(MediaServerImageRef::Jellyfin {
            input_name: Arc::<str>::from(parts[1].as_str()),
            server_id: Arc::<str>::from(parts[2].as_str()),
            item_id: Arc::<str>::from(parts[3].as_str()),
            image_kind: query_value(query, "image_kind").map(Arc::<str>::from).ok_or_else(|| {
                MediaServerError::new(MediaServerErrorKind::MediaServerItemNotFound)
                    .detail("jellyfin media-server image URL is missing image_kind")
            })?,
            tag: query_value(query, "tag").map(Arc::<str>::from),
        }),
        "plex" => Ok(MediaServerImageRef::Plex {
            input_name: Arc::<str>::from(parts[1].as_str()),
            server_id: Arc::<str>::from(parts[2].as_str()),
            rating_key: Arc::<str>::from(parts[3].as_str()),
            image_path: query_value(query, "image_path").map(Arc::<str>::from).ok_or_else(|| {
                MediaServerError::new(MediaServerErrorKind::MediaServerItemNotFound)
                    .detail("plex media-server image URL is missing image_path")
            })?,
        }),
        _ => Err(MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
            .detail("unsupported media server image URL scheme")),
    }
}

fn query_value(query: &str, key: &str) -> Option<String> {
    url::form_urlencoded::parse(query.as_bytes()).find_map(|(name, value)| (name == key).then(|| value.into_owned()))
}

fn unescape_internal_url_component(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(byte) = decode_hex_byte(bytes[i + 1], bytes[i + 2]) {
                decoded.push(byte);
                i += 3;
                continue;
            }
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn decode_hex_byte(high: u8, low: u8) -> Option<u8> {
    Some(hex_value(high)? << 4 | hex_value(low)?)
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        playlist_mapper::media_server_image_ref_to_internal_url, MediaServerEpisode, MediaServerLibrary,
        MediaServerLibraryRef, MediaServerMovie, MediaServerPage, MediaServerPageRequest, MediaServerSeason,
        MediaServerSeries, MediaServerStatus,
    };
    use futures::{stream, StreamExt};
    use reqwest::header::{HeaderValue, AUTHORIZATION};
    use std::sync::{Arc as StdArc, Mutex};

    #[derive(Default)]
    struct MockPlaybackClient {
        seen_range: Mutex<Option<String>>,
        stream_error: Option<MediaServerError>,
    }

    impl MediaServerCatalogClient for MockPlaybackClient {
        fn discover(&self) -> impl std::future::Future<Output = Result<MediaServerStatus, MediaServerError>> {
            std::future::pending::<Result<MediaServerStatus, MediaServerError>>()
        }
        fn list_libraries(
            &self,
        ) -> impl std::future::Future<Output = Result<Vec<MediaServerLibrary>, MediaServerError>> {
            std::future::ready(Err(MediaServerError::new(MediaServerErrorKind::MediaServerLibraryUnavailable)))
        }
        fn list_movies(
            &self,
            _library: &MediaServerLibraryRef,
            _page: MediaServerPageRequest,
        ) -> impl std::future::Future<Output = Result<MediaServerPage<MediaServerMovie>, MediaServerError>> {
            std::future::ready(Err(MediaServerError::new(MediaServerErrorKind::MediaServerLibraryUnavailable)))
        }
        fn list_series(
            &self,
            _library: &MediaServerLibraryRef,
            _page: MediaServerPageRequest,
        ) -> impl std::future::Future<Output = Result<MediaServerPage<MediaServerSeries>, MediaServerError>> {
            std::future::ready(Err(MediaServerError::new(MediaServerErrorKind::MediaServerLibraryUnavailable)))
        }

        fn list_seasons(
            &self,
            _library: &MediaServerLibraryRef,
            _page: MediaServerPageRequest,
        ) -> impl std::future::Future<Output = Result<MediaServerPage<MediaServerSeason>, MediaServerError>> {
            std::future::ready(Err(MediaServerError::new(MediaServerErrorKind::MediaServerLibraryUnavailable)))
        }

        fn list_episodes(
            &self,
            _library: &MediaServerLibraryRef,
            _page: MediaServerPageRequest,
        ) -> impl std::future::Future<Output = Result<MediaServerPage<MediaServerEpisode>, MediaServerError>> {
            std::future::ready(Err(MediaServerError::new(MediaServerErrorKind::MediaServerLibraryUnavailable)))
        }

        fn open_stream(
            &self,
            _stream_ref: &MediaServerStreamRef,
            range: Option<&str>,
        ) -> impl std::future::Future<Output = Result<crate::MediaServerStreamResponse, MediaServerError>> {
            *self.seen_range.lock().expect("lock") = range.map(ToOwned::to_owned);
            if let Some(error) = self.stream_error.clone() {
                return std::future::ready(Err(error));
            }
            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("video/mp4"));
            headers.insert(CONTENT_RANGE, HeaderValue::from_static("bytes 0-1023/2048"));
            headers.insert(CONTENT_LENGTH, HeaderValue::from_static("1024"));
            headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer should-not-leak"));
            std::future::ready(Ok(MediaServerStreamResponse {
                status: StatusCode::PARTIAL_CONTENT,
                headers,
                body: stream::once(async { Ok::<Bytes, MediaServerError>(Bytes::from_static(b"data")) }).boxed(),
            }))
        }

        fn open_image(
            &self,
            _image_ref: &MediaServerImageRef,
        ) -> impl std::future::Future<Output = Result<MediaServerResourceResponse, MediaServerError>> {
            std::future::ready(Ok(MediaServerResourceResponse {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                body: Bytes::new(),
            }))
        }
    }

    #[tokio::test]
    async fn media_server_stream_response_forwards_range_and_filters_headers() {
        let client = MockPlaybackClient::default();
        let stream_ref = MediaServerStreamRef::Plex {
            input_name: "media_server".into(),
            server_id: "server".into(),
            rating_key: "rating".into(),
            part_key: "/library/parts/redacted/file.mkv".into(),
        };

        let response =
            media_server_stream_response(&client, &stream_ref, Some("bytes=0-1023")).await.expect("stream opens");

        assert_eq!(client.seen_range.lock().expect("lock").as_deref(), Some("bytes=0-1023"));
        assert_eq!(response.status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers.get(CONTENT_RANGE).and_then(|v| v.to_str().ok()), Some("bytes 0-1023/2048"));
        assert!(response.headers.get(AUTHORIZATION).is_none());

        let chunks = response.body.collect::<Vec<_>>().await;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].as_ref().map(Bytes::as_ref), Ok(b"data".as_slice()));
    }

    #[test]
    fn classifies_media_server_local_and_provider_origins() {
        let input_name = StdArc::<str>::from("media_server");
        let media_server = classify_playback_origin(
            InputType::Plex,
            PlaylistItemType::Video,
            &input_name,
            "media-server://plex/server/rating?part_key=%2Flibrary%2Fparts%2Fredacted%2Ffile.mkv",
        )
        .expect("media_server ref parses");
        assert!(matches!(media_server, PlaybackOrigin::MediaServer(MediaServerStreamRef::Plex { .. })));

        let local = classify_playback_origin(
            InputType::Library,
            PlaylistItemType::LocalVideo,
            &input_name,
            "file:///tmp/a.mkv",
        )
        .expect("local classifies");
        assert_eq!(local, PlaybackOrigin::LocalLibrary);

        let encoded = parse_media_server_stream_ref(
            &input_name,
            "media-server://emby/server%2Fone/item%3Fone?media_source_id=media%2Fsource%3Fone",
        )
        .expect("encoded media_server ref parses");
        assert_eq!(
            encoded,
            MediaServerStreamRef::Emby {
                input_name: input_name.clone(),
                server_id: "server/one".into(),
                item_id: "item?one".into(),
                media_source_id: Some("media/source?one".into()),
            }
        );

        let unavailable = parse_media_server_stream_ref(&input_name, "media-server://unavailable/server/library/item")
            .expect_err("unavailable sentinel should map to a stable playback error");
        assert_eq!(unavailable.kind, MediaServerErrorKind::NoDirectPlayableMediaServerSource);

        let provider = classify_playback_origin(
            InputType::M3u,
            PlaylistItemType::Live,
            &input_name,
            "http://example.invalid/live",
        )
        .expect("provider classifies");
        assert_eq!(provider, PlaybackOrigin::Provider);
    }

    #[test]
    fn parse_media_server_image_ref_roundtrips_internal_urls() {
        let emby = MediaServerImageRef::Emby {
            input_name: "emby/input".into(),
            server_id: "server/one+".into(),
            item_id: "item?one".into(),
            image_kind: "Primary".into(),
            tag: Some("tag/one+?".into()),
        };
        assert_eq!(
            parse_media_server_image_ref(&media_server_image_ref_to_internal_url(&emby))
                .expect("emby image ref parses"),
            emby
        );

        let jellyfin = MediaServerImageRef::Jellyfin {
            input_name: "jellyfin/input".into(),
            server_id: "server/two+".into(),
            item_id: "item?two".into(),
            image_kind: "Backdrop".into(),
            tag: Some("tag/two+?".into()),
        };
        assert_eq!(
            parse_media_server_image_ref(&media_server_image_ref_to_internal_url(&jellyfin))
                .expect("jellyfin image ref parses"),
            jellyfin
        );

        let plex = MediaServerImageRef::Plex {
            input_name: "plex/input".into(),
            server_id: "server/three+".into(),
            rating_key: "rating?three".into(),
            image_path: "/library/metadata/1/thumb/2?X-Plex-Token=ignored+".into(),
        };
        assert_eq!(
            parse_media_server_image_ref(&media_server_image_ref_to_internal_url(&plex))
                .expect("plex image ref parses"),
            plex
        );
    }

    #[test]
    fn parse_media_server_image_ref_reports_missing_query_parts_as_not_found() {
        let emby = parse_media_server_image_ref("media-server://image/emby/input/server/item")
            .expect_err("emby image_kind is required");
        assert_eq!(emby.kind, MediaServerErrorKind::MediaServerItemNotFound);

        let jellyfin = parse_media_server_image_ref("media-server://image/jellyfin/input/server/item")
            .expect_err("jellyfin image_kind is required");
        assert_eq!(jellyfin.kind, MediaServerErrorKind::MediaServerItemNotFound);

        let plex = parse_media_server_image_ref("media-server://image/plex/input/server/rating")
            .expect_err("plex image_path is required");
        assert_eq!(plex.kind, MediaServerErrorKind::MediaServerItemNotFound);
    }

    #[test]
    fn parse_media_server_image_ref_rejects_invalid_url_shapes() {
        let wrong_prefix = parse_media_server_image_ref("media-server://plex/input/server/rating")
            .expect_err("image URLs require the image prefix");
        assert_eq!(wrong_prefix.kind, MediaServerErrorKind::MediaServerStreamOpenFailed);

        let too_few_parts = parse_media_server_image_ref("media-server://image/plex/input/server")
            .expect_err("image URLs require enough path parts");
        assert_eq!(too_few_parts.kind, MediaServerErrorKind::MediaServerStreamOpenFailed);

        let unsupported =
            parse_media_server_image_ref("media-server://image/kodi/input/server/item?image_kind=Primary")
                .expect_err("unsupported image schemes are rejected");
        assert_eq!(unsupported.kind, MediaServerErrorKind::MediaServerStreamOpenFailed);
    }

    #[tokio::test]
    async fn media_server_auth_denied_stays_media_server_error() {
        let client = MockPlaybackClient {
            stream_error: Some(MediaServerError::new(MediaServerErrorKind::MediaServerAuthDenied)),
            ..MockPlaybackClient::default()
        };
        let stream_ref = MediaServerStreamRef::Emby {
            input_name: "media_server".into(),
            server_id: "server".into(),
            item_id: "item".into(),
            media_source_id: None,
        };

        let error =
            media_server_stream_response(&client, &stream_ref, None).await.expect_err("auth denied should fail");

        assert_eq!(error.kind, MediaServerErrorKind::MediaServerAuthDenied);
    }
}
