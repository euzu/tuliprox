//! Migrates API users between the config file and the user database.
//!
//! This ran as a method on `ApiProxyConfig`, which made the configuration model
//! perform persistence and depend on this layer. The configuration is now the
//! subject of the migration rather than its owner.

use crate::model::{AppConfig, ApiProxyConfig, Config};
use crate::repository::{backup_api_user_db_file, get_api_user_db_path, load_api_user, merge_api_user};
use crate::utils;
use crate::utils::file_exists_async;
use arc_swap::access::Access;
use arc_swap::ArcSwap;
use shared::model::{ApiProxyConfigDto, ConfigPaths};
use std::io::ErrorKind;
use std::sync::Arc;

fn serialize_api_proxy_config(config: &ApiProxyConfigDto) -> Result<String, String> {
    let mut serialized = String::new();
    let options = serde_saphyr::ser_options! {prefer_block_scalars: false};
    serde_saphyr::to_fmt_writer_with_options(&mut serialized, config, options)
        .map_err(|err| format!("Could not serialize api proxy config: {err}"))?;
    Ok(serialized)
}

async fn api_proxy_file_would_change(api_proxy_file: &str, config: &ApiProxyConfigDto) -> Result<bool, String> {
let serialized = serialize_api_proxy_config(config)?;
match tokio::fs::read_to_string(api_proxy_file).await {
    Ok(existing) => Ok(existing != serialized),
    Err(err) if err.kind() == ErrorKind::NotFound => Ok(true),
    Err(err) => Err(format!("Could not read api proxy file {api_proxy_file}: {err}")),
}
}

async fn backfill_output_clusters_to_file(api_proxy: &ApiProxyConfig, cfg: &AppConfig, errors: &mut Vec<String>) {
    if api_proxy.user.is_empty() {
        return;
    }
    let paths = <Arc<ArcSwap<ConfigPaths>> as Access<ConfigPaths>>::load(&cfg.paths);
    let api_proxy_file = paths.api_proxy_file_path.as_str();
    let dto = ApiProxyConfigDto::from(api_proxy);
    match api_proxy_file_would_change(api_proxy_file, &dto).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(err) => {
            errors.push(err);
            return;
        }
    }
    let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&cfg.config);
    let backup_dir = config.get_backup_dir();
    if let Err(err) = utils::save_api_proxy(api_proxy_file, backup_dir.as_ref(), &dto).await {
        errors.push(format!("Error saving api proxy file: {err}"));
    }
}

// we have the option to store user in the config file or in the user_db
// When we switch from one to other we need to migrate the existing data.
/// # Panics
pub async fn migrate_api_user(api_proxy: &mut ApiProxyConfig, cfg: &AppConfig, errors: &mut Vec<String>) {
    let paths = <Arc<ArcSwap<ConfigPaths>> as Access<ConfigPaths>>::load(&cfg.paths);
    let api_proxy_file = paths.api_proxy_file_path.as_str();
    if api_proxy.use_user_db {
        // we have user defined in config file.
        // we migrate them to the db and delete them from the config file
        if !&api_proxy.user.is_empty() {
            if let Err(err) = merge_api_user(cfg, &api_proxy.user).await {
                errors.push(err.to_string());
            } else {
                let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&cfg.config);
                let backup_dir = config.get_backup_dir();
                api_proxy.user = vec![];
                if let Err(err) =
                    utils::save_api_proxy(api_proxy_file, backup_dir.as_ref(), &ApiProxyConfigDto::from(&*api_proxy))
                        .await
                {
                    errors.push(format!("Error saving api proxy file: {err}"));
                }
            }
        }
        match load_api_user(cfg).await {
            Ok(users) => {
                let mut users = users;
                api_proxy.resolve_target_users(&mut users);
                api_proxy.user = users;
            }
            Err(err) => {
                println!("{err}");
                errors.push(err.to_string());
            }
        }
    } else {
        backfill_output_clusters_to_file(api_proxy, cfg, errors).await;
        let user_db_path = get_api_user_db_path(cfg);
        if file_exists_async(&user_db_path).await {
            // we can't have user defined in db file.
            // we need to load them and save them into the config file
            if let Ok(stored_users) = load_api_user(cfg).await {
                let mut stored_users = stored_users;
                api_proxy.resolve_target_users(&mut stored_users);
                for stored_user in stored_users {
                    if let Some(target_user) = api_proxy.user.iter_mut().find(|t| t.target == stored_user.target) {
                        for stored_credential in &stored_user.credentials {
                            if !target_user.credentials.iter().any(|c| c.username == stored_credential.username) {
                                target_user.credentials.push(stored_credential.clone());
                            }
                        }
                    } else {
                        api_proxy.user.push(stored_user);
                    }
                }
            }

            let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&cfg.config);
            let backup_dir = config.get_backup_dir();
            let dto = ApiProxyConfigDto::from(&*api_proxy);
            match api_proxy_file_would_change(api_proxy_file, &dto).await {
                Ok(true) => {
                    if let Err(err) = utils::save_api_proxy(api_proxy_file, backup_dir.as_ref(), &dto).await {
                        errors.push(format!("Error saving api proxy file: {err}"));
                    } else {
                        backup_api_user_db_file(cfg, &user_db_path).await;
                        let _ = tokio::fs::remove_file(&user_db_path).await;
                    }
                }
                Ok(false) => {
                    backup_api_user_db_file(cfg, &user_db_path).await;
                    let _ = tokio::fs::remove_file(&user_db_path).await;
                }
                Err(err) => errors.push(err),
            }
        }
    }
}
