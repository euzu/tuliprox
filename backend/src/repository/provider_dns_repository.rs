use indexmap::IndexMap;
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use shared::utils::DNS_RESOLVED_FILE;
use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use crate::api::model::AppState;
use crate::model::ConfigProvider;

/// All providers' resolved DNS data, keyed by provider name → hostname → IPs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DnsResolvedStore {
    #[serde(flatten)]
    pub providers: HashMap<String, IndexMap<String, Vec<IpAddr>>>,
}

static DNS_WRITER_GENERATION: AtomicU64 = AtomicU64::new(0);
const DNS_WRITER_FLUSH_INTERVAL_SECS: u64 = 2;
const DNS_WRITER_FLUSH_BATCH_THRESHOLD: usize = 32;

pub fn next_dns_writer_generation() -> u64 { DNS_WRITER_GENERATION.fetch_add(1, Ordering::SeqCst) + 1 }

fn is_dns_writer_generation_current(generation: u64) -> bool {
    DNS_WRITER_GENERATION.load(Ordering::SeqCst) == generation
}

pub fn dns_resolved_file_path(storage_dir: &str) -> PathBuf {
    PathBuf::from(storage_dir).join(DNS_RESOLVED_FILE)
}

#[derive(Debug)]
pub enum DnsResolvedStoreLoadError {
    Read(std::io::Error),
    Parse(serde_json::Error),
}

impl std::fmt::Display for DnsResolvedStoreLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(err) => write!(f, "read failed: {err}"),
            Self::Parse(err) => write!(f, "parse failed: {err}"),
        }
    }
}

pub async fn load_dns_resolved_store_from_path(path: &Path) -> Result<Option<DnsResolvedStore>, DnsResolvedStoreLoadError> {
    match fs::read_to_string(path).await {
        Ok(data) => serde_json::from_str(&data).map(Some).map_err(DnsResolvedStoreLoadError::Parse),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(DnsResolvedStoreLoadError::Read(err)),
    }
}

pub async fn persist_dns_resolved_store(path: &Path, store: &DnsResolvedStore) -> Result<(), String> {
    let json = match serde_json::to_string_pretty(store) {
        Ok(value) => value,
        Err(err) => return Err(format!("serialize failed: {err}")),
    };

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .await
            .map_err(|err| format!("create parent dir '{}' failed: {err}", parent.display()))?;
    }

    let tmp_path = path.with_extension("json.tmp");
    fs::write(&tmp_path, json)
        .await
        .map_err(|err| format!("write temp file '{}' failed: {err}", tmp_path.display()))?;

    if let Err(err) = fs::rename(&tmp_path, path).await {
        #[cfg(windows)]
        {
            if fs::remove_file(path).await.is_ok() && fs::rename(&tmp_path, path).await.is_ok() {
                debug!(
                    "Persisted DNS resolved store to '{}' (providers={})",
                    path.display(),
                    store.providers.len()
                );
                return Ok(());
            }
        }
        let _ = fs::remove_file(&tmp_path).await;
        return Err(format!(
            "rename temp file '{}' → '{}' failed: {err}",
            tmp_path.display(),
            path.display()
        ));
    }

    debug!(
        "Persisted DNS resolved store to '{}' (providers={})",
        path.display(),
        store.providers.len()
    );
    Ok(())
}

/// Load persisted DNS resolved data and seed the in-memory caches for all providers.
/// Called at startup and after config reloads, before DNS background tasks run.
pub async fn load_persisted_dns_resolved(app_state: &std::sync::Arc<crate::api::model::AppState>) {
    let storage_dir = app_state.app_config.config.load().storage_dir.clone();
    let path = dns_resolved_file_path(&storage_dir);

    let store = match load_dns_resolved_store_from_path(&path).await {
        Ok(Some(store)) => store,
        Ok(None) => {
            debug!("No persisted DNS resolved file found at '{}'", path.display());
            return;
        }
        Err(err) => {
            warn!("Failed to load persisted DNS resolved file '{}': {err}", path.display());
            return;
        }
    };

    let sources = app_state.app_config.sources.load();
    let mut seeded_count = 0usize;
    for (provider_name, hosts) in &store.providers {
        let Some(provider) = sources.get_provider_by_name(provider_name) else {
            debug!("Persisted DNS resolved data for unknown provider '{provider_name}', skipping");
            continue;
        };
        if !provider.get_dns_config().is_some_and(|cfg| cfg.enabled) {
            debug!("Persisted DNS resolved data for disabled-dns provider '{provider_name}', skipping");
            continue;
        }
        for (host, ips) in hosts {
            if !ips.is_empty() {
                // Only seed if the cache is currently empty for this host (don't overwrite runtime data).
                if provider.dns_cache.ip_count(host) == 0 {
                    provider.dns_cache.store_resolved(host, ips.clone());
                    seeded_count += 1;
                }
            }
        }
    }

    if seeded_count > 0 {
        info!("Seeded {seeded_count} host(s) from persisted DNS resolved file '{}'", path.display());
    }
}

