use super::is_hop_by_hop_response_header;
use crate::{
    api::model::{AppState, BoxedProviderStream, ProviderStreamInfo, StreamError},
    media_server::{
        playback::{
            media_server_image_response as open_media_server_proxy_image_response,
            media_server_stream_response as open_media_server_proxy_stream_response, parse_media_server_image_ref,
            parse_media_server_stream_ref, safe_media_server_response_headers,
        },
        AnyMediaServerCatalogClient, MediaServerError, MediaServerErrorKind, MediaServerHttpClient,
        MediaServerImageRef, MediaServerStreamRef,
    },
    model::ConfigInput,
};
use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderName, HeaderValue, Response, StatusCode},
};
use futures::{StreamExt, TryStreamExt};
use shared::{
    model::{HookCtx, PluginHook, PluginHost, PluginImageRequest, PluginStreamRequest, PluginStreamTarget},
    utils::sanitize_sensitive_info,
};
use std::sync::Arc;
use url::Url;

pub(super) fn is_media_server_playback_url(input: &ConfigInput, stream_url: &str) -> bool {
    input.input_type.is_media_server() || is_media_server_stream_ref_url(stream_url)
}

/// A `media-server://` reference owned by a media-server input; never a provider URL.
pub(super) fn is_media_server_input_stream_ref(input: &ConfigInput, stream_url: &str) -> bool {
    (input.input_type.is_media_server() || input.input_type.is_plugin()) && is_media_server_stream_ref_url(stream_url)
}

pub(super) fn is_media_server_stream_ref_url(stream_url: &str) -> bool {
    Url::parse(stream_url).is_ok_and(|url| url.scheme() == "media-server")
}

fn media_server_catalog_client_for_input(
    app_state: &AppState,
    input: &ConfigInput,
    not_media_server_detail: &'static str,
) -> Result<AnyMediaServerCatalogClient, MediaServerError> {
    if !input.input_type.is_media_server() {
        return Err(MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
            .provider("media-server")
            .detail(not_media_server_detail));
    }
    let http_client = MediaServerHttpClient::new(app_state.http_clients.default.load().as_ref().clone());
    input.media_server_catalog_client(http_client)
}

fn plugin_media_error(kind: MediaServerErrorKind, detail: impl Into<String>) -> MediaServerError {
    MediaServerError::new(kind).provider("plugin").detail(detail.into())
}

/// The input must still name `plugin_id` and that plugin must still serve a media catalog.
fn ensure_plugin_media_provider(
    app_state: &AppState,
    input: &ConfigInput,
    plugin_id: &str,
) -> Result<(), MediaServerError> {
    if input.plugin.as_deref() != Some(plugin_id) {
        return Err(plugin_media_error(
            MediaServerErrorKind::MediaServerItemNotFound,
            "input is no longer bound to plugin",
        ));
    }
    if !app_state.plugins.provides(PluginHook::MediaFetch, plugin_id) {
        return Err(plugin_media_error(
            MediaServerErrorKind::MediaServerItemNotFound,
            format!("plugin '{plugin_id}' is not installed or enabled"),
        ));
    }
    Ok(())
}

/// Opens the upstream target a media provider plugin resolved. The plugin's
/// headers were screened by the host; only safe response headers are kept.
async fn open_plugin_target(
    app_state: &AppState,
    target: &PluginStreamTarget,
    range: Option<&str>,
) -> Result<(StatusCode, HeaderMap, BoxedProviderStream), MediaServerError> {
    let client = app_state.http_clients.default.load();
    let mut request = client.get(&target.url);
    if let Some(range) = range {
        request = request.header(header::RANGE, range);
    }
    for (name, value) in &target.headers {
        if range.is_some() && name.eq_ignore_ascii_case("range") {
            continue;
        }
        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
            request = request.header(name, value);
        }
    }
    let response = request.send().await.map_err(|err| {
        plugin_media_error(
            MediaServerErrorKind::MediaServerStreamOpenFailed,
            format!("upstream request failed: {}", sanitize_sensitive_info(&err.to_string())),
        )
    })?;
    let status = response.status();
    let headers = safe_media_server_response_headers(response.headers());
    let body = response
        .bytes_stream()
        .map_err(|err| StreamError::Stream(sanitize_sensitive_info(&err.to_string()).into_owned()))
        .boxed();
    Ok((status, headers, body))
}

