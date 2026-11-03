use crate::fetched_playlist::FetchedPlaylist;
use parking_lot::Mutex;
use shared::model::{
    InputOverrideSections, ItemOverride, OverrideItemKind, PlaylistEntry, PlaylistGroup, PlaylistItem, UUIDType,
};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
};
use tuliprox_core::model::{
    apply_override, override_key, AppConfig, ConfigInput, ConfigTarget, InputAliasMap, OverrideKey, OverrideLookup,
};
use tuliprox_repository::{
    OverrideParseResult, OverrideRepository, OverrideRevision, OverrideScope, ParsedOverrideFile, SkippedOverrideFile,
};

#[derive(Debug, Default, Clone)]
pub struct InputOverrideSet {
    pub input: Arc<HashMap<Arc<str>, OverrideLookup>>,
    pub aliases: Arc<InputAliasMap>,
    pub revisions: Vec<(OverrideScope, OverrideRevision)>,
    pub applied_files: Vec<String>,
    pub skipped_files: Vec<SkippedOverrideFile>,
    pub exists: bool,
}

impl InputOverrideSet {
    #[must_use]
    pub fn empty(aliases: Option<InputAliasMap>) -> Self {
        Self {
            input: Arc::new(HashMap::new()),
            aliases: Arc::new(aliases.unwrap_or_default()),
            revisions: Vec::new(),
            applied_files: Vec::new(),
            skipped_files: Vec::new(),
            exists: false,
        }
    }

    pub async fn load(app_config: &AppConfig) -> Self {
        let paths = app_config.paths.load();
        let override_dir = tuliprox_core::utils::resolve_override_dir(&paths.config_path);
        let sources = app_config.sources.load();
        Self::load_from_dir(&override_dir, &sources.inputs).await
    }

    pub async fn load_from_dir(override_dir: &Path, inputs: &[Arc<ConfigInput>]) -> Self {
        match tokio::fs::try_exists(override_dir).await {
            Ok(true) => {}
            Ok(false) => return Self::empty(None),
            Err(err) => {
                log::warn!("Failed to check existence of override directory '{}': {err}", override_dir.display());
                return Self::empty(None);
            }
        }

        let aliases = Arc::new(InputAliasMap::from_inputs(inputs));
        let mut parent_names: HashSet<Arc<str>> = HashSet::new();
        for input in inputs {
            let parent = aliases.parent_of(input.name.as_ref()).unwrap_or(input.name.as_ref());
            parent_names.insert(Arc::from(parent));
        }

        let repo = OverrideRepository::new(override_dir);
        let mut existing_inputs = Vec::new();
        for parent in &parent_names {
            let input_scope = OverrideScope::Input(parent.to_string());
            let path = match repo.file_path(&input_scope) {
                Ok(p) => p,
                Err(err) => {
                    log::warn!("Invalid override file path for {input_scope:?}: {err}");
                    continue;
                }
            };
            match tokio::fs::try_exists(&path).await {
                Ok(true) => existing_inputs.push((Arc::clone(parent), input_scope, path)),
                Ok(false) => {}
                Err(err) => {
                    log::warn!(
                        "Failed to check existence of override file for {input_scope:?} at '{}': {err}",
                        path.display()
                    );
                }
            }
        }

        let mut revisions = Vec::new();
        let mut applied_files = Vec::new();
        let mut skipped_files = Vec::new();

        let input_map =
            load_input_overrides(&repo, existing_inputs, &mut revisions, &mut applied_files, &mut skipped_files).await;

        Self { input: Arc::new(input_map), aliases, revisions, applied_files, skipped_files, exists: true }
    }
}

