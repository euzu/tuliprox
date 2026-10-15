use crate::{
    api::{
        api_utils::{coalesce_byte_stream, internal_server_error, try_unwrap_body},
        model::HdHomerunAppState,
    },
    auth::{try_check_network_access_only, AuthBasic, Fingerprint},
    model::{AppConfig, ConfigInputFlags, ConfigTarget, ProxyUserCredentials},
    processing::parser::xtream::get_xtream_url,
    repository::{iter_raw_m3u_target_playlist, M3uPlaylistIterator, XtreamPlaylistIterator},
    utils::arc_str_serde,
};
use axum::response::IntoResponse;
use bytes::Bytes;
use futures::{stream, Stream, StreamExt};
use log::{error, warn};
use serde::{Deserialize, Serialize};
use serde_json::json;
use shared::{
    error::TuliproxError,
    model::{M3uPlaylistItem, PlaylistItemType, TargetType, XtreamCluster, XtreamPlaylistItem},
    utils::concat_path,
};
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
            self.id,  // Correct: 8-digit hex device ID
            self.udn  // Correct: Application/Device UUID for UDN
        )
    }
}

#[inline]
#[allow(clippy::unnecessary_wraps)]
fn serialize_lineup(lineup: &Lineup, has_next: bool) -> Result<Bytes, String> {
    match serde_json::to_string(lineup) {
        Ok(mut content) => {
            if has_next {
                content.push(',');
            }
            Ok(Bytes::from(content))
        }
        Err(_) => Ok(Bytes::from("")),
    }
}

fn get_xtream_item_stream_url(
    cfg: &Arc<AppConfig>,
    cluster: XtreamCluster,
    credentials: &ProxyUserCredentials,
    base_url: Option<&str>,
    item: &XtreamPlaylistItem,
) -> Arc<str> {
    let input = cfg.get_input_by_name(&item.input_name);
    let (live_stream_use_prefix, live_stream_without_extension) = input.as_ref().map_or((true, false), |i| {
        (
            i.has_flag(ConfigInputFlags::XtreamLiveStreamUsePrefix),
            i.has_flag(ConfigInputFlags::XtreamLiveStreamWithoutExtension),
        )
    });
    let container_extension = item.get_container_extension();
    match base_url {
        None => item.url.clone(),
        Some(url) => get_xtream_url(
            cluster,
            url,
            &credentials.username,
            &credentials.password,
            item.virtual_id.get(),
            container_extension.as_deref(),
            live_stream_use_prefix,
            live_stream_without_extension,
        )
        .into(),
    }
}

#[inline]
fn get_m3u_item_stream_url(item: &M3uPlaylistItem) -> Arc<str> {
    if item.t_stream_url.is_empty() {
        item.url.clone()
    } else {
        item.t_stream_url.clone()
    }
}

/// Base urls for xtream live and vod stream urls; `None` means the provider url is used (redirect).
fn xtream_base_urls(
    credentials: &ProxyUserCredentials,
    target: &ConfigTarget,
    base_url: String,
) -> (Option<String>, Option<String>) {
    let use_base_url = |item_type| !(credentials.proxy.is_redirect(item_type) || target.is_force_redirect(item_type));
    let live = use_base_url(PlaylistItemType::Live).then(|| base_url.clone());
    let vod = use_base_url(PlaylistItemType::Video).then_some(base_url);
    (live, vod)
}

fn xtream_item_to_lineup_stream<I>(
    cfg: Arc<AppConfig>,
    cluster: XtreamCluster,
    credentials: Arc<ProxyUserCredentials>,
    base_url: Option<String>,
    channels: Option<I>,
) -> impl Stream<Item = Result<Bytes, String>>
where
    I: Stream<Item = Result<(XtreamPlaylistItem, bool), TuliproxError>> + Send + Unpin + 'static,
{
    match channels {
        Some(chans) => {
            let mapped = chans.map(move |entry| {
                let (item, has_next) = entry.map_err(|error| error.to_string())?;
                let stream_url = get_xtream_item_stream_url(&cfg, cluster, &credentials, base_url.as_deref(), &item);

                let lineup = Lineup {
                    guide_number: item.epg_channel_id.unwrap_or(item.name.clone()),
                    guide_name: item.title.clone(),
                    url: stream_url,
                };
                serialize_lineup(&lineup, has_next)
            });
            mapped.left_stream()
        }
        None => stream::once(async { Ok(Bytes::from("")) }).right_stream(),
    }
}

