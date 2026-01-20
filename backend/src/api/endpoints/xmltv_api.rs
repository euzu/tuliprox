use crate::api::api_utils::{get_user_target, get_user_target_by_credentials, internal_server_error, resource_response, serve_file, try_unwrap_body};
use crate::api::model::AppState;
use crate::api::model::UserApiRequest;
use crate::model::Config;
use crate::model::{ConfigTarget, ProxyUserCredentials, TargetOutput};
use crate::repository::get_target_storage_path;
use crate::repository::m3u_get_epg_file_path;
use crate::repository::storage_const;
use crate::repository::XML_PREAMBLE;
use crate::repository::{xtream_get_epg_file_path, xtream_get_storage_path};
use crate::utils::{async_file_reader, deobscure_text};
use crate::utils::{format_xtream_time, EpgConsumer, EpgProcessor};
use crate::utils;
use axum::response::IntoResponse;
use chrono::{Offset, TimeZone, Utc};
use chrono_tz::Tz;
use log::{error, trace};
use quick_xml::events::Event;
use shared::error::{info_err, TuliproxError};
use shared::model::{parse_xmltv_time, PlaylistItemType, ShortEpgDto, ShortEpgResultDto};
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
                        xtream_get_epg_file_path(&storage_path),
                    );
                }
            }
            TargetOutput::M3u(_) => {
                if let Some(target_path) = get_target_storage_path(config, &target.name) {
                    return get_epg_path_for_target_of_type(
                        &target.name,
                        m3u_get_epg_file_path(&target_path),
                    );
                }
            }
            TargetOutput::Strm(_) | TargetOutput::HdHomeRun(_) => {}
        }
    }
    None
}

/// Parses user-defined EPG timeshift configuration.
/// Supports either a numeric offset (e.g. "+2:30", "-1:15")
/// or a timezone name (e.g. "`Europe/Berlin`", "`UTC`", "`America/New_York`").
///
/// Returns the total offset in minutes (i32).
fn parse_timeshift(time_shift: Option<&str>) -> Option<i32> {
    time_shift.and_then(|offset| {
        // Try to parse as timezone name first
        if let Ok(tz) = offset.parse::<Tz>() {
            // Determine the current UTC offset of that timezone (including DST)
            let now = Utc::now();
            let local_time = tz.from_utc_datetime(&now.naive_utc());
            let offset_minutes = local_time.offset().fix().local_minus_utc() / 60;
            return Some(offset_minutes);
        }

        // If not a timezone, try to parse as numeric offset
        let sign_factor = if offset.starts_with('-') { -1 } else { 1 };
        let offset = offset.trim_start_matches(&['-', '+'][..]);

        let parts: Vec<&str> = offset.split(':').collect();
        let hours: i32 = parts.first().and_then(|h| h.parse().ok()).unwrap_or(0);
        let minutes: i32 = parts.get(1).and_then(|m| m.parse().ok()).unwrap_or(0);

        let total_minutes = hours * 60 + minutes;
        (total_minutes > 0).then_some(sign_factor * total_minutes)
    })
}

pub async fn serve_epg(
    app_state: &Arc<AppState>,
    epg_path: &Path,
    user: &ProxyUserCredentials,
    target: &Arc<ConfigTarget>,
    filter: Option<Arc<str>>,
) -> axum::response::Response {
    if let Ok(exists) = tokio::fs::try_exists(epg_path).await {
        if exists {
            let rewrite_resources = app_state.app_config.is_reverse_proxy_resource_rewrite_enabled();
            let encrypt_secret = app_state.app_config.get_reverse_proxy_rewrite_secret().unwrap_or_else(|| app_state.app_config.encrypt_secret);

            // If redirect is true → rewrite_urls = false → keep original
            // If redirect is false and rewrite_resources is true → rewrite_urls = true → rewriting allowed
            // If redirect is false and rewrite_resources is false → rewrite_urls = false → no rewriting
            let redirect = user.proxy.is_redirect(PlaylistItemType::Live) || target.is_force_redirect(PlaylistItemType::Live);
            let rewrite_urls = !redirect && rewrite_resources;

            // Use 0 for timeshift if None
            let timeshift = parse_timeshift(user.epg_timeshift.as_deref()).unwrap_or(0);

            return if timeshift != 0 || rewrite_urls || filter.is_some() {
                let server_info = app_state.app_config.get_user_server_info(user);
                let base_url = format!("{}/{}/{}/{}/", server_info.get_base_url(),
                                       storage_const::EPG_RESOURCE_PATH, &user.username, &user.password);
                // Apply timeshift and/or rewrite URLs and/or filter
                serve_epg_with_rewrites(epg_path, timeshift, rewrite_urls, &encrypt_secret, &base_url, filter).await
            } else {
                // Neither timeshift nor rewrite needed, serve original file
                serve_file(epg_path, mime::TEXT_XML.to_string()).await.into_response()
            };
        }
    }
    get_empty_epg_response()
}

