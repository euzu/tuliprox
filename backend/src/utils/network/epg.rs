use shared::error::TuliproxError;
use crate::model::{ConfigInput, PersistedEpgSource};
use crate::model::TVGuide;
use crate::utils::{add_prefix_to_filename, cleanup_unlisted_files_with_suffix, prepare_file_path};
use crate::utils::request;
use log::debug;
use std::path::PathBuf;
use shared::utils::{sanitize_sensitive_info, short_hash};

fn get_input_raw_epg_file_path(url: &str, input: &ConfigInput, working_dir: &str) -> Option<PathBuf> {
    let file_prefix = short_hash(url);
    prepare_file_path(input.persist.as_deref(), working_dir, "")
        .map(|path| add_prefix_to_filename(&path, format!("{file_prefix}_epg_").as_str(), Some("xml")))
}

async fn download_epg_file(url: &str, client: &reqwest::Client, input: &ConfigInput, working_dir: &str) -> Result<PathBuf, TuliproxError> {
    debug!("Getting epg file path for url: {}", sanitize_sensitive_info(url));
    let persist_file_path = get_input_raw_epg_file_path(url, input, working_dir);

    if input.cache_duration_seconds > 0 {
        if let Some(path) = &persist_file_path {
            if path.exists() {
                if let Ok(metadata) = tokio::fs::metadata(path).await {
                    if let Ok(modified) = metadata.modified() {
                        if let Ok(elapsed) = std::time::SystemTime::now().duration_since(modified) {
                            if elapsed.as_secs() < input.cache_duration_seconds {
                                debug!("Using cached epg file: {}", path.display());
                                return Ok(path.clone());
                            }
                        }
                    }
                }
            }
        }
    }

    request::get_input_epg_content_as_file(client, input, working_dir, url, persist_file_path).await
}

pub async fn get_xmltv(client: &reqwest::Client, input: &ConfigInput, working_dir: &str) -> (Option<TVGuide>, Vec<TuliproxError>) {
    match &input.epg {
        None => (None, vec![]),
        Some(epg_config) => {
            let mut errors = vec![];
            let mut file_paths = vec![];
            let mut stored_file_paths = vec![];

            for epg_source in &epg_config.sources {
                match download_epg_file(&epg_source.url, client, input, working_dir).await {
                    Ok(file_path) => {
                        stored_file_paths.push(file_path.clone());
                        file_paths.push(PersistedEpgSource {file_path, priority: epg_source.priority, logo_override: epg_source.logo_override});
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