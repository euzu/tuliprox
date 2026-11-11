use crate::{
    api::{
        api_utils::{
            failover_group::{
                acquire_group_member, record_member_failure, record_member_success, resolve_hls_group_entry,
                GroupSelectionFailure, GroupSelectionRequest, ResolvedGroupPlayback, SelectionMode,
            },
            member_playback_url, select_provider_stream_url,
        },
        model::AppState,
    },
    model::{ConfigInput, ConfigTarget, InputSource, ProviderConfig},
    processing::parser::hls::{classify_hls_playlist, HlsPlaylistKind},
};
use axum::http::HeaderMap;
use std::{sync::Arc, time::Duration};
use tuliprox_repository::{failover_group_repository::GroupMember, FailoverGroupRecord};
use tuliprox_session::failover_group::FailureClass;
use url::Url;

/// Probed manifest result: stores the content and response metadata of the verified manifest.
#[derive(Debug, Clone)]
pub(in crate::api::endpoints::hls_api) struct ProbedManifest {
    pub content: String,
    pub request_url: String,
    pub response_url: String,
    pub response_headers: HeaderMap,
    pub playlist_kind: HlsPlaylistKind,
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
    let origin_member_url = super::routing::resolve_hls_origin_playlist_url(
        app_state,
        target,
        input,
        member.member_vid.get(),
        raw_member_url.as_ref(),
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
    Some(super::legacy::ensure_hls_manifest_extension(&normalized_url))
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
    if let Some(status) = super::legacy::hls_manifest_error_status(&err_msg) {
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
pub(in crate::api::endpoints::hls_api) async fn probe_selected_manifest(
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
    resolved: &ResolvedGroupPlayback,
    request_headers: &HeaderMap,
    deadline: tokio::time::Instant,
) -> Result<ProbedManifest, FailureClass> {
    let cfg = resolved.acquired.handle.handle().and_then(|handle| handle.allocation.get_provider_config());
    let allocated = cfg.as_ref().map_or_else(|| Arc::clone(&resolved.input.name), |cfg| Arc::clone(&cfg.name));

    let disabled = app_state.get_disabled_headers();
    let default_agent = app_state.app_config.config.load().default_user_agent.clone();
    let headers = super::legacy::build_hls_manifest_request_headers(
        &resolved.input.headers,
        request_headers,
        disabled.as_ref(),
        default_agent.as_deref(),
        resolved.member.upstream_user_agent.as_deref(),
    );

    let probe_work = async {
        let initial_url = build_hls_group_member_origin_url(
            app_state,
            target,
            &resolved.input,
            &resolved.member,
            &allocated,
            cfg.as_ref(),
        )
        .await
        .ok_or(FailureClass::Transport)?;

        let mut source = InputSource::for_request_url(&resolved.input, initial_url.clone());
        source.headers.retain(|name, _| !name.eq_ignore_ascii_case("user-agent"));
        let (content, response_url, response_headers) =
            super::legacy::download_legacy_hls_manifest(app_state, &source, &headers)
                .await
                .map_err(|err| classify_hls_download_error(&err))?;

        match classify_hls_playlist(&content) {
            HlsPlaylistKind::Invalid => Err(FailureClass::Stream),
            HlsPlaylistKind::Media => {
                if media_playlist_has_segments(&content) {
                    Ok(ProbedManifest {
                        content,
                        request_url: initial_url,
                        response_url,
                        response_headers,
                        playlist_kind: HlsPlaylistKind::Media,
                    })
                } else {
                    Err(FailureClass::Stream)
                }
            }
            HlsPlaylistKind::Master => {
                let first_variant_ref = master_playlist_first_variant_uri(&content).ok_or(FailureClass::Stream)?;
                let base = Url::parse(&response_url).map_err(|_| FailureClass::Stream)?;
                let variant_url = base.join(first_variant_ref).map_err(|_| FailureClass::Stream)?.to_string();

                let mut variant_source = InputSource::for_request_url(&resolved.input, variant_url);
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
                        request_url: initial_url,
                        response_url,
                        response_headers,
                        playlist_kind: HlsPlaylistKind::Master,
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
pub(in crate::api::endpoints::hls_api) async fn resolve_checked_hls_group_entry(
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
    group: Arc<FailoverGroupRecord>,
    req: GroupSelectionRequest<'_>,
    request_headers: &HeaderMap,
) -> Result<(ResolvedGroupPlayback, Option<ProbedManifest>), GroupSelectionFailure> {
    if !matches!(req.mode, SelectionMode::Live) {
        let resolved = resolve_hls_group_entry(app_state, group, req).await?;
        return Ok((resolved, None));
    }

    let started = tokio::time::Instant::now();
    let attempt_budget = Duration::from_millis(app_state.hls.proxy.origin_manifest_timeout_ms().max(1));
    let total_deadline = (req.start.timeout_secs != 0).then(|| started + Duration::from_secs(req.start.timeout_secs));
    let mut exclude: Vec<usize> = req.exclude.to_vec();

    for attempt in 0..req.start.max_attempts.max(1) {
        if total_deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
            break;
        }

        let selection = GroupSelectionRequest {
            mode: req.mode.clone(),
            exclude: &exclude,
            prefer: if attempt == 0 { req.prefer } else { None },
            ..req
        };

        let selecting = acquire_group_member(app_state, selection);
        let mut acquired = if let Some(deadline) = total_deadline {
            match tokio::time::timeout_at(deadline, selecting).await {
                Ok(Ok(acquired)) => acquired,
                Ok(Err(err)) => return Err(err),
                Err(_) => return Err(GroupSelectionFailure::Exhausted),
            }
        } else {
            selecting.await?
        };

        let member = group.members[acquired.index].clone();
        let Some(member_input) = app_state.app_config.get_input_by_name(&member.input_name) else {
            record_member_failure(&app_state.member_health, &member, acquired.half_open.take(), FailureClass::Stream);
            drop(acquired.handle);
            exclude.push(acquired.index);
            continue;
        };

        let stream_url = Arc::clone(&member.url);
        let mut resolved = ResolvedGroupPlayback { acquired, member, input: member_input, stream_url };

        let deadline = total_deadline.map_or_else(
            || tokio::time::Instant::now() + attempt_budget,
            |d| d.min(tokio::time::Instant::now() + attempt_budget),
        );

        match probe_selected_manifest(app_state, target, &resolved, request_headers, deadline).await {
            Ok(probed) => {
                record_member_success(&app_state.member_health, &resolved.member, resolved.acquired.half_open.take());
                return Ok((resolved, Some(probed)));
            }
            Err(class) => {
                record_member_failure(
                    &app_state.member_health,
                    &resolved.member,
                    resolved.acquired.half_open.take(),
                    class,
                );
                drop(resolved.acquired.handle);
                exclude.push(resolved.acquired.index);
            }
        }
    }

    Err(GroupSelectionFailure::Exhausted)
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

    #[test]
    fn probe_input_source_removes_user_agent_header() {
        let mut headers = std::collections::HashMap::new();
        headers.insert("User-Agent".to_string(), "InputAgent/1.0".to_string());
        headers.insert("Authorization".to_string(), "Bearer token".to_string());
        let mut source = InputSource {
            name: Arc::from("test"),
            url: "http://example.com/playlist.m3u8".to_string(),
            provider: None,
            username: None,
            password: None,
            method: shared::model::InputFetchMethod::GET,
            headers,
        };
        source.headers.retain(|name, _| !name.eq_ignore_ascii_case("user-agent"));
        assert!(!source.headers.contains_key("User-Agent"));
        assert_eq!(source.headers.get("Authorization").map(String::as_str), Some("Bearer token"));
    }
}
