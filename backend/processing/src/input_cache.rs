use log::{error, warn};
use serde::{Deserialize, Serialize};
use shared::model::{PersistedPlaylistUpdateClusterSnapshot, PersistedPlaylistUpdateInputResult};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};
use tuliprox_repository::{build_input_storage_path, get_input_storage_path};

pub const STATUS_FILE: &str = "status.json";

#[derive(Serialize, Deserialize, Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClusterState {
    #[default]
    Ok,
    Failed,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ClusterStatus {
    pub status: ClusterState,
    pub timestamp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_update: Option<PersistedPlaylistUpdateClusterSnapshot>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct EpgSourceFetchStatus {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq)]
pub struct InputStatus {
    #[serde(default)]
    pub clusters: HashMap<String, ClusterStatus>,
    /// Last input completion for every input type; never used to establish cache validity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_input_update: Option<PersistedPlaylistUpdateInputResult>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub epg_sources: Vec<EpgSourceFetchStatus>,
}

pub fn update_epg_source_success(status: &mut InputStatus, index: usize, label: String, timestamp: u64) {
    if status.epg_sources.len() <= index {
        status.epg_sources.resize(index + 1, EpgSourceFetchStatus::default());
    }
    let entry = &mut status.epg_sources[index];
    entry.label = label;
    entry.fetched_at = Some(timestamp);
    entry.failed_at = None;
    entry.error = None;
}

pub fn update_epg_source_failure(
    status: &mut InputStatus,
    index: usize,
    label: String,
    timestamp: u64,
    error_msg: String,
) {
    if status.epg_sources.len() <= index {
        status.epg_sources.resize(index + 1, EpgSourceFetchStatus::default());
    }
    let entry = &mut status.epg_sources[index];
    entry.label = label;
    entry.failed_at = Some(timestamp);
    entry.error = Some(error_msg);
}

pub async fn resolve_input_storage_path(storage_dir: &str, input_name: &str) -> PathBuf {
    if let Ok(path) = get_input_storage_path(input_name, storage_dir).await {
        path
    } else {
        build_input_storage_path(input_name, storage_dir)
    }
}

pub fn load_input_status(path: &Path) -> InputStatus {
    let status_path = path.join(STATUS_FILE);
    if status_path.exists() {
        match fs::read_to_string(&status_path) {
            Ok(content) => match serde_json::from_str(&content) {
                Ok(status) => return status,
                Err(e) => warn!("Failed to parse input status file {}: {e}", status_path.display()),
            },
            Err(e) => warn!("Failed to read input status file {}: {e}", status_path.display()),
        }
    }
    InputStatus::default()
}

pub async fn load_input_status_async(path: &Path) -> InputStatus {
    let status_path = path.join(STATUS_FILE);
    match tokio::fs::read_to_string(&status_path).await {
        Ok(content) => match serde_json::from_str(&content) {
            Ok(status) => status,
            Err(e) => {
                warn!("Failed to parse input status file {}: {e}", status_path.display());
                InputStatus::default()
            }
        },
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!("Failed to read input status file {}: {e}", status_path.display());
            }
            InputStatus::default()
        }
    }
}

pub fn save_input_status(path: &Path, status: &InputStatus) {
    let status_path = path.join(STATUS_FILE);
    if let Some(parent) = status_path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            error!("Failed to create input storage directory {}: {e}", parent.display());
            return;
        }
    }
    match serde_json::to_string_pretty(status) {
        Ok(content) => {
            if let Err(e) = fs::write(&status_path, content) {
                error!("Failed to write input status file {}: {e}", status_path.display());
            }
        }
        Err(e) => error!("Failed to serialize input status: {e}"),
    }
}