struct XmlEpgConsumer<W: AsyncWriteExt + Unpin> {
    writer: quick_xml::writer::Writer<W>,
}

impl<W: AsyncWriteExt + Unpin + Send> EpgConsumer for XmlEpgConsumer<W> {
    async fn handle_event(&mut self, event: &Event<'_>, _decoder: quick_xml::Decoder) -> Result<(), TuliproxError> {
        self.writer.write_event_async(event.clone())
            .await
            .map_err(|e| info_err!("Failed to write EPG event: {}", e))
    }
}

#[allow(clippy::too_many_lines)]
async fn serve_epg_with_rewrites(
    epg_path: &Path,
    offset_minutes: i32,
    rewrite_urls: bool,
    secret: &[u8; 16],
    base_url: &str,
    filter: Option<Arc<str>>,
) -> axum::response::Response {
    match tokio::fs::try_exists(epg_path).await {
        Ok(exists) => {
            if !exists {
                return axum::http::StatusCode::NOT_FOUND.into_response();
            }
        }
        Err(err) => {
            error!("Failed to open egp file {}, {err:?}", epg_path.display());
            return axum::http::StatusCode::NOT_FOUND.into_response();
        }
    }

    let encrypt_secret = *secret;
    let rewrite_base_url = base_url.to_owned();
    match tokio::fs::File::open(epg_path).await {
        Ok(file) => {
            let reader = async_file_reader(file);
            let (tx, rx) = tokio::io::duplex(8192);
            tokio::spawn(async move {
                let mut encoder = async_compression::tokio::write::GzipEncoder::new(tx);

                // Work-Around BytesText DocType escape, see below
                if let Err(err) = encoder.write_all(XML_PREAMBLE.as_ref()).await {
                    error!("EPG: Failed to write xml header {err}");
                }

                let mut consumer = XmlEpgConsumer {
                    writer: quick_xml::writer::Writer::new(encoder),
                };

                let mut processor = EpgProcessor::new(
                    reader,
                    offset_minutes,
                    rewrite_urls,
                    rewrite_base_url,
                    encrypt_secret,
                    filter,
                );

                if let Err(e) = processor.process(&mut consumer).await {
                    error!("EPG: Failed to process EPG: {e}");
                }

                let mut encoder = consumer.writer.into_inner();
                if let Err(e) = encoder.shutdown().await {
                    error!("Failed to shutdown epg gzip encoder: {e}");
                }
            });

            let body_stream = ReaderStream::new(rx);
            try_unwrap_body!(axum::response::Response::builder()
                    .header(
                        axum::http::header::CONTENT_TYPE,
                        mime::TEXT_XML.to_string()
                    )
                    .header(axum::http::header::CONTENT_ENCODING, "gzip") // Set Content-Encoding header
                    .body(axum::body::Body::from_stream(body_stream)))
        }
        Err(_) => internal_server_error!(),
    }
}

pub async fn serve_short_epg(
    _app_state: &Arc<AppState>,
    epg_path: &Path,
    _user: &ProxyUserCredentials,
    _target: &Arc<ConfigTarget>,
    channel_id: std::sync::Arc<str>,
    stream_id: &str,
) -> axum::response::Response {
    match tokio::fs::File::open(epg_path).await {
        Ok(file) => {
            let reader = async_file_reader(file);
            let mut consumer = DtoEpgConsumer {
                items: Vec::new(),
                current_item: None,
                current_tag: String::new(),
                stream_id: stream_id.to_string(),
            };

            let mut processor = EpgProcessor::new(
                reader,
                0,
                false,
                String::new(),
                [0u8; 16],
                Some(channel_id),
            );

            if let Err(e) = processor.process(&mut consumer).await {
                error!("EPG: Failed to process EPG for short epg: {e}");
            }

            let short_epg = ShortEpgResultDto::new(consumer.items);

            match serde_json::to_string(&short_epg) {
                Ok(json) => (
                    axum::http::StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())],
                    json
                ).into_response(),
                Err(_) => internal_server_error!(),
            }
        }
        Err(_) => internal_server_error!(),
    }
}

