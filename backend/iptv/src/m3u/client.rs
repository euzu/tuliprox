use shared::{error::TuliproxError, model::PlaylistGroup};
use std::sync::Arc;
use tuliprox_core::{
    model::{AppConfig, Config, ConfigInput, InputSource},
    utils::{prepare_file_path, request},
};
use tuliprox_parser::m3u;

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
    match download_reader(app_config, client, cfg, input, explicit_source).await {
        Ok(reader) => (m3u::parse_m3u(cfg, input, reader).await, vec![]),
        Err(err) => (vec![], vec![err]),
    }
}

pub(crate) async fn download_m3u_playlist_for_update(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    cfg: &Arc<Config>,
    input: &ConfigInput,
) -> (Vec<PlaylistGroup>, Vec<TuliproxError>) {
    match download_reader(app_config, client, cfg, input, None).await {
        Ok(reader) => match m3u::parse_m3u_for_update(cfg, input, reader).await {
            Ok(groups) => (groups, vec![]),
            Err(error) => (vec![], vec![TuliproxError::Download(format!("M3U update parsing failed: {error}"))]),
        },
        Err(error) => (vec![], vec![error]),
    }
}

async fn download_reader(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    cfg: &Config,
    input: &ConfigInput,
    explicit_source: Option<InputSource>,
) -> Result<request::DynReader, TuliproxError> {
    let storage_dir = &cfg.storage_dir;
    let input_source: InputSource = explicit_source.unwrap_or_else(|| input.into());
    let persist_file_path = prepare_file_path(input.persist.as_deref(), storage_dir, "");
    request::get_input_text_content_as_stream(app_config, client, &input_source, storage_dir, persist_file_path).await
}
