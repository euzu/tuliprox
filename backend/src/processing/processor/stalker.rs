//! Stalker/Ministra playlist processor.
//!
//! Orchestrates the full Stalker download: auth + catalog fetch (live, VOD, series) +
//! pre-resolve of `create_link` (when enabled) + persistence into the B+Tree store. The
//! processor is the single entry point the playlist dispatcher uses for `InputType::Stalker`
//! — mirror of `xtream::download_xtream_playlist`.
//!
//! Reverse-proxy re-resolve (when a 4xx upstream error is observed) is implemented in the
//! HLS/Xtream endpoints and reaches back into the API client via the helper
//! [`crate::utils::network::stalker::client::StalkerApiClient::create_link`].

#![allow(clippy::too_many_lines, clippy::needless_pass_by_value)]

use futures::stream::{self, StreamExt};
use log::{debug, info, warn};
use parking_lot::Mutex;
use shared::error::TuliproxError;
use shared::model::stalker::StalkerStreamKind;
use shared::model::stalker_item::StalkerPlaylistItem;
use shared::model::{PlaylistGroup, PlaylistItem, PlaylistItemType};
use shared::utils::{generate_provider_playlist_uuid, Internable};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Weak};

use crate::model::{AppConfig, ConfigInput, ConfigInputFlags, StalkerInputConfig};
use crate::processing::parser::stalker as parser;
use crate::repository::stalker_repository::{
    ensure_stalker_storage_path, persist_stalker_item, persist_stalker_items, read_stalker_item,
};
use crate::utils::network::stalker::catalog::StalkerCategory;
use crate::utils::network::stalker::client::StalkerApiClient;
use crate::utils::network::stalker::error::StalkerError;
use crate::utils::network::stalker::profile::StalkerHandshake;

/// Cluster selector used by the Stalker processor. Mirrors the Xtream cluster split
/// (Live/Video/Series) but uses Stalker-native kind names.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum StalkerCluster {
    Live,
    Vod,
    Series,
}

impl StalkerCluster {
    pub fn as_stream_kind(self) -> StalkerStreamKind {
        match self {
            Self::Live => StalkerStreamKind::Live,
            Self::Vod => StalkerStreamKind::Movie,
            Self::Series => StalkerStreamKind::Episode,
        }
    }
}

const DEFAULT_STALKER_CLUSTERS: [StalkerCluster; 3] =
    [StalkerCluster::Live, StalkerCluster::Vod, StalkerCluster::Series];