fn prune_store_to_runtime_enabled_providers(app_state: &Arc<AppState>, store: &mut DnsResolvedStore) -> usize {
    let sources = app_state.app_config.sources.load();
    let enabled_provider_names = sources
        .provider
        .iter()
        .filter(|provider| provider.get_dns_config().is_some_and(|cfg| cfg.enabled))
        .map(|provider| provider.name.to_string())
        .collect::<HashSet<String>>();
    prune_store_to_enabled_provider_names(store, &enabled_provider_names)
}

fn prune_store_to_enabled_provider_names(
    store: &mut DnsResolvedStore,
    enabled_provider_names: &HashSet<String>,
) -> usize {
    let before = store.providers.len();
    store
        .providers
        .retain(|provider_name, _| enabled_provider_names.contains(provider_name));
    before.saturating_sub(store.providers.len())
}

fn is_provider_dns_enabled_in_runtime(app_state: &Arc<AppState>, provider_name: &str) -> bool {
    let sources = app_state.app_config.sources.load();
    sources
        .get_provider_by_name(provider_name)
        .is_some_and(|provider| provider.get_dns_config().is_some_and(|cfg| cfg.enabled))
}

pub async fn prune_persisted_dns_resolved_to_runtime(app_state: &Arc<AppState>) {
    let storage_dir = app_state.app_config.config.load().storage_dir.clone();
    let path = dns_resolved_file_path(&storage_dir);
    let mut store = match load_dns_resolved_store_from_path(&path).await {
        Ok(Some(existing)) => existing,
        Ok(None) => return,
        Err(err) => {
            warn!(
                "Failed to load DNS resolved store for runtime-prune '{}': {err}",
                path.display()
            );
            return;
        }
    };

    let removed = prune_store_to_runtime_enabled_providers(app_state, &mut store);
    if removed == 0 {
        return;
    }

    if let Err(err) = persist_dns_resolved_store(&path, &store).await {
        warn!(
            "Failed to persist DNS resolved store after runtime-prune '{}': {err}",
            path.display()
        );
    } else {
        info!(
            "Pruned {removed} stale provider(s) from DNS resolved store '{}'",
            path.display()
        );
    }
}

#[derive(Debug)]
pub struct DnsResolvedWriteUpdate {
    pub provider_name: String,
    pub resolved: IndexMap<String, Vec<IpAddr>>,
}

pub type DnsResolvedWriteTx = mpsc::Sender<DnsResolvedWriteUpdate>;

#[allow(clippy::too_many_lines)]
pub fn spawn_dns_resolved_writer(
    app_state: Arc<AppState>,
    cancel: CancellationToken,
    generation: u64,
) -> DnsResolvedWriteTx {
    let storage_dir = app_state.app_config.config.load().storage_dir.clone();
    let path = dns_resolved_file_path(&storage_dir);
    let (tx, mut rx) = mpsc::channel::<DnsResolvedWriteUpdate>(256);

    tokio::spawn(async move {
        let mut store = match load_dns_resolved_store_from_path(&path).await {
            Ok(Some(existing)) => existing,
            Ok(None) => DnsResolvedStore::default(),
            Err(err) => {
                warn!("Failed to load existing DNS resolved store '{}': {err}", path.display());
                DnsResolvedStore::default()
            }
        };
        let removed_on_start = prune_store_to_runtime_enabled_providers(&app_state, &mut store);
        if removed_on_start > 0 {
            info!(
                "Pruned {removed_on_start} stale provider(s) from DNS resolved store on writer start '{}'",
                path.display()
            );
            if let Err(err) = persist_dns_resolved_store(&path, &store).await {
                warn!(
                    "Failed to persist DNS resolved store after startup prune '{}': {err}",
                    path.display()
                );
            }
        }
        let mut dirty = false;
        let mut dirty_updates = 0usize;
        let mut flush_before_exit = false;
        let mut flush_timer = std::pin::pin!(tokio::time::sleep(Duration::from_secs(
            DNS_WRITER_FLUSH_INTERVAL_SECS,
        )));
        let mut flush_timer_active = false;

        loop {
            if !is_dns_writer_generation_current(generation) {
                debug!(
                    "Stopping stale DNS resolved writer generation {} for '{}'",
                    generation,
                    path.display()
                );
                break;
            }
            tokio::select! {
                () = cancel.cancelled() => {
                    debug!("Stopping DNS resolved writer task for '{}'", path.display());
                    flush_before_exit = true;
                    break;
                }
                maybe_update = rx.recv() => {
                    let Some(update) = maybe_update else {
                        debug!("DNS resolved writer channel closed for '{}'", path.display());
                        flush_before_exit = true;
                        break;
                    };
                    if !is_dns_writer_generation_current(generation) {
                        debug!(
                            "Dropping update for stale DNS resolved writer generation {} on '{}'",
                            generation,
                            path.display()
                        );
                        break;
                    }

                    if update.resolved.is_empty() || !is_provider_dns_enabled_in_runtime(&app_state, update.provider_name.as_str()) {
                        store.providers.remove(update.provider_name.as_str());
                    } else {
                        store.providers.insert(update.provider_name, update.resolved);
                    }

                    dirty = true;
                    dirty_updates = dirty_updates.saturating_add(1);

                    if dirty_updates >= DNS_WRITER_FLUSH_BATCH_THRESHOLD {
                        let _ = prune_store_to_runtime_enabled_providers(&app_state, &mut store);
                        if let Err(err) = persist_dns_resolved_store(&path, &store).await {
                            warn!("Failed to persist DNS resolved store '{}': {err}", path.display());
                        } else {
                            dirty = false;
                            dirty_updates = 0;
                            flush_timer_active = false;
                        }
                    } else if !flush_timer_active {
                        flush_timer.as_mut().reset(
                            tokio::time::Instant::now() + Duration::from_secs(DNS_WRITER_FLUSH_INTERVAL_SECS),
                        );
                        flush_timer_active = true;
                    }
                }
                () = &mut flush_timer, if flush_timer_active => {
                    flush_timer_active = false;
                    if dirty {
                        let _ = prune_store_to_runtime_enabled_providers(&app_state, &mut store);
                        if let Err(err) = persist_dns_resolved_store(&path, &store).await {
                            warn!("Failed to persist DNS resolved store '{}': {err}", path.display());
                        } else {
                            dirty = false;
                            dirty_updates = 0;
                        }
                    }
                }
            }
        }

        if flush_before_exit && dirty {
            let _ = prune_store_to_runtime_enabled_providers(&app_state, &mut store);
            if let Err(err) = persist_dns_resolved_store(&path, &store).await {
                warn!(
                    "Failed to persist DNS resolved store during writer shutdown '{}': {err}",
                    path.display()
                );
            }
        }
    });

    tx
}

