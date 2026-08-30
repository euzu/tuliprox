#![allow(clippy::wildcard_imports)]
use super::*;

///
/// `BitTV` archive media URLs look like `2026/07/24/14/13/38-06800.ts` and lose Flussonic
/// path markers after HLS rewrite, so the panel would otherwise keep showing Live + live EPG.
pub(in crate::api) fn m3u_catchup_epg_reference_from_session_token(session_token: &str) -> Option<i64> {
    let rest = session_token.strip_prefix("m3u-catchup|")?;
    for marker in ["|archive|", "|timeshift_abs|"] {
        if let Some(idx) = rest.rfind(marker) {
            let after = &rest[idx + marker.len()..];
            let start = after.split('|').next()?.trim();
            if let Ok(ts) = start.parse::<i64>() {
                return Some(ts);
            }
        }
    }
    None
}

pub(super) fn resolve_m3u_archive_reference(stream_url: &str, session_token: Option<&str>) -> Option<i64> {
    m3u_archive_epg_reference_ts(stream_url)
        .or_else(|| epg_reference_ts_from_date_tree_path(stream_url))
        .or_else(|| session_token.and_then(m3u_catchup_epg_reference_from_session_token))
}

pub(super) fn looks_like_archive_media_path(path: &str) -> bool {
    let rel = path.trim_start_matches('/');
    if rel.is_empty() {
        return false;
    }
    rel.starts_with("dvr-") || rel.contains("/dvr-") || epg_reference_ts_from_date_tree_path(rel).is_some()
}

/// `BitTV` / Flussonic date-tree segments: `YYYY/MM/DD/HH/MM/SS-*.ts` or `dvr-YYYY/...`.
pub(in crate::api) fn epg_reference_ts_from_date_tree_path(path: &str) -> Option<i64> {
    let owned_path;
    let mut rel = path.trim_start_matches('/');
    if let Some(idx) = rel.find('?') {
        rel = &rel[..idx];
    }
    if rel.contains("://") {
        let parsed = Url::parse(rel).ok()?;
        owned_path = parsed.path().trim_start_matches('/').to_string();
        rel = owned_path.as_str();
    }
    if let Some(rest) = rel.strip_prefix("dvr-") {
        rel = rest;
    }
    let mut parts = rel.split('/');
    let year: i32 = parts.next()?.parse().ok()?;
    if !(2000..=2100).contains(&year) {
        return None;
    }
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    let hour: u32 = parts.next()?.parse().ok()?;
    let minute: u32 = parts.next()?.parse().ok()?;
    let sec_token = parts.next()?.split('-').next()?.trim_end_matches(".ts").trim_end_matches(".m3u8");
    let second: u32 = sec_token.parse().ok()?;
    let naive = chrono::NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(hour, minute, second)?;
    Some(naive.and_utc().timestamp())
}

/// Join a client-leaked relative DVR/media path against the session's origin URL.
///
/// When an origin `.m3u8` is force-piped without `rewrite_hls`, players resolve
/// `dvr-2026/...ts?token=` against the proxy playlist URL (`/hls/.../{token}.m3u8`).
pub(super) fn resolve_leaked_hls_relative_origin(
    session_stream_url: &str,
    relative_path: &str,
    request_query: Option<&str>,
) -> Option<String> {
    let rel = relative_path.trim_start_matches('/');
    if rel.is_empty() || rel.contains("://") || rel.split('/').any(|segment| matches!(segment, "." | "..")) {
        return None;
    }
    // Only recover archive-style relative paths (BitTV/Flussonic DVR or date trees).
    if !looks_like_archive_media_path(rel) {
        return None;
    }
    let parsed = url::Url::parse(session_stream_url).ok()?;
    let session_path = parsed.path();

    // If the session URL is already inside a DVR/date tree, strip back to the stream root
    // so sibling relative segments do not nest under the previous segment directory.
    let joined = if rel.starts_with("dvr-") {
        if let Some(idx) = session_path.find("/dvr-") {
            let mut joined = parsed.clone();
            joined.set_path(&format!("{}{}", &session_path[..=idx], rel));
            joined.set_query(None);
            joined.into()
        } else {
            parsed.join(rel).ok()?.into()
        }
    } else if let Some(idx) = session_path.find("/202") {
        let mut joined = parsed.clone();
        joined.set_path(&format!("{}/{}", &session_path[..idx], rel));
        joined.set_query(None);
        joined.into()
    } else {
        parsed.join(rel).ok()?.into()
    };

    if let Some(query) = request_query.filter(|q| !q.is_empty()) {
        Some(format!("{joined}?{query}"))
    } else {
        Some(joined)
    }
}