static RUNTIME_STALKER_CLIENTS: LazyLock<Mutex<HashMap<String, Weak<StalkerApiClient>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Top-level orchestrator. Mirrors the `download_xtream_playlist` signature so the
/// dispatcher can swap the call in place.
#[allow(clippy::too_many_lines)]
pub async fn download_stalker_playlist(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &ConfigInput,
    clusters: Option<&[StalkerCluster]>,
) -> (Vec<PlaylistGroup>, Vec<TuliproxError>, bool) {
    let stalker_cfg = match input.stalker.as_ref() {
        Some(cfg) => cfg.clone(),
        None => {
            return (
                vec![],
                vec![TuliproxError::ConfigInput(format!(
                    "Stalker input '{}' has no stalker configuration block",
                    input.name
                ))],
                false,
            );
        }
    };

    let skip_clusters = skip_clusters_for(input);
    let resolved_clusters: Vec<StalkerCluster> = clusters
        .map_or_else(
            || DEFAULT_STALKER_CLUSTERS.to_vec(),
            <[StalkerCluster]>::to_vec,
        )
        .into_iter()
        .filter(|c| !skip_clusters.contains(c))
        .collect();

    if resolved_clusters.is_empty() {
        info!("Stalker input '{}' has all clusters skipped", input.name);
        return (vec![], vec![], false);
    }

    let portal_url = match resolve_stalker_portal_url(input) {
        Ok(url) => url,
        Err(err) => return (vec![], vec![err], false),
    };

    let api_client = match StalkerApiClient::new(client.clone(), portal_url, stalker_cfg.clone()) {
        Ok(client) => client,
        Err(err) => {
            return (
                vec![],
                vec![TuliproxError::ConfigInput(format!(
                    "failed to build Stalker client for input '{}': {err}",
                    input.name
                ))],
                false,
            );
        }
    };

    let handshake = match api_client.handshake().await {
        Ok(h) => h,
        Err(err) => {
            return (
                vec![],
                vec![TuliproxError::ProviderConnection(format!(
                    "Stalker handshake for input '{}' failed: {err}",
                    input.name
                ))],
                false,
            );
        }
    };

    let storage_path = match ensure_stalker_storage_path(app_config, &input.name).await {
        Ok(p) => p,
        Err(err) => {
            return (
                vec![],
                vec![TuliproxError::Io(format!(
                    "could not prepare Stalker storage for input '{}': {err}",
                    input.name
                ))],
                false,
            );
        }
    };

    let use_disk_based_processing = app_config.config.load().disk_based_processing;

    let mut groups: Vec<PlaylistGroup> = Vec::new();
    let mut errors: Vec<TuliproxError> = Vec::new();

    for cluster in &resolved_clusters {
        let cluster_result = match cluster {
            StalkerCluster::Live => process_stalker_live(&api_client, &handshake, input, &stalker_cfg).await,
            StalkerCluster::Vod => process_stalker_vod(&api_client, &handshake, input, &stalker_cfg).await,
            StalkerCluster::Series => {
                process_stalker_series(&api_client, &handshake, input, &stalker_cfg).await
            }
        };

        match cluster_result {
            Ok(items) => {
                if let Err(err) = persist_stalker_items(app_config, &storage_path, &items).await {
                    errors.push(err);
                }
                let mut group = group_for_cluster(items, *cluster, &input.name);
                if use_disk_based_processing {
                    // Drop the in-memory items when disk-based processing is enabled —
                    // the runtime will re-read them via `StalkerDiskPlaylistSource`.
                    group.channels.clear();
                }
                groups.push(group);
            }
            Err(err) => {
                errors.push(err);
            }
        }
    }

    // EPG bulk fetch. The Stalker/Ministra portal can return very large
    // programme sets, so records are streamed through a bounded channel and
    // persisted in small batches instead of buffering the whole response body
    // or the whole record set in memory.
    if input.has_flag(ConfigInputFlags::StalkerPreResolvePlayback) {
        const BULK_EPG_PERIOD_HOURS: u32 = 24;
        const BULK_EPG_BATCH_SIZE: usize = 512;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::utils::network::stalker::epg::StalkerProgramRecord>(256);
        let persist_app_config = Arc::clone(app_config);
        let persist_storage_path = storage_path.clone();
        let persist_task = tokio::spawn(async move {
            let mut batch: Vec<crate::utils::network::stalker::epg::StalkerProgramRecord> =
                Vec::with_capacity(BULK_EPG_BATCH_SIZE);
            let mut received = 0_u64;
            let mut inserted = 0_u64;
            while let Some(record) = rx.recv().await {
                received = received.saturating_add(1);
                batch.push(record);
                if batch.len() >= BULK_EPG_BATCH_SIZE {
                    inserted = inserted.saturating_add(
                        crate::repository::stalker_repository::persist_stalker_epg_programs(
                            &persist_app_config,
                            &persist_storage_path,
                            &batch,
                        )
                        .await?,
                    );
                    batch.clear();
                }
            }
            if !batch.is_empty() {
                inserted = inserted.saturating_add(
                    crate::repository::stalker_repository::persist_stalker_epg_programs(
                        &persist_app_config,
                        &persist_storage_path,
                        &batch,
                    )
                    .await?,
                );
            }
            Ok::<(u64, u64), TuliproxError>((received, inserted))
        });
        let bulk_result = api_client
            .stream_bulk_epg(
                &handshake,
                BULK_EPG_PERIOD_HOURS,
                |record| {
                    let _ = tx.blocking_send(record);
                },
            )
            .await;
        drop(tx);
        let persisted = persist_task.await;
        match bulk_result {
            Ok(()) => {
                let persist_summary = match persisted {
                    Ok(Ok(summary)) => summary,
                    Ok(Err(err)) => {
                        warn!("Stalker input '{}': bulk EPG persist failed: {err}", input.name);
                        (0, 0)
                    }
                    Err(err) => {
                        warn!("Stalker input '{}': bulk EPG persist task failed: {err}", input.name);
                        (0, 0)
                    }
                };
                let (received, inserted) = persist_summary;
                if received == 0 {
                    info!("Stalker input '{}': bulk EPG returned no programs", input.name);
                } else {
                    info!(
                        "Stalker input '{}': persisted {inserted} EPG program records (received {received})",
                        input.name
                    );
                }
            }
            Err(err) => {
                if let Ok(Err(persist_err)) = persisted {
                    warn!("Stalker input '{}': bulk EPG persist failed after fetch error: {persist_err}", input.name);
                }
                warn!(
                    "Stalker input '{}': bulk EPG fetch failed: {err}",
                    input.name
                );
            }
        }
    }

    parser::log_stalker_download_summary(
        &input.name,
        count_channels_in(&groups, StalkerCluster::Live),
        count_channels_in(&groups, StalkerCluster::Vod),
        count_channels_in(&groups, StalkerCluster::Series),
    );

    (groups, errors, use_disk_based_processing)
}

