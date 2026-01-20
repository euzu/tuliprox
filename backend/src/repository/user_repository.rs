use crate::model::PlaylistXtreamCategory;
use crate::model::{AppConfig, ProxyUserCredentials, TargetUser};
use crate::model::Config;
use crate::repository::bplustree::BPlusTree;
use crate::repository::storage_const;
use crate::repository::xtream_repository::xtream_get_playlist_categories;
use crate::utils;
use crate::utils::json_write_documents_to_file;
use chrono::Local;
use log::error;
use shared::model::{PlaylistBouquetDto, PlaylistClusterBouquetDto, ProxyType, ProxyUserStatus, TargetType, XtreamCluster};
use std::collections::{HashMap, HashSet};
use std::io::Error;
use std::path::{Path, PathBuf};
use tokio::task;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredProxyUserCredentialsDeprecated {
    pub target: String,
    pub username: String,
    pub password: String,
    pub token: Option<String>,
    pub proxy: ProxyType,
    pub server: Option<String>,
    pub epg_timeshift: Option<String>,
    pub created_at: Option<i64>,
    pub exp_date: Option<i64>,
    pub max_connections: Option<u32>,
    pub status: Option<ProxyUserStatus>,
    pub ui_enabled: bool,
}

impl StoredProxyUserCredentialsDeprecated {
    fn to(stored: &StoredProxyUserCredentialsDeprecated) -> ProxyUserCredentials {
        ProxyUserCredentials {
            username: stored.username.clone(),
            password: stored.password.clone(),
            token: stored.token.clone(),
            proxy: stored.proxy,
            server: stored.server.clone(),
            epg_timeshift: stored.epg_timeshift.clone(),
            created_at: stored.created_at,
            exp_date: stored.exp_date,
            max_connections: stored.max_connections.unwrap_or_default(),
            status: stored.status,
            ui_enabled: stored.ui_enabled,
            comment: None,
        }
    }
}

// This is a Helper class to store all user into one Database file.
// For the Config files we keep the old structure where a user is assigned to a target.
// But for storing inside one db file it is easier to store the target next to the user.
// due to known issue with  bincode and skip_serialization_if we have to list all fields and can't use ProxyUserCredentials
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredProxyUserCredentials {
    pub target: String,
    pub username: String,
    pub password: String,
    pub token: Option<String>,
    pub proxy: ProxyType,
    pub server: Option<String>,
    pub epg_timeshift: Option<String>,
    pub created_at: Option<i64>,
    pub exp_date: Option<i64>,
    pub max_connections: Option<u32>,
    pub status: Option<ProxyUserStatus>,
    pub ui_enabled: bool,
    pub comment: Option<String>,
}

impl StoredProxyUserCredentials {
    fn from(proxy: &ProxyUserCredentials, target_name: &str) -> Self {
        Self {
            target: String::from(target_name),
            username: proxy.username.clone(),
            password: proxy.password.clone(),
            token: proxy.token.clone(),
            proxy: proxy.proxy,
            server: proxy.server.clone(),
            epg_timeshift: proxy.epg_timeshift.clone(),
            created_at: proxy.created_at,
            exp_date: proxy.exp_date,
            max_connections: if proxy.max_connections > 0 { Some(proxy.max_connections) } else { None },
            status: proxy.status,
            ui_enabled: proxy.ui_enabled,
            comment: proxy.comment.clone(),
        }
    }

    fn to(stored: &StoredProxyUserCredentials) -> ProxyUserCredentials {
        ProxyUserCredentials {
            username: stored.username.clone(),
            password: stored.password.clone(),
            token: stored.token.clone(),
            proxy: stored.proxy,
            server: stored.server.clone(),
            epg_timeshift: stored.epg_timeshift.clone(),
            created_at: stored.created_at,
            exp_date: stored.exp_date,
            max_connections: stored.max_connections.unwrap_or_default(),
            status: stored.status,
            ui_enabled: stored.ui_enabled,
            comment: stored.comment.clone(),
        }
    }
}

pub fn get_api_user_db_path(cfg: &AppConfig) -> PathBuf {
    let paths = cfg.paths.load();
    PathBuf::from(&paths.config_path).join(storage_const::API_USER_DB_FILE)
}

fn add_target_user_to_user_tree(target_users: &[TargetUser], user_tree: &mut BPlusTree<String, StoredProxyUserCredentials>) {
    for target_user in target_users {
        for user in &target_user.credentials {
            let store_user: StoredProxyUserCredentials = StoredProxyUserCredentials::from(user, &target_user.target);
            user_tree.insert(user.username.clone(), store_user);
        }
    }
}