fn m3u_item_to_lineup_stream<I>(channels: Option<I>) -> impl Stream<Item = Result<Bytes, String>>
where
    I: Stream<Item = Result<(M3uPlaylistItem, bool), TuliproxError>> + Send + Unpin + 'static,
{
    match channels {
        Some(chans) => {
            let mapped = chans.map(move |entry| {
                let (item, has_next) = entry.map_err(|error| error.to_string())?;
                let lineup = Lineup {
                    guide_number: item.epg_channel_id.clone().unwrap_or(item.name.clone()),
                    guide_name: item.title.clone(),
                    url: get_m3u_item_stream_url(&item),
                };
                serialize_lineup(&lineup, has_next)
            });
            mapped.left_stream()
        }
        None => stream::once(async { Ok(Bytes::from("")) }).right_stream(),
    }
}

fn create_device(app_state: &Arc<HdHomerunAppState>) -> Option<Device> {
    if let Some(credentials) = app_state.app_state.app_config.get_user_credentials(&app_state.device.t_username) {
        let server_info = app_state.app_state.app_config.get_user_server_info(&credentials)?;
        let device = &app_state.device;
        let device_url = format!("{}://{}:{}", server_info.protocol, server_info.host, device.port);

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
            app_state.device.t_username, app_state.device.name
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
    fingerprint: Fingerprint,
    axum::extract::State(app_state): axum::extract::State<Arc<HdHomerunAppState>>,
) -> impl IntoResponse {
    let cfg = Arc::clone(&app_state.app_state.app_config);
    let network_access_denied =
        cfg.get_target_for_username(&app_state.device.t_username).is_none_or(|(credentials, _)| {
            try_check_network_access_only(
                &credentials,
                &fingerprint,
                &app_state.app_state.app_config,
                &app_state.app_state.geoip,
            )
            .is_err()
        });
    let current_state = app_state.hd_scan_state.load(std::sync::atomic::Ordering::Acquire);
    if network_access_denied || current_state < 0 {
        axum::Json(json!({
            "ScanInProgress": 0,
            "ScanPossible": i32::from(!network_access_denied),
            "Source": "Cable",
            "SourceList": ["Cable"],
        }))
        .into_response()
    } else {
        let new_state = current_state.saturating_add(20);
        let final_state = if new_state > 100 { 100 } else { new_state };

        let cfg = Arc::clone(&app_state.app_state.app_config);
        let num_of_channels =
            if let Some((credentials, target)) = cfg.get_target_for_username(&app_state.device.t_username) {
                if target.has_output(TargetType::M3u) {
                    if let Some(iter) = iter_raw_m3u_target_playlist(&cfg, &target, None).await {
                        let cred = Arc::clone(&credentials);
                        iter.filter_map(move |res| {
                            let cred = Arc::clone(&cred);
                            async move {
                                let item = res.ok()?;
                                cred.allows_item_type(item.item_type).then_some(item)
                            }
                        })
                        .count()
                        .await
                    } else {
                        0
                    }
                } else if target.has_output(TargetType::Xtream) {
                    let cred = Arc::clone(&credentials);
                    let live = if cred.allows_cluster(XtreamCluster::Live) {
                        match XtreamPlaylistIterator::new(XtreamCluster::Live, &cfg, &target, None, &cred).await {
                            Ok(stream) => stream.filter_map(|entry| async move { entry.ok() }).count().await,
                            Err(_) => 0,
                        }
                    } else {
                        0
                    };
                    let vod = if cred.allows_cluster(XtreamCluster::Video) {
                        match XtreamPlaylistIterator::new(XtreamCluster::Video, &cfg, &target, None, &cred).await {
                            Ok(stream) => stream.filter_map(|entry| async move { entry.ok() }).count().await,
                            Err(_) => 0,
                        }
                    } else {
                        0
                    };
                    live + vod
                } else {
                    0
                }
            } else {
                0
            };

        if final_state >= 100 {
            app_state.hd_scan_state.store(-1, std::sync::atomic::Ordering::Release);
        } else {
            app_state.hd_scan_state.store(final_state, std::sync::atomic::Ordering::Release);
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
    fingerprint: Fingerprint,
    axum::extract::State(app_state): axum::extract::State<Arc<HdHomerunAppState>>,
    axum::extract::Query(query): axum::extract::Query<LineupPostQuery>,
) -> impl IntoResponse {
    let cfg = Arc::clone(&app_state.app_state.app_config);
    let allowed = cfg.get_target_for_username(&app_state.device.t_username).is_some_and(|(credentials, _)| {
        try_check_network_access_only(
            &credentials,
            &fingerprint,
            &app_state.app_state.app_config,
            &app_state.app_state.geoip,
        )
        .is_ok()
    });
    if !allowed {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }
    match query.scan.as_str() {
        "start" => {
            app_state.hd_scan_state.store(0, std::sync::atomic::Ordering::Release);
            axum::http::StatusCode::OK.into_response()
        }
        "abort" => {
            app_state.hd_scan_state.store(-1, std::sync::atomic::Ordering::Release);
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
    let use_output = target.get_hdhomerun_output().as_ref().and_then(|o| o.use_output);
    let use_all = use_output.is_none();
    let use_m3u = use_output.as_ref() == Some(&TargetType::M3u);
    let use_xtream = use_output.as_ref() == Some(&TargetType::Xtream);
    if (use_all || use_m3u) && target.has_output(TargetType::M3u) {
        let iterator = M3uPlaylistIterator::new(cfg, target, credentials).await.ok();
        let stream = m3u_item_to_lineup_stream(iterator);
        let body_stream = stream::once(async { Ok(Bytes::from("[")) })
            .chain(stream)
            .chain(stream::once(async { Ok(Bytes::from("]")) }));
        return try_unwrap_body!(axum::response::Response::builder()
            .status(axum::http::StatusCode::OK)
            .header(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
            .body(axum::body::Body::from_stream(coalesce_byte_stream(body_stream))));
    } else if (use_all || use_xtream) && target.has_output(TargetType::Xtream) {
        let Some(server_info) = app_state.app_state.app_config.get_user_server_info(credentials) else {
            return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        let (base_url_live, base_url_vod) = xtream_base_urls(credentials, target, server_info.get_base_url());

        let live_channels = if credentials.allows_cluster(XtreamCluster::Live) {
            XtreamPlaylistIterator::new(XtreamCluster::Live, cfg, target, None, credentials).await.ok()
        } else {
            None
        };
        let vod_channels = if credentials.allows_cluster(XtreamCluster::Video) {
            XtreamPlaylistIterator::new(XtreamCluster::Video, cfg, target, None, credentials).await.ok()
        } else {
            None
        };
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
        let both_non_empty =
            live_stream_peek.as_mut().peek().await.is_some() && vod_stream_peek.as_mut().peek().await.is_some();
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
            .header(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
            .body(axum::body::Body::from_stream(coalesce_byte_stream(body_stream))));
    }
    axum::http::StatusCode::NOT_FOUND.into_response()
}

/// Resolves the device user and checks basic auth (when given) and network access.
fn authorize_device_user(
    app_state: &HdHomerunAppState,
    fingerprint: &Fingerprint,
    basic_auth: Option<&(String, String)>,
) -> Result<(Arc<ProxyUserCredentials>, Arc<ConfigTarget>), Box<axum::response::Response>> {
    let cfg = &app_state.app_state.app_config;
    let Some((credentials, target)) = cfg.get_target_for_username(&app_state.device.t_username) else {
        return Err(Box::new(axum::http::StatusCode::NOT_FOUND.into_response()));
    };
    if let Some((username, password)) = basic_auth {
        if *username != credentials.username || *password != credentials.password {
            return Err(Box::new(axum::http::StatusCode::UNAUTHORIZED.into_response()));
        }
    }
    try_check_network_access_only(&credentials, fingerprint, cfg, &app_state.app_state.geoip)
        .map_err(|err| Box::new(err.into_player_response(cfg.get_auth_error_status())))?;
    Ok((credentials, target))
}

async fn authorized_lineup(
    app_state: &Arc<HdHomerunAppState>,
    fingerprint: &Fingerprint,
    basic_auth: Option<&(String, String)>,
) -> axum::response::Response {
    match authorize_device_user(app_state, fingerprint, basic_auth) {
        Ok((credentials, target)) => {
            lineup(app_state, &app_state.app_state.app_config, &credentials, &target).await.into_response()
        }
        Err(response) => *response,
    }
}

async fn auth_lineup_json(
    fingerprint: Fingerprint,
    AuthBasic(basic_auth): AuthBasic,
    axum::extract::State(app_state): axum::extract::State<Arc<HdHomerunAppState>>,
) -> impl IntoResponse {
    authorized_lineup(&app_state, &fingerprint, Some(&basic_auth)).await
}

async fn lineup_json(
    fingerprint: Fingerprint,
    axum::extract::State(app_state): axum::extract::State<Arc<HdHomerunAppState>>,
) -> impl IntoResponse {
    authorized_lineup(&app_state, &fingerprint, None).await
}

/// Channel identifier requested via `/auto/{channel}` or `/tuner{n}/{channel}`.
///
/// Clients request the `GuideNumber` from `lineup.json`, optionally as `v<number>` and/or with a
/// `.ts` suffix. The `GuideNumber` is the epg channel id or the name, so a leading `v` is only
/// treated as virtual channel prefix when the remainder is numeric (`v101`, `v2.1`); `vox.de`
/// stays untouched.
#[derive(Debug, PartialEq)]
struct ChannelQuery<'a> {
    raw: &'a str,
    numeric: Option<&'a str>,
    virtual_id: Option<u32>,
}

impl<'a> ChannelQuery<'a> {
    fn parse(channel: &'a str) -> Option<Self> {
        let trimmed = channel.trim();
        let raw = trimmed
            .len()
            .checked_sub(3)
            .and_then(|idx| trimmed.get(idx..).filter(|ext| ext.eq_ignore_ascii_case(".ts")).map(|_| &trimmed[..idx]))
            .unwrap_or(trimmed)
            .trim();
        if raw.is_empty() {
            return None;
        }
        let numeric = raw
            .strip_prefix(['v', 'V'])
            .filter(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit() || b == b'.'));
        let virtual_id = numeric.unwrap_or(raw).parse::<u32>().ok();
        Some(Self { raw, numeric, virtual_id })
    }

    /// Match quality, lower is better: exact `GuideNumber`, `GuideNumber` without `v` prefix,
    /// virtual id. The `GuideNumber` wins so a numeric guide number never resolves to another
    /// channel that happens to carry the same virtual id.
    fn rank<C: LineupChannel>(&self, channel: &C) -> Option<u8> {
        let guide_number = channel.guide_number();
        if guide_number == self.raw {
            Some(0)
        } else if self.numeric == Some(guide_number) {
            Some(1)
        } else if self.virtual_id == Some(channel.virtual_id()) {
            Some(2)
        } else {
            None
        }
    }
}

trait LineupChannel {
    fn guide_number(&self) -> &str;
    fn virtual_id(&self) -> u32;
}

impl LineupChannel for M3uPlaylistItem {
    fn guide_number(&self) -> &str { self.epg_channel_id.as_deref().unwrap_or(&self.name) }
    fn virtual_id(&self) -> u32 { self.virtual_id.get() }
}

impl LineupChannel for XtreamPlaylistItem {
    fn guide_number(&self) -> &str { self.epg_channel_id.as_deref().unwrap_or(&self.name) }
    fn virtual_id(&self) -> u32 { self.virtual_id.get() }
}

/// Best matching channel of a lineup iterator together with its rank.
///
/// Uses the same user scoped iterators as `lineup.json`, so bouquet, user filter and url rewrite
/// apply exactly as for the published lineup.
async fn find_channel<C, S>(mut channels: S, query: &ChannelQuery<'_>) -> Option<(u8, C)>
where
    C: LineupChannel,
    S: Stream<Item = Result<(C, bool), TuliproxError>> + Unpin,
{
    let mut best: Option<(u8, C)> = None;
    while let Some(entry) = channels.next().await {
        let Ok((channel, _has_next)) = entry else { continue };
        let Some(rank) = query.rank(&channel) else { continue };
        if rank == 0 {
            return Some((rank, channel));
        }
        if best.as_ref().is_none_or(|(best_rank, _)| rank < *best_rank) {
            best = Some((rank, channel));
        }
    }
    best
}

async fn resolve_channel_stream_url(
    cfg: &Arc<AppConfig>,
    credentials: &Arc<ProxyUserCredentials>,
    target: &ConfigTarget,
    query: &ChannelQuery<'_>,
) -> Option<Arc<str>> {
    let use_output = target.get_hdhomerun_output().as_ref().and_then(|o| o.use_output);
    let use_all = use_output.is_none();
    let use_m3u = use_output.as_ref() == Some(&TargetType::M3u);
    let use_xtream = use_output.as_ref() == Some(&TargetType::Xtream);

    if (use_all || use_m3u) && target.has_output(TargetType::M3u) {
        let channels = M3uPlaylistIterator::new(cfg, target, credentials).await.ok()?;
        return find_channel(channels, query).await.map(|(_, item)| get_m3u_item_stream_url(&item));
    }
    if (use_all || use_xtream) && target.has_output(TargetType::Xtream) {
        let server_info = cfg.get_user_server_info(credentials)?;
        let (base_url_live, base_url_vod) = xtream_base_urls(credentials, target, server_info.get_base_url());
        let mut best: Option<(u8, Arc<str>)> = None;
        for (cluster, base_url) in [(XtreamCluster::Live, base_url_live), (XtreamCluster::Video, base_url_vod)] {
            if !credentials.allows_cluster(cluster) {
                continue;
            }
            let Ok(channels) = XtreamPlaylistIterator::new(cluster, cfg, target, None, credentials).await else {
                continue;
            };
            let Some((rank, item)) = find_channel(channels, query).await else { continue };
            if best.as_ref().is_none_or(|(best_rank, _)| rank < *best_rank) {
                let url = get_xtream_item_stream_url(cfg, cluster, credentials, base_url.as_deref(), &item);
                if rank == 0 {
                    return Some(url);
                }
                best = Some((rank, url));
            }
        }
        return best.map(|(_, url)| url);
    }
    None
}

/// Redirects to the stream url published in `lineup.json`.
///
/// Connection limits are enforced by the stream endpoint (`max_connections`, grace period). A
/// pre-check here would reject channel switches while the previous stream is still counted.
async fn tune_channel(
    app_state: &Arc<HdHomerunAppState>,
    fingerprint: &Fingerprint,
    basic_auth: Option<&(String, String)>,
    tuner_num: Option<u8>,
    channel: &str,
) -> axum::response::Response {
    let (credentials, target) = match authorize_device_user(app_state, fingerprint, basic_auth) {
        Ok(user) => user,
        Err(response) => return *response,
    };

    if let Some(tuner) = tuner_num.filter(|tuner| *tuner >= app_state.device.tuner_count) {
        warn!(
            "Requested tuner {tuner} exceeds tuner count {} for device '{}'",
            app_state.device.tuner_count, app_state.device.name
        );
        return axum::http::StatusCode::NOT_FOUND.into_response();
    }

    let Some(query) = ChannelQuery::parse(channel) else {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    };

    let Some(stream_url) =
        resolve_channel_stream_url(&app_state.app_state.app_config, &credentials, &target, &query).await
    else {
        warn!("Channel '{channel}' not found for HDHomeRun device '{}'", app_state.device.name);
        return axum::http::StatusCode::NOT_FOUND.into_response();
    };
    axum::response::Redirect::temporary(&stream_url).into_response()
}

async fn auto_channel(
    fingerprint: Fingerprint,
    axum::extract::State(app_state): axum::extract::State<Arc<HdHomerunAppState>>,
    axum::extract::Path(channel): axum::extract::Path<String>,
) -> impl IntoResponse {
    tune_channel(&app_state, &fingerprint, None, None, &channel).await
}

async fn tuner_channel(
    fingerprint: Fingerprint,
    axum::extract::State(app_state): axum::extract::State<Arc<HdHomerunAppState>>,
    axum::extract::Path((tuner_num, channel)): axum::extract::Path<(u8, String)>,
) -> impl IntoResponse {
    tune_channel(&app_state, &fingerprint, None, Some(tuner_num), &channel).await
}

async fn auth_auto_channel(
    fingerprint: Fingerprint,
    AuthBasic(basic_auth): AuthBasic,
    axum::extract::State(app_state): axum::extract::State<Arc<HdHomerunAppState>>,
    axum::extract::Path(channel): axum::extract::Path<String>,
) -> impl IntoResponse {
    tune_channel(&app_state, &fingerprint, Some(&basic_auth), None, &channel).await
}

async fn auth_tuner_channel(
    fingerprint: Fingerprint,
    AuthBasic(basic_auth): AuthBasic,
    axum::extract::State(app_state): axum::extract::State<Arc<HdHomerunAppState>>,
    axum::extract::Path((tuner_num, channel)): axum::extract::Path<(u8, String)>,
) -> impl IntoResponse {
    tune_channel(&app_state, &fingerprint, Some(&basic_auth), Some(tuner_num), &channel).await
}

pub fn hdhr_api_register(basic_auth: bool) -> axum::Router<Arc<HdHomerunAppState>> {
    axum::Router::new()
        .route("/device.xml", axum::routing::get(device_xml))
        .route("/device.json", axum::routing::get(device_json))
        .route("/discover.json", axum::routing::get(discover_json))
        .route("/lineup_status.json", axum::routing::get(lineup_status))
        .route(
            "/lineup.json",
            if basic_auth { axum::routing::get(auth_lineup_json) } else { axum::routing::get(lineup_json) },
        )
        .route("/lineup.post", axum::routing::post(lineup_post))
        .route(
            "/auto/{channel}",
            if basic_auth { axum::routing::get(auth_auto_channel) } else { axum::routing::get(auto_channel) },
        )
        .route(
            "/tuner{tuner_num}/{channel}",
            if basic_auth { axum::routing::get(auth_tuner_channel) } else { axum::routing::get(tuner_channel) },
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::model::create_test_app_state,
        model::{
            ApiProxyConfig, ApiProxyServerInfo, Config, ConfigInput, ConfigSource, HdHomeRunDeviceConfig,
            SourcesConfig, TargetUser,
        },
        repository::{ensure_target_storage_path, m3u_write_playlist},
    };
    use axum::{
        body::Body,
        http::{header, Request, StatusCode},
    };
    use shared::{
        model::{
            ClusterFlags, ConfigTargetDto, HdHomeRunDeviceConfigDto, M3uTargetOutputDto, PlaylistGroup, PlaylistItem,
            PlaylistItemHeader, ProxyType, TargetOutputDto, VirtualId,
        },
        utils::Internable,
    };
    use std::sync::atomic::AtomicI8;
    use tower::ServiceExt;

    const PROVIDER_HOST: &str = "http://provider.example";

    fn test_hdhr_app_state(username: &str, tuner_count: u8) -> Arc<HdHomerunAppState> {
        let app_state = create_test_app_state(Config::default());
        let dto = HdHomeRunDeviceConfigDto { name: "test_hdhr".to_string(), tuner_count, ..Default::default() };
        let mut device: HdHomeRunDeviceConfig = (&dto).into();
        device.t_username = username.to_string();
        device.t_enabled = true;
        Arc::new(HdHomerunAppState { app_state, device: Arc::new(device), hd_scan_state: Arc::new(AtomicI8::new(-1)) })
    }

    fn make_request(uri: &str) -> Request<Body> {
        let mut req = Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap();
        req.extensions_mut().insert(axum::extract::ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 1234))));
        req
    }

    fn m3u_item(virtual_id: u32, name: &str, epg_channel_id: Option<&str>) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                id: virtual_id.to_string().intern(),
                virtual_id: VirtualId::new(virtual_id),
                name: name.intern(),
                title: name.intern(),
                group: "News".intern(),
                epg_channel_id: epg_channel_id.map(Internable::intern),
                url: format!("{PROVIDER_HOST}/live/puser/ppass/{virtual_id}.ts").intern(),
                input_name: "input".intern(),
                input_stream_id: virtual_id.to_string().intern(),
                item_type: PlaylistItemType::Live,
                xtream_cluster: XtreamCluster::Live,
                ..PlaylistItemHeader::default()
            },
        }
    }

    /// Device with an M3U target persisted on disk, served to a reverse proxy user.
    async fn m3u_device(items: Vec<PlaylistItem>) -> (tempfile::TempDir, Arc<HdHomerunAppState>) {
        let temp = tempfile::tempdir().unwrap();
        let state = test_hdhr_app_state("hdhr", 2);
        let app_config = &state.app_state.app_config;

        let mut config = (*app_config.config.load_full()).clone();
        config.storage_dir = temp.path().to_string_lossy().into_owned();
        app_config.config.store(Arc::new(config));

        let target = ConfigTarget::from(&ConfigTargetDto {
            id: 1,
            name: "default".to_string(),
            output: vec![TargetOutputDto::M3u(M3uTargetOutputDto::default())],
            ..Default::default()
        });
        let input = Arc::new(ConfigInput { id: 1, name: "input".intern(), enabled: true, ..ConfigInput::default() });
        app_config.sources.store(Arc::new(SourcesConfig {
            inputs: vec![Arc::clone(&input)],
            sources: vec![ConfigSource {
                inputs: vec![Arc::clone(&input.name)],
                targets: vec![Arc::new(target.clone())],
            }],
            ..SourcesConfig::default()
        }));

        let mut user = ProxyUserCredentials::default();
        user.username = "hdhr".to_string();
        user.password = "secret".to_string();
        user.proxy = ProxyType::Reverse(None);
        user.output_clusters = ClusterFlags::all();
        app_config.api_proxy.store(Some(Arc::new(ApiProxyConfig {
            server: vec![ApiProxyServerInfo {
                name: "default".to_string(),
                protocol: "http".to_string(),
                host: "tuliprox.local".to_string(),
                port: Some("8901".to_string()),
                timezone: "UTC".to_string(),
                message: String::new(),
                path: None,
            }],
            user: vec![TargetUser { target: target.name.clone(), credentials: vec![Arc::new(user)] }],
            ..Default::default()
        })));

        let groups =
            vec![PlaylistGroup { id: 1, title: "News".intern(), channels: items, xtream_cluster: XtreamCluster::Live }];
        let target_path = ensure_target_storage_path(&app_config.config.load(), &target.name).await.unwrap();
        m3u_write_playlist(app_config, &target, target.get_m3u_output().unwrap(), &target_path, &groups, false)
            .await
            .unwrap();
        (temp, state)
    }

    async fn location(state: &Arc<HdHomerunAppState>, uri: &str) -> Result<String, StatusCode> {
        let resp = hdhr_api_register(false).with_state(Arc::clone(state)).oneshot(make_request(uri)).await.unwrap();
        if resp.status() != StatusCode::TEMPORARY_REDIRECT {
            return Err(resp.status());
        }
        Ok(resp.headers()[header::LOCATION].to_str().unwrap().to_string())
    }

    #[test]
    fn channel_query_strips_numeric_virtual_channel_prefix_only() {
        let q = |channel| ChannelQuery::parse(channel).map(|q| (q.raw, q.numeric, q.virtual_id));
        assert_eq!(q("v101"), Some(("v101", Some("101"), Some(101))));
        assert_eq!(q("V101.ts"), Some(("V101", Some("101"), Some(101))));
        assert_eq!(q("101.TS"), Some(("101", None, Some(101))));
        assert_eq!(q("  v2.1  "), Some(("v2.1", Some("2.1"), None)));
        assert_eq!(q("vox.de"), Some(("vox.de", None, None)));
        assert_eq!(q("VIVA"), Some(("VIVA", None, None)));
        assert_eq!(q("v"), Some(("v", None, None)));
        assert_eq!(q(".ts"), None);
        assert_eq!(q("  "), None);
        assert_eq!(q("ä.ts"), Some(("ä", None, None)));
    }

    #[tokio::test]
    async fn find_channel_prefers_guide_number_over_virtual_id() {
        let items: Vec<M3uPlaylistItem> = [m3u_item(101, "Other", None), m3u_item(7, "Numeric", Some("101"))]
            .iter()
            .map(M3uPlaylistItem::from)
            .collect();
        let find = |channel: &'static str| {
            let items = items.clone();
            async move {
                let query = ChannelQuery::parse(channel).unwrap();
                let channels = stream::iter(items.into_iter().map(|item| Ok((item, false))));
                find_channel(channels, &query).await.map(|(rank, item)| (rank, item.virtual_id.get()))
            }
        };
        assert_eq!(find("101").await, Some((0, 7)));
        assert_eq!(find("v101").await, Some((1, 7)));
        assert_eq!(find("Other").await, Some((0, 101)));
        assert_eq!(find("v7").await, Some((2, 7)));
        assert_eq!(find("v999").await, None);
    }

    #[tokio::test]
    async fn auto_channel_redirects_to_proxied_stream_url() {
        let (_temp, state) = m3u_device(vec![
            m3u_item(1, "VOX", Some("vox.de")),
            m3u_item(2, "Das Erste", Some("101")),
            m3u_item(101, "Other", None),
        ])
        .await;

        // virtual id, guide number starting with `v`, explicit tuner with `.ts` suffix
        for uri in ["/auto/v1", "/auto/vox.de", "/tuner1/vox.de.ts"] {
            assert_eq!(
                location(&state, uri).await.as_deref(),
                Ok("http://tuliprox.local:8901/m3u-stream/hdhr/secret/1.ts"),
                "{uri}"
            );
        }
        // numeric guide number wins over the virtual id 101 lookup
        assert_eq!(
            location(&state, "/auto/v101").await.as_deref(),
            Ok("http://tuliprox.local:8901/m3u-stream/hdhr/secret/2.ts")
        );
    }

    #[tokio::test]
    async fn tune_rejects_unknown_channel_and_tuner() {
        let (_temp, state) = m3u_device(vec![m3u_item(1, "VOX", Some("vox.de"))]).await;
        assert_eq!(location(&state, "/auto/v404").await, Err(StatusCode::NOT_FOUND));
        assert_eq!(location(&state, "/auto/ox.de").await, Err(StatusCode::NOT_FOUND));
        assert_eq!(location(&state, "/tuner2/vox.de").await, Err(StatusCode::NOT_FOUND));
        assert_eq!(location(&state, "/tunerX/vox.de").await, Err(StatusCode::BAD_REQUEST));
    }

    #[tokio::test]
    async fn auto_channel_unknown_user_returns_not_found() {
        let state = test_hdhr_app_state("unknown_user", 2);
        assert_eq!(location(&state, "/auto/v1").await, Err(StatusCode::NOT_FOUND));
        assert_eq!(location(&state, "/tuner0/v1").await, Err(StatusCode::NOT_FOUND));
    }

    #[tokio::test]
    async fn auth_channel_routes_require_basic_auth() {
        let state = test_hdhr_app_state("test_user", 2);
        for uri in ["/auto/v1", "/tuner0/v1"] {
            let resp = hdhr_api_register(true).with_state(Arc::clone(&state)).oneshot(make_request(uri)).await.unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{uri}");
        }
    }
}
