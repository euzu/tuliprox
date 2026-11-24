//! Recordings as an Xtream VOD category for playlist users.
//!
//! An API user with the `recording` player feature sees completed recordings of
//! their target through the same `get_vod_categories` / `get_vod_streams` /
//! `get_vod_info` / `/movie/{user}/{pass}/{id}.{ext}` endpoints a player already
//! uses. Everything here is opt-in: [`recording_vod_applies`] is the single gate,
//! and when it is false no recording state is read at all.
//!
//! Two invariants make the extension safe:
//!
//! * a playlist item always wins — recordings are only consulted after a
//!   playlist lookup failed, so a provider movie keeps its id;
//! * ids are persisted and never reused, so a client that cached a stream id
//!   cannot be served a different recording under it after a restart.

use super::{
    activate_local_file_stream, admit_local_file_connection, apply_stream_throttle, create_session_fingerprint,
    resolve_playback_request_admission, EvictionReentryGuard,
};
use crate::{
    api::{
        endpoints::{
            recording_api::{check_recording_visibility_with_context, AppRecordingVisibility},
            recording_media_api::{build_response, resolve_for_open, serve_range_parts},
        },
        model::{AppState, StreamError},
    },
    auth::Fingerprint,
    model::{ConfigInput, ConfigTarget, ProxyUserCredentials},
    utils::request::content_type_from_ext,
};
use axum::{
    body::{Body, Bytes},
    http::HeaderMap,
    response::Response,
};
use futures::{Stream, StreamExt};
use shared::{
    error::TuliproxError,
    model::{
        create_stream_channel_with_type, xtream_const::RECORDINGS_VOD_CATEGORY_ID, PlayerRecordingLevel,
        PlaylistItemType, StreamProperties, TargetType, VideoStreamDetailProperties, VideoStreamProperties, VirtualId,
        XtreamCluster, XtreamMappingOptions, XtreamPlaylistItem,
    },
};
use std::{
    collections::VecDeque,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tuliprox_core::model::api_user_claims;
use tuliprox_dvr::recording::recording_queue::RecordingVodEntry;

/// Whether recordings are served as Xtream VOD to this user on this target.
///
/// Cheapest predicate first: a playlist user with at least read access, the video
/// cluster, a user built from `api_proxy.yml` (the internal Web UI and recording
/// identities carry `t_is_api_user`), a target with Xtream output, and only then the
/// global recording switch. The user check is a field comparison; a credential
/// lookup would scan every configured API user on each request.
pub(crate) fn recording_vod_applies(app_state: &AppState, user: &ProxyUserCredentials, target: &ConfigTarget) -> bool {
    matches!(
        user.player_features.recording,
        PlayerRecordingLevel::Read | PlayerRecordingLevel::Private | PlayerRecordingLevel::Shared
    ) && user.allows_cluster(XtreamCluster::Video)
        && !user.t_is_api_user
        && target.has_output(TargetType::Xtream)
        && crate::api::model::recording::recording_supervisor::recording_enabled(&app_state.app_config)
}

/// The completed recordings this user may see, in queue order.
///
/// The queue projects only recordings the caller could see at all; the full
/// visibility rule still runs per entry. One [`AppRecordingVisibility`] serves the
/// whole request: its per-item cache makes the source lookup of a shared recording
/// a single evaluation.
pub(crate) async fn visible_recording_entries(
    app_state: &AppState,
    user: &ProxyUserCredentials,
    target: &ConfigTarget,
) -> Vec<RecordingVodEntry> {
    let claims = api_user_claims(user);
    let Some(subject_id) = claims.subject_id.as_ref() else {
        return Vec::new();
    };
    let entries = app_state.recording.queue.committed_recording_vod_entries(subject_id, &target.name, target.id).await;
    let visibility = AppRecordingVisibility::new(app_state);
    let mut visible = Vec::with_capacity(entries.len());
    for entry in entries {
        if check_recording_visibility_with_context(app_state, &entry.recording, &claims, subject_id, Some(&visibility))
            .await
        {
            visible.push(entry);
        }
    }
    visible
}

/// The Xtream VOD item that publishes one recording.
///
/// `item_type` stays `Video`: a `LocalVideo` item would route playback into the
/// library file path instead of the recording that owns the id. The video properties
/// carry the recorded window and completion time, so both the listing document and
/// the info document publish them.
pub(crate) fn recording_vod_item(entry: &RecordingVodEntry) -> XtreamPlaylistItem {
    recording_vod_item_named(entry.vod_id, &entry.title, &entry.filename, entry.duration_secs, entry.added)
}

fn recording_vod_item_named(
    vod_id: u32,
    title: &str,
    filename: &str,
    duration_secs: Option<u64>,
    added: Option<i64>,
) -> XtreamPlaylistItem {
    // The URL only carries the container extension the mapper publishes; no
    // route serves it and no field of the document is built from it.
    let url = format!("recording:///{filename}");
    let video = VideoStreamProperties {
        name: Arc::from(title),
        stream_id: vod_id,
        category_id: RECORDINGS_VOD_CATEGORY_ID,
        added: Arc::from(added.map(|ts| ts.to_string()).unwrap_or_default()),
        details: duration_secs.map(|secs| {
            // Xtream providers publish `duration` as `HH:MM:SS` and `duration_secs` as seconds.
            let duration = format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60);
            VideoStreamDetailProperties {
                duration_secs: Some(Arc::from(secs.to_string().as_str())),
                duration: Some(Arc::from(duration.as_str())),
                ..Default::default()
            }
        }),
        ..Default::default()
    };
    XtreamPlaylistItem {
        virtual_id: VirtualId::new(vod_id),
        provider_id: 0,
        name: title.into(),
        logo: "".into(),
        logo_small: "".into(),
        group: "".into(),
        title: title.into(),
        parent_code: "".into(),
        rec: "".into(),
        url: url.as_str().into(),
        epg_channel_id: None,
        xtream_cluster: XtreamCluster::Video,
        additional_properties: Some(StreamProperties::Video(Box::new(video))),
        item_type: PlaylistItemType::Video,
        category_id: RECORDINGS_VOD_CATEGORY_ID,
        input_name: "".into(),
        channel_no: 0,
        source_ordinal: 0,
        input_stream_id: "".into(),
        upstream_user_agent: None,
        input_url: "".into(),
    }
}