pub async fn merge_api_user(cfg: &AppConfig, target_users: &[TargetUser]) -> Result<u64, Error> {
    let path = get_api_user_db_path(cfg);
    let write_lock = cfg.file_locks.write_lock(&path).await;
    let mut user_tree: BPlusTree<String, StoredProxyUserCredentials> = task::spawn_blocking({
        let path = path.clone();
        move || BPlusTree::load(&path).unwrap_or_else(|_| BPlusTree::new())
    })
        .await
        .map_err(|err| Error::other(format!("Failed to load user db: {err}")))?;
    add_target_user_to_user_tree(target_users, &mut user_tree);
    let result = task::spawn_blocking({
        let path = path.clone();
        move || user_tree.store(&path)
    })
        .await
        .map_err(|err| Error::other(format!("Failed to store user db: {err}")))?;
    drop(write_lock);
    result
}

/// # Panics
///
/// Will panic if `backup_dir` is not given
pub async fn backup_api_user_db_file(cfg: &AppConfig, path: &Path) {
    if let Some(backup_dir) = cfg.config.load().backup_dir.as_ref() {
        let backup_path = PathBuf::from(backup_dir).join(format!("{}_{}", storage_const::API_USER_DB_FILE, Local::now().format("%Y%m%d_%H%M%S")));
        let lock = cfg.file_locks.read_lock(path).await;
        let copy_result = tokio::fs::copy(path, &backup_path).await;
        drop(lock);
        if let Err(err) = copy_result {
            error!("Could not backup file {}:{}", &backup_path.to_str().unwrap_or("?"), err);
        }
    }
}

pub async fn store_api_user(cfg: &AppConfig, target_users: &[TargetUser]) -> Result<u64, Error> {
    let mut user_tree = BPlusTree::<String, StoredProxyUserCredentials>::new();
    add_target_user_to_user_tree(target_users, &mut user_tree);
    let path = get_api_user_db_path(cfg);
    backup_api_user_db_file(cfg, &path).await;
    let write_lock = cfg.file_locks.write_lock(&path).await;
    let result = task::spawn_blocking({
        let path = path.clone();
        move || user_tree.store(&path)
    }).await.map_err(|err| Error::other(format!("Failed to store user db: {err}")))?;
    drop(write_lock);
    result
}

// TODO remove me if we get stable on user_db
pub async fn load_api_user_deprecated(cfg: &AppConfig) -> Result<Vec<TargetUser>, Error> {
    let path = get_api_user_db_path(cfg);
    let lock = cfg.file_locks.read_lock(&path).await;
    let user_tree = BPlusTree::<String, StoredProxyUserCredentialsDeprecated>::load(&path)?;
    drop(lock);
    let mut target_users: HashMap<String, TargetUser> = HashMap::new();
    for (_uname, stored_user) in &user_tree {
        let proxy_user: ProxyUserCredentials = StoredProxyUserCredentialsDeprecated::to(stored_user);
        let target_name = stored_user.target.clone();
        match target_users.entry(target_name) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let target = entry.get_mut();
                target.credentials.push(proxy_user);
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(TargetUser {
                    target: stored_user.target.clone(),
                    credentials: vec![proxy_user],
                });
            }
        }
    }
    Ok(target_users.into_values().collect())
}


pub async fn load_api_user(cfg: &AppConfig) -> Result<Vec<TargetUser>, Error> {
    let path = get_api_user_db_path(cfg);
    let lock = cfg.file_locks.read_lock(&path).await;
    let Ok(user_tree) = BPlusTree::<String, StoredProxyUserCredentials>::load(&path) else { return load_api_user_deprecated(cfg).await };
    drop(lock);
    let mut target_users: HashMap<String, TargetUser> = HashMap::new();
    for (_uname, stored_user) in &user_tree {
        let proxy_user: ProxyUserCredentials = StoredProxyUserCredentials::to(stored_user);
        let target_name = stored_user.target.clone();
        match target_users.entry(target_name) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let target = entry.get_mut();
                target.credentials.push(proxy_user);
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(TargetUser {
                    target: stored_user.target.clone(),
                    credentials: vec![proxy_user],
                });
            }
        }
    }
    Ok(target_users.into_values().collect())
}

pub fn get_user_storage_path(cfg: &Config, username: &str) -> Option<PathBuf> {
    cfg.user_config_dir.as_ref().and_then(|ucd| utils::get_file_path(ucd, Some(std::path::PathBuf::from(username))))
}