fn plugin_resolve_error(what: &str, err: &str) -> MediaServerError {
    plugin_media_error(
        MediaServerErrorKind::MediaServerStreamOpenFailed,
        format!("plugin {what} resolution failed: {}", sanitize_sensitive_info(err)),
    )
}

async fn open_plugin_stream_for_input(
    app_state: &AppState,
    input: &ConfigInput,
    plugin_id: &str,
    item_id: &str,
    stream_key: Option<&str>,
    req_headers: &HeaderMap,
) -> Result<(BoxedProviderStream, ProviderStreamInfo), MediaServerError> {
    ensure_plugin_media_provider(app_state, input, plugin_id)?;
    let range = req_headers.get(header::RANGE).and_then(|value| value.to_str().ok());
    let ctx = HookCtx::new("").with_input(&input.name);
    let request = PluginStreamRequest {
        input_name: &input.name,
        url: &input.url,
        username: input.username.as_deref(),
        password: input.password.as_deref(),
        options: &input.plugin_options,
        item_id,
        stream_key,
        range,
    };
    let target = app_state
        .plugins
        .resolve_stream(plugin_id, &ctx, &request)
        .await
        .map_err(|err| plugin_resolve_error("stream", &err))?;
    let (status, headers, body) = open_plugin_target(app_state, &target, range).await?;
    Ok((body, Some((header_pairs(&headers), status, None, None))))
}

async fn open_plugin_image_resource(
    app_state: &AppState,
    input_name: &str,
    plugin_id: &str,
    item_id: &str,
    image_key: &str,
) -> Result<Response<Body>, MediaServerError> {
    let input = app_state.app_config.get_input_by_name(&Arc::<str>::from(input_name)).ok_or_else(|| {
        plugin_media_error(MediaServerErrorKind::MediaServerItemNotFound, "media-server image input was not found")
    })?;
    ensure_plugin_media_provider(app_state, &input, plugin_id)?;
    let ctx = HookCtx::new("").with_input(&input.name);
    let request = PluginImageRequest {
        input_name: &input.name,
        url: &input.url,
        username: input.username.as_deref(),
        password: input.password.as_deref(),
        options: &input.plugin_options,
        item_id,
        image_key,
    };
    let target = app_state
        .plugins
        .resolve_image(plugin_id, &ctx, &request)
        .await
        .map_err(|err| plugin_resolve_error("image", &err))?;
    let (status, headers, body) = open_plugin_target(app_state, &target, None).await?;
    build_image_response(status, &headers, body, "plugin")
}

/// The proxied response headers as owned pairs, without hop-by-hop headers.
fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(key, _)| !is_hop_by_hop_response_header(key))
        .filter_map(|(key, value)| value.to_str().ok().map(|value| (key.to_string(), value.to_string())))
        .collect()
}

fn build_image_response<S>(
    status: StatusCode,
    headers: &HeaderMap,
    body: S,
    provider: &'static str,
) -> Result<Response<Body>, MediaServerError>
where
    S: futures::TryStream + Send + 'static,
    S::Ok: Into<axum::body::Bytes>,
    S::Error: Into<axum::BoxError>,
{
    let mut builder = Response::builder().status(status);
    for (key, value) in headers {
        if !is_hop_by_hop_response_header(key) {
            builder = builder.header(key, value);
        }
    }
    builder.body(Body::from_stream(body)).map_err(|err| {
        MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
            .provider(provider)
            .detail(format!("{provider} image response build failed: {err}"))
    })
}