pub(super) fn legacy_hls_route_allowed_with_cache(
    cache_enabled: bool,
    decoded_session_token: Option<&str>,
    existing_session_token: Option<&str>,
) -> bool {
    !cache_enabled
        || decoded_session_token.is_some_and(|decoded| {
            existing_session_token.is_some_and(|existing| decoded == existing && is_m3u_catchup_session_token(existing))
        })
}

pub(super) fn query_flag_is_archive(key: &str) -> bool {
    key.eq_ignore_ascii_case("utc") || key.eq_ignore_ascii_case("utcstart")
}

pub(super) fn query_flag_marks_start_context(key: &str) -> bool {
    key.eq_ignore_ascii_case("end")
        || key.eq_ignore_ascii_case("duration")
        || key.eq_ignore_ascii_case("lutc")
        || key.eq_ignore_ascii_case("offset")
}

pub(in crate::api) fn m3u_archive_epg_reference_ts(stream_url: &str) -> Option<i64> {
    use crate::iptv::m3u::parse_flussonic_archive_file;

    let parsed = Url::parse(stream_url).ok()?;
    // Flussonic / TiviMate path forms: archive|index|video|mono-{utc}-{duration}.m3u8
    // and timeshift_abs / timeshift_rel. Without this, HLS sessions stay LiveHls in the panel.
    if let Some(file) = parsed.path_segments().and_then(|mut segments| segments.next_back()) {
        if let Some(archive) = parse_flussonic_archive_file(file) {
            if let Some(ts) = archive.epg_reference_ts() {
                return Some(ts);
            }
        }
    }
    // BitTV date-tree: /YYYY/MM/DD/HH/MM/SS-*.ts
    if let Some(ts) = epg_reference_ts_from_date_tree_path(parsed.path()) {
        return Some(ts);
    }
    let mut start_ts = None;
    let mut has_start_context = false;
    for (key, value) in parsed.query_pairs() {
        if query_flag_is_archive(&key) {
            if let Ok(ts) = value.parse::<i64>() {
                return Some(ts);
            }
        } else if key.eq_ignore_ascii_case("start") || key.eq_ignore_ascii_case("timestamp") {
            start_ts = value.parse::<i64>().ok();
        } else if query_flag_marks_start_context(&key) {
            has_start_context = true;
        }
    }

    has_start_context.then_some(start_ts).flatten()
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn admit_recovered_archive_stream(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &Arc<ProxyUserCredentials>,
    req_headers: &HeaderMap,
    input: &Arc<ConfigInput>,
    mut session: UserSession,
    stream_channel: StreamChannel,
) -> Result<(UserSession, StreamChannel, Option<GraceMode>), Box<axum::response::Response>> {
    if session.permission == UserConnectionPermission::Exhausted {
        return Err(Box::new(
            hls_admission_failure_manifest_response(
                app_state,
                fingerprint,
                user,
                stream_channel,
                session.provider.clone(),
                req_headers,
                ConnectFailureReason::UserConnectionsExhausted,
            )
            .await,
        ));
    }
    if app_state.active_provider.is_over_limit(&session.provider).await {
        return Err(Box::new(
            hls_admission_failure_manifest_response(
                app_state,
                fingerprint,
                user,
                stream_channel,
                session.provider.clone(),
                req_headers,
                ConnectFailureReason::ProviderConnectionsExhausted,
            )
            .await,
        ));
    }
    let (connection_admission, grace_mode, _) = crate::api::api_utils::resolve_playback_request_admission(
        &app_state.admission_ctx(),
        user,
        fingerprint,
        Some(&session),
        &session.token,
        true,
        crate::api::api_utils::EvictionReentryGuard::Session(&session.token),
        false,
        false,
    )
    .await;
    let connection_permission = connection_admission.permission;
    let connection_kind = connection_admission.kind.or(session.connection_kind);
    session.permission = connection_permission;
    if let Some(connection_kind) = connection_kind {
        session.connection_kind = Some(connection_kind);
    }
    if connection_permission == UserConnectionPermission::Exhausted
        || (connection_permission == UserConnectionPermission::GracePeriod && connection_kind.is_none())
    {
        let provider = if session.provider.is_empty() { input.name.clone() } else { session.provider.clone() };
        return Err(Box::new(
            hls_admission_failure_manifest_response(
                app_state,
                fingerprint,
                user,
                stream_channel,
                provider,
                req_headers,
                ConnectFailureReason::UserConnectionsExhausted,
            )
            .await,
        ));
    }
    Ok((session, stream_channel, grace_mode))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn hls_api_stream_leaked_relative(
    fingerprint: Fingerprint,
    req_headers: HeaderMap,
    app_state: Arc<AppState>,
    user: Arc<ProxyUserCredentials>,
    target: Arc<ConfigTarget>,
    input: Arc<ConfigInput>,
    stream_id: u32,
    mut session: UserSession,
    session_stream_url: String,
    relative_path: String,
    request_query: Option<&str>,
) -> axum::response::Response {
    if let Err(e) = check_network_access_only(&user, &fingerprint, &app_state.app_config, &app_state.geoip) {
        return e.into_player_response(app_state.app_config.get_auth_error_status());
    }
    let Some(origin_url) = resolve_leaked_hls_relative_origin(&session_stream_url, &relative_path, request_query)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let archive_reference = resolve_m3u_archive_reference(&origin_url, Some(session.token.as_str()))
        .or_else(|| epg_reference_ts_from_date_tree_path(&relative_path))
        .or_else(|| epg_reference_ts_from_date_tree_path(&origin_url));
    let is_archive_media = looks_like_archive_media_path(&relative_path) || looks_like_archive_media_path(&origin_url);
    session.stream_url = origin_url.intern();
    let mut stream_channel = resolve_stream_channel(
        &app_state,
        &target,
        &input,
        stream_id,
        &session.stream_url,
        archive_reference,
        Some(session.token.as_str()),
    )
    .await;
    // Leaked DVR/date-tree segments are always archive playback for the panel, even when the
    // prior session was live and the date-tree timestamp could not be parsed.
    if is_archive_media {
        stream_channel.item_type = PlaylistItemType::Catchup;
        stream_channel.cluster = XtreamCluster::Video;
        if stream_channel.epg_reference_ts.is_none() {
            stream_channel.epg_reference_ts = archive_reference;
        }
    }
    let (session, stream_channel, grace_mode) = match admit_recovered_archive_stream(
        &app_state,
        &fingerprint,
        &user,
        &req_headers,
        &input,
        session,
        stream_channel,
    )
    .await
    {
        Ok(admission) => admission,
        Err(response) => return *response,
    };
    force_provider_stream_response(
        &fingerprint,
        &app_state,
        &session,
        stream_channel,
        crate::api::api_utils::ForceStreamRequestContext {
            req_headers: &req_headers,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: get_hls_session_ttl_secs(&app_state),
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        grace_mode,
    )
    .await
    .into_response()
}
