use crate::api::model::app_state::HdHomerunAppState;
use crate::auth::AuthBasic;
use crate::model::{AppConfig, ConfigTarget, ProxyUserCredentials};
use shared::model::{M3uPlaylistItem, PlaylistItemType, TargetType, XtreamCluster, XtreamPlaylistItem};
use crate::processing::parser::xtream::get_xtream_url;
use crate::repository::m3u_playlist_iterator::M3uPlaylistIterator;
use crate::repository::xtream_playlist_iterator::XtreamPlaylistIterator;
use axum::response::IntoResponse;
use bytes::Bytes;
use futures::{stream, Stream, StreamExt};
use log::{error, warn};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use shared::utils::get_string_from_serde_value;
// https://info.hdhomerun.com/info/http_api

// const DISCOVERY_BYTES: &[u8] =  &[0, 2, 0, 12, 1, 4, 255, 255, 255, 255, 2, 4, 255, 255, 255, 255, 115, 204, 125, 143];
// const RESPONSE_BYTES: &[u8] =  &[0, 3, 0, 12, 1, 4, 255, 255, 255, 255, 2, 4, 255, 255, 255, 255, 115, 204, 125, 143];

#[derive(Serialize, Deserialize, Clone)]
struct Lineup {
    #[serde(rename = "GuideNumber")]
    guide_number: String,
    #[serde(rename = "GuideName")]
    guide_name: String,
    #[serde(rename = "URL")]
    url: String,
}

#[derive(Serialize, Deserialize, Clone)]
struct Device {
    #[serde(rename = "FriendlyName")]
    friendly_name: String,
    #[serde(rename = "Manufacturer")]
    manufacturer: String,
    // #[serde(rename = "ManufacturerURL")]
    // manufacturer_url: String,
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

}

impl Device {
    fn as_xml(&self) -> String {
        format!(r#"<root xmlns="urn:schemas-upnp-org:device-1-0">
<specVersion>
<major>1</major>
<minor>0</minor>
</specVersion>
<URLBase>{}</URLBase>
<device>
  <deviceType>urn:dial-multicast:com.silicondust.hdhomerun</deviceType>
  <friendlyName>{}</friendlyName>
  <manufacturer>{}</manufacturer>
  <modelName>{}</modelName>
  <modelNumber>{}</modelNumber>
  <tunerCount>{}</tunerCount>
  <serialNumber>{}</serialNumber>
  <UDN>uuid:{}</UDN>
</device>
</root>"#,
                self.base_url, self.friendly_name, self.manufacturer, self.model_number,
                self.model_number, self.tuner_count, self.id, self.id
        )
    }
}

fn xtream_item_to_lineup_stream<I>(cfg: Arc<AppConfig>, cluster: XtreamCluster, credentials: Arc<ProxyUserCredentials>,
                                   base_url: Option<String>, channels: Option<I>) -> impl Stream<Item=Result<Bytes, String>>
