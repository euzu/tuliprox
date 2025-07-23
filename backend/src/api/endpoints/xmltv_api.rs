use axum::response::IntoResponse;
use chrono::{Duration, NaiveDateTime, TimeDelta};
use flate2::write::GzEncoder;
use flate2::Compression;
use log::{error, trace};
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, Writer};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::api::api_utils::{get_user_target, serve_file};
use crate::api::model::app_state::AppState;
use crate::api::model::request::UserApiRequest;
use crate::model::Config;
use crate::model::{ConfigTarget, ProxyUserCredentials, TargetOutput};
use crate::repository::m3u_repository::m3u_get_epg_file_path;
use crate::repository::storage::get_target_storage_path;
use crate::repository::xtream_repository::{xtream_get_epg_file_path, xtream_get_storage_path};
use crate::{utils};
use crate::api::api_utils::try_unwrap_body;

pub fn get_empty_epg_response() -> impl axum::response::IntoResponse + Send {
    try_unwrap_body!(axum::response::Response::builder()
        .status(axum::http::StatusCode::OK) // Entspricht `HttpResponse::Ok()`
        .header(axum::http::header::CONTENT_TYPE, axum::http::HeaderValue::from_static("text/xml"))
        .body(axum::body::Body::from(r#"<?xml version="1.0" encoding="utf-8" ?><!DOCTYPE tv SYSTEM "xmltv.dtd"><tv generator-info-name="Xtream Codes" generator-info-url=""></tv>"#)))
}

fn time_correct(date_time: &str, correction: &TimeDelta) -> String {
    // Split the dateTime string into date and time parts
    let date_time_split: Vec<&str> = date_time.split(' ').collect();
    if date_time_split.len() != 2 {
        return date_time.to_string();
    }

    // Parse the datetime string
    NaiveDateTime::parse_from_str(date_time_split[0], "%Y%m%d%H%M%S").map_or_else(|_| date_time.to_string(), |native_dt| {
        let corrected_dt = native_dt + *correction;
        // Format the corrected datetime back to string
        let formatted_dt = corrected_dt.format("%Y%m%d%H%M%S").to_string();
        let result = format!("{} {}", formatted_dt, date_time_split[1]);
        result
    })
}

fn get_epg_path_for_target_of_type(target_name: &str, epg_path: PathBuf) -> Option<PathBuf> {
    if utils::path_exists(&epg_path) {
        return Some(epg_path);
    }
    trace!("Cant find epg file for {target_name} target: {}", epg_path.to_str().unwrap_or("?"));
    None
}

fn get_epg_path_for_target(config: &Config, target: &ConfigTarget) -> Option<PathBuf> {
    // TODO if we have multiple targets, first one serves, this can be problematic when
    // we use m3u playlist but serve xtream target epg

    // TODO if we share the same virtual_id for epg, can we store an epg file for the target ?
    for output in &target.output {
        match output {
            TargetOutput::Xtream(_) => {
                if let Some(storage_path) = xtream_get_storage_path(config, &target.name) {
                    return get_epg_path_for_target_of_type(&target.name, xtream_get_epg_file_path(&storage_path));
                }
            }
            TargetOutput::M3u(_) => {
                if let Some(target_path) = get_target_storage_path(config, &target.name) {
                    return get_epg_path_for_target_of_type(&target.name, m3u_get_epg_file_path(&target_path));
                }
            }
            TargetOutput::Strm(_) | TargetOutput::HdHomeRun(_) => {}
        }
    }
    None
}

// `-2:30`(-2h30m), `1:45` (1h45m), `+0:15` (15m), `2` (2h), `:30` (30m), `:3` (3m), `2:` (3h)
fn parse_timeshift(time_shift: Option<&String>) -> Option<i32> {
    time_shift.and_then(|offset| {
        let sign_factor = if offset.starts_with('-') { -1 } else { 1 };
        let offset = offset.trim_start_matches(&['-', '+'][..]);

        let parts: Vec<&str> = offset.split(':').collect();
        let hours: i32 = parts.first().and_then(|h| h.parse().ok()).unwrap_or(0);
        let minutes: i32 = parts.get(1).and_then(|m| m.parse().ok()).unwrap_or(0);

        let total_minutes = hours * 60 + minutes;
        (total_minutes > 0).then_some(sign_factor * total_minutes)
    })
}

async fn serve_epg(epg_path: &Path, user: &ProxyUserCredentials) -> impl axum::response::IntoResponse + Send {
    match File::open(epg_path) {
        Ok(epg_file) => {
            match parse_timeshift(user.epg_timeshift.as_ref()) {
                None => serve_file(epg_path, mime::TEXT_XML).await.into_response(),
                Some(duration) => {
                    serve_epg_with_timeshift(epg_file, duration).into_response()
                }
            }
        }
        Err(_) => {
            get_empty_epg_response().into_response()
        }
    }
}

fn serve_epg_with_timeshift(epg_file: File, offset_minutes: i32) -> impl axum::response::IntoResponse + Send {
    let reader = utils::file_reader(epg_file);
    let encoder = GzEncoder::new(Vec::with_capacity(4096), Compression::default());
    let mut xml_reader = Reader::from_reader(reader);
    let mut xml_writer = Writer::new(encoder);
    let mut buf = Vec::with_capacity(1024);
    let duration = Duration::minutes(i64::from(offset_minutes));

    loop {
        match xml_reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) if e.name().as_ref() == b"programme" => {
                // Modify the attributes
                let mut elem = BytesStart::from(e.name());
                for attr in e.attributes() {
                    match attr {
                        Ok(attr) if attr.key.as_ref() == b"start" => {
                            let start_value = attr.decode_and_unescape_value(xml_reader.decoder())
                                .expect("Failed to decode start attribute");
                            // Modify the start attribute value as needed
                            elem.push_attribute(("start", time_correct(&start_value, &duration).as_str()));
                        }
                        Ok(attr) if attr.key.as_ref() == b"stop" => {
                            let stop_value = attr.decode_and_unescape_value(xml_reader.decoder())
                                .expect("Failed to decode stop attribute");
                            // Modify the stop attribute value as needed
                            elem.push_attribute(("stop", time_correct(&stop_value, &duration).as_str()));
                        }
                        Ok(attr) => {
                            // Copy any other attributes as they are
                            elem.push_attribute(attr);
                        }
                        Err(e) => {
                            error!("Error parsing attribute: {e}");
                        }
                    }
                }

                // Write the modified start event
                xml_writer.write_event(Event::Start(elem)).expect("Failed to write event");
            }
            Ok(Event::Eof) => break, // End of file
            Ok(event) => {
                // Write any other event as is
                xml_writer.write_event(event).expect("Failed to write event");
            }
            Err(e) => {
                error!("Error: {e}");
                break;
            }
        }

        buf.clear();
    }
    match xml_writer.into_inner().finish() {
        Ok(compressed_data) =>
            try_unwrap_body!(axum::response::Response::builder()
            .header(axum::http::header::CONTENT_TYPE, mime::APPLICATION_OCTET_STREAM.to_string())
            .header(axum::http::header::CONTENT_ENCODING, "gzip") // Set Content-Encoding header
            .body(axum::body::Body::from(compressed_data))),
        Err(err) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response()
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
        return get_empty_epg_response().into_response();
    };

    serve_epg(&epg_path, &user).await.into_response()
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
}