pub(super) async fn open_media_server_stream_for_input(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    stream_url: &str,
    req_headers: &HeaderMap,
) -> Result<(BoxedProviderStream, ProviderStreamInfo), MediaServerError> {
    let stream_ref = parse_media_server_stream_ref(&input.name, stream_url)?;
    if let MediaServerStreamRef::Plugin { ref plugin_id, ref item_id, ref stream_key, .. } = stream_ref {
        return open_plugin_stream_for_input(app_state, input, plugin_id, item_id, stream_key.as_deref(), req_headers)
            .await;
    }
    let range = req_headers.get(header::RANGE).and_then(|value| value.to_str().ok());
    let client =
        media_server_catalog_client_for_input(app_state, input, "playlist item is not backed by a media-server input")?;
    let response = open_media_server_proxy_stream_response(&client, &stream_ref, range).await?;

    let headers = header_pairs(&response.headers);
    let status = response.status;
    let stream = response.body.map_err(|err| StreamError::Stream(err.to_string())).boxed();
    Ok((stream, Some((headers, status, None, None))))
}

pub(super) async fn open_media_server_image_resource(
    app_state: &Arc<AppState>,
    resource_url: &str,
) -> Result<Response<Body>, MediaServerError> {
    let image_ref = parse_media_server_image_ref(resource_url)?;
    if let MediaServerImageRef::Plugin { ref input_name, ref plugin_id, ref item_id, ref image_key } = image_ref {
        return open_plugin_image_resource(app_state, input_name, plugin_id, item_id, image_key).await;
    }
    let input_name = media_server_image_input_name(&image_ref);
    let input = app_state.app_config.get_input_by_name(input_name).ok_or_else(|| {
        MediaServerError::new(MediaServerErrorKind::MediaServerItemNotFound)
            .provider("media-server")
            .detail("media-server image input was not found")
    })?;
    let client = media_server_catalog_client_for_input(
        app_state,
        &input,
        "media-server image input is not backed by a media-server input",
    )?;
    let response = open_media_server_proxy_image_response(&client, &image_ref).await?;

    let body = response.body.map_err(|err| StreamError::Stream(err.to_string()));
    build_image_response(response.status, &response.headers, body, "media-server")
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
        | MediaServerImageRef::Silo { input_name, .. }
        | MediaServerImageRef::Plex { input_name, .. }
        | MediaServerImageRef::Plugin { input_name, .. } => input_name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::api_utils::tests::create_test_app_state;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    async fn spawn_single_request_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test server binds");
        let addr = listener.local_addr().expect("test server address");
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            loop {
                let n = socket.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await;
            String::from_utf8_lossy(&request).to_lowercase()
        });
        (addr, task)
    }

    #[tokio::test]
    async fn open_plugin_target_drops_plugin_range_when_client_range_present() {
        let app_state = create_test_app_state();
        let (addr, server_task) = spawn_single_request_server().await;

        let target = PluginStreamTarget {
            url: format!("http://{addr}/stream"),
            headers: vec![("Range".into(), "bytes=0-10".into()), ("RANGE".into(), "bytes=50-60".into())],
            ttl_secs: None,
        };

        let result = open_plugin_target(&app_state, &target, Some("bytes=100-200")).await;
        assert!(result.is_ok());

        let req = tokio::time::timeout(std::time::Duration::from_secs(5), server_task)
            .await
            .expect("server completed")
            .expect("server ok");
        assert!(req.contains("range: bytes=100-200"), "client range should be present: {req}");
        assert!(!req.contains("range: bytes=0-10"), "plugin range 0-10 should be dropped: {req}");
        assert!(!req.contains("range: bytes=50-60"), "plugin range 50-60 should be dropped: {req}");
    }

    #[tokio::test]
    async fn open_plugin_target_keeps_plugin_range_when_no_client_range() {
        let app_state = create_test_app_state();
        let (addr, server_task) = spawn_single_request_server().await;

        let target = PluginStreamTarget {
            url: format!("http://{addr}/stream"),
            headers: vec![("Range".into(), "bytes=0-10".into())],
            ttl_secs: None,
        };

        let result = open_plugin_target(&app_state, &target, None).await;
        assert!(result.is_ok());

        let req = tokio::time::timeout(std::time::Duration::from_secs(5), server_task)
            .await
            .expect("server completed")
            .expect("server ok");
        assert!(req.contains("range: bytes=0-10"), "plugin range should be kept: {req}");
    }
}
