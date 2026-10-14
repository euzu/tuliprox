use crate::{
    bplustree::{BPlusTree, BPlusTreeError, BPlusTreeQuery},
    storage_const::{FILE_CHANNEL_GROUPS, FILE_CHANNEL_GROUPS_ALIAS, FILE_CHANNEL_GROUPS_MEMBER},
    target_bouquet_repository::sanitize_target_prefix,
};
use log::debug;
use shared::{
    error::TuliproxError,
    model::{
        ChannelGroupOverridesDto, ChannelGroupPolicy, ChannelMemberRefDto, PlaylistItem, PlaylistItemType,
        StreamProperties, UUIDType, VirtualId, XtreamCluster, CHANNEL_GROUP_OVERRIDES_VERSION,
    },
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tuliprox_core::{
    model::{AppConfig, ConfigTarget},
    utils::{remove_file_if_exists, write_text_file_atomic},
};

pub const CHANNEL_GROUP_MUTATION_LOCK: &str = "channel_group:mutation";

#[derive(Debug, Copy, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ChannelGroupOrigin {
    Auto,
    Manual,
    AutoEdited,
}

#[derive(Debug, Clone)]
pub struct PendingChannelGroup {
    pub group_uuid: UUIDType,
    pub key: Arc<str>,
    pub origin: ChannelGroupOrigin,
    pub policy: ChannelGroupPolicy,
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
pub struct ChannelGroupRecord {
    pub group_vid: VirtualId,
    pub key: Arc<str>,
    pub origin: ChannelGroupOrigin,
    pub policy: ChannelGroupPolicy,
    pub members: Vec<GroupMember>,
    pub orphaned: Vec<ChannelMemberRefDto>,
}

pub struct ChannelGroupStagedPublication {
    target_path: PathBuf,
    staged_groups: Option<PathBuf>,
    staged_members: Option<PathBuf>,
    staged_aliases: Option<PathBuf>,
}

impl ChannelGroupStagedPublication {
    pub fn discard(self) { drop(self); }

    pub fn publish(mut self) -> std::io::Result<()> {
        if let Some(p) = self.staged_groups.take() {
            std::fs::rename(&p, self.target_path.join(FILE_CHANNEL_GROUPS))?;
        } else {
            let _ = remove_file_if_exists(&self.target_path.join(FILE_CHANNEL_GROUPS));
        }

        if let Some(p) = self.staged_members.take() {
            std::fs::rename(&p, self.target_path.join(FILE_CHANNEL_GROUPS_MEMBER))?;
        } else {
            let _ = remove_file_if_exists(&self.target_path.join(FILE_CHANNEL_GROUPS_MEMBER));
        }

        if let Some(p) = self.staged_aliases.take() {
            std::fs::rename(&p, self.target_path.join(FILE_CHANNEL_GROUPS_ALIAS))?;
        } else {
            let _ = remove_file_if_exists(&self.target_path.join(FILE_CHANNEL_GROUPS_ALIAS));
        }

        Ok(())
    }
}

impl Drop for ChannelGroupStagedPublication {
    fn drop(&mut self) {
        if let Some(p) = &self.staged_groups {
            let _ = remove_file_if_exists(p);
        }
        if let Some(p) = &self.staged_members {
            let _ = remove_file_if_exists(p);
        }
        if let Some(p) = &self.staged_aliases {
            let _ = remove_file_if_exists(p);
        }
    }
}

pub struct ChannelGroupStore;

impl ChannelGroupStore {
    pub fn write_staged(
        target_path: &Path,
        records: &[ChannelGroupRecord],
        aliases: &[(VirtualId, VirtualId)],
    ) -> std::io::Result<ChannelGroupStagedPublication> {
        if records.is_empty() {
            return Ok(ChannelGroupStagedPublication {
                target_path: target_path.to_path_buf(),
                staged_groups: None,
                staged_members: None,
                staged_aliases: None,
            });
        }

        let stage_suffix = format!(".stage-{}", uuid::Uuid::new_v4());
        let staged_groups_path = target_path.join(format!("{FILE_CHANNEL_GROUPS}{stage_suffix}"));
        let mut groups_tree = BPlusTree::<VirtualId, ChannelGroupRecord>::new();
        for record in records {
            groups_tree.insert(record.group_vid, record.clone());
        }
        if let Err(err) = groups_tree.store(&staged_groups_path) {
            let _ = remove_file_if_exists(&staged_groups_path);
            return Err(err);
        }

        let staged_members_path = target_path.join(format!("{FILE_CHANNEL_GROUPS_MEMBER}{stage_suffix}"));
        let mut members_tree = BPlusTree::<VirtualId, VirtualId>::new();
        for record in records {
            for member in &record.members {
                members_tree.insert(member.member_vid, record.group_vid);
            }
        }
        if let Err(err) = members_tree.store(&staged_members_path) {
            let _ = remove_file_if_exists(&staged_groups_path);
            let _ = remove_file_if_exists(&staged_members_path);
            return Err(err);
        }

        let staged_aliases_path = if aliases.is_empty() {
            None
        } else {
            let path = target_path.join(format!("{FILE_CHANNEL_GROUPS_ALIAS}{stage_suffix}"));
            let mut aliases_tree = BPlusTree::<VirtualId, VirtualId>::new();
            for (alias_vid, group_vid) in aliases {
                aliases_tree.insert(*alias_vid, *group_vid);
            }
            if let Err(err) = aliases_tree.store(&path) {
                let _ = remove_file_if_exists(&staged_groups_path);
                let _ = remove_file_if_exists(&staged_members_path);
                let _ = remove_file_if_exists(&path);
                return Err(err);
            }
            Some(path)
        };

        Ok(ChannelGroupStagedPublication {
            target_path: target_path.to_path_buf(),
            staged_groups: Some(staged_groups_path),
            staged_members: Some(staged_members_path),
            staged_aliases: staged_aliases_path,
        })
    }

