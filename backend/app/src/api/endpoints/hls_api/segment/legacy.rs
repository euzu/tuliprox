use crate::{
    api::{
        api_utils::{get_headers_from_request, HeaderFilter},
        endpoints::hls_api::owner_token::HlsRequestStage,
        model::AppState,
    },
    model::{ConfigInput, ConfigInputFlags, InputSource, ReverseProxyDisabledHeaderConfig},
    processing::parser::hls::{classify_hls_playlist, HlsManifestSource, HlsPlaylistKind},
    utils::{content_coding::OutboundContentCodingPolicy, request},
};
use axum::http::{header, HeaderMap};
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

/// Playlist body, final response URL, response headers and playlist kind of a manifest download.
pub(in crate::api::endpoints::hls_api) type ClassifiedHlsManifest = (String, String, HeaderMap, HlsPlaylistKind);

/// A parsed variant from an HLS master playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::api::endpoints::hls_api) struct MasterVariant<'a> {
    pub uri: &'a str,
    pub bandwidth: Option<u64>,
}

fn extract_stream_inf_bandwidth(rest: &str) -> Option<u64> {
    let attrs = crate::processing::parser::hls::origin_manifest::parse_attribute_list(rest)?;
    attrs.get("BANDWIDTH")?.parse::<u64>().ok()
}

/// Parses all `#EXT-X-STREAM-INF:` variants from a master playlist.
pub(in crate::api::endpoints::hls_api) fn parse_master_variants(body: &str) -> Vec<MasterVariant<'_>> {
    let mut variants = Vec::new();
    let mut pending_bandwidth = None;
    let mut in_stream_inf = false;

    for raw_line in body.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            in_stream_inf = true;
            pending_bandwidth = extract_stream_inf_bandwidth(rest);
        } else if !line.starts_with('#') {
            if in_stream_inf {
                variants.push(MasterVariant { uri: line, bandwidth: pending_bandwidth.take() });
                in_stream_inf = false;
            }
        } else if line.starts_with("#EXT") {
            in_stream_inf = false;
            pending_bandwidth = None;
        }
    }
    variants
}

/// Picks the variant URI from a master playlist:
/// - if single variant: that variant
/// - else if BANDWIDTH is closest to `target_bandwidth` (when known): that variant
/// - else: the first variant
pub(in crate::api::endpoints::hls_api) fn select_master_variant_uri(
    body: &str,
    target_bandwidth: Option<u64>,
) -> Option<&str> {
    let variants = parse_master_variants(body);
    if variants.is_empty() {
        return None;
    }
    if variants.len() == 1 {
        return Some(variants[0].uri);
    }
    if let Some(target_bw) = target_bandwidth {
        let closest = variants
            .iter()
            .filter_map(|v| v.bandwidth.map(|bw| (bw.abs_diff(target_bw), v.uri)))
            .min_by_key(|(diff, _)| *diff);
        if let Some((_, uri)) = closest {
            return Some(uri);
        }
    }
    Some(variants[0].uri)
}

/// Decision for a child manifest request based on origin account and group session context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::api::endpoints::hls_api) enum StaleChildDecision {
    /// Accept the requested sealed stream URL as-is (origin provider matches or not a child variant).
    Accept,
    /// Stale child manifest from an earlier member in a live legacy group session:
    /// re-resolve the bound member's entry URL instead of returning 404.
    ReResolveBoundMember,
    /// Stale child manifest from another provider in a non-group or non-live session:
    /// reject with 404.
    RejectNotFound,
}

/// Decides whether a child manifest request should be accepted as sealed, re-resolved for the
/// bound member of a switched live legacy group, or rejected with 404.
/// `selected_member_vid` is the member currently bound to the session; a child of another member
/// of the same provider is stale even when its provider matches. Tokens without a member id keep
/// the provider-only match.
pub(in crate::api::endpoints::hls_api) fn evaluate_child_manifest_source(
    stage: HlsRequestStage<'_>,
    selected_provider: &str,
    selected_member_vid: Option<u32>,
    is_live_legacy_group: bool,
) -> StaleChildDecision {
    match stage {
        HlsRequestStage::Variant { source: HlsManifestSource::Child, origin_provider, origin_member_vid } => {
            let member_matches = match (origin_member_vid, selected_member_vid) {
                (Some(origin), Some(bound)) => origin == bound,
                _ => true,
            };
            if origin_provider == Some(selected_provider) && member_matches {
                StaleChildDecision::Accept
            } else if is_live_legacy_group {
                StaleChildDecision::ReResolveBoundMember
            } else {
                StaleChildDecision::RejectNotFound
            }
        }
        _ => StaleChildDecision::Accept,
    }
}

