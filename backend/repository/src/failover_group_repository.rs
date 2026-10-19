use crate::{
    bplustree::{BPlusTree, BPlusTreeError, BPlusTreeQuery},
    storage_const::{
        FILE_FAILOVER_GROUPS, FILE_FAILOVER_GROUPS_ALIAS, FILE_FAILOVER_GROUPS_CATALOG, FILE_FAILOVER_GROUPS_MEMBER,
        FILE_FAILOVER_GROUPS_META,
    },
};
pub use shared::model::FailoverGroupOrigin;
use shared::model::{
    ChannelMemberRefDto, FailoverGroupPolicy, PlaylistItem, PlaylistItemType, StreamProperties,
    TargetFailoverOverridesDto, UUIDType, VirtualId, XtreamCluster,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tuliprox_core::{
    model::{AppConfig, ConfigTarget},
    utils::remove_file_if_exists,
};

#[derive(Debug, Clone)]
pub struct PendingFailoverGroup {
    pub group_uuid: UUIDType,
    pub key: Arc<str>,
    pub origin: FailoverGroupOrigin,
    pub policy: FailoverGroupPolicy,
    pub members: Vec<PlaylistItem>,
    pub orphaned: Vec<ChannelMemberRefDto>,
    pub epg_split: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MemberArchive {
    pub duration_days: u32,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GroupMember {
    pub member_vid: VirtualId,
    pub input_name: Arc<str>,
    pub provider_id: u32,
    pub input_stream_id: Arc<str>,
    pub url: Arc<str>,
    pub item_type: PlaylistItemType,
    pub cluster: XtreamCluster,
    pub title: Arc<str>,
    pub archive: Option<MemberArchive>,
    pub epg_channel_id: Option<Arc<str>>,
    pub additional_properties: Option<StreamProperties>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FailoverGroupRecord {
    pub group_vid: VirtualId,
    pub key: Arc<str>,
    pub origin: FailoverGroupOrigin,
    pub policy: FailoverGroupPolicy,
    pub members: Vec<GroupMember>,
    pub orphaned: Vec<ChannelMemberRefDto>,
}

impl FailoverGroupRecord {
    /// Virtual id of the stored member that `member` refers to.
    #[must_use]
    pub fn member_vid(&self, member: &ChannelMemberRefDto) -> Option<VirtualId> {
        self.members
            .iter()
            .find(|stored| {
                stored.input_name.as_ref() == member.input.as_str()
                    && stored.input_stream_id.as_ref() == member.stream_id.as_str()
            })
            .map(|stored| stored.member_vid)
    }
}

/// Eligible live channel of the pre-grouping catalog, in playlist order. The quality of a channel
/// is not stored: it comes from the member health index at read time.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CatalogRecord {
    pub member_ref: ChannelMemberRefDto,
    pub natural_key: Option<Arc<str>>,
    pub dsl_forced: bool,
    pub epg_channel_id: Option<Arc<str>>,
    /// Raw `header.name`, the source of the group name (processing derives it from here).
    pub name: Arc<str>,
    /// Display title, `header.get_name()`.
    pub title: Arc<str>,
    pub logo: Arc<str>,
    pub playlist_group: Arc<str>,
    pub provider_id: u32,
    pub has_archive: bool,
}

/// What the run that published the failover groups applied from the failover files.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FailoverGroupMeta {
    /// Etag of the override section used by the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrides_etag: Option<Arc<str>>,
    /// Load error of the failover files, in which case no overrides were applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrides_error: Option<String>,
    /// Canonicalized override section the run applied, `None` without a section.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_overrides: Option<TargetFailoverOverridesDto>,
    /// Keys of groups whose representative changed by a hot apply after the run: the published playlist item
    /// still shows the old one. A playlist update writes a fresh meta, which clears the list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub display_stale_groups: Vec<String>,
}

/// New member order and policy of one persisted group, applied without a playlist update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotApplyGroupChange {
    pub group_vid: VirtualId,
    /// Every member of the group, in the new order.
    pub member_order: Vec<VirtualId>,
    pub policy: FailoverGroupPolicy,
}

/// Everything a playlist publication hands to the failover group store.
/// `Default` is a target without failover groups: nothing is written, stale files are removed.
#[derive(Debug, Clone, Default)]
pub struct FailoverGroupBatch {
    pub groups: Vec<PendingFailoverGroup>,
    pub catalog: Vec<CatalogRecord>,
    pub meta: FailoverGroupMeta,
    pub enabled: bool,
}

impl FailoverGroupBatch {
    #[must_use]
    pub fn with_groups(groups: Vec<PendingFailoverGroup>) -> Self { Self { groups, enabled: true, ..Self::default() } }

    /// Catalog and metadata to publish, `None` for a target without failover groups.
    #[must_use]
    pub fn side_files(&self) -> Option<FailoverGroupSideFiles<'_>> {
        self.enabled.then_some(FailoverGroupSideFiles { catalog: &self.catalog, meta: &self.meta })
    }
}

