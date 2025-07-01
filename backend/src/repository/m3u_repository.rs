use shared::error::{create_tuliprox_error,info_err};
use shared::error::{str_to_io_error, TuliproxError, TuliproxErrorKind};
use crate::model::{AppConfig, ProxyUserCredentials};
use crate::model::{Config, ConfigTarget, M3uTargetOutput};
use shared::model::{M3uPlaylistItem, PlaylistGroup, PlaylistItem};
use crate::repository::indexed_document::{IndexedDocumentDirectAccess, IndexedDocumentIterator, IndexedDocumentWriter};
use crate::repository::m3u_playlist_iterator::{M3uPlaylistM3uTextIterator};
use crate::repository::storage::{get_target_storage_path};
use log::error;
use std::fs::File;
use std::io::{Error, Write};
use std::path::{Path, PathBuf};
use shared::model::PlaylistItemType;
use crate::repository::storage_const;
use crate::utils;

macro_rules! cant_write_result {
    ($path:expr, $err:expr) => {
        create_tuliprox_error!(TuliproxErrorKind::Notify, "failed to write m3u playlist: {} - {}", $path.to_str().unwrap() ,$err)
    }
}

pub fn m3u_get_file_paths(target_path: &Path) -> (PathBuf, PathBuf) {
    let m3u_path = target_path.join(PathBuf::from(format!("{}.{}", storage_const::FILE_M3U, storage_const::FILE_SUFFIX_DB)));
    let index_path = target_path.join(PathBuf::from(format!("{}.{}", storage_const::FILE_M3U, storage_const::FILE_SUFFIX_INDEX)));
    (m3u_path, index_path)
}

pub fn m3u_get_epg_file_path(target_path: &Path) -> PathBuf {
    let path = target_path.join(PathBuf::from(format!("{}.{}", storage_const::FILE_M3U, storage_const::FILE_SUFFIX_DB)));
    utils::add_prefix_to_filename(&path, "epg_", Some("xml"))
}

fn persist_m3u_playlist_as_text(cfg: &Config, target: &ConfigTarget, target_output: &M3uTargetOutput, m3u_playlist: &Vec<M3uPlaylistItem>) {
    if let Some(filename) = target_output.filename.as_ref() {
        if let Some(m3u_filename) = utils::get_file_path(&cfg.working_dir, Some(PathBuf::from(filename))) {
            match File::create(&m3u_filename) {
                Ok(file) => {
                    let mut buf_writer = utils::file_writer(&file);
                    let _ = buf_writer.write(b"#EXTM3U\n");
                    for m3u in m3u_playlist {
                        let _ = buf_writer.write(m3u.to_m3u(target.options.as_ref(), false).to_string().as_bytes());
                        let _ = buf_writer.write(b"\n");
                    }
                }
                Err(_) => {
                    error!("Can't write m3u plain playlist {}", &m3u_filename.to_str().unwrap());
                }
            }
        }
    }
}

pub async fn m3u_write_playlist( cfg: &AppConfig, target: &ConfigTarget, target_output: &M3uTargetOutput, target_path: &Path, new_playlist: &[PlaylistGroup]) -> Result<(), TuliproxError> {
    if !new_playlist.is_empty() {
        let (m3u_path, idx_path) = m3u_get_file_paths(target_path);
        let m3u_playlist = new_playlist.iter()
            .flat_map(|pg| &pg.channels)
            .filter(|&pli| pli.header.item_type != PlaylistItemType::SeriesInfo)
            .map(PlaylistItem::to_m3u).collect::<Vec<M3uPlaylistItem>>();

        persist_m3u_playlist_as_text(&cfg.config.load(), target, target_output, &m3u_playlist);
        {
            let _file_lock = cfg.file_locks.write_lock(&m3u_path);
            match IndexedDocumentWriter::new(m3u_path.clone(), idx_path) {
                Ok(mut writer) => {
                    for m3u in m3u_playlist {
                        match writer.write_doc(m3u.virtual_id, &m3u) {
                            Ok(()) => {}
                            Err(err) => return Err(cant_write_result!(&m3u_path, err))
                        }
                    }
                    writer.store().map_err(|err| cant_write_result!(&m3u_path, err))?;
                }
                Err(err) => return Err(cant_write_result!(&m3u_path, err))
            }
        }
    }
    Ok(())
}

pub async fn m3u_load_rewrite_playlist(
    cfg: &AppConfig,
    target: &ConfigTarget,
    user: &ProxyUserCredentials,
) -> Result<M3uPlaylistM3uTextIterator, TuliproxError> {
    M3uPlaylistM3uTextIterator::new(cfg, target, user).await
}

pub async fn m3u_get_item_for_stream_id(stream_id: u32, cfg: &AppConfig, target: &ConfigTarget) -> Result<M3uPlaylistItem, Error> {
    if stream_id < 1 {
        return Err(str_to_io_error("id should start with 1"));
    }
    {
        let target_path = get_target_storage_path(&cfg.config.load(), target.name.as_str()).ok_or_else(|| str_to_io_error(&format!("Could not find path for target {}", &target.name)))?;
        let (m3u_path, idx_path) = m3u_get_file_paths(&target_path);
        let _file_lock = cfg.file_locks.read_lock(&m3u_path);
        IndexedDocumentDirectAccess::read_indexed_item::<u32, M3uPlaylistItem>(&m3u_path, &idx_path, &stream_id)
    }
}

pub async fn iter_raw_m3u_playlist(config: &AppConfig, target: &ConfigTarget) -> Option<(utils::FileReadGuard, impl Iterator<Item=(M3uPlaylistItem, bool)>)> {
    let target_path = get_target_storage_path(&config.config.load(), target.name.as_str())?;
    let (m3u_path, idx_path) = m3u_get_file_paths(&target_path);
    if !m3u_path.exists() || !idx_path.exists() {
        return None;
    }
    let file_lock = config.file_locks.read_lock(&m3u_path).await;
    match IndexedDocumentIterator::<u32, M3uPlaylistItem>::new(&m3u_path, &idx_path)
        .map_err(|err| info_err!(format!("Could not deserialize file {m3u_path:?} - {err}"))) {
        Ok(reader) => Some((file_lock, reader)),
        Err(_) => None
    }
}
