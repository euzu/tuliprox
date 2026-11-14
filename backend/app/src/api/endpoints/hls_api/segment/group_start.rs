use crate::{
    api::{
        api_utils::{
            failover_group::{
                member_needs_stalker_resolution, member_stream_channel, resolve_hls_group_entry,
                resolve_hls_group_entry_with_probe, GroupSelectionFailure, GroupSelectionRequest,
                ResolvedGroupPlayback, SelectionMode,
            },
            member_playback_url, record_connect_failed_attempt, resolve_stalker_stream_for_account,
            select_provider_stream_url, stalker_stream_kind, ConnectFailedAttempt,
        },
        model::AppState,
    },
    auth::Fingerprint,
    model::{ConfigInput, ConfigTarget, InputSource, ProviderConfig},
    processing::parser::hls::{classify_hls_playlist, HlsPlaylistKind},
};
use axum::http::{header, HeaderMap, HeaderValue};
use shared::model::{ConnectFailureReason, FailureStage, StreamChannel};
use std::{borrow::Cow, sync::Arc};
use tuliprox_hls::api::{
    extract_hls_provider_session_headers, merge_hls_provider_session_headers, HlsOriginSource, HlsSessionKey,
};
use tuliprox_repository::{failover_group_repository::GroupMember, FailoverGroupRecord};
use tuliprox_session::{failover_group::FailureClass, ProviderSessionCookieStore};
use url::Url;

/// Request headers for the variant probe. Cookies the master response set are added only when the
/// provider cookie store admits them for the variant URL (same origin, path, domain and secure rules).
/// A configured input cookie is merged with them (master wins per name) and removed from
/// `variant_source`, because configured input headers take precedence over request headers.
fn variant_probe_headers<'a>(
    headers: &'a HeaderMap,
    master_url: &Url,
    master_response_headers: &HeaderMap,
    variant_source: &mut InputSource,
) -> Cow<'a, HeaderMap> {
    let mut store = ProviderSessionCookieStore::default();
    store.update(master_url, &extract_hls_provider_session_headers(master_response_headers));
    let Some(session_headers) = store.headers_for(&variant_source.url) else {
        return Cow::Borrowed(headers);
    };
    let mut master_cookie = HeaderMap::new();
    super::append_user_session_provider_headers(&mut master_cookie, &session_headers);

    let mut configured_cookie = HeaderMap::new();
    if let Some(cookie) = headers.get(header::COOKIE) {
        configured_cookie.insert(header::COOKIE, cookie.clone());
    }
    variant_source.headers.retain(|name, value| {
        if !name.eq_ignore_ascii_case(header::COOKIE.as_str()) {
            return true;
        }
        if let Ok(cookie) = HeaderValue::from_str(value) {
            configured_cookie.insert(header::COOKIE, cookie);
        }
        false
    });

    let mut variant_headers = headers.clone();
    if let Some(cookie) = merge_hls_provider_session_headers(&configured_cookie, &master_cookie).remove(header::COOKIE)
    {
        variant_headers.insert(header::COOKIE, cookie);
    }
    Cow::Owned(variant_headers)
}

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

/// Checks if a live failover-group shared cache session is currently reusable.
/// The session is keyed by the logical group (`target_id` + `group_vid`), not by a member.
/// Only sessions with committed origin manifests and currently healthy origin paths
/// (no consecutive refresh failures, origin path not degraded, and no publication lateness)
/// provide verified evidence that the bound member origin works, justifying skipping the probe
/// and recording member success.
pub(in crate::api::endpoints::hls_api) async fn has_active_shared_cache_session(
    app_state: &Arc<AppState>,
    target_id: u16,
    group_vid: u32,
) -> bool {
    let session_key = HlsSessionKey::for_group(target_id, group_vid);
    let Some(session) = app_state.hls.proxy.sessions().get_by_key(&session_key).await else {
        return false;
    };
    let s = session.read().await;
    s.origin_reusable_at(app_state.hls.proxy.now_ms())
}

