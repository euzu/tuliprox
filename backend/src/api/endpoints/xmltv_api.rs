use crate::api::api_utils::{empty_json_response_as_array, get_user_target, get_user_target_by_credentials, internal_server_error, resource_response, stream_json_or_bin_response, try_unwrap_body};
use crate::api::model::AppState;
use crate::api::model::UserApiRequest;
use crate::model::{Config, EPG_ATTRIB_ID, EPG_TAG_CHANNEL};
use crate::model::{ConfigTarget, ProxyUserCredentials, TargetOutput};
use crate::repository::m3u_get_epg_file_path_for_target;
use crate::repository::storage_const;
use crate::repository::XML_PREAMBLE;
use crate::repository::{get_target_storage_path, BPlusTreeQuery};
use crate::repository::{xtream_get_epg_file_path_for_target, xtream_get_storage_path};
use crate::utils;
use crate::utils::{deobscure_text, file_exists_async, format_xmltv_time_utc, get_epg_processing_options, obscure_text, EpgProcessingOptions};
use axum::response::IntoResponse;
use chrono::DateTime;
use log::{error, trace};
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use shared::concat_string;
use shared::model::{EpgChannel, EpgProgramme, ShortEpgDto, ShortEpgResultDto};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
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
    trace!(
        "Can't find epg file for {target_name} target: {}",
        epg_path.to_str().unwrap_or("?")
    );
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
        let _file_lock = app_state.app_config.file_locks.read_lock(epg_path).await;
        match BPlusTreeQuery::<Arc<str>, EpgChannel>::try_new(epg_path) {
            Ok(query) => {
                let iterator: Box<dyn Iterator<Item=EpgChannel> + Send> = Box::new(query.disk_iter().map(|(_, v)| v));
                return stream_json_or_bin_response(accept, iterator);
            }
            Err(err) => {
                error!("Failed to open epg db for target {} {} - {err}", target.name, epg_path.display());
            }
        }
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

    let _file_lock = app_state.app_config.file_locks.read_lock(epg_path).await;
    let mut query = match BPlusTreeQuery::<Arc<str>, EpgChannel>::try_new(epg_path) {
        Ok(query) => query,
        Err(err) => {
            error!("Failed to open BPlusTreeQuery {} - {err}", epg_path.display());
            return get_empty_epg_response();
        }
    };

    let epg_processing_options = get_epg_processing_options(app_state, user, target);

    let base_url = if epg_processing_options.offset_minutes != 0 || epg_processing_options.rewrite_urls {
        let server_info = app_state.app_config.get_user_server_info(user);
        Some(concat_string!(&server_info.get_base_url(), "/", storage_const::EPG_RESOURCE_PATH, "/", &user.username, "/", &user.password))
    } else {
        None
    };

    let limit = limit.unwrap_or_default();

    let (mut tx, rx) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        // Work-Around BytesText DocType escape, see below
        if let Err(err) = tx.write_all(XML_PREAMBLE.as_ref()).await {
            error!("EPG: Failed to write xml header {err}");
        }
        if let Err(err) = tx.write_all(r#"<tv generator-info-name="X" generator-info-url="tuliprox">"#.as_bytes()).await {
            error!("EPG: Failed to write xml tv header {err}");
        }

        let mut writer = quick_xml::writer::Writer::new(tx);
        for (_, channel) in query.iter() {
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
                    let icon = match (epg_processing_options.rewrite_urls, base_url.as_ref(),
                                      obscure_text(&epg_processing_options.encrypt_secret, icon_url)) {
                        (true, Some(base), Ok(enc)) => concat_string!(base, &enc),
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
                    let (user_start, user_stop) = apply_user_offset(programme.start, programme.stop, epg_processing_options.offset_minutes);
                    elem.push_attribute(("start", format_xmltv_time_utc(user_start).as_str()));
                    elem.push_attribute(("stop", format_xmltv_time_utc(user_stop).as_str()));
                    elem.push_attribute(("channel", &programme.channel[..]));
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
    let _file_lock = app_state.app_config.file_locks.read_lock(epg_path).await;
    match BPlusTreeQuery::<Arc<str>, EpgChannel>::try_new(epg_path) {
        Ok(mut query) => {
            match query.query(channel_id) {
                Ok(Some(item)) => return Some(item),
                Ok(None) => {}
                Err(err) => {
                    error!("Failed to query db file {}: {err}", epg_path.display());
                }
            }
        }
        Err(err) => { error!("Failed to read db file {}: {err}", epg_path.display()); }
    }
    None
}

fn format_xmltv_time(ts: i64) -> String {
    if let Some(dt) = DateTime::from_timestamp(ts, 0) {
        dt.naive_utc().format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
        String::new()
    }
}

fn apply_user_offset(start: i64, stop: i64, offset_minutes: i32) -> (i64, i64) {
    let user_start = start + i64::from(offset_minutes) * 60;
    let user_end = stop + i64::from(offset_minutes) * 60;
    (user_start, user_end)
}

fn from_programme(stream_id: &Arc<str>, programme: &EpgProgramme, epg_processing_options: &EpgProcessingOptions) -> ShortEpgDto {
    let (user_start, user_end) = apply_user_offset(programme.start, programme.stop, epg_processing_options.offset_minutes);

    ShortEpgDto {
        id: Arc::clone(stream_id),
        epg_id: Arc::clone(&programme.channel),
        title: programme.title.as_ref().map_or_else(String::new, ToString::to_string),
        lang: String::new(),
        start: format_xmltv_time(user_start),
        end: format_xmltv_time(user_end),
        description: programme.desc.as_ref().map_or_else(String::new, ToString::to_string),
        channel_id: Arc::clone(&programme.channel),
        start_timestamp: user_start.to_string(),
        stop_timestamp: user_end.to_string(),
        stream_id: Arc::clone(stream_id),
    }
}

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
        if file_exists_async(epg_path).await {
            if let Some(epg_channel) = get_epg_channel(app_state, channel_id, epg_path).await {
                let epg_processing_options = get_epg_processing_options(app_state, user, target);
                ShortEpgResultDto {
                    epg_listings: if limit > 0 {
                        epg_channel.get_programme_with_limit(limit).iter().map(|p| from_programme(&stream_id, p, &epg_processing_options)).collect()
                    } else {
                        epg_channel.programmes.iter().map(|p| from_programme(&stream_id, p, &epg_processing_options)).collect()
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
        Ok(json) => (
            axum::http::StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())],
            json
        ).into_response(),
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
    axum::extract::Query(api_req): axum::extract::Query<UserApiRequest>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    let Some((user, target)) = get_user_target(&api_req, &app_state) else {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    };

    if user.permission_denied(&app_state) {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }

    let config = &app_state.app_config.config.load();
    let Some(epg_path) = get_epg_path_for_target(config, &target) else {
        // No epg configured,  No processing or timeshift, epg can't be mapped to the channels.
        // we do not deliver epg
        return get_empty_epg_response();
    };

    serve_epg(&app_state, &epg_path, &user, &target, None).await
}

async fn epg_api_resource(
    req_headers: axum::http::HeaderMap,
    axum::extract::Query(api_req): axum::extract::Query<UserApiRequest>,
    axum::extract::Path((username, password, resource)): axum::extract::Path<(
        String,
        String,
        String,
    )>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    let Some((user, _target)) =
        get_user_target_by_credentials(&username, &password, &api_req, &app_state)
    else {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    };
    if user.permission_denied(&app_state) {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }

    let encrypt_secret = app_state.app_config.get_reverse_proxy_rewrite_secret().unwrap_or_else(|| app_state.app_config.encrypt_secret);
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
        .route("/xmltv.php", axum::routing::get(xmltv_api))
        .route("/update/epg.php", axum::routing::get(xmltv_api))
        .route("/epg", axum::routing::get(xmltv_api))
        .route(&format!("/{}/{{username}}/{{password}}/{{resource}}", storage_const::EPG_RESOURCE_PATH),
               axum::routing::get(epg_api_resource),
        )
}