/// Appends the stale master response's `Set-Cookie` headers to the served child's headers, so the
/// provider session keeps the cookies of both responses instead of only the child's.
pub(in crate::api::endpoints::hls_api) fn merge_master_response_cookies(
    mut child: ClassifiedHlsManifest,
    master_response_headers: &HeaderMap,
) -> ClassifiedHlsManifest {
    for value in master_response_headers.get_all(header::SET_COOKIE) {
        child.2.append(header::SET_COOKIE, value.clone());
    }
    child
}

/// When a stale child manifest request for a switched group session re-resolves the bound member's
/// entry URL and receives a master playlist, this selects the best variant, downloads that child
/// playlist and returns it as a classified manifest.
pub(in crate::api::endpoints::hls_api) async fn resolve_stale_child_master_variant(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    master_content: &str,
    master_response_url: &str,
    master_response_headers: &HeaderMap,
    headers: &HeaderMap,
    target_bandwidth: Option<u64>,
) -> Result<ClassifiedHlsManifest, std::io::Error> {
    let Some(variant_ref) = select_master_variant_uri(master_content, target_bandwidth) else {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "master playlist contains no variant URI"));
    };
    let base_url =
        Url::parse(master_response_url).map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
    let variant_url = base_url
        .join(variant_ref)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?
        .to_string();

    let mut variant_source = InputSource::for_request_url(input, variant_url);
    variant_source.headers.retain(|name, _| !name.eq_ignore_ascii_case("user-agent"));
    let variant_headers =
        super::group_start::variant_probe_headers(headers, &base_url, master_response_headers, &mut variant_source);
    let (content, response_url, response_headers) =
        download_legacy_hls_manifest(app_state, &variant_source, &variant_headers).await?;
    let playlist_kind = classify_hls_playlist(&content);
    if playlist_kind == HlsPlaylistKind::Invalid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "child manifest response is not an HLS playlist",
        ));
    }
    Ok((content, response_url, response_headers, playlist_kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_master_variant_single_variant() {
        let body = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1280000,RESOLUTION=1280x720\nvariant.m3u8\n";
        assert_eq!(select_master_variant_uri(body, None), Some("variant.m3u8"));
        assert_eq!(select_master_variant_uri(body, Some(5_000_000)), Some("variant.m3u8"));
    }

    #[test]
    fn select_master_variant_closest_bandwidth() {
        let body = "#EXTM3U\n\
            #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\nlow.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1280x720\nmid.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080\nhigh.m3u8\n";

        // Closer to 2500000 than 800000 or 5000000
        assert_eq!(select_master_variant_uri(body, Some(2_200_000)), Some("mid.m3u8"));
        // Closer to 5000000
        assert_eq!(select_master_variant_uri(body, Some(4_500_000)), Some("high.m3u8"));
        // Closer to 800000
        assert_eq!(select_master_variant_uri(body, Some(500_000)), Some("low.m3u8"));
    }

    #[test]
    fn select_master_variant_no_bandwidth_selects_first() {
        // Multiple variants with bandwidth, but target_bandwidth is None => first
        let body_with_bw = "#EXTM3U\n\
            #EXT-X-STREAM-INF:BANDWIDTH=800000\nlow.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=2500000\nmid.m3u8\n";
        assert_eq!(select_master_variant_uri(body_with_bw, None), Some("low.m3u8"));

        // Multiple variants without BANDWIDTH tag in manifest => first
        let body_no_bw = "#EXTM3U\n\
            #EXT-X-STREAM-INF:RESOLUTION=640x360\nlow.m3u8\n\
            #EXT-X-STREAM-INF:RESOLUTION=1280x720\nmid.m3u8\n";
        assert_eq!(select_master_variant_uri(body_no_bw, Some(2_000_000)), Some("low.m3u8"));
        assert_eq!(select_master_variant_uri(body_no_bw, None), Some("low.m3u8"));
    }

    #[test]
    fn select_master_variant_empty_playlist() {
        let body = "#EXTM3U\n";
        assert_eq!(select_master_variant_uri(body, Some(1_000_000)), None);
    }

    #[test]
    fn evaluate_child_manifest_source_decision() {
        let stale_child_stage = HlsRequestStage::Variant {
            source: HlsManifestSource::Child,
            origin_provider: Some("provider-a"),
            origin_member_vid: None,
        };
        // Live legacy group session with stale child => ReResolveBoundMember
        assert_eq!(
            evaluate_child_manifest_source(stale_child_stage, "provider-b", None, true),
            StaleChildDecision::ReResolveBoundMember
        );
        // Non-group or non-live session with stale child => RejectNotFound
        assert_eq!(
            evaluate_child_manifest_source(stale_child_stage, "provider-b", None, false),
            StaleChildDecision::RejectNotFound
        );
        // Matching provider => Accept
        assert_eq!(
            evaluate_child_manifest_source(stale_child_stage, "provider-a", None, true),
            StaleChildDecision::Accept
        );
        assert_eq!(
            evaluate_child_manifest_source(stale_child_stage, "provider-a", None, false),
            StaleChildDecision::Accept
        );

        // Entry or other variant stages => Accept (not a child variant stage)
        let entry_stage = HlsRequestStage::Entry;
        assert_eq!(evaluate_child_manifest_source(entry_stage, "provider-b", None, true), StaleChildDecision::Accept);
        let entry_variant_stage = HlsRequestStage::Variant {
            source: HlsManifestSource::Entry,
            origin_provider: Some("provider-a"),
            origin_member_vid: None,
        };
        assert_eq!(
            evaluate_child_manifest_source(entry_variant_stage, "provider-b", None, true),
            StaleChildDecision::Accept
        );
    }

    #[test]
    fn evaluate_child_manifest_source_distinguishes_members_of_the_same_provider() {
        // Two failover members of the same provider: only the bound member's child is current.
        let child_of_member = |member_vid| HlsRequestStage::Variant {
            source: HlsManifestSource::Child,
            origin_provider: Some("provider-a"),
            origin_member_vid: member_vid,
        };

        assert_eq!(
            evaluate_child_manifest_source(child_of_member(Some(1)), "provider-a", Some(2), true),
            StaleChildDecision::ReResolveBoundMember,
            "a child of the previous member must re-resolve the bound member"
        );
        assert_eq!(
            evaluate_child_manifest_source(child_of_member(Some(2)), "provider-a", Some(2), true),
            StaleChildDecision::Accept,
            "a child of the bound member stays accepted"
        );
        // Tokens sealed before member ids fall back to the provider-only match.
        assert_eq!(
            evaluate_child_manifest_source(child_of_member(None), "provider-a", Some(2), true),
            StaleChildDecision::Accept
        );
        // Non-group sessions keep the provider-only behavior.
        assert_eq!(
            evaluate_child_manifest_source(child_of_member(Some(1)), "provider-a", None, false),
            StaleChildDecision::Accept
        );
        assert_eq!(
            evaluate_child_manifest_source(child_of_member(Some(1)), "provider-b", None, false),
            StaleChildDecision::RejectNotFound
        );
    }

    #[test]
    fn stale_child_success_keeps_master_and_variant_cookies() {
        use tuliprox_hls::api::extract_hls_provider_session_headers;

        let mut variant_headers = HeaderMap::new();
        variant_headers.append(header::SET_COOKIE, "variant=1; Path=/".parse().expect("variant cookie"));
        let mut master_headers = HeaderMap::new();
        master_headers.append(header::SET_COOKIE, "master=1; Path=/".parse().expect("master cookie"));

        // Before the merge the success path persisted only the child response's cookies.
        let child_only = extract_hls_provider_session_headers(&variant_headers);
        assert!(!child_only.cookies.iter().any(|cookie| cookie.starts_with("master=1")));

        let served_child = merge_master_response_cookies(
            (String::new(), String::new(), variant_headers, HlsPlaylistKind::Media),
            &master_headers,
        );
        let persisted = extract_hls_provider_session_headers(&served_child.2);
        assert!(
            persisted.cookies.iter().any(|cookie| cookie.starts_with("variant=1")),
            "the child response cookies are persisted: {:?}",
            persisted.cookies
        );
        assert!(
            persisted.cookies.iter().any(|cookie| cookie.starts_with("master=1")),
            "the stale master response cookies are persisted too: {:?}",
            persisted.cookies
        );
        let cookie = persisted.headers.get(axum::http::header::COOKIE.as_str()).expect("persisted cookie header");
        assert!(cookie.contains("variant=1") && cookie.contains("master=1"), "{cookie}");
    }

    #[test]
    fn select_master_variant_ignores_media_and_iframe_tags() {
        let body = "#EXTM3U\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"English\",DEFAULT=YES,URI=\"audio.m3u8\"\n\
            #EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=860000,URI=\"iframe.m3u8\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=1280000,RESOLUTION=1280x720\n\
            video.m3u8\n";
        let variants = parse_master_variants(body);
        assert_eq!(variants.len(), 1);
        assert_eq!(variants[0].uri, "video.m3u8");
        assert_eq!(variants[0].bandwidth, Some(1_280_000));
        assert_eq!(select_master_variant_uri(body, None), Some("video.m3u8"));

        let media_and_iframe_only = "#EXTM3U\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"English\",URI=\"audio.m3u8\"\n\
            #EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=860000,URI=\"iframe.m3u8\"\n";
        assert_eq!(parse_master_variants(media_and_iframe_only), Vec::new());
        assert_eq!(select_master_variant_uri(media_and_iframe_only, None), None);

        // Lines without #EXT-X-STREAM-INF must not be picked as variants
        let unadorned_uris = "#EXTM3U\nvideo1.m3u8\nvideo2.m3u8\n";
        assert_eq!(parse_master_variants(unadorned_uris), Vec::new());
        assert_eq!(select_master_variant_uri(unadorned_uris, None), None);
    }
}