/// Catalog and metadata of a publication, written whenever failover groups are enabled for the target.
#[derive(Clone, Copy)]
pub struct FailoverGroupSideFiles<'a> {
    pub catalog: &'a [CatalogRecord],
    pub meta: &'a FailoverGroupMeta,
}

const PUBLISHED_FILES: [&str; 5] = [
    FILE_FAILOVER_GROUPS,
    FILE_FAILOVER_GROUPS_MEMBER,
    FILE_FAILOVER_GROUPS_ALIAS,
    FILE_FAILOVER_GROUPS_CATALOG,
    FILE_FAILOVER_GROUPS_META,
];

/// Staged files of a publication, one slot per entry of [`PUBLISHED_FILES`]. A slot without a staged
/// file removes the published file on `publish`. Staged files that are not published are removed on drop.
pub struct FailoverGroupStagedPublication {
    target_path: PathBuf,
    staged: [Option<PathBuf>; PUBLISHED_FILES.len()],
}

impl FailoverGroupStagedPublication {
    const GROUPS: usize = 0;
    const MEMBERS: usize = 1;
    const ALIASES: usize = 2;
    const CATALOG: usize = 3;
    const META: usize = 4;

    fn new(target_path: &Path) -> Self { Self { target_path: target_path.to_path_buf(), staged: Default::default() } }

    /// Registers the staged path of `slot` before it is written, so a failed write is cleaned up on drop.
    fn stage(&mut self, slot: usize, stage_suffix: &str) -> PathBuf {
        let path = self.target_path.join(format!("{}{stage_suffix}", PUBLISHED_FILES[slot]));
        self.staged[slot] = Some(path.clone());
        path
    }

    pub fn discard(self) { drop(self); }

    pub fn publish(mut self) -> std::io::Result<()> {
        for (slot, name) in self.staged.iter_mut().zip(PUBLISHED_FILES) {
            let published = self.target_path.join(name);
            match slot.take() {
                Some(staged) => std::fs::rename(&staged, published).inspect_err(|_| {
                    let _ = remove_file_if_exists(&staged);
                })?,
                None => {
                    let _ = remove_file_if_exists(&published);
                }
            }
        }
        Ok(())
    }
}

impl Drop for FailoverGroupStagedPublication {
    fn drop(&mut self) {
        for staged in self.staged.iter().flatten() {
            let _ = remove_file_if_exists(staged);
        }
    }
}

pub struct FailoverGroupStore;

