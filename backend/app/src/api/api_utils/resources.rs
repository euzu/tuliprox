use super::{
    force_stream_response, get_headers_from_request, get_hls_playback_ttl_secs, is_hop_by_hop_response_header,
    media_server_image_error_status, open_media_server_image_resource, redirect, serve_file, try_unwrap_body,
    ForceStreamRequestContext, HeaderFilter, ResourceFetchPolicy, StreamResponseMode, RESOURCE_REDIRECT_LIMIT,
};
use crate::{
    api::model::{tee_stream, AppState, StreamDetails, StreamError, UserSession},
    auth::Fingerprint,
    model::{ConfigInput, PlaybackKind},
    utils::{
        async_file_writer, classify_output_resource_url, classify_resource_hop, create_new_file_for_write,
        debug_if_enabled, request,
        request::{classify_resource_destination, send_with_retry_and_provider, ResourceDestination},
        trace_if_enabled, LRUResourceCache, ResourceHop,
    },
};
use arc_swap::ArcSwapOption;
use axum::{
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
};
use futures::TryStreamExt;
use log::{debug, error, warn};
use shared::{
    model::{InputFetchMethod, PlaylistItemType, ResourceOutputPolicy, StreamChannel},
    utils::sanitize_sensitive_info,
};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;
use url::Url;

pub(crate) async fn force_hls_resource_response(
    fingerprint: &Fingerprint,
    app_state: &Arc<AppState>,
    user_session: &UserSession,
    stream_channel: StreamChannel,
    mut ctx: ForceStreamRequestContext<'_>,
    grace_mode: Option<crate::api::model::GraceMode>,
) -> axum::response::Response {
    // `resolve_stream_channel` classified the item with `hls_playback_kind`, like the manifest paths.
    let kind = if stream_channel.item_type == PlaylistItemType::Catchup {
        PlaybackKind::Catchup
    } else {
        PlaybackKind::LiveHls
    };
    ctx.session_reservation_ttl_secs = get_hls_playback_ttl_secs(app_state, kind);
    force_stream_response(
        fingerprint,
        app_state,
        user_session,
        stream_channel,
        ctx,
        grace_mode,
        StreamResponseMode::HlsResource,
    )
    .await
}

/// Status a finite HLS resource answers with instead of a fallback body, or `None` on success.
pub(super) fn hls_resource_failure_status(stream_details: &StreamDetails) -> Option<StatusCode> {
    match stream_details.stream_info.as_ref() {
        Some((_, status, _, _)) if !status.is_success() => Some(*status),
        Some((_, _, _, Some(_))) => Some(StatusCode::SERVICE_UNAVAILABLE),
        _ if stream_details.custom_reason.is_some() => Some(StatusCode::SERVICE_UNAVAILABLE),
        _ if !stream_details.has_stream() => Some(StatusCode::BAD_GATEWAY),
        _ => None,
    }
}

pub(super) fn get_add_cache_content(
    res_url: &str,
    mime_type: Option<String>,
    cache: &Arc<ArcSwapOption<RwLock<LRUResourceCache>>>,
) -> Arc<dyn Fn(usize) + Send + Sync> {
    let resource_url = String::from(res_url);
    let cache = Arc::clone(cache);
    let add_cache_content: Arc<dyn Fn(usize) + Send + Sync> = Arc::new(move |size| {
        let res_url = resource_url.clone();
        let mime_type = mime_type.clone();
        // todo spawn, replace with unboundchannel
        let cache = Arc::clone(&cache);
        tokio::spawn(async move {
            if let Some(cache) = cache.load().as_ref() {
                let _ = cache.write().await.add_content(&res_url, mime_type, size);
            }
        });
    });
    add_cache_content
}

pub(super) fn get_mime_type(headers: &HeaderMap, resource_url: &str) -> Option<String> {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok()) // Option<&str>
        .map(ToString::to_string) // Option<String>
        .or_else(|| {
            // fallback to guess
            mime_guess::from_path(resource_url).first_raw().map(ToString::to_string)
        })
}