/// The info document of one recording, built from the recording alone.
///
/// The same item mapping as the listing, so duration, completion time and container
/// extension cannot drift between `get_vod_streams` and `get_vod_info`. No provider
/// metadata is consulted: the recording is ours, not the provider's.
pub(crate) fn recording_vod_info(
    entry: &RecordingVodEntry,
    options: &XtreamMappingOptions,
) -> Result<String, TuliproxError> {
    serde_json::to_string(&recording_vod_item(entry).to_info_document(options))
        .map_err(|err| TuliproxError::RepositoryXtream(err.to_string()))
}

/// The serialized `get_vod_streams` documents of all visible recordings.
///
/// Serialized with the same mapping helper the playlist iterator uses, so a player
/// cannot tell a recording entry from a provider movie entry apart from its id and
/// category.
pub(crate) async fn recording_vod_documents(
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    target: &ConfigTarget,
) -> Result<Vec<String>, TuliproxError> {
    let Some(xtream_output) = target.get_xtream_output() else {
        return Ok(Vec::new());
    };
    let entries = visible_recording_entries(app_state, user, target).await;
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let options = Arc::new(crate::model::xtream_mapping_option_from_target_options(
        target,
        xtream_output,
        &app_state.app_config,
        user,
        app_state.get_encrypt_secret(),
    )?);
    let mut documents = Vec::with_capacity(entries.len());
    for entry in entries {
        documents.push(tuliprox_repository::xtream_item_json(&recording_vod_item(&entry), &options).await?);
    }
    Ok(documents)
}

