use std::io::Write;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use shared::error::TuliproxError;
use uuid::Uuid;

const SCHEMA_VERSION: u8 = 2;
const ACTIVE_MANIFEST: &str = "active.json";
const CHECKPOINT: &str = "checkpoint.json";
static MANIFEST_WRITE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClusterFiles {
    pub generation: u64,
    pub data: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SeriesFiles {
    pub generation: u64,
    pub roots: PathBuf,
    pub episodes: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StalkerActiveManifest {
    pub schema: u8,
    pub identity_fingerprint: u64,
    pub live: Option<ClusterFiles>,
    pub vod: Option<ClusterFiles>,
    pub series: Option<SeriesFiles>,
    pub epg: Option<ClusterFiles>,
}

impl StalkerActiveManifest {
    pub fn empty(identity_fingerprint: u64) -> Self {
        Self {
            schema: SCHEMA_VERSION,
            identity_fingerprint,
            live: None,
            vod: None,
            series: None,
            epg: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StalkerRefreshPhase {
    LiveBulk,
    Live { page: u32 },
    Vod { page: u32 },
    SeriesRoots { page: u32 },
    SeriesDetails { provider_id: Option<u32> },
    Epg,
    Complete,
    Terminal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StalkerCheckpoint {
    pub schema: u8,
    pub identity_fingerprint: u64,
    pub generation: u64,
    pub selection_mask: u8,
    pub started_at: i64,
    pub phase: StalkerRefreshPhase,
    pub retry_count: u8,
    pub page_signature: Option<u64>,
    pub processed: u64,
    pub skipped_count: u64,
    pub skipped_sample: Vec<u32>,
}

impl StalkerCheckpoint {
    pub fn new(identity_fingerprint: u64, generation: u64, selection_mask: u8, started_at: i64) -> Self {
        Self {
            schema: SCHEMA_VERSION,
            identity_fingerprint,
            generation,
            selection_mask,
            started_at,
            phase: StalkerRefreshPhase::LiveBulk,
            retry_count: 0,
            page_signature: None,
            processed: 0,
            skipped_count: 0,
            skipped_sample: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StalkerGenerationData {
    Live,
    Vod,
    SeriesRoots,
    SeriesEpisodes,
    Epg,
}

impl StalkerGenerationData {
    const fn file_label(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Vod => "vod",
            Self::SeriesRoots => "series-roots",
            Self::SeriesEpisodes => "series-episodes",
            Self::Epg => "epg",
        }
    }
}

pub fn generation_data_path(
    storage_path: &Path,
    generation: u64,
    data: StalkerGenerationData,
) -> PathBuf {
    storage_path.join(format!("generation-{generation}-{}.db", data.file_label()))
}

fn repository_error(context: &str, err: impl std::fmt::Display) -> TuliproxError {
    TuliproxError::RepositoryStalker(format!("{context}: {err}"))
}

fn unique_temporary_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map_or_else(std::ffi::OsString::new, ToOwned::to_owned);
    name.push(format!(".{}.tmp", Uuid::new_v4()));
    path.with_file_name(name)
}

async fn atomic_write_json<T: Serialize>(path: PathBuf, value: &T) -> Result<(), TuliproxError> {
    let bytes = serde_json::to_vec(value).map_err(|err| repository_error("encode Stalker state", err))?;
    tokio::task::spawn_blocking(move || {
        let temporary = unique_temporary_path(&path);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .map_err(|err| repository_error("open temporary Stalker state", err))?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|err| repository_error("write temporary Stalker state", err))?;
        crate::utils::rename_or_copy(&temporary, &path, false)
            .map_err(|err| repository_error("publish Stalker state", err))
    })
    .await
    .map_err(|err| repository_error("join Stalker state writer", err))?
}

async fn load_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, TuliproxError> {
    match tokio::fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|err| repository_error("decode Stalker state", err)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(repository_error("read Stalker state", err)),
    }
}

pub async fn save_active_manifest(
    storage_path: &Path,
    manifest: &StalkerActiveManifest,
) -> Result<(), TuliproxError> {
    tokio::fs::create_dir_all(storage_path)
        .await
        .map_err(|err| repository_error("create Stalker state directory", err))?;
    atomic_write_json(storage_path.join(ACTIVE_MANIFEST), manifest).await
}

pub async fn load_active_manifest(
    storage_path: &Path,
    identity_fingerprint: u64,
) -> Result<StalkerActiveManifest, TuliproxError> {
    let path = storage_path.join(ACTIVE_MANIFEST);
    let manifest = load_json::<StalkerActiveManifest>(&path).await?;
    if let Some(manifest) = manifest {
        if manifest.schema == SCHEMA_VERSION && manifest.identity_fingerprint == identity_fingerprint {
            return Ok(manifest);
        }
    }
    let empty = StalkerActiveManifest::empty(identity_fingerprint);
    save_active_manifest(storage_path, &empty).await?;
    let checkpoint_path = storage_path.join(CHECKPOINT);
    if let Err(err) = tokio::fs::remove_file(&checkpoint_path).await {
        if err.kind() != std::io::ErrorKind::NotFound {
            return Err(repository_error("remove incompatible Stalker checkpoint", err));
        }
    }
    Ok(empty)
}

pub async fn publish_selection(
    storage_path: &Path,
    identity_fingerprint: u64,
    generation: u64,
    selection_mask: u8,
) -> Result<StalkerActiveManifest, TuliproxError> {
    let _guard = MANIFEST_WRITE_LOCK.lock().await;
    let mut manifest = load_active_manifest(storage_path, identity_fingerprint).await?;
    if selection_mask & 0b0001 != 0 {
        manifest.live = Some(ClusterFiles {
            generation,
            data: generation_data_path(storage_path, generation, StalkerGenerationData::Live),
        });
    }
    if selection_mask & 0b0010 != 0 {
        manifest.vod = Some(ClusterFiles {
            generation,
            data: generation_data_path(storage_path, generation, StalkerGenerationData::Vod),
        });
    }
    if selection_mask & 0b0100 != 0 {
        manifest.series = Some(SeriesFiles {
            generation,
            roots: generation_data_path(storage_path, generation, StalkerGenerationData::SeriesRoots),
            episodes: generation_data_path(storage_path, generation, StalkerGenerationData::SeriesEpisodes),
        });
    }
    if selection_mask & 0b1000 != 0 {
        manifest.epg = Some(ClusterFiles {
            generation,
            data: generation_data_path(storage_path, generation, StalkerGenerationData::Epg),
        });
    }
    save_active_manifest(storage_path, &manifest).await?;
    Ok(manifest)
}

pub async fn save_checkpoint(
    storage_path: &Path,
    checkpoint: &StalkerCheckpoint,
) -> Result<(), TuliproxError> {
    tokio::fs::create_dir_all(storage_path)
        .await
        .map_err(|err| repository_error("create Stalker checkpoint directory", err))?;
    atomic_write_json(storage_path.join(CHECKPOINT), checkpoint).await
}

pub async fn load_checkpoint(
    storage_path: &Path,
    identity_fingerprint: u64,
) -> Result<Option<StalkerCheckpoint>, TuliproxError> {
    let path = storage_path.join(CHECKPOINT);
    let checkpoint = load_json::<StalkerCheckpoint>(&path).await?;
    if checkpoint.as_ref().is_some_and(|state| {
        state.schema != SCHEMA_VERSION || state.identity_fingerprint != identity_fingerprint
    }) {
        if let Err(err) = tokio::fs::remove_file(&path).await {
            if err.kind() != std::io::ErrorKind::NotFound {
                return Err(repository_error("remove incompatible Stalker checkpoint", err));
            }
        }
        return Ok(None);
    }
    Ok(checkpoint)
}

pub async fn clear_checkpoint(storage_path: &Path) -> Result<(), TuliproxError> {
    match tokio::fs::remove_file(storage_path.join(CHECKPOINT)).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(repository_error("remove Stalker checkpoint", err)),
    }
}

pub async fn cleanup_obsolete_generations(
    storage_path: &Path,
    manifest: &StalkerActiveManifest,
) -> Result<(), TuliproxError> {
    let mut active = HashSet::new();
    if let Some(files) = &manifest.live {
        active.insert(files.data.clone());
    }
    if let Some(files) = &manifest.vod {
        active.insert(files.data.clone());
    }
    if let Some(files) = &manifest.series {
        active.insert(files.roots.clone());
        active.insert(files.episodes.clone());
    }
    if let Some(files) = &manifest.epg {
        active.insert(files.data.clone());
    }

    let mut entries = match tokio::fs::read_dir(storage_path).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(repository_error("read Stalker generation directory", err)),
    };
    let mut candidates = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|err| repository_error("read Stalker generation entry", err))?
    {
        let path = entry.path();
        let has_generation_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("generation-"));
        let has_generation_extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("db") || extension.eq_ignore_ascii_case("tmp"));
        if has_generation_name && has_generation_extension {
            let generation = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix("generation-"))
                .and_then(|name| name.split_once('-'))
                .and_then(|(generation, _)| generation.parse::<u64>().ok());
            if let Some(generation) = generation {
                candidates.push((path, generation));
            }
        }
    }
    let mut retired_generations: Vec<u64> = candidates
        .iter()
        .filter(|(path, _)| !active.contains(path))
        .map(|(_, generation)| *generation)
        .collect();
    retired_generations.sort_unstable();
    retired_generations.dedup();
    let keep_from = retired_generations.len().saturating_sub(2);
    let retained = &retired_generations[keep_from..];
    for (path, generation) in candidates {
        if !active.contains(&path) && !retained.contains(&generation) {
            tokio::fs::remove_file(&path)
                .await
                .map_err(|err| repository_error("remove obsolete Stalker generation", err))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publishing_live_preserves_active_series_generation() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let mut manifest = StalkerActiveManifest::empty(17);
        manifest.series = Some(SeriesFiles {
            generation: 1,
            roots: "series-roots-1.db".into(),
            episodes: "series-episodes-1.db".into(),
        });
        save_active_manifest(temp.path(), &manifest).await?;

        publish_selection(temp.path(), 17, 2, 0b0001).await?;

        let active = load_active_manifest(temp.path(), 17).await?;
        assert_eq!(active.live.as_ref().map(|files| files.generation), Some(2));
        assert_eq!(active.series, manifest.series);
        Ok(())
    }

    #[tokio::test]
    async fn stalker_selection_publish_replaces_selected_entries_atomically() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let mut manifest = StalkerActiveManifest::empty(17);
        manifest.live = Some(ClusterFiles { generation: 1, data: "old-live.db".into() });
        manifest.vod = Some(ClusterFiles { generation: 1, data: "old-vod.db".into() });
        manifest.series = Some(SeriesFiles {
            generation: 1,
            roots: "old-series-roots.db".into(),
            episodes: "old-series-episodes.db".into(),
        });
        save_active_manifest(temp.path(), &manifest).await?;

        publish_selection(temp.path(), 17, 2, 0b0101).await?;

        let active = load_active_manifest(temp.path(), 17).await?;
        assert_eq!(
            active.live,
            Some(ClusterFiles {
                generation: 2,
                data: generation_data_path(temp.path(), 2, StalkerGenerationData::Live),
            })
        );
        assert_eq!(active.vod, manifest.vod);
        assert_eq!(
            active.series,
            Some(SeriesFiles {
                generation: 2,
                roots: generation_data_path(temp.path(), 2, StalkerGenerationData::SeriesRoots),
                episodes: generation_data_path(temp.path(), 2, StalkerGenerationData::SeriesEpisodes),
            })
        );
        assert!(active.epg.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn identity_change_invalidates_active_and_staging() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let mut manifest = StalkerActiveManifest::empty(17);
        manifest.live = Some(ClusterFiles { generation: 1, data: "live-1.db".into() });
        save_active_manifest(temp.path(), &manifest).await?;
        save_checkpoint(temp.path(), &StalkerCheckpoint::new(17, 2, 1, 123)).await?;

        let active = load_active_manifest(temp.path(), 18).await?;

        assert_eq!(active.identity_fingerprint, 18);
        assert!(active.live.is_none());
        assert!(load_checkpoint(temp.path(), 18).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn checkpoint_round_trips_atomically() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let mut checkpoint = StalkerCheckpoint::new(7, 3, 1, 123);
        checkpoint.phase = StalkerRefreshPhase::Vod { page: 9 };
        checkpoint.processed = 42;
        save_checkpoint(temp.path(), &checkpoint).await?;

        assert_eq!(load_checkpoint(temp.path(), 7).await?, Some(checkpoint));
        Ok(())
    }

    #[test]
    fn checkpoint_keeps_generation_start_time() {
        let checkpoint = StalkerCheckpoint::new(7, 3, 1, 1_723_456_789);
        assert_eq!(checkpoint.started_at, 1_723_456_789);
    }

    #[test]
    fn generation_paths_do_not_collide_between_clusters() {
        let root = Path::new("/tmp/stalker");
        let live = generation_data_path(root, 9, StalkerGenerationData::Live);
        let vod = generation_data_path(root, 9, StalkerGenerationData::Vod);
        let roots = generation_data_path(root, 9, StalkerGenerationData::SeriesRoots);
        let episodes = generation_data_path(root, 9, StalkerGenerationData::SeriesEpisodes);
        assert_ne!(live, vod);
        assert_ne!(roots, episodes);
        assert!(live.ends_with("generation-9-live.db"));
    }

    #[test]
    fn temporary_path_appends_suffix_to_complete_filename() {
        let path = unique_temporary_path(Path::new("/tmp/stalker/manifest.json"));
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();

        assert!(name.starts_with("manifest.json."));
        assert_eq!(path.extension().and_then(|extension| extension.to_str()), Some("tmp"));
    }

    #[tokio::test]
    async fn cleanup_preserves_active_and_two_recent_generations() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let active_path = generation_data_path(temp.path(), 4, StalkerGenerationData::Live);
        let oldest_path = generation_data_path(temp.path(), 1, StalkerGenerationData::Live);
        let previous_path = generation_data_path(temp.path(), 2, StalkerGenerationData::Live);
        let recent_path = generation_data_path(temp.path(), 3, StalkerGenerationData::Live);
        tokio::fs::write(&active_path, b"active").await?;
        tokio::fs::write(&oldest_path, b"oldest").await?;
        tokio::fs::write(&previous_path, b"previous").await?;
        tokio::fs::write(&recent_path, b"recent").await?;
        let mut manifest = StalkerActiveManifest::empty(17);
        manifest.live = Some(ClusterFiles { generation: 4, data: active_path.clone() });

        cleanup_obsolete_generations(temp.path(), &manifest).await?;

        assert!(tokio::fs::try_exists(active_path).await?);
        assert!(!tokio::fs::try_exists(oldest_path).await?);
        assert!(tokio::fs::try_exists(previous_path).await?);
        assert!(tokio::fs::try_exists(recent_path).await?);
        Ok(())
    }
}
