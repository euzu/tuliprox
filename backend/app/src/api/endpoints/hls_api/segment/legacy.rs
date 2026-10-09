use crate::{
    api::{
        api_utils::{get_headers_from_request, HeaderFilter},
        model::AppState,
    },
    model::{ConfigInput, ConfigInputFlags, InputSource, ReverseProxyDisabledHeaderConfig},
    utils::{content_coding::OutboundContentCodingPolicy, request},
};
use axum::http::HeaderMap;
use shared::{defaults::HLS_EXT, utils::replace_url_extension};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tuliprox_core::model::{PlaybackRequestId, PlaybackRequestOutcome, ProviderBindingTag};
use tuliprox_hls::{
    api::{
        force_identity_without_range, scrub_hls_origin_headers, should_remove_hls_origin_header, MAX_HLS_MANIFEST_BYTES,
    },
    MAX_MANUAL_REDIRECTS,
};
use url::Url;

pub(in crate::api::endpoints::hls_api) async fn release_prepared_hls_manifest_session(
    app_state: &Arc<AppState>,
    username: &str,
    session_token: &str,
    addr: &std::net::SocketAddr,
) {
    let _transition_guard = app_state.active_users.acquire_playback_transition(username, session_token).await;
    app_state.active_users.release_unbound_session_reservation(username, session_token, None, false).await;
    app_state.active_users.clear_unbound_session_addr(username, session_token, addr).await;
}

pub(in crate::api::endpoints::hls_api) async fn terminate_failed_hls_manifest_session(
    app_state: &Arc<AppState>,
    username: &str,
    session_token: &str,
    provider_name: Option<&Arc<str>>,
    binding_tag: Option<ProviderBindingTag>,
    request_id: Option<PlaybackRequestId>,
) {
    let _transition_guard = app_state.active_users.acquire_playback_transition(username, session_token).await;
    app_state.active_users.terminate_session(username, session_token).await;
    let mut cleared = false;
    if let Some(request_id) = request_id {
        app_state.active_provider.finish_identified_playback_request(
            session_token,
            request_id,
            PlaybackRequestOutcome::ProviderFailed,
        );
        cleared = true;
    } else if let (Some(provider_name), Some(binding_tag)) = (provider_name, binding_tag) {
        // A failed manifest lets the next entry fall back to another provider, but only
        // when the preference still belongs to this binding.
        app_state.active_provider.forget_identified_provider_affinity(session_token, provider_name, binding_tag);
        app_state.active_provider.clear_identified_provider_reservation(
            session_token,
            provider_name,
            Some(binding_tag),
        );
        cleared = true;
    }
    if !cleared {
        app_state.active_provider.clear_provider_reservation(session_token);
    }
}

pub(in crate::api::endpoints::hls_api) fn normalize_xtream_live_hls_url(hls_url: &str, input: &ConfigInput) -> String {
    if !input.input_type.is_xtream() || !input.has_flag(ConfigInputFlags::XtreamLiveStreamUsePrefix) {
        return hls_url.to_string();
    }

    let (Some(username), Some(password)) = (input.username.as_deref(), input.password.as_deref()) else {
        return hls_url.to_string();
    };

    let Ok(mut parsed) = Url::parse(hls_url) else {
        return hls_url.to_string();
    };
    let Some(segments) = parsed.path_segments() else {
        return hls_url.to_string();
    };

    let parts: Vec<&str> = segments.collect();
    if parts.len() >= 3 && parts[0] == username && parts[1] == password {
        parsed.set_path(&format!("/live/{}", parts.join("/")));
        return parsed.to_string();
    }

    hls_url.to_string()
}

pub(in crate::api::endpoints::hls_api) fn ensure_hls_manifest_extension(url: &str) -> String {
    let with_extension = replace_url_extension(url, HLS_EXT);
    let (base_url, suffix) = match with_extension.find(['?', '#'].as_ref()) {
        Some(pos) => (&with_extension[..pos], &with_extension[pos..]),
        None => (with_extension.as_str(), ""),
    };
    let Some(path_without_ext) = base_url.strip_suffix(HLS_EXT) else {
        return with_extension;
    };
    format!("{}{}{}", path_without_ext.trim_end_matches('.'), HLS_EXT, suffix)
}

pub(in crate::api::endpoints::hls_api) fn build_hls_manifest_request_headers(
    input_headers: &HashMap<String, String>,
    req_headers: &HeaderMap,
    disabled_headers: Option<&ReverseProxyDisabledHeaderConfig>,
    default_user_agent: Option<&str>,
    upstream_user_agent: Option<&str>,
) -> HeaderMap {
    let input_headers = input_headers
        .iter()
        .filter(|(key, _)| !should_remove_hls_origin_header(key, disabled_headers))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<HashMap<_, _>>();
    let disabled_headers_for_filter = disabled_headers.cloned();
    let filter_header: HeaderFilter = Some(Box::new(move |name: &str| {
        !name.eq_ignore_ascii_case("range")
            && !should_remove_hls_origin_header(name, disabled_headers_for_filter.as_ref())
    }));
    let forwarded = get_headers_from_request(req_headers, &filter_header);
    let mut headers =
        request::get_request_headers(Some(&input_headers), Some(&forwarded), disabled_headers, default_user_agent);
    request::overlay_upstream_user_agent(&mut headers, upstream_user_agent, disabled_headers);
    scrub_hls_origin_headers(&mut headers, disabled_headers);
    force_identity_without_range(&mut headers);
    headers
}

pub(in crate::api::endpoints::hls_api) async fn download_legacy_hls_manifest(
    app_state: &Arc<AppState>,
    input: &InputSource,
    headers: &HeaderMap,
) -> Result<(String, String, HeaderMap), std::io::Error> {
    let deadline = Duration::from_millis(app_state.hls.proxy.origin_manifest_timeout_ms().max(1));
    let fetch_options = request::RequestFetchOptions::with_attempt_idle_timeout(deadline)
        .with_content_coding(OutboundContentCodingPolicy::Identity);
    let body_options = request::TextContentBodyOptions::hls_manifest(MAX_HLS_MANIFEST_BYTES, deadline);
    let options = request::TextContentFetchOptions::new(fetch_options, body_options);

    if app_state.should_use_manual_redirects() {
        request::download_text_content_with_manual_redirects_and_headers_and_options(
            &app_state.app_config,
            &app_state.http_clients.no_redirect.load(),
            input,
            Some(headers),
            false,
            MAX_MANUAL_REDIRECTS,
            options,
        )
        .await
    } else {
        request::download_text_content_with_headers_and_options(
            &app_state.app_config,
            &app_state.http_clients.default.load(),
            input,
            Some(headers),
            false,
            options,
        )
        .await
    }
}
