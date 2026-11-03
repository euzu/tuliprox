//! The catalog cache and the browse view.
//! `<data_dir>/marketplace/<source-id>.json` holds the parsed catalog of one
//! source with its sync state. A failed sync keeps the old catalog and records
//! the error; a cache written for another URL is discarded.

use super::{
    fetch::{fetch_catalog, CatalogFetch, FetchPolicy, MarketplaceError},
    format::{compatibility, parse, plugin_dto, Catalog},
};
use reqwest::Client;
use semver::Version;
use serde::{Deserialize, Serialize};
use shared::model::{
    is_valid_plugin_id, normalize_plugin_category, MarketplaceCategoryDto, MarketplaceDto, MarketplaceInstalledDto,
    MarketplaceQueryDto, PluginRepositoryInfoDto,
};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use tuliprox_core::model::PluginRepository;
use tuliprox_plugin_api::ABI_MAJOR;

const CACHE_DIR: &str = "marketplace";

/// The on-disk cache of one source.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct CacheFile {
    /// The resolved URL the catalog was fetched from.
    pub(crate) url: String,
    pub(crate) catalog: Option<Catalog>,
    pub(crate) etag: Option<String>,
    pub(crate) last_modified: Option<String>,
    /// Unix seconds of the last successful sync.
    pub(crate) last_sync: Option<i64>,
    pub(crate) last_error: Option<String>,
    pub(crate) warnings: Vec<String>,
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX))
}

/// `None` when the source id is not a path-safe id.
pub(crate) fn cache_path(data_dir: &Path, source_id: &str) -> Option<PathBuf> {
    is_valid_plugin_id(source_id).then(|| data_dir.join(CACHE_DIR).join(format!("{source_id}.json")))
}

/// The cache for `url`; empty when missing, unreadable or for another URL.
pub(crate) fn read_cache(path: &Path, url: &str) -> CacheFile {
    let cache = fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<CacheFile>(&text).ok())
        .filter(|cache| cache.url == url);
    cache.unwrap_or_else(|| CacheFile { url: url.to_string(), ..CacheFile::default() })
}

pub(crate) fn write_cache(path: &Path, cache: &CacheFile) -> io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let content = serde_json::to_vec(cache).map_err(io::Error::other)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
    let written = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(&content)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

async fn load(data_dir: &Path, source: &PluginRepository) -> CacheFile {
    let url = source.resolved_url();
    let Some(path) = cache_path(data_dir, &source.id) else {
        return CacheFile { url, ..CacheFile::default() };
    };
    let fallback = url.clone();
    tokio::task::spawn_blocking(move || read_cache(&path, &url))
        .await
        .unwrap_or_else(|_| CacheFile { url: fallback, ..CacheFile::default() })
}

async fn store(data_dir: &Path, source_id: &str, cache: CacheFile) -> Result<(), MarketplaceError> {
    let Some(path) = cache_path(data_dir, source_id) else {
        return Err(MarketplaceError::Storage("the source id is not valid".to_string()));
    };
    tokio::task::spawn_blocking(move || write_cache(&path, &cache))
        .await
        .map_err(|err| MarketplaceError::Storage(err.to_string()))?
        .map_err(|err| MarketplaceError::Storage(err.kind().to_string()))
}

fn info(source: &PluginRepository, cache: &CacheFile) -> PluginRepositoryInfoDto {
    PluginRepositoryInfoDto {
        id: source.id.clone(),
        name: source.name.clone(),
        url: source.url.clone(),
        enabled: source.enabled,
        last_sync: cache.last_sync,
        last_error: cache.last_error.clone(),
        plugin_count: cache.catalog.as_ref().map_or(0, |catalog| catalog.plugins.len()),
    }
}

/// Fetch, parse and store the catalog of `source`. A `304` keeps the cache and
/// refreshes its sync time. On failure the old catalog stays and the error is
/// recorded as `last_error`.
///
/// # Errors
/// The [`MarketplaceError`] of the fetch, parse or store.
pub async fn sync_source(
    data_dir: &Path,
    client: &Client,
    policy: FetchPolicy,
    source: &PluginRepository,
) -> Result<PluginRepositoryInfoDto, MarketplaceError> {
    let mut cache = load(data_dir, source).await;
    let (etag, last_modified) =
        if cache.catalog.is_some() { (cache.etag.clone(), cache.last_modified.clone()) } else { (None, None) };
    let outcome: Result<_, MarketplaceError> = async {
        match fetch_catalog(client, policy, &cache.url, etag.as_deref(), last_modified.as_deref()).await? {
            CatalogFetch::NotModified => Ok(None),
            CatalogFetch::Fresh { bytes, etag, last_modified } => {
                let (catalog, warnings) = parse(&bytes, policy.allow_http).map_err(MarketplaceError::Catalog)?;
                Ok(Some((catalog, warnings, etag, last_modified)))
            }
        }
    }
    .await;
    match outcome {
        Ok(fresh) => {
            if let Some((catalog, warnings, etag, last_modified)) = fresh {
                cache.catalog = Some(catalog);
                cache.warnings = warnings;
                cache.etag = etag;
                cache.last_modified = last_modified;
            }
            cache.last_sync = Some(now_secs());
            cache.last_error = None;
            let row = info(source, &cache);
            store(data_dir, &source.id, cache).await?;
            Ok(row)
        }
        Err(err) => {
            cache.last_error = Some(err.to_string());
            if let Err(store_err) = store(data_dir, &source.id, cache).await {
                log::warn!("cannot record the marketplace sync error of `{}`: {store_err}", source.id);
            }
            Err(err)
        }
    }
}