fn ensure_user_storage_path(cfg: &Config, username: &str) -> Option<PathBuf> {
    if let Some(path) = get_user_storage_path(cfg, username) {
        if !path.exists() && std::fs::create_dir_all(&path).is_err() {
            error!("Failed to create user config dir, can't create directory {}", path.display());
        }
        Some(path)
    } else {
        None
    }
}

fn user_get_live_bouquet_path(user_storage_path: &Path, target: TargetType) -> PathBuf {
    user_storage_path.join(PathBuf::from(format!("{}_{}", target.to_string().to_lowercase(), storage_const::USER_LIVE_BOUQUET)))
}

fn user_get_vod_bouquet_path(user_storage_path: &Path, target: TargetType) -> PathBuf {
    user_storage_path.join(PathBuf::from(format!("{}_{}", target.to_string().to_lowercase(), storage_const::USER_VOD_BOUQUET)))
}

fn user_get_series_bouquet_path(user_storage_path: &Path, target: TargetType) -> PathBuf {
    user_storage_path.join(PathBuf::from(format!("{}_{}", target.to_string().to_lowercase(), storage_const::USER_SERIES_BOUQUET)))
}

async fn save_xtream_user_bouquet_for_target(config: &Config, target_name: &str, storage_path: &Path, cluster: XtreamCluster, bouquet: Option<&Vec<String>>) -> Result<(), Error> {
    let bouquet_path = match cluster {
        XtreamCluster::Live => user_get_live_bouquet_path(storage_path, TargetType::Xtream),
        XtreamCluster::Video => user_get_vod_bouquet_path(storage_path, TargetType::Xtream),
        XtreamCluster::Series => user_get_series_bouquet_path(storage_path, TargetType::Xtream),
    };

    if let Some(bouquet_categories) = bouquet {
        if let Some(xtream_categories) = xtream_get_playlist_categories(config, target_name, cluster).await {
            let filtered: Vec<PlaylistXtreamCategory> = xtream_categories.iter().filter(|p| bouquet_categories.contains(&p.name)).cloned().collect();
            if filtered.is_empty() {
                if tokio::fs::try_exists(&bouquet_path).await.is_ok_and(|r| r) {
                    tokio::fs::remove_file(bouquet_path).await?;
                }
            } else {
                json_write_documents_to_file(&bouquet_path, &filtered).await.map_err(|err| Error::other(format!("Failed to write xtream bouquet file: {err}")))?;
            }
        }
    }

    Ok(())
}

async fn save_m3u_user_bouquet_for_target(storage_path: &Path, target: TargetType, cluster: XtreamCluster, bouquet: Option<&Vec<String>>) -> Result<(), Error> {
    let bouquet_path = match cluster {
        XtreamCluster::Live => user_get_live_bouquet_path(storage_path, target),
        XtreamCluster::Video => user_get_vod_bouquet_path(storage_path, target),
        XtreamCluster::Series => user_get_series_bouquet_path(storage_path, target),
    };
    match bouquet {
        Some(bouquet_categories) => {
            let categories = bouquet_categories.clone();
                json_write_documents_to_file(&bouquet_path, &categories).await.map_err(|err| Error::other(format!("Failed to write m3u bouquet file: {err}")))?;
        }
        None => if bouquet_path.exists() {
            tokio::fs::remove_file(bouquet_path).await?;
        }
    }

    Ok(())
}

async fn save_user_bouquet_for_target(config: &Config, target_name: &str, storage_path: &Path, target: TargetType, bouquet: Option<&PlaylistClusterBouquetDto>) -> Result<(), Error> {
    if target == TargetType::Xtream {
        save_xtream_user_bouquet_for_target(config, target_name, storage_path, XtreamCluster::Live, bouquet.and_then(|b| b.live.as_ref())).await?;
        save_xtream_user_bouquet_for_target(config, target_name, storage_path, XtreamCluster::Video, bouquet.and_then(|b| b.vod.as_ref())).await?;
        save_xtream_user_bouquet_for_target(config, target_name, storage_path, XtreamCluster::Series, bouquet.and_then(|b| b.series.as_ref())).await?;
    } else {
        save_m3u_user_bouquet_for_target(storage_path, target, XtreamCluster::Live, bouquet.and_then(|b| b.live.as_ref())).await?;
        save_m3u_user_bouquet_for_target(storage_path, target, XtreamCluster::Video, bouquet.and_then(|b| b.vod.as_ref())).await?;
        save_m3u_user_bouquet_for_target(storage_path, target, XtreamCluster::Series, bouquet.and_then(|b| b.series.as_ref())).await?;
    }
    Ok(())
}

