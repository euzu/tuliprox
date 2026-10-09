use super::is_hop_by_hop_response_header;
use crate::{
    api::model::{AppState, BoxedProviderStream, ProviderStreamInfo, StreamError},
    media_server::{
        playback::{
            media_server_image_response as open_media_server_proxy_image_response,
            media_server_stream_response as open_media_server_proxy_stream_response, parse_media_server_image_ref,
            parse_media_server_stream_ref,
        },
        MediaServerError, MediaServerErrorKind, MediaServerHttpClient, MediaServerImageRef,
    },
    model::ConfigInput,
};
use axum::{
    body::Body,
    http::{header, HeaderMap, Response, StatusCode},
};
use futures::{StreamExt, TryStreamExt};
use shared::model::InputType;
use std::sync::Arc;
use url::Url;

pub(super) fn is_media_server_playback_url(input: &ConfigInput, stream_url: &str) -> bool {
    input.input_type == InputType::Plex || is_media_server_stream_ref_url(stream_url)
}

pub(super) fn is_media_server_stream_ref_url(stream_url: &str) -> bool {
    Url::parse(stream_url).is_ok_and(|url| url.scheme() == "media-server")
}

pub(super) async fn open_media_server_stream_for_input(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    stream_url: &str,
    req_headers: &HeaderMap,
) -> Result<(BoxedProviderStream, ProviderStreamInfo), MediaServerError> {
    let stream_ref = parse_media_server_stream_ref(&input.name, stream_url)?;
    let range = req_headers.get(header::RANGE).and_then(|value| value.to_str().ok());
    let http_client = MediaServerHttpClient::new(app_state.http_clients.default.load().as_ref().clone());

    let response = match input.input_type {
        InputType::Plex => {
            let client = input.plex_catalog_client(http_client)?;
            open_media_server_proxy_stream_response(&client, &stream_ref, range).await?
        }
        InputType::Emby | InputType::Jellyfin => {
            return Err(MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
                .provider("media-server")
                .detail("media-server playback proxy is not implemented for this input type"));
        }
        InputType::M3u
        | InputType::Xtream
        | InputType::M3uBatch
        | InputType::XtreamBatch
        | InputType::Stalker
        | InputType::StalkerBatch
        | InputType::Library
        | InputType::Staged => {
            return Err(MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
                .provider("media-server")
                .detail("playlist item is not backed by a media-server input"));
        }
    };

    let headers = response
        .headers
        .iter()
        .filter(|(key, _)| !is_hop_by_hop_response_header(key))
        .filter_map(|(key, value)| value.to_str().ok().map(|value| (key.to_string(), value.to_string())))
        .collect::<Vec<_>>();
    let status = response.status;
    let stream = response.body.map_err(|err| StreamError::Stream(err.to_string())).boxed();
    Ok((stream, Some((headers, status, None, None))))
}

pub(super) async fn open_media_server_image_resource(
    app_state: &Arc<AppState>,
    resource_url: &str,
) -> Result<Response<Body>, MediaServerError> {
    let image_ref = parse_media_server_image_ref(resource_url)?;
    let input_name = media_server_image_input_name(&image_ref);
    let input = app_state.app_config.get_input_by_name(input_name).ok_or_else(|| {
        MediaServerError::new(MediaServerErrorKind::MediaServerItemNotFound)
            .provider("media-server")
            .detail("media-server image input was not found")
    })?;
    let http_client = MediaServerHttpClient::new(app_state.http_clients.default.load().as_ref().clone());

    let response = match input.input_type {
        InputType::Plex => {
            let client = input.plex_catalog_client(http_client)?;
            open_media_server_proxy_image_response(&client, &image_ref).await?
        }
        InputType::Emby | InputType::Jellyfin => {
            return Err(MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
                .provider("media-server")
                .detail("media-server image proxy is not implemented for this input type"));
        }
        InputType::M3u
        | InputType::Xtream
        | InputType::M3uBatch
        | InputType::XtreamBatch
        | InputType::Stalker
        | InputType::StalkerBatch
        | InputType::Library
        | InputType::Staged => {
            return Err(MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
                .provider("media-server")
                .detail("media-server image input is not backed by a media-server input"));
        }
    };

    let mut builder = Response::builder().status(response.status);
    for (key, value) in &response.headers {
        if !is_hop_by_hop_response_header(key) {
            builder = builder.header(key, value);
        }
    }
    let body = response.body.map_err(|err| StreamError::Stream(err.to_string()));
    builder.body(Body::from_stream(body)).map_err(|err| {
        MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
            .provider("media-server")
            .detail(format!("media-server image response build failed: {err}"))
    })
}

pub(super) fn media_server_image_error_status(err: &MediaServerError) -> StatusCode {
    match err.kind {
        MediaServerErrorKind::MediaServerItemNotFound | MediaServerErrorKind::NoDirectPlayableMediaServerSource => {
            StatusCode::NOT_FOUND
        }
        MediaServerErrorKind::MediaServerStreamOpenFailed if is_media_server_image_validation_error(err) => {
            StatusCode::BAD_REQUEST
        }
        MediaServerErrorKind::MediaServerStreamOpenFailed
        | MediaServerErrorKind::MediaServerAuthDenied
        | MediaServerErrorKind::MediaServerUnavailable
        | MediaServerErrorKind::MediaServerLibraryUnavailable
        | MediaServerErrorKind::MediaServerLibraryTypeUnsupported
        | MediaServerErrorKind::MediaServerCatalogDecodeFailed
        | MediaServerErrorKind::MediaServerCatalogPageStalled
        | MediaServerErrorKind::MediaServerCatalogIncomplete
        | MediaServerErrorKind::MediaServerRateLimited
        | MediaServerErrorKind::MediaServerDiscoveryFailed => StatusCode::BAD_GATEWAY,
    }
}

pub(super) fn is_media_server_image_validation_error(err: &MediaServerError) -> bool {
    err.detail_text().is_some_and(|detail| {
        detail.contains("resource URL is not a media server image URL")
            || detail.contains("media server image URL is missing required path parts")
            || detail.contains("unsupported media server image URL scheme")
            || detail.contains("media-server image input is not backed by a media-server input")
    })
}

pub(super) fn media_server_image_input_name(image_ref: &MediaServerImageRef) -> &Arc<str> {
    match image_ref {
        MediaServerImageRef::Emby { input_name, .. }
        | MediaServerImageRef::Jellyfin { input_name, .. }
        | MediaServerImageRef::Plex { input_name, .. } => input_name,
    }
}
