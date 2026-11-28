use super::{
    blocks_new_playback, get_redirect_alternative_url, get_stream_alternative_url, is_media_server_playback_url,
    redirect, resolve_request_url_for_logging, RedirectParams,
};
#[cfg(test)]
use super::{resolve_streaming_strategy_with_provider_handle, StreamingAcquireOptions};
#[cfg(test)]
use crate::api::model::StreamingStrategy;
#[cfg(test)]
use crate::auth::Fingerprint;
use crate::{
    api::{
        endpoints::xtream_api::get_xtream_player_api_stream_url,
        model::{AppState, ProviderStreamState},
    },
    model::{ConfigInput, ConfigTarget, ProxyUserCredentials},
    utils::debug_if_enabled,
};
use axum::{http::StatusCode, response::IntoResponse};
use log::error;
#[cfg(test)]
use shared::model::StreamChannel;
use shared::{
    defaults::{DASH_EXT, HLS_EXT},
    error::TuliproxError,
    model::{PlaylistEntry, PlaylistItemType, TargetType, UserConnectionPermission},
    utils::{replace_url_extension, sanitize_sensitive_info},
};
use std::{borrow::Cow, sync::Arc};

pub(crate) fn resolve_redirect_location<'a>(
    input: Option<&ConfigInput>,
    stream_url: &'a str,
) -> Result<Cow<'a, str>, TuliproxError> {
    input.map_or(Ok(Cow::Borrowed(stream_url)), |input| input.resolve_url(stream_url))
}

/// Determines the appropriate streaming strategy for the given input and stream URL.
///
/// This function attempts to acquire a connection to a streaming provider, either using a forced provider
/// (if specified), or based on the input name. It then selects a corresponding `StreamingOption`:
///
/// - If no connections are available (`Exhausted`), it returns a custom stream indicating exhaustion.
/// - If a connection is available or in a grace period, it constructs a streaming URL accordingly:
///   - If the URL already targets the selected provider account, the original URL is reused.
///   - Otherwise, an alternative URL is generated based on the provider and input.
///
/// The function returns:
/// - an optional `ProviderConnectionGuard` to manage the connection's lifecycle,
/// - a `ProviderStreamState` describing how the stream state is,
/// - and optional HTTP headers to include in the request.
///
/// This logic helps abstract the decision-making behind provider selection and stream URL resolution.
#[cfg(test)]
pub(super) async fn resolve_streaming_strategy(
    app_state: &Arc<AppState>,
    stream_url: &str,
    fingerprint: &Fingerprint,
    input: &ConfigInput,
    options: StreamingAcquireOptions<'_>,
    stream_channel: Option<&StreamChannel>,
) -> StreamingStrategy {
    resolve_streaming_strategy_with_provider_handle(
        app_state,
        stream_url,
        fingerprint,
        input,
        options,
        stream_channel,
        None,
    )
    .await
}

pub(super) fn get_grace_period_millis(
    connection_permission: UserConnectionPermission,
    stream_response_params: &ProviderStreamState,
    config_grace_period_millis: u64,
) -> u64 {
    if config_grace_period_millis > 0
        && (
            matches!(stream_response_params, ProviderStreamState::GracePeriod(_, _)) // provider grace period
            || connection_permission == UserConnectionPermission::GracePeriod
            // user grace period
        )
    {
        config_grace_period_millis
    } else {
        0
    }
}

pub(super) fn should_defer_provider_open_for_grace_hold(
    provider_grace_active: bool,
    hold_stream: bool,
    item_type: PlaylistItemType,
    is_reopen: bool,
) -> bool {
    if !(provider_grace_active && hold_stream) {
        return false;
    }

    // Catch-up must open immediately so its payload can be classified before response headers are committed.
    if item_type == PlaylistItemType::Catchup {
        return false;
    }

    // v3.3.0 opened provider-affine VOD/Series reopens immediately, even when
    // provider grace was temporarily in effect. Parking these requests in GracePending
    // was introduced later and breaks players like libmpv during seek/reopen retries.
    // Keep hold-stream behavior for live/admission paths, but restore direct-open behavior
    // for provider-affine on-demand session reopens.
    !(!item_type.is_live() && item_type.requires_provider_affinity() && is_reopen)
}

/// Redirect decision for a playback request.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PlaybackRedirectDecision {
    /// Input in maintenance or a media-server playback URL: the request is never redirected.
    Disabled,
    /// The request stays on this listener.
    Proxied,
    /// Redirect to the provider URL unchanged.
    Plain,
    /// Redirect to the HLS variant of the provider URL.
    Hls,
    /// Redirect to the DASH variant of the provider URL.
    Dash,
    /// HLS playlist item requested with a DASH extension: an M3u redirect ends as DASH, an Xtream redirect as HLS.
    HlsAndDash,
}

impl PlaybackRedirectDecision {
    fn is_disabled(self) -> bool { matches!(self, Self::Disabled) }
    fn should_redirect(self) -> bool { matches!(self, Self::Plain | Self::Hls | Self::Dash | Self::HlsAndDash) }
    fn is_hls(self) -> bool { matches!(self, Self::Hls | Self::HlsAndDash) }
    fn is_dash(self) -> bool { matches!(self, Self::Dash | Self::HlsAndDash) }
}