where
    I: Iterator<Item=(XtreamPlaylistItem, bool)> + 'static,
{
    match channels {
        Some(chans) => {
            let mapped = chans.map(move |(item, has_next)| {
                let input_options = cfg.get_input_options_by_name(&item.input_name);
                let (live_stream_use_prefix, live_stream_without_extension) = input_options.as_ref()
                    .map_or((true, false), |o| (o.xtream_live_stream_use_prefix, o.xtream_live_stream_without_extension));
                let container_extension = item.get_additional_property("container_extension").map(|v| get_string_from_serde_value(&v).unwrap_or_default());
                let stream_url = match &base_url {
                    None => item.url.to_string(),
                    Some(url) => get_xtream_url(cluster, url, &credentials.username, &credentials.password, item.virtual_id, container_extension.as_ref(), live_stream_use_prefix, live_stream_without_extension)
                };

                let lineup = Lineup {
                    guide_number: item.epg_channel_id.unwrap_or(item.name).to_string(),
                    guide_name: item.title.to_string(),
                    url: stream_url,
                };
                match serde_json::to_string(&lineup) {
                    Ok(content) => {
                        Ok(Bytes::from(if has_next {
                            format!("{content},")
                        } else {
                            content
                        }))
                    }
                    Err(_) => Ok(Bytes::from("")),
                }
            });
            stream::iter(mapped).left_stream()
        }
        None => {
            stream::once(async { Ok(Bytes::from("")) }).right_stream()
        }
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
                    guide_number: item.epg_channel_id.unwrap_or(item.name).to_string(),
                    guide_name: item.title.to_string(),
                    url: (if item.t_stream_url.is_empty() { &item.url } else { &item.t_stream_url }).to_string(),
                };
                match serde_json::to_string(&lineup) {
                    Ok(content) => {
                        Ok(Bytes::from(if has_next {
                            format!("{content},")
                        } else {
                            content
                        }))
                    }
                    Err(_) => Ok(Bytes::from("")),
                }
            });
            stream::iter(mapped).left_stream()
        }
        None => {
            stream::once(async { Ok(Bytes::from("")) }).right_stream()
        }
    }
}

fn create_device(app_state: &Arc<HdHomerunAppState>) -> Option<Device> {
    if let Some(credentials) = app_state.app_state.app_config.get_user_credentials(&app_state.device.t_username) {
        let server_info = app_state.app_state.app_config.get_user_server_info(&credentials);
        let device = &app_state.device;
        let device_url = format!("{}://{}:{}", server_info.protocol, server_info.host, device.port);
        Some(Device {
            friendly_name: device.friendly_name.to_string(),
            manufacturer: device.manufacturer.to_string(),
            //manufacturer_url: "https://github.com/euzu/tuliprox".to_string(),
            model_number: device.model_number.to_string(),
            model_name: device.model_name.to_string(),
            firmware_name: device.firmware_name.to_string(),
            tuner_count: device.tuner_count,
            firmware_version: device.firmware_version.to_string(),
            auth: String::new(),
            id: device.device_udn.to_string(),
            lineup_url: format!("{device_url}/lineup.json"),
            discover_url: format!("{device_url}/discover.json"),
            base_url: device_url,
        })
    } else {
        error!("Failed to get credentials for username: {} for device: {} ", &app_state.device.t_username, &app_state.device.name);
        None
    }
}

async fn device_xml(axum::extract::State(app_state): axum::extract::State<Arc<HdHomerunAppState>>) -> impl IntoResponse {
    if let Some(device) = create_device(&app_state) {
        axum::response::Response::builder()
            .status(axum::http::StatusCode::OK)
            .header(axum::http::header::CONTENT_TYPE, "application/xml")
            .body(axum::body::Body::from(device.as_xml()))
            .unwrap()
            .into_response()
    } else {
        axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
    }
}

async fn device_json(axum::extract::State(app_state): axum::extract::State<Arc<HdHomerunAppState>>) -> impl IntoResponse {
    if let Some(device) = create_device(&app_state) {
        axum::Json(device).into_response()
    } else {
        axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
    }
}

async fn discover_json(axum::extract::State(app_state): axum::extract::State<Arc<HdHomerunAppState>>) -> impl IntoResponse {
    if let Some(device) = create_device(&app_state) {
        axum::Json(device).into_response()
    } else {
        axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
    }
}

async fn lineup_status() -> impl IntoResponse {
    axum::Json(json!({
                "ScanInProgress": 0,
                "ScanPossible": 0,
                "Source": "Cable",
                "SourceList": ["Cable"],
            }))
}