pub async fn save_input_status_async(path: &Path, status: &InputStatus) {
    let status_path = path.join(STATUS_FILE);
    if let Some(parent) = status_path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            error!("Failed to create input storage directory {}: {e}", parent.display());
            return;
        }
    }
    match serde_json::to_string_pretty(status) {
        Ok(content) => {
            if let Err(e) = tokio::fs::write(&status_path, content).await {
                error!("Failed to write input status file {}: {e}", status_path.display());
            }
        }
        Err(e) => error!("Failed to serialize input status: {e}"),
    }
}

pub fn is_cache_valid(status: &InputStatus, cluster: &str, cache_duration_seconds: u64) -> bool {
    if cache_duration_seconds == 0 {
        return false;
    }
    if let Some(cluster_status) = status.clusters.get(cluster) {
        if cluster_status.status != ClusterState::Ok {
            return false;
        }
        let now = shared::utils::current_time_secs();
        if now >= cluster_status.timestamp {
            return now - cluster_status.timestamp < cache_duration_seconds;
        }
        // Timestamp in future? Invalid.
        return false;
    }
    false
}

pub fn update_cluster_status(status: &mut InputStatus, cluster: &str, state: ClusterState) {
    let now = shared::utils::current_time_secs();
    status.clusters.insert(cluster.to_string(), ClusterStatus { status: state, timestamp: now, last_update: None });
}

/// Replaces exactly one cluster's last snapshot while retaining its cache timestamp.
///
/// Stalker stores cache validity under `default`; its timestamp and state seed a
/// per-cluster read projection without changing the cache key used by processing.
pub fn replace_cluster_snapshot(
    status: &mut InputStatus,
    cluster: &str,
    snapshot: PersistedPlaylistUpdateClusterSnapshot,
) -> bool {
    if let Some(cluster_status) = status.clusters.get_mut(cluster) {
        if cluster_status.last_update == Some(snapshot) {
            return false;
        }
        cluster_status.last_update = Some(snapshot);
        return true;
    }

    let Some(default_status) = status.clusters.get("default") else {
        return false;
    };
    status.clusters.insert(
        cluster.to_string(),
        ClusterStatus {
            status: default_status.status,
            timestamp: default_status.timestamp,
            last_update: Some(snapshot),
        },
    );
    true
}

/// Replaces a cluster read projection from the canonical `default` cache status.
pub fn replace_cluster_snapshot_from_default_status(
    status: &mut InputStatus,
    cluster: &str,
    snapshot: PersistedPlaylistUpdateClusterSnapshot,
) -> bool {
    let Some(default_status) = status.clusters.get("default") else {
        return false;
    };
    let replacement = ClusterStatus {
        status: default_status.status,
        timestamp: default_status.timestamp,
        last_update: Some(snapshot),
    };
    if status.clusters.get(cluster) == Some(&replacement) {
        return false;
    }
    status.clusters.insert(cluster.to_string(), replacement);
    true
}

#[cfg(test)]
mod tests {
    use super::{
        load_input_status, replace_cluster_snapshot, replace_cluster_snapshot_from_default_status, save_input_status,
        ClusterState, ClusterStatus, InputStatus,
    };
    use shared::model::{
        PersistedPlaylistUpdateClusterSnapshot, PersistedPlaylistUpdateTechnicalState, PlaylistUpdateDataSource,
        XtreamCluster,
    };

