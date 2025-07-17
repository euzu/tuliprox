use shared::error::TuliproxError;
use crate::model::{ConfigInput, PersistedEpgSource};
use crate::model::TVGuide;
use crate::utils::{add_prefix_to_filename, cleanup_unlisted_files_with_suffix, prepare_file_path};
use crate::utils::request;
use log::debug;
use std::path::PathBuf;
use std::sync::Arc;
use shared::utils::{sanitize_sensitive_info, short_hash};

async fn download_epg_file(url: &str, client: &Arc<reqwest::Client>, input: &ConfigInput, working_dir: &str) -> Result<PathBuf, TuliproxError> {
    debug!("Getting epg file path for url: {}", sanitize_sensitive_info(url));
    let file_prefix = short_hash(url);
    let persist_file_path = prepare_file_path(input.persist.as_deref(), working_dir, "")
        .map(|path| add_prefix_to_filename(&path, format!("{file_prefix}_epg_").as_str(), Some("xml")));

    request::get_input_epg_content_as_file(Arc::clone(client), input, working_dir, url, persist_file_path).await
}

pub async fn get_xmltv(client: Arc<reqwest::Client>, input: &ConfigInput, working_dir: &str) -> (Option<TVGuide>, Vec<TuliproxError>) {
    match &input.epg {
        None => (None, vec![]),
        Some(epg_config) => {
            let mut errors = vec![];
            let mut file_paths = vec![];
            let mut stored_file_paths = vec![];

            for epg_source in &epg_config.sources {
                match download_epg_file(&epg_source.url, &client, input, working_dir).await {
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