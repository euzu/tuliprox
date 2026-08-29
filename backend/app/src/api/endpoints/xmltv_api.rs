use crate::{
    api::{
        api_utils::{
            coalesce_byte_stream, create_api_proxy_user, empty_json_response_as_array, get_user_target,
            get_user_target_by_credentials, internal_server_error, resource_response,
            stream_json_or_bin_response_try_stream, try_unwrap_body,
        },
        model::{AppState, UserApiRequest, UserApiRequestQueryOrBody},
        static_headers::CT_XML,
    },
    auth::{resolve_api_user_context, Fingerprint},
    model::{
        ApiProxyServerInfo, Config, ConfigTarget, ProxyUserCredentials, EPG_ATTRIB_ID, EPG_ATTRIB_LANG,
        EPG_TAG_CATEGORY, EPG_TAG_CHANNEL, EPG_TAG_LIVE, EPG_TAG_NEW,
    },
    repository::{
        epg_query_channels_by_storage_key, get_target_storage_path, m3u_get_epg_file_path_for_target, storage_const,
        xtream_get_epg_file_path_for_target, xtream_get_storage_path, BPlusTreeQuery, LockedReceiverStream,
        XML_PREAMBLE,
    },
    utils,
    utils::{
        canonicalize_output_epg_id, canonicalize_untrusted_epg_id, deobscure_text, file_exists_async,
        format_xmltv_time_utc, get_epg_processing_options, lowercase_xmltv_text, obscure_text, EpgIdOutputCase,
        EpgProcessingOptions, EpgTimeShift,
    },
};
use axum::response::IntoResponse;
use chrono::{DateTime, TimeZone};
use log::{error, trace};
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use shared::{
    concat_string,
    model::{
        ConfigTargetOptions, EpgChannel, EpgProgramme, EpgProgrammeDto, ShortEpgDto, ShortEpgResultDto, StreamEpgEntry,
        StreamEpgItemRequest, StreamEpgRequest, StreamEpgResponse, TargetType,
    },
    utils::{concat_path, concat_path_leading_slash, obfuscate_text, Internable},
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::mpsc,
    task,
};
use tokio_stream::StreamExt;
use tokio_util::io::ReaderStream;

