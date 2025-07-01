use shared::error::info_err;
use shared::error::{TuliproxError, TuliproxErrorKind};
use crate::model::{AppConfig, ConfigTarget, TargetOutput};
use shared::model::{PlaylistGroup, PlaylistItemType};
use crate::model::Epg;
use crate::repository::epg_repository::epg_write;
use crate::repository::strm_repository::write_strm_playlist;
use crate::repository::m3u_repository::m3u_write_playlist;
use crate::repository::storage::{ensure_target_storage_path, get_target_id_mapping_file};
use crate::repository::target_id_mapping::TargetIdMapping;
use crate::repository::xtream_repository::xtream_write_playlist;
use std::path::Path;
use shared::utils::{is_dash_url, is_hls_url};
use crate::utils;

pub async fn persist_playlist(app_config: &AppConfig, playlist: &mut [PlaylistGroup], epg: Option<&Epg>,
                              target: &ConfigTarget) -> Result<(), Vec<TuliproxError>> {
    let mut errors = vec![];
    let config = &app_config.config.load();
    let target_path = match ensure_target_storage_path(config, &target.name) {
        Ok(path) => path,
        Err(err) => return Err(vec![err]),
    };

    let (mut target_id_mapping, file_lock) = get_target_id_mapping(app_config, &target_path).await;

    // Virtual IDs assignment
    for group in playlist.iter_mut() {
        for channel in &mut group.channels {
            let header = &mut channel.header;
            let provider_id = header.get_provider_id().unwrap_or_default();
            if provider_id == 0 {
                header.item_type = match (is_hls_url(&header.url), header.item_type) {
                    (true, _) => PlaylistItemType::LiveHls,
                    (false, PlaylistItemType::Live) => {
                        if is_dash_url(&header.url) {
                            PlaylistItemType::LiveDash
                        } else {
                            PlaylistItemType::LiveUnknown
                        }
                    }
                    _ => header.item_type,
                };
            }
            let uuid = header.get_uuid();
            let item_type = header.item_type;
            header.virtual_id = target_id_mapping.get_and_update_virtual_id(uuid, provider_id, item_type, 0);
        }
    }

    for output in &target.output {
        let result = match output {
            TargetOutput::Xtream(_xtream_output) => xtream_write_playlist(app_config, target, playlist).await,
            TargetOutput::M3u(m3u_output) => m3u_write_playlist(app_config, target, m3u_output, &target_path, playlist).await,
            TargetOutput::Strm(strm_output) => write_strm_playlist(app_config, target, strm_output, playlist).await,
            TargetOutput::HdHomeRun(_hdhomerun_output) => Ok(()),
        };

        if let Err(err) = result {
            errors.push(err);
        } else if !playlist.is_empty() {
            if let Err(err) = epg_write(config, target, &target_path, epg, output) {
                errors.push(err);
            }
        }
    }

    if let Err(err) = target_id_mapping.persist() {
        errors.push(info_err!(err.to_string()));
    }
    drop(file_lock);

    if errors.is_empty() { Ok(()) } else { Err(errors) }
}

pub async fn get_target_id_mapping(cfg: &AppConfig, target_path: &Path) -> (TargetIdMapping, utils::FileWriteGuard) {
    let target_id_mapping_file = get_target_id_mapping_file(target_path);
    let file_lock = cfg.file_locks.write_lock(&target_id_mapping_file).await;
    (TargetIdMapping::new(&target_id_mapping_file), file_lock)
}
