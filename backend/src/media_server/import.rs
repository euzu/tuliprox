use crate::{
    media_server::{
        media_server_catalog_snapshot_to_playlist, plex::PlexClient, refresh_media_server_catalog_complete_before_publish,
        MediaServerCatalogRefreshPolicy, MediaServerError,
    },
    model::ConfigInput,
};
use shared::{error::TuliproxError, model::{InputType, PlaylistGroup}};
use std::time::Duration;

pub async fn download_plex_media_server_playlist(
    client: &reqwest::Client,
    input: &ConfigInput,
) -> (Vec<PlaylistGroup>, Vec<TuliproxError>) {
    match load_plex_media_server_playlist(client, input).await {
        Ok(playlist) => (playlist, Vec::new()),
        Err(error) => (Vec::new(), vec![media_server_error_to_download_error(input, &error)]),
    }
}

async fn load_plex_media_server_playlist(
    client: &reqwest::Client,
    input: &ConfigInput,
) -> Result<Vec<PlaylistGroup>, MediaServerError> {
    if input.input_type != InputType::Plex {
        return Err(MediaServerError::new(crate::media_server::MediaServerErrorKind::MediaServerDiscoveryFailed)
            .provider("plex")
            .detail("input type is not plex"));
    }
    let plex = PlexClient::from_input(client.clone(), input).await?;
    let media_server = input.media_server.as_ref().ok_or_else(|| {
        MediaServerError::new(crate::media_server::MediaServerErrorKind::MediaServerDiscoveryFailed)
            .provider("plex")
            .detail("missing media_server configuration")
    })?;
    let policy = MediaServerCatalogRefreshPolicy {
        page_size: usize::from(media_server.catalog.page_size),
        request_delay: Duration::from_millis(media_server.catalog.request_delay_ms),
    };
    let snapshot = refresh_media_server_catalog_complete_before_publish(&plex, policy).await?;
    Ok(media_server_catalog_snapshot_to_playlist(&snapshot))
}

fn media_server_error_to_download_error(input: &ConfigInput, error: &MediaServerError) -> TuliproxError {
    TuliproxError::Download(format!("media-server input '{}' Plex catalog import failed: {error}", input.name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_server::{MediaServerError, MediaServerErrorKind};
    use shared::utils::Internable;

    #[test]
    fn media_server_import_error_is_redacted_for_download_errors() {
        let input = ConfigInput { name: "plex_media_server".intern(), ..ConfigInput::default() };
        let error = MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
            .provider("plex")
            .detail("https://pms.example.invalid/video?X-Plex-Token=secret-token");

        let rendered = media_server_error_to_download_error(&input, &error).to_string();

        assert!(!rendered.contains("secret-token"));
        assert!(rendered.contains("***"));
    }
}
