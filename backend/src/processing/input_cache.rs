use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use log::{error, warn};
use crate::repository::{build_input_storage_path, get_input_storage_path};

pub const STATUS_FILE: &str = "status.json";

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub enum ClusterState {
    #[default]
    Ok,
    Failed,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ClusterStatus {
    pub status: ClusterState,
    pub timestamp: u64,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct InputStatus {
    #[serde(default)]
    pub clusters: HashMap<String, ClusterStatus>,
}

pub fn resolve_input_storage_path(working_dir: &str, input_name: &str) -> PathBuf {
    if let Ok(path) = get_input_storage_path(input_name, working_dir) { path } else {
        build_input_storage_path(input_name, working_dir)
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
        },
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
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        if now > cluster_status.timestamp {
             // check if age is within duration
             // timestamp is creation time.
             // wait, assuming timestamp is Last Update Time.
             // now - timestamp < duration
             return now - cluster_status.timestamp < cache_duration_seconds;
        }
        // Timestamp in future? Invalid.
        return false;
    }
    false
}

pub fn update_cluster_status(status: &mut InputStatus, cluster: &str, state: ClusterState) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    status.clusters.insert(cluster.to_string(), ClusterStatus {
        status: state,
        timestamp: now,
    });
}
