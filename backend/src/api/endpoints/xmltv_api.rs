use crate::{
    api::{
        api_utils::{
            empty_json_response_as_array, get_user_target, get_user_target_by_credentials, internal_server_error,
            resource_response, stream_json_or_bin_response_stream, try_option_forbidden, try_unwrap_body,
        },
        model::{AppState, UserApiRequest},
    },
    model::{Config, ConfigTarget, ProxyUserCredentials, TargetOutput, EPG_ATTRIB_ID, EPG_TAG_CHANNEL},
    repository::{
        get_target_storage_path, m3u_get_epg_file_path_for_target, storage_const, xtream_get_epg_file_path_for_target,
        xtream_get_storage_path, BPlusTreeQuery, LockedReceiverStream, XML_PREAMBLE,
    },
    utils,
    utils::{
        deobscure_text, file_exists_async, format_xmltv_time_utc, get_epg_processing_options, obscure_text,
        EpgProcessingOptions, EpgTimeShift,
    },
};
use axum::response::IntoResponse;
use chrono::{DateTime, TimeZone};
use log::{debug, error, trace};
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use shared::{
    concat_string,
    model::{EpgChannel, EpgProgramme, ShortEpgDto, ShortEpgResultDto},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{io::AsyncWriteExt, sync::mpsc, task};
use tokio_util::io::ReaderStream;

pub fn get_empty_epg_response() -> axum::response::Response {
    try_unwrap_body!(axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, axum::http::HeaderValue::from_static("text/xml"))
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
        match output {
            TargetOutput::Xtream(_) => {
                if let Some(storage_path) = xtream_get_storage_path(config, &target.name) {
                    return get_epg_path_for_target_of_type(
                        &target.name,
                        xtream_get_epg_file_path_for_target(&storage_path),
                    );
                }
            }
            TargetOutput::M3u(_) => {
                if let Some(target_path) = get_target_storage_path(config, &target.name) {
                    return get_epg_path_for_target_of_type(
                        &target.name,
                        m3u_get_epg_file_path_for_target(&target_path),
                    );
                }
            }
            TargetOutput::Strm(_) | TargetOutput::HdHomeRun(_) => {}
        }
    }
    None
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
        let iter_lock = app_state.app_config.file_locks.read_lock(epg_path).await;
        let bg_lock = app_state.app_config.file_locks.read_lock(epg_path).await;
        let epg_path = epg_path.to_path_buf();
        let target_name = target.name.clone();
        let (tx, rx) = mpsc::channel::<EpgChannel>(64);

        let epg_path_for_log = epg_path.clone();
        let target_name_for_log = target_name.clone();
        let handle = task::spawn_blocking(move || {
            let _guard = bg_lock;
            let Ok(query) = BPlusTreeQuery::<Arc<str>, EpgChannel>::try_new(&epg_path) else {
                error!("Failed to open epg db for target {} {}", target_name, epg_path.display());
                return;
            };
            for (_, channel) in query.disk_iter() {
                if tx.blocking_send(channel).is_err() {
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
            }
        });

        let stream = LockedReceiverStream::new(rx, iter_lock);
        return stream_json_or_bin_response_stream(accept, stream);
    }
    try_unwrap_body!(empty_json_response_as_array())
}

