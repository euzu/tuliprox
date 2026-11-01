//! EPG status index and playlist explorer overlay.
//!
//! Provides [`EpgStatusIndex`] (cached per source by stamp) and overlay functions that merge
//! saved override values into served [`UiPlaylistItem`]s with one lookup per item, without running
//! filters or mappers.

use crate::{
    api::{
        endpoints::{v1_api_playlist::epg_inspect::with_epg_query, xmltv_api::get_epg_path_for_target_by_type},
        model::{check_scan_cancelled, AppState},
    },
    model::{ConfigInput, ConfigTarget, EpgSource, EpgSourceType},
    processing::{epg::get_input_raw_epg_file_path, parser::ics::parse_ics_file_to_channel},
    repository::{OverrideParseResult, OverrideScope, ParsedOverrideFile},
    utils::file_exists_async,
};
use chrono::Utc;
use shared::{
    error::TuliproxError,
    model::{
        ExplorerEpgState, FieldDelta, ItemOverride, OverrideItemKind, PlaylistItemHeader, TargetType,
        UiItemExplorerMeta, UiOverrideState, UiPlaylistItem,
    },
};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, RwLock},
    time::SystemTime,
};
use tuliprox_core::{
    model::{override_key, InputAliasMap, OverrideKey},
    utils::{with_folded_epg_id, EpgIdOutputCase},
};

/// In-memory EPG status index mapping channel IDs to their latest programme stop timestamp.
///
/// If a channel has no programmes, `max_stop` holds `i64::MIN`.
#[derive(Debug, Clone)]
pub struct EpgStatusIndex {
    pub max_stop: HashMap<Arc<str>, i64>,
    pub case: EpgIdOutputCase,
}

impl Default for EpgStatusIndex {
    fn default() -> Self { Self { max_stop: HashMap::new(), case: EpgIdOutputCase::Preserve } }
}

impl EpgStatusIndex {
    #[must_use]
    pub fn new(max_stop: HashMap<Arc<str>, i64>, case: EpgIdOutputCase) -> Self { Self { max_stop, case } }