/// The visible recording published under `vod_id`, if there is one.
///
/// The entry is selected before the visibility rule runs, so a single lookup costs
/// one projection and one visibility evaluation, not one per completed recording.
/// The caller passes the request's [`AppRecordingVisibility`], so the later open check
/// reuses its per-item cache instead of evaluating the same source again.
pub(crate) async fn visible_recording_by_vod_id(
    app_state: &AppState,
    user: &ProxyUserCredentials,
    vod_id: u32,
    visibility: &AppRecordingVisibility<'_>,
) -> Option<RecordingVodEntry> {
    if vod_id == 0 {
        return None;
    }
    let entry = app_state.recording.queue.committed_recording_vod_entry(vod_id).await?;
    let claims = api_user_claims(user);
    let subject_id = claims.subject_id.as_ref()?;
    check_recording_visibility_with_context(app_state, &entry.recording, &claims, subject_id, Some(visibility))
        .await
        .then_some(entry)
}

/// Appends the recording documents after a playlist JSON stream.
///
/// The playlist stream reports `has_next` per entry, so the trailing comma rule
/// can only be decided with one entry of lookahead: the last playlist entry has
/// to learn whether recordings follow it. With no recordings the adapter is a
/// byte-identical pass-through.
pub(crate) struct AppendRecordingVod<S> {
    base: S,
    /// One entry of lookahead: only the next entry tells whether a comma is allowed.
    pending: Option<String>,
    /// A base error held back until the entry buffered before it was emitted.
    deferred_error: Option<TuliproxError>,
    extras: VecDeque<String>,
    base_done: bool,
}

impl<S> AppendRecordingVod<S> {
    pub(crate) fn new(base: S, extras: Vec<String>) -> Self {
        Self { base, pending: None, deferred_error: None, extras: extras.into(), base_done: false }
    }
}

impl<S> Stream for AppendRecordingVod<S>
where
    S: Stream<Item = Result<(String, bool), TuliproxError>> + Unpin,
{
    type Item = Result<(String, bool), TuliproxError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(err) = self.deferred_error.take() {
                return Poll::Ready(Some(Err(err)));
            }
            if !self.base_done {
                match Pin::new(&mut self.base).poll_next(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Some(Ok((line, _)))) => match self.pending.replace(line) {
                        Some(previous) => return Poll::Ready(Some(Ok((previous, true)))),
                        None => continue,
                    },
                    Poll::Ready(Some(Err(err))) => {
                        self.base_done = true;
                        self.extras.clear();
                        if let Some(last) = self.pending.take() {
                            self.deferred_error = Some(err);
                            return Poll::Ready(Some(Ok((last, false))));
                        }
                        return Poll::Ready(Some(Err(err)));
                    }
                    Poll::Ready(None) => {
                        self.base_done = true;
                        if let Some(last) = self.pending.take() {
                            return Poll::Ready(Some(Ok((last, !self.extras.is_empty()))));
                        }
                    }
                }
            }
            return match self.extras.pop_front() {
                Some(line) => {
                    let has_next = !self.extras.is_empty();
                    Poll::Ready(Some(Ok((line, has_next))))
                }
                None => Poll::Ready(None),
            };
        }
    }
}

/// The content type of a recording file, by container extension.
pub(crate) fn recording_content_type(filename: &str) -> &'static str {
    std::path::Path::new(filename)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .map_or("application/octet-stream", content_type_from_ext)
}