/// The display name the catalog at `url` declares, fetched now and not cached.
/// Used when a source is added: its id and name come from the catalog. `None`
/// when the catalog has no name (or only a blank one).
///
/// # Errors
/// The [`MarketplaceError`] of the fetch or the parse.
pub async fn probe_source_name(
    client: &Client,
    policy: FetchPolicy,
    url: &str,
) -> Result<Option<String>, MarketplaceError> {
    match fetch_catalog(client, policy, url, None, None).await? {
        CatalogFetch::Fresh { bytes, .. } => {
            let (catalog, _warnings) = parse(&bytes, policy.allow_http).map_err(MarketplaceError::Catalog)?;
            Ok(catalog.name.map(|name| name.trim().to_string()).filter(|name| !name.is_empty()))
        }
        CatalogFetch::NotModified => Ok(None),
    }
}

/// The info row of `source` from its cache; no network.
pub async fn source_info(data_dir: &Path, source: &PluginRepository) -> PluginRepositoryInfoDto {
    info(source, &load(data_dir, source).await)
}

/// Whether a catalog is cached for `source` at its current URL.
pub async fn has_catalog(data_dir: &Path, source: &PluginRepository) -> bool {
    load(data_dir, source).await.catalog.is_some()
}

/// The per-entry warnings of the last successful sync.
pub async fn source_warnings(data_dir: &Path, source: &PluginRepository) -> Vec<String> {
    load(data_dir, source).await.warnings
}

/// Delete the cache file of a removed source. A missing file is fine; an
/// installed plugin is never touched.
pub async fn remove_cache(data_dir: &Path, source_id: &str) {
    let Some(path) = cache_path(data_dir, source_id) else { return };
    let id = source_id.to_string();
    let removed = tokio::task::spawn_blocking(move || fs::remove_file(path)).await;
    if let Ok(Err(err)) = removed {
        if err.kind() != io::ErrorKind::NotFound {
            log::warn!("cannot remove the marketplace cache of `{id}`: {err}");
        }
    }
}

fn host(host_version: &str) -> Version { Version::parse(host_version).unwrap_or_else(|_| Version::new(0, 0, 0)) }

/// The category id of a catalog plugin. Normalised on every read: a cache written before categories were
/// normalised still holds them as the catalog spelled them.
fn category_of(plugin: &super::format::Plugin) -> Option<String> {
    plugin.category.as_deref().and_then(normalize_plugin_category)
}

/// The words of a search, lower case. Every one has to match.
fn search_words(query: &MarketplaceQueryDto) -> Vec<String> {
    query.q.as_deref().unwrap_or_default().split_whitespace().map(str::to_lowercase).collect()
}

/// Whether `word` occurs in `field` where a word starts: at the beginning, or after anything that is not a letter
/// or a digit. `live` matches `live-tv` and `Live TV`, not `delivers`; `discord-notifier` matches as typed.
fn starts_a_word(field: &str, word: &str) -> bool {
    field
        .match_indices(word)
        .any(|(at, _)| at == 0 || !field[..at].chars().next_back().is_some_and(char::is_alphanumeric))
}

/// Whether every word of the search starts a word in the id, name, description, author or category of
/// `plugin`. The words are matched one by one, so `slack notifier` finds a plugin that says Slack in its name
/// and notifier in its category, and `live tv` finds the category `live-tv`.
fn matches_text(plugin: &super::format::Plugin, words: &[String]) -> bool {
    if words.is_empty() {
        return true;
    }
    let category = category_of(plugin);
    let fields: Vec<String> =
        [Some(&plugin.id), Some(&plugin.name), plugin.description.as_ref(), plugin.author.as_ref(), category.as_ref()]
            .into_iter()
            .flatten()
            .map(|field| field.to_lowercase())
            .collect();
    words.iter().all(|word| fields.iter().any(|field| starts_a_word(field, word)))
}