    /// Computes the EPG status for `epg_id` relative to timestamp `now`.
    #[must_use]
    pub fn status(&self, epg_id: Option<&str>, now: i64) -> ExplorerEpgState {
        let Some(id) = epg_id.map(str::trim).filter(|s| !s.is_empty()) else {
            return ExplorerEpgState::MissingId;
        };
        let stop_opt = match self.case {
            EpgIdOutputCase::Preserve => self.max_stop.get(id).copied(),
            EpgIdOutputCase::LowercaseAscii => with_folded_epg_id(id, |folded| self.max_stop.get(folded).copied()),
        };
        match stop_opt {
            Some(max_stop) => {
                if max_stop > now {
                    ExplorerEpgState::Ok
                } else {
                    ExplorerEpgState::NoProgramme
                }
            }
            None => ExplorerEpgState::NotInSources,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EpgStampKey {
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    inode: u64,
}

impl EpgStampKey {
    fn from_metadata(m: &std::fs::Metadata) -> Option<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Some(Self {
            len: m.len(),
            modified: m.modified().ok()?,
            #[cfg(unix)]
            inode: m.ino(),
        })
    }
}

struct CachedEpgStatusIndex {
    stamp: EpgStampKey,
    last_used: std::time::Instant,
    index: Arc<EpgStatusIndex>,
}

type EpgIndexCacheMap = HashMap<PathBuf, CachedEpgStatusIndex>;
static EPG_INDEX_CACHE: OnceLock<RwLock<EpgIndexCacheMap>> = OnceLock::new();

fn get_cached_index(path: &Path, stamp: &EpgStampKey) -> Option<Arc<EpgStatusIndex>> {
    let cache = EPG_INDEX_CACHE.get_or_init(Default::default);
    let mut guard = cache.write().ok()?;
    guard.retain(|_, entry| entry.last_used.elapsed() < std::time::Duration::from_mins(30));
    let entry = guard.get_mut(path)?;
    if entry.stamp == *stamp {
        entry.last_used = std::time::Instant::now();
        Some(Arc::clone(&entry.index))
    } else {
        None
    }
}

fn put_cached_index(path: PathBuf, stamp: EpgStampKey, index: Arc<EpgStatusIndex>) {
    let cache = EPG_INDEX_CACHE.get_or_init(Default::default);
    if let Ok(mut guard) = cache.write() {
        guard.retain(|_, entry| entry.last_used.elapsed() < std::time::Duration::from_mins(30));
        guard.insert(path, CachedEpgStatusIndex { stamp, last_used: std::time::Instant::now(), index });
    }
}

/// Builds or retrieves the cached [`EpgStatusIndex`] for a target EPG database.
pub async fn get_target_epg_status_index(app_state: &Arc<AppState>, target: &ConfigTarget) -> Arc<EpgStatusIndex> {
    let config = app_state.app_config.config.load();
    let epg_path = get_epg_path_for_target_by_type(&config, target, TargetType::Xtream)
        .or_else(|| get_epg_path_for_target_by_type(&config, target, TargetType::M3u));
    drop(config);
    let case = target.epg_id_case();
    let Some(epg_path) = epg_path else {
        return Arc::new(EpgStatusIndex::new(HashMap::new(), case));
    };
    if !tokio::fs::try_exists(&epg_path).await.unwrap_or(false) {
        return Arc::new(EpgStatusIndex::new(HashMap::new(), case));
    }

    let stamp = tokio::fs::metadata(&epg_path).await.ok().and_then(|m| EpgStampKey::from_metadata(&m));
    if let Some(ref stamp) = stamp {
        if let Some(cached) = get_cached_index(&epg_path, stamp) {
            return cached;
        }
    }

    let index_res = with_epg_query(
        &app_state.epg_scans,
        &app_state.app_config.file_locks,
        &epg_path,
        move |query, epg_path, cancel| {
            let mut max_stop = HashMap::new();
            for entry in query.iter() {
                check_scan_cancelled(cancel)?;
                let (_key, channel) = entry.map_err(|err| {
                    TuliproxError::RepositoryEpg(format!("Cannot read EPG database {}: {err}", epg_path.display()))
                })?;
                let stop = channel.programmes.iter().map(|p| p.stop).max().unwrap_or(i64::MIN);
                max_stop.insert(channel.id, stop);
            }
            Ok(EpgStatusIndex::new(max_stop, case))
        },
    )
    .await;

    match index_res {
        Ok(index) => {
            let index = Arc::new(index);
            if let Some(stamp) = stamp {
                put_cached_index(epg_path, stamp, Arc::clone(&index));
            }
            index
        }
        Err(err) => {
            log::warn!("Failed to read target EPG status for {}: {err}", epg_path.display());
            Arc::new(EpgStatusIndex::new(HashMap::new(), case))
        }
    }
}

/// Builds or retrieves the cached [`EpgStatusIndex`] for an input from stored/cached EPG files on disk (NEVER network).
pub async fn get_input_epg_status_index(app_state: &Arc<AppState>, input: &ConfigInput) -> Arc<EpgStatusIndex> {
    let Some(epg_config) = input.epg.as_ref() else {
        return Arc::new(EpgStatusIndex::new(HashMap::new(), EpgIdOutputCase::LowercaseAscii));
    };
    let storage_dir = app_state.app_config.config.load().storage_dir.clone();
    let mut combined_max_stop: HashMap<Arc<str>, i64> = HashMap::new();

    for epg_source in &epg_config.sources {
        let Ok(raw_epg_path) = get_input_raw_epg_file_path(epg_source, input, &storage_dir).await else {
            continue;
        };
        if !file_exists_async(&raw_epg_path).await {
            continue;
        }
        let stamp = tokio::fs::metadata(&raw_epg_path).await.ok().and_then(|m| EpgStampKey::from_metadata(&m));
        let Some(stamp) = stamp else {
            continue;
        };

        if let Some(cached) = get_cached_index(&raw_epg_path, &stamp) {
            for (id, stop) in &cached.max_stop {
                combined_max_stop.entry(Arc::clone(id)).and_modify(|s| *s = (*s).max(*stop)).or_insert(*stop);
            }
            continue;
        }

        let parsed_res = parse_input_epg_source_file(app_state, &raw_epg_path, epg_source).await;
        match parsed_res {
            Ok(parsed) => {
                let idx = Arc::new(parsed);
                put_cached_index(raw_epg_path, stamp, Arc::clone(&idx));
                for (id, stop) in &idx.max_stop {
                    combined_max_stop.entry(Arc::clone(id)).and_modify(|s| *s = (*s).max(*stop)).or_insert(*stop);
                }
            }
            Err(err) => {
                log::warn!("Failed to parse input EPG file {}: {err}", raw_epg_path.display());
            }
        }
    }

    Arc::new(EpgStatusIndex::new(combined_max_stop, EpgIdOutputCase::LowercaseAscii))
}

async fn parse_input_epg_source_file(
    app_state: &Arc<AppState>,
    raw_epg_path: &Path,
    epg_source: &EpgSource,
) -> Result<EpgStatusIndex, TuliproxError> {
    let _cache_lock = app_state.app_config.file_locks.read_lock(raw_epg_path).await;
    match epg_source.source_type {
        EpgSourceType::Xmltv => {
            let max_stop = parse_xmltv_channel_max_stops(raw_epg_path).await?;
            Ok(EpgStatusIndex::new(max_stop, EpgIdOutputCase::LowercaseAscii))
        }
        EpgSourceType::Ics => {
            let (Some(ics_config), Some(channel_id)) = (epg_source.ics.as_ref(), epg_source.channel_id.clone()) else {
                return Ok(EpgStatusIndex::new(HashMap::new(), EpgIdOutputCase::LowercaseAscii));
            };
            let channel =
                parse_ics_file_to_channel(raw_epg_path, channel_id, epg_source.channel_title.clone(), ics_config)
                    .await?;
            let stop = channel.programmes.iter().map(|p| p.stop).max().unwrap_or(i64::MIN);
            let mut max_stop = HashMap::new();
            with_folded_epg_id(&channel.id, |folded| {
                max_stop.insert(Arc::from(folded), stop);
            });
            Ok(EpgStatusIndex::new(max_stop, EpgIdOutputCase::LowercaseAscii))
        }
    }
}

async fn parse_xmltv_channel_max_stops(path: &Path) -> Result<HashMap<Arc<str>, i64>, TuliproxError> {
    use quick_xml::events::Event;
    use tuliprox_core::{
        model::{EPG_ATTRIB_CHANNEL, EPG_ATTRIB_ID, EPG_ATTRIB_STOP, EPG_TAG_CHANNEL, EPG_TAG_PROGRAMME},
        utils::{
            async_file_reader, compressed_file_reader_async::CompressedFileReaderAsync, parse_xmltv_time,
            with_folded_epg_id,
        },
    };

    let compressed_reader = CompressedFileReaderAsync::new(path)
        .await
        .map_err(|err| TuliproxError::RepositoryEpg(format!("Cannot open EPG file {}: {err}", path.display())))?;
    let mut reader = quick_xml::reader::Reader::from_reader(async_file_reader(compressed_reader));
    let mut buf = Vec::new();
    let mut max_stop: HashMap<Arc<str>, i64> = HashMap::new();

    loop {
        match reader.read_event_into_async(&mut buf).await {
            Ok(Event::Empty(e) | Event::Start(e)) => {
                let name = e.name();
                let tag = name.as_ref();
                if tag == EPG_TAG_CHANNEL {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == EPG_ATTRIB_ID {
                            if let Ok(val) = attr.normalized_value(quick_xml::XmlVersion::Implicit1_0) {
                                with_folded_epg_id(&val, |folded| {
                                    max_stop.entry(Arc::from(folded)).or_insert(i64::MIN);
                                });
                            }
                        }
                    }
                } else if tag == EPG_TAG_PROGRAMME {
                    let mut channel = None;
                    let mut stop = None;
                    for attr in e.attributes().flatten() {
                        let k = attr.key.as_ref();
                        if k == EPG_ATTRIB_CHANNEL {
                            channel = attr.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok();
                        } else if k == EPG_ATTRIB_STOP {
                            stop = attr.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok();
                        }
                    }
                    if let (Some(chan), Some(stop_str)) = (channel, stop) {
                        if let Some(stop_ts) = parse_xmltv_time(&stop_str) {
                            with_folded_epg_id(&chan, |folded| {
                                max_stop
                                    .entry(Arc::from(folded))
                                    .and_modify(|s| *s = (*s).max(stop_ts))
                                    .or_insert(stop_ts);
                            });
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(TuliproxError::RepositoryEpg(err.to_string())),
            _ => {}
        }
        buf.clear();
    }
    Ok(max_stop)
}

fn parent_input_name(aliases: &InputAliasMap, name: &str) -> String {
    aliases.parent_of(name).map_or_else(|| name.to_string(), ToString::to_string)
}

fn inputs_for_target(app_config: &crate::model::AppConfig, aliases: &InputAliasMap, target_name: &str) -> Vec<String> {
    let sources = app_config.sources.load();
    let mut parents = Vec::new();
    let mut seen = HashSet::new();
    for source in &sources.sources {
        if !source.targets.iter().any(|t| t.name == target_name) {
            continue;
        }
        for input in &source.inputs {
            let parent = parent_input_name(aliases, input);
            if seen.insert(parent.clone()) {
                parents.push(parent);
            }
        }
    }
    parents
}

fn targets_for_input(app_config: &crate::model::AppConfig, aliases: &InputAliasMap, parent: &str) -> Vec<String> {
    let sources = app_config.sources.load();
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    for source in &sources.sources {
        if !source.inputs.iter().any(|input| parent_input_name(aliases, input) == parent) {
            continue;
        }
        for target in &source.targets {
            if seen.insert(target.name.clone()) {
                names.push(target.name.clone());
            }
        }
    }
    names
}

fn header_of_ui_item(item: &UiPlaylistItem) -> PlaylistItemHeader {
    PlaylistItemHeader {
        id: Arc::clone(&item.provider_id),
        url: Arc::clone(&item.url),
        input_name: Arc::clone(&item.input_name),
        item_type: item.item_type,
        ..Default::default()
    }
}

fn apply_override_to_ui_item(item: &mut UiPlaylistItem, ov: &ItemOverride) {
    match &ov.name {
        FieldDelta::Keep => {}
        FieldDelta::Clear => item.name = Arc::from(""),
        FieldDelta::Set(v) => item.name = Arc::from(v.as_str()),
    }
    match &ov.title {
        FieldDelta::Keep => {}
        FieldDelta::Clear => item.title = Arc::from(""),
        FieldDelta::Set(v) => item.title = Arc::from(v.as_str()),
    }
    match &ov.group {
        FieldDelta::Keep => {}
        FieldDelta::Clear => item.group = Arc::from(""),
        FieldDelta::Set(v) => item.group = Arc::from(v.as_str()),
    }
    match &ov.logo {
        FieldDelta::Keep => match &ov.logo_small {
            FieldDelta::Keep => {}
            FieldDelta::Clear => item.logo = Arc::from(""),
            FieldDelta::Set(v) => item.logo = Arc::from(v.as_str()),
        },
        FieldDelta::Clear => item.logo = Arc::from(""),
        FieldDelta::Set(v) => item.logo = Arc::from(v.as_str()),
    }
    match ov.rating {
        FieldDelta::Keep => {}
        FieldDelta::Clear => item.rating = 0.0,
        FieldDelta::Set(v) => item.rating = v,
    }
    match &ov.epg_channel_id {
        FieldDelta::Keep => {}
        FieldDelta::Clear => item.epg_channel_id = None,
        FieldDelta::Set(v) => item.epg_channel_id = Some(Arc::from(v.as_str())),
    }
}

fn merge_item_override(base: &mut ItemOverride, overlay: &ItemOverride) {
    if !overlay.audio_track.is_keep() {
        base.audio_track = overlay.audio_track.clone();
    }
    if !overlay.cast.is_keep() {
        base.cast = overlay.cast.clone();
    }
    if !overlay.chno.is_keep() {
        base.chno = overlay.chno;
    }
    if !overlay.director.is_keep() {
        base.director = overlay.director.clone();
    }
    if !overlay.epg_channel_id.is_keep() {
        base.epg_channel_id = overlay.epg_channel_id.clone();
    }
    if !overlay.genre.is_keep() {
        base.genre = overlay.genre.clone();
    }
    if !overlay.group.is_keep() {
        base.group = overlay.group.clone();
    }
    if !overlay.logo.is_keep() {
        base.logo = overlay.logo.clone();
    }
    if !overlay.logo_small.is_keep() {
        base.logo_small = overlay.logo_small.clone();
    }
    if !overlay.name.is_keep() {
        base.name = overlay.name.clone();
    }
    if !overlay.parent_code.is_keep() {
        base.parent_code = overlay.parent_code.clone();
    }
    if !overlay.plot.is_keep() {
        base.plot = overlay.plot.clone();
    }
    if !overlay.rating.is_keep() {
        base.rating = overlay.rating;
    }
    if !overlay.rec.is_keep() {
        base.rec = overlay.rec.clone();
    }
    if !overlay.release_date.is_keep() {
        base.release_date = overlay.release_date.clone();
    }
    if !overlay.time_shift.is_keep() {
        base.time_shift = overlay.time_shift.clone();
    }
    if !overlay.title.is_keep() {
        base.title = overlay.title.clone();
    }
    if !overlay.tmdb.is_keep() {
        base.tmdb = overlay.tmdb;
    }
}

/// Checks whether any override files exist for `target` or any of its inputs.
pub async fn has_target_overrides(app_state: &Arc<AppState>, target: &ConfigTarget) -> bool {
    let repo = app_state.override_repository();
    if let Ok(target_path) = repo.file_path(&OverrideScope::Target(target.name.clone())) {
        if tokio::fs::try_exists(&target_path).await.unwrap_or(false) {
            return true;
        }
    }
    let sources = app_state.app_config.sources.load();
    let aliases = InputAliasMap::from_inputs(&sources.inputs);
    let parents = inputs_for_target(&app_state.app_config, &aliases, &target.name);
    for parent in parents {
        if let Ok(input_path) = repo.file_path(&OverrideScope::Input(parent)) {
            if tokio::fs::try_exists(&input_path).await.unwrap_or(false) {
                return true;
            }
        }
    }
    false
}

/// Checks whether an override file exists for `input`.
pub async fn has_input_overrides(app_state: &Arc<AppState>, input: &ConfigInput) -> bool {
    let repo = app_state.override_repository();
    let sources = app_state.app_config.sources.load();
    let aliases = InputAliasMap::from_inputs(&sources.inputs);
    let parent = parent_input_name(&aliases, &input.name);
    let Ok(input_path) = repo.file_path(&OverrideScope::Input(parent)) else {
        return false;
    };
    tokio::fs::try_exists(&input_path).await.unwrap_or(false)
}

#[derive(Debug, Clone)]
struct MergedOverrideEntry {
    ov: ItemOverride,
    pending: bool,
}

/// Merges saved override values into target playlist items with ONE lookup per item,
/// assigns `override_state` and computes `epg_state` for live items.
pub async fn apply_overlay_target(items: &mut [UiPlaylistItem], target: &ConfigTarget, app_state: &Arc<AppState>) {
    let sources = app_state.app_config.sources.load();
    let aliases = InputAliasMap::from_inputs(&sources.inputs);
    drop(sources);

    let parents = inputs_for_target(&app_state.app_config, &aliases, &target.name);
    let repo = app_state.override_repository();
    let status_store = app_state.override_status_store();

    let target_scope = OverrideScope::Target(target.name.clone());
    let target_file = repo.load(&target_scope).await.ok();
    let target_revision =
        target_file.as_ref().map_or_else(crate::repository::OverrideRevision::empty, |f| f.revision.clone());
    let target_pending = status_store.is_pending(&target.name, &target_scope, &target_revision).await;

    let mut merged_lookup: HashMap<(Arc<str>, OverrideKey), MergedOverrideEntry> = HashMap::new();

    // 1. Populate input overrides
    for parent in &parents {
        let input_scope = OverrideScope::Input(parent.clone());
        if let Ok(loaded) = repo.load(&input_scope).await {
            let input_pending = status_store.is_pending(&target.name, &input_scope, &loaded.revision).await;
            if let OverrideParseResult::Valid { file: ParsedOverrideFile::Input(file), .. } = &loaded.parsed {
                let parent_arc: Arc<str> = Arc::from(parent.as_str());
                for (kind, section) in [
                    (OverrideItemKind::Live, &file.sections.live),
                    (OverrideItemKind::Vod, &file.sections.vod),
                    (OverrideItemKind::Series, &file.sections.series),
                ] {
                    for (id, ov) in section {
                        let key = OverrideKey::new(kind, id.as_str());
                        merged_lookup.insert(
                            (Arc::clone(&parent_arc), key),
                            MergedOverrideEntry { ov: ov.clone(), pending: input_pending },
                        );
                    }
                }
            }
        }
    }

    // 2. Overlay target overrides
    if let Some(loaded) = &target_file {
        if let OverrideParseResult::Valid { file: ParsedOverrideFile::Target(file), .. } = &loaded.parsed {
            for (parent_name, sections) in &file.inputs {
                let parent_arc: Arc<str> = Arc::from(parent_name.as_str());
                for (kind, section) in [
                    (OverrideItemKind::Live, &sections.live),
                    (OverrideItemKind::Vod, &sections.vod),
                    (OverrideItemKind::Series, &sections.series),
                ] {
                    for (id, ov) in section {
                        let key = OverrideKey::new(kind, id.as_str());
                        merged_lookup
                            .entry((Arc::clone(&parent_arc), key))
                            .and_modify(|existing| {
                                merge_item_override(&mut existing.ov, ov);
                                existing.pending = existing.pending || target_pending;
                            })
                            .or_insert_with(|| MergedOverrideEntry { ov: ov.clone(), pending: target_pending });
                    }
                }
            }
        }
    }

    let epg_index = get_target_epg_status_index(app_state, target).await;
    let now = Utc::now().timestamp();

    apply_lookup_overlay(items, &aliases, &merged_lookup, &epg_index, now);
}

/// Merges saved override values into input playlist items with ONE lookup per item,
/// assigns `override_state` and computes `epg_state` for live items.
pub async fn apply_overlay_input(items: &mut [UiPlaylistItem], input: &ConfigInput, app_state: &Arc<AppState>) {
    let sources = app_state.app_config.sources.load();
    let aliases = InputAliasMap::from_inputs(&sources.inputs);
    drop(sources);

    let parent = parent_input_name(&aliases, &input.name);
    let repo = app_state.override_repository();
    let status_store = app_state.override_status_store();

    let input_scope = OverrideScope::Input(parent.clone());
    let input_file = repo.load(&input_scope).await.ok();
    let input_revision =
        input_file.as_ref().map_or_else(crate::repository::OverrideRevision::empty, |f| f.revision.clone());

    // "in input view, pending for any target"
    let targets = targets_for_input(&app_state.app_config, &aliases, &parent);
    let mut input_pending = false;
    for target_name in targets {
        if status_store.is_pending(&target_name, &input_scope, &input_revision).await {
            input_pending = true;
            break;
        }
    }

    let mut merged_lookup: HashMap<(Arc<str>, OverrideKey), MergedOverrideEntry> = HashMap::new();
    let parent_arc: Arc<str> = Arc::from(parent.as_str());

    if let Some(loaded) = &input_file {
        if let OverrideParseResult::Valid { file: ParsedOverrideFile::Input(file), .. } = &loaded.parsed {
            for (kind, section) in [
                (OverrideItemKind::Live, &file.sections.live),
                (OverrideItemKind::Vod, &file.sections.vod),
                (OverrideItemKind::Series, &file.sections.series),
            ] {
                for (id, ov) in section {
                    let key = OverrideKey::new(kind, id.as_str());
                    merged_lookup.insert(
                        (Arc::clone(&parent_arc), key),
                        MergedOverrideEntry { ov: ov.clone(), pending: input_pending },
                    );
                }
            }
        }
    }

    let epg_index = get_input_epg_status_index(app_state, input).await;
    let now = Utc::now().timestamp();

    apply_lookup_overlay(items, &aliases, &merged_lookup, &epg_index, now);
}

fn apply_lookup_overlay(
    items: &mut [UiPlaylistItem],
    aliases: &InputAliasMap,
    merged_lookup: &HashMap<(Arc<str>, OverrideKey), MergedOverrideEntry>,
    epg_index: &EpgStatusIndex,
    now: i64,
) {
    let mut parent_cache: HashMap<Arc<str>, Arc<str>> = HashMap::new();
    let mut get_parent = |input_name: &Arc<str>| -> Arc<str> {
        if let Some(parent) = parent_cache.get(input_name) {
            return Arc::clone(parent);
        }
        let parent_str = aliases.parent_of(input_name).unwrap_or(input_name.as_ref());
        let parent_arc: Arc<str> = Arc::from(parent_str);
        parent_cache.insert(Arc::clone(input_name), Arc::clone(&parent_arc));
        parent_arc
    };

    let parents_with_entries: HashSet<Arc<str>> = merged_lookup.keys().map(|(p, _)| Arc::clone(p)).collect();

    let mut key_counts: HashMap<(Arc<str>, OverrideKey), usize> = HashMap::new();
    let mut item_keys: Vec<Option<OverrideKey>> = Vec::with_capacity(items.len());

    for item in items.iter() {
        let parent = get_parent(&item.input_name);
        if parents_with_entries.contains(&parent) {
            let header = header_of_ui_item(item);
            if let Some(k) = override_key(&header, aliases) {
                if merged_lookup.contains_key(&(Arc::clone(&parent), k.clone())) {
                    *key_counts.entry((Arc::clone(&parent), k.clone())).or_insert(0) += 1;
                }
                item_keys.push(Some(k));
                continue;
            }
        }
        item_keys.push(None);
    }

    for (item, key_opt) in items.iter_mut().zip(item_keys) {
        let parent = get_parent(&item.input_name);
        let mut override_state = UiOverrideState { edited: false, pending: false, ambiguous: false };

        if let Some(key) = key_opt {
            item.provider_id = Arc::clone(&key.id);
            if let Some(merged) = merged_lookup.get(&(Arc::clone(&parent), key.clone())) {
                let is_ambiguous = key_counts.get(&(Arc::clone(&parent), key)).copied().unwrap_or(0) > 1;
                if is_ambiguous {
                    override_state = UiOverrideState { edited: true, pending: merged.pending, ambiguous: true };
                } else {
                    apply_override_to_ui_item(item, &merged.ov);
                    override_state = UiOverrideState { edited: true, pending: merged.pending, ambiguous: false };
                }
            }
        }

        let epg_state =
            if item.item_type.is_live() { Some(epg_index.status(item.epg_channel_id.as_deref(), now)) } else { None };

        item.explorer_meta = Some(UiItemExplorerMeta { override_state, epg_state });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::Internable;
    use shared::model::XtreamCluster;

    #[test]
    fn epg_status_four_cases() {
        let now = 1000i64;
        let mut max_stop = HashMap::new();
        max_stop.insert(Arc::from("past_only"), 900i64);
        max_stop.insert(Arc::from("no_programmes"), i64::MIN);
        max_stop.insert(Arc::from("future_programme"), 1200i64);
        let index = EpgStatusIndex::new(max_stop, EpgIdOutputCase::Preserve);

        // Case 1: MissingId (None or empty/whitespace)
        assert_eq!(index.status(None, now), ExplorerEpgState::MissingId);
        assert_eq!(index.status(Some(""), now), ExplorerEpgState::MissingId);
        assert_eq!(index.status(Some("   "), now), ExplorerEpgState::MissingId);

        // Case 2: NotInSources (id not known to index)
        assert_eq!(index.status(Some("unknown_channel"), now), ExplorerEpgState::NotInSources);

        // Case 3: NoProgramme (past-only programme or channel with no programmes)
        assert_eq!(index.status(Some("past_only"), now), ExplorerEpgState::NoProgramme);
        assert_eq!(index.status(Some("no_programmes"), now), ExplorerEpgState::NoProgramme);

        // Case 4: Ok (programme stop > now)
        assert_eq!(index.status(Some("future_programme"), now), ExplorerEpgState::Ok);
    }

    #[test]
    fn cached_index_status_changes_with_now() {
        let mut max_stop = HashMap::new();
        max_stop.insert(Arc::from("bbc.one"), 1500i64);
        let index = EpgStatusIndex::new(max_stop, EpgIdOutputCase::Preserve);

        // At now = 1000, 1500 > 1000 -> Ok
        assert_eq!(index.status(Some("bbc.one"), 1000), ExplorerEpgState::Ok);
        // At now = 1500, stop is not strictly greater than now -> NoProgramme
        assert_eq!(index.status(Some("bbc.one"), 1500), ExplorerEpgState::NoProgramme);
        // At now = 2000, 1500 < 2000 -> NoProgramme
        assert_eq!(index.status(Some("bbc.one"), 2000), ExplorerEpgState::NoProgramme);
    }

    #[test]
    fn status_index_case_folding_lowercase_ascii() {
        let mut max_stop = HashMap::new();
        max_stop.insert(Arc::from("daserste.de"), 2000i64);
        let index = EpgStatusIndex::new(max_stop, EpgIdOutputCase::LowercaseAscii);

        // Mixed/uppercase query matches lowercase database entry under LowercaseAscii
        assert_eq!(index.status(Some("DasErste.de"), 1000), ExplorerEpgState::Ok);
        assert_eq!(index.status(Some("DASERSTE.DE"), 1000), ExplorerEpgState::Ok);
    }

    #[test]
    fn overlay_shows_saved_value_and_pending() {
        let now = 1000i64;
        let item = UiPlaylistItem {
            virtual_id: 1,
            provider_id: "12345".intern(),
            name: "Original Name".intern(),
            title: "Original Title".intern(),
            group: "Original Group".intern(),
            logo: "http://example.com/logo.png".intern(),
            url: "http://example.com/live/1.m3u8".intern(),
            item_type: shared::model::PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            category_id: 1,
            rating: 0.0,
            input_name: "test_input".intern(),
            epg_channel_id: None,
            explorer_meta: None,
        };

        let mut items = vec![item];

        let input = ConfigInput { name: "test_input".intern(), ..Default::default() };
        let aliases = InputAliasMap::from_inputs(&[Arc::new(input)]);

        let ov = ItemOverride {
            title: FieldDelta::Set("Saved Title".to_string()),
            group: FieldDelta::Set("Saved Group".to_string()),
            ..Default::default()
        };

        let mut merged_lookup = HashMap::new();
        let key = OverrideKey::new(OverrideItemKind::Live, "12345");
        merged_lookup.insert((Arc::from("test_input"), key), MergedOverrideEntry { ov, pending: true });

        let epg_index = EpgStatusIndex::default();
        apply_lookup_overlay(&mut items, &aliases, &merged_lookup, &epg_index, now);

        assert_eq!(items[0].title.as_ref(), "Saved Title");
        assert_eq!(items[0].group.as_ref(), "Saved Group");
        assert_eq!(
            items[0].explorer_meta,
            Some(UiItemExplorerMeta {
                override_state: UiOverrideState { edited: true, pending: true, ambiguous: false },
                epg_state: Some(ExplorerEpgState::MissingId),
            })
        );
    }

    #[test]
    fn overlay_recomputes_epg_status() {
        let now = 1000i64;
        let mut max_stop = HashMap::new();
        max_stop.insert(Arc::from("bbc.one"), 2000i64);
        let epg_index = EpgStatusIndex::new(max_stop, EpgIdOutputCase::Preserve);

        let item = UiPlaylistItem {
            virtual_id: 1,
            provider_id: "12345".intern(),
            name: "BBC One".intern(),
            title: "BBC One".intern(),
            group: "UK".intern(),
            logo: "".intern(),
            url: "http://example.com/live/1.m3u8".intern(),
            item_type: shared::model::PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            category_id: 1,
            rating: 0.0,
            input_name: "test_input".intern(),
            epg_channel_id: None, // originally missing
            explorer_meta: None,
        };

        let mut items = vec![item];

        let input = ConfigInput { name: "test_input".intern(), ..Default::default() };
        let aliases = InputAliasMap::from_inputs(&[Arc::new(input)]);

        // Override provides bbc.one
        let ov = ItemOverride { epg_channel_id: FieldDelta::Set("bbc.one".to_string()), ..Default::default() };

        let mut merged_lookup = HashMap::new();
        let key = OverrideKey::new(OverrideItemKind::Live, "12345");
        merged_lookup.insert((Arc::from("test_input"), key), MergedOverrideEntry { ov, pending: false });

        apply_lookup_overlay(&mut items, &aliases, &merged_lookup, &epg_index, now);

        assert_eq!(items[0].epg_channel_id.as_deref(), Some("bbc.one"));
        assert_eq!(
            items[0].explorer_meta,
            Some(UiItemExplorerMeta {
                override_state: UiOverrideState { edited: true, pending: false, ambiguous: false },
                epg_state: Some(ExplorerEpgState::Ok),
            })
        );
    }

    #[test]
    fn overlay_ambiguous_saved_key_sets_edited_and_ambiguous() {
        let now = 1000i64;
        let item1 = UiPlaylistItem {
            virtual_id: 1,
            provider_id: "12345".intern(),
            name: "Item 1".intern(),
            title: "Item 1".intern(),
            group: "Group 1".intern(),
            logo: "".intern(),
            url: "http://example.com/live/1.m3u8".intern(),
            item_type: shared::model::PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            category_id: 1,
            rating: 0.0,
            input_name: "test_input".intern(),
            epg_channel_id: None,
            explorer_meta: None,
        };
        let item2 = UiPlaylistItem {
            virtual_id: 2,
            provider_id: "12345".intern(), // Duplicate key
            name: "Item 2".intern(),
            title: "Item 2".intern(),
            group: "Group 2".intern(),
            logo: "".intern(),
            url: "http://example.com/live/2.m3u8".intern(),
            item_type: shared::model::PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            category_id: 1,
            rating: 0.0,
            input_name: "test_input".intern(),
            epg_channel_id: None,
            explorer_meta: None,
        };

        let mut items = vec![item1, item2];

        let input = ConfigInput { name: "test_input".intern(), ..Default::default() };
        let aliases = InputAliasMap::from_inputs(&[Arc::new(input)]);

        let ov = ItemOverride { title: FieldDelta::Set("New Title".to_string()), ..Default::default() };

        let mut merged_lookup = HashMap::new();
        let key = OverrideKey::new(OverrideItemKind::Live, "12345");
        merged_lookup.insert((Arc::from("test_input"), key), MergedOverrideEntry { ov, pending: false });

        let epg_index = EpgStatusIndex::default();
        apply_lookup_overlay(&mut items, &aliases, &merged_lookup, &epg_index, now);

        // Ambiguous key with saved override -> edited: true, ambiguous: true, title not modified
        assert_eq!(items[0].title.as_ref(), "Item 1");
        assert_eq!(
            items[0].explorer_meta.as_ref().map(|m| m.override_state),
            Some(UiOverrideState { edited: true, pending: false, ambiguous: true })
        );
        assert_eq!(
            items[1].explorer_meta.as_ref().map(|m| m.override_state),
            Some(UiOverrideState { edited: true, pending: false, ambiguous: true })
        );
    }

    #[test]
    fn overlay_liveness_derived_from_item_type() {
        let now = 1000i64;
        let vod_item = UiPlaylistItem {
            virtual_id: 1,
            provider_id: "vod_1".intern(),
            name: "VOD Movie".intern(),
            title: "VOD Movie".intern(),
            group: "Movies".intern(),
            logo: "".intern(),
            url: "http://example.com/vod/1.mp4".intern(),
            item_type: shared::model::PlaylistItemType::Video,
            xtream_cluster: XtreamCluster::Live, // Even if cluster is Live (e.g. M3U fallback)
            category_id: 1,
            rating: 0.0,
            input_name: "test_input".intern(),
            epg_channel_id: None,
            explorer_meta: None,
        };

        let mut items = vec![vod_item];
        let aliases = InputAliasMap::default();
        let merged_lookup = HashMap::new();
        let epg_index = EpgStatusIndex::default();

        apply_lookup_overlay(&mut items, &aliases, &merged_lookup, &epg_index, now);

        // Since item_type is Video, epg_state MUST be None
        assert!(items[0].explorer_meta.as_ref().and_then(|m| m.epg_state).is_none());
    }

    #[test]
    fn no_override_files_response_unchanged() {
        let item = UiPlaylistItem {
            virtual_id: 1,
            provider_id: "p1".intern(),
            name: "Original Name".intern(),
            title: "Original Title".intern(),
            group: "Original Group".intern(),
            logo: "".intern(),
            url: "http://example.com/live/1.m3u8".intern(),
            item_type: shared::model::PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            category_id: 1,
            rating: 0.0,
            input_name: "input1".intern(),
            epg_channel_id: None,
            explorer_meta: None,
        };

        let json = serde_json::to_string(&item).expect("serialize item");
        // Serialization when explorer_meta is None must produce exactly 12 fields
        let value: serde_json::Value = serde_json::from_str(&json).expect("deserialize as JSON value");
        let arr = value.as_array().expect("array serialization");
        assert_eq!(arr.len(), 12);
    }
}