pub(super) async fn build_resource_stream_response(
    app_state: &Arc<AppState>,
    cache_key: Option<&str>,
    fetch_policy: ResourceFetchPolicy,
    resource_url: &str,
    response: reqwest::Response,
) -> axum::response::Response {
    let sanitized_resource_url = sanitize_sensitive_info(resource_url);
    let status = response.status();
    let mut response_builder = axum::response::Response::builder().status(status);
    let mime_type = get_mime_type(response.headers(), resource_url);
    let has_content_range = response.headers().contains_key(header::CONTENT_RANGE);
    for (key, value) in response.headers() {
        if !is_hop_by_hop_response_header(key)
            && (fetch_policy == ResourceFetchPolicy::Public
                || matches!(
                    *key,
                    header::CONTENT_TYPE
                        | header::CONTENT_LENGTH
                        | header::CONTENT_ENCODING
                        | header::CONTENT_RANGE
                        | header::ACCEPT_RANGES
                        | header::ETAG
                        | header::LAST_MODIFIED
                        | header::CACHE_CONTROL
                ))
        {
            response_builder = response_builder.header(key, value);
        }
    }

    if !response_builder.headers_ref().is_some_and(|h| h.contains_key(header::CACHE_CONTROL)) {
        response_builder = response_builder.header(header::CACHE_CONTROL, "public, max-age=14400");
    }

    let byte_stream = response.bytes_stream().map_err(|err| StreamError::reqwest(&err));
    // Cache only complete responses (200 OK without Content-Range)
    let can_cache = status == StatusCode::OK && !has_content_range;
    if can_cache {
        if let Some(cache_key) = cache_key {
            debug!("Caching eligible resource stream {sanitized_resource_url}");
            let cache_resource_path = if let Some(cache) = app_state.cache.load().as_ref() {
                Some(cache.write().await.store_path(cache_key, mime_type.as_deref()))
            } else {
                None
            };
            if let Some(resource_path) = cache_resource_path {
                match create_new_file_for_write(&resource_path).await {
                    Ok(file) => {
                        debug!("Persisting resource stream {sanitized_resource_url} to {}", resource_path.display());
                        let writer = async_file_writer(file);
                        let add_cache_content = get_add_cache_content(cache_key, mime_type, &app_state.cache);
                        let tee = tee_stream(byte_stream, writer, &resource_path, add_cache_content);
                        return try_unwrap_body!(response_builder.body(axum::body::Body::from_stream(tee)));
                    }
                    Err(err) => {
                        warn!(
                            "Failed to create cache file {} for {sanitized_resource_url}: {err}",
                            resource_path.display()
                        );
                    }
                }
            } else {
                debug!(
                    "Resource cache unavailable; streaming response for {sanitized_resource_url} without persistence"
                );
            }
        }
    }

    try_unwrap_body!(response_builder.body(axum::body::Body::from_stream(byte_stream)))
}

/// Whether outbound requests of this configuration may leave through a proxy, including one provided by
/// the environment, because the public resource client honours both.
pub(super) fn proxy_in_use(app_state: &Arc<AppState>) -> bool {
    app_state.app_config.config.load().proxy.is_some() || crate::model::proxy_env_present()
}