impl FailoverGroupStore {
    pub fn write_staged(
        target_path: &Path,
        records: &[FailoverGroupRecord],
        aliases: &[(VirtualId, VirtualId)],
        side_files: Option<FailoverGroupSideFiles<'_>>,
    ) -> std::io::Result<FailoverGroupStagedPublication> {
        let mut publication = FailoverGroupStagedPublication::new(target_path);
        let stage_suffix = format!(".stage-{}", uuid::Uuid::new_v4());

        if !records.is_empty() {
            let groups_path = publication.stage(FailoverGroupStagedPublication::GROUPS, &stage_suffix);
            let mut groups_tree = BPlusTree::<VirtualId, FailoverGroupRecord>::new();
            for record in records {
                groups_tree.insert(record.group_vid, record.clone());
            }
            groups_tree.store(&groups_path)?;

            let members_path = publication.stage(FailoverGroupStagedPublication::MEMBERS, &stage_suffix);
            let mut members_tree = BPlusTree::<VirtualId, VirtualId>::new();
            for record in records {
                for member in &record.members {
                    members_tree.insert(member.member_vid, record.group_vid);
                }
            }
            members_tree.store(&members_path)?;

            if !aliases.is_empty() {
                let aliases_path = publication.stage(FailoverGroupStagedPublication::ALIASES, &stage_suffix);
                let mut aliases_tree = BPlusTree::<VirtualId, VirtualId>::new();
                for (alias_vid, group_vid) in aliases {
                    aliases_tree.insert(*alias_vid, *group_vid);
                }
                aliases_tree.store(&aliases_path)?;
            }
        }

        if let Some(side_files) = side_files {
            let catalog_path = publication.stage(FailoverGroupStagedPublication::CATALOG, &stage_suffix);
            let mut catalog_tree = BPlusTree::<u32, CatalogRecord>::new();
            for (order, record) in (0u32..).zip(side_files.catalog) {
                catalog_tree.insert(order, record.clone());
            }
            catalog_tree.store(&catalog_path)?;

            let meta_path = publication.stage(FailoverGroupStagedPublication::META, &stage_suffix);
            std::fs::write(&meta_path, serde_json::to_vec(side_files.meta).map_err(std::io::Error::other)?)?;
        }

        Ok(publication)
    }

    pub fn write(
        target_path: &Path,
        records: &[FailoverGroupRecord],
        aliases: &[(VirtualId, VirtualId)],
        side_files: Option<FailoverGroupSideFiles<'_>>,
    ) -> std::io::Result<()> {
        Self::write_staged(target_path, records, aliases, side_files)?.publish()
    }

    /// Rewrites member order and policy of the given groups in place. The member and alias indexes stay
    /// valid because the member set of a group does not change. The caller holds the target lock.
    pub fn update_order_and_policy(target_path: &Path, changes: &[HotApplyGroupChange]) -> std::io::Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        let invalid = |msg: String| std::io::Error::new(std::io::ErrorKind::InvalidData, msg);
        let mut records = Self::read_all_groups(target_path)?;
        let positions: HashMap<VirtualId, usize> =
            records.iter().enumerate().map(|(pos, record)| (record.group_vid, pos)).collect();

        for change in changes {
            let record = positions
                .get(&change.group_vid)
                .and_then(|&pos| records.get_mut(pos))
                .ok_or_else(|| invalid(format!("failover group {} is not stored", change.group_vid)))?;
            let mut remaining: HashMap<VirtualId, GroupMember> =
                record.members.drain(..).map(|member| (member.member_vid, member)).collect();
            let reordered = change
                .member_order
                .iter()
                .map(|member_vid| {
                    remaining.remove(member_vid).ok_or_else(|| {
                        invalid(format!("member {member_vid} is not part of failover group {}", change.group_vid))
                    })
                })
                .collect::<std::io::Result<Vec<_>>>()?;
            if !remaining.is_empty() {
                return Err(invalid(format!("member order of failover group {} is incomplete", change.group_vid)));
            }
            record.members = reordered;
            record.policy = change.policy;
        }