/// The bound member origin and its entry URL of an existing live failover-group shared cache
/// session. The session is keyed by the logical group, so viewers joining the group all reuse the
/// same bound member regardless of the member they entered through.
pub(in crate::api::endpoints::hls_api) async fn shared_cache_group_session_origin(
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
    group: &FailoverGroupRecord,
    group_vid: u32,
) -> Option<(HlsOriginSource, String)> {
    let session_key = HlsSessionKey::for_group(target.id, group_vid);
    let session = app_state.hls.proxy.sessions().get_by_key(&session_key).await?;
    let (origin_source, stored_entry_url) = {
        let session = session.read().await;
        let origin_source = session.origin_source.clone();
        let stored_entry_url = session
            .origin_account_runtime
            .as_ref()
            .and_then(|runtime| runtime.entry_url().or_else(|| runtime.raw_request_url()))
            .filter(|url| !url.is_empty());
        (origin_source, stored_entry_url)
    };

    let entry_url = if let Some(url) = stored_entry_url {
        url
    } else {
        let input = app_state.app_config.get_input_by_name(&origin_source.input_name)?;
        let member = group.members.iter().find(|member| {
            member.input_name.as_ref() == origin_source.input_name.as_ref()
                && member.input_stream_id.as_ref() == origin_source.stream_ref.as_str()
        })?;
        let account = Arc::clone(&origin_source.input_name);
        build_hls_group_member_origin_url(app_state, target, &input, member, &account, None).await?
    };
    Some((origin_source, entry_url))
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
                let variant_headers = variant_probe_headers(&headers, &base, &response_headers, &mut variant_source);
                let (variant_content, _, _) =
                    super::legacy::download_legacy_hls_manifest(app_state, &variant_source, &variant_headers)
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
    fingerprint: &Fingerprint,
) -> Result<(ResolvedGroupPlayback, Option<ProbedManifest>), GroupSelectionFailure> {
    if !matches!(req.mode, SelectionMode::Live) {
        let resolved = resolve_hls_group_entry(app_state, group, req).await?;
        return Ok((resolved, None));
    }

    let username = req.username;
    let group_for_connect_failed = Arc::clone(&group);

    resolve_hls_group_entry_with_probe(
        app_state,
        group,
        req,
        Some({
            let group = &group_for_connect_failed;
            move |resolved: &ResolvedGroupPlayback, deadline| {
                let input = Arc::clone(&resolved.input);
                let member = resolved.member.clone();
                let selected_provider_config =
                    resolved.acquired.handle.handle().and_then(|handle| handle.allocation.get_provider_config());

                async move {
                    if check_active_cache
                        && has_active_shared_cache_session(app_state, target.id, group.group_vid.get()).await
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

                    match probe_selected_manifest(
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
                    {
                        Ok(manifest) => Ok(Some(manifest)),
                        Err(class) => {
                            record_probe_member_connect_failed(
                                app_state,
                                target,
                                group,
                                fingerprint,
                                &member,
                                &input,
                                username,
                                request_headers,
                            );
                            Err(class)
                        }
                    }
                }
            }
        }),
    )
    .await
    .map(|(resolved, probed)| (resolved, probed.flatten()))
}

/// Writes the per-member `ConnectFailed` history row for a failed HLS start probe, mirroring the
/// MPEG-TS provider-open failure path (`record_provider_open_failure`): reason `ChannelUnavailable`
/// and stage `ProviderOpen`.
#[allow(clippy::too_many_arguments)]
fn record_probe_member_connect_failed(
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
    group: &FailoverGroupRecord,
    fingerprint: &Fingerprint,
    member: &GroupMember,
    input: &Arc<ConfigInput>,
    username: &str,
    request_headers: &HeaderMap,
) {
    let Some(user) = app_state.app_config.get_user_credentials(username) else {
        return;
    };
    let base = StreamChannel { target_id: target.id, virtual_id: group.group_vid.get(), ..StreamChannel::default() };
    let stream_channel = member_stream_channel(&base, group, member);
    record_connect_failed_attempt(ConnectFailedAttempt {
        app_state,
        fingerprint,
        user: &user,
        stream_channel,
        provider_name: Arc::clone(&input.name),
        req_headers: request_headers,
        reason: ConnectFailureReason::ChannelUnavailable,
        failure_stage: FailureStage::ProviderOpen,
    });
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

    fn master_response_with_cookie() -> HeaderMap {
        let mut response = HeaderMap::new();
        response.insert(axum::http::header::SET_COOKIE, "tok=abc; Path=/".parse().expect("set-cookie"));
        response
    }

    fn base_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::USER_AGENT, "probe".parse().expect("user-agent"));
        headers
    }

    fn variant_source(url: &str, configured_cookie: Option<&str>) -> InputSource {
        let mut headers = std::collections::HashMap::new();
        if let Some(cookie) = configured_cookie {
            headers.insert("Cookie".to_string(), cookie.to_string());
        }
        headers.insert("Authorization".to_string(), "Bearer token".to_string());
        InputSource {
            name: Arc::from("test"),
            url: url.to_string(),
            provider: None,
            username: None,
            password: None,
            method: shared::model::InputFetchMethod::GET,
            headers,
        }
    }

    fn cookie_of(headers: &HeaderMap) -> Option<&str> { headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) }

    #[test]
    fn variant_probe_headers_forward_master_cookie_on_same_origin() {
        let master = Url::parse("http://origin.example/live/master.m3u8").expect("master url");
        let headers = base_headers();
        let mut source = variant_source("http://origin.example/live/v.m3u8", None);
        let variant = variant_probe_headers(&headers, &master, &master_response_with_cookie(), &mut source);
        assert_eq!(cookie_of(&variant), Some("tok=abc"));
        assert_eq!(variant.get(header::USER_AGENT), headers.get(header::USER_AGENT));
        assert!(source.headers.contains_key("Authorization"));
    }

    #[test]
    fn variant_probe_headers_merge_configured_cookie_and_remove_it_from_source() {
        let master = Url::parse("http://origin.example/live/master.m3u8").expect("master url");
        let headers = base_headers();
        let mut source = variant_source("http://origin.example/live/v.m3u8", Some("cfg=1; tok=old"));
        let variant = variant_probe_headers(&headers, &master, &master_response_with_cookie(), &mut source);
        assert_eq!(cookie_of(&variant), Some("cfg=1; tok=abc"));
        assert!(!source.headers.keys().any(|name| name.eq_ignore_ascii_case("cookie")));
        assert!(source.headers.contains_key("Authorization"));
    }

    #[test]
    fn variant_probe_headers_drop_master_cookie_across_origins() {
        let master = Url::parse("http://origin.example/live/master.m3u8").expect("master url");
        let headers = base_headers();
        let mut source = variant_source("http://cdn.example/live/v.m3u8", Some("cfg=1"));
        let variant = variant_probe_headers(&headers, &master, &master_response_with_cookie(), &mut source);
        assert!(matches!(variant, Cow::Borrowed(_)));
        assert!(cookie_of(&variant).is_none());
        assert_eq!(source.headers.get("Cookie").map(String::as_str), Some("cfg=1"));
    }

    #[test]
    fn variant_probe_headers_without_master_cookie_keep_original_headers() {
        let master = Url::parse("http://origin.example/live/master.m3u8").expect("master url");
        let headers = base_headers();
        let mut source = variant_source("http://origin.example/live/v.m3u8", Some("cfg=1"));
        let variant = variant_probe_headers(&headers, &master, &HeaderMap::new(), &mut source);
        assert!(matches!(variant, Cow::Borrowed(_)));
        assert_eq!(source.headers.get("Cookie").map(String::as_str), Some("cfg=1"));
    }
}
