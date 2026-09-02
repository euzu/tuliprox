use log::debug;
use shared::{error::TuliproxError, model::XtreamCluster};
use std::path::{Path, PathBuf};
use tuliprox_core::utils::{write_file_atomic, FileLockManager};

pub const RAW_GROUP_CATALOG_VERSION: u8 = 1;

/// Versioned entity for persisted raw group catalogs.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RawInputGroupCatalog {
    pub version: u8,
    pub input: String,
    pub cluster: XtreamCluster,
    pub groups: Vec<String>,
}

impl RawInputGroupCatalog {
    pub fn new(input: impl Into<String>, cluster: XtreamCluster, mut groups: Vec<String>) -> Self {
        groups.sort_unstable();
        groups.dedup();
        Self { version: RAW_GROUP_CATALOG_VERSION, input: input.into(), cluster, groups }
    }
}

pub fn raw_group_catalog_file_name(cluster: XtreamCluster) -> &'static str {
    match cluster {
        XtreamCluster::Live => "raw-groups-live.json",
        XtreamCluster::Video => "raw-groups-vod.json",
        XtreamCluster::Series => "raw-groups-series.json",
    }
}

pub fn raw_group_catalog_path(input_storage_dir: &Path, cluster: XtreamCluster) -> PathBuf {
    input_storage_dir.join(raw_group_catalog_file_name(cluster))
}