/// Fetch the live cluster: categories + channels (paginated). The pre-resolve pass is
/// driven by the `StalkerPreResolvePlayback` flag — when set we call `create_link` for
/// every item and persist the resolved URL.
#[allow(clippy::too_many_arguments)]
pub async fn process_stalker_live(
    api_client: &StalkerApiClient,
    handshake: &StalkerHandshake,
    input: &ConfigInput,
    _stalker_cfg: &crate::model::StalkerInputConfig,
) -> Result<Vec<StalkerPlaylistItem>, TuliproxError> {
    let raw_items = api_client.get_live_streams(handshake).await.map_err(stalker_err_to_repo)?;
    let categories = match api_client.get_live_categories(handshake).await {
        Ok(categories) => categories,
        Err(err) => {
            warn!(
                "Stalker input '{}': live categories unavailable, continuing without category map: {err}",
                input.name
            );
            Vec::new()
        }
    };
    let category_map = build_category_map(categories);
    let added_at = chrono::Utc::now().timestamp();
    let mut items: Vec<StalkerPlaylistItem> = raw_items
        .iter()
        .map(|raw| {
            let category = raw
                .category_id
                .as_deref()
                .and_then(|s| s.parse::<u32>().ok())
                .and_then(|id| category_map.get(&id));
            parser::map_stalker_to_playlist_item(raw, category, StalkerStreamKind::Live, added_at)
        })
        .collect();
    if input.has_flag(ConfigInputFlags::StalkerPreResolvePlayback) {
        pre_resolve_playback_urls(api_client, handshake, &mut items, StalkerCluster::Live).await;
    }
    Ok(items)
}

/// Fetch the VOD cluster. Same shape as `process_stalker_live` but with `Movie` items.
pub async fn process_stalker_vod(
    api_client: &StalkerApiClient,
    handshake: &StalkerHandshake,
    input: &ConfigInput,
    _stalker_cfg: &crate::model::StalkerInputConfig,
) -> Result<Vec<StalkerPlaylistItem>, TuliproxError> {
    let raw_items = api_client.get_vod_streams(handshake).await.map_err(stalker_err_to_repo)?;
    let categories = match api_client.get_vod_categories(handshake).await {
        Ok(categories) => categories,
        Err(err) => {
            warn!(
                "Stalker input '{}': VOD categories unavailable, continuing without category map: {err}",
                input.name
            );
            Vec::new()
        }
    };
    let category_map = build_category_map(categories);
    let added_at = chrono::Utc::now().timestamp();
    let mut items: Vec<StalkerPlaylistItem> = raw_items
        .iter()
        .map(|raw| {
            let category = raw
                .category_id
                .as_deref()
                .and_then(|s| s.parse::<u32>().ok())
                .and_then(|id| category_map.get(&id));
            parser::map_stalker_to_playlist_item(raw, category, StalkerStreamKind::Movie, added_at)
        })
        .collect();
    if input.has_flag(ConfigInputFlags::StalkerPreResolvePlayback) {
        pre_resolve_playback_urls(api_client, handshake, &mut items, StalkerCluster::Vod).await;
    }
    Ok(items)
}