pub fn get_empty_epg_response() -> axum::response::Response {
    try_unwrap_body!(axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, CT_XML.clone()) //axum::http::HeaderValue::from_static("text/xml"))
        .body(axum::body::Body::from(r#"<?xml version="1.0" encoding="utf-8" ?><!DOCTYPE tv SYSTEM "xmltv.dtd"><tv generator-info-name="Xtream Codes" generator-info-url=""></tv>"#)))
}

fn get_epg_path_for_target_of_type(target_name: &str, epg_path: PathBuf) -> Option<PathBuf> {
    if utils::path_exists(&epg_path) {
        return Some(epg_path);
    }
    trace!("Can't find epg file for {target_name} target: {}", epg_path.to_str().unwrap_or("?"));
    None
}

pub(in crate::api) fn get_epg_path_for_target(config: &Config, target: &ConfigTarget) -> Option<PathBuf> {
    // TODO if we have multiple targets, first one serves, this can be problematic when
    // we use m3u playlist but serve xtream target epg

    // TODO if we share the same virtual_id for epg, can we store an epg file for the target ?
    for output in &target.output {
        if let Some(epg_path) = get_epg_path_for_target_by_type(config, target, TargetType::from(output)) {
            return Some(epg_path);
        }
    }
    None
}

pub(in crate::api) fn get_epg_path_for_target_by_type(
    config: &Config,
    target: &ConfigTarget,
    output_type: TargetType,
) -> Option<PathBuf> {
    if !output_type.supports_epg() {
        return None;
    }
    match output_type {
        TargetType::Xtream => {
            if let Some(storage_path) = xtream_get_storage_path(config, &target.name) {
                return get_epg_path_for_target_of_type(
                    &target.name,
                    xtream_get_epg_file_path_for_target(&storage_path),
                );
            }
            None
        }
        TargetType::M3u => {
            if let Some(target_path) = get_target_storage_path(config, &target.name) {
                return get_epg_path_for_target_of_type(&target.name, m3u_get_epg_file_path_for_target(&target_path));
            }
            None
        }
        TargetType::Strm | TargetType::HdHomeRun => None,
    }
}

pub async fn serve_epg(
    app_state: &Arc<AppState>,
    epg_path: &Path,
    user: &ProxyUserCredentials,
    target: &Arc<ConfigTarget>,
    limit: Option<u32>,
) -> axum::response::Response {
    if file_exists_async(epg_path).await {
        serve_epg_with_rewrites(app_state, epg_path, user, target, limit).await
    } else {
        get_empty_epg_response()
    }
}

pub async fn serve_epg_web_ui(
    app_state: &Arc<AppState>,
    accept: Option<&str>,
    epg_path: &Path,
    target: &Arc<ConfigTarget>,
) -> axum::response::Response {
    if file_exists_async(epg_path).await {
        let config = app_state.app_config.config.load();
        let web_ui_path = config.web_ui.as_ref().and_then(|w| w.path.as_ref()).map_or("", String::as_str);
        let resource_url = concat_path_leading_slash(web_ui_path, "api/v1/playlist/resource");
        let encrypt_secret = app_state.get_encrypt_secret();
        let iter_lock = app_state.app_config.file_locks.read_lock(epg_path).await;
        let bg_lock = app_state.app_config.file_locks.read_lock(epg_path).await;
        let epg_path = epg_path.to_path_buf();
        let target_name = target.name.clone();
        let (tx, rx) = mpsc::channel::<Result<EpgChannel, String>>(64);

        let epg_path_for_log = epg_path.clone();
        let target_name_for_log = target_name.clone();
        let join_error_tx = tx.clone();
        let handle = task::spawn_blocking(move || {
            let _guard = bg_lock;
            let query = match BPlusTreeQuery::<Arc<str>, EpgChannel>::try_new(&epg_path) {
                Ok(query) => query,
                Err(error) => {
                    let message =
                        format!("Failed to open epg db for target {target_name} {}: {error}", epg_path.display());
                    error!("{message}");
                    let _ = tx.blocking_send(Err(message));
                    return;
                }
            };
            for entry in query.disk_iter() {
                let (_, channel) = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        let message = format!("EPG stream failed for {}: {error}", epg_path.display());
                        error!("{message}");
                        let _ = tx.blocking_send(Err(message));
                        break;
                    }
                };
                if tx.blocking_send(Ok(channel)).is_err() {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            if let Err(err) = handle.await {
                error!(
                    "EPG web UI producer task failed for target {} {}: {err}",
                    target_name_for_log,
                    epg_path_for_log.display()
                );
                let _ = join_error_tx
                    .send(Err(format!(
                        "EPG web UI producer task failed for target {} {}: {err}",
                        target_name_for_log,
                        epg_path_for_log.display()
                    )))
                    .await;
            }
        });

        let stream = LockedReceiverStream::new(rx, iter_lock).map(move |result| {
            result.map(|channel| rewrite_epg_channel_resource_url(&encrypt_secret, &resource_url, channel))
        });
        return stream_json_or_bin_response_try_stream(accept, stream);
    }
    try_unwrap_body!(empty_json_response_as_array())
}

pub fn rewrite_epg_channel_resource_url(
    encrypt_secret: &[u8; 16],
    resource_url: &str,
    mut channel: EpgChannel,
) -> EpgChannel {
    let Some(icon) = channel.icon.as_ref() else {
        return channel;
    };
    if icon.is_empty() || icon.starts_with('/') {
        return channel;
    }
    channel.icon = Some(concat_path(resource_url, &obfuscate_text(encrypt_secret, icon)).intern());
    channel
}

macro_rules! continue_on_err {
    ($expr:expr) => {
        if let Err(_err) = $expr {
            continue;
        }
    };
}

async fn write_programme_classification_tags<W: AsyncWrite + Unpin>(
    writer: &mut quick_xml::Writer<W>,
    programme: &EpgProgramme,
) -> Result<(), quick_xml::Error> {
    for category in &programme.categories {
        let mut elem = BytesStart::new(EPG_TAG_CATEGORY);
        if let Some(lang) = &category.lang {
            elem.push_attribute((EPG_ATTRIB_LANG, lang.as_ref()));
        }
        writer.write_event_async(Event::Start(elem)).await?;
        writer.write_event_async(Event::Text(BytesText::new(category.value.as_ref()))).await?;
        writer.write_event_async(Event::End(BytesEnd::new(EPG_TAG_CATEGORY))).await?;
    }

    if programme.is_live {
        writer.write_event_async(Event::Empty(BytesStart::new(EPG_TAG_LIVE))).await?;
    }
    if programme.is_new {
        writer.write_event_async(Event::Empty(BytesStart::new(EPG_TAG_NEW))).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn serve_epg_with_rewrites(
    app_state: &Arc<AppState>,
    epg_path: &Path,
    user: &ProxyUserCredentials,
    target: &Arc<ConfigTarget>,
    limit: Option<u32>,
) -> axum::response::Response {
    if !file_exists_async(epg_path).await {
        return get_empty_epg_response();
    }

    let epg_processing_options = get_epg_processing_options(&app_state.app_config, user, target);
    let lowercase_display_names =
        target.options.as_ref().is_some_and(ConfigTargetOptions::lowercase_xmltv_display_names);

    let server_info = app_state.app_config.get_user_server_info(user);
    let base_url =
        if !matches!(epg_processing_options.time_shift, EpgTimeShift::None) || epg_processing_options.rewrite_urls {
            server_info.as_ref().map(|si| {
                concat_string!(
                    &si.get_base_url(),
                    "/",
                    storage_const::EPG_RESOURCE_PATH,
                    "/",
                    &user.username,
                    "/",
                    &user.password
                )
            })
        } else {
            None
        };

    let generator_info = server_info.as_ref().map(ApiProxyServerInfo::get_base_url).unwrap_or_default();

    let limit = limit.unwrap_or_default();

    // EPG ids visible under the user's content filter; None = no filtering.
    let visible_epg_ids =
        crate::api::endpoints::user_visibility::collect_visible_epg_channel_ids(&app_state.app_config, target, user)
            .await;

    let bg_lock = app_state.app_config.file_locks.read_lock(epg_path).await;
    let epg_path = epg_path.to_path_buf();
    let (channel_tx, mut channel_rx) = mpsc::channel::<Result<EpgChannel, String>>(256);

    let epg_path_for_log = epg_path.clone();
    let join_error_tx = channel_tx.clone();
    let spawn_handle = task::spawn_blocking(move || {
        let _guard = bg_lock;
        let mut query = match BPlusTreeQuery::<Arc<str>, EpgChannel>::try_new(&epg_path) {
            Ok(query) => query,
            Err(error) => {
                let message = format!("Failed to open BPlusTreeQuery {}: {error}", epg_path.display());
                error!("{message}");
                let _ = channel_tx.blocking_send(Err(message));
                return;
            }
        };

        for entry in query.iter() {
            let (_, channel) = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    let message = format!("EPG rewrite stream failed for {}: {error}", epg_path.display());
                    error!("{message}");
                    let _ = channel_tx.blocking_send(Err(message));
                    break;
                }
            };
            if channel_tx.blocking_send(Ok(channel)).is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        if let Err(err) = spawn_handle.await {
            error!("EPG rewrite producer task failed for {}: {err}", epg_path_for_log.display());
            let _ = join_error_tx
                .send(Err(format!("EPG rewrite producer task failed for {}: {err}", epg_path_for_log.display())))
                .await;
        }
    });

    let (mut tx, rx) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        // Work-Around BytesText DocType escape, see below
        if let Err(err) = tx.write_all(XML_PREAMBLE.as_ref()).await {
            error!("EPG: Failed to write xml header {err}");
            return;
        }
        if let Err(err) = tx
            .write_all(format!(r#"<tv generator-info-name="X" generator-info-url="{generator_info}">"#).as_bytes())
            .await
        {
            error!("EPG: Failed to write xml tv header {err}");
            return;
        }

        let mut writer = quick_xml::writer::Writer::new(tx);
        while let Some(result) = channel_rx.recv().await {
            let channel = match result {
                Ok(channel) => channel,
                Err(error) => {
                    error!("{error}");
                    return;
                }
            };
            if let Some(visible) = &visible_epg_ids {
                if !visible.contains(&channel.id.to_lowercase()) {
                    continue;
                }
            }
            let programmes = if limit > 0 {
                channel.get_programme_with_limit(limit)
            } else {
                channel.programmes.iter().collect::<Vec<&EpgProgramme>>()
            };

            if !programmes.is_empty() {
                let mut elem = BytesStart::new(EPG_TAG_CHANNEL);
                elem.push_attribute((EPG_ATTRIB_ID, channel.id.as_ref()));
                continue_on_err!(writer.write_event_async(Event::Start(elem)).await);

                let elem = BytesStart::new("display-name");
                continue_on_err!(writer.write_event_async(Event::Start(elem)).await);
                let title: &str = channel.title.as_deref().unwrap_or("");
                let display_name = lowercase_xmltv_text(title, lowercase_display_names);
                continue_on_err!(writer.write_event_async(Event::Text(BytesText::new(display_name.as_ref()))).await);

                let elem = BytesEnd::new("display-name");
                continue_on_err!(writer.write_event_async(Event::End(elem)).await);

                if let Some(icon_url) = &channel.icon {
                    let icon = match (
                        epg_processing_options.rewrite_urls,
                        base_url.as_ref(),
                        obscure_text(&epg_processing_options.encrypt_secret, icon_url),
                    ) {
                        (true, Some(base), Ok(enc)) => concat_string!(base, "/", &enc),
                        _ => icon_url.to_string(),
                    };

                    let mut elem = BytesStart::new("icon");
                    elem.push_attribute(("src", icon.as_ref()));
                    if (writer.write_event_async(Event::Empty(elem)).await).is_err() {
                        // ignore
                    }
                }

                let elem = BytesEnd::new(EPG_TAG_CHANNEL);
                continue_on_err!(writer.write_event_async(Event::End(elem)).await);

                for programme in programmes {
                    let mut elem = BytesStart::new("programme");
                    let (user_start, user_stop) = (programme.start, programme.stop);
                    elem.push_attribute((
                        "start",
                        format_xmltv_time_utc(user_start, &epg_processing_options.time_shift).as_str(),
                    ));
                    elem.push_attribute((
                        "stop",
                        format_xmltv_time_utc(user_stop, &epg_processing_options.time_shift).as_str(),
                    ));
                    elem.push_attribute(("channel", channel.id.as_ref()));
                    if let Some(catchup_id) = programme.catchup_id.as_ref() {
                        elem.push_attribute(("catchup-id", catchup_id.as_ref()));
                    }
                    continue_on_err!(writer.write_event_async(Event::Start(elem)).await);

                    if let Some(title) = &programme.title {
                        let elem = BytesStart::new("title");
                        continue_on_err!(writer.write_event_async(Event::Start(elem)).await);
                        continue_on_err!(writer.write_event_async(Event::Text(BytesText::new(title))).await);
                        continue_on_err!(writer.write_event_async(Event::End(BytesEnd::new("title"))).await);
                    }

                    if let Some(desc) = &programme.desc {
                        let elem = BytesStart::new("desc");
                        continue_on_err!(writer.write_event_async(Event::Start(elem)).await);
                        continue_on_err!(writer.write_event_async(Event::Text(BytesText::new(desc))).await);
                        continue_on_err!(writer.write_event_async(Event::End(BytesEnd::new("desc"))).await);
                    }

                    if let Err(err) = write_programme_classification_tags(&mut writer, programme).await {
                        error!("EPG classification tags write failed: {err}");
                    }

                    let _ = writer.write_event_async(Event::End(BytesEnd::new("programme"))).await;
                }
            }
        }

        let mut out = writer.into_inner();

        if let Err(err) = out.write_all("</tv>".as_bytes()).await {
            error!("EPG: Failed to write xml tv close {err}");
        }

        if let Err(e) = out.shutdown().await {
            error!("Failed to shutdown epg gzip encoder: {e}");
        }
    });

    let body_stream = ReaderStream::new(rx);
    try_unwrap_body!(axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, mime::TEXT_XML.to_string())
        .body(axum::body::Body::from_stream(coalesce_byte_stream(body_stream))))
}

/// Looks up an EPG channel by its exact, target-output-case storage key.
async fn get_epg_channel_by_storage_key(
    app_state: &Arc<AppState>,
    storage_key: &Arc<str>,
    epg_path: &Path,
) -> Option<EpgChannel> {
    let file_lock = app_state.app_config.file_locks.read_lock(epg_path).await;
    let epg_path = epg_path.to_path_buf();
    let storage_key = Arc::clone(storage_key);

    match task::spawn_blocking(move || -> Option<EpgChannel> {
        let _guard = file_lock;
        match BPlusTreeQuery::<Arc<str>, EpgChannel>::try_new(&epg_path) {
            Ok(mut query) => match query.query(&storage_key) {
                Ok(Some(item)) => Some(item),
                Ok(None) => None,
                Err(err) => {
                    error!("Failed to query db file {}: {err}", epg_path.display());
                    None
                }
            },
            Err(err) => {
                error!("Failed to read db file {}: {err}", epg_path.display());
                None
            }
        }
    })
    .await
    {
        Ok(result) => result,
        Err(err) => {
            error!("Failed to run epg query task: {err}");
            None
        }
    }
}

fn format_xmltv_time(ts: i64) -> String {
    if let Some(dt) = DateTime::from_timestamp(ts, 0) {
        dt.naive_utc().format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
        String::new()
    }
}

fn get_applied_epg_timeshift(
    programme: &EpgProgramme,
    epg_processing_options: &EpgProcessingOptions,
) -> (String, String, i64, i64) {
    let (start_ts, stop_ts) = get_applied_epg_timestamps(programme, epg_processing_options);
    let (start_str, stop_str) = format_epg_timeshift_strings(programme, epg_processing_options, start_ts, stop_ts);
    (start_str, stop_str, start_ts, stop_ts)
}

fn get_applied_epg_timestamps(programme: &EpgProgramme, epg_processing_options: &EpgProcessingOptions) -> (i64, i64) {
    match &epg_processing_options.time_shift {
        EpgTimeShift::None | EpgTimeShift::TimeZone(_) => (programme.start, programme.stop),
        EpgTimeShift::Fixed(m) => {
            let off = i64::from(*m) * 60;
            (programme.start + off, programme.stop + off)
        }
    }
}

fn get_source_epg_timestamps(programme: &EpgProgramme) -> (i64, i64) { (programme.start, programme.stop) }

fn format_epg_timeshift_strings(
    programme: &EpgProgramme,
    epg_processing_options: &EpgProcessingOptions,
    start_ts: i64,
    stop_ts: i64,
) -> (String, String) {
    match &epg_processing_options.time_shift {
        EpgTimeShift::TimeZone(tz) => {
            match (
                chrono::Utc.timestamp_opt(programme.start, 0).single(),
                chrono::Utc.timestamp_opt(programme.stop, 0).single(),
            ) {
                (Some(s_dt), Some(e_dt)) => (
                    s_dt.with_timezone(tz).format("%Y-%m-%d %H:%M:%S").to_string(),
                    e_dt.with_timezone(tz).format("%Y-%m-%d %H:%M:%S").to_string(),
                ),
                _ => (format_xmltv_time(start_ts), format_xmltv_time(stop_ts)),
            }
        }
        EpgTimeShift::None | EpgTimeShift::Fixed(_) => (format_xmltv_time(start_ts), format_xmltv_time(stop_ts)),
    }
}

fn from_programme(
    stream_id: &Arc<str>,
    epg_id: &Arc<str>,
    programme: &EpgProgramme,
    epg_processing_options: &EpgProcessingOptions,
    has_archive: bool,
) -> ShortEpgDto {
    let (start_str, end_str, start_ts, stop_ts) = get_applied_epg_timeshift(programme, epg_processing_options);

    ShortEpgDto {
        id: Arc::clone(stream_id),
        epg_id: Arc::clone(epg_id),
        title: programme.title.as_ref().map_or_else(String::new, ToString::to_string),
        lang: String::new(),
        start: start_str,
        end: end_str,
        description: programme.desc.as_ref().map_or_else(String::new, ToString::to_string),
        channel_id: Arc::clone(epg_id),
        start_timestamp: start_ts.to_string(),
        stop_timestamp: stop_ts.to_string(),
        stream_id: Arc::clone(stream_id),
        now_playing: None,
        has_archive: has_archive.then_some(1),
    }
}

const DEFAULT_SHORT_EPG_LIMIT: u32 = 4;

#[allow(clippy::too_many_arguments)]
pub async fn serve_short_epg(
    app_state: &Arc<AppState>,
    epg_path: &Path,
    user: &ProxyUserCredentials,
    target: &Arc<ConfigTarget>,
    channel_id: &Arc<str>,
    stream_id: Arc<str>,
    limit: u32,
    has_archive: bool,
) -> axum::response::Response {
    let lowercase_ids = target.options.as_ref().is_some_and(ConfigTargetOptions::lowercase_epg_ids);
    let output_case = EpgIdOutputCase::from_lowercase(lowercase_ids);
    let storage_key = canonicalize_output_epg_id(channel_id, output_case);
    let response_channel_id = Arc::clone(&storage_key);
    let short_epg = {
        // It seems provider set limit to 4 if it is undefined oor 0.
        let limit = if limit > 0 { limit } else { DEFAULT_SHORT_EPG_LIMIT };
        if file_exists_async(epg_path).await {
            if let Some(epg_channel) = get_epg_channel_by_storage_key(app_state, &storage_key, epg_path).await {
                let epg_processing_options = get_epg_processing_options(&app_state.app_config, user, target);
                ShortEpgResultDto {
                    epg_listings: epg_channel
                        .get_programme_with_limit(limit)
                        .iter()
                        .map(|p| {
                            from_programme(&stream_id, &response_channel_id, p, &epg_processing_options, has_archive)
                        })
                        .collect(),
                }
            } else {
                ShortEpgResultDto::default()
            }
        } else {
            ShortEpgResultDto::default()
        }
    };

    match serde_json::to_string(&short_epg) {
        Ok(json) => {
            (axum::http::StatusCode::OK, [(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())], json)
                .into_response()
        }
        Err(_) => internal_server_error!(),
    }
}

/// Serves per-stream EPG data for the UI "now playing" / "up next" display.
/// Queries epg.db by `epg_channel_id`, filters to 8h window, applies user timeshift.
/// Returns empty entries for all error/missing cases (no errors surfaced to client).
const STREAM_EPG_ARCHIVE_WINDOW_BACK_SECS: i64 = 4 * 3600;
const STREAM_EPG_ARCHIVE_WINDOW_FWD_SECS: i64 = 12 * 3600;
const MAX_STREAM_EPG_ITEMS: usize = 256;
const MAX_STREAM_EPG_CHANNEL_ID_BYTES: usize = 512;

struct PreparedStreamEpgEntry {
    storage_key: Arc<str>,
    reference_ts: Option<i64>,
}

struct PreparedStreamEpgRequest {
    entries: Vec<PreparedStreamEpgEntry>,
}

fn prepare_stream_epg_request(
    items: &[StreamEpgItemRequest],
    output_case: EpgIdOutputCase,
) -> PreparedStreamEpgRequest {
    let mut entry_index_by_storage_key = HashMap::<Arc<str>, usize>::with_capacity(items.len());
    let mut entries = Vec::<PreparedStreamEpgEntry>::with_capacity(items.len());

    for item in items {
        let storage_key = canonicalize_untrusted_epg_id(&item.epg_channel_id, output_case);
        if let Some(entry_index) = entry_index_by_storage_key.get(storage_key.as_ref()).copied() {
            if entries[entry_index].reference_ts.is_none() {
                entries[entry_index].reference_ts = item.reference_ts;
            }
        } else {
            entry_index_by_storage_key.insert(Arc::clone(&storage_key), entries.len());
            entries.push(PreparedStreamEpgEntry { storage_key, reference_ts: item.reference_ts });
        }
    }

    PreparedStreamEpgRequest { entries }
}

async fn serve_stream_epg(
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    target: &Arc<ConfigTarget>,
    epg_path: &Path,
    prepared: PreparedStreamEpgRequest,
) -> StreamEpgResponse {
    let epg_processing_options = get_epg_processing_options(&app_state.app_config, user, target);
    let live_now = chrono::Utc::now().timestamp();

    let PreparedStreamEpgRequest { entries: prepared_entries } = prepared;
    let storage_keys = prepared_entries.iter().map(|entry| Arc::clone(&entry.storage_key)).collect();

    let channels: Vec<Option<EpgChannel>> =
        match epg_query_channels_by_storage_key(&app_state.app_config.file_locks, epg_path, storage_keys).await {
            Ok(results) => results.into_iter().map(|(_, channel)| channel).collect(),
            Err(err) => {
                error!("{err}");
                std::iter::repeat_with(|| None).take(prepared_entries.len()).collect()
            }
        };

    let mut entries = Vec::with_capacity(channels.len());

    for (channel, prepared_entry) in channels.into_iter().zip(prepared_entries) {
        let reference_ts = prepared_entry.reference_ts.unwrap_or(live_now);
        let window_start = reference_ts.saturating_sub(STREAM_EPG_ARCHIVE_WINDOW_BACK_SECS);
        let window_end = reference_ts.saturating_add(STREAM_EPG_ARCHIVE_WINDOW_FWD_SECS);
        let programmes = match channel {
            Some(ch) => {
                stream_epg_programmes_for_channel(&ch.programmes, &epg_processing_options, window_start, window_end)
            }
            None => Vec::new(),
        };

        entries.push(StreamEpgEntry {
            epg_channel_id: prepared_entry.storage_key.to_string(),
            target_id: Some(target.id),
            programmes,
        });
    }

    StreamEpgResponse { entries }
}

fn stream_epg_programmes_for_channel(
    programmes: &[EpgProgramme],
    epg_processing_options: &EpgProcessingOptions,
    window_start: i64,
    window_end: i64,
) -> Vec<EpgProgrammeDto> {
    programmes
        .iter()
        .filter_map(|programme| {
            let (source_start_ts, source_stop_ts) = get_source_epg_timestamps(programme);
            (source_stop_ts > window_start && source_start_ts <= window_end).then(|| {
                let (start_ts, stop_ts) = get_applied_epg_timestamps(programme, epg_processing_options);
                let (start_str, stop_str) =
                    format_epg_timeshift_strings(programme, epg_processing_options, start_ts, stop_ts);
                EpgProgrammeDto {
                    start: start_str,
                    stop: stop_str,
                    start_timestamp: start_ts,
                    stop_timestamp: stop_ts,
                    title: programme.title.as_ref().map_or_else(String::new, ToString::to_string),
                }
            })
        })
        .collect()
}

fn group_stream_epg_items(items: Vec<StreamEpgItemRequest>) -> Result<Vec<(u16, Vec<StreamEpgItemRequest>)>, String> {
    if items.is_empty() {
        return Err("items must not be empty".to_string());
    }
    if items.len() > MAX_STREAM_EPG_ITEMS {
        return Err(format!("items must not exceed {MAX_STREAM_EPG_ITEMS} entries"));
    }

    let mut groups = Vec::<(u16, Vec<StreamEpgItemRequest>)>::new();
    for item in items {
        if item.epg_channel_id.is_empty() {
            return Err("epg_channel_id must not be empty".to_string());
        }
        if item.epg_channel_id.len() > MAX_STREAM_EPG_CHANNEL_ID_BYTES {
            return Err(format!("epg_channel_id must not exceed {MAX_STREAM_EPG_CHANNEL_ID_BYTES} bytes"));
        }
        let Some(target_id) = item.target_id else {
            return Err("all items must include target_id".to_string());
        };

        if let Some((_, group_items)) = groups.iter_mut().find(|(group_target_id, _)| *group_target_id == target_id) {
            group_items.push(item);
        } else {
            groups.push((target_id, vec![item]));
        }
    }

    Ok(groups)
}

fn empty_stream_epg_entries(target_id: u16, prepared: &PreparedStreamEpgRequest) -> Vec<StreamEpgEntry> {
    prepared
        .entries
        .iter()
        .map(|entry| StreamEpgEntry {
            epg_channel_id: entry.storage_key.to_string(),
            target_id: Some(target_id),
            programmes: Vec::new(),
        })
        .collect()
}

fn stream_epg_bad_request(message: &str) -> axum::response::Response {
    (axum::http::StatusCode::BAD_REQUEST, axum::Json(serde_json::json!({ "error": message }))).into_response()
}

/// Handles stream EPG API requests for per-stream programme display.
pub(crate) async fn stream_epg_api(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(stream_epg_req): axum::extract::Json<StreamEpgRequest>,
) -> impl IntoResponse + Send {
    let grouped_items = match group_stream_epg_items(stream_epg_req.items) {
        Ok(groups) => groups,
        Err(message) => return stream_epg_bad_request(&message),
    };

    let config = app_state.app_config.config.load_full();
    let user = create_api_proxy_user(&app_state);
    let mut entries = Vec::new();

    for (target_id, items) in grouped_items {
        let Some(target) = app_state.app_config.get_target_by_id(target_id) else {
            return stream_epg_bad_request(&format!("unknown target_id: {target_id}"));
        };
        let output_case = EpgIdOutputCase::from_lowercase(
            target.options.as_ref().is_some_and(ConfigTargetOptions::lowercase_epg_ids),
        );
        let prepared = prepare_stream_epg_request(&items, output_case);

        let Some(epg_path) = get_epg_path_for_target(config.as_ref(), &target) else {
            entries.extend(empty_stream_epg_entries(target_id, &prepared));
            continue;
        };

        let result = serve_stream_epg(&app_state, &user, &target, &epg_path, prepared).await;
        entries.extend(result.entries);
    }

    let result = StreamEpgResponse { entries };

    match serde_json::to_string(&result) {
        Ok(json) => {
            (axum::http::StatusCode::OK, [(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())], json)
                .into_response()
        }
        Err(_) => internal_server_error!(),
    }
}

/// Handles XMLTV EPG API requests, serving the appropriate EPG file with optional time-shifting based on user configuration.
///
/// Returns a 403 Forbidden response if the user or target is invalid or if the user lacks permission. If no EPG file is configured for the target, returns an empty EPG response. Otherwise, serves the EPG file, applying a time shift if specified by the user.
///
/// # Examples
///
/// ```
/// // Example usage within an Axum router:
/// let router = xmltv_api_register();
/// // A GET request to /xmltv.php with valid query parameters will invoke this handler.
/// ```
async fn xmltv_api(
    fingerprint: &Fingerprint,
    api_req: UserApiRequest,
    app_state: &Arc<AppState>,
) -> impl IntoResponse + Send {
    api_req.log_sanitized("xmltv_api");
    let auth_status = app_state.app_config.get_auth_error_status();
    let Some((user, target)) = get_user_target(&api_req, app_state) else {
        return auth_status.into_response();
    };
    if let Err(e) = resolve_api_user_context(
        user.clone(),
        target.clone(),
        fingerprint.clone(),
        &app_state.app_config,
        &app_state.geoip,
    ) {
        return e.into_player_response(auth_status);
    }

    let config = &app_state.app_config.config.load();
    let Some(epg_path) = get_epg_path_for_target(config, &target) else {
        return get_empty_epg_response();
    };

    serve_epg(app_state, &epg_path, &user, &target, None).await
}

async fn xmltv_api_get(
    fingerprint: Fingerprint,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Query(api_req): axum::extract::Query<UserApiRequest>,
) -> impl IntoResponse + Send {
    xmltv_api(&fingerprint, api_req, &app_state).await
}

async fn xmltv_api_post(
    fingerprint: Fingerprint,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    UserApiRequestQueryOrBody(api_req): UserApiRequestQueryOrBody,
) -> impl IntoResponse + Send {
    xmltv_api(&fingerprint, api_req, &app_state).await
}

async fn epg_api_resource(
    fingerprint: Fingerprint,
    req_headers: axum::http::HeaderMap,
    axum::extract::Query(api_req): axum::extract::Query<UserApiRequest>,
    axum::extract::Path((username, password, resource)): axum::extract::Path<(String, String, String)>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    let auth_status = app_state.app_config.get_auth_error_status();
    let Some((user, target)) = get_user_target_by_credentials(&username, &password, &api_req, &app_state) else {
        return auth_status.into_response();
    };
    if let Err(e) = resolve_api_user_context(
        user.clone(),
        target.clone(),
        fingerprint.clone(),
        &app_state.app_config,
        &app_state.geoip,
    ) {
        return e.into_player_response(auth_status);
    }

    let encrypt_secret = app_state.get_encrypt_secret();
    if let Ok(resource_url) = deobscure_text(&encrypt_secret, &resource) {
        resource_response(&app_state, &resource_url, &req_headers, None).await.into_response()
    } else {
        axum::http::StatusCode::BAD_REQUEST.into_response()
    }
}

/// Registers the XMLTV EPG API routes for handling HTTP GET requests.
///
/// The returned router maps the `/xmltv.php`, `/update/epg.php`, and `/epg` endpoints to the `xmltv_api` handler, enabling XMLTV EPG data retrieval with optional time-shifting and compression.
///
/// # Examples
///
/// ```
/// let router = xmltv_api_register();
/// // The router can now be used with an Axum server.
/// ```
pub fn xmltv_api_register() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/xmltv.php", axum::routing::get(xmltv_api_get).post(xmltv_api_post))
        .route("/epg", axum::routing::get(xmltv_api_get).post(xmltv_api_post))
        .route("/update/epg.php", axum::routing::get(xmltv_api_get).post(xmltv_api_post))
        .route(
            &format!("/{}/{{username}}/{{password}}/{{resource}}", storage_const::EPG_RESOURCE_PATH),
            axum::routing::get(epg_api_resource),
        )
}

#[cfg(test)]
mod tests {
    use super::{
        empty_stream_epg_entries, from_programme, get_epg_path_for_target, get_epg_path_for_target_by_type,
        group_stream_epg_items, prepare_stream_epg_request, rewrite_epg_channel_resource_url, serve_epg,
        serve_short_epg, serve_stream_epg, stream_epg_api, stream_epg_programmes_for_channel,
        write_programme_classification_tags, MAX_STREAM_EPG_CHANNEL_ID_BYTES, MAX_STREAM_EPG_ITEMS,
    };
    use crate::{
        api::model::{create_test_app_state, AppState},
        model::{
            Config, ConfigTarget, Epg, IcsEpgSourceConfig, M3uTargetOutput, ProxyUserCredentials, TargetOutput,
            XtreamTargetFlagsSet, XtreamTargetOutput,
        },
        processing::parser::ics::parse_ics_file_to_channel,
        repository::{epg_write_file, BPlusTree},
        utils::{lowercase_xmltv_text, EpgIdOutputCase, EpgProcessingOptions, EpgTimeShift},
    };
    use arc_swap::ArcSwapOption;
    use axum::response::IntoResponse;
    use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
    use shared::{
        foundation::Filter,
        model::{
            ConfigTargetOptions, EpgCategory, EpgChannel, EpgOutputOptions, EpgProgramme, ProcessingOrder,
            StreamEpgItemRequest, StreamEpgRequest, TargetType,
        },
        utils::{concat_path, obfuscate_text, Internable},
    };
    use std::{
        collections::HashMap,
        fs, io,
        pin::Pin,
        sync::Arc,
        task::{Context, Poll},
    };
    use tempfile::tempdir;
    use tokio::io::AsyncWrite;

    struct ErroringWriter;

    impl AsyncWrite for ErroringWriter {
        fn poll_write(self: Pin<&mut Self>, _cx: &mut Context<'_>, _buf: &[u8]) -> Poll<Result<usize, io::Error>> {
            Poll::Ready(Err(io::Error::new(io::ErrorKind::ConnectionReset, "synthetic write failure")))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    fn test_target_with_xtream_and_m3u() -> ConfigTarget {
        ConfigTarget {
            id: 1,
            enabled: true,
            name: "mixed-target".to_string(),
            options: None,
            sort: None,
            filter: Filter::default(),
            output: vec![
                TargetOutput::Xtream(XtreamTargetOutput {
                    flags: XtreamTargetFlagsSet::new(),
                    trakt: None,
                    filter: None,
                }),
                TargetOutput::M3u(M3uTargetOutput {
                    filename: None,
                    include_type_in_url: false,
                    mask_redirect_url: false,
                    filter: None,
                }),
            ],
            rename: None,
            mapping_ids: None,
            mapping: Arc::new(ArcSwapOption::new(None)),
            favourites: None,
            processing_order: ProcessingOrder::default(),
            execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
            watch: None,
            use_memory_cache: false,
        }
    }

    fn test_target_with_xtream_only() -> ConfigTarget {
        ConfigTarget {
            id: 1,
            enabled: true,
            name: "xtream-only".to_string(),
            options: None,
            sort: None,
            filter: Filter::default(),
            output: vec![TargetOutput::Xtream(XtreamTargetOutput {
                flags: XtreamTargetFlagsSet::new(),
                trakt: None,
                filter: None,
            })],
            rename: None,
            mapping_ids: None,
            mapping: Arc::new(ArcSwapOption::new(None)),
            favourites: None,
            processing_order: ProcessingOrder::default(),
            execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
            watch: None,
            use_memory_cache: false,
        }
    }

    fn test_config_with_storage(storage_dir: &str) -> Config {
        Config { storage_dir: storage_dir.to_string(), ..Default::default() }
    }

    fn test_app_state() -> Arc<AppState> { create_test_app_state(Config::default()) }

    fn test_target_with_epg_options(lowercase_ids: bool, lowercase_display_names: bool) -> Arc<ConfigTarget> {
        let mut target = test_target_with_xtream_only();
        target.options = Some(ConfigTargetOptions {
            epg_output: EpgOutputOptions { lowercase_ids, lowercase_xmltv_display_names: lowercase_display_names },
            ..ConfigTargetOptions::default()
        });
        Arc::new(target)
    }

    fn write_test_epg_db(path: &std::path::Path, channel: EpgChannel) { write_test_epg_channels(path, [channel]); }

    fn write_test_epg_channels(path: &std::path::Path, channels: impl IntoIterator<Item = EpgChannel>) {
        let mut tree = BPlusTree::<Arc<str>, EpgChannel>::new();
        for channel in channels {
            tree.insert(Arc::clone(&channel.id), channel);
        }
        tree.store(path).expect("test EPG DB should be written");
    }

    fn stream_epg_item(
        id: impl Into<String>,
        target_id: Option<u16>,
        reference_ts: Option<i64>,
    ) -> StreamEpgItemRequest {
        StreamEpgItemRequest { epg_channel_id: id.into(), target_id, reference_ts }
    }

    async fn response_body_text(response: axum::response::Response) -> String {
        let body =
            axum::body::to_bytes(response.into_body(), 64 * 1024).await.expect("response body should be readable");
        String::from_utf8(body.to_vec()).expect("response body should be UTF-8")
    }

    #[tokio::test]
    async fn regular_xmltv_output_contains_imported_ics_channel_and_programme() {
        let dir = tempdir().expect("temp dir");
        let ics_path = dir.path().join("calendar.ics");
        fs::write(
            &ics_path,
            concat!(
                "BEGIN:VCALENDAR\r\n",
                "VERSION:2.0\r\n",
                "BEGIN:VEVENT\r\n",
                "UID:f1-race\r\n",
                "DTSTART:20300310T140000Z\r\n",
                "DTEND:20300310T160000Z\r\n",
                "SUMMARY:Formula 1 Grand Prix\r\n",
                "DESCRIPTION:Imported calendar programme\r\n",
                "CATEGORIES:Motorsport,Live Event\r\n",
                "END:VEVENT\r\n",
                "END:VCALENDAR\r\n",
            ),
        )
        .expect("write ICS fixture");

        let channel =
            parse_ics_file_to_channel(&ics_path, "f1.calendar".intern(), None, &IcsEpgSourceConfig::default())
                .await
                .expect("parse ICS fixture");
        let epg_path = dir.path().join("epg.db");
        epg_write_file(
            "ics-target",
            &Epg { priority: 0, logo_override: false, attributes: None, children: vec![Arc::new(channel)] },
            &epg_path,
            &HashMap::<Arc<str>, Arc<str>>::new(),
            &EpgOutputOptions::default(),
        )
        .expect("write EPG database");

        let config = test_config_with_storage(dir.path().to_string_lossy().as_ref());
        let app_state = create_test_app_state(config);
        let target = Arc::new(test_target_with_xtream_and_m3u());
        let response = serve_epg(&app_state, &epg_path, &ProxyUserCredentials::default(), &target, None).await;
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("read XMLTV response body");
        let xml = String::from_utf8(body.to_vec()).expect("XMLTV response is UTF-8");

        assert!(xml.contains(r#"<channel id="f1.calendar">"#));
        assert!(xml.contains("<display-name>f1.calendar</display-name>"));
        assert!(xml.contains(r#"channel="f1.calendar""#));
        assert!(xml.contains("<title>Formula 1 Grand Prix</title>"));
        assert!(xml.contains("<desc>Imported calendar programme</desc>"));
        assert!(xml.contains("<category>Motorsport</category>"));
        assert!(xml.contains("<category>Live Event</category>"));
        assert!(!xml.contains("<live"));
        assert!(!xml.contains("<new"));
    }

    #[test]
    fn get_epg_path_for_target_by_type_prefers_requested_output() {
        let dir = tempdir().expect("temp dir");
        let config = test_config_with_storage(dir.path().to_string_lossy().as_ref());
        let target = test_target_with_xtream_and_m3u();

        let xtream_storage = crate::repository::xtream_get_storage_path(&config, &target.name).expect("xtream storage");
        let m3u_storage = crate::repository::get_target_storage_path(&config, &target.name).expect("m3u storage");
        let xtream_epg = crate::repository::xtream_get_epg_file_path_for_target(&xtream_storage);
        let m3u_epg = crate::repository::m3u_get_epg_file_path_for_target(&m3u_storage);

        fs::create_dir_all(xtream_storage).expect("create xtream dir");
        fs::create_dir_all(m3u_epg.parent().expect("m3u epg parent")).expect("create m3u dir");
        fs::write(&xtream_epg, b"xtream").expect("write xtream epg");
        fs::write(&m3u_epg, b"m3u").expect("write m3u epg");

        let picked_xtream = get_epg_path_for_target_by_type(&config, &target, TargetType::Xtream).expect("xtream path");
        let picked_m3u = get_epg_path_for_target_by_type(&config, &target, TargetType::M3u).expect("m3u path");

        assert_eq!(picked_xtream, xtream_epg);
        assert_eq!(picked_m3u, m3u_epg);
    }

    #[test]
    fn get_epg_path_for_target_keeps_output_order_fallback() {
        let dir = tempdir().expect("temp dir");
        let config = test_config_with_storage(dir.path().to_string_lossy().as_ref());
        let target = test_target_with_xtream_and_m3u();

        let xtream_storage = crate::repository::xtream_get_storage_path(&config, &target.name).expect("xtream storage");
        let m3u_storage = crate::repository::get_target_storage_path(&config, &target.name).expect("m3u storage");
        let xtream_epg = crate::repository::xtream_get_epg_file_path_for_target(&xtream_storage);
        let m3u_epg = crate::repository::m3u_get_epg_file_path_for_target(&m3u_storage);

        fs::create_dir_all(xtream_storage).expect("create xtream dir");
        fs::create_dir_all(m3u_epg.parent().expect("m3u epg parent")).expect("create m3u dir");
        fs::write(&xtream_epg, b"xtream").expect("write xtream epg");
        fs::write(&m3u_epg, b"m3u").expect("write m3u epg");

        let picked = get_epg_path_for_target(&config, &target).expect("fallback path");
        assert_eq!(picked, xtream_epg);
    }

    #[test]
    fn get_epg_path_for_target_xtream_only_target_uses_xtream_path() {
        // Regression: stream_epg_api used to probe m3u first regardless of the target's
        // configured outputs, producing a misleading "Can't find epg file" TRACE for
        // xtream-only targets. After the fix, get_epg_path_for_target iterates
        // target.output and must not look for an m3u epg at all.
        let dir = tempdir().expect("temp dir");
        let config = test_config_with_storage(dir.path().to_string_lossy().as_ref());
        let target = test_target_with_xtream_only();

        let xtream_storage = crate::repository::xtream_get_storage_path(&config, &target.name).expect("xtream storage");
        let xtream_epg = crate::repository::xtream_get_epg_file_path_for_target(&xtream_storage);

        fs::create_dir_all(&xtream_storage).expect("create xtream dir");
        fs::write(&xtream_epg, b"xtream").expect("write xtream epg");

        let picked = get_epg_path_for_target(&config, &target).expect("xtream path");
        assert_eq!(picked, xtream_epg);
    }

    #[test]
    fn test_group_stream_epg_items_rejects_empty_requests() {
        let err = group_stream_epg_items(Vec::new()).unwrap_err();
        assert_eq!(err, "items must not be empty");
    }

    #[test]
    fn test_group_stream_epg_items_rejects_missing_target() {
        let err = group_stream_epg_items(vec![stream_epg_item("epg-1", None, None)]).unwrap_err();
        assert_eq!(err, "all items must include target_id");
    }

    #[test]
    fn test_group_stream_epg_items_rejects_empty_channel_id() {
        let err = group_stream_epg_items(vec![stream_epg_item("", Some(1), None)]).unwrap_err();

        assert_eq!(err, "epg_channel_id must not be empty");
    }

    #[test]
    fn test_group_stream_epg_items_rejects_overlong_channel_id() {
        let err = group_stream_epg_items(vec![stream_epg_item(
            "x".repeat(MAX_STREAM_EPG_CHANNEL_ID_BYTES + 1),
            Some(1),
            None,
        )])
        .unwrap_err();

        assert_eq!(err, format!("epg_channel_id must not exceed {MAX_STREAM_EPG_CHANNEL_ID_BYTES} bytes"));
    }

    #[test]
    fn test_group_stream_epg_items_rejects_excessive_item_count() {
        let items =
            (0..=MAX_STREAM_EPG_ITEMS).map(|index| stream_epg_item(format!("epg-{index}"), Some(1), None)).collect();

        let err = group_stream_epg_items(items).unwrap_err();

        assert_eq!(err, format!("items must not exceed {MAX_STREAM_EPG_ITEMS} entries"));
    }

    #[test]
    fn test_group_stream_epg_items_accepts_configured_limits() {
        let mut items = (0..MAX_STREAM_EPG_ITEMS)
            .map(|index| stream_epg_item(format!("epg-{index}"), Some(1), None))
            .collect::<Vec<_>>();
        items[0].epg_channel_id = "x".repeat(MAX_STREAM_EPG_CHANNEL_ID_BYTES);

        let groups = group_stream_epg_items(items).expect("request at the configured limits should be accepted");

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].1.len(), MAX_STREAM_EPG_ITEMS);
    }

    #[tokio::test]
    async fn stream_epg_api_returns_bad_request_for_invalid_item() {
        let response = stream_epg_api(
            axum::extract::State(test_app_state()),
            axum::extract::Json(StreamEpgRequest { items: vec![stream_epg_item("", Some(1), None)] }),
        )
        .await
        .into_response();

        assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
        let body = response_body_text(response).await;
        let error = serde_json::from_str::<serde_json::Value>(&body).expect("bad-request response should contain JSON");
        assert_eq!(error["error"], "epg_channel_id must not be empty");
    }

    #[test]
    fn test_group_stream_epg_items_groups_by_target_in_input_order() {
        let groups = group_stream_epg_items(vec![
            stream_epg_item("epg-1", Some(2), None),
            stream_epg_item("epg-2", Some(5), None),
            stream_epg_item("epg-3", Some(2), None),
        ])
        .unwrap();

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, 2);
        assert_eq!(
            groups[0].1.iter().map(|item| item.epg_channel_id.as_str()).collect::<Vec<_>>(),
            vec!["epg-1", "epg-3"]
        );
        assert_eq!(groups[1].0, 5);
        assert_eq!(groups[1].1.iter().map(|item| item.epg_channel_id.as_str()).collect::<Vec<_>>(), vec!["epg-2"]);
    }

    #[test]
    fn stream_epg_archive_window_uses_reference() {
        let reference = 1_700_000_000_i64 - 3_600;
        let window_start = reference.saturating_sub(super::STREAM_EPG_ARCHIVE_WINDOW_BACK_SECS);
        let window_end = reference.saturating_add(super::STREAM_EPG_ARCHIVE_WINDOW_FWD_SECS);
        assert!(window_start < reference);
        assert!(window_end > reference);
    }

    #[test]
    fn stream_epg_request_keeps_first_available_reference_per_exact_id() {
        let items = [
            stream_epg_item("epg-1", Some(1), Some(100)),
            stream_epg_item("epg-1", Some(1), Some(200)),
            stream_epg_item("epg-2", Some(1), None),
            stream_epg_item("epg-2", Some(1), Some(300)),
        ];
        let prepared = prepare_stream_epg_request(&items, EpgIdOutputCase::Preserve);

        assert_eq!(prepared.entries.len(), 2);
        assert_eq!(prepared.entries[0].storage_key.as_ref(), "epg-1");
        assert_eq!(prepared.entries[0].reference_ts, Some(100));
        assert_eq!(prepared.entries[1].storage_key.as_ref(), "epg-2");
        assert_eq!(prepared.entries[1].reference_ts, Some(300));
    }

    #[test]
    fn stream_epg_request_canonicalizes_before_deduplication() {
        let items = [
            stream_epg_item("Example.Channel", Some(1), Some(100)),
            stream_epg_item("example.CHANNEL", Some(1), Some(200)),
            stream_epg_item("ÄBC.Id", Some(1), None),
        ];

        let prepared = prepare_stream_epg_request(&items, EpgIdOutputCase::LowercaseAscii);
        let storage_keys = prepared.entries.iter().map(|entry| entry.storage_key.as_ref()).collect::<Vec<_>>();

        assert_eq!(storage_keys, vec!["example.channel", "Äbc.id"]);
        assert_eq!(prepared.entries[0].reference_ts, Some(100));
        assert_eq!(prepared.entries[1].reference_ts, None);
    }

    #[test]
    fn stream_epg_request_uses_non_interned_lookup_ids() {
        let globally_interned = "request.example".intern();
        let items = [stream_epg_item("REQUEST.Example", Some(1), None)];

        let prepared = prepare_stream_epg_request(&items, EpgIdOutputCase::LowercaseAscii);

        assert_eq!(prepared.entries[0].storage_key.as_ref(), globally_interned.as_ref());
        assert!(!Arc::ptr_eq(&prepared.entries[0].storage_key, &globally_interned));

        let preserve_prepared = prepare_stream_epg_request(&items, EpgIdOutputCase::Preserve);
        let preserve_interned = "REQUEST.Example".intern();
        assert_eq!(preserve_prepared.entries[0].storage_key.as_ref(), preserve_interned.as_ref());
        assert!(!Arc::ptr_eq(&preserve_prepared.entries[0].storage_key, &preserve_interned));
    }

    #[test]
    fn stream_epg_request_preserves_case_distinct_entries_by_default() {
        let items = [
            stream_epg_item("Example.Channel", Some(1), Some(100)),
            stream_epg_item("example.CHANNEL", Some(1), Some(200)),
        ];

        let prepared = prepare_stream_epg_request(&items, EpgIdOutputCase::Preserve);

        assert_eq!(prepared.entries.len(), 2);
        assert_eq!(prepared.entries[0].storage_key.as_ref(), "Example.Channel");
        assert_eq!(prepared.entries[0].reference_ts, Some(100));
        assert_eq!(prepared.entries[1].storage_key.as_ref(), "example.CHANNEL");
        assert_eq!(prepared.entries[1].reference_ts, Some(200));
    }

    #[test]
    fn empty_stream_epg_entries_use_prepared_canonical_ids() {
        let items =
            [stream_epg_item("Example.Channel", Some(7), None), stream_epg_item("example.CHANNEL", Some(7), None)];
        let prepared = prepare_stream_epg_request(&items, EpgIdOutputCase::LowercaseAscii);

        let entries = empty_stream_epg_entries(7, &prepared);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].epg_channel_id, "example.channel");
        assert_eq!(entries[0].target_id, Some(7));
        assert!(entries[0].programmes.is_empty());

        let preserve_prepared = prepare_stream_epg_request(&items, EpgIdOutputCase::Preserve);
        let preserve_entries = empty_stream_epg_entries(7, &preserve_prepared);
        assert_eq!(preserve_entries.len(), 2);
        assert_eq!(preserve_entries[0].epg_channel_id, "Example.Channel");
        assert_eq!(preserve_entries[1].epg_channel_id, "example.CHANNEL");
    }

    #[test]
    fn short_epg_response_uses_canonical_id_for_both_id_fields() {
        let stream_id = "42".intern();
        let epg_id = "example.channel".intern();
        let programme = EpgProgramme::new_all(100, 200, Arc::clone(&epg_id), Some("News".intern()), None, None);
        let options =
            EpgProcessingOptions { rewrite_urls: false, time_shift: EpgTimeShift::None, encrypt_secret: [0; 16] };

        let result = from_programme(&stream_id, &epg_id, &programme, &options, false);

        assert_eq!(result.epg_id.as_ref(), "example.channel");
        assert_eq!(result.channel_id.as_ref(), "example.channel");
    }

    #[test]
    fn from_programme_sets_has_archive_only_when_capability_true() {
        let stream_id = "42".intern();
        let epg_id = "example.channel".intern();
        let programme = EpgProgramme::new_all(100, 200, Arc::clone(&epg_id), Some("News".intern()), None, None);
        let options =
            EpgProcessingOptions { rewrite_urls: false, time_shift: EpgTimeShift::None, encrypt_secret: [0; 16] };

        let archived = from_programme(&stream_id, &epg_id, &programme, &options, true);
        let live_only = from_programme(&stream_id, &epg_id, &programme, &options, false);

        assert_eq!(archived.has_archive, Some(1));
        assert_eq!(live_only.has_archive, None);
    }

    #[test]
    fn lowercased_xmltv_display_name_remains_xml_escaped() {
        let display_name = lowercase_xmltv_text("CAFÉ NETWORK & <HD>", true);
        let mut writer = quick_xml::Writer::new(Vec::new());
        writer.write_event(Event::Start(BytesStart::new("display-name"))).expect("display-name start should serialize");
        writer
            .write_event(Event::Text(BytesText::new(display_name.as_ref())))
            .expect("display-name text should serialize");
        writer.write_event(Event::End(BytesEnd::new("display-name"))).expect("display-name end should serialize");

        let xml = String::from_utf8(writer.into_inner()).expect("serialized XML should be UTF-8");

        assert_eq!(xml, "<display-name>café network &amp; &lt;hd&gt;</display-name>");
    }

    #[tokio::test]
    async fn xmltv_output_uses_canonical_channel_ids_and_only_lowercases_display_names() {
        let dir = tempdir().expect("temp dir");
        let epg_path = dir.path().join("epg.db");
        let mut programme = EpgProgramme::new_all(
            100,
            200,
            "source.MixedCase".intern(),
            Some("News & Updates".intern()),
            Some("Keep <Case>".intern()),
            None,
        );
        programme.categories = vec![
            EpgCategory { value: "News & Analysis".intern(), lang: Some("en".intern()) },
            EpgCategory { value: "Sports".intern(), lang: None },
        ];
        programme.is_live = true;
        programme.is_new = true;
        write_test_epg_db(
            &epg_path,
            EpgChannel {
                id: "example.channel".intern(),
                title: Some("CAFÉ NETWORK & <HD>".intern()),
                icon: None,
                programmes: vec![programme],
            },
        );
        let app_state = test_app_state();
        let target = test_target_with_epg_options(true, true);

        let response = serve_epg(&app_state, &epg_path, &ProxyUserCredentials::default(), &target, None).await;
        let xml = response_body_text(response).await;

        assert!(xml.contains(r#"<channel id="example.channel">"#), "XMLTV output: {xml}");
        assert!(xml.contains(
            r#"<programme start="19700101000140 +0000" stop="19700101000320 +0000" channel="example.channel">"#
        ));
        assert!(xml.contains("<display-name>café network &amp; &lt;hd&gt;</display-name>"));
        assert!(xml.contains("<title>News &amp; Updates</title>"));
        assert!(xml.contains("<desc>Keep &lt;Case&gt;</desc>"));
        assert!(!xml.contains("<title>news &amp; updates</title>"));
        assert!(xml.contains(r#"<category lang="en">News &amp; Analysis</category>"#));
        assert!(xml.contains("<category>Sports</category>"));
        assert!(xml.contains("<live/>"));
        assert!(xml.contains("<new/>"));

        let title_pos = xml.find("<title>News").expect("title position");
        let desc_pos = xml.find("<desc>Keep").expect("desc position");
        let first_category_pos = xml.find("<category lang=\"en\">News").expect("first category position");
        let second_category_pos = xml.find("<category>Sports").expect("second category position");
        let live_pos = xml.find("<live/>").expect("live position");
        let new_pos = xml.find("<new/>").expect("new position");
        assert!(title_pos < desc_pos);
        assert!(desc_pos < first_category_pos);
        assert!(first_category_pos < second_category_pos);
        assert!(second_category_pos < live_pos);
        assert!(live_pos < new_pos);
    }

    #[tokio::test]
    async fn programme_classification_writer_propagates_io_errors() {
        let mut programme = EpgProgramme::new(100, 200, "channel".intern());
        programme.categories = vec![EpgCategory { value: "Sports".intern(), lang: None }];
        let mut writer = quick_xml::Writer::new(ErroringWriter);

        assert!(write_programme_classification_tags(&mut writer, &programme).await.is_err());
    }

    #[tokio::test]
    async fn xmltv_output_preserves_ids_and_display_names_when_options_are_disabled() {
        let dir = tempdir().expect("temp dir");
        let epg_path = dir.path().join("epg.db");
        write_test_epg_db(
            &epg_path,
            EpgChannel {
                id: "Example.Channel".intern(),
                title: Some("MixedCase Network".intern()),
                icon: None,
                programmes: vec![EpgProgramme::new(100, 200, "different.source.case".intern())],
            },
        );
        let app_state = test_app_state();
        let target = Arc::new(test_target_with_xtream_only());

        let response = serve_epg(&app_state, &epg_path, &ProxyUserCredentials::default(), &target, None).await;
        let xml = response_body_text(response).await;

        assert!(xml.contains(r#"<channel id="Example.Channel">"#));
        assert!(xml.contains(r#"channel="Example.Channel""#));
        assert!(xml.contains("<display-name>MixedCase Network</display-name>"));
    }

    #[tokio::test]
    async fn xmltv_output_preserves_legacy_mixed_case_key_order_when_options_are_disabled() {
        let dir = tempdir().expect("temp dir");
        let epg_path = dir.path().join("epg.db");
        write_test_epg_channels(
            &epg_path,
            [
                EpgChannel {
                    id: "Z.Channel".intern(),
                    title: Some("First Network".intern()),
                    icon: None,
                    programmes: vec![EpgProgramme::new(100, 200, "Z.Channel".intern())],
                },
                EpgChannel {
                    id: "a.channel".intern(),
                    title: Some("Second Network".intern()),
                    icon: None,
                    programmes: vec![EpgProgramme::new(100, 200, "a.channel".intern())],
                },
            ],
        );
        let app_state = test_app_state();
        let target = Arc::new(test_target_with_xtream_only());

        let response = serve_epg(&app_state, &epg_path, &ProxyUserCredentials::default(), &target, None).await;
        let xml = response_body_text(response).await;
        let first_position = xml.find(r#"<channel id="Z.Channel">"#).expect("first channel should exist");
        let second_position = xml.find(r#"<channel id="a.channel">"#).expect("second channel should exist");

        assert!(first_position < second_position, "XMLTV output should preserve legacy key order: {xml}");
    }

    #[tokio::test]
    async fn xmltv_output_lowercases_only_display_name_when_id_option_is_disabled() {
        let dir = tempdir().expect("temp dir");
        let epg_path = dir.path().join("epg.db");
        write_test_epg_db(
            &epg_path,
            EpgChannel {
                id: "Example.Channel".intern(),
                title: Some("CAFÉ NETWORK".intern()),
                icon: None,
                programmes: vec![EpgProgramme::new_all(
                    100,
                    200,
                    "Example.Channel".intern(),
                    Some("MixedCase Programme".intern()),
                    Some("Keep <Case>".intern()),
                    None,
                )],
            },
        );
        let app_state = test_app_state();
        let target = test_target_with_epg_options(false, true);

        let response = serve_epg(&app_state, &epg_path, &ProxyUserCredentials::default(), &target, None).await;
        let xml = response_body_text(response).await;

        assert!(xml.contains(r#"<channel id="Example.Channel">"#));
        assert!(xml.contains(r#"channel="Example.Channel""#));
        assert!(xml.contains("<display-name>café network</display-name>"));
        assert!(xml.contains("<title>MixedCase Programme</title>"));
        assert!(xml.contains("<desc>Keep &lt;Case&gt;</desc>"));
    }

    #[tokio::test]
    async fn xmltv_output_lowercases_only_ids_when_display_name_option_is_disabled() {
        let dir = tempdir().expect("temp dir");
        let epg_path = dir.path().join("epg.db");
        write_test_epg_db(
            &epg_path,
            EpgChannel {
                id: "example.channel".intern(),
                title: Some("MixedCase Network".intern()),
                icon: None,
                programmes: vec![EpgProgramme::new_all(
                    100,
                    200,
                    "example.channel".intern(),
                    Some("MixedCase Programme".intern()),
                    Some("Keep <Case>".intern()),
                    None,
                )],
            },
        );
        let app_state = test_app_state();
        let target = test_target_with_epg_options(true, false);

        let response = serve_epg(&app_state, &epg_path, &ProxyUserCredentials::default(), &target, None).await;
        let xml = response_body_text(response).await;

        assert!(xml.contains(r#"<channel id="example.channel">"#));
        assert!(xml.contains(r#"channel="example.channel""#));
        assert!(xml.contains("<display-name>MixedCase Network</display-name>"));
        assert!(xml.contains("<title>MixedCase Programme</title>"));
        assert!(xml.contains("<desc>Keep &lt;Case&gt;</desc>"));
    }

    #[tokio::test]
    async fn short_epg_uses_target_output_case_for_legacy_and_lowercase_storage_keys() {
        let dir = tempdir().expect("temp dir");
        let lowercase_epg_path = dir.path().join("lowercase-epg.db");
        let legacy_epg_path = dir.path().join("legacy-epg.db");
        let now = chrono::Utc::now().timestamp();
        write_test_epg_db(
            &lowercase_epg_path,
            EpgChannel {
                id: "example.channel".intern(),
                title: Some("Sample Network".intern()),
                icon: None,
                programmes: vec![EpgProgramme::new_all(
                    now - 60,
                    now + 3600,
                    "example.channel".intern(),
                    Some("Current".intern()),
                    None,
                    None,
                )],
            },
        );
        write_test_epg_db(
            &legacy_epg_path,
            EpgChannel {
                id: "Example.Channel".intern(),
                title: Some("Legacy Network".intern()),
                icon: None,
                programmes: vec![EpgProgramme::new_all(
                    now - 60,
                    now + 3600,
                    "Example.Channel".intern(),
                    Some("Legacy Current".intern()),
                    None,
                    None,
                )],
            },
        );
        let legacy_db_before = fs::read(&legacy_epg_path).expect("legacy EPG DB should be readable");
        let app_state = test_app_state();
        let target = test_target_with_epg_options(true, false);
        let request_id = "Example.Channel".intern();

        let response = serve_short_epg(
            &app_state,
            &lowercase_epg_path,
            &ProxyUserCredentials::default(),
            &target,
            &request_id,
            "42".intern(),
            4,
            false,
        )
        .await;
        let body = response_body_text(response).await;
        let result = serde_json::from_str::<shared::model::ShortEpgResultDto>(&body)
            .expect("short EPG response should deserialize");

        assert_eq!(result.epg_listings.len(), 1);
        assert_eq!(result.epg_listings[0].epg_id.as_ref(), "example.channel");
        assert_eq!(result.epg_listings[0].channel_id.as_ref(), "example.channel");

        let disabled_target = Arc::new(test_target_with_xtream_only());
        let disabled_response = serve_short_epg(
            &app_state,
            &legacy_epg_path,
            &ProxyUserCredentials::default(),
            &disabled_target,
            &request_id,
            "42".intern(),
            4,
            false,
        )
        .await;
        let disabled_body = response_body_text(disabled_response).await;
        let disabled_result = serde_json::from_str::<shared::model::ShortEpgResultDto>(&disabled_body)
            .expect("disabled short EPG response should deserialize");
        assert_eq!(disabled_result.epg_listings.len(), 1);
        assert_eq!(disabled_result.epg_listings[0].epg_id.as_ref(), "Example.Channel");
        assert_eq!(disabled_result.epg_listings[0].channel_id.as_ref(), "Example.Channel");
        assert_eq!(
            fs::read(&legacy_epg_path).expect("legacy EPG DB should remain readable"),
            legacy_db_before,
            "serving a legacy EPG DB must not rewrite its key schema"
        );
    }

    #[tokio::test]
    async fn stream_epg_lowercase_mode_deduplicates_case_variants_and_keeps_first_reference() {
        let dir = tempdir().expect("temp dir");
        let epg_path = dir.path().join("epg.db");
        let first_reference = 1_700_000_000;
        write_test_epg_db(
            &epg_path,
            EpgChannel {
                id: "example.channel".intern(),
                title: Some("Sample Network".intern()),
                icon: None,
                programmes: vec![EpgProgramme::new_all(
                    first_reference - 60,
                    first_reference + 60,
                    "example.channel".intern(),
                    Some("First Window".intern()),
                    None,
                    None,
                )],
            },
        );
        let app_state = test_app_state();
        let target = test_target_with_epg_options(true, false);
        let items = [
            stream_epg_item("Example.Channel", Some(target.id), Some(first_reference)),
            stream_epg_item("example.CHANNEL", Some(target.id), Some(first_reference + 7 * 24 * 3600)),
        ];
        let prepared = prepare_stream_epg_request(&items, EpgIdOutputCase::LowercaseAscii);

        let response =
            serve_stream_epg(&app_state, &ProxyUserCredentials::default(), &target, &epg_path, prepared).await;

        assert_eq!(response.entries.len(), 1);
        assert_eq!(response.entries[0].epg_channel_id, "example.channel");
        assert_eq!(response.entries[0].programmes.len(), 1);
        assert_eq!(response.entries[0].programmes[0].title, "First Window");
    }

    #[tokio::test]
    async fn stream_epg_preserve_mode_keeps_case_distinct_legacy_entries() {
        let dir = tempdir().expect("temp dir");
        let epg_path = dir.path().join("legacy-epg.db");
        let first_reference = 1_700_000_000;
        let second_reference = first_reference + 7 * 24 * 3600;
        write_test_epg_channels(
            &epg_path,
            [
                EpgChannel {
                    id: "Example.Channel".intern(),
                    title: Some("First Network".intern()),
                    icon: None,
                    programmes: vec![EpgProgramme::new_all(
                        first_reference - 60,
                        first_reference + 60,
                        "Example.Channel".intern(),
                        Some("First Window".intern()),
                        None,
                        None,
                    )],
                },
                EpgChannel {
                    id: "example.CHANNEL".intern(),
                    title: Some("Second Network".intern()),
                    icon: None,
                    programmes: vec![EpgProgramme::new_all(
                        second_reference - 60,
                        second_reference + 60,
                        "example.CHANNEL".intern(),
                        Some("Second Window".intern()),
                        None,
                        None,
                    )],
                },
            ],
        );
        let app_state = test_app_state();
        let target = Arc::new(test_target_with_xtream_only());
        let items = [
            stream_epg_item("Example.Channel", Some(target.id), Some(first_reference)),
            stream_epg_item("example.CHANNEL", Some(target.id), Some(second_reference)),
        ];
        let prepared = prepare_stream_epg_request(&items, EpgIdOutputCase::Preserve);

        let response =
            serve_stream_epg(&app_state, &ProxyUserCredentials::default(), &target, &epg_path, prepared).await;

        assert_eq!(response.entries.len(), 2);
        assert_eq!(response.entries[0].epg_channel_id, "Example.Channel");
        assert_eq!(response.entries[0].programmes.len(), 1);
        assert_eq!(response.entries[0].programmes[0].title, "First Window");
        assert_eq!(response.entries[1].epg_channel_id, "example.CHANNEL");
        assert_eq!(response.entries[1].programmes.len(), 1);
        assert_eq!(response.entries[1].programmes[0].title, "Second Window");
    }

    fn sample_channel(icon: Option<&str>) -> EpgChannel {
        EpgChannel {
            id: "channel-1".intern(),
            title: Some("Channel".intern()),
            icon: icon.map(Internable::intern),
            programmes: Vec::new(),
        }
    }

    #[test]
    fn rewrite_epg_channel_resource_url_wraps_external_icon() {
        let secret = [9u8; 16];
        let resource_url = "/api/v1/playlist/resource";
        let channel = sample_channel(Some("https://cdn.example.com/logo.png"));

        let rewritten = rewrite_epg_channel_resource_url(&secret, resource_url, channel);

        assert_eq!(
            rewritten.icon.as_deref(),
            Some(concat_path(resource_url, &obfuscate_text(&secret, "https://cdn.example.com/logo.png")).as_str())
        );
    }

    #[test]
    fn rewrite_epg_channel_resource_url_keeps_internal_path() {
        let secret = [9u8; 16];
        let resource_url = "/api/v1/playlist/resource";
        let channel = sample_channel(Some("/api/v1/library/thumbnail/test"));

        let rewritten = rewrite_epg_channel_resource_url(&secret, resource_url, channel);

        assert_eq!(rewritten.icon.as_deref(), Some("/api/v1/library/thumbnail/test"));
    }

    #[test]
    fn stream_epg_programmes_for_channel_filters_using_shifted_fixed_times() {
        let window_start = 10_000;
        let window_end = window_start + 8 * 3600;
        let epg_processing_options =
            EpgProcessingOptions { rewrite_urls: false, time_shift: EpgTimeShift::Fixed(120), encrypt_secret: [0; 16] };
        let programmes = vec![
            EpgProgramme::new_all(
                window_start - 7_300,
                window_start - 100,
                "channel-1".intern(),
                Some("Shifted Into Window".intern()),
                None,
                None,
            ),
            EpgProgramme::new_all(
                window_start + 60,
                window_start + 600,
                "channel-1".intern(),
                Some("Already In Window".intern()),
                None,
                None,
            ),
            EpgProgramme::new_all(
                window_end + 60,
                window_end + 600,
                "channel-1".intern(),
                Some("Still Outside Window".intern()),
                None,
                None,
            ),
        ];

        let filtered =
            stream_epg_programmes_for_channel(&programmes, &epg_processing_options, window_start, window_end);

        assert_eq!(
            filtered.iter().map(|programme| programme.title.as_str()).collect::<Vec<_>>(),
            vec!["Already In Window"]
        );
        assert_eq!(filtered[0].start_timestamp, window_start + 7_260);
        assert_eq!(filtered[0].stop_timestamp, window_start + 7_800);
    }
}