pub async fn save_user_bouquet(cfg: &Config, target_name: &str, username: &str, bouquet: &PlaylistBouquetDto) -> Result<(), Error> {
    if let Some(storage_path) = ensure_user_storage_path(cfg, username) {
        save_user_bouquet_for_target(cfg, target_name, &storage_path, TargetType::Xtream, bouquet.xtream.as_ref()).await?;
        save_user_bouquet_for_target(cfg, target_name, &storage_path, TargetType::M3u, bouquet.m3u.as_ref()).await?;
        Ok(())
    } else {
        Err(Error::new(std::io::ErrorKind::NotFound, format!("User config path not found for user {username}")))
    }
}

async fn load_user_bouquet_from_file(file: &Path) -> Option<String> {
    tokio::fs::read_to_string(file).await.ok().filter(|content| !(content.is_empty() || content == "null"))
}

fn convert_xtream_user_bouquet(bouquet_cluster: Option<String>) -> Option<String> {
    bouquet_cluster
        .and_then(|c| serde_json::from_str::<Vec<PlaylistXtreamCategory>>(&c).ok())
        .map(|v| v.into_iter().map(|c| c.name).collect::<Vec<_>>())
        .and_then(|v| serde_json::to_string(&v).ok())
}

pub async fn load_user_bouquet_as_json(cfg: &Config, username: &str, target: TargetType) -> Option<String> {
    if let Some(storage_path) = get_user_storage_path(cfg, username) {
        if storage_path.exists() {
            let live_content = load_user_bouquet_from_file(&user_get_live_bouquet_path(&storage_path, target)).await;
            let vod_content = load_user_bouquet_from_file(&user_get_vod_bouquet_path(&storage_path, target)).await;
            let series_content = load_user_bouquet_from_file(&user_get_series_bouquet_path(&storage_path, target)).await;
            let (live, vod, series) = if target == TargetType::Xtream {
                (convert_xtream_user_bouquet(live_content),
                 convert_xtream_user_bouquet(vod_content),
                 convert_xtream_user_bouquet(series_content))
            } else {
                (live_content, vod_content, series_content)
            };
            return Some(format!(r#"{{"live": {}, "vod": {}, "series": {} }}"#,
                                live.unwrap_or("null".to_string()),
                                vod.unwrap_or("null".to_string()),
                                series.unwrap_or("null".to_string()),
            ));
        }
    }
    None
}

async fn user_get_cluster_bouquet(cfg: &Config, username: &str, target: TargetType, cluster: XtreamCluster) -> Option<String> {
    if let Some(storage_path) = get_user_storage_path(cfg, username) {
        if storage_path.exists() {
            return load_user_bouquet_from_file(&match cluster {
                XtreamCluster::Live => user_get_live_bouquet_path(&storage_path, target),
                XtreamCluster::Video => user_get_vod_bouquet_path(&storage_path, target),
                XtreamCluster::Series => user_get_series_bouquet_path(&storage_path, target),
            }).await;
        }
    }
    None
}

pub(crate) async fn user_get_live_bouquet(cfg: &Config, username: &str, target: TargetType) -> Option<String> {
    user_get_cluster_bouquet(cfg, username, target, XtreamCluster::Live).await
}

pub(crate) async fn user_get_vod_bouquet(cfg: &Config, username: &str, target: TargetType) -> Option<String> {
    user_get_cluster_bouquet(cfg, username, target, XtreamCluster::Video).await
}

pub(crate) async fn user_get_series_bouquet(cfg: &Config, username: &str, target: TargetType) -> Option<String> {
    user_get_cluster_bouquet(cfg, username, target, XtreamCluster::Series).await
}

