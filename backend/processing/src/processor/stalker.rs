//! Stalker/Ministra playlist processor.
//!
//! Orchestrates Stalker authentication, resumable catalog refresh and B+Tree persistence. The
//! processor is the single entry point the playlist dispatcher uses for `InputType::Stalker`
//! — mirror of `xtream::download_xtream_playlist`.
//!
//! Reverse-proxy re-resolve (when a 4xx upstream error is observed) is implemented in the
//! HLS/Xtream endpoints and reaches back into the API client via the helper
//! [`tuliprox_iptv::stalker::client::StalkerApiClient::create_link`].

#![allow(clippy::too_many_lines, clippy::needless_pass_by_value)]

use super::stalker_refresh::{
    advance_stalker_refresh, StalkerClusterSelection, StalkerRefreshMode, StalkerRefreshOutcome,
};
use log::{debug, info, warn};
use lru::LruCache;
use parking_lot::Mutex;
use shared::{
    error::TuliproxError,
    model::{stalker::StalkerStreamKind, stalker_item::StalkerPlaylistItem, PlaylistGroup, PlaylistItem},
    utils::Internable,
};
use std::{
    borrow::Cow,
    collections::HashMap,
    num::NonZeroUsize,
    sync::{Arc, LazyLock, Weak},
    time::{Duration, Instant},
};
use tuliprox_core::model::{AppConfig, ConfigInput, ConfigInputFlags, StalkerInputConfig};
use tuliprox_iptv::stalker::{client::StalkerApiClient, error::StalkerError, parser};
use tuliprox_repository::{
    stalker_generation_repository::{load_active_manifest, load_checkpoint},
    stalker_repository::{ensure_stalker_storage_path, load_stalker_items_at, read_stalker_item_at},
};

/// Cluster selector used by the Stalker processor. Mirrors the Xtream cluster split
/// (Live/Video/Series) but uses Stalker-native kind names.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum StalkerCluster {
    Live,
    Vod,
    Series,
}

const DEFAULT_STALKER_CLUSTERS: [StalkerCluster; 3] =
    [StalkerCluster::Live, StalkerCluster::Vod, StalkerCluster::Series];

static RUNTIME_STALKER_CLIENTS: LazyLock<Mutex<LruCache<String, Arc<StalkerApiClient>>>> =
    LazyLock::new(|| Mutex::new(LruCache::new(NonZeroUsize::new(64).unwrap_or(NonZeroUsize::MIN))));

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct RuntimeLinkKey {
    fingerprint: u64,
    generation: u64,
    provider_id: u32,
    kind: StalkerStreamKind,
}

struct RuntimeLink {
    url: Arc<str>,
    expires_at: Instant,
}

static RUNTIME_STALKER_LINKS: LazyLock<Mutex<LruCache<RuntimeLinkKey, RuntimeLink>>> =
    LazyLock::new(|| Mutex::new(LruCache::new(NonZeroUsize::new(4096).unwrap_or(NonZeroUsize::MIN))));

static STALKER_REFRESH_LOCKS: LazyLock<tokio::sync::Mutex<HashMap<Arc<str>, Weak<tokio::sync::Semaphore>>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

async fn try_acquire_stalker_refresh(input_name: &Arc<str>) -> Option<tokio::sync::OwnedSemaphorePermit> {
    let semaphore = {
        let mut locks = STALKER_REFRESH_LOCKS.lock().await;
        let semaphore = locks.get(input_name).and_then(Weak::upgrade).unwrap_or_else(|| {
            let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
            locks.insert(Arc::clone(input_name), Arc::downgrade(&semaphore));
            semaphore
        });
        locks.retain(|_, lock| lock.strong_count() > 0);
        semaphore
    };
    semaphore.try_acquire_owned().ok()
}

fn cached_resolved_link(key: RuntimeLinkKey, force_refresh: bool) -> Option<Arc<str>> {
    let mut cache = RUNTIME_STALKER_LINKS.lock();
    let expired = cache.peek(&key).is_some_and(|entry| entry.expires_at <= Instant::now());
    if force_refresh || expired {
        cache.pop(&key);
        return None;
    }
    cache.get(&key).map(|entry| Arc::clone(&entry.url))
}

fn cache_resolved_link(key: RuntimeLinkKey, url: Arc<str>) {
    RUNTIME_STALKER_LINKS.lock().put(key, RuntimeLink { url, expires_at: Instant::now() + Duration::from_secs(45) });
}