        let mut groups_tree = BPlusTree::<VirtualId, FailoverGroupRecord>::new();
        for record in records {
            groups_tree.insert(record.group_vid, record);
        }
        Self::replace_file(&target_path.join(FILE_FAILOVER_GROUPS), |staged| groups_tree.store(staged).map(drop))
    }

    /// Applies `changes` and records `meta`, so that a failure in between never leaves groups that are
    /// newer than the metadata describing them: the metadata is invalidated (no etag, no applied section)
    /// before the groups are rewritten and replaced by `meta` afterwards. The caller holds the target lock.
    pub fn apply_hot_change(
        target_path: &Path,
        changes: &[HotApplyGroupChange],
        meta: &FailoverGroupMeta,
    ) -> std::io::Result<()> {
        Self::apply_hot_change_with(target_path, changes, meta, Self::write_meta)
    }

    fn apply_hot_change_with(
        target_path: &Path,
        changes: &[HotApplyGroupChange],
        meta: &FailoverGroupMeta,
        write_final_meta: impl FnOnce(&Path, &FailoverGroupMeta) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        if !changes.is_empty() {
            let invalidated = FailoverGroupMeta {
                overrides_etag: None,
                overrides_error: meta.overrides_error.clone(),
                applied_overrides: None,
                display_stale_groups: meta.display_stale_groups.clone(),
            };
            Self::write_meta(target_path, &invalidated)?;
            Self::update_order_and_policy(target_path, changes)?;
        }
        write_final_meta(target_path, meta)
    }

    /// Replaces the publication metadata. The caller holds the target lock.
    pub fn write_meta(target_path: &Path, meta: &FailoverGroupMeta) -> std::io::Result<()> {
        let bytes = serde_json::to_vec(meta).map_err(std::io::Error::other)?;
        Self::replace_file(&target_path.join(FILE_FAILOVER_GROUPS_META), |staged| std::fs::write(staged, &bytes))
    }

    /// Writes `path` through a staged sibling and renames it over, so readers never see a partial file.
    fn replace_file(path: &Path, write: impl FnOnce(&Path) -> std::io::Result<()>) -> std::io::Result<()> {
        let mut staged_name = path.as_os_str().to_owned();
        staged_name.push(format!(".stage-{}", uuid::Uuid::new_v4()));
        let staged = PathBuf::from(staged_name);
        write(&staged).and_then(|()| std::fs::rename(&staged, path)).inspect_err(|_| {
            let _ = remove_file_if_exists(&staged);
        })
    }

    pub fn delete(target_path: &Path) -> std::io::Result<()> {
        for name in PUBLISHED_FILES {
            let _ = remove_file_if_exists(&target_path.join(name));
        }
        Ok(())
    }

    pub fn read_alias(target_path: &Path, vid: VirtualId) -> std::io::Result<Option<VirtualId>> {
        let db_path = target_path.join(FILE_FAILOVER_GROUPS_ALIAS);
        if !db_path.exists() {
            return Ok(None);
        }
        let mut query = match BPlusTreeQuery::<VirtualId, VirtualId>::try_new(&db_path) {
            Ok(q) => q,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        query.query(&vid).map_err(BPlusTreeError::to_io)
    }

    pub fn read_group(target_path: &Path, group_vid: VirtualId) -> std::io::Result<Option<FailoverGroupRecord>> {
        let db_path = target_path.join(FILE_FAILOVER_GROUPS);
        if !db_path.exists() {
            return Ok(None);
        }
        let mut query = match BPlusTreeQuery::<VirtualId, FailoverGroupRecord>::try_new(&db_path) {
            Ok(q) => q,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        query.query(&group_vid).map_err(BPlusTreeError::to_io)
    }

    pub fn read_member(
        target_path: &Path,
        member_vid: VirtualId,
    ) -> std::io::Result<Option<(FailoverGroupRecord, usize)>> {
        let db_path = target_path.join(FILE_FAILOVER_GROUPS_MEMBER);
        if !db_path.exists() {
            return Ok(None);
        }
        let mut member_query = match BPlusTreeQuery::<VirtualId, VirtualId>::try_new(&db_path) {
            Ok(q) => q,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        let Some(group_vid) = member_query.query(&member_vid).map_err(BPlusTreeError::to_io)? else {
            return Ok(None);
        };
        drop(member_query);
        let Some(record) = Self::read_group(target_path, group_vid)? else {
            return Ok(None);
        };
        let Some(idx) = record.members.iter().position(|m| m.member_vid == member_vid) else {
            return Ok(None);
        };
        Ok(Some((record, idx)))
    }

    pub fn read_all_groups(target_path: &Path) -> std::io::Result<Vec<FailoverGroupRecord>> {
        let db_path = target_path.join(FILE_FAILOVER_GROUPS);
        if !db_path.exists() {
            return Ok(Vec::new());
        }
        let mut query = match BPlusTreeQuery::<VirtualId, FailoverGroupRecord>::try_new(&db_path) {
            Ok(q) => q,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        let mut records = Vec::new();
        for entry in query.iter() {
            let (_key, record) = entry?;
            records.push(record);
        }
        Ok(records)
    }

    pub fn read_all_aliases(target_path: &Path) -> std::io::Result<HashMap<VirtualId, VirtualId>> {
        let db_path = target_path.join(FILE_FAILOVER_GROUPS_ALIAS);
        if !db_path.exists() {
            return Ok(HashMap::new());
        }
        let mut query = match BPlusTreeQuery::<VirtualId, VirtualId>::try_new(&db_path) {
            Ok(q) => q,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
            Err(err) => return Err(err),
        };
        let mut aliases = HashMap::new();
        for entry in query.iter() {
            let (alias_vid, group_vid) = entry?;
            aliases.insert(alias_vid, group_vid);
        }
        Ok(aliases)
    }

    /// All catalog records in playlist order; empty when the target has no catalog.
    pub fn read_catalog(target_path: &Path) -> std::io::Result<Vec<CatalogRecord>> {
        let db_path = target_path.join(FILE_FAILOVER_GROUPS_CATALOG);
        if !db_path.exists() {
            return Ok(Vec::new());
        }
        let mut query = match BPlusTreeQuery::<u32, CatalogRecord>::try_new(&db_path) {
            Ok(q) => q,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        let mut records = Vec::new();
        for entry in query.iter() {
            let (_order, record) = entry?;
            records.push(record);
        }
        Ok(records)
    }

    /// Publication metadata; default when the target has none.
    pub fn read_meta(target_path: &Path) -> std::io::Result<FailoverGroupMeta> {
        match std::fs::read(target_path.join(FILE_FAILOVER_GROUPS_META)) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(FailoverGroupMeta::default()),
            Err(err) => Err(err),
        }
    }

    pub fn load_target_data(target_path: &Path, generation: u64) -> std::io::Result<FailoverGroupTargetData> {
        let records = Self::read_all_groups(target_path)?;
        let aliases = Self::read_all_aliases(target_path)?;
        let meta = Self::read_meta(target_path)?;
        let mut groups = HashMap::with_capacity(records.len());
        let mut members = HashMap::new();
        for record in records {
            let group_vid = record.group_vid;
            for (idx, member) in record.members.iter().enumerate() {
                members.insert(member.member_vid, (group_vid, idx));
            }
            groups.insert(group_vid, Arc::new(record));
        }
        Ok(FailoverGroupTargetData { generation, groups, members, aliases, meta })
    }
}

#[derive(Debug, Clone, Default)]
pub struct FailoverGroupTargetData {
    pub generation: u64,
    pub groups: HashMap<VirtualId, Arc<FailoverGroupRecord>>,
    pub members: HashMap<VirtualId, (VirtualId, usize)>,
    pub aliases: HashMap<VirtualId, VirtualId>,
    pub meta: FailoverGroupMeta,
}

impl FailoverGroupTargetData {
    /// `(input_name, input_stream_id)` of every member of every group.
    pub fn member_keys(&self) -> impl Iterator<Item = (Arc<str>, Arc<str>)> + '_ {
        self.groups.values().flat_map(|group| {
            group.members.iter().map(|member| (Arc::clone(&member.input_name), Arc::clone(&member.input_stream_id)))
        })
    }
}

/// Catalog together with the generation it was loaded for.
type GenerationCatalog = (u64, Arc<[CatalogRecord]>);

pub struct FailoverGroupCache {
    targets: tokio::sync::RwLock<HashMap<String, Arc<FailoverGroupTargetData>>>,
    /// Catalog per target and generation. Kept apart from the playback data: only the editor API reads it.
    catalogs: tokio::sync::RwLock<HashMap<String, GenerationCatalog>>,
    /// Per target: serializes store reads so concurrent requests after a generation bump load
    /// once, without making other targets wait.
    load_locks: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    #[cfg(any(test, feature = "test-support"))]
    loads: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "test-support"))]
    catalog_loads: std::sync::atomic::AtomicUsize,
}