async fn lineup(app_state: &Arc<HdHomerunAppState>, cfg: &Arc<AppConfig>, credentials: &Arc<ProxyUserCredentials>, target: &ConfigTarget) -> impl IntoResponse {
    let use_output = target.get_hdhomerun_output().as_ref().and_then(|o| o.use_output);
    let use_all = use_output.is_none();
    let use_m3u = use_output.as_ref() == Some(&TargetType::M3u);
    let use_xtream = use_output.as_ref() == Some(&TargetType::Xtream);
    if (use_all || use_m3u) && target.has_output(&TargetType::M3u) {
        let iterator = M3uPlaylistIterator::new(cfg, target, credentials).await.ok();
        let stream = m3u_item_to_lineup_stream(iterator);
        let body_stream = stream::once(async { Ok(Bytes::from("[")) })
            .chain(stream)
            .chain(stream::once(async { Ok(Bytes::from("]")) }));
        return axum::response::Response::builder()
            .status(axum::http::StatusCode::OK)
            .header(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
            .body(axum::body::Body::from_stream(body_stream))
            .unwrap().into_response();
    } else if (use_all || use_xtream) && target.has_output(&TargetType::Xtream) {
        let server_info = app_state.app_state.app_config.get_user_server_info(credentials);
        let base_url = server_info.get_base_url();

        let base_url_live = if credentials.proxy.is_redirect(PlaylistItemType::Live) || target.is_force_redirect(PlaylistItemType::Live) { None } else { Some(base_url.clone()) };
        let base_url_vod = if credentials.proxy.is_redirect(PlaylistItemType::Video) || target.is_force_redirect(PlaylistItemType::Video) { None } else { Some(base_url) };

        let live_channels = XtreamPlaylistIterator::new(XtreamCluster::Live, cfg, target, None, credentials).await.ok();
        let vod_channels = XtreamPlaylistIterator::new(XtreamCluster::Video, cfg, target, None, credentials).await.ok();
        // TODO include series when resolved
        //let series_channels = xtream_repository::iter_raw_xtream_playlist(cfg, target, XtreamCluster::Series);
        let live_stream = xtream_item_to_lineup_stream(Arc::clone(cfg), XtreamCluster::Live, Arc::clone(credentials), base_url_live.clone(), live_channels);
        let vod_stream = xtream_item_to_lineup_stream(Arc::clone(cfg), XtreamCluster::Video, Arc::clone(credentials), base_url_vod.clone(), vod_channels);
        let live_stream_peek = live_stream.peekable();
        let vod_stream_peek = vod_stream.peekable();
        // helper to decide if a comma is needed
        let comma_stream = if live_stream_peek.size_hint().0 > 0 && vod_stream_peek.size_hint().0 > 0 {
            stream::once(async { Ok(Bytes::from(",")) }).left_stream()
        } else {
            stream::empty().right_stream()
        };
        let body_stream = stream::once(async { Ok(Bytes::from("[")) })
            .chain(live_stream_peek)
            .chain(comma_stream)
            .chain(vod_stream_peek)
            .chain(stream::once(async { Ok(Bytes::from("]")) }));
        return axum::response::Response::builder()
            .status(axum::http::StatusCode::OK)
            .header(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
            .body(axum::body::Body::from_stream(body_stream))
            .unwrap()
            .into_response();
    }
    axum::http::StatusCode::NOT_FOUND.into_response()
}

async fn auth_lineup_json(AuthBasic((username, password)): AuthBasic, axum::extract::State(app_state): axum::extract::State<Arc<HdHomerunAppState>>) -> impl IntoResponse {
    let cfg = Arc::clone(&app_state.app_state.app_config);
    if let Some((credentials, target)) = cfg.get_target_for_username(&app_state.device.t_username) {
        if !username.eq(&credentials.username) || !password.eq(&credentials.password) {
            return axum::http::StatusCode::UNAUTHORIZED.into_response();
        }
        let user_credentials = Arc::new(credentials);
        return lineup(&app_state, &cfg, &user_credentials, &target).await.into_response();
    }
    axum::http::StatusCode::NOT_FOUND.into_response()
}

async fn lineup_json(axum::extract::State(app_state): axum::extract::State<Arc<HdHomerunAppState>>) -> impl IntoResponse {
    let cfg = Arc::clone(&app_state.app_state.app_config);
    if let Some((credentials, target)) = cfg.get_target_for_username(&app_state.device.t_username) {
        let user_credentials = Arc::new(credentials);
        return lineup(&app_state, &cfg, &user_credentials, &target).await.into_response();
    }
    axum::http::StatusCode::NOT_FOUND.into_response()
}

async fn auto_channel(axum::extract::State(_app_state): axum::extract::State<Arc<HdHomerunAppState>>,
                      axum::extract::Path(channel): axum::extract::Path<String>) -> impl IntoResponse {
    warn!("HdHomerun api not implemented for auto_channel {channel}");
    axum::http::StatusCode::NOT_FOUND.into_response()
}

pub fn hdhr_api_register(basic_auth: bool) -> axum::Router<Arc<HdHomerunAppState>> {
    axum::Router::new()
        .route("/device.xml", axum::routing::get(device_xml))
        .route("/device.json", axum::routing::get(device_json))
        .route("/discover.json", axum::routing::get(discover_json))
        .route("/lineup_status.json", axum::routing::get(lineup_status))
        .route("/lineup.json", if basic_auth { axum::routing::get(auth_lineup_json) } else { axum::routing::get(lineup_json) })
        // cfg.service(web::resource("/lineup.xml").route(web::get().to(lineup_xml)));
        // cfg.service(web::resource("/lineup.m3u").route(web::get().to(lineup_m3u)));
        .route("/auto/{channel}", axum::routing::get(auto_channel))
        .route("/tuner{tuner_num}/{channel}", axum::routing::get(auto_channel))
}

// fn start_hdhomerum_discovery_handler(ssdp_socket: Arc<UdpSocket>, server: String, location: String, cache_control: String, usn: String) {
//     let mut buffer = [0; 4096];
//     actix_rt::spawn(async move {
//         let response_bytes = RESPONSE_BYTES;
//         loop {
//             match ssdp_socket.recv_from(&mut buffer).await {
//                 Ok((size, src_addr)) => {
//                     let content = &buffer[..size];
//                     if content == DISCOVERY_BYTES {
//                         match ssdp_socket.send_to(&response_bytes, src_addr).await {
//                             Err(err) => eprintln!("Failed to send SSDP response: {err:?}"),
//                             Ok(_) => println!("Sent SSDP response to: {src_addr:?}"),
//                         }
//                     }
//                 }
//                 Err(err) => eprintln!("Failed to receive SSDP request: {err:?}"),
//             }
//         }
//     });
// }
//
// pub async fn start_hdhomerun(/*host: &str, */port: u16) {
//     let version = "2021.08.18";
//     let server_url = format!("http://10.41.41.89:{port}");
//
//     // let multicast_addr: Ipv4Addr = "255.255.255.255".parse().unwrap();
//
//     let socket_addr: SocketAddr = "0.0.0.0:65001".parse().unwrap();
//     let socket = Socket::new(Domain::IPV4, Type::DGRAM, None).unwrap();
//     // setting SO_REUSEADDR-Option if other dlna server is running
//     socket.set_reuse_address(true).unwrap();
//     socket.bind(&socket_addr.into()).unwrap();
//     let udp_socket = UdpSocket::from_std(socket.into()).unwrap();
//
//     let ssdp_socket = Arc::new(udp_socket);
//     // ssdp_socket.join_multicast_v4(multicast_addr, "0.0.0.0".parse().unwrap()).unwrap();
//     let server = format!("SERVER: HDHomeRun/{}", version);
//     let location = format!("LOCATION: {server_url}/device.xml");
//     let cache_control = "CACHE-CONTROL: max-age=1800";
//     let usn = "USN: uuid:12345678-90ab-cdef-1234-567890abcdef::urn:dial-multicast:com.silicondust.hdhomerun";
//     start_hdhomerum_discovery_handler(Arc::clone(&ssdp_socket), server.to_string(), location.to_string(), cache_control.to_string(), usn.to_string());
// }