pub(super) async fn fetch_resource_with_retry(
    app_state: &Arc<AppState>,
    url: &Url,
    fetch_policy: ResourceFetchPolicy,
    resource_url: &str,
    req_headers: &HashMap<String, Vec<u8>>,
    input: Option<&ConfigInput>,
) -> Option<axum::response::Response> {
    let cache_key = fetch_policy.cache_key(resource_url);
    let config = app_state.app_config.config.load();
    let default_user_agent = config.default_user_agent.clone();
    drop(config);

    let disabled_headers = app_state.get_disabled_headers();
    let mut current_url = url.clone();
    let mut current_headers = req_headers.clone();
    let mut current_input = input;
    let mut method = input.map_or(InputFetchMethod::GET, |i| i.method);

    for redirects in 0..=RESOURCE_REDIRECT_LIMIT {
        // Every hop is routed on its own, so a redirect cannot move a request to an egress the hop
        // itself would not have used.
        let hop = classify_resource_hop(current_url.as_str(), proxy_in_use(app_state)).await;
        if hop == ResourceHop::Blocked {
            debug!("Refused resource destination local to this host: {}", sanitize_sensitive_info(resource_url));
            return None;
        }
        let use_proxy_aware_client = hop == ResourceHop::ViaProxy;
        let provider_config = current_input.and_then(|i| i.get_resolve_provider(current_url.as_str()));
        let response = match send_with_retry_and_provider(
            &app_state.app_config,
            &current_url,
            provider_config.as_ref(),
            // Hand redirects back instead of retrying them: the loop below follows them itself, so that
            // every hop is classified and no client-side redirect policy decides where the request ends.
            true,
            |resolved_url| {
                let http_client = if use_proxy_aware_client {
                    app_state.http_clients.resource_public_no_redirect.load()
                } else {
                    app_state.http_clients.resource_no_redirect.load()
                };
                request::get_client_request(
                    &http_client,
                    method,
                    current_input.map(|i| &i.headers),
                    resolved_url,
                    Some(&current_headers),
                    disabled_headers.as_ref(),
                    default_user_agent.as_deref(),
                )
            },
        )
        .await
        {
            Ok(response) => response,
            Err(err) => {
                debug!(
                    "Resource fetch failed for {}: {}",
                    sanitize_sensitive_info(resource_url),
                    sanitize_sensitive_info(&err.to_string())
                );
                return None;
            }
        };

        let status = response.status();
        if status.is_redirection() {
            if redirects == RESOURCE_REDIRECT_LIMIT {
                debug!("Resource redirect limit reached for {}", sanitize_sensitive_info(resource_url));
                return None;
            }
            let next_url = response
                .headers()
                .get(header::LOCATION)
                .and_then(|location| location.to_str().ok())
                .and_then(|location| response.url().join(location).ok());
            let Some(next_url) = next_url else {
                debug!("Resource redirect has no usable location for {}", sanitize_sensitive_info(resource_url));
                return None;
            };
            let same_origin = response.url().scheme() == next_url.scheme()
                && response.url().host_str() == next_url.host_str()
                && response.url().port_or_known_default() == next_url.port_or_known_default();
            if !same_origin {
                current_headers.retain(|key, _| request::is_safe_cross_origin_redirect_header(key));
                current_input = None;
            }
            if !matches!(status, StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT) {
                method = InputFetchMethod::GET;
            }
            current_url = next_url;
            continue;
        }

        if status.is_success() {
            return Some(
                build_resource_stream_response(app_state, cache_key, fetch_policy, resource_url, response).await,
            );
        }

        debug_if_enabled!("Failed to open resource got status {status} for {}", sanitize_sensitive_info(resource_url));
        if !fetch_policy.relays_upstream_response() {
            return Some(status.into_response());
        }
        let mut response_builder = axum::response::Response::builder().status(status);
        for (key, value) in response.headers() {
            if !is_hop_by_hop_response_header(key) {
                response_builder = response_builder.header(key, value);
            }
        }
        let stream = response.bytes_stream().map_err(|err| StreamError::reqwest(&err));
        return Some(try_unwrap_body!(response_builder.body(axum::body::Body::from_stream(stream))));
    }
    None
}

pub fn resource_input_for_url<'a>(input: Option<&'a ConfigInput>, resource_url: &str) -> Option<&'a ConfigInput> {
    let input = input?;
    let source = Url::parse(&input.url).ok()?;
    let resource = Url::parse(resource_url).ok()?;
    (source.scheme() == resource.scheme()
        && source.host_str() == resource.host_str()
        && source.port_or_known_default() == resource.port_or_known_default())
    .then_some(input)
}