pub async fn queue_provider_resolved_snapshot(writer_tx: &DnsResolvedWriteTx, provider: &Arc<ConfigProvider>) {
    let update = DnsResolvedWriteUpdate {
        provider_name: provider.name.to_string(),
        resolved: provider.snapshot_resolved_ordered(),
    };
    if let Err(err) = writer_tx.send(update).await {
        warn!("Failed to queue DNS resolved update for provider '{}': {err}", provider.name);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        is_dns_writer_generation_current, load_dns_resolved_store_from_path, next_dns_writer_generation,
        persist_dns_resolved_store, prune_store_to_enabled_provider_names, DnsResolvedStore, DnsResolvedStoreLoadError,
    };
    use indexmap::IndexMap;
    use std::collections::{HashMap, HashSet};
    use std::net::IpAddr;

    #[tokio::test]
    async fn load_dns_resolved_store_reports_parse_error() {
        let dir = tempfile::tempdir().expect("temp dir should be created");
        let path = dir.path().join("provider_dns_resolved.json");
        tokio::fs::write(&path, "{ not-json").await.expect("test file should be written");

        let result = load_dns_resolved_store_from_path(&path).await;
        assert!(matches!(result, Err(DnsResolvedStoreLoadError::Parse(_))));
    }

    #[tokio::test]
    async fn persist_and_load_dns_resolved_store_roundtrip() {
        let dir = tempfile::tempdir().expect("temp dir should be created");
        let path = dir.path().join("provider_dns_resolved.json");
        let mut providers = HashMap::new();
        providers.insert(
            "provider-a".to_string(),
            IndexMap::from([(
                "example.com".to_string(),
                vec!["203.0.113.10".parse::<IpAddr>().expect("valid ip")],
            )]),
        );
        let store = DnsResolvedStore { providers };

        persist_dns_resolved_store(&path, &store).await.expect("persist should succeed");
        let loaded = load_dns_resolved_store_from_path(&path)
            .await
            .expect("load should succeed")
            .expect("store should exist");

        assert_eq!(loaded.providers.len(), 1);
        assert_eq!(
            loaded
                .providers
                .get("provider-a")
                .and_then(|hosts| hosts.get("example.com")),
            Some(&vec!["203.0.113.10".parse::<IpAddr>().expect("valid ip")])
        );
    }

    #[test]
    fn generation_switch_marks_previous_writer_as_stale() {
        let old_generation = next_dns_writer_generation();
        let new_generation = next_dns_writer_generation();
        assert_ne!(old_generation, new_generation);
        assert!(!is_dns_writer_generation_current(old_generation));
        assert!(is_dns_writer_generation_current(new_generation));
    }

    #[test]
    fn prune_store_to_enabled_provider_names_removes_stale_entries() {
        let mut providers = HashMap::new();
        providers.insert(
            "provider-a".to_string(),
            IndexMap::from([("example.com".to_string(), vec!["203.0.113.10".parse::<IpAddr>().expect("valid ip")])]),
        );
        providers.insert(
            "provider-b".to_string(),
            IndexMap::from([("example.org".to_string(), vec!["203.0.113.20".parse::<IpAddr>().expect("valid ip")])]),
        );
        let mut store = DnsResolvedStore { providers };
        let enabled = HashSet::from(["provider-a".to_string()]);

        let removed = prune_store_to_enabled_provider_names(&mut store, &enabled);
        assert_eq!(removed, 1);
        assert!(store.providers.contains_key("provider-a"));
        assert!(!store.providers.contains_key("provider-b"));
    }
}