    #[test]
    fn persisted_cluster_snapshot_reads_legacy_status_json_without_last_update() {
        let temp = tempfile::tempdir().expect("temporary status directory");
        std::fs::write(temp.path().join(super::STATUS_FILE), r#"{"clusters":{"live":{"status":"Ok","timestamp":17}}}"#)
            .expect("legacy status fixture");

        let status = load_input_status(temp.path());

        let live = status.clusters.get("live").expect("legacy live status");
        assert_eq!(live.status, ClusterState::Ok);
        assert_eq!(live.timestamp, 17);
        assert_eq!(live.last_update, None);
        assert_eq!(status.last_input_update, None);
        assert_eq!(status.epg_sources, []);
    }

    #[test]
    fn epg_sources_update_tracks_success_and_failure() {
        let mut status = InputStatus::default();
        super::update_epg_source_success(&mut status, 0, "guide1.xml".to_string(), 100);
        super::update_epg_source_failure(&mut status, 1, "guide2.xml".to_string(), 105, "network error".to_string());

        assert_eq!(status.epg_sources.len(), 2);
        assert_eq!(status.epg_sources[0].label, "guide1.xml");
        assert_eq!(status.epg_sources[0].fetched_at, Some(100));
        assert_eq!(status.epg_sources[0].failed_at, None);
        assert_eq!(status.epg_sources[0].error, None);

        assert_eq!(status.epg_sources[1].label, "guide2.xml");
        assert_eq!(status.epg_sources[1].fetched_at, None);
        assert_eq!(status.epg_sources[1].failed_at, Some(105));
        assert_eq!(status.epg_sources[1].error.as_deref(), Some("network error"));

        // Success resolves previous error
        super::update_epg_source_success(&mut status, 1, "guide2.xml".to_string(), 110);
        assert_eq!(status.epg_sources[1].fetched_at, Some(110));
        assert_eq!(status.epg_sources[1].failed_at, None);
        assert_eq!(status.epg_sources[1].error, None);
    }

    #[test]
    fn persisted_cluster_snapshot_replaces_one_value_without_history() {
        let temp = tempfile::tempdir().expect("temporary status directory");
        let mut status = InputStatus::default();
        super::update_cluster_status(&mut status, XtreamCluster::Live.as_ref(), ClusterState::Ok);
        let first = PersistedPlaylistUpdateClusterSnapshot {
            source: Some(PlaylistUpdateDataSource::Provider),
            technical_state: Some(PersistedPlaylistUpdateTechnicalState::Succeeded),
            ..PersistedPlaylistUpdateClusterSnapshot::default()
        };
        let replacement = PersistedPlaylistUpdateClusterSnapshot {
            source: Some(PlaylistUpdateDataSource::Cache),
            technical_state: Some(PersistedPlaylistUpdateTechnicalState::Succeeded),
            ..PersistedPlaylistUpdateClusterSnapshot::default()
        };

        assert!(replace_cluster_snapshot(&mut status, XtreamCluster::Live.as_ref(), first));
        assert!(replace_cluster_snapshot(&mut status, XtreamCluster::Live.as_ref(), replacement));
        save_input_status(temp.path(), &status);

        let reloaded = load_input_status(temp.path());
        assert_eq!(reloaded.clusters.len(), 1);
        assert_eq!(
            reloaded.clusters["live"].last_update,
            Some(replacement),
            "the new completion must replace, not append to, the prior snapshot"
        );
        let encoded = std::fs::read_to_string(temp.path().join(super::STATUS_FILE)).expect("persisted status");
        assert_eq!(encoded.matches("last_update").count(), 1);
    }

    #[test]
    fn persisted_cluster_snapshot_uses_current_canonical_default_status_for_projection() {
        let mut status = InputStatus::default();
        status.clusters.insert(
            "default".to_string(),
            ClusterStatus { status: ClusterState::Failed, timestamp: 23, last_update: None },
        );
        status.clusters.insert(
            XtreamCluster::Live.as_ref().to_string(),
            ClusterStatus { status: ClusterState::Ok, timestamp: 17, last_update: None },
        );
        let snapshot = PersistedPlaylistUpdateClusterSnapshot {
            source: Some(PlaylistUpdateDataSource::Provider),
            ..PersistedPlaylistUpdateClusterSnapshot::default()
        };

        assert!(replace_cluster_snapshot_from_default_status(&mut status, XtreamCluster::Live.as_ref(), snapshot));

        let projected = &status.clusters[XtreamCluster::Live.as_ref()];
        assert_eq!(projected.status, ClusterState::Failed);
        assert_eq!(projected.timestamp, 23);
        assert_eq!(projected.last_update, Some(snapshot));
    }
}