    pub fn write(
        target_path: &Path,
        records: &[ChannelGroupRecord],
        aliases: &[(VirtualId, VirtualId)],
    ) -> std::io::Result<()> {
        Self::write_staged(target_path, records, aliases)?.publish()
    }

    pub fn delete(target_path: &Path) -> std::io::Result<()> {
        let _ = remove_file_if_exists(&target_path.join(FILE_CHANNEL_GROUPS));
        let _ = remove_file_if_exists(&target_path.join(FILE_CHANNEL_GROUPS_MEMBER));
        let _ = remove_file_if_exists(&target_path.join(FILE_CHANNEL_GROUPS_ALIAS));
        Ok(())
    }

    pub fn read_alias(target_path: &Path, vid: VirtualId) -> std::io::Result<Option<VirtualId>> {
        let db_path = target_path.join(FILE_CHANNEL_GROUPS_ALIAS);
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

    pub fn read_group(target_path: &Path, group_vid: VirtualId) -> std::io::Result<Option<ChannelGroupRecord>> {
        let db_path = target_path.join(FILE_CHANNEL_GROUPS);
        if !db_path.exists() {
            return Ok(None);
        }
        let mut query = match BPlusTreeQuery::<VirtualId, ChannelGroupRecord>::try_new(&db_path) {
            Ok(q) => q,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        query.query(&group_vid).map_err(BPlusTreeError::to_io)
    }

    pub fn read_member(
        target_path: &Path,
        member_vid: VirtualId,
    ) -> std::io::Result<Option<(ChannelGroupRecord, usize)>> {
        let db_path = target_path.join(FILE_CHANNEL_GROUPS_MEMBER);
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

    pub fn read_all_groups(target_path: &Path) -> std::io::Result<Vec<ChannelGroupRecord>> {
        let db_path = target_path.join(FILE_CHANNEL_GROUPS);
        if !db_path.exists() {
            return Ok(Vec::new());
        }
        let mut query = match BPlusTreeQuery::<VirtualId, ChannelGroupRecord>::try_new(&db_path) {
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
        let db_path = target_path.join(FILE_CHANNEL_GROUPS_ALIAS);
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

    pub fn load_target_data(target_path: &Path, generation: u64) -> std::io::Result<ChannelGroupTargetData> {
        let records = Self::read_all_groups(target_path)?;
        let aliases = Self::read_all_aliases(target_path)?;
        let mut groups = HashMap::with_capacity(records.len());
        let mut members = HashMap::new();
        for record in records {
            let group_vid = record.group_vid;
            for (idx, member) in record.members.iter().enumerate() {
                members.insert(member.member_vid, (group_vid, idx));
            }
            groups.insert(group_vid, Arc::new(record));
        }
        Ok(ChannelGroupTargetData { generation, groups, members, aliases })
    }
}

#[derive(Debug, Clone, Default)]
pub struct ChannelGroupTargetData {
    pub generation: u64,
    pub groups: HashMap<VirtualId, Arc<ChannelGroupRecord>>,
    pub members: HashMap<VirtualId, (VirtualId, usize)>,
    pub aliases: HashMap<VirtualId, VirtualId>,
}

pub struct ChannelGroupCache {
    targets: tokio::sync::RwLock<HashMap<String, Arc<ChannelGroupTargetData>>>,
}

impl Default for ChannelGroupCache {
    fn default() -> Self { Self::new() }
}

impl ChannelGroupCache {
    #[must_use]
    pub fn new() -> Self { Self { targets: tokio::sync::RwLock::new(HashMap::new()) } }

    pub async fn get_or_load(
        &self,
        app_config: &Arc<AppConfig>,
        playlists: &crate::PlaylistStorageState,
        target: &ConfigTarget,
    ) -> Arc<ChannelGroupTargetData> {
        let current_generation = playlists.channel_group_generation();
        {
            let guard = self.targets.read().await;
            if let Some(cached) = guard.get(&target.name) {
                if cached.generation == current_generation {
                    return Arc::clone(cached);
                }
            }
        }

        let config = app_config.config.load();
        let Some(target_path) = crate::get_target_storage_path(&config, &target.name) else {
            return Arc::new(ChannelGroupTargetData {
                generation: current_generation,
                groups: HashMap::new(),
                members: HashMap::new(),
                aliases: HashMap::new(),
            });
        };

        let file_lock = app_config.file_locks.read_lock(&target_path).await;
        let loaded = tokio::task::spawn_blocking(move || {
            let _guard = file_lock;
            ChannelGroupStore::load_target_data(&target_path, current_generation)
        })
        .await
        .unwrap_or_else(|_| {
            Ok(ChannelGroupTargetData {
                generation: current_generation,
                groups: HashMap::new(),
                members: HashMap::new(),
                aliases: HashMap::new(),
            })
        })
        .unwrap_or_else(|_| ChannelGroupTargetData {
            generation: current_generation,
            groups: HashMap::new(),
            members: HashMap::new(),
            aliases: HashMap::new(),
        });

        let arc_data = Arc::new(loaded);
        let mut guard = self.targets.write().await;
        guard.insert(target.name.clone(), Arc::clone(&arc_data));
        arc_data
    }
}

#[derive(Debug)]
pub enum ChannelGroupSaveError {
    RevisionMismatch { current: u64 },
    Io(TuliproxError),
}

impl std::fmt::Display for ChannelGroupSaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RevisionMismatch { current } => write!(f, "revision mismatch: current is {current}"),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ChannelGroupSaveError {}

impl From<TuliproxError> for ChannelGroupSaveError {
    fn from(err: TuliproxError) -> Self { Self::Io(err) }
}

/// Returns the channel group storage directory derived from `config_path`.
pub fn channel_group_overrides_dir(config_path: &Path) -> PathBuf { config_path.join("channel_groups") }

/// Derives the canonical filename for a target's channel group overrides.
pub fn channel_group_overrides_file_name(target_name: &str) -> String {
    let prefix = sanitize_target_prefix(target_name);
    let hash = blake3::hash(target_name.as_bytes());
    let hex = hash.to_hex();
    format!("{}--{}.yml", prefix, &hex[..12])
}

/// Derives the full path to a target's channel group overrides file.
pub fn channel_group_overrides_path(config_path: &Path, target_name: &str) -> PathBuf {
    channel_group_overrides_dir(config_path).join(channel_group_overrides_file_name(target_name))
}

/// Loads a target's channel group overrides from disk. Returns `Ok(None)` if the file does not exist.
/// Fails closed with an error on corruption, target mismatch, or unsupported version.
pub async fn load_channel_group_overrides(
    app_config: &AppConfig,
    target_name: &str,
) -> Result<Option<ChannelGroupOverridesDto>, TuliproxError> {
    let paths = app_config.paths.load();
    let config_path = Path::new(&paths.config_path);
    let file_path = channel_group_overrides_path(config_path, target_name);

    let _lock = app_config.file_locks.read_lock(&file_path).await;

    let content = match tokio::fs::read_to_string(&file_path).await {
        Ok(c) => c,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(TuliproxError::ChannelGroup(format!(
                "Failed to read channel group overrides file {}: {err}",
                file_path.display()
            )));
        }
    };

    let mut dto: ChannelGroupOverridesDto = serde_saphyr::from_str(&content).map_err(|err| {
        TuliproxError::ChannelGroup(format!(
            "Failed to parse channel group overrides YAML in {}: {err}",
            file_path.display()
        ))
    })?;

    if dto.version != CHANNEL_GROUP_OVERRIDES_VERSION {
        return Err(TuliproxError::ChannelGroup(format!(
            "Unsupported channel group overrides version {} in {}",
            dto.version,
            file_path.display()
        )));
    }

    if dto.target != target_name {
        return Err(TuliproxError::ChannelGroup(format!(
            "Target name mismatch in {}: file contains target '{}', expected '{target_name}'",
            file_path.display(),
            dto.target
        )));
    }

    dto.canonicalize();
    Ok(Some(dto))
}

/// Saves channel group overrides with optimistic concurrency control.
/// Returns the new revision on success.
pub async fn save_channel_group_overrides(
    app_config: &AppConfig,
    target_name: &str,
    mut dto: ChannelGroupOverridesDto,
    expected_revision: u64,
) -> Result<u64, ChannelGroupSaveError> {
    let paths = app_config.paths.load();
    let config_path = Path::new(&paths.config_path);
    let file_path = channel_group_overrides_path(config_path, target_name);

    let _mutation_lock = app_config.file_locks.write_lock_str(CHANNEL_GROUP_MUTATION_LOCK).await;
    let _file_lock = app_config.file_locks.write_lock(&file_path).await;

    let current = match tokio::fs::read_to_string(&file_path).await {
        Ok(content) => {
            let existing: ChannelGroupOverridesDto = serde_saphyr::from_str(&content).map_err(|err| {
                TuliproxError::ChannelGroup(format!(
                    "Failed to parse channel group overrides YAML in {}: {err}",
                    file_path.display()
                ))
            })?;
            if existing.target != target_name {
                return Err(ChannelGroupSaveError::Io(TuliproxError::ChannelGroup(format!(
                    "Target mismatch in existing overrides file {}: '{}' != '{target_name}'",
                    file_path.display(),
                    existing.target
                ))));
            }
            existing.revision
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => 0,
        Err(err) => {
            return Err(ChannelGroupSaveError::Io(TuliproxError::ChannelGroup(format!(
                "Failed to read channel group overrides {}: {err}",
                file_path.display()
            ))));
        }
    };

    if current != expected_revision {
        return Err(ChannelGroupSaveError::RevisionMismatch { current });
    }

    let next_revision = current + 1;
    dto.version = CHANNEL_GROUP_OVERRIDES_VERSION;
    dto.target = target_name.to_string();
    dto.revision = next_revision;
    dto.canonicalize();

    let yaml = serde_saphyr::to_string(&dto).map_err(|err| {
        TuliproxError::ChannelGroup(format!("Failed to serialize channel group overrides to YAML: {err}"))
    })?;

    write_text_file_atomic(&file_path, &yaml).await.map_err(|err| {
        TuliproxError::ChannelGroup(format!("Failed to write channel group overrides {}: {err}", file_path.display()))
    })?;

    debug!(
        "Saved channel group overrides for '{}' (revision {}) to {}",
        target_name,
        next_revision,
        file_path.display()
    );

    Ok(next_revision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_swap::{ArcSwap, ArcSwapOption};
    use shared::model::ConfigPaths;
    use std::sync::Arc;
    use tuliprox_core::{
        model::{Config, MediaToolCapabilities, SourcesConfig},
        utils::FileLockManager,
    };

    fn test_app_config(config_path: &str) -> AppConfig {
        AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            api_proxy: Arc::new(ArcSwapOption::default()),
            file_locks: Arc::new(FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
                home_path: String::new(),
                config_path: config_path.to_string(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(ArcSwapOption::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(MediaToolCapabilities::new()),
        }
    }

    fn test_app_config_with_config_dir() -> (AppConfig, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = test_app_config(dir.path().to_str().expect("utf-8 path"));
        (cfg, dir)
    }

    #[tokio::test]
    async fn save_then_load_round_trips_and_bumps_revision() {
        let (app_config, _dir) = test_app_config_with_config_dir();
        let dto = ChannelGroupOverridesDto::new("Main TV");
        let revision = save_channel_group_overrides(&app_config, "Main TV", dto, 0).await.expect("saves");
        assert_eq!(revision, 1);
        let loaded = load_channel_group_overrides(&app_config, "Main TV").await.expect("loads").expect("exists");
        assert_eq!(loaded.revision, 1);
        assert_eq!(loaded.target, "Main TV");
    }

    #[tokio::test]
    async fn stale_revision_is_rejected() {
        let (app_config, _dir) = test_app_config_with_config_dir();
        save_channel_group_overrides(&app_config, "T", ChannelGroupOverridesDto::new("T"), 0).await.expect("saves");
        let err =
            save_channel_group_overrides(&app_config, "T", ChannelGroupOverridesDto::new("T"), 0).await.unwrap_err();
        assert!(matches!(err, ChannelGroupSaveError::RevisionMismatch { current: 1 }));
    }

    #[tokio::test]
    async fn target_mismatch_and_unknown_version_fail_closed() {
        let (app_config, _dir) = test_app_config_with_config_dir();
        let path = channel_group_overrides_path(Path::new(&app_config.paths.load().config_path), "T");
        tokio::fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        tokio::fs::write(&path, "version: 1\ntarget: Other\nrevision: 3\n").await.unwrap();
        assert!(load_channel_group_overrides(&app_config, "T").await.is_err());
        tokio::fs::write(&path, "version: 9\ntarget: T\nrevision: 3\n").await.unwrap();
        assert!(load_channel_group_overrides(&app_config, "T").await.is_err());
    }

    #[tokio::test]
    async fn missing_file_is_none() {
        let (app_config, _dir) = test_app_config_with_config_dir();
        assert!(load_channel_group_overrides(&app_config, "T").await.expect("ok").is_none());
    }
}