/// Fetch the series cluster: list + per-series details. Series roots and episodes are both
/// persisted into the B+Tree so the runtime can render series and episodes through the
/// same source.
pub async fn process_stalker_series(
    api_client: &StalkerApiClient,
    handshake: &StalkerHandshake,
    input: &ConfigInput,
    _stalker_cfg: &crate::model::StalkerInputConfig,
) -> Result<Vec<StalkerPlaylistItem>, TuliproxError> {
    let raw_series = api_client.get_series_list(handshake).await.map_err(stalker_err_to_repo)?;
    let categories = match api_client.get_series_categories(handshake).await {
        Ok(categories) => categories,
        Err(err) => {
            warn!(
                "Stalker input '{}': series categories unavailable, continuing without category map: {err}",
                input.name
            );
            Vec::new()
        }
    };
    let category_map = build_category_map(categories);
    let added_at = chrono::Utc::now().timestamp();
    let mut items: Vec<StalkerPlaylistItem> = Vec::new();
    for raw in &raw_series {
        let category = raw
            .category_id
            .as_deref()
            .and_then(|s| s.parse::<u32>().ok())
            .and_then(|id| category_map.get(&id));
        let root = parser::map_stalker_series_root(raw, category, added_at);
        // Episodes: pull details for each series root. We do this synchronously here to
        // match the existing Xtream pattern; the per-series cost is acceptable for the
        // size of typical Stalker portals.
        let series_id = root.stream_id;
        match api_client.get_series_details(handshake, series_id).await {
            Ok(details) => {
                let episodes = parser::map_stalker_series_details(&details, &root, added_at);
                items.push(root);
                items.extend(episodes);
            }
            Err(err) => {
                warn!(
                    "Stalker get_series_details failed for series_id={series_id} on input '{}': {err}",
                    input.name
                );
                items.push(root);
            }
        }
    }
    if input.has_flag(ConfigInputFlags::StalkerPreResolvePlayback) {
        pre_resolve_playback_urls(api_client, handshake, &mut items, StalkerCluster::Series).await;
    }
    Ok(items)
}

fn build_category_map(categories: Vec<StalkerCategory>) -> HashMap<u32, StalkerCategory> {
    categories
        .into_iter()
        .filter_map(|c| c.id.parse::<u32>().ok().map(|id| (id, c)))
        .collect()
}

/// Walk every item and call `create_link` to convert the raw `cmd` into a playable URL.
/// Items without a `cmd` are skipped silently. On failure we keep the original `cmd` so
/// the runtime re-resolve path still has something to retry against.
pub async fn pre_resolve_playback_urls(
    api_client: &StalkerApiClient,
    handshake: &StalkerHandshake,
    items: &mut [StalkerPlaylistItem],
    cluster: StalkerCluster,
) {
    let kind = cluster.as_stream_kind();
    let requests = items.iter().enumerate().filter_map(|(index, item)| {
        if item.is_series_root() {
            return None;
        }
        let descriptor = item.playback_descriptor.as_ref()?;
        let cmd = descriptor.candidates.first()?.cmd.clone();
        if cmd.is_empty() {
            return None;
        }
        let series_number = (kind == StalkerStreamKind::Episode).then_some(item.number);
        // Honour the playback mode the parser derived from the raw item's temp-link
        // capability flags. Default to DirectUrl for descriptors with no mode set.
        let requested_mode = descriptor.primary_mode;
        Some((index, cmd, series_number, requested_mode))
    }).collect::<Vec<_>>();

    let requests = requests.into_iter().map(|(index, cmd, series_number, requested_mode)| async move {
            let result = api_client.create_link(handshake, kind, requested_mode, &cmd, series_number, None, None).await;
            (index, result)
    });

    let results = stream::iter(requests).buffer_unordered(32).collect::<Vec<_>>().await;
    for (index, result) in results {
        match result {
            Ok(resolved) => {
                items[index].stream_url = Internable::intern(resolved.stream_url);
            }
            Err(err) => {
                debug!(
                    "Stalker pre-resolve create_link failed for stream_id={} on cluster {:?}: {err}",
                    items[index].stream_id, cluster
                );
            }
        }
    }
}

/// Resolve a per-input portal URL using the standard `provider://` resolution pipeline.
fn resolve_stalker_portal_url(input: &ConfigInput) -> Result<String, TuliproxError> {
    let url = input.url.as_str();
    if url.trim().is_empty() {
        return Err(TuliproxError::ConfigInput(format!(
            "Stalker input '{}' has no URL configured",
            input.name
        )));
    }
    input.resolve_url(url).map(Cow::into_owned)
}

fn skip_clusters_for(input: &ConfigInput) -> Vec<StalkerCluster> {
    let mut out = Vec::new();
    if input.has_flag(ConfigInputFlags::SkipLive) {
        out.push(StalkerCluster::Live);
    }
    if input.has_flag(ConfigInputFlags::SkipVod) {
        out.push(StalkerCluster::Vod);
    }
    if input.has_flag(ConfigInputFlags::SkipSeries) {
        out.push(StalkerCluster::Series);
    }
    out
}