pub async fn resource_redirect_or_proxy(
    app_state: &Arc<AppState>,
    resource_url: &str,
    req_headers: &HeaderMap,
    input: Option<&ConfigInput>,
) -> axum::response::Response {
    let Some(resource_url) = shared::model::persisted_resource_url(resource_url) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let resource_url = resource_url.as_ref();
    if resource_url.starts_with("/api/v1/library/thumbnail/") {
        return redirect(resource_url).into_response();
    }
    if resource_url.starts_with("media-server://image/") {
        return resource_response(app_state, ResourceFetchPolicy::NonPublic, resource_url, req_headers, None)
            .await
            .into_response();
    }
    let Ok(url) = Url::parse(resource_url) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if !matches!(url.scheme(), "http" | "https") {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(host) = url.host_str() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if classify_resource_destination(host).await == ResourceDestination::Public {
        redirect(resource_url).into_response()
    } else {
        resource_proxy_response(app_state, resource_url, req_headers, resource_input_for_url(input, resource_url)).await
    }
}

pub async fn resource_proxy_response(
    app_state: &Arc<AppState>,
    resource_url: &str,
    req_headers: &HeaderMap,
    input: Option<&ConfigInput>,
) -> axum::response::Response {
    let Some(resource_url) = shared::model::persisted_resource_url(resource_url) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let resource_url = resource_url.as_ref();
    let input = resource_input_for_url(input, resource_url);
    match classify_output_resource_url(resource_url).await {
        ResourceOutputPolicy::Direct if resource_url.starts_with("/api/v1/library/thumbnail/") => {
            redirect(resource_url).into_response()
        }
        ResourceOutputPolicy::Direct => {
            resource_response(app_state, ResourceFetchPolicy::Public, resource_url, req_headers, input)
                .await
                .into_response()
        }
        ResourceOutputPolicy::Proxy => {
            resource_response(app_state, ResourceFetchPolicy::NonPublic, resource_url, req_headers, input)
                .await
                .into_response()
        }
        ResourceOutputPolicy::Blocked => StatusCode::FORBIDDEN.into_response(),
    }
}

/// Callers must pass an already-decoded resource URL (not a `resource://v1/` locator).
/// [`resource_redirect_or_proxy`] and [`resource_proxy_response`] handle the decode before
/// dispatching here; XMLTV and Web-UI endpoints pass URLs from authenticated tokens.
pub async fn resource_response(
    app_state: &Arc<AppState>,
    fetch_policy: ResourceFetchPolicy,
    resource_url: &str,
    req_headers: &HeaderMap,
    input: Option<&ConfigInput>,
) -> impl IntoResponse + Send {
    if resource_url.is_empty() {
        return StatusCode::NO_CONTENT.into_response();
    }

    if resource_url.starts_with("media-server://image/") {
        return match open_media_server_image_resource(app_state, resource_url).await {
            Ok(response) => response,
            Err(err) => {
                let status = media_server_image_error_status(&err);
                match status {
                    StatusCode::BAD_REQUEST => warn!("Invalid media-server image resource URL: {err}"),
                    StatusCode::NOT_FOUND => debug!("Media-server image resource was not found: {err}"),
                    _ => error!("Can't open media-server image from upstream: {err}"),
                }
                status.into_response()
            }
        };
    }
    let filter: HeaderFilter = Some(Box::new(request::is_safe_cross_origin_redirect_header));
    let req_headers = get_headers_from_request(req_headers, &filter);
    let cache_key = fetch_policy.cache_key(resource_url);
    if let (Some(cache_key), Some(cache)) = (cache_key, app_state.cache.load().as_ref()) {
        let cache_hit = {
            let mut guard = cache.write().await;
            guard.get_content(cache_key)
        };

        if let Some((resource_path, mime_type)) = cache_hit {
            trace_if_enabled!("Responding resource from cache {}", sanitize_sensitive_info(resource_url));
            return serve_file(
                &resource_path,
                mime_type.unwrap_or_else(|| mime::APPLICATION_OCTET_STREAM.to_string()),
                Some("public, max-age=14400"),
            )
            .await
            .into_response();
        }
    }
    trace_if_enabled!("Try to fetch resource {}", sanitize_sensitive_info(resource_url));
    if let Ok(url) = Url::parse(resource_url) {
        // A resource URL is chosen by playlist or EPG content, so the destination is never fetched
        // blindly: an address local to this host (loopback, link-local, cloud metadata) would turn
        // the proxy into a reader for the proxy host itself. Private network destinations stay
        // allowed, because self-hosted services are the reason this route exists.
        let Some(host) = url.host_str() else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        if classify_resource_destination(host).await == ResourceDestination::Blocked {
            debug!("Refused resource destination local to this host: {}", sanitize_sensitive_info(resource_url));
            return StatusCode::FORBIDDEN.into_response();
        }
        if let Some(resp) =
            fetch_resource_with_retry(app_state, &url, fetch_policy, resource_url, &req_headers, input).await
        {
            return resp;
        }
        // Upstream failure after retries
        return StatusCode::BAD_GATEWAY.into_response();
    }
    error!("Url is malformed {}", sanitize_sensitive_info(resource_url));
    StatusCode::BAD_REQUEST.into_response()
}
