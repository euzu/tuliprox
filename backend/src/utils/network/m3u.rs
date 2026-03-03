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
    let working_dir = &cfg.working_dir;
    let input_source: InputSource = {
        match input.staged.as_ref() {
            None => input.into(),
            Some(staged) => if staged.enabled { staged.into() } else { input.into() },
        }
    };
    let persist_file_path = prepare_file_path(input.persist.as_deref(), working_dir, "");
    match request::get_input_text_content_as_stream(
        app_config,
        client,
        &input_source,
        working_dir,
        persist_file_path,
    )
    .await
    {
        Ok(reader) => (m3u::parse_m3u(cfg, input, reader).await, vec![]),
        Err(err) => (vec![], vec![err]),
    }
}