struct DtoEpgConsumer {
    items: Vec<ShortEpgDto>,
    current_item: Option<ShortEpgDto>,
    current_tag: String,
    stream_id: String,
}


impl EpgConsumer for DtoEpgConsumer {
    async fn handle_event(&mut self, event: &Event<'_>, decoder: quick_xml::Decoder) -> Result<(), TuliproxError> {
        match event {
            Event::Start(e) => {
                let tag = e.name();
                match tag.as_ref() {
                    b"programme" => {
                        let mut item = ShortEpgDto::default();
                        item.stream_id.clone_from(&self.stream_id);
                        item.id.clone_from(&self.stream_id);
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"start" => {
                                    if let Ok(v) = attr.decode_and_unescape_value(decoder) {
                                        if let Some(ts) = parse_xmltv_time(&v) {
                                            item.start_timestamp = ts.to_string();
                                            item.start = format_xtream_time(ts);
                                        }
                                    }
                                }
                                b"stop" => {
                                    if let Ok(v) = attr.decode_and_unescape_value(decoder) {
                                        if let Some(ts) = parse_xmltv_time(&v) {
                                            item.stop_timestamp = ts.to_string();
                                            item.end = format_xtream_time(ts);
                                        }
                                    }
                                }
                                b"channel" => {
                                    if let Ok(v) = attr.decode_and_unescape_value(decoder) {
                                        item.epg_id = v.to_string();
                                        item.channel_id = v.to_string();
                                    }
                                }
                                _ => {}
                            }
                        }
                        self.current_item = Some(item);
                    }
                    _ => self.current_tag = String::from_utf8_lossy(tag.as_ref()).to_string(),
                }
            }
            Event::Text(e) => {
                if let Some(item) = &mut self.current_item {
                    if let Ok(text) = decoder.decode(e.as_ref()) {
                        let unescaped = match quick_xml::escape::unescape(&text) {
                            Ok(u) => u,
                            Err(_) => text,
                        };
                        match self.current_tag.as_str() {
                            "title" => item.title.push_str(&unescaped),
                            "desc" => item.description.push_str(&unescaped),
                            _ => {}
                        }
                    }
                }
            }
            Event::End(e) => {
                if e.name().as_ref() == b"programme" {
                    if let Some(item) = self.current_item.take() {
                        self.items.push(item);
                    }
                }
                if matches!(e.name().as_ref(), b"programme" | b"title" | b"desc") {
                    self.current_tag.clear();
                }
            }
            _ => {}
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_timeshift() {
        assert_eq!(parse_timeshift(Some(&String::from("2"))), Some(120));
        assert_eq!(parse_timeshift(Some(&String::from("-1:30"))), Some(-90));
        assert_eq!(parse_timeshift(Some(&String::from("+0:15"))), Some(15));
        assert_eq!(parse_timeshift(Some(&String::from("1:45"))), Some(105));
        assert_eq!(parse_timeshift(Some(&String::from(":45"))), Some(45));
        assert_eq!(parse_timeshift(Some(&String::from("-:45"))), Some(-45));
        assert_eq!(parse_timeshift(Some(&String::from("0:30"))), Some(30));
        assert_eq!(parse_timeshift(Some(&String::from(":3"))), Some(3));
        assert_eq!(parse_timeshift(Some(&String::from("2:"))), Some(120));
        assert_eq!(parse_timeshift(Some(&String::from("+2:00"))), Some(120));
        assert_eq!(parse_timeshift(Some(&String::from("-0:10"))), Some(-10));
        assert_eq!(parse_timeshift(Some(&String::from("invalid"))), None);
        assert_eq!(parse_timeshift(Some(&String::from("+abc"))), None);
        assert_eq!(parse_timeshift(Some(&String::new())), None);
        assert_eq!(parse_timeshift(None), None);
    }

    #[test]
    fn test_parse_timezone() {
        // This will depend on current DST; we just check it’s within a valid range
        let berlin = parse_timeshift(Some(&"Europe/Berlin".to_string())).unwrap();
        assert!(berlin == 60 || berlin == 120, "Berlin offset should be 60 or 120, got {berlin}");

        let new_york = parse_timeshift(Some(&"America/New_York".to_string())).unwrap();
        assert!(new_york == -300 || new_york == -240, "New York offset should be -300 or -240, got {new_york}");

        let tokyo = parse_timeshift(Some(&"Asia/Tokyo".to_string())).unwrap();
        assert_eq!(tokyo, 540); // always UTC+9

        let utc = parse_timeshift(Some(&"UTC".to_string())).unwrap();
        assert_eq!(utc, 0);
    }
}
