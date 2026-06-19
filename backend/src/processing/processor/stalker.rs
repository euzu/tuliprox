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
use shared::model::{PlaylistGroup, PlaylistItem};
use shared::utils::{short_hash, Internable};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
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

// Bounded fan-out: Stalker/Ministra portals are typically small installations
// that throttle or ban clients hammering create_link.
const STALKER_PRE_RESOLVE_CONCURRENCY: usize = 8;


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
    let mut live_count = 0_usize;
    let mut vod_count = 0_usize;
    let mut series_count = 0_usize;

    for cluster in &resolved_clusters {
        let cluster_result = match cluster {
            StalkerCluster::Live => process_stalker_live(&api_client, &handshake, input).await,
            StalkerCluster::Vod => process_stalker_vod(&api_client, &handshake, input).await,
            StalkerCluster::Series => process_stalker_series(&api_client, &handshake, input).await,
        };

        match cluster_result {
            Ok(items) => {
                // Count before any group post-processing so the download summary
                // reflects what was actually fetched, not the cleared remnants.
                match cluster {
                    StalkerCluster::Live => live_count = items.len(),
                    StalkerCluster::Vod => vod_count = items.len(),
                    StalkerCluster::Series => series_count = items.len(),
                }
                match persist_stalker_items(app_config, &storage_path, cluster.as_stream_kind(), &items).await {
                    Ok(_) => {
                        let mut cluster_groups = groups_for_cluster(items, *cluster, &input.name);
                        if use_disk_based_processing {
                            // Drop the in-memory items when disk-based processing is enabled —
                            // the runtime will re-read them via `StalkerDiskPlaylistSource`.
                            for group in &mut cluster_groups {
                                group.channels.clear();
                            }
                        }
                        groups.extend(cluster_groups);
                    }
                    Err(err) => {
                        warn!(
                            "Stalker input '{}': persisting {cluster:?} items failed: {err}",
                            input.name
                        );
                        errors.push(err);
                        if !use_disk_based_processing {
                            // In-memory mode still has the full channel list, so the groups
                            // remain valid despite the persist failure. In disk-based mode
                            // the cleared groups would be phantom-empty (nothing on disk to
                            // re-read), so the cluster is skipped entirely.
                            groups.extend(groups_for_cluster(items, *cluster, &input.name));
                        }
                    }
                }
            }
            Err(err) => {
                errors.push(err);
            }
        }
    }

    // EPG bulk fetch. The whole fetch is collected in memory and persisted as a
    // single snapshot once the stream completes. The `on_program` callback runs
    // on the async consumer side, so it must never block — calling
    // `blocking_send` from it would panic inside the tokio runtime.
    if input.has_flag(ConfigInputFlags::StalkerBulkEpg) {
        const BULK_EPG_PERIOD_HOURS: u32 = 24;
        let mut records: Vec<crate::utils::network::stalker::epg::StalkerProgramRecord> = Vec::new();
        let bulk_result = api_client
            .stream_bulk_epg(&handshake, BULK_EPG_PERIOD_HOURS, |record| records.push(record))
            .await;
        match bulk_result {
            Ok(()) => {
                let received = records.len();
                if received == 0 {
                    info!("Stalker input '{}': bulk EPG returned no programs", input.name);
                } else {
                    match crate::repository::stalker_repository::persist_stalker_epg_programs(
                        app_config,
                        &storage_path,
                        &records,
                    )
                    .await
                    {
                        Ok(inserted) => info!(
                            "Stalker input '{}': persisted {inserted} EPG program records (received {received})",
                            input.name
                        ),
                        Err(err) => {
                            warn!("Stalker input '{}': bulk EPG persist failed: {err}", input.name);
                        }
                    }
                }
            }
            Err(err) => {
                warn!("Stalker input '{}': bulk EPG fetch failed: {err}", input.name);
            }
        }
    }

    parser::log_stalker_download_summary(&input.name, live_count, vod_count, series_count);

    (groups, errors, use_disk_based_processing)
}