impl Default for FailoverGroupCache {
    fn default() -> Self { Self::new() }
}

impl FailoverGroupCache {
    #[must_use]
    pub fn new() -> Self {
        Self {
            targets: tokio::sync::RwLock::new(HashMap::new()),
            catalogs: tokio::sync::RwLock::new(HashMap::new()),
            load_locks: std::sync::Mutex::new(HashMap::new()),
            #[cfg(any(test, feature = "test-support"))]
            loads: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(any(test, feature = "test-support"))]
            catalog_loads: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn load_lock(&self, target_name: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.load_locks.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(locks.entry(target_name.to_string()).or_default())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn hold_load_lock_for_test(&self, target_name: &str) -> tokio::sync::OwnedMutexGuard<()> {
        self.load_lock(target_name).lock_owned().await
    }

    /// Runs a blocking store read under the target's file lock; `None` without a storage path or when the
    /// read task did not finish.
    async fn read_store<T: Send + 'static>(
        app_config: &Arc<AppConfig>,
        target_name: &str,
        read: impl FnOnce(&Path) -> std::io::Result<T> + Send + 'static,
    ) -> Option<std::io::Result<T>> {
        let target_path = crate::get_target_storage_path(&app_config.config.load(), target_name)?;
        let file_lock = app_config.file_locks.read_lock(&crate::get_target_id_mapping_file(&target_path)).await;
        tokio::task::spawn_blocking(move || {
            let _guard = file_lock;
            read(&target_path)
        })
        .await
        .ok()
    }

    async fn cached(&self, target_name: &str, generation: u64) -> Option<Arc<FailoverGroupTargetData>> {
        let guard = self.targets.read().await;
        guard.get(target_name).filter(|cached| cached.generation == generation).map(Arc::clone)
    }

    pub async fn get_or_load(
        &self,
        app_config: &Arc<AppConfig>,
        playlists: &crate::PlaylistStorageState,
        target: &ConfigTarget,
    ) -> Arc<FailoverGroupTargetData> {
        let current_generation = playlists.failover_group_generation(&target.name);
        if let Some(cached) = self.cached(&target.name, current_generation).await {
            return cached;
        }

        let _load_guard = self.load_lock(&target.name).lock_owned().await;
        if let Some(cached) = self.cached(&target.name, current_generation).await {
            return cached;
        }

        let empty = || FailoverGroupTargetData { generation: current_generation, ..FailoverGroupTargetData::default() };

        #[cfg(any(test, feature = "test-support"))]
        self.loads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let loaded = Self::read_store(app_config, &target.name, move |target_path| {
            FailoverGroupStore::load_target_data(target_path, current_generation)
        })
        .await
        .and_then(Result::ok)
        .unwrap_or_else(empty);

        let arc_data = Arc::new(loaded);
        let mut guard = self.targets.write().await;
        guard.insert(target.name.clone(), Arc::clone(&arc_data));
        arc_data
    }

    async fn cached_catalog(&self, target_name: &str, generation: u64) -> Option<Arc<[CatalogRecord]>> {
        let guard = self.catalogs.read().await;
        guard.get(target_name).filter(|(cached, _)| *cached == generation).map(|(_, catalog)| Arc::clone(catalog))
    }

    /// The catalog of the pre-grouping playlist of `target`, loaded once per generation. Empty when the
    /// target has no catalog yet. A failed read is reported and not cached.
    pub async fn get_or_load_catalog(
        &self,
        app_config: &Arc<AppConfig>,
        playlists: &crate::PlaylistStorageState,
        target: &ConfigTarget,
    ) -> std::io::Result<Arc<[CatalogRecord]>> {
        let current_generation = playlists.failover_group_generation(&target.name);
        if let Some(cached) = self.cached_catalog(&target.name, current_generation).await {
            return Ok(cached);
        }

        let _load_guard = self.load_lock(&target.name).lock_owned().await;
        if let Some(cached) = self.cached_catalog(&target.name, current_generation).await {
            return Ok(cached);
        }

        #[cfg(any(test, feature = "test-support"))]
        self.catalog_loads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let records = match Self::read_store(app_config, &target.name, FailoverGroupStore::read_catalog).await {
            Some(result) => result?,
            None => Vec::new(),
        };
        let catalog: Arc<[CatalogRecord]> = Arc::from(records);
        self.catalogs.write().await.insert(target.name.clone(), (current_generation, Arc::clone(&catalog)));
        Ok(catalog)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn loaded_target_count(&self) -> usize { self.targets.read().await.len() }

    #[cfg(any(test, feature = "test-support"))]
    pub fn load_count(&self) -> usize { self.loads.load(std::sync::atomic::Ordering::Relaxed) }

    #[cfg(any(test, feature = "test-support"))]
    pub fn catalog_load_count(&self) -> usize { self.catalog_loads.load(std::sync::atomic::Ordering::Relaxed) }
}

#[cfg(test)]
mod tests;