/// Top-level orchestrator. Mirrors the `download_xtream_playlist` signature so the
/// dispatcher can swap the call in place.
#[allow(clippy::too_many_lines)]
pub async fn download_stalker_playlist(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &ConfigInput,
    clusters: Option<&[StalkerCluster]>,
    refresh_mode: StalkerRefreshMode,
    materialize_active: bool,
) -> (Vec<PlaylistGroup>, Vec<TuliproxError>, bool, bool) {
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
                false,
            );
        }
    };

    let skip_clusters = skip_clusters_for(input);
    let resolved_clusters: Vec<StalkerCluster> = clusters
        .map_or_else(|| DEFAULT_STALKER_CLUSTERS.to_vec(), <[StalkerCluster]>::to_vec)
        .into_iter()
        .filter(|c| !skip_clusters.contains(c))
        .collect();

    if resolved_clusters.is_empty() {
        info!("Stalker input '{}' has all clusters skipped", input.name);
        return (vec![], vec![], false, false);
    }
    let refresh_selection = StalkerClusterSelection::requested(input, &resolved_clusters);

    let portal_url = match resolve_stalker_portal_url(input) {
        Ok(url) => url,
        Err(err) => return (vec![], vec![err], false, false),
    };

    let identity_fingerprint = stalker_identity_fingerprint(&portal_url, &stalker_cfg);
    let api_client = match cached_runtime_stalker_client(client, portal_url, &stalker_cfg) {
        Ok(client) => client,
        Err(err) => {
            return (
                vec![],
                vec![TuliproxError::ConfigInput(format!(
                    "failed to build Stalker client for input '{}': {err}",
                    input.name
                ))],
                false,
                false,
            );
        }
    };

    let storage_path = match ensure_stalker_storage_path(app_config, &input.name).await {
        Ok(p) => p,
        Err(err) => {
            return (
                vec![],
                vec![TuliproxError::Io(format!("could not prepare Stalker storage for input '{}': {err}", input.name))],
                false,
                false,
            );
        }
    };

    let outcome = if let Some(_refresh_permit) = try_acquire_stalker_refresh(&input.name).await {
        let handshake = match api_client.handshake().await {
            Ok(handshake) => handshake,
            Err(err) => {
                return (
                    vec![],
                    vec![TuliproxError::ProviderConnection(format!(
                        "Stalker handshake for input '{}' failed: {err}",
                        input.name
                    ))],
                    false,
                    false,
                );
            }
        };
        loop {
            let refresh = advance_stalker_refresh(
                app_config,
                api_client.as_ref(),
                &handshake,
                refresh_selection,
                &storage_path,
                identity_fingerprint,
                refresh_mode.budget(),
            );
            let result = if refresh_mode == StalkerRefreshMode::ServerSlice {
                if let Ok(result) = tokio::time::timeout(Duration::from_mins(45), refresh).await {
                    result
                } else {
                    let checkpoint = match load_checkpoint(&storage_path, identity_fingerprint).await {
                        Ok(checkpoint) => checkpoint,
                        Err(err) => return (Vec::new(), vec![err], false, false),
                    };
                    break StalkerRefreshOutcome::Yielded {
                        phase: checkpoint.as_ref().map_or(
                            tuliprox_repository::stalker_generation_repository::StalkerRefreshPhase::LiveBulk,
                            |state| state.phase.clone(),
                        ),
                        processed: checkpoint.as_ref().map_or(0, |state| state.processed),
                        skipped: checkpoint.as_ref().map_or(0, |state| state.skipped_count),
                        error: None,
                    };
                }
            } else {
                refresh.await
            };
            match result {
                Ok(StalkerRefreshOutcome::Yielded { error: None, .. })
                    if refresh_mode == StalkerRefreshMode::Complete => {}
                Ok(StalkerRefreshOutcome::Yielded { .. }) if refresh_mode == StalkerRefreshMode::Parallel => {
                    tokio::task::yield_now().await;
                }
                Ok(outcome) => break outcome,
                Err(err) => return (Vec::new(), vec![err], false, false),
            }
        }
    } else {
        return (Vec::new(), Vec::new(), app_config.config.load().disk_based_processing, true);
    };

    let yielded = matches!(&outcome, StalkerRefreshOutcome::Yielded { .. });
    let terminal = matches!(&outcome, StalkerRefreshOutcome::Terminal(_));
    let mut errors = Vec::new();
    match outcome {
        StalkerRefreshOutcome::Complete => {}
        StalkerRefreshOutcome::Yielded { phase, processed, skipped, error } => {
            info!(
                "Stalker input '{}' yielded in phase {phase:?} after {processed} records ({skipped} skipped)",
                input.name
            );
            if let Some(error) = error {
                warn!("Stalker input '{}': resumable refresh paused after error: {error}", input.name);
            }
        }
        StalkerRefreshOutcome::Terminal(error) => errors.push(error),
    }

    let manifest = match load_active_manifest(&storage_path, identity_fingerprint).await {
        Ok(manifest) => manifest,
        Err(err) => {
            errors.push(err);
            return (Vec::new(), errors, false, yielded);
        }
    };
    if terminal {
        if let Err(err) =
            tuliprox_repository::stalker_generation_repository::cleanup_obsolete_generations(&storage_path, &manifest)
                .await
        {
            errors.push(err);
        }
    }
    let use_disk_based_processing = app_config.config.load().disk_based_processing;
    if !materialize_active {
        return (Vec::new(), errors, use_disk_based_processing, yielded);
    }
    let mut groups = Vec::new();
    let mut counts = [0_usize; 3];
    for cluster in resolved_clusters {
        let items_result = match cluster {
            StalkerCluster::Live => match manifest.live.as_ref() {
                Some(files) => load_stalker_items_at(app_config, &files.data).await,
                None => Ok(Vec::new()),
            },
            StalkerCluster::Vod => match manifest.vod.as_ref() {
                Some(files) => load_stalker_items_at(app_config, &files.data).await,
                None => Ok(Vec::new()),
            },
            StalkerCluster::Series => match manifest.series.as_ref() {
                Some(files) => {
                    match (
                        load_stalker_items_at(app_config, &files.roots).await,
                        load_stalker_items_at(app_config, &files.episodes).await,
                    ) {
                        (Ok(mut roots), Ok(episodes)) => {
                            roots.extend(episodes);
                            Ok(roots)
                        }
                        (Err(err), _) | (_, Err(err)) => Err(err),
                    }
                }
                None => Ok(Vec::new()),
            },
        };
        match items_result {
            Ok(items) => {
                counts[cluster as usize] = items.len();
                let cluster_groups = groups_for_cluster(items, cluster, &input.name);
                groups.extend(cluster_groups);
            }
            Err(err) => errors.push(err),
        }
    }
    parser::log_stalker_download_summary(&input.name, counts[0], counts[1], counts[2]);
    (groups, errors, use_disk_based_processing, yielded)
}