/// Invalidates (deletes) the raw group catalog file for a specific cluster.
pub async fn invalidate_raw_group_catalog(
    input_storage_dir: &Path,
    cluster: XtreamCluster,
    file_locks: &FileLockManager,
) -> Result<(), TuliproxError> {
    let path = raw_group_catalog_path(input_storage_dir, cluster);
    let _lock = file_locks.write_lock(&path).await;
    match tokio::fs::remove_file(&path).await {
        Ok(()) => {
            debug!("Invalidated raw group catalog: {}", path.display());
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => {
            Err(TuliproxError::RepositoryStorage(format!("Failed to invalidate catalog {}: {err}", path.display())))
        }
    }
}

/// Atomically publishes a raw group catalog for a cluster under `input_storage_dir`.
pub async fn publish_raw_group_catalog(
    input_storage_dir: &Path,
    input_name: &str,
    cluster: XtreamCluster,
    groups: Vec<String>,
    file_locks: &FileLockManager,
) -> Result<(), TuliproxError> {
    let catalog = RawInputGroupCatalog::new(input_name, cluster, groups);
    let json_bytes = serde_json::to_vec_pretty(&catalog)
        .map_err(|err| TuliproxError::RepositoryStorage(format!("Failed to serialize raw group catalog: {err}")))?;

    let path = raw_group_catalog_path(input_storage_dir, cluster);
    let _lock = file_locks.write_lock(&path).await;

    write_file_atomic(&path, &json_bytes).await.map_err(|err| {
        TuliproxError::RepositoryStorage(format!("Failed to publish raw group catalog to {}: {err}", path.display()))
    })?;

    debug!(
        "Published raw group catalog for input '{}' cluster '{:?}' ({} groups) to {}",
        input_name,
        cluster,
        catalog.groups.len(),
        path.display()
    );
    Ok(())
}

/// Loads a raw group catalog from disk. Returns `Ok(None)` if the catalog does not exist.
/// Fails with an error if the catalog is corrupt, has an unsupported version, or has a mismatched input/cluster.
pub async fn load_raw_group_catalog(
    input_storage_dir: &Path,
    expected_input: &str,
    cluster: XtreamCluster,
    file_locks: &FileLockManager,
) -> Result<Option<RawInputGroupCatalog>, TuliproxError> {
    let path = raw_group_catalog_path(input_storage_dir, cluster);
    let _lock = file_locks.read_lock(&path).await;

    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Ok(None);
    }

    let path_clone = path.clone();
    let expected_input_owned = expected_input.to_string();

    tokio::task::spawn_blocking(move || {
        let file = match std::fs::File::open(&path_clone) {
            Ok(f) => f,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(TuliproxError::RepositoryStorage(format!(
                    "Failed to open catalog {}: {err}",
                    path_clone.display()
                )));
            }
        };

        let reader = std::io::BufReader::new(file);
        let catalog: RawInputGroupCatalog = serde_json::from_reader(reader).map_err(|err| {
            TuliproxError::RepositoryStorage(format!(
                "Failed to parse raw group catalog {}: {err}",
                path_clone.display()
            ))
        })?;

        if catalog.version != RAW_GROUP_CATALOG_VERSION {
            return Err(TuliproxError::RepositoryStorage(format!(
                "Raw group catalog version mismatch in {}: expected {}, found {}",
                path_clone.display(),
                RAW_GROUP_CATALOG_VERSION,
                catalog.version
            )));
        }

        if catalog.input != expected_input_owned {
            return Err(TuliproxError::RepositoryStorage(format!(
                "Raw group catalog input mismatch in {}: expected '{}', found '{}'",
                path_clone.display(),
                expected_input_owned,
                catalog.input
            )));
        }

        if catalog.cluster != cluster {
            return Err(TuliproxError::RepositoryStorage(format!(
                "Raw group catalog cluster mismatch in {}: expected '{:?}', found '{:?}'",
                path_clone.display(),
                cluster,
                catalog.cluster
            )));
        }

        Ok(Some(catalog))
    })
    .await
    .map_err(|err| TuliproxError::Task(format!("Spawn blocking failed for catalog load: {err}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn filenames_match_spec() {
        let dir = Path::new("/storage/input_1");
        assert_eq!(
            raw_group_catalog_path(dir, XtreamCluster::Live),
            Path::new("/storage/input_1/raw-groups-live.json")
        );
        assert_eq!(
            raw_group_catalog_path(dir, XtreamCluster::Video),
            Path::new("/storage/input_1/raw-groups-vod.json")
        );
        assert_eq!(
            raw_group_catalog_path(dir, XtreamCluster::Series),
            Path::new("/storage/input_1/raw-groups-series.json")
        );
    }

    #[tokio::test]
    async fn publish_and_load_round_trip() {
        let temp_dir = TempDir::new().unwrap();
        let locks = FileLockManager::new();
        let groups = vec![
            "News".to_string(),
            "Sports".to_string(),
            "News".to_string(),
            "".to_string(),
            " Animation".to_string(),
        ];

        publish_raw_group_catalog(temp_dir.path(), "provider_a", XtreamCluster::Live, groups, &locks).await.unwrap();

        let loaded = load_raw_group_catalog(temp_dir.path(), "provider_a", XtreamCluster::Live, &locks).await.unwrap();

        assert!(loaded.is_some());
        let cat = loaded.unwrap();
        assert_eq!(cat.version, 1);
        assert_eq!(cat.input, "provider_a");
        assert_eq!(cat.cluster, XtreamCluster::Live);
        assert_eq!(cat.groups, vec!["", " Animation", "News", "Sports"]);
    }

    #[tokio::test]
    async fn empty_catalog_is_distinct_from_missing() {
        let temp_dir = TempDir::new().unwrap();
        let locks = FileLockManager::new();

        // Missing returns None
        let missing = load_raw_group_catalog(temp_dir.path(), "provider_a", XtreamCluster::Live, &locks).await.unwrap();
        assert!(missing.is_none());

        // Publish empty catalog
        publish_raw_group_catalog(temp_dir.path(), "provider_a", XtreamCluster::Live, vec![], &locks).await.unwrap();

        let loaded = load_raw_group_catalog(temp_dir.path(), "provider_a", XtreamCluster::Live, &locks).await.unwrap();
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().groups, Vec::<String>::new());
    }

    #[tokio::test]
    async fn invalidation_removes_catalog() {
        let temp_dir = TempDir::new().unwrap();
        let locks = FileLockManager::new();

        publish_raw_group_catalog(
            temp_dir.path(),
            "provider_a",
            XtreamCluster::Video,
            vec!["Movies".to_string()],
            &locks,
        )
        .await
        .unwrap();

        assert!(raw_group_catalog_path(temp_dir.path(), XtreamCluster::Video).exists());

        invalidate_raw_group_catalog(temp_dir.path(), XtreamCluster::Video, &locks).await.unwrap();

        assert!(!raw_group_catalog_path(temp_dir.path(), XtreamCluster::Video).exists());
    }

    #[tokio::test]
    async fn input_mismatch_fails() {
        let temp_dir = TempDir::new().unwrap();
        let locks = FileLockManager::new();

        publish_raw_group_catalog(temp_dir.path(), "provider_a", XtreamCluster::Live, vec!["News".to_string()], &locks)
            .await
            .unwrap();

        let res = load_raw_group_catalog(temp_dir.path(), "different_provider", XtreamCluster::Live, &locks).await;
        assert!(res.is_err());
    }
}
