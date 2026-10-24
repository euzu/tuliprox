use crate::{
    api::{
        api_utils::{coalesce_byte_stream, internal_server_error, try_unwrap_body},
        model::{AppState, HdHomerunAppState},
    },
    auth::{AuthBasic, Fingerprint},
    model::{AppConfig, ConfigInputFlags, ConfigTarget, ProxyUserCredentials},
    processing::parser::xtream::get_xtream_url,
    repository::{
        get_target_storage_path, hdhomerun_lineup_view, iter_raw_m3u_target_playlist, lineup_guide_candidates,
        lineup_guide_index_view, m3u_lineup_guide_index_path, xtream_lineup_guide_index_path, LineupGuideCandidate,
        LineupGuideLookup, LineupGuideView, M3uPlaylistIterator, M3uUserItemLookup, XtreamPlaylistIterator,
        XtreamUserItemLookup,
    },
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
use std::{path::PathBuf, sync::Arc};

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
        // A missing cluster contributes nothing. A placeholder chunk would still be visible to the
        // caller that peeks at both clusters to decide whether a separator is needed, which ends the
        // lineup on a trailing comma.
        None => stream::empty().right_stream(),
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
        None => stream::empty().right_stream(),
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
            app_state.app_state.try_check_network_access(&credentials, &fingerprint).is_err()
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
        app_state.app_state.try_check_network_access(&credentials, &fingerprint).is_ok()
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
    let view = hdhomerun_lineup_view(target);
    if view == Some(LineupGuideView::M3u) {
        let iterator = M3uPlaylistIterator::new(cfg, target, credentials).await.ok();
        let stream = m3u_item_to_lineup_stream(iterator);
        let body_stream = stream::once(async { Ok(Bytes::from("[")) })
            .chain(stream)
            .chain(stream::once(async { Ok(Bytes::from("]")) }));
        return try_unwrap_body!(axum::response::Response::builder()
            .status(axum::http::StatusCode::OK)
            .header(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
            .body(axum::body::Body::from_stream(coalesce_byte_stream(body_stream))));
    } else if view == Some(LineupGuideView::Xtream) {
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
    app_state
        .app_state
        .try_check_network_access(&credentials, fingerprint)
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
        let raw = match trimmed.rsplit_once('.') {
            Some((stem, ext)) if ext.eq_ignore_ascii_case("ts") => stem.trim(),
            _ => trimmed,
        };
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

/// Keys the guide index is asked for, in the order the lineup scan ranks them: the exact guide
/// number, then the guide number without a leading `v`.
///
/// Both are the strings `lineup.json` publishes. They are plain request data, so they are not
/// interned into the shared pool.
fn guide_lookup_keys(query: &ChannelQuery<'_>) -> Vec<Arc<str>> {
    query.numeric.map_or_else(|| vec![Arc::from(query.raw)], |numeric| vec![Arc::from(query.raw), Arc::from(numeric)])
}

/// Guide index file of `view`, when the target publishes an `HDHomeRun` lineup from it.
///
/// Read path only: the target directory is not created here.
fn guide_index_path(app_state: &AppState, target: &ConfigTarget, view: LineupGuideView) -> Option<PathBuf> {
    if lineup_guide_index_view(target) != Some(view) {
        return None;
    }
    let config = app_state.app_config.config.load();
    match view {
        LineupGuideView::M3u => get_target_storage_path(&config, target.name.as_str())
            .map(|target_path| m3u_lineup_guide_index_path(&target_path)),
        LineupGuideView::Xtream => xtream_lineup_guide_index_path(&config, &target.name),
    }
}

/// Candidates the guide index of `view` holds for `query`, or `None` without a usable index.
async fn guide_index_candidates(
    app_state: &AppState,
    target: &ConfigTarget,
    view: LineupGuideView,
    query: &ChannelQuery<'_>,
) -> Option<Vec<LineupGuideCandidate>> {
    let index_path = guide_index_path(app_state, target, view)?;
    match lineup_guide_candidates(&app_state.app_config.file_locks, index_path, guide_lookup_keys(query)).await {
        LineupGuideLookup::Candidates(candidates) => Some(candidates),
        LineupGuideLookup::NoEntry => Some(Vec::new()),
        LineupGuideLookup::Missing => None,
    }
}

/// Clusters the lineup of the Xtream view publishes, in the order the scan searches them.
const LINEUP_CLUSTERS: [XtreamCluster; 2] = [XtreamCluster::Live, XtreamCluster::Video];

/// Answer of a target guide index.
enum GuideIndexAnswer {
    /// The index named a channel this user's output publishes.
    Url(Arc<str>),
    /// The index is usable and nothing this request can address is published to this user, so the
    /// target does not serve it. Answering that without scanning is what keeps an unknown channel
    /// cheap: the index follows every rewrite of the playlist db it describes, and the writers
    /// outside that step only touch stream metadata and episodes, never a guide number.
    NotServed,
    /// No usable index. The caller falls back to its playlist scan, which stays the reference.
    Unavailable,
}

/// Answers a tune request from the guide index of the M3U view.
///
/// Walks the candidates in the order the scan ranks them: every feed of the exact guide number,
/// then of the guide number without `v`, both in lineup order, and finally the virtual id itself.
/// The first one this user's output publishes wins. Channel failover will pick among the same
/// candidates by quality of service instead.
async fn resolve_m3u_channel_via_guide_index(
    app_state: &AppState,
    credentials: &Arc<ProxyUserCredentials>,
    target: &ConfigTarget,
    query: &ChannelQuery<'_>,
) -> GuideIndexAnswer {
    let Some(candidates) = guide_index_candidates(app_state, target, LineupGuideView::M3u, query).await else {
        return GuideIndexAnswer::Unavailable;
    };
    // Nothing to look up: answer before preparing the per-user pipeline.
    if candidates.is_empty() && query.virtual_id.is_none() {
        return GuideIndexAnswer::NotServed;
    }
    let lookup = match M3uUserItemLookup::new(&app_state.app_config, &app_state.playlists, target, credentials).await {
        Ok(lookup) => lookup,
        Err(err) => {
            warn!("Failed to prepare m3u output of target {} for HDHomeRun: {err}", target.name);
            return GuideIndexAnswer::Unavailable;
        }
    };

    for LineupGuideCandidate { key, entries } in candidates {
        for entry in entries {
            // A feed that is gone or hidden for this user does not end the walk: the next feed of
            // the same guide number may be the one the lineup publishes. An item that no longer
            // carries the guide number it was found under means the index is behind.
            if let Some(item) = lookup.get(entry.virtual_id).await {
                if item.guide_number() == key.as_ref() {
                    return GuideIndexAnswer::Url(get_m3u_item_stream_url(&item));
                }
            }
        }
    }
    if let Some(stream_id) = query.virtual_id {
        if let Some(item) = lookup.get(stream_id).await {
            return GuideIndexAnswer::Url(get_m3u_item_stream_url(&item));
        }
    }
    GuideIndexAnswer::NotServed
}

/// Answers a tune request from the guide index of the Xtream view.
///
/// Same walk as the M3U counterpart. The lineup publishes live and video, so an entry of a cluster
/// this user may not watch is skipped while the remaining feeds are still tried, and the virtual id
/// is looked up in the live db before the video db, like the scan does.
async fn resolve_xtream_channel_via_guide_index(
    app_state: &AppState,
    credentials: &Arc<ProxyUserCredentials>,
    target: &ConfigTarget,
    query: &ChannelQuery<'_>,
) -> GuideIndexAnswer {
    let cfg = &app_state.app_config;
    let Some(server_info) = cfg.get_user_server_info(credentials) else {
        return GuideIndexAnswer::Unavailable;
    };
    let Some(candidates) = guide_index_candidates(app_state, target, LineupGuideView::Xtream, query).await else {
        return GuideIndexAnswer::Unavailable;
    };
    if candidates.is_empty() && query.virtual_id.is_none() {
        return GuideIndexAnswer::NotServed;
    }
    let lookup = XtreamUserItemLookup::new(cfg, &app_state.playlists, target, credentials, &LINEUP_CLUSTERS).await;
    let (base_url_live, base_url_vod) = xtream_base_urls(credentials, target, server_info.get_base_url());
    let stream_url = |cluster: XtreamCluster, item: &XtreamPlaylistItem| {
        let base_url = if cluster == XtreamCluster::Live { &base_url_live } else { &base_url_vod };
        GuideIndexAnswer::Url(get_xtream_item_stream_url(cfg, cluster, credentials, base_url.as_deref(), item))
    };

    for LineupGuideCandidate { key, entries } in candidates {
        for entry in entries {
            if let Some(item) = lookup.get(entry.virtual_id, entry.cluster).await {
                if item.guide_number() == key.as_ref() {
                    return stream_url(entry.cluster, &item);
                }
            }
        }
    }
    if let Some(stream_id) = query.virtual_id {
        for cluster in LINEUP_CLUSTERS {
            if let Some(item) = lookup.get(stream_id, cluster).await {
                return stream_url(cluster, &item);
            }
        }
    }
    GuideIndexAnswer::NotServed
}

/// Rewritten stream url of the channel a tune request addresses.
///
/// The target guide index answers first, so a request costs a few keyed lookups instead of a full
/// playlist read, and a request nothing published matches ends as "not served" right there. The
/// scan only runs when the target has no usable index.
async fn resolve_channel_stream_url(
    app_state: &AppState,
    credentials: &Arc<ProxyUserCredentials>,
    target: &ConfigTarget,
    query: &ChannelQuery<'_>,
) -> Option<Arc<str>> {
    let cfg = &app_state.app_config;
    match hdhomerun_lineup_view(target)? {
        LineupGuideView::M3u => {
            match resolve_m3u_channel_via_guide_index(app_state, credentials, target, query).await {
                GuideIndexAnswer::Url(url) => return Some(url),
                GuideIndexAnswer::NotServed => return None,
                GuideIndexAnswer::Unavailable => {}
            }
            let channels = M3uPlaylistIterator::new(cfg, target, credentials).await.ok()?;
            find_channel(channels, query).await.map(|(_, item)| get_m3u_item_stream_url(&item))
        }
        LineupGuideView::Xtream => {
            match resolve_xtream_channel_via_guide_index(app_state, credentials, target, query).await {
                GuideIndexAnswer::Url(url) => return Some(url),
                GuideIndexAnswer::NotServed => return None,
                GuideIndexAnswer::Unavailable => {}
            }
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
            best.map(|(_, url)| url)
        }
    }
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

    let Some(stream_url) = resolve_channel_stream_url(&app_state.app_state, &credentials, &target, &query).await else {
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
        repository::{
            ensure_target_storage_path, lineup_guide_index_write, m3u_lineup_guide_index_path, m3u_write_playlist,
            persist_playlist, save_user_bouquet, LineupGuideView, TargetPlaylistPersistOptions,
        },
    };
    use axum::{
        body::Body,
        http::{header, Request, StatusCode},
    };
    use shared::{
        model::{
            ClusterFlags, ConfigTargetDto, HdHomeRunDeviceConfigDto, HdHomeRunTargetOutputDto, M3uTargetOutputDto,
            PlaylistBouquetDto, PlaylistClusterBouquetDto, PlaylistGroup, PlaylistItem, PlaylistItemHeader, ProxyType,
            TargetOutputDto, UUIDType, VirtualId, XtreamTargetOutputDto,
        },
        utils::Internable,
    };
    use std::sync::atomic::AtomicI8;
    use tower::ServiceExt;

    const PROVIDER_HOST: &str = "http://provider.example";

    /// Target playlist view a device publishes its lineup from.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum LineupSource {
        M3u,
        Xtream,
    }

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
        m3u_item_in_group(virtual_id, name, epg_channel_id, "News")
    }

    fn m3u_item_in_group(virtual_id: u32, name: &str, epg_channel_id: Option<&str>, group: &str) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                id: virtual_id.to_string().intern(),
                // Persistence derives the virtual id of an item from its uuid, so the fixtures need
                // one per channel to keep them apart.
                uuid: UUIDType::from_valid_uuid(&format!("hdhr-test-{virtual_id}")),
                virtual_id: VirtualId::new(virtual_id),
                name: name.intern(),
                title: name.intern(),
                group: group.intern(),
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

    /// Device whose target is persisted on disk and served to a reverse proxy user.
    ///
    /// With a `lineup` source the target also publishes an `HDHomeRun` output and the playlist goes
    /// through the persistence entry point that writes the guide index of the lineup view. Without
    /// it the M3U playlist is written directly, the way a target that never publishes a lineup is
    /// persisted and leaves no index behind.
    async fn hdhomerun_device(
        items: Vec<PlaylistItem>,
        lineup: Option<LineupSource>,
    ) -> (tempfile::TempDir, Arc<HdHomerunAppState>, Arc<ConfigTarget>) {
        let temp = tempfile::tempdir().unwrap();
        let state = test_hdhr_app_state("hdhr", 2);
        let app_config = &state.app_state.app_config;

        let mut config = (*app_config.config.load_full()).clone();
        config.storage_dir = temp.path().to_string_lossy().into_owned();
        config.user_config_dir = Some(temp.path().to_string_lossy().into_owned());
        app_config.config.store(Arc::new(config));

        let mut output = vec![match lineup {
            Some(LineupSource::Xtream) => TargetOutputDto::Xtream(XtreamTargetOutputDto::default()),
            _ => TargetOutputDto::M3u(M3uTargetOutputDto::default()),
        }];
        if let Some(source) = lineup {
            let use_output = match source {
                LineupSource::M3u => TargetType::M3u,
                LineupSource::Xtream => TargetType::Xtream,
            };
            output.push(TargetOutputDto::HdHomeRun(HdHomeRunTargetOutputDto {
                use_output: Some(use_output),
                ..Default::default()
            }));
        }
        let target =
            ConfigTarget::from(&ConfigTargetDto { id: 1, name: "default".to_string(), output, ..Default::default() });
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

        let mut groups =
            vec![PlaylistGroup { id: 1, title: "News".intern(), channels: items, xtream_cluster: XtreamCluster::Live }];
        if lineup.is_some() {
            persist_playlist(app_config, &mut groups, None, &target, None, TargetPlaylistPersistOptions::default())
                .await
                .expect("target playlist persisted");
        } else {
            let target_path = ensure_target_storage_path(&app_config.config.load(), &target.name).await.unwrap();
            m3u_write_playlist(app_config, &target, target.get_m3u_output().unwrap(), &target_path, &groups, false)
                .await
                .unwrap();
        }
        (temp, state, Arc::new(target))
    }

    /// Device with a target that publishes no `HDHomeRun` output.
    async fn m3u_device(items: Vec<PlaylistItem>) -> (tempfile::TempDir, Arc<HdHomerunAppState>) {
        let (temp, state, _target) = hdhomerun_device(items, None).await;
        (temp, state)
    }

    /// Guide index file of the device target.
    async fn guide_index_path(state: &Arc<HdHomerunAppState>, target: &ConfigTarget) -> std::path::PathBuf {
        let config = state.app_state.app_config.config.load();
        let target_path = ensure_target_storage_path(&config, target.name.as_str()).await.unwrap();
        m3u_lineup_guide_index_path(&target_path)
    }

    /// Guide index file of the device target of the Xtream view.
    fn xtream_guide_index_path(state: &Arc<HdHomerunAppState>, target: &ConfigTarget) -> std::path::PathBuf {
        xtream_lineup_guide_index_path(&state.app_state.app_config.config.load(), &target.name)
            .expect("xtream storage path")
    }

    /// The lineup the device publishes, parsed the way a client reads it.
    async fn lineup_entries(state: &Arc<HdHomerunAppState>) -> Vec<Lineup> {
        let response =
            hdhr_api_register(false).with_state(Arc::clone(state)).oneshot(make_request("/lineup.json")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&body).expect("lineup parses")
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

    #[tokio::test]
    async fn guide_index_follows_the_lineup_output() {
        let (_temp, state, target) =
            hdhomerun_device(vec![m3u_item(1, "VOX", Some("vox.de"))], Some(LineupSource::M3u)).await;
        let index_path = guide_index_path(&state, &target).await;
        assert!(index_path.exists(), "a target with an HDHomeRun output gets a guide index");
        assert_eq!(
            location(&state, "/auto/vox.de").await.as_deref(),
            Ok("http://tuliprox.local:8901/m3u-stream/hdhr/secret/1.ts")
        );

        // The same playlist persisted without the lineup output leaves no index behind.
        let without_lineup = ConfigTarget::from(&ConfigTargetDto {
            id: 1,
            name: "default".to_string(),
            output: vec![TargetOutputDto::M3u(M3uTargetOutputDto::default())],
            ..Default::default()
        });
        let mut groups = vec![PlaylistGroup {
            id: 1,
            title: "News".intern(),
            channels: vec![m3u_item(1, "VOX", Some("vox.de"))],
            xtream_cluster: XtreamCluster::Live,
        }];
        persist_playlist(
            &state.app_state.app_config,
            &mut groups,
            None,
            &without_lineup,
            None,
            TargetPlaylistPersistOptions::default(),
        )
        .await
        .expect("playlist persisted");

        assert!(!index_path.exists(), "no index for a target without an HDHomeRun output");
    }

    #[tokio::test]
    async fn guide_index_of_a_view_that_is_gone_is_cleaned_up() {
        // The lineup output stays, only the view behind it changes from Xtream to M3U. The index of
        // the vanished view would describe a playlist this target no longer serves.
        let (_temp, state, target) =
            hdhomerun_device(vec![m3u_item(1, "VOX", Some("vox.de"))], Some(LineupSource::Xtream)).await;
        let xtream_index = xtream_guide_index_path(&state, &target);
        assert!(xtream_index.exists(), "the Xtream view gets its index");

        let m3u_target = ConfigTarget::from(&ConfigTargetDto {
            id: 1,
            name: "default".to_string(),
            output: vec![
                TargetOutputDto::M3u(M3uTargetOutputDto::default()),
                TargetOutputDto::HdHomeRun(HdHomeRunTargetOutputDto {
                    use_output: Some(TargetType::M3u),
                    ..Default::default()
                }),
            ],
            ..Default::default()
        });
        let mut groups = vec![PlaylistGroup {
            id: 1,
            title: "News".intern(),
            channels: vec![m3u_item(1, "VOX", Some("vox.de"))],
            xtream_cluster: XtreamCluster::Live,
        }];
        persist_playlist(
            &state.app_state.app_config,
            &mut groups,
            None,
            &m3u_target,
            None,
            TargetPlaylistPersistOptions::default(),
        )
        .await
        .expect("playlist persisted");

        assert!(!xtream_index.exists(), "the index of the vanished view is removed");
        assert!(guide_index_path(&state, &m3u_target).await.exists(), "the M3U view gets its index");
    }

    #[tokio::test]
    async fn channel_resolution_uses_the_guide_index_before_the_scan() {
        // Two channels carry the same guide number. The index of the persisted playlist points at
        // the first one, so replacing it with an index of the second one answers only if the
        // request is served from the index: the scan would still return the first channel.
        let (_temp, state, target) = hdhomerun_device(
            vec![m3u_item(1, "VOX", Some("vox.de")), m3u_item(2, "VOX HD", Some("vox.de"))],
            Some(LineupSource::M3u),
        )
        .await;
        let index_path = guide_index_path(&state, &target).await;
        assert_eq!(
            location(&state, "/auto/vox.de").await.as_deref(),
            Ok("http://tuliprox.local:8901/m3u-stream/hdhr/secret/1.ts")
        );

        let second_channel = vec![PlaylistGroup {
            id: 1,
            title: "News".intern(),
            channels: vec![m3u_item(2, "VOX HD", Some("vox.de"))],
            xtream_cluster: XtreamCluster::Live,
        }];
        lineup_guide_index_write(
            &state.app_state.app_config.file_locks,
            &second_channel,
            LineupGuideView::M3u,
            &index_path,
            None,
        )
        .await
        .expect("index rewritten");

        assert_eq!(
            location(&state, "/auto/vox.de").await.as_deref(),
            Ok("http://tuliprox.local:8901/m3u-stream/hdhr/secret/2.ts")
        );
    }

    #[tokio::test]
    async fn unknown_guide_number_is_answered_without_scanning_the_playlist() {
        // The index is authoritative for the guide numbers it was written with: what it does not
        // know is not served. Replacing it with an index of another playlist makes that observable,
        // because the playlist still holds a channel carrying the requested guide number, which the
        // scan would have found.
        let (_temp, state, target) =
            hdhomerun_device(vec![m3u_item(1, "VOX", Some("vox.de"))], Some(LineupSource::M3u)).await;
        let elsewhere = vec![PlaylistGroup {
            id: 1,
            title: "News".intern(),
            channels: vec![m3u_item(9, "Other", Some("other.de"))],
            xtream_cluster: XtreamCluster::Live,
        }];
        lineup_guide_index_write(
            &state.app_state.app_config.file_locks,
            &elsewhere,
            LineupGuideView::M3u,
            &guide_index_path(&state, &target).await,
            None,
        )
        .await
        .expect("index rewritten");

        assert_eq!(location(&state, "/auto/vox.de").await, Err(StatusCode::NOT_FOUND));
        // A virtual id is still resolved through the index, which knows no guide number for it.
        assert_eq!(
            location(&state, "/auto/v1").await.as_deref(),
            Ok("http://tuliprox.local:8901/m3u-stream/hdhr/secret/1.ts")
        );
    }

    #[tokio::test]
    async fn channel_resolution_falls_back_to_the_scan_without_an_index() {
        let (_temp, state, target) = hdhomerun_device(
            vec![m3u_item(1, "VOX", Some("vox.de")), m3u_item(2, "VOX HD", Some("vox.de"))],
            Some(LineupSource::M3u),
        )
        .await;
        std::fs::remove_file(guide_index_path(&state, &target).await).expect("index removed");

        // Guide number, its numeric form and the bare virtual id all still resolve.
        for uri in ["/auto/vox.de", "/auto/v2", "/tuner1/vox.de.ts"] {
            let expected = if uri == "/auto/v2" {
                "http://tuliprox.local:8901/m3u-stream/hdhr/secret/2.ts"
            } else {
                "http://tuliprox.local:8901/m3u-stream/hdhr/secret/1.ts"
            };
            assert_eq!(location(&state, uri).await.as_deref(), Ok(expected), "{uri}");
        }
    }

    #[tokio::test]
    async fn index_serves_the_next_feed_when_the_first_is_outside_the_bouquet() {
        // Both feeds of the guide number are in the index, in lineup order. The first one is outside
        // this user's bouquet, so the request must not leak it and has to end up on the second one.
        let (_temp, state, _target) = hdhomerun_device(
            vec![
                m3u_item_in_group(1, "Hidden HD", Some("vox.de"), "Hidden"),
                m3u_item_in_group(2, "VOX", Some("vox.de"), "News"),
            ],
            Some(LineupSource::M3u),
        )
        .await;
        let bouquet = PlaylistBouquetDto {
            xtream: None,
            m3u: Some(PlaylistClusterBouquetDto { live: Some(vec!["News".to_string()]), ..Default::default() }),
        };
        save_user_bouquet(&state.app_state.app_config, "default", "hdhr", &bouquet).await.expect("bouquet saved");

        assert_eq!(
            location(&state, "/auto/vox.de").await.as_deref(),
            Ok("http://tuliprox.local:8901/m3u-stream/hdhr/secret/2.ts")
        );
    }

    #[tokio::test]
    async fn guide_number_with_no_visible_feed_is_answered_without_scanning_the_playlist() {
        // Only the feed the bouquet excludes is in the index, so every candidate is hidden for this
        // user. The request ends as 404 instead of walking the playlist, which still holds a second
        // feed of that guide number and would have answered it.
        let (_temp, state, target) = hdhomerun_device(
            vec![
                m3u_item_in_group(1, "Hidden HD", Some("vox.de"), "Hidden"),
                m3u_item_in_group(2, "VOX", Some("vox.de"), "News"),
            ],
            Some(LineupSource::M3u),
        )
        .await;
        let hidden_only = vec![PlaylistGroup {
            id: 1,
            title: "News".intern(),
            channels: vec![m3u_item_in_group(1, "Hidden HD", Some("vox.de"), "Hidden")],
            xtream_cluster: XtreamCluster::Live,
        }];
        lineup_guide_index_write(
            &state.app_state.app_config.file_locks,
            &hidden_only,
            LineupGuideView::M3u,
            &guide_index_path(&state, &target).await,
            None,
        )
        .await
        .expect("index rewritten");
        let bouquet = PlaylistBouquetDto {
            xtream: None,
            m3u: Some(PlaylistClusterBouquetDto { live: Some(vec!["News".to_string()]), ..Default::default() }),
        };
        save_user_bouquet(&state.app_state.app_config, "default", "hdhr", &bouquet).await.expect("bouquet saved");

        assert_eq!(location(&state, "/auto/vox.de").await, Err(StatusCode::NOT_FOUND));
    }

    #[tokio::test]
    async fn xtream_tuner_redirects_to_the_published_lineup_url() {
        let (_temp, state, target) = hdhomerun_device(
            vec![m3u_item(1, "VOX", Some("vox.de")), m3u_item(2, "Das Erste", Some("101"))],
            Some(LineupSource::Xtream),
        )
        .await;
        assert!(xtream_guide_index_path(&state, &target).exists(), "the Xtream view gets its own index");

        let published = lineup_entries(&state).await;
        let expected = published
            .iter()
            .find(|entry| entry.guide_number.as_ref() == "vox.de")
            .expect("the lineup publishes the channel");

        // The tuner has to redirect to exactly the url the lineup published, for the guide number
        // as well as for the virtual id a client may build itself.
        for uri in ["/auto/vox.de", "/auto/v1"] {
            assert_eq!(location(&state, uri).await.as_deref(), Ok(expected.url.as_ref()), "{uri}");
        }
    }

    fn vod_item(virtual_id: u32, name: &str, epg_channel_id: Option<&str>) -> PlaylistItem {
        let mut item = m3u_item(virtual_id, name, epg_channel_id);
        item.header.item_type = PlaylistItemType::Video;
        item.header.xtream_cluster = XtreamCluster::Video;
        item
    }

    fn live_and_vod_groups(live: Vec<PlaylistItem>, vod: Vec<PlaylistItem>) -> Vec<PlaylistGroup> {
        let mut groups =
            vec![PlaylistGroup { id: 1, title: "News".intern(), channels: live, xtream_cluster: XtreamCluster::Live }];
        if !vod.is_empty() {
            groups.push(PlaylistGroup {
                id: 2,
                title: "Movies".intern(),
                channels: vod,
                xtream_cluster: XtreamCluster::Video,
            });
        }
        groups
    }

    #[tokio::test]
    async fn update_that_keeps_a_cluster_db_carries_its_entries_into_the_index() {
        // A provider answering without movies leaves the published video db in place. The index
        // rebuilt from that update learns the live channels from it and carries the movies over
        // from the previous index, which was written together with the kept video db.
        let (_temp, state, target) =
            hdhomerun_device(vec![m3u_item(1, "VOX", Some("vox.de"))], Some(LineupSource::Xtream)).await;
        let app_config = &state.app_state.app_config;
        let index_path = xtream_guide_index_path(&state, &target);
        let persist = |groups: Vec<PlaylistGroup>| {
            let target = Arc::clone(&target);
            async move {
                let mut groups = groups;
                persist_playlist(app_config, &mut groups, None, &target, None, TargetPlaylistPersistOptions::default())
                    .await
                    .expect("playlist persisted");
            }
        };
        let indexed_clusters = |guide: &'static str| {
            let index_path = index_path.clone();
            async move {
                match lineup_guide_candidates(&app_config.file_locks, index_path, vec![guide.intern()]).await {
                    LineupGuideLookup::Candidates(candidates) => candidates
                        .into_iter()
                        .flat_map(|candidate| candidate.entries)
                        .map(|entry| entry.cluster)
                        .collect::<Vec<_>>(),
                    LineupGuideLookup::NoEntry | LineupGuideLookup::Missing => Vec::new(),
                }
            }
        };
        let live_only = || {
            live_and_vod_groups(
                vec![m3u_item(1, "VOX", Some("vox.de")), m3u_item(3, "Sport", Some("sport.de"))],
                Vec::new(),
            )
        };

        persist(live_and_vod_groups(
            vec![m3u_item(1, "VOX", Some("vox.de"))],
            vec![vod_item(2, "Film", Some("film.de"))],
        ))
        .await;
        let film_url = lineup_entries(&state)
            .await
            .into_iter()
            .find(|entry| entry.guide_number.as_ref() == "film.de")
            .expect("the lineup publishes the movie")
            .url;

        persist(live_only()).await;
        assert_eq!(indexed_clusters("film.de").await, vec![XtreamCluster::Video], "the movie is carried over");
        assert_eq!(indexed_clusters("sport.de").await, vec![XtreamCluster::Live], "the new live channel is indexed");
        assert_eq!(location(&state, "/auto/film.de").await.as_deref(), Ok(film_url.as_ref()));
        assert!(location(&state, "/auto/sport.de").await.is_ok());

        // Without a previous index nothing describes the kept video db, so no index is written and
        // the scan answers instead.
        std::fs::remove_file(&index_path).expect("index removed");
        persist(live_only()).await;
        assert!(!index_path.exists(), "an index that cannot describe the kept video db is not written");
        assert_eq!(location(&state, "/auto/film.de").await.as_deref(), Ok(film_url.as_ref()));
    }

    #[tokio::test]
    async fn virtual_id_is_tried_when_every_guide_number_feed_is_hidden() {
        // `v2` names the guide number `2` of a feed outside the bouquet and the virtual id of a
        // published channel. The scan ranks the virtual id after the guide number, so the index
        // has to fall through to it instead of answering 404.
        let (_temp, state, _target) = hdhomerun_device(
            vec![
                m3u_item_in_group(1, "Hidden", Some("2"), "Hidden"),
                m3u_item_in_group(2, "VOX", Some("vox.de"), "News"),
            ],
            Some(LineupSource::M3u),
        )
        .await;
        let bouquet = PlaylistBouquetDto {
            xtream: None,
            m3u: Some(PlaylistClusterBouquetDto { live: Some(vec!["News".to_string()]), ..Default::default() }),
        };
        save_user_bouquet(&state.app_state.app_config, "default", "hdhr", &bouquet).await.expect("bouquet saved");

        assert_eq!(
            location(&state, "/auto/v2").await.as_deref(),
            Ok("http://tuliprox.local:8901/m3u-stream/hdhr/secret/2.ts")
        );
    }

    #[tokio::test]
    async fn guide_index_failure_does_not_fail_the_playlist_publication() {
        // The guide index is a sidecar. A directory where the index file belongs makes removing and
        // writing it fail; the playlist must still be published and the tuners scan it instead.
        let (_temp, state, target) =
            hdhomerun_device(vec![m3u_item(1, "VOX", Some("vox.de"))], Some(LineupSource::M3u)).await;
        let index_path = guide_index_path(&state, &target).await;
        std::fs::remove_file(&index_path).expect("index removed");
        std::fs::create_dir(&index_path).expect("blocking directory created");

        let mut groups = vec![PlaylistGroup {
            id: 1,
            title: "News".intern(),
            channels: vec![m3u_item(1, "VOX", Some("vox.de")), m3u_item(2, "Sport", Some("sport.de"))],
            xtream_cluster: XtreamCluster::Live,
        }];
        persist_playlist(
            &state.app_state.app_config,
            &mut groups,
            None,
            &target,
            None,
            TargetPlaylistPersistOptions::default(),
        )
        .await
        .expect("playlist published despite the index failure");

        assert!(index_path.is_dir(), "the index could not be written");
        assert_eq!(
            location(&state, "/auto/sport.de").await.as_deref(),
            Ok("http://tuliprox.local:8901/m3u-stream/hdhr/secret/2.ts")
        );
    }

    #[tokio::test]
    async fn percent_encoded_guide_number_is_decoded_before_matching() {
        // A channel without EPG id publishes its name as guide number, spaces and all. Clients send
        // it percent-encoded; the index and the scan both have to see the decoded name.
        let items = || vec![m3u_item(1, "Das Erste HD", None), m3u_item(2, "AC/DC TV", None)];
        let (_temp, indexed, _target) = hdhomerun_device(items(), Some(LineupSource::M3u)).await;
        let (_temp_scan, scanned) = m3u_device(items()).await;

        for state in [&indexed, &scanned] {
            assert_eq!(
                location(state, "/auto/Das%20Erste%20HD").await.as_deref(),
                Ok("http://tuliprox.local:8901/m3u-stream/hdhr/secret/1.ts")
            );
            assert_eq!(
                location(state, "/auto/AC%2FDC%20TV").await.as_deref(),
                Ok("http://tuliprox.local:8901/m3u-stream/hdhr/secret/2.ts")
            );
        }
    }
}