/// The category filter of a query, normalised. `Err` for a filter that names no valid category: it matches
/// nothing, which is what an unknown category should show.
fn wanted_category(query: &MarketplaceQueryDto) -> Result<Option<String>, ()> {
    match query.category.as_deref().map(str::trim).filter(|category| !category.is_empty()) {
        None => Ok(None),
        Some(raw) => normalize_plugin_category(raw).map(Some).ok_or(()),
    }
}

/// The marketplace view: the plugins of every enabled source's cache, filtered
/// by `query`, with the installed copy and the other sources offering the same
/// id. `host_version` is the running Tuliprox version.
#[allow(clippy::implicit_hasher)]
pub async fn browse(
    data_dir: &Path,
    sources: &[PluginRepository],
    installed: &HashMap<String, MarketplaceInstalledDto>,
    query: &MarketplaceQueryDto,
    host_version: &str,
) -> MarketplaceDto {
    let host_version = host(host_version);
    let mut caches = Vec::new();
    for source in sources.iter().filter(|source| source.enabled) {
        caches.push((source, load(data_dir, source).await));
    }

    let mut offered: HashMap<&str, Vec<&str>> = HashMap::new();
    for (source, cache) in &caches {
        for plugin in cache.catalog.iter().flat_map(|catalog| &catalog.plugins) {
            offered.entry(plugin.id.as_str()).or_default().push(source.id.as_str());
        }
    }

    let words = search_words(query);
    let wanted = wanted_category(query);
    // The facet counts the plugins of the search and the source before the category filter, so choosing a
    // category does not make the other categories vanish.
    let mut facets: BTreeMap<String, usize> = BTreeMap::new();
    let mut total = 0;
    let mut plugins = Vec::new();
    for (source, cache) in &caches {
        if query.source.as_deref().is_some_and(|wanted| !wanted.is_empty() && wanted != source.id) {
            continue;
        }
        let mut rows: Vec<_> = Vec::new();
        for plugin in
            cache.catalog.iter().flat_map(|catalog| &catalog.plugins).filter(|plugin| matches_text(plugin, &words))
        {
            total += 1;
            let category = category_of(plugin);
            if let Some(category) = &category {
                *facets.entry(category.clone()).or_default() += 1;
            }
            let in_category = match &wanted {
                Ok(None) => true,
                Ok(Some(wanted)) => category.as_ref() == Some(wanted),
                Err(()) => false,
            };
            if !in_category {
                continue;
            }
            let also_in = offered
                .get(plugin.id.as_str())
                .map(|ids| ids.iter().filter(|id| **id != source.id).map(ToString::to_string).collect())
                .unwrap_or_default();
            rows.push(plugin_dto(
                plugin,
                (&source.id, &source.name),
                &host_version,
                ABI_MAJOR,
                installed.get(&plugin.id),
                also_in,
            ));
        }
        rows.sort_by_key(|row| row.name.to_lowercase());
        plugins.extend(rows);
    }
    // The selected category stays in the list even when the search leaves nothing in it, so it can be unselected.
    if let Ok(Some(wanted)) = wanted {
        facets.entry(wanted).or_default();
    }

    MarketplaceDto {
        plugins,
        sources: caches.iter().map(|(source, cache)| info(source, cache)).collect(),
        categories: facets.into_iter().map(|(id, count)| MarketplaceCategoryDto { id, count }).collect(),
        total,
    }
}

/// The catalog release an install resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRelease {
    /// Where the package is downloaded from.
    pub package_url: String,
}

/// The outcome of [`resolve_release`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseLookup {
    Found(ResolvedRelease),
    /// The id or the version is not in the cached catalog.
    NotFound,
    /// The release exists but cannot install here; the reason.
    Incompatible(String),
}

/// Find `id` at `version` in the cached catalog of `source` and check its
/// compatibility with this host.
pub async fn resolve_release(
    data_dir: &Path,
    source: &PluginRepository,
    id: &str,
    version: &str,
    host_version: &str,
) -> ReleaseLookup {
    let cache = load(data_dir, source).await;
    let Some(plugin) = cache.catalog.iter().flat_map(|catalog| &catalog.plugins).find(|plugin| plugin.id == id) else {
        return ReleaseLookup::NotFound;
    };
    let wanted = Version::parse(version.trim()).ok();
    let release = plugin.releases.iter().find(|release| match &wanted {
        Some(wanted) => Version::parse(&release.version).is_ok_and(|own| own == *wanted),
        None => release.version == version,
    });
    let Some(release) = release else { return ReleaseLookup::NotFound };
    match compatibility(release, &host(host_version), ABI_MAJOR) {
        Ok(()) => ReleaseLookup::Found(ResolvedRelease { package_url: release.package_url.clone() }),
        Err(reason) => ReleaseLookup::Incompatible(reason),
    }
}

#[cfg(test)]
mod tests;
