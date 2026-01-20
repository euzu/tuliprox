use crate::model::TVGuide;
use crate::model::{ConfigInput, PersistedEpgSource};
use crate::utils::{add_prefix_to_filename, prepare_file_path, request};
use crate::utils::{cleanup_unlisted_files_with_suffix};
use log::debug;
use shared::error::{TuliproxError, info_err};
use shared::utils::{sanitize_sensitive_info, short_hash};
use std::path::PathBuf;
use shared::concat_string;
use crate::processing::processor::playlist::PlaylistProcessingContext;
use crate::repository::storage::get_input_storage_path;
use crate::repository::storage_const;

pub fn get_input_raw_epg_file_path(url: &str, input: &ConfigInput, working_dir: &str) -> std::io::Result<PathBuf> {

    let file_prefix = short_hash(url);

    if let Some(persist_path) = input.persist.as_deref() {
        if !persist_path.is_empty() {
            if let Some(path) = prepare_file_path(input.persist.as_deref(), working_dir, "")
                .map(|path| add_prefix_to_filename(&path, concat_string!(&file_prefix, "_epg_").as_str(), Some("xml"))) {
                return Ok(path);
            }
        }
    }

    let download_path = get_input_storage_path(&input.name, working_dir)?;
    Ok(download_path.join(format!("{}_{}", file_prefix, storage_const::FILE_EPG)))
}

async fn download_epg_file(url: &str, ctx: &PlaylistProcessingContext, input: &ConfigInput, working_dir: &str) -> Result<PathBuf, TuliproxError> {
    debug!("Getting epg file path for url: {}", sanitize_sensitive_info(url));
    let persist_file_path = get_input_raw_epg_file_path(url, input, working_dir).map_err(|e| info_err!("Could not access epg file download directory: {}", e))?;

    if input.cache_duration_seconds > 0 {
        if let Ok(metadata) = tokio::fs::metadata(&persist_file_path).await {
            if let Ok(modified) = metadata.modified() {
                if let Ok(elapsed) = std::time::SystemTime::now().duration_since(modified) {
                    if elapsed.as_secs() < input.cache_duration_seconds {
                        debug!("Using cached epg file: {}", persist_file_path.display());
                        return Ok(persist_file_path);
                    }
                }
            }
        }
        // Cache miss: file doesn't exist
    }

    let lock_key: std::sync::Arc<str> = persist_file_path.display().to_string().into();
    let _input_lock = ctx.get_input_lock(&lock_key).await;

    if ctx.is_input_downloaded(&lock_key).await {
        return Ok(persist_file_path);
    }
    debug!("Downloading epg for input '{}'", input.name);
    match request::get_input_epg_content_as_file(&ctx.client, input, working_dir, url, &persist_file_path).await {
        Ok(path) => {
            ctx.mark_input_downloaded(lock_key.clone()).await;
            Ok(path)
        }
        Err(err) => Err(err)
    }
}

pub async fn get_xmltv(ctx: &PlaylistProcessingContext, input: &ConfigInput, working_dir: &str) -> (Option<TVGuide>, Vec<TuliproxError>) {
    match &input.epg {
        None => (None, vec![]),
        Some(epg_config) => {
            let mut errors = vec![];
            let mut file_paths = vec![];
            let mut stored_file_paths = vec![];

            for epg_source in &epg_config.sources {
                match download_epg_file(&epg_source.url, ctx, input, working_dir).await {
                    Ok(file_path) => {
                        stored_file_paths.push(file_path.clone());
                        file_paths.push(PersistedEpgSource { file_path, priority: epg_source.priority, logo_override: epg_source.logo_override });
                    }
                    Err(err) => {
                        errors.push(err);
                    }
                }
            }

            let _ = cleanup_unlisted_files_with_suffix(&stored_file_paths, "_epg.xml");

            if file_paths.is_empty() {
                (None, errors)
            } else {
                (Some(TVGuide::new(file_paths)), errors)
            }
        }
    }
}