use crate::model::{AppConfig, Config, ConfigInput, InputSource};
use crate::processing::parser::m3u;
use crate::utils::prepare_file_path;
use crate::utils::request;
use shared::error::TuliproxError;
use shared::model::PlaylistGroup;
use std::sync::Arc;

pub async fn download_m3u_playlist(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    cfg: &Arc<Config>,
    input: &ConfigInput,
) -> (Vec<PlaylistGroup>, Vec<TuliproxError>) {
    download_m3u_playlist_from_source(app_config, client, cfg, input, None).await
}

pub async fn download_m3u_playlist_from_source(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    cfg: &Arc<Config>,
    input: &ConfigInput,
    explicit_source: Option<InputSource>,
) -> (Vec<PlaylistGroup>, Vec<TuliproxError>) {
    let storage_dir = &cfg.storage_dir;
    let input_source: InputSource = explicit_source.unwrap_or_else(|| {
        match input.staged.as_ref() {
            None => input.into(),
            Some(staged) => if staged.enabled { staged.into() } else { input.into() },
        }
    });
    let persist_file_path = prepare_file_path(input.persist.as_deref(), storage_dir, "");
    match request::get_input_text_content_as_stream(
        app_config,
        client,
        &input_source,
        storage_dir,
        persist_file_path,
    )
    .await
    {
        Ok(reader) => (m3u::parse_m3u(cfg, input, reader).await, vec![]),
        Err(err) => (vec![], vec![err]),
    }
}