#[derive(Debug)]
pub struct TargetOverrideSet {
    pub input: Arc<HashMap<Arc<str> /*parent*/, OverrideLookup>>,
    pub target: HashMap<Arc<str>, OverrideLookup>,
    pub aliases: Option<Arc<InputAliasMap>>,
    pub revisions: Vec<(OverrideScope, OverrideRevision)>,
    pub target_locked: Mutex<HashMap<UUIDType, Arc<ItemOverride>>>,
    pub applied_files: Vec<String>,
    pub skipped_files: Vec<SkippedOverrideFile>,
    pub ambiguous_keys: std::sync::atomic::AtomicUsize,
}

impl TargetOverrideSet {
    #[must_use]
    pub fn empty(aliases: Option<InputAliasMap>) -> Self {
        Self {
            input: Arc::new(HashMap::new()),
            target: HashMap::new(),
            aliases: aliases.map(Arc::new),
            revisions: Vec::new(),
            target_locked: Mutex::new(HashMap::new()),
            applied_files: Vec::new(),
            skipped_files: Vec::new(),
            ambiguous_keys: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    #[must_use]
    pub fn new(
        input: impl Into<Arc<HashMap<Arc<str>, OverrideLookup>>>,
        target: HashMap<Arc<str>, OverrideLookup>,
        aliases: impl Into<Option<InputAliasMap>>,
        revisions: Vec<(OverrideScope, OverrideRevision)>,
    ) -> Self {
        Self {
            input: input.into(),
            target,
            aliases: aliases.into().map(Arc::new),
            revisions,
            target_locked: Mutex::new(HashMap::new()),
            applied_files: Vec::new(),
            skipped_files: Vec::new(),
            ambiguous_keys: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    #[must_use]
    pub fn with_diagnostics(mut self, applied_files: Vec<String>, skipped_files: Vec<SkippedOverrideFile>) -> Self {
        self.applied_files = applied_files;
        self.skipped_files = skipped_files;
        self
    }

    pub fn record_ambiguous_keys(&self, count: usize) {
        self.ambiguous_keys.fetch_add(count, std::sync::atomic::Ordering::Relaxed);
    }

    #[must_use]
    pub fn ambiguous_keys_count(&self) -> usize { self.ambiguous_keys.load(std::sync::atomic::Ordering::Relaxed) }

    #[must_use]
    pub fn has_no_files(&self) -> bool { self.revisions.is_empty() && self.skipped_files.is_empty() }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.input.values().all(OverrideLookup::is_empty) && self.target.values().all(OverrideLookup::is_empty)
    }

    #[must_use]
    pub fn has_overrides_for(&self, runtime_input: &str) -> bool {
        if self.is_empty() {
            return false;
        }
        let parent = self.aliases.as_deref().and_then(|a| a.parent_of(runtime_input)).unwrap_or(runtime_input);
        self.input.get(parent).is_some_and(|l| !l.is_empty()) || self.target.get(parent).is_some_and(|l| !l.is_empty())
    }

    pub fn target_locked(&self) -> parking_lot::MutexGuard<'_, HashMap<UUIDType, Arc<ItemOverride>>> {
        self.target_locked.lock()
    }

    pub async fn load_for_target(
        input_overrides: &Arc<InputOverrideSet>,
        app_config: &AppConfig,
        target: &ConfigTarget,
        target_input_names: &[&str],
    ) -> Self {
        let paths = app_config.paths.load();
        let override_dir = tuliprox_core::utils::resolve_override_dir(&paths.config_path);
        Self::load_from_input_set(input_overrides, &override_dir, target, target_input_names).await
    }

    pub async fn load(app_config: &AppConfig, target: &ConfigTarget, playlists: &[FetchedPlaylist<'_>]) -> Self {
        let input_set = Arc::new(InputOverrideSet::load(app_config).await);
        let names: Vec<&str> = playlists.iter().map(|p| p.input.name.as_ref()).collect();
        Self::load_for_target(&input_set, app_config, target, &names).await
    }

    pub async fn load_from_dir(
        override_dir: &Path,
        target: &ConfigTarget,
        inputs: &[Arc<ConfigInput>],
        target_input_names: &[&str],
    ) -> Self {
        let input_set = Arc::new(InputOverrideSet::load_from_dir(override_dir, inputs).await);
        Self::load_from_input_set(&input_set, override_dir, target, target_input_names).await
    }

    async fn load_from_input_set(
        input_overrides: &InputOverrideSet,
        override_dir: &Path,
        target: &ConfigTarget,
        target_input_names: &[&str],
    ) -> Self {
        if !input_overrides.exists {
            return Self::empty(Some((*input_overrides.aliases).clone()));
        }

        let repo = OverrideRepository::new(override_dir);
        let target_scope = OverrideScope::Target(target.name.clone());
        let target_file_path = match repo.file_path(&target_scope) {
            Ok(path) => Some(path),
            Err(err) => {
                log::warn!("Invalid override file path for {target_scope:?}: {err}");
                None
            }
        };

        let target_file_exists = if let Some(ref path) = target_file_path {
            tokio::fs::try_exists(path).await.unwrap_or(false)
        } else {
            false
        };

        // Filter input revisions, applied_files, skipped_files and input map to the parent inputs the target consumes
        let mut consumed_parents: HashSet<Arc<str>> = HashSet::new();
        for name in target_input_names {
            let parent = input_overrides.aliases.parent_of(name).unwrap_or(name);
            consumed_parents.insert(Arc::from(parent));
        }

        let mut expected_file_names: HashSet<String> = HashSet::new();
        for parent in &consumed_parents {
            let scope = OverrideScope::Input(parent.to_string());
            if let Ok(path) = repo.file_path(&scope) {
                if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
                    expected_file_names.insert(file_name.to_string());
                }
            }
        }

        let mut revisions: Vec<(OverrideScope, OverrideRevision)> = input_overrides
            .revisions
            .iter()
            .filter(|(scope, _)| match scope {
                OverrideScope::Input(parent) => consumed_parents.contains(parent.as_str()),
                OverrideScope::Target(_) => true,
            })
            .cloned()
            .collect();

        let mut applied_files: Vec<String> = input_overrides
            .applied_files
            .iter()
            .filter(|name| expected_file_names.contains(name.as_str()))
            .cloned()
            .collect();

        let mut skipped_files: Vec<SkippedOverrideFile> = input_overrides
            .skipped_files
            .iter()
            .filter(|skipped| expected_file_names.contains(&skipped.name))
            .cloned()
            .collect();

        let mut input_map = HashMap::new();
        for (parent, lookup) in input_overrides.input.iter() {
            if consumed_parents.contains(parent) {
                input_map.insert(Arc::clone(parent), lookup.clone());
            }
        }

        let target_map = if let (true, Some(target_path)) = (target_file_exists, target_file_path.as_deref()) {
            load_target_override(
                &repo,
                target_scope,
                target_path,
                &input_overrides.aliases,
                &mut revisions,
                &mut applied_files,
                &mut skipped_files,
            )
            .await
        } else {
            HashMap::new()
        };

        Self {
            input: Arc::new(input_map),
            target: target_map,
            aliases: Some(Arc::clone(&input_overrides.aliases)),
            revisions,
            target_locked: Mutex::new(HashMap::new()),
            applied_files,
            skipped_files,
            ambiguous_keys: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

async fn load_input_overrides(
    repo: &OverrideRepository,
    existing_inputs: Vec<(Arc<str>, OverrideScope, std::path::PathBuf)>,
    revisions: &mut Vec<(OverrideScope, OverrideRevision)>,
    applied_files: &mut Vec<String>,
    skipped_files: &mut Vec<SkippedOverrideFile>,
) -> HashMap<Arc<str>, OverrideLookup> {
    let mut input_map = HashMap::with_capacity(existing_inputs.len());
    for (parent_name, input_scope, path) in existing_inputs {
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        match repo.load(&input_scope).await {
            Ok(loaded) => match loaded.parsed {
                OverrideParseResult::Malformed { error } => {
                    log::warn!("Override file '{}' is malformed: {error}", path.display());
                    skipped_files.push(SkippedOverrideFile::malformed(file_name));
                }
                OverrideParseResult::UnsupportedSchema { version } => {
                    log::warn!("Override file '{}' has unsupported schema version: {version}", path.display());
                    skipped_files.push(SkippedOverrideFile::unsupported(file_name));
                }
                OverrideParseResult::Valid { file, field_errors } => {
                    if !field_errors.is_empty() {
                        log::warn!(
                            "Override file '{}' has {} field validation error(s)",
                            path.display(),
                            field_errors.len()
                        );
                    }
                    if let ParsedOverrideFile::Input(input_file) = file {
                        let lookup = sections_to_lookup(input_file.sections);
                        if !lookup.is_empty() {
                            input_map.insert(parent_name, lookup);
                        }
                    }
                    applied_files.push(file_name);
                    revisions.push((input_scope, loaded.revision));
                }
            },
            Err(err) => {
                log::warn!("Failed to read override file '{}': {err}", path.display());
            }
        }
    }
    input_map
}

async fn load_target_override(
    repo: &OverrideRepository,
    target_scope: OverrideScope,
    target_path: &Path,
    aliases: &InputAliasMap,
    revisions: &mut Vec<(OverrideScope, OverrideRevision)>,
    applied_files: &mut Vec<String>,
    skipped_files: &mut Vec<SkippedOverrideFile>,
) -> HashMap<Arc<str>, OverrideLookup> {
    let mut target_map: HashMap<Arc<str>, OverrideLookup> = HashMap::new();
    let file_name = target_path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
    match repo.load(&target_scope).await {
        Ok(loaded) => match loaded.parsed {
            OverrideParseResult::Malformed { error } => {
                log::warn!("Override file '{}' is malformed: {error}", target_path.display());
                skipped_files.push(SkippedOverrideFile::malformed(file_name));
            }
            OverrideParseResult::UnsupportedSchema { version } => {
                log::warn!("Override file '{}' has unsupported schema version: {version}", target_path.display());
                skipped_files.push(SkippedOverrideFile::unsupported(file_name));
            }
            OverrideParseResult::Valid { file, field_errors } => {
                if !field_errors.is_empty() {
                    log::warn!(
                        "Override file '{}' has {} field validation error(s)",
                        target_path.display(),
                        field_errors.len()
                    );
                }
                if let ParsedOverrideFile::Target(target_file) = file {
                    for (input_key, sections) in target_file.inputs {
                        let parent = aliases.parent_of(&input_key).unwrap_or(&input_key);
                        let parent_arc: Arc<str> = Arc::from(parent);
                        let lookup = sections_to_lookup(sections);
                        let entry = target_map.entry(parent_arc).or_default();
                        for (key, item) in lookup.map {
                            if entry.map.insert(key.clone(), item).is_some() {
                                log::warn!(
                                    "Target override for target '{}' defines duplicate key {:?} for parent input '{parent}' (defined under multiple input sections)",
                                    target_scope.name(),
                                    key
                                );
                            }
                        }
                    }
                }
                applied_files.push(file_name);
                revisions.push((target_scope, loaded.revision));
            }
        },
        Err(err) => {
            log::warn!("Failed to read override file '{}': {err}", target_path.display());
        }
    }
    target_map
}

fn sections_to_lookup(mut sections: InputOverrideSections) -> OverrideLookup {
    sections.prune_empty();
    let total = sections.live.len() + sections.vod.len() + sections.series.len();
    let mut map = HashMap::with_capacity(total);

    for (id, item) in sections.live {
        map.insert(OverrideKey::new(OverrideItemKind::Live, id), Arc::new(item));
    }
    for (id, item) in sections.vod {
        map.insert(OverrideKey::new(OverrideItemKind::Vod, id), Arc::new(item));
    }
    for (id, item) in sections.series {
        map.insert(OverrideKey::new(OverrideItemKind::Series, id), Arc::new(item));
    }

    OverrideLookup::new(map)
}

#[derive(Debug, Default, Clone)]
pub struct OverrideApplyOutcome {
    pub applied: usize,
    pub ambiguous: Vec<OverrideKey>,
    pub target_locked: HashMap<UUIDType, Arc<ItemOverride>>,
}

#[inline]
pub fn resolve_item_uuid(item: &PlaylistItem) -> Option<UUIDType> {
    if item.header.uuid == UUIDType::default() {
        let fallback = item.get_uuid();
        if fallback == UUIDType::default() {
            log::debug!("Item with nil uuid cannot be locked: id={}", item.header.id);
            None
        } else {
            Some(fallback)
        }
    } else {
        Some(item.header.uuid)
    }
}

#[inline]
pub fn resolve_item_uuid_from_parts(stored_uuid: UUIDType, fallback_uuid: UUIDType, id: &str) -> Option<UUIDType> {
    if stored_uuid != UUIDType::default() {
        Some(stored_uuid)
    } else if fallback_uuid != UUIDType::default() {
        Some(fallback_uuid)
    } else {
        log::debug!("Item with nil uuid cannot be locked: id={id}");
        None
    }
}

pub fn apply_overrides(items: &mut [PlaylistItem], set: &TargetOverrideSet) -> OverrideApplyOutcome {
    if set.is_empty() || items.is_empty() {
        return OverrideApplyOutcome::default();
    }

    let default_aliases = InputAliasMap::default();
    let aliases: &InputAliasMap = set.aliases.as_deref().unwrap_or(&default_aliases);

    let mut item_keys: Vec<Option<OverrideKey>> = Vec::with_capacity(items.len());
    let mut key_counts: HashMap<(&str, OverrideKey), (usize, usize)> = HashMap::new();

    for (idx, item) in items.iter().enumerate() {
        let runtime_input = item.header.input_name.as_ref();
        let parent_str = aliases.parent_of(runtime_input).unwrap_or(runtime_input);

        let input_lookup = set.input.get(parent_str);
        let target_lookup = set.target.get(parent_str);

        if input_lookup.is_some_and(|l| !l.is_empty()) || target_lookup.is_some_and(|l| !l.is_empty()) {
            if let Some(key) = override_key(&item.header, aliases) {
                let has_ov = input_lookup.is_some_and(|l| l.get(&key).is_some())
                    || target_lookup.is_some_and(|l| l.get(&key).is_some());
                if has_ov {
                    match key_counts.entry((parent_str, key.clone())) {
                        std::collections::hash_map::Entry::Vacant(e) => {
                            e.insert((1, idx));
                            item_keys.push(Some(key));
                        }
                        std::collections::hash_map::Entry::Occupied(mut e) => {
                            let (count, first_idx) = e.get_mut();
                            *count += 1;
                            item_keys[*first_idx] = None;
                            item_keys.push(None);
                        }
                    }
                    continue;
                }
            }
        }
        item_keys.push(None);
    }

    let mut ambiguous: Vec<OverrideKey> = Vec::new();
    for ((parent_str, key), (count, _)) in &key_counts {
        if *count > 1 {
            log::warn!(
                "Ambiguous override key for parent '{parent_str}': {key:?} occurs {count} times, skipping override"
            );
            ambiguous.push(key.clone());
        }
    }
    drop(key_counts);

    let mut applied = 0;
    let mut target_locked = HashMap::new();

    for (item, key_opt) in items.iter_mut().zip(item_keys) {
        let Some(key) = key_opt else {
            continue;
        };

        let runtime_input = item.header.input_name.as_ref();
        let parent_str = aliases.parent_of(runtime_input).unwrap_or(runtime_input);

        let input_ov = set.input.get(parent_str).and_then(|l| l.get(&key));
        let target_ov = set.target.get(parent_str).and_then(|l| l.get(&key));

        if input_ov.is_none() && target_ov.is_none() {
            continue;
        }

        applied += 1;

        if let Some(ov) = input_ov {
            apply_override(&mut item.header, ov);
        }

        if let Some(ov) = target_ov {
            apply_override(&mut item.header, ov);
            if let Some(uuid) = resolve_item_uuid(item) {
                target_locked.insert(uuid, Arc::clone(ov));
            }
        }
    }

    OverrideApplyOutcome { applied, ambiguous, target_locked }
}

pub fn reapply_target_overrides<'a, S: std::hash::BuildHasher>(
    items: impl Iterator<Item = &'a mut PlaylistItem>,
    locked: &HashMap<UUIDType, Arc<ItemOverride>, S>,
) -> bool {
    reapply_target_overrides_ext(items, locked, false)
}

pub fn reapply_target_overrides_skip_chno<'a, S: std::hash::BuildHasher>(
    items: impl Iterator<Item = &'a mut PlaylistItem>,
    locked: &HashMap<UUIDType, Arc<ItemOverride>, S>,
) -> bool {
    reapply_target_overrides_ext(items, locked, true)
}

pub fn reapply_target_overrides_ext<'a, S: std::hash::BuildHasher>(
    items: impl Iterator<Item = &'a mut PlaylistItem>,
    locked: &HashMap<UUIDType, Arc<ItemOverride>, S>,
    skip_chno: bool,
) -> bool {
    if locked.is_empty() {
        return false;
    }

    let mut group_restored = false;
    for item in items {
        if let Some(uuid) = resolve_item_uuid(item) {
            if let Some(ov) = locked.get(&uuid) {
                let old_group = if ov.group.is_keep() { None } else { Some(Arc::clone(&item.header.group)) };

                if skip_chno {
                    tuliprox_core::model::apply_override_skip_chno(&mut item.header, ov);
                } else {
                    apply_override(&mut item.header, ov);
                }

                if let Some(old) = old_group {
                    if old != item.header.group {
                        group_restored = true;
                    }
                }
            }
        }
    }

    group_restored
}

pub fn regroup_moved_items<S: std::hash::BuildHasher>(
    groups: &mut Vec<PlaylistGroup>,
    locked: &HashMap<UUIDType, Arc<ItemOverride>, S>,
) {
    if locked.is_empty() {
        return;
    }

    let mut moved: Vec<PlaylistItem> = Vec::new();
    let mut emptied_group_ids: HashSet<u32> = HashSet::new();

    for group in groups.iter_mut() {
        let group_title = Arc::clone(&group.title);
        if !group.channels.iter().any(|item| {
            resolve_item_uuid(item).is_some_and(|uuid| locked.contains_key(&uuid)) && item.header.group != group_title
        }) {
            continue;
        }
        let had_items = !group.channels.is_empty();
        let mut kept = Vec::with_capacity(group.channels.len());
        for item in group.channels.drain(..) {
            if resolve_item_uuid(&item).is_some_and(|uuid| locked.contains_key(&uuid))
                && item.header.group != group_title
            {
                moved.push(item);
            } else {
                kept.push(item);
            }
        }
        if had_items && kept.is_empty() {
            emptied_group_ids.insert(group.id);
        }
        group.channels = kept;
    }

    if moved.is_empty() {
        return;
    }

    let mut next_id = groups.iter().map(|g| g.id).max().unwrap_or(0) + 1;

    for item in moved {
        let target_title = Arc::clone(&item.header.group);
        let cluster = item.header.xtream_cluster;

        if let Some(target_group) = groups.iter_mut().find(|g| g.xtream_cluster == cluster && g.title == target_title) {
            target_group.channels.push(item);
        } else {
            let id = next_id;
            next_id += 1;
            groups.push(PlaylistGroup { id, title: target_title, channels: vec![item], xtream_cluster: cluster });
        }
    }

    groups.retain(|g| !emptied_group_ids.contains(&g.id) || !g.channels.is_empty());
}