/// Answers `/movie/{user}/{pass}/{vod_id}.{ext}` from a recording.
///
/// Only reached when no playlist item carries the id. `None` means "no recording to
/// serve": the caller keeps its regular fallback, so an id nobody owns answers
/// exactly like an unknown playlist id and the existence of another user's private
/// recording stays undisclosed. Admission runs before the file is opened, so a
/// refused request costs no read, and the body reuses the range and clip handling of
/// the library playback path.
pub(crate) async fn serve_recording_vod(
    fingerprint: &Fingerprint,
    req_headers: &HeaderMap,
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    target: &ConfigTarget,
    vod_id: u32,
) -> Option<Response> {
    if user.permission_denied(&app_state.app_config) {
        return Some(recording_admission_expired_response(app_state, fingerprint, user, target, vod_id, req_headers));
    }
    let visibility = AppRecordingVisibility::new(app_state);
    let entry = visible_recording_by_vod_id(app_state, user, vod_id, &visibility).await?;
    let claims = api_user_claims(user);
    // `resolve_for_open` stays the security boundary; the shared visibility instance
    // only spares it a second source evaluation.
    let resolved = resolve_for_open(app_state, &claims, &entry.uuid, Some(&visibility)).await.ok()?;
    let content_type = recording_content_type(&entry.filename);

    let item = recording_vod_item(&entry);
    let stream_channel = create_stream_channel_with_type(target.id, &item, PlaylistItemType::Video);
    let playback_session_token = create_session_fingerprint(fingerprint, &user.username, vod_id, false);
    // The same transition guard the local-file path takes: admission and activation of
    // one session must not interleave with another request of that session.
    let _transition_guard =
        app_state.active_users.acquire_playback_transition(&user.username, &playback_session_token).await;
    let user_session =
        app_state.active_users.get_and_update_user_session(&user.username, &playback_session_token).await;
    let (admission, _grace_mode, request_class) = resolve_playback_request_admission(
        &app_state.admission_ctx(),
        user,
        fingerprint,
        user_session.as_ref(),
        playback_session_token.as_str(),
        false,
        EvictionReentryGuard::Session(playback_session_token.as_str()),
        false,
        false,
    )
    .await;
    if admission.is_reentry_suppressed() {
        return Some(crate::api::api_utils::reentry_suppressed_response());
    }

    // A recording plays from local storage and holds no provider connection, so the
    // session and the admission records carry no provider name.
    let input: &ConfigInput = std::sync::LazyLock::force(&RECORDING_INPUT);
    let connection_permission = match admit_local_file_connection(
        fingerprint,
        app_state,
        &stream_channel,
        req_headers,
        input,
        user,
        admission.permission(),
        Some(playback_session_token.as_str()),
    )
    .await
    {
        Ok(permission) => permission,
        Err(response) => return Some(*response),
    };

    // Admission is done; the file is opened only for a request that may stream. The
    // session placeholder that carries the counted connection is created by the
    // activation below, which runs after a satisfiable range, so a 416 leaves no
    // counted connection behind.
    let (status, headers, body) = match serve_range_parts(&resolved, req_headers, false, Some(content_type)).await {
        Ok(parts) => parts,
        Err(response) => return Some(*response),
    };

    let stream = apply_stream_throttle(app_state, item.item_type, body_to_provider_stream(body));
    let stream = match activate_local_file_stream(
        app_state,
        fingerprint,
        &stream_channel,
        req_headers,
        input,
        user,
        connection_permission,
        crate::api::model::ConnectionKind::Normal,
        Some(playback_session_token.as_str()),
        Some(request_class),
        stream,
    )
    .await
    {
        Ok(stream) => stream,
        Err(response) => return Some(*response),
    };

    Some(build_response(status, headers, Body::from_stream(stream)))
}

/// The rejection a playlist user with an expired account gets on the movie path.
fn recording_admission_expired_response(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    target: &ConfigTarget,
    vod_id: u32,
    req_headers: &HeaderMap,
) -> Response {
    crate::api::api_utils::admission_failure_response(
        app_state,
        fingerprint,
        user,
        create_stream_channel_with_type(
            target.id,
            &recording_vod_item_named(vod_id, "", "", None, None),
            PlaylistItemType::Video,
        ),
        "".into(),
        req_headers,
        shared::model::ConnectFailureReason::UserAccountExpired,
    )
}

/// The input recording playback runs under: no provider, because the bytes come from
/// local storage.
static RECORDING_INPUT: std::sync::LazyLock<ConfigInput> = std::sync::LazyLock::new(ConfigInput::default);

/// The body chunks as the byte-stream type the connection admission wraps.
fn body_to_provider_stream(body: Body) -> crate::api::model::BoxedProviderStream {
    body.into_data_stream()
        .map(|chunk: Result<Bytes, axum::Error>| chunk.map_err(|err| StreamError::Stream(err.to_string())))
        .boxed()
}

#[cfg(test)]
mod tests;
