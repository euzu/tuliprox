use shared::error::{info_err};
use shared::error::{TuliproxError};
use crate::model::{ConfigInput, InputSource};
use shared::model::{PlaylistEntry, PlaylistItem, XtreamCluster};
use crate::utils::xtream;

pub(in crate::processing) async fn playlist_resolve_download_playlist_item(
    client: &reqwest::Client,
    pli: &PlaylistItem,
    input: &ConfigInput,
    errors: &mut Vec<TuliproxError>,
    resolve_delay: u16,
    cluster: XtreamCluster,
    default_user_agent: Option<&str>,
) -> Option<String> {
    let mut result = None;
    let provider_id = pli.get_provider_id()?;
    if let Some(info_url) = xtream::get_xtream_player_api_info_url(input, cluster, provider_id) {
        let input_source = InputSource::from(input).with_url(info_url);
        result = match xtream::get_xtream_stream_info_content(client, &input_source, true, default_user_agent).await {
            Ok(content) => Some(content),
            Err(err) => {
                errors.push(info_err!("{err}"));
                None
            }
        };
    }
    if resolve_delay > 0 {
        tokio::time::sleep(std::time::Duration::new(u64::from(resolve_delay), 0)).await;
    }
    result
}