fn group_for_cluster(
    items: Vec<StalkerPlaylistItem>,
    cluster: StalkerCluster,
    input_name: &str,
) -> PlaylistGroup {
    use shared::model::XtreamCluster;
    let (xtream_cluster, default_group) = match cluster {
        StalkerCluster::Live => (XtreamCluster::Live, "Live"),
        StalkerCluster::Vod => (XtreamCluster::Video, "Movies"),
        StalkerCluster::Series => (XtreamCluster::Series, "Series"),
    };
    let group_title: Arc<str> = Internable::intern(default_group.to_string());
    let channels: Vec<PlaylistItem> = items
        .into_iter()
        .map(|item| playlist_item_from_stalker(&item, cluster, &group_title, input_name))
        .collect();
    PlaylistGroup {
        id: 0,
        title: group_title,
        channels,
        xtream_cluster,
    }
}

fn playlist_item_from_stalker(
    item: &StalkerPlaylistItem,
    cluster: StalkerCluster,
    group_title: &Arc<str>,
    input_name: &str,
) -> PlaylistItem {
    let item_type = match cluster {
        StalkerCluster::Live => PlaylistItemType::Live,
        StalkerCluster::Vod => PlaylistItemType::Video,
        StalkerCluster::Series => {
            if item.is_series_root() {
                PlaylistItemType::SeriesInfo
            } else {
                PlaylistItemType::Series
            }
        }
    };
    let stream_id_str: Arc<str> = Internable::intern(item.stream_id.to_string());
    let url = parser::create_stalker_stream_url(item.stream_url.as_ref());
    PlaylistItem {
        header: shared::model::PlaylistItemHeader {
            id: Arc::clone(&stream_id_str),
            uuid: generate_provider_playlist_uuid(input_name, &stream_id_str, item_type),
            virtual_id: item.stream_id,
            name: Arc::clone(&item.name),
            logo: item.logo_url.clone().unwrap_or_else(|| Internable::intern(String::new())),
            logo_small: Internable::intern(String::new()),
            group: Arc::clone(group_title),
            title: Arc::clone(&item.name),
            parent_code: Internable::intern(String::new()),
            audio_track: Internable::intern(String::new()),
            time_shift: Internable::intern(String::new()),
            rec: Internable::intern(String::new()),
            url,
            epg_channel_id: item.epg_channel_id.clone(),
            item_type,
            xtream_cluster: match cluster {
                StalkerCluster::Live => shared::model::XtreamCluster::Live,
                StalkerCluster::Vod => shared::model::XtreamCluster::Video,
                StalkerCluster::Series => shared::model::XtreamCluster::Series,
            },
            additional_properties: None,
            input_name: Internable::intern(input_name.to_string()),
            chno: item.number,
            category_id: item.category_id,
            source_ordinal: 0,
        },
    }
}

fn count_channels_in(groups: &[PlaylistGroup], cluster: StalkerCluster) -> usize {
    groups
        .iter()
        .filter(|g| match cluster {
            StalkerCluster::Live => g.xtream_cluster == shared::model::XtreamCluster::Live,
            StalkerCluster::Vod => g.xtream_cluster == shared::model::XtreamCluster::Video,
            StalkerCluster::Series => g.xtream_cluster == shared::model::XtreamCluster::Series,
        })
        .map(|g| g.channels.len())
        .sum()
}

fn stalker_err_to_repo(err: StalkerError) -> TuliproxError {
    TuliproxError::ProviderConnection(format!("Stalker client error: {err}"))
}

fn runtime_client_cache_key(portal_url: &str, cfg: &StalkerInputConfig) -> String {
    format!("{portal_url}|{cfg:?}")
}

fn cached_runtime_stalker_client(
    http_client: &reqwest::Client,
    portal_url: String,
    cfg: &StalkerInputConfig,
) -> Result<Arc<StalkerApiClient>, TuliproxError> {
    let key = runtime_client_cache_key(&portal_url, cfg);
    if let Some(client) = RUNTIME_STALKER_CLIENTS
        .lock()
        .get(&key)
        .and_then(Weak::upgrade)
    {
        return Ok(client);
    }

    let client = Arc::new(
        StalkerApiClient::new(http_client.clone(), portal_url, cfg.clone()).map_err(stalker_err_to_repo)?,
    );
    let mut cache = RUNTIME_STALKER_CLIENTS.lock();
    cache.retain(|_, weak| weak.strong_count() > 0);
    cache.insert(key, Arc::downgrade(&client));
    Ok(client)
}

