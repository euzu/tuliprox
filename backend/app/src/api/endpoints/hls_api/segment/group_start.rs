use crate::{
    api::{
        api_utils::{
            failover_group::{
                member_needs_stalker_resolution, resolve_hls_group_entry, resolve_hls_group_entry_with_probe,
                GroupSelectionFailure, GroupSelectionRequest, ResolvedGroupPlayback, SelectionMode,
            },
            member_playback_url, resolve_stalker_stream_for_account, select_provider_stream_url, stalker_stream_kind,
        },
        model::AppState,
    },
    model::{ConfigInput, ConfigTarget, InputSource, ProviderConfig},
    processing::parser::hls::{classify_hls_playlist, HlsPlaylistKind},
};
use axum::http::HeaderMap;
use std::sync::Arc;
use tuliprox_repository::{failover_group_repository::GroupMember, FailoverGroupRecord};
use tuliprox_session::failover_group::FailureClass;
use url::Url;

/// Probed manifest result: stores the content and response metadata of the verified manifest.
#[derive(Debug)]
pub(in crate::api::endpoints::hls_api) struct ProbedManifest {
    pub content: String,
    pub request_url: String,
    pub response_url: String,
    pub response_headers: HeaderMap,
    pub playlist_kind: HlsPlaylistKind,
    pub user_agent_stream_index: Option<u64>,
}

/// Normalizes and formats the origin playlist URL for an HLS member.
pub(in crate::api::endpoints::hls_api) async fn format_member_origin_url(
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
    input: &Arc<ConfigInput>,
    member: &GroupMember,
    raw_member_url: &str,
    selected_provider_config: Option<&Arc<ProviderConfig>>,
) -> String {
    let origin_member_url = super::routing::resolve_hls_origin_playlist_url(
        app_state,
        target,
        input,
        member.member_vid.get(),
        raw_member_url,
    )
    .await
    .unwrap_or_else(|_| raw_member_url.to_string());

    let mapped_url = if let Some(cfg) = selected_provider_config {
        select_provider_stream_url(&origin_member_url, input, cfg, false, &app_state.app_config)
            .await
            .map_or_else(|| origin_member_url.clone(), |(_, u)| u)
    } else {
        origin_member_url
    };

    let normalized_url = super::legacy::normalize_xtream_live_hls_url(&mapped_url, input);
    super::legacy::ensure_hls_manifest_extension(&normalized_url)
}

/// Builds the origin playlist URL for an HLS failover group member.
/// Shared by the cached branch, the legacy branch, and the start probe.
pub(in crate::api::endpoints::hls_api) async fn build_hls_group_member_origin_url(
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
    input: &Arc<ConfigInput>,
    member: &GroupMember,
    allocated_provider: &Arc<str>,
    selected_provider_config: Option<&Arc<ProviderConfig>>,
) -> Option<String> {
    let raw_member_url = member_playback_url(app_state, input, member, allocated_provider).await?;
    Some(
        format_member_origin_url(app_state, target, input, member, raw_member_url.as_ref(), selected_provider_config)
            .await,
    )
}

/// Checks if an active shared cache session already committed an origin manifest for the member.
/// Only committed sessions provide verified evidence that the member origin works, justifying
/// skipping the probe and recording member success.
pub(in crate::api::endpoints::hls_api) async fn has_active_shared_cache_session(
    app_state: &Arc<AppState>,
    input_id: u16,
    stream_ref: &str,
) -> bool {
    let session_key = tuliprox_hls::api::HlsSessionKey::new(input_id, stream_ref);
    let Some(session) = app_state.hls.proxy.sessions().get_by_key(&session_key).await else {
        return false;
    };
    let s = session.read().await;
    s.origin_manifest_commits() > 0
}

/// Whether a media playlist has at least one segment URI line.
pub(in crate::api::endpoints::hls_api) fn media_playlist_has_segments(body: &str) -> bool {
    body.lines().map(str::trim).any(|line| !line.is_empty() && !line.starts_with('#'))
}

/// Finds the URI line of the first variant in a master playlist.
pub(in crate::api::endpoints::hls_api) fn master_playlist_first_variant_uri(body: &str) -> Option<&str> {
    body.lines().map(str::trim).find(|line| !line.is_empty() && !line.starts_with('#'))
}