fn classify_playback_redirect(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    target: &ConfigTarget,
    user: &ProxyUserCredentials,
    item_type: PlaylistItemType,
    provider_url: &str,
    stream_ext: Option<&str>,
) -> PlaybackRedirectDecision {
    if blocks_new_playback(app_state, input, user, false) || is_media_server_playback_url(input, provider_url) {
        return PlaybackRedirectDecision::Disabled;
    }

    let redirect_request = user.proxy.is_redirect(item_type) || target.is_force_redirect(item_type);
    let is_hls_request = item_type == PlaylistItemType::LiveHls || stream_ext == Some(HLS_EXT);
    let is_dash_request = (!is_hls_request && item_type == PlaylistItemType::LiveDash) || stream_ext == Some(DASH_EXT);

    // Recording playback keeps provider headers and proxy handling on this listener.
    // DASH redirects because relative MPD resource URLs are not rewritten.
    if is_dash_request || (!user.is_recording_proxy_user() && redirect_request) {
        return match (is_hls_request, is_dash_request) {
            (true, true) => PlaybackRedirectDecision::HlsAndDash,
            (true, false) => PlaybackRedirectDecision::Hls,
            (false, true) => PlaybackRedirectDecision::Dash,
            (false, false) => PlaybackRedirectDecision::Plain,
        };
    }
    PlaybackRedirectDecision::Proxied
}

pub(crate) fn should_redirect_playback(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    target: &ConfigTarget,
    user: &ProxyUserCredentials,
    item_type: PlaylistItemType,
    provider_url: &str,
    stream_ext: Option<&str>,
) -> bool {
    classify_playback_redirect(app_state, input, target, user, item_type, provider_url, stream_ext).should_redirect()
}

pub async fn redirect_response<'a, P>(
    app_state: &Arc<AppState>,
    params: &'a RedirectParams<'a, P>,
) -> Option<impl IntoResponse + Send>
where
    P: PlaylistEntry,
{
    let item_type = params.item.get_item_type();
    let provider_url = params.item.get_provider_url();
    // An input in maintenance or media server playback is never redirected to its provider.
    let decision = classify_playback_redirect(
        app_state,
        params.input,
        params.target,
        params.user,
        item_type,
        provider_url.as_ref(),
        params.stream_ext,
    );
    if decision.is_disabled() {
        return None;
    }
    let is_hls_request = decision.is_hls();
    let is_dash_request = decision.is_dash();

    if params.user.is_recording_proxy_user() && !is_dash_request {
        return None;
    }

    if params.target_type == TargetType::M3u {
        if decision.should_redirect() {
            let redirect_url: Arc<str> = if is_hls_request {
                replace_url_extension(&provider_url, HLS_EXT).into()
            } else {
                provider_url.clone()
            };
            let redirect_url =
                if is_dash_request { replace_url_extension(&redirect_url, DASH_EXT).into() } else { redirect_url };
            let redirect_url = get_redirect_alternative_url(app_state, &redirect_url, params.input);
            let redirect_url = match resolve_redirect_location(Some(params.input), &redirect_url) {
                Ok(url) => url,
                Err(err) => {
                    error!("Failed to resolve redirect url: {}", sanitize_sensitive_info(&err.to_string()));
                    return Some(StatusCode::BAD_REQUEST.into_response());
                }
            };
            debug_if_enabled!("Redirecting stream request to {}", sanitize_sensitive_info(redirect_url.as_ref()));
            return Some(redirect(redirect_url.as_ref()).into_response());
        }
    } else if params.target_type == TargetType::Xtream {
        let Some(provider_id) = params.provider_id else {
            return Some(StatusCode::BAD_REQUEST.into_response());
        };

        if decision.should_redirect() {
            let target_name = params.target.name.as_str();
            let virtual_id = params.item.get_virtual_id();
            let stream_url = match get_xtream_player_api_stream_url(
                params.input,
                params.req_context,
                &params.get_query_path(provider_id, &provider_url),
                &provider_url,
            ) {
                None => {
                    error!(
                        "Can't find stream url for target {target_name}, context {}, stream_id {virtual_id}",
                        params.req_context
                    );
                    return Some(StatusCode::BAD_REQUEST.into_response());
                }
                Some(url) => match app_state.active_provider.get_next_provider(&params.input.name) {
                    Some(provider_cfg) => match get_stream_alternative_url(&url, params.input, &provider_cfg) {
                        Some(stream_url) => stream_url,
                        None => return Some(StatusCode::BAD_REQUEST.into_response()),
                    },
                    None => url.to_string(),
                },
            };
            let stream_url = match resolve_redirect_location(Some(params.input), &stream_url) {
                Ok(url) => url,
                Err(err) => {
                    error!("Failed to resolve redirect url: {}", sanitize_sensitive_info(&err.to_string()));
                    return Some(StatusCode::BAD_REQUEST.into_response());
                }
            };

            // hls or dash redirect
            if is_dash_request {
                let redirect_url = if is_hls_request {
                    &replace_url_extension(&stream_url, HLS_EXT)
                } else {
                    &replace_url_extension(&stream_url, DASH_EXT)
                };
                debug_if_enabled!(
                    "Redirecting stream request to {}",
                    sanitize_sensitive_info(resolve_request_url_for_logging(params.input, redirect_url).as_ref())
                );
                return Some(redirect(redirect_url).into_response());
            }

            debug_if_enabled!(
                "Redirecting stream request to {}",
                sanitize_sensitive_info(resolve_request_url_for_logging(params.input, stream_url.as_ref()).as_ref())
            );
            return Some(redirect(stream_url.as_ref()).into_response());
        }
    }

    None
}
