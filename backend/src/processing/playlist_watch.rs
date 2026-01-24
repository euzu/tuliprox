use std::collections::BTreeSet;
use std::sync::Arc;
use std::path::{Path};
use log::{error};
use shared::model::{PlaylistGroup};
use crate::messaging::{send_message};
use crate::model::{AppConfig, MessageContent, WatchChanges};
use crate::utils;
use crate::utils::{binary_deserialize, binary_serialize, file_exists_async};

pub async fn process_group_watch(app_config: &Arc<AppConfig>, client: &reqwest::Client, target_name: &str, pl: &PlaylistGroup) {
    let mut new_tree = BTreeSet::new();
    pl.channels.iter().for_each(|chan| {
        let header = &chan.header;
        let title = if header.title.is_empty() { header.name.clone() } else { header.title.clone() };
        new_tree.insert(title);
    });

    let watch_filename = format!("{}/{}.bin", utils::sanitize_filename(target_name), utils::sanitize_filename(&pl.title));
    let cfg = app_config.config.load();
    match utils::get_file_path(&cfg.working_dir, Some(std::path::PathBuf::from(&watch_filename))) {
        Some(path) => {
            let save_path = path.as_path();
            let mut changed = false;
            if file_exists_async(&path).await {
                if let Some(loaded_tree) = load_watch_tree(&path).await {
                    // Find elements in set2 but not in set1
                    let added_difference: BTreeSet<Arc<str>> = new_tree.difference(&loaded_tree).cloned().collect();
                    let removed_difference: BTreeSet<Arc<str>> = loaded_tree.difference(&new_tree).cloned().collect();
                    if !added_difference.is_empty() || !removed_difference.is_empty() {
                        changed = true;
                        handle_watch_notification(app_config, client, &added_difference, &removed_difference, target_name, &pl.title).await;
                    }
                } else {
                    error!("failed to load watch_file {}", &path.to_str().unwrap_or_default());
                    changed = true;
                }
            } else {
                changed = true;
            }
            if changed {
                match save_watch_tree(save_path, &new_tree).await {
                    Ok(()) => {}
                    Err(err) => {
                        error!("failed to write watch_file {}: {}", save_path.to_str().unwrap_or_default(), err);
                    }
                }
            }
        }
        None => {
            error!("failed to write watch_file {}", &watch_filename);
        }
    }
}

async fn handle_watch_notification(app_config: &Arc<AppConfig>, client: &reqwest::Client, added: &BTreeSet<Arc<str>>, removed: &BTreeSet<Arc<str>>, target_name: &str, group_name: &str) {
    let added = added.iter().map(std::string::ToString::to_string).collect::<Vec<String>>();
    let removed = removed.iter().map(std::string::ToString::to_string).collect::<Vec<String>>();
    if !added.is_empty() || !removed.is_empty() {

        let changes = WatchChanges {
            target: target_name.to_string(),
            group: group_name.to_string(),
            added,
            removed
        };

        send_message(app_config, client, MessageContent::Watch(changes)).await;
    }
}

async fn load_watch_tree(path: &Path) -> Option<BTreeSet<Arc<str>>> {
     let encoded = tokio::fs::read(path).await.ok()?;
     binary_deserialize(&encoded[..]).ok()
}

async fn save_watch_tree(path: &Path, tree: &BTreeSet<Arc<str>>) -> std::io::Result<()> {
    let encoded: Vec<u8> = binary_serialize(&tree)?;
    tokio::fs::write(path, encoded).await
}