/// Returns a filter set for bouquet categories based on user preferences.
///
/// # Arguments
/// * `category_id` - Optional category filter:
///   - `Some(id)` where id > 0: Returns only the specified category
///   - `Some(0)`: Treated as uncategorized, uses bouquet filtering
///   - `None`: Uses user's saved bouquet configuration
///
/// # Returns
/// * `Some(HashSet)` - Set of category IDs or names to include
/// * `None` - No filtering applied
///
/// TODO xtream converts ids to u32 again, separate m3u and xtream handling
pub async fn user_get_bouquet_filter(config: &Config, username: &str, category_id: Option<u32>, target: TargetType, cluster: XtreamCluster) -> Option<HashSet<String>> {
    if let Some(cid) = category_id {
        if cid > 0 {
            return Some(HashSet::from([cid.to_string()]));
        }
    }

    let bouquet = match cluster {
        XtreamCluster::Live => user_get_live_bouquet(config, username, target).await,
        XtreamCluster::Video => user_get_vod_bouquet(config, username, target).await,
        XtreamCluster::Series => user_get_series_bouquet(config, username, target).await,
    };

    match bouquet {
        None => None,
        Some(bouquet_categories) => {
            let mut filter = HashSet::new();
            let entries: Option<Vec<String>> = if target == TargetType::Xtream {
                // xtream filter has PlaylistXtreamCategory
                serde_json::from_str::<Vec<PlaylistXtreamCategory>>(&bouquet_categories)
                    .ok()
                    .map(|v| v.into_iter().map(|c| c.id.to_string()).collect())
            } else {
                // m3u filter has only group names
                serde_json::from_str::<Vec<String>>(&bouquet_categories).ok()
            };

            if let Some(entries) = entries {
                filter.extend(entries);
            }
            Some(filter)
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::FileLockManager;
    use arc_swap::{ArcSwap, ArcSwapAny};
    use shared::model::{ConfigPaths, ProxyType, ProxyUserStatus};
    use std::env::temp_dir;
    use std::sync::Arc;

    #[tokio::test]
    pub async fn save_target_user() {
        let user =
            TargetUser {
                target: "test".to_string(),
                credentials: vec![
                    ProxyUserCredentials {
                        username: "Test".to_string(),
                        password: "Test".to_string(),
                        token: Some("Test".to_string()),
                        proxy: ProxyType::Reverse(None),
                        server: Some("default".to_string()),
                        epg_timeshift: None,
                        created_at: None,
                        exp_date: Some(1_672_705_545),
                        max_connections: 1,
                        status: Some(ProxyUserStatus::Active),
                        ui_enabled: true,
                        comment: None,
                    },
                    ProxyUserCredentials {
                        username: "Test2".to_string(),
                        password: "Test".to_string(),
                        token: Some("Test".to_string()),
                        proxy: ProxyType::Reverse(None),
                        server: Some("default".to_string()),
                        epg_timeshift: None,
                        created_at: None,
                        exp_date: Some(1_672_705_545),
                        max_connections: 1,
                        status: Some(ProxyUserStatus::Expired),
                        ui_enabled: true,
                        comment: None,
                    },
                    ProxyUserCredentials {
                        username: "Test3".to_string(),
                        password: "Test".to_string(),
                        token: Some("Test".to_string()),
                        proxy: ProxyType::Reverse(None),
                        server: Some("default".to_string()),
                        epg_timeshift: None,
                        created_at: None,
                        exp_date: Some(1_672_705_545),
                        max_connections: 1,
                        status: Some(ProxyUserStatus::Expired),
                        ui_enabled: true,
                        comment: None,
                    },
                    ProxyUserCredentials {
                        username: "Test4".to_string(),
                        password: "Test".to_string(),
                        token: Some("Test".to_string()),
                        proxy: ProxyType::Reverse(None),
                        server: Some("default".to_string()),
                        epg_timeshift: None,
                        created_at: None,
                        exp_date: Some(1_672_705_545),
                        max_connections: 1,
                        status: Some(ProxyUserStatus::Expired),
                        ui_enabled: true,
                        comment: None,
                    }
                ],
            };

        let cfg = AppConfig {
            config: Arc::new(ArcSwapAny::default()),
            sources: Arc::new(ArcSwapAny::default()),
            hdhomerun: Arc::new(ArcSwapAny::default()),
            api_proxy: Arc::new(ArcSwapAny::default()),
            paths: Arc::new(ArcSwap::from(Arc::new(ConfigPaths {
                config_path: temp_dir().to_string_lossy().to_string(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            }))),
            file_locks: Arc::new(FileLockManager::default()),
            custom_stream_response: Arc::new(ArcSwapAny::default()),
            access_token_secret: Default::default(),
            encrypt_secret: Default::default(),
        };
        let target_user = vec![user];
        let _ = store_api_user(&cfg, &target_user).await;

        let user_list = load_api_user(&cfg).await;
        assert!(user_list.is_ok());
        assert_eq!(user_list.as_ref().unwrap().len(), 1);
        assert_eq!(user_list.as_ref().unwrap().first().unwrap().credentials.len(), 4);
    }
}
