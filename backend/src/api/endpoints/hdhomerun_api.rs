use crate::api::api_utils::{try_unwrap_body, internal_server_error};
use crate::api::model::HdHomerunAppState;
use crate::auth::AuthBasic;
use crate::model::{AppConfig, ConfigTarget, ProxyUserCredentials};
use crate::utils::arc_str_serde;
use crate::processing::parser::xtream::get_xtream_url;
use crate::repository::{iter_raw_m3u_target_playlist, M3uPlaylistIterator};
use crate::repository::XtreamPlaylistIterator;
use axum::response::IntoResponse;
use bytes::Bytes;
use futures::{stream, Stream, StreamExt};
use log::{error, warn};
use serde::{Deserialize, Serialize};
use serde_json::json;
use shared::model::{
    M3uPlaylistItem, PlaylistItemType, TargetType, XtreamCluster, XtreamPlaylistItem,
};
use shared::utils::{concat_path};
use std::sync::Arc;

#[derive(Serialize, Deserialize, Clone)]
struct Lineup {
    #[serde(with = "arc_str_serde", rename = "GuideNumber")]
    guide_number: Arc<str>,
    #[serde(with = "arc_str_serde", rename = "GuideName")]
    guide_name: Arc<str>,
    #[serde(with = "arc_str_serde", rename = "URL")]
    url: Arc<str>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Device {
    #[serde(rename = "FriendlyName")]
    friendly_name: String,
    #[serde(rename = "Manufacturer")]
    manufacturer: String,
    #[serde(rename = "ModelNumber")]
    model_number: String,
    #[serde(rename = "ModelName")]
    model_name: String,
    #[serde(rename = "FirmwareName")]
    firmware_name: String,
    #[serde(rename = "TunerCount")]
    tuner_count: u8,
    #[serde(rename = "FirmwareVersion")]
    firmware_version: String,
    #[serde(rename = "DeviceID")]
    id: String,
    #[serde(rename = "DeviceAuth")]
    auth: String,
    #[serde(rename = "BaseURL")]
    base_url: String,
    #[serde(rename = "LineupURL")]
    lineup_url: String,
    #[serde(rename = "DiscoverURL")]
    discover_url: String,
    #[serde(skip)] // UDN is not needed in JSON responses
    udn: String,
}

impl Device {
    fn as_xml(&self) -> String {
        format!(
            r#"<root xmlns="urn:schemas-upnp-org:device-1-0">
<specVersion>
<major>1</major>
<minor>0</minor>
</specVersion>
<URLBase>{}</URLBase>
<device>
  <deviceType>urn:schemas-upnp-org:device:MediaServer:1</deviceType>
  <friendlyName>{}</friendlyName>
  <manufacturer>{}</manufacturer>
  <modelName>{}</modelName>
  <modelNumber>{}</modelNumber>
  <tunerCount>{}</tunerCount>
  <serialNumber>{}</serialNumber>
  <UDN>uuid:{}</UDN>
</device>
</root>"#,
            self.base_url,
            self.friendly_name,
            self.manufacturer,
            self.model_name,
            self.model_number,
            self.tuner_count,
            self.id, // Correct: 8-digit hex device ID
            self.udn // Correct: Application/Device UUID for UDN
        )
    }
}

fn xtream_item_to_lineup_stream<I>(
    cfg: Arc<AppConfig>,
    cluster: XtreamCluster,
    credentials: Arc<ProxyUserCredentials>,
    base_url: Option<String>,
    channels: Option<I>,
) -> impl Stream<Item=Result<Bytes, String>>
where
    I: Iterator<Item=(XtreamPlaylistItem, bool)> + 'static,
{
    match channels {
        Some(chans) => {
            let mapped = chans.map(move |(item, has_next)| {
                let input_options = cfg.get_input_options_by_name(&item.input_name);
                let (live_stream_use_prefix, live_stream_without_extension) = input_options
                    .as_ref()
                    .map_or((true, false), |o| {
                        (
                            o.xtream_live_stream_use_prefix,
                            o.xtream_live_stream_without_extension,
                        )
                    });
                let container_extension = item.get_container_extension();
                let stream_url = match &base_url {
                    None => item.url.clone(),
                    Some(url) => get_xtream_url(
                        cluster,
                        url,
                        &credentials.username,
                        &credentials.password,
                        item.virtual_id,
                        container_extension.as_deref(),
                        live_stream_use_prefix,
                        live_stream_without_extension,
                    ).into(),
                };

                let lineup = Lineup {
                    guide_number: item.epg_channel_id.unwrap_or(item.name.clone()),
                    guide_name: item.title.clone(),
                    url: stream_url.clone(),
                };
                match serde_json::to_string(&lineup) {
                    Ok(mut content) => {
                        if has_next {
                            content.push(',');
                        }
                        Ok(Bytes::from(content))
                    },
                    Err(_) => Ok(Bytes::from("")),
                }
            });
            stream::iter(mapped).left_stream()
        }
        None => stream::once(async { Ok(Bytes::from("")) }).right_stream(),
    }
}

fn m3u_item_to_lineup_stream<I>(channels: Option<I>) -> impl Stream<Item=Result<Bytes, String>>
where
    I: Iterator<Item=(M3uPlaylistItem, bool)> + 'static,
{
    match channels {
        Some(chans) => {
            let mapped = chans.map(move |(item, has_next)| {
                let lineup = Lineup {
                    guide_number: item.epg_channel_id.clone().unwrap_or(item.name.clone()),
                    guide_name: item.title.clone(),
                    url: if item.t_stream_url.is_empty() {
                        item.url.clone()
                    } else {
                        item.t_stream_url.clone()
                    }
                };
                match serde_json::to_string(&lineup) {
                    Ok(mut content) => {
                        if has_next {
                            content.push(',');
                        }
                        Ok(Bytes::from(content))
                    },
                    Err(_) => Ok(Bytes::from("")),
                }
            });
            stream::iter(mapped).left_stream()
        }
        None => stream::once(async { Ok(Bytes::from("")) }).right_stream(),
    }
}

fn create_device(app_state: &Arc<HdHomerunAppState>) -> Option<Device> {
    if let Some(credentials) = app_state
        .app_state
        .app_config
        .get_user_credentials(&app_state.device.t_username)
    {
        let server_info = app_state
            .app_state
            .app_config
            .get_user_server_info(&credentials);
        let device = &app_state.device;
        let device_url = format!(
            "{}://{}:{}",
            server_info.protocol, server_info.host, device.port
        );

        Some(Device {
            friendly_name: device.friendly_name.clone(),
            manufacturer: device.manufacturer.clone(),
            model_number: device.model_number.clone(),
            model_name: device.model_name.clone(),
            firmware_name: device.firmware_name.clone(),
            tuner_count: device.tuner_count,
            firmware_version: device.firmware_version.clone(),
            auth: String::new(),
            id: device.device_id.clone(),
            udn: device.device_udn.clone(), // Set UDN from device configuration
            lineup_url: concat_path(&device_url, "lineup.json"),
            discover_url: concat_path(&device_url, "discover.json"),
            base_url: device_url,
        })
    } else {
        error!(
            "Failed to get credentials for username: {} for device: {} ",
            &app_state.device.t_username, &app_state.device.name
        );
        None
    }
}

async fn device_xml(
    axum::extract::State(app_state): axum::extract::State<Arc<HdHomerunAppState>>,
) -> impl IntoResponse {
    if let Some(device) = create_device(&app_state) {
        try_unwrap_body!(axum::response::Response::builder()
            .status(axum::http::StatusCode::OK)
            .header(axum::http::header::CONTENT_TYPE, "application/xml")
            .body(axum::body::Body::from(device.as_xml())))
    } else {
        internal_server_error!()
    }
}

async fn device_json(
    axum::extract::State(app_state): axum::extract::State<Arc<HdHomerunAppState>>,
) -> impl IntoResponse {
    if let Some(device) = create_device(&app_state) {
        axum::Json(device).into_response()
    } else {
        internal_server_error!()
    }
}

async fn discover_json(
    axum::extract::State(app_state): axum::extract::State<Arc<HdHomerunAppState>>,
) -> impl IntoResponse {
    if let Some(device) = create_device(&app_state) {
        axum::Json(device).into_response()
    } else {
        internal_server_error!()
    }
}

async fn lineup_status(
    axum::extract::State(app_state): axum::extract::State<Arc<HdHomerunAppState>>,
) -> impl IntoResponse {
    let current_state = app_state
        .hd_scan_state
        .load(std::sync::atomic::Ordering::Acquire);
    if current_state < 0 {
        axum::Json(json!({
            "ScanInProgress": 0,
            "ScanPossible": 1,
            "Source": "Cable",
            "SourceList": ["Cable"],
        }))
            .into_response()
    } else {
        let new_state = current_state.saturating_add(20);
        let final_state = if new_state > 100 { 100 } else { new_state };

        let cfg = Arc::clone(&app_state.app_state.app_config);
        let num_of_channels = if let Some((user, target)) =
            cfg.get_target_for_username(&app_state.device.t_username)
        {
            if target.has_output(TargetType::M3u) {
                if let Some((_guard, iter)) = iter_raw_m3u_target_playlist(&cfg, &target, None).await
                {
                    iter.count()
                } else {
                    0
                }
            } else if target.has_output(TargetType::Xtream) {
                let credentials = Arc::new(user);
                let live = XtreamPlaylistIterator::new(XtreamCluster::Live, &cfg, &target, None, &credentials).await.map_or(0, std::iter::Iterator::count);
                let vod = XtreamPlaylistIterator::new(XtreamCluster::Video, &cfg, &target, None, &credentials).await.map_or(0, std::iter::Iterator::count);
                live + vod
            } else {
                0
            }
        } else {
            0
        };

        if final_state >= 100 {
            app_state
                .hd_scan_state
                .store(-1, std::sync::atomic::Ordering::Release);
        } else {
            app_state
                .hd_scan_state
                .store(final_state, std::sync::atomic::Ordering::Release);
        }
        let found = (num_of_channels * usize::try_from(final_state).unwrap_or(1)) / 100;
        axum::Json(json!({
            "ScanInProgress": 1,
            "Progress": final_state,
            "Found": found,
        }))
            .into_response()
    }
}

#[derive(Deserialize)]
struct LineupPostQuery {
    scan: String,
}

async fn lineup_post(
    axum::extract::State(app_state): axum::extract::State<Arc<HdHomerunAppState>>,
    axum::extract::Query(query): axum::extract::Query<LineupPostQuery>,
) -> impl IntoResponse {
    match query.scan.as_str() {
        "start" => {
            app_state
                .hd_scan_state
                .store(0, std::sync::atomic::Ordering::Release);
            axum::http::StatusCode::OK.into_response()
        }
        "abort" => {
            app_state
                .hd_scan_state
                .store(-1, std::sync::atomic::Ordering::Release);
            axum::http::StatusCode::OK.into_response()
        }
        _ => axum::http::StatusCode::BAD_REQUEST.into_response(),
    }
}

async fn lineup(
    app_state: &Arc<HdHomerunAppState>,
    cfg: &Arc<AppConfig>,
    credentials: &Arc<ProxyUserCredentials>,
    target: &ConfigTarget,
) -> impl IntoResponse {
    let use_output = target
        .get_hdhomerun_output()
        .as_ref()
        .and_then(|o| o.use_output);
    let use_all = use_output.is_none();
    let use_m3u = use_output.as_ref() == Some(&TargetType::M3u);
    let use_xtream = use_output.as_ref() == Some(&TargetType::Xtream);
    if (use_all || use_m3u) && target.has_output(TargetType::M3u) {
        let iterator = M3uPlaylistIterator::new(cfg, target, credentials)
            .await
            .ok();
        let stream = m3u_item_to_lineup_stream(iterator);
        let body_stream = stream::once(async { Ok(Bytes::from("[")) })
            .chain(stream)
            .chain(stream::once(async { Ok(Bytes::from("]")) }));
        return try_unwrap_body!(axum::response::Response::builder()
            .status(axum::http::StatusCode::OK)
            .header(
                axum::http::header::CONTENT_TYPE,
                mime::APPLICATION_JSON.to_string()
            )
            .body(axum::body::Body::from_stream(body_stream)));
    } else if (use_all || use_xtream) && target.has_output(TargetType::Xtream) {
        let server_info = app_state
            .app_state
            .app_config
            .get_user_server_info(credentials);
        let base_url = server_info.get_base_url();

        let base_url_live = if credentials.proxy.is_redirect(PlaylistItemType::Live)
            || target.is_force_redirect(PlaylistItemType::Live)
        {
            None
        } else {
            Some(base_url.clone())
        };
        let base_url_vod = if credentials.proxy.is_redirect(PlaylistItemType::Video)
            || target.is_force_redirect(PlaylistItemType::Video)
        {
            None
        } else {
            Some(base_url)
        };

        let live_channels =
            XtreamPlaylistIterator::new(XtreamCluster::Live, cfg, target, None, credentials)
                .await
                .ok();
        let vod_channels =
            XtreamPlaylistIterator::new(XtreamCluster::Video, cfg, target, None, credentials)
                .await
                .ok();
        let live_stream = xtream_item_to_lineup_stream(
            Arc::clone(cfg),
            XtreamCluster::Live,
            Arc::clone(credentials),
            base_url_live.clone(),
            live_channels,
        );
        let vod_stream = xtream_item_to_lineup_stream(
            Arc::clone(cfg),
            XtreamCluster::Video,
            Arc::clone(credentials),
            base_url_vod.clone(),
            vod_channels,
        );

        let mut live_stream_peek = Box::pin(live_stream.peekable());
        let mut vod_stream_peek = Box::pin(vod_stream.peekable());
        let both_non_empty = live_stream_peek.as_mut().peek().await.is_some() && vod_stream_peek.as_mut().peek().await.is_some();
        let comma_stream = if both_non_empty {
            stream::once(async { Ok(Bytes::from(",")) }).left_stream()
        } else {
            stream::empty().right_stream()
        };

        let body_stream = stream::once(async { Ok(Bytes::from("[")) })
            .chain(live_stream_peek)
            .chain(comma_stream)
            .chain(vod_stream_peek)
            .chain(stream::once(async { Ok(Bytes::from("]")) }));

        return try_unwrap_body!(axum::response::Response::builder()
            .status(axum::http::StatusCode::OK)
            .header(
                axum::http::header::CONTENT_TYPE,
                mime::APPLICATION_JSON.to_string()
            )
            .body(axum::body::Body::from_stream(body_stream)));
    }
    axum::http::StatusCode::NOT_FOUND.into_response()
}

async fn auth_lineup_json(
    AuthBasic((username, password)): AuthBasic,
    axum::extract::State(app_state): axum::extract::State<Arc<HdHomerunAppState>>,
) -> impl IntoResponse {
    let cfg = Arc::clone(&app_state.app_state.app_config);
    if let Some((credentials, target)) = cfg.get_target_for_username(&app_state.device.t_username)
    {
        if !username.eq(&credentials.username) || !password.eq(&credentials.password) {
            return axum::http::StatusCode::UNAUTHORIZED.into_response();
        }
        let user_credentials = Arc::new(credentials);
        return lineup(&app_state, &cfg, &user_credentials, &target)
            .await
            .into_response();
    }
    axum::http::StatusCode::NOT_FOUND.into_response()
}

async fn lineup_json(
    axum::extract::State(app_state): axum::extract::State<Arc<HdHomerunAppState>>,
) -> impl IntoResponse {
    let cfg = Arc::clone(&app_state.app_state.app_config);
    if let Some((credentials, target)) = cfg.get_target_for_username(&app_state.device.t_username)
    {
        let user_credentials = Arc::new(credentials);
        return lineup(&app_state, &cfg, &user_credentials, &target)
            .await
            .into_response();
    }
    axum::http::StatusCode::NOT_FOUND.into_response()
}

async fn auto_channel(
    axum::extract::State(_app_state): axum::extract::State<Arc<HdHomerunAppState>>,
    axum::extract::Path(channel): axum::extract::Path<String>,
) -> impl IntoResponse {
    warn!("HdHomerun api not implemented for auto_channel {channel}");
    axum::http::StatusCode::NOT_FOUND.into_response()
}

pub fn hdhr_api_register(basic_auth: bool) -> axum::Router<Arc<HdHomerunAppState>> {
    axum::Router::new()
        .route("/device.xml", axum::routing::get(device_xml))
        .route("/device.json", axum::routing::get(device_json))
        .route("/discover.json", axum::routing::get(discover_json))
        .route("/lineup_status.json", axum::routing::get(lineup_status))
        .route(
            "/lineup.json",
            if basic_auth {
                axum::routing::get(auth_lineup_json)
            } else {
                axum::routing::get(lineup_json)
            },
        )
        .route("/lineup.post", axum::routing::post(lineup_post))
        .route("/auto/{channel}", axum::routing::get(auto_channel))
        .route(
            "/tuner{tuner_num}/{channel}",
            axum::routing::get(auto_channel),
        )
}