pub async fn re_resolve_stalker_url(
    app_config: &Arc<AppConfig>,
    http_client: &reqwest::Client,
    input: &ConfigInput,
    provider_id: u32,
    kind: StalkerStreamKind,
) -> Result<Option<Arc<str>>, TuliproxError> {
    if !input.has_flag(ConfigInputFlags::StalkerRuntimeResolvePlayback) || provider_id == 0 {
        return Ok(None);
    }
    let stalker_cfg = input.stalker.as_ref().ok_or_else(|| {
        TuliproxError::ConfigInput(format!("Stalker input '{}' has no stalker configuration block", input.name))
    })?;
    let storage_path = ensure_stalker_storage_path(app_config, &input.name).await?;
    let Some(mut item) = read_stalker_item(app_config, &storage_path, kind, provider_id).await? else {
        return Ok(None);
    };
    let Some(descriptor) = item.playback_descriptor.as_ref() else {
                 debug!("Stalker re-resolve skipped: stream_id={} has no playback_descriptor", item.stream_id);
                 return Ok(None);
             };
    if descriptor.candidates.is_empty() {
        debug!("Stalker re-resolve skipped: stream_id={} descriptor has no candidate", item.stream_id);
        return Ok(None);
    }
    if descriptor
        .candidates
        .iter()
        .all(|candidate| candidate.cmd.trim().is_empty())
    {
        debug!(
            "Stalker re-resolve skipped: stream_id={} descriptor candidates are empty",
            item.stream_id
        );
        return Ok(None);
    }
    let portal_url = resolve_stalker_portal_url(input)?;
    let api_client = cached_runtime_stalker_client(
        http_client,
        portal_url,
        stalker_cfg,
    )?;
    let handshake = api_client.handshake().await.map_err(stalker_err_to_repo)?;
    let series_number = (kind == StalkerStreamKind::Episode).then_some(item.number);

    for candidate in &descriptor.candidates {
        if candidate.cmd.trim().is_empty() {
            continue;
        }
        match api_client
            .create_link(
                &handshake,
                kind,
                candidate.playback_mode,
                &candidate.cmd,
                series_number,
                None,
                None,
            )
            .await
        {
            Ok(resolved) => {
                item.stream_url = Internable::intern(resolved.stream_url);
                persist_stalker_item(app_config, &storage_path, &item).await?;
                return Ok(Some(Arc::clone(&item.stream_url)));
            }
            Err(err) => {
                debug!(
                    "Stalker runtime re-resolve candidate failed for stream_id={}, mode={:?}: {err}",
                    item.stream_id, candidate.playback_mode
                );
            }
        }
    }

    warn!(
        "Stalker runtime re-resolve failed for stream_id={}, invalidating stale stream_url",
        item.stream_id
    );
    // Invalidate the stale persisted URL so the next request triggers a
    // fresh re-resolve rather than serving a known-bad URL indefinitely.
    item.stream_url = Internable::intern(String::new());
    if let Err(persist_err) =
        persist_stalker_item(app_config, &storage_path, &item).await
    {
        warn!(
            "Stalker runtime re-resolve: persisting invalidated stream_url failed: {persist_err}"
        );
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_cfg() -> StalkerInputConfig {
        StalkerInputConfig::default()
    }

    #[test]
    fn runtime_client_cache_key_changes_with_endpoint_preference() {
        let mut cfg = runtime_cfg();
        let key_a = runtime_client_cache_key("http://portal.example", &cfg);
        cfg.endpoint_preference = shared::model::stalker::StalkerEndpointPreference::Portal;
        let key_b = runtime_client_cache_key("http://portal.example", &cfg);
        assert_ne!(key_a, key_b);
    }

    #[test]
    fn runtime_client_cache_key_changes_with_size_caps() {
        let mut cfg = runtime_cfg();
        let key_a = runtime_client_cache_key("http://portal.example", &cfg);
        cfg.size_caps = Some(crate::model::StalkerSizeCaps {
            create_link_kb: 128,
            ordered_list_mb: 8,
            get_epg_mb: 64,
        });
        let key_b = runtime_client_cache_key("http://portal.example", &cfg);
        assert_ne!(key_a, key_b);
    }
}