/// Resolve a per-input portal URL using the standard `provider://` resolution pipeline.
fn resolve_stalker_portal_url(input: &ConfigInput) -> Result<String, TuliproxError> {
    let url = input.url.as_str();
    if url.trim().is_empty() {
        return Err(TuliproxError::ConfigInput(format!("Stalker input '{}' has no URL configured", input.name)));
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

/// Map a Stalker failure onto the workspace error type, keeping its classification.
///
/// Flattening everything to `ProviderConnection` read as "the network had a bad moment",
/// so a misconfigured portal URL and a rejected password were both reported as connection
/// trouble - and both counted as worth retrying.
fn stalker_err_to_repo(err: StalkerError) -> TuliproxError {
    tuliprox_iptv::error::stalker_error_to_tuliprox(&err)
}

/// Cache key for the runtime client map. Built from explicit, non-secret
/// fields — `StalkerInputConfig` carries the portal account credentials, so
/// formatting the whole config with `{:?}` would leak username and password
/// into a long-lived in-memory map key. Credentials still differentiate the
/// key, but only as a non-reversible short hash.
fn runtime_client_cache_key(portal_url: &str, cfg: &StalkerInputConfig) -> String {
    format!("{portal_url}|{:016x}", cfg.identity_fingerprint(portal_url))
}

pub fn stalker_identity_fingerprint(portal_url: &str, cfg: &StalkerInputConfig) -> u64 {
    cfg.identity_fingerprint(portal_url)
}

fn cached_runtime_stalker_client(
    http_client: &reqwest::Client,
    portal_url: String,
    cfg: &StalkerInputConfig,
) -> Result<Arc<StalkerApiClient>, TuliproxError> {
    let key = runtime_client_cache_key(&portal_url, cfg);
    if let Some(client) = RUNTIME_STALKER_CLIENTS.lock().get(&key).cloned() {
        return Ok(client);
    }

    let client =
        Arc::new(StalkerApiClient::new(http_client.clone(), portal_url, cfg.clone()).map_err(stalker_err_to_repo)?);
    RUNTIME_STALKER_CLIENTS.lock().put(key, Arc::clone(&client));
    Ok(client)
}

pub async fn re_resolve_stalker_url(
    app_config: &Arc<AppConfig>,
    http_client: &reqwest::Client,
    input: &ConfigInput,
    provider_id: u32,
    kind: StalkerStreamKind,
    force_refresh: bool,
) -> Result<Option<Arc<str>>, TuliproxError> {
    if provider_id == 0 {
        return Ok(None);
    }
    let stalker_cfg = input.stalker.as_ref().ok_or_else(|| {
        TuliproxError::ConfigInput(format!("Stalker input '{}' has no stalker configuration block", input.name))
    })?;
    let portal_url = resolve_stalker_portal_url(input)?;
    let identity_fingerprint = stalker_cfg.identity_fingerprint(&portal_url);
    let storage_path = ensure_stalker_storage_path(app_config, &input.name).await?;
    let manifest = load_active_manifest(&storage_path, identity_fingerprint).await?;
    let generation_and_path = match kind {
        StalkerStreamKind::Live | StalkerStreamKind::Archive => {
            manifest.live.as_ref().map(|files| (files.generation, &files.data))
        }
        StalkerStreamKind::Movie => manifest.vod.as_ref().map(|files| (files.generation, &files.data)),
        StalkerStreamKind::Episode => manifest.series.as_ref().map(|files| (files.generation, &files.episodes)),
    };
    let Some((generation, item_path)) = generation_and_path else { return Ok(None) };
    let link_key = RuntimeLinkKey { fingerprint: identity_fingerprint, generation, provider_id, kind };
    if let Some(url) = cached_resolved_link(link_key, force_refresh) {
        return Ok(Some(url));
    }
    let Some(item) = read_stalker_item_at(app_config, item_path, provider_id).await? else {
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
    if descriptor.candidates.iter().all(|candidate| candidate.cmd.trim().is_empty()) {
        debug!("Stalker re-resolve skipped: stream_id={} descriptor candidates are empty", item.stream_id);
        return Ok(None);
    }
    let api_client = cached_runtime_stalker_client(http_client, portal_url, stalker_cfg)?;
    let handshake = api_client.handshake().await.map_err(stalker_err_to_repo)?;
    let series_number = (kind == StalkerStreamKind::Episode).then_some(item.number);

    for candidate in &descriptor.candidates {
        if candidate.cmd.trim().is_empty() {
            continue;
        }
        match api_client
            .create_link(&handshake, kind, candidate.playback_mode, &candidate.cmd, series_number, None, None)
            .await
        {
            Ok(resolved) => {
                let url = Internable::intern(resolved.stream_url);
                cache_resolved_link(link_key, Arc::clone(&url));
                return Ok(Some(url));
            }
            Err(err) => {
                debug!(
                    "Stalker runtime re-resolve candidate failed for stream_id={}, mode={:?}: {err}",
                    item.stream_id, candidate.playback_mode
                );
            }
        }
    }

    warn!("Stalker runtime re-resolve failed for stream_id={}, invalidating stale stream_url", item.stream_id);
    RUNTIME_STALKER_LINKS.lock().pop(&link_key);
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
        cfg.size_caps =
            Some(tuliprox_core::model::StalkerSizeCaps { create_link_kb: 128, ordered_list_mb: 8, get_epg_mb: 64 });
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

    #[test]
    fn runtime_client_cache_keeps_session_client_alive() -> Result<(), TuliproxError> {
        let http = reqwest::Client::new();
        let cfg = runtime_cfg();
        let first = cached_runtime_stalker_client(&http, "http://portal.example".to_string(), &cfg)?;
        let weak = Arc::downgrade(&first);
        drop(first);

        let second = cached_runtime_stalker_client(&http, "http://portal.example".to_string(), &cfg)?;

        assert!(weak.upgrade().is_some());
        let upgraded = weak.upgrade().ok_or_else(|| TuliproxError::ProviderConnection("client dropped".to_string()))?;
        assert!(Arc::ptr_eq(&upgraded, &second));
        Ok(())
    }

    #[test]
    fn resolved_link_cache_can_be_forced_stale() {
        let key = RuntimeLinkKey { fingerprint: 8, generation: 1, provider_id: 43, kind: StalkerStreamKind::Live };
        cache_resolved_link(key, "http://stream.example/live".into());
        assert_eq!(cached_resolved_link(key, false).as_deref(), Some("http://stream.example/live"));
        assert!(cached_resolved_link(key, true).is_none());
    }

    #[test]
    fn resolved_link_cache_is_scoped_to_the_published_generation() {
        let old = RuntimeLinkKey { fingerprint: 7, generation: 1, provider_id: 42, kind: StalkerStreamKind::Live };
        let current = RuntimeLinkKey { generation: 2, ..old };
        cache_resolved_link(old, "http://stream.example/old".into());
        assert!(cached_resolved_link(current, false).is_none());
    }

    #[tokio::test]
    async fn refresh_ownership_is_non_blocking_across_invocations() {
        let name: Arc<str> = "portal-a".into();
        let first = try_acquire_stalker_refresh(&name).await;
        assert!(first.is_some());
        assert!(try_acquire_stalker_refresh(&name).await.is_none());
        drop(first);
        assert!(try_acquire_stalker_refresh(&name).await.is_some());
    }
}