macro_rules! continue_on_err {
    ($expr:expr) => {
        if let Err(_err) = $expr {
            continue;
        }
    };
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

    let epg_processing_options = get_epg_processing_options(app_state, user, target);

    let server_info = app_state.app_config.get_user_server_info(user);
    let base_url =
        if !matches!(epg_processing_options.time_shift, EpgTimeShift::None) || epg_processing_options.rewrite_urls {
            Some(concat_string!(
                &server_info.get_base_url(),
                "/",
                storage_const::EPG_RESOURCE_PATH,
                "/",
                &user.username,
                "/",
                &user.password
            ))
        } else {
            None
        };

    let limit = limit.unwrap_or_default();

    let bg_lock = app_state.app_config.file_locks.read_lock(epg_path).await;
    let epg_path = epg_path.to_path_buf();
    let (channel_tx, mut channel_rx) = mpsc::channel::<EpgChannel>(256);

    let epg_path_for_log = epg_path.clone();
    let spawn_handle = task::spawn_blocking(move || {
        let _guard = bg_lock;
        let Ok(mut query) = BPlusTreeQuery::<Arc<str>, EpgChannel>::try_new(&epg_path) else {
            error!("Failed to open BPlusTreeQuery {}", epg_path.display());
            return;
        };

        for (_, channel) in query.iter() {
            if channel_tx.blocking_send(channel).is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        if let Err(err) = spawn_handle.await {
            error!("EPG rewrite producer task failed for {}: {err}", epg_path_for_log.display());
        }
    });


    let generator_info = server_info.get_base_url();

    let (mut tx, rx) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        // Work-Around BytesText DocType escape, see below
        if let Err(err) = tx.write_all(XML_PREAMBLE.as_ref()).await {
            error!("EPG: Failed to write xml header {err}");
            return;
        }
        if let Err(err) = tx.write_all(format!(r#"<tv generator-info-name="X" generator-info-url="{generator_info}">"#).as_bytes()).await
        {
            error!("EPG: Failed to write xml tv header {err}");
            return;
        }

        let mut writer = quick_xml::writer::Writer::new(tx);
        while let Some(channel) = channel_rx.recv().await {
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
                continue_on_err!(writer.write_event_async(Event::Text(BytesText::new(title))).await);

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
        .body(axum::body::Body::from_stream(body_stream)))
}

async fn get_epg_channel(app_state: &Arc<AppState>, channel_id: &Arc<str>, epg_path: &Path) -> Option<EpgChannel> {
    let file_lock = app_state.app_config.file_locks.read_lock(epg_path).await;
    let epg_path = epg_path.to_path_buf();
    let channel_id = Arc::clone(channel_id);

    match task::spawn_blocking(move || -> Option<EpgChannel> {
        let _guard = file_lock;
        match BPlusTreeQuery::<Arc<str>, EpgChannel>::try_new(&epg_path) {
            Ok(mut query) => match query.query(&channel_id) {
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
    match &epg_processing_options.time_shift {
        EpgTimeShift::None => {
            (format_xmltv_time(programme.start), format_xmltv_time(programme.stop), programme.start, programme.stop)
        }
        EpgTimeShift::Fixed(m) => {
            let off = i64::from(*m) * 60;
            let s = programme.start + off;
            let e = programme.stop + off;
            (format_xmltv_time(s), format_xmltv_time(e), s, e)
        }
        EpgTimeShift::TimeZone(tz) => {
            let s_dt = chrono::Utc.timestamp_opt(programme.start, 0).unwrap().with_timezone(tz);
            let e_dt = chrono::Utc.timestamp_opt(programme.stop, 0).unwrap().with_timezone(tz);
            // We use the original timestamps (programme.start/stop) here because TimeZone adjustment
            // is only for the visual string representation. The absolute event time (UTC) remains unchanged.
            // Unlike 'Fixed' offset which artificially shifts the event time.
            (
                s_dt.format("%Y-%m-%d %H:%M:%S").to_string(),
                e_dt.format("%Y-%m-%d %H:%M:%S").to_string(),
                programme.start,
                programme.stop,
            )
        }
    }
}

fn from_programme(
    stream_id: &Arc<str>,
    epg_id: &Arc<str>,
    programme: &EpgProgramme,
    epg_processing_options: &EpgProcessingOptions,
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
        has_archive: None,
    }
}

const DEFAULT_SHORT_EPG_LIMIT: u32 = 4;

pub async fn serve_short_epg(
    app_state: &Arc<AppState>,
    epg_path: &Path,
    user: &ProxyUserCredentials,
    target: &Arc<ConfigTarget>,
    channel_id: &Arc<str>,
    stream_id: Arc<str>,
    limit: u32,
) -> axum::response::Response {
    let short_epg = {
        // It seems provider set limit to 4 if it is undefined oor 0.
        let limit = if limit > 0 { limit } else { DEFAULT_SHORT_EPG_LIMIT };
        if file_exists_async(epg_path).await {
            if let Some(epg_channel) = get_epg_channel(app_state, channel_id, epg_path).await {
                let epg_processing_options = get_epg_processing_options(app_state, user, target);
                ShortEpgResultDto {
                    epg_listings: if limit > 0 {
                        epg_channel
                            .get_programme_with_limit(limit)
                            .iter()
                            .map(|p| from_programme(&stream_id, channel_id, p, &epg_processing_options))
                            .collect()
                    } else {
                        epg_channel
                            .programmes
                            .iter()
                            .map(|p| from_programme(&stream_id, channel_id, p, &epg_processing_options))
                            .collect()
                    },
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
async fn xmltv_api(api_req: UserApiRequest, app_state: &Arc<AppState>) -> impl IntoResponse + Send {
    let auth_status = app_state.app_config.get_auth_error_status();
    let (user, target) = try_option_forbidden!(
        get_user_target(&api_req, app_state),
        auth_status,
        false,
        format!("Could not find any user for xmltv api {}", api_req.username)
    );

    if user.permission_denied(app_state) {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }

    let config = &app_state.app_config.config.load();
    let Some(epg_path) = get_epg_path_for_target(config, &target) else {
        // No epg configured,  No processing or timeshift, epg can't be mapped to the channels.
        // we do not deliver epg
        return get_empty_epg_response();
    };

    serve_epg(app_state, &epg_path, &user, &target, None).await
}

async fn xmltv_api_get(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Query(api_req): axum::extract::Query<UserApiRequest>,
) -> impl IntoResponse + Send {
    xmltv_api(api_req, &app_state).await
}

async fn xmltv_api_post(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Query(api_query_req): axum::extract::Query<UserApiRequest>,
    api_form_req: Result<axum::extract::Form<UserApiRequest>, axum::extract::rejection::FormRejection>,
) -> impl IntoResponse + Send {
    if let Err(ref rejection) = api_form_req {
        debug!("xmltv_api_post: form parsing failed: {rejection:?}");
    }
    let form_req = api_form_req.as_ref().ok().map(|form| &form.0);
    let api_req = UserApiRequest::merge_query_over_form(&api_query_req, form_req);
    xmltv_api(api_req, &app_state).await
}

async fn epg_api_resource(
    req_headers: axum::http::HeaderMap,
    axum::extract::Query(api_req): axum::extract::Query<UserApiRequest>,
    axum::extract::Path((username, password, resource)): axum::extract::Path<(String, String, String)>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    let auth_status = app_state.app_config.get_auth_error_status();
    let (user, _target) = try_option_forbidden!(
        get_user_target_by_credentials(&username, &password, &api_req, &app_state),
        auth_status,
        false,
        format!("Could not find any user for epg resource {username}")
    );
    if user.permission_denied(&app_state) {
        return axum::http::StatusCode::FORBIDDEN.into_response();
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
        .route("/xmltv.php", axum::routing::get(xmltv_api_get))
        .route("/xmltv.php", axum::routing::post(xmltv_api_post))
        .route("/epg", axum::routing::get(xmltv_api_get))
        .route("/epg", axum::routing::post(xmltv_api_post))
        .route("/update/epg.php", axum::routing::get(xmltv_api_get))
        .route("/update/epg.php", axum::routing::post(xmltv_api_post))
        .route(
            &format!("/{}/{{username}}/{{password}}/{{resource}}", storage_const::EPG_RESOURCE_PATH),
            axum::routing::get(epg_api_resource),
        )
}
