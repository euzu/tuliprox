use crate::api::api_utils::{get_user_target, serve_file, get_user_target_by_credentials,
                            resource_response, try_unwrap_body, internal_server_error};
use crate::api::model::AppState;
use crate::api::model::UserApiRequest;
use crate::model::{get_attr_value, Config, EPG_TAG_ICON, EPG_TAG_PROGRAMME};
use crate::model::{ConfigTarget, ProxyUserCredentials, TargetOutput};
use crate::repository::storage::get_target_storage_path;
use crate::repository::storage_const;
use crate::repository::xtream_repository::{xtream_get_epg_file_path, xtream_get_storage_path};
use crate::{utils};
use crate::utils::{async_file_reader, deobscure_text, obscure_text};
use axum::response::IntoResponse;
use chrono::{DateTime, Duration, FixedOffset, NaiveDateTime, Offset, TimeZone, Utc};
use chrono_tz::Tz;
use log::{error, trace};
use quick_xml::events::{BytesStart, Event};
use shared::model::{PlaylistItemType};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;
use crate::repository::m3u_repository::m3u_get_epg_file_path;

pub fn get_empty_epg_response() -> axum::response::Response {
    try_unwrap_body!(axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, axum::http::HeaderValue::from_static("text/xml"))
        .body(axum::body::Body::from(r#"<?xml version="1.0" encoding="utf-8" ?><!DOCTYPE tv SYSTEM "xmltv.dtd"><tv generator-info-name="Xtream Codes" generator-info-url=""></tv>"#)))
}

/// Applies an EPG timeshift to an XMLTV programme time, taking the existing timezone into account.
/// @see `https://wiki.xmltv.org/index.php/XMLTVFormat`
///
/// # Arguments
/// * `original` - original XMLTV datetime string, e.g. "20080715003000 -0600"
/// * `shift` - timeshift in minutes as `chrono::Duration`
///
/// # Returns
/// A new datetime string in the same format with the same timezone.
fn time_correct(original: &str, shift: &Duration) -> String {
    let (datetime_part, tz_part) = if let Some((dt, tz)) = original.trim().rsplit_once(' ') {
        (dt, tz)
    } else {
        (original.trim(), "+0000")
    };

    let Ok(naive_dt) = NaiveDateTime::parse_from_str(datetime_part, "%Y%m%d%H%M%S") else { return original.to_string() };

    let tz_offset_minutes = if tz_part.len() == 5 {
        let sign = if &tz_part[0..1] == "-" { -1 } else { 1 };
        let hours: i32 = tz_part[1..3].parse().unwrap_or(0);
        let mins: i32 = tz_part[3..5].parse().unwrap_or(0);
        sign * (hours * 60 + mins)
    } else {
        0
    };

    let tz = FixedOffset::east_opt(tz_offset_minutes * 60).unwrap_or(FixedOffset::east_opt(0).unwrap());

    // Use modern chrono API
    let dt: DateTime<FixedOffset> = tz
        .from_local_datetime(&naive_dt)
        .single()
        .unwrap_or_else(|| tz.from_utc_datetime(&naive_dt));

    let shifted_dt = dt + *shift;

    format!("{} {}", shifted_dt.format("%Y%m%d%H%M%S"), format_offset(tz_offset_minutes))
}

fn format_offset(offset_minutes: i32) -> String {
    let sign = if offset_minutes < 0 { '-' } else { '+' };
    let abs = offset_minutes.abs();
    let hours = abs / 60;
    let mins = abs % 60;
    format!("{sign}{hours:02}{mins:02}")
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
                if let Err(err)  =encoder.write_all(b"<?xml version=\"1.0\" encoding=\"utf-8\"?>\n").await {
                    error!("EPG: Failed to write xml header {err}");
                }

                if let Err(err)  =encoder.write_all(b"<!DOCTYPE tv SYSTEM \"xmltv.dtd\">\n").await {
                    error!("EPG: Failed to write epg doc type {err}");
                }

                let mut xml_reader = quick_xml::reader::Reader::from_reader(async_file_reader(reader));
                let mut xml_writer = quick_xml::writer::Writer::new(encoder);

                // TODO howto avoid BytesText to escape the doctype "xmltv.dtd"  which is written as &quote;xmltv.dtd&quote;
                // if let Err(err)  =xml_writer.write_event_async(Event::Decl(quick_xml::events::BytesDecl::new("1.0", Some("utf-8"), None))).await {
                //     error!("EPG: Failed to write epg header {err}");
                // }
                // if let Err(err)  = xml_writer.write_event_async(Event::DocType( quick_xml::events::BytesText::new(r#"tv SYSTEM "xmltv.dtd""#))).await {
                //     error!("EPG: Failed to write epg doc type {err}");
                // }

                let mut buf = Vec::with_capacity(4096);
                let duration = Duration::minutes(i64::from(offset_minutes));
                let mut skip_depth = None;

                loop {
                    buf.clear();
                    let event = match xml_reader.read_event_into_async(&mut buf).await {
                        Ok(e) => e,
                        Err(e) => {
                            error!("Error reading epg XML event: {e}");
                            break;
                        }
                    };

                    if let Some(flt) =  &filter {
                        // Filter
                        match &event {
                            Event::Start(e) => {
                                if skip_depth.is_none() {
                                    let should_skip = match e.name().as_ref() {
                                        b"channel" => {
                                            e.attributes()
                                                .filter_map(Result::ok)
                                                .find(|a| a.key.as_ref() == b"id")
                                                .and_then(|a| a.unescape_value().ok())
                                                .is_some_and(|v| flt.as_ref() != v.as_ref())
                                        }
                                        b"programme" => {
                                            e.attributes()
                                                .filter_map(Result::ok)
                                                .find(|a| a.key.as_ref() == b"channel")
                                                .and_then(|a| a.unescape_value().ok())
                                                .is_some_and(|v| flt.as_ref() != v.as_ref())
                                        }
                                        _ => false,
                                    };

                                    if should_skip {
                                        skip_depth = Some(1);
                                        continue;
                                    }
                                } else {
                                    skip_depth = skip_depth.map(|d| d + 1);
                                    continue;
                                }
                            }
                            Event::End(_) => {
                                if let Some(depth) = skip_depth {
                                    if depth == 1 {
                                        skip_depth = None;
                                    } else {
                                        skip_depth = Some(depth - 1);
                                    }
                                    continue;
                                }
                            }
                            Event::Empty(_) => {
                                if skip_depth.is_some() {
                                    continue;
                                }
                            }
                            _ => {}
                        }

                        if skip_depth.is_some() {
                            continue;
                        }
                    }

                    match &event {
                        Event::Start(ref e) if offset_minutes != 0 && e.name().as_ref() == b"programme" => {
                            // Modify the attributes
                            let mut elem = BytesStart::new(EPG_TAG_PROGRAMME);
                            for attr in e.attributes() {
                                match attr {
                                    Ok(attr) if attr.key.as_ref() == b"start" => {
                                        if let Ok(start_value) = attr.decode_and_unescape_value(xml_reader.decoder()) {
                                            // Modify the start attribute value as needed
                                            elem.push_attribute(("start", time_correct(&start_value, &duration).as_str()));
                                        } else {
                                            // keep original attribute unchanged ?
                                            elem.push_attribute(attr);
                                        }
                                    }
                                    Ok(attr) if attr.key.as_ref() == b"stop" => {
                                        if let Ok(stop_value) = attr.decode_and_unescape_value(xml_reader.decoder()) {
                                            // Modify the stop attribute value as needed
                                            elem.push_attribute(("stop", time_correct(&stop_value, &duration).as_str()));
                                        } else {
                                            elem.push_attribute(attr);
                                        }
                                    }
                                    Ok(attr) => {
                                        // Copy any other attributes as they are
                                        elem.push_attribute(attr);
                                    }
                                    Err(e) => {
                                        error!("Error parsing epg attribute: {e}");
                                    }
                                }
                            }

                            // Write the modified start event
                            if let Err(e) = xml_writer.write_event_async(Event::Start(elem)).await {
                                error!("Failed to write epg Start event: {e}");
                                break;
                            }
                        }
                        ref event @ (Event::Empty(ref e) | Event::Start(ref e)) if rewrite_urls && e.name().as_ref() == b"icon" => {
                            // Modify the attributes
                            let mut elem = BytesStart::new(EPG_TAG_ICON);
                            for attr in e.attributes() {
                                match attr {
                                    Ok(attr) if attr.key.as_ref() == b"src" => {
                                        if let Some(icon) = get_attr_value(&attr) {
                                            if icon.is_empty() {
                                                elem.push_attribute(attr);
                                            } else {
                                                let rewritten_url = if let Ok(encrypted) = obscure_text(&encrypt_secret, &icon) {
                                                    format!("{rewrite_base_url}{encrypted}")
                                                 } else {
                                                    icon
                                                };
                                                elem.push_attribute(("src", rewritten_url.as_str()));
                                            }
                                        } else {
                                            elem.push_attribute(attr);
                                        }
                                    }
                                    Ok(attr) => {
                                        // Copy any other attributes as they are
                                        elem.push_attribute(attr);
                                    }
                                    Err(e) => {
                                        error!("Error parsing epg attribute: {e}");
                                    }
                                }
                            }

                            let out_event = match event {
                                    Event::Empty(_) => Some(Event::Empty(elem)),
                                    Event::Start(_) => Some(Event::Start(elem)),
                                    _ => None,
                                };
                            if let Some(out) = out_event {
                                if let Err(e) = xml_writer.write_event_async(out).await {
                                    error!("Failed to write epg icon event: {e}");
                                    break;
                                }
                            }
                        }
                        Event::Decl(_) | Event::DocType(_) => {},
                        Event::Eof => break, // End of file
                        _ => {
                            // Write any other event as is
                            if let Err(e) = xml_writer.write_event_async(event).await {
                                error!("Failed to epg write event: {e}");
                                break;
                            }
                        }
                    }
                }
                buf.clear();
                let mut encoder = xml_writer.into_inner();
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