/// Fetch the live cluster: categories + channels (paginated). The pre-resolve pass is
/// driven by the `StalkerPreResolvePlayback` flag — when set we call `create_link` for
/// every item and persist the resolved URL.
pub async fn process_stalker_live(
    api_client: &StalkerApiClient,
    handshake: &StalkerHandshake,
    input: &ConfigInput,
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
) -> Result<Vec<StalkerPlaylistItem>, TuliproxError> {
    // Bounded fan-out for per-series detail fetches — enough to hide latency
    // without hammering fragile Ministra portals.
    const STALKER_SERIES_DETAILS_CONCURRENCY: usize = 4;
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
    let roots: Vec<StalkerPlaylistItem> = raw_series
        .iter()
        .map(|raw| {
            let category = raw
                .category_id
                .as_deref()
                .and_then(|s| s.parse::<u32>().ok())
                .and_then(|id| category_map.get(&id));
            parser::map_stalker_series_root(raw, category, added_at)
        })
        .collect();

    // Fetch per-series details with bounded concurrency. Results are collected
    // and restored to the original catalog order before episode mapping so the
    // collision-safe storage-id assignment stays deterministic regardless of
    // response arrival order. The original i64 series id is used for the portal
    // call (the narrowed u32 `stream_id` is only a storage key).
    //
    // The `(index, series_id)` tuples are collected into an owned `Vec` *before*
    // the futures are built. This mirrors the pattern used by
    // `pre_resolve_playback_urls` below: without it, the `.map` closure would
    // receive a `&StalkerPlaylistItem` tied to `roots.iter()`'s lifetime, and
    // `stream::iter(...).buffer_unordered(...)` would reject the closure for
    // not being HRTB (it must work for *any* reference lifetime, not just the
    // one from this iterator).
    let detail_requests: Vec<(usize, u32)> = roots
        .iter()
        .enumerate()
        .map(|(index, root)| (index, root.series_id.unwrap_or(root.stream_id)))
        .collect();
    let detail_futures = detail_requests.into_iter().map(|(index, series_id)| async move {
        let result = api_client.get_series_details(handshake, series_id).await;
        (index, result)
    });
    let mut detail_results = stream::iter(detail_futures)
        .buffer_unordered(STALKER_SERIES_DETAILS_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    detail_results.sort_by_key(|(index, _)| *index);

    let mut used_episode_ids: HashSet<u32> = HashSet::new();
    let mut items: Vec<StalkerPlaylistItem> = Vec::new();
    for (root, (_, result)) in roots.into_iter().zip(detail_results) {
        match result {
            Ok(details) => {
                let episodes =
                    parser::map_stalker_series_details(&details, &root, added_at, &mut used_episode_ids);
                items.push(root);
                items.extend(episodes);
            }
            Err(err) => {
                warn!(
                    "Stalker get_series_details failed for series_id={} on input '{}': {err}",
                    root.stream_id, input.name
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

    let results = stream::iter(requests)
        .buffer_unordered(STALKER_PRE_RESOLVE_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
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

/// Convert the items of one cluster into per-category `PlaylistGroup`s,
/// mirroring the Xtream pipeline and the disk-based source: items are grouped
/// by their portal category id (falling back to a cluster default title for
/// items without a category) instead of collapsing the whole cluster into a
/// single group.
fn groups_for_cluster(
    items: Vec<StalkerPlaylistItem>,
    cluster: StalkerCluster,
    input_name: &str,
) -> Vec<PlaylistGroup> {
    use shared::model::XtreamCluster;
    let xtream_cluster = match cluster {
        StalkerCluster::Live => XtreamCluster::Live,
        StalkerCluster::Vod => XtreamCluster::Video,
        StalkerCluster::Series => XtreamCluster::Series,
    };
    let mut groups_map: indexmap::IndexMap<u32, PlaylistGroup> = indexmap::IndexMap::new();
    for item in items {
        let category_id = item.category_id;
        let pli = PlaylistItem::from_stalker(&item, input_name);
        let group = groups_map.entry(category_id).or_insert_with(|| PlaylistGroup {
            id: category_id,
            title: Arc::clone(&pli.header.group),
            channels: Vec::new(),
            xtream_cluster,
        });
        group.channels.push(pli);
    }
    groups_map.into_values().collect()
}

fn stalker_err_to_repo(err: StalkerError) -> TuliproxError {
    TuliproxError::ProviderConnection(format!("Stalker client error: {err}"))
}

/// Cache key for the runtime client map. Built from explicit, non-secret
/// fields — `StalkerInputConfig` carries the portal account credentials, so
/// formatting the whole config with `{:?}` would leak username and password
/// into a long-lived in-memory map key. Credentials still differentiate the
/// key, but only as a non-reversible short hash.
fn runtime_client_cache_key(portal_url: &str, cfg: &StalkerInputConfig) -> String {
    use std::fmt::Write;
    let mut key = String::with_capacity(192);
    let _ = write!(
        key,
        "{portal_url}|auth={:?}|preset={:?}|endpoint={:?}|pages={:?}",
        cfg.auth_mode, cfg.mag_preset, cfg.endpoint_preference, cfg.catalog_max_pages
    );
    if let Some(caps) = cfg.size_caps.as_ref() {
        let _ = write!(
            key,
            "|caps={}:{}:{}",
            caps.create_link_kb, caps.ordered_list_mb, caps.get_epg_mb
        );
    }
    if let Some(device) = cfg.device.as_ref() {
        let _ = write!(
            key,
            "|dev={}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
            device.mac_address.as_deref().unwrap_or_default(),
            device.device_profile.as_deref().unwrap_or_default(),
            device.serial_number.as_deref().unwrap_or_default(),
            device.device_id.as_deref().unwrap_or_default(),
            device.device_id2.as_deref().unwrap_or_default(),
            device.signature.as_deref().unwrap_or_default(),
            device.timezone.as_deref().unwrap_or_default(),
            device.locale.as_deref().unwrap_or_default(),
            device.user_agent.as_deref().unwrap_or_default(),
            device.x_user_agent.as_deref().unwrap_or_default(),
        );
    }
    let credentials = format!(
        "{}\n{}",
        cfg.username.as_deref().unwrap_or_default(),
        cfg.password.as_deref().unwrap_or_default()
    );
    let _ = write!(key, "|cred={}", short_hash(&credentials));
    key
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

    #[test]
    fn runtime_client_cache_key_does_not_leak_credentials() {
        let mut cfg = runtime_cfg();
        cfg.username = Some("secret_user".to_string());
        cfg.password = Some("secret_pass".to_string());
        let key = runtime_client_cache_key("http://portal.example", &cfg);
        assert!(!key.contains("secret_user"));
        assert!(!key.contains("secret_pass"));
        // Different credentials must still produce a different cache key so two
        // inputs against the same portal never share a client.
        let mut other = cfg.clone();
        other.password = Some("other_pass".to_string());
        let other_key = runtime_client_cache_key("http://portal.example", &other);
        assert_ne!(key, other_key);
    }
}