fn is_transport_error_kind(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::NetworkUnreachable
            | std::io::ErrorKind::HostUnreachable
            | std::io::ErrorKind::NetworkDown
    )
}

fn inspect_typed_error(error: &std::io::Error) -> Option<FailureClass> {
    if is_transport_error_kind(error.kind()) {
        return Some(FailureClass::Transport);
    }
    let mut source: &(dyn std::error::Error + 'static) = error.get_ref()?;
    loop {
        if let Some(req_err) = source.downcast_ref::<reqwest::Error>() {
            if let Some(status) = req_err.status() {
                return Some(FailureClass::from_status(status));
            }
            if req_err.is_timeout() || req_err.is_connect() {
                return Some(FailureClass::Transport);
            }
        }
        if let Some(io_err) = source.downcast_ref::<std::io::Error>() {
            if is_transport_error_kind(io_err.kind()) {
                return Some(FailureClass::Transport);
            }
        }
        source = source.source()?;
    }
}

/// Classifies an error from manifest download into a `FailureClass` matching `open_failure_class`.
pub(in crate::api::endpoints::hls_api) fn classify_hls_download_error(err: &std::io::Error) -> FailureClass {
    if let Some(class) = inspect_typed_error(err) {
        return class;
    }
    if crate::utils::content_coding::is_http_body_transport_error(err) {
        return FailureClass::Transport;
    }

    let err_msg = err.to_string();
    if let Some(status) = crate::utils::request::request_failed_status(&err_msg) {
        FailureClass::from_status(status)
    } else if is_transport_error_message(&err_msg) {
        FailureClass::Transport
    } else {
        FailureClass::Stream
    }
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.as_bytes().windows(needle.len()).any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn is_transport_error_message(msg: &str) -> bool {
    const PATTERNS: &[&str] = &[
        "timeout",
        "timed out",
        "connection reset",
        "connection refused",
        "connect error",
        "broken pipe",
        "network unreachable",
    ];
    PATTERNS.iter().any(|&pattern| contains_ignore_ascii_case(msg, pattern))
}

/// Downloads and validates the selected member's manifest within the attempt deadline.
/// Master playlists are followed once to verify that the first variant is a valid media playlist
/// with at least one segment URI.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(in crate::api::endpoints::hls_api) async fn probe_selected_manifest(
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
    input: &Arc<ConfigInput>,
    member: &GroupMember,
    selected_provider_config: Option<&Arc<ProviderConfig>>,
    request_headers: &HeaderMap,
    map_account: bool,
    user_agent_stream_index: Option<u64>,
    deadline: tokio::time::Instant,
) -> Result<ProbedManifest, FailureClass> {
    let cfg = selected_provider_config;
    let allocated = cfg.as_ref().map_or_else(|| Arc::clone(&input.name), |cfg| Arc::clone(&cfg.name));

    let disabled = app_state.get_disabled_headers();
    let default_agent = app_state.app_config.config.load().default_user_agent.clone();
    let mut headers = super::legacy::build_hls_manifest_request_headers(
        &input.headers,
        request_headers,
        disabled.as_ref(),
        default_agent.as_deref(),
        member.upstream_user_agent.as_deref(),
    );
    if let Some(stream_index) = user_agent_stream_index {
        crate::utils::request::append_user_agent_stream_index(&mut headers, stream_index);
    }

    let probe_work = async {
        let initial_url = build_hls_group_member_origin_url(
            app_state,
            target,
            input,
            member,
            &allocated,
            if map_account { cfg } else { None },
        )
        .await
        .ok_or(FailureClass::Transport)?;

        let mut source = InputSource::for_request_url(input, initial_url.clone());
        source.headers.retain(|name, _| !name.eq_ignore_ascii_case("user-agent"));
        let mut download_result = super::legacy::download_legacy_hls_manifest(app_state, &source, &headers).await;
        let mut playlist_kind =
            download_result.as_ref().map_or(HlsPlaylistKind::Invalid, |(content, _, _)| classify_hls_playlist(content));

        let mut final_request_url = initial_url.clone();
        let is_rejected = super::entry::hls_download_rejects_link(download_result.as_ref().map(|_| playlist_kind));

        if is_rejected && member_needs_stalker_resolution(input, &member.url, &allocated) {
            if let Some((_, fresh_raw_url)) = resolve_stalker_stream_for_account(
                app_state,
                input,
                &allocated,
                member.provider_id,
                stalker_stream_kind(member.cluster, member.item_type),
                true,
            )
            .await
            {
                let fresh_url = format_member_origin_url(
                    app_state,
                    target,
                    input,
                    member,
                    &fresh_raw_url,
                    if map_account { cfg } else { None },
                )
                .await;
                if fresh_url != initial_url {
                    let mut fresh_source = InputSource::for_request_url(input, fresh_url.clone());
                    fresh_source.headers.retain(|name, _| !name.eq_ignore_ascii_case("user-agent"));
                    download_result =
                        super::legacy::download_legacy_hls_manifest(app_state, &fresh_source, &headers).await;
                    playlist_kind = download_result
                        .as_ref()
                        .map_or(HlsPlaylistKind::Invalid, |(content, _, _)| classify_hls_playlist(content));
                    final_request_url = fresh_url;
                }
            }
        }

        let (content, response_url, response_headers) =
            download_result.map_err(|err| classify_hls_download_error(&err))?;

        match playlist_kind {
            HlsPlaylistKind::Invalid => Err(FailureClass::Stream),
            HlsPlaylistKind::Media => {
                if media_playlist_has_segments(&content) {
                    Ok(ProbedManifest {
                        content,
                        request_url: final_request_url,
                        response_url,
                        response_headers,
                        playlist_kind: HlsPlaylistKind::Media,
                        user_agent_stream_index,
                    })
                } else {
                    Err(FailureClass::Stream)
                }
            }
            HlsPlaylistKind::Master => {
                let first_variant_ref = master_playlist_first_variant_uri(&content).ok_or(FailureClass::Stream)?;
                let base = Url::parse(&response_url).map_err(|_| FailureClass::Stream)?;
                let variant_url = base.join(first_variant_ref).map_err(|_| FailureClass::Stream)?.to_string();

                let mut variant_source = InputSource::for_request_url(input, variant_url);
                variant_source.headers.retain(|name, _| !name.eq_ignore_ascii_case("user-agent"));
                let (variant_content, _, _) =
                    super::legacy::download_legacy_hls_manifest(app_state, &variant_source, &headers)
                        .await
                        .map_err(|err| classify_hls_download_error(&err))?;

                if classify_hls_playlist(&variant_content) == HlsPlaylistKind::Media
                    && media_playlist_has_segments(&variant_content)
                {
                    Ok(ProbedManifest {
                        content,
                        request_url: final_request_url,
                        response_url,
                        response_headers,
                        playlist_kind: HlsPlaylistKind::Master,
                        user_agent_stream_index,
                    })
                } else {
                    Err(FailureClass::Stream)
                }
            }
        }
    };

    match tokio::time::timeout_at(deadline, probe_work).await {
        Ok(result) => result,
        Err(_) => Err(FailureClass::Transport),
    }
}

/// Resolves a live HLS failover group entry with manifest start probing and failover.
/// For non-live selection modes (e.g. archive), probing is bypassed.
#[allow(clippy::too_many_arguments)]
pub(in crate::api::endpoints::hls_api) async fn resolve_checked_hls_group_entry(
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
    group: Arc<FailoverGroupRecord>,
    req: GroupSelectionRequest<'_>,
    request_headers: &HeaderMap,
    map_account: bool,
    user_session: Option<(&str, &str)>,
    check_active_cache: bool,
) -> Result<(ResolvedGroupPlayback, Option<ProbedManifest>), GroupSelectionFailure> {
    if !matches!(req.mode, SelectionMode::Live) {
        let resolved = resolve_hls_group_entry(app_state, group, req).await?;
        return Ok((resolved, None));
    }

    resolve_hls_group_entry_with_probe(
        app_state,
        group,
        req,
        Some(|resolved: &ResolvedGroupPlayback, deadline| {
            let input = Arc::clone(&resolved.input);
            let member = resolved.member.clone();
            let selected_provider_config =
                resolved.acquired.handle.handle().and_then(|handle| handle.allocation.get_provider_config());
            let stream_ref = Arc::clone(&member.input_stream_id);

            async move {
                if check_active_cache && has_active_shared_cache_session(app_state, input.id, stream_ref.as_ref()).await
                {
                    return Ok(None);
                }

                let user_agent_stream_index = if let Some((username, session_token)) = user_session {
                    crate::api::api_utils::resolve_stream_user_agent_index(
                        app_state,
                        &input,
                        selected_provider_config.is_some(),
                        username,
                        Some(session_token),
                    )
                    .await
                } else {
                    None
                };

                probe_selected_manifest(
                    app_state,
                    target,
                    &input,
                    &member,
                    selected_provider_config.as_ref(),
                    request_headers,
                    map_account,
                    user_agent_stream_index,
                    deadline,
                )
                .await
                .map(Some)
            }
        }),
    )
    .await
    .map(|(resolved, probed)| (resolved, probed.flatten()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_empty_manifest_is_invalid() {
        assert_eq!(classify_hls_playlist(""), HlsPlaylistKind::Invalid);
        assert!(!media_playlist_has_segments(""));
    }

    #[test]
    fn parser_extm3u_only_is_invalid() {
        assert_eq!(classify_hls_playlist("#EXTM3U\n"), HlsPlaylistKind::Invalid);
        assert!(!media_playlist_has_segments("#EXTM3U\n"));
    }

    #[test]
    fn parser_media_without_segments_is_detected() {
        let body = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:10\n";
        assert_eq!(classify_hls_playlist(body), HlsPlaylistKind::Media);
        assert!(!media_playlist_has_segments(body));
    }

    #[test]
    fn parser_media_with_segments_is_valid() {
        let body = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\nsegment1.ts\n";
        assert_eq!(classify_hls_playlist(body), HlsPlaylistKind::Media);
        assert!(media_playlist_has_segments(body));
    }

    #[test]
    fn parser_master_with_relative_uri_and_query() {
        let body = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1280000,RESOLUTION=1280x720\nvariant.m3u8?token=abc&auth=1\n";
        assert_eq!(classify_hls_playlist(body), HlsPlaylistKind::Master);
        let uri = master_playlist_first_variant_uri(body).expect("variant uri");
        assert_eq!(uri, "variant.m3u8?token=abc&auth=1");

        let base = Url::parse("http://example.com/stream/index.m3u8").expect("base url");
        let joined = base.join(uri).expect("joined url");
        assert_eq!(joined.as_str(), "http://example.com/stream/variant.m3u8?token=abc&auth=1");
    }

    #[test]
    fn classify_download_error_typed_io_errors() {
        let err = std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out");
        assert_eq!(classify_hls_download_error(&err), FailureClass::Transport);

        let err = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        assert_eq!(classify_hls_download_error(&err), FailureClass::Transport);

        let err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        assert_eq!(classify_hls_download_error(&err), FailureClass::Transport);

        let err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken pipe");
        assert_eq!(classify_hls_download_error(&err), FailureClass::Transport);
    }

    #[test]
    fn classify_download_error_http_status_codes() {
        let err = shared::error::string_to_io_error("Request failed (401 Unauthorized): http://example.com");
        assert_eq!(classify_hls_download_error(&err), FailureClass::Auth);

        let err = shared::error::string_to_io_error("Request failed (403 Forbidden): http://example.com");
        assert_eq!(classify_hls_download_error(&err), FailureClass::Auth);

        let err = shared::error::string_to_io_error("Request failed (429 Too Many Requests): http://example.com");
        assert_eq!(classify_hls_download_error(&err), FailureClass::Capacity);

        let err = shared::error::string_to_io_error("Request failed (500 Internal Server Error): http://example.com");
        assert_eq!(classify_hls_download_error(&err), FailureClass::Stream);

        let err = shared::error::string_to_io_error("Request failed (404 Not Found): http://example.com");
        assert_eq!(classify_hls_download_error(&err), FailureClass::Stream);
    }

    #[test]
    fn classify_download_error_string_fallback() {
        let err = shared::error::string_to_io_error("connect error: dial tcp 1.2.3.4:80");
        assert_eq!(classify_hls_download_error(&err), FailureClass::Transport);

        let err = shared::error::string_to_io_error("some unknown stream parsing failure");
        assert_eq!(classify_hls_download_error(&err), FailureClass::Stream);
    }
}
