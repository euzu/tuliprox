use crate::{
    emby::EmbyCatalogClient, jellyfin::JellyfinCatalogClient, plex::client::PlexCatalogClient,
    MediaServerCatalogClient, MediaServerEpisode, MediaServerError, MediaServerImageRef, MediaServerLibrary,
    MediaServerLibraryRef, MediaServerMovie, MediaServerPage, MediaServerPageRequest, MediaServerSeason,
    MediaServerSeries, MediaServerStatus, MediaServerStreamRef, MediaServerStreamResponse,
};

/// Statically dispatched catalog client for any supported media-server kind.
///
/// `MediaServerCatalogClient` is not object safe, so callers that only learn the
/// kind from configuration hold this enum instead of a trait object.
pub enum AnyMediaServerCatalogClient {
    Plex(PlexCatalogClient),
    Emby(EmbyCatalogClient),
    Jellyfin(JellyfinCatalogClient),
}

/// Forwards one trait method call to whichever client the enum holds.
macro_rules! dispatch {
    ($client:expr, $method:ident($($arg:expr),*)) => {
        match $client {
            AnyMediaServerCatalogClient::Plex(client) => client.$method($($arg),*).await,
            AnyMediaServerCatalogClient::Emby(client) => client.$method($($arg),*).await,
            AnyMediaServerCatalogClient::Jellyfin(client) => client.$method($($arg),*).await,
        }
    };
}

impl MediaServerCatalogClient for AnyMediaServerCatalogClient {
    async fn discover(&self) -> Result<MediaServerStatus, MediaServerError> { dispatch!(self, discover()) }

    async fn list_libraries(&self) -> Result<Vec<MediaServerLibrary>, MediaServerError> {
        dispatch!(self, list_libraries())
    }

    async fn list_movies(
        &self,
        library: &MediaServerLibraryRef,
        page: MediaServerPageRequest,
    ) -> Result<MediaServerPage<MediaServerMovie>, MediaServerError> {
        dispatch!(self, list_movies(library, page))
    }

    async fn list_series(
        &self,
        library: &MediaServerLibraryRef,
        page: MediaServerPageRequest,
    ) -> Result<MediaServerPage<MediaServerSeries>, MediaServerError> {
        dispatch!(self, list_series(library, page))
    }

    async fn list_seasons(
        &self,
        library: &MediaServerLibraryRef,
        page: MediaServerPageRequest,
    ) -> Result<MediaServerPage<MediaServerSeason>, MediaServerError> {
        dispatch!(self, list_seasons(library, page))
    }

    async fn list_episodes(
        &self,
        library: &MediaServerLibraryRef,
        page: MediaServerPageRequest,
    ) -> Result<MediaServerPage<MediaServerEpisode>, MediaServerError> {
        dispatch!(self, list_episodes(library, page))
    }

    async fn open_stream(
        &self,
        stream_ref: &MediaServerStreamRef,
        range: Option<&str>,
    ) -> Result<MediaServerStreamResponse, MediaServerError> {
        dispatch!(self, open_stream(stream_ref, range))
    }

    async fn open_image(&self, image_ref: &MediaServerImageRef) -> Result<MediaServerStreamResponse, MediaServerError> {
        dispatch!(self, open_image(image_ref))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        emby::client::EmbyClientSettings, plex::client::PlexClientSettings, MediaServerErrorKind, MediaServerHttpClient,
    };
    use std::sync::Arc;

    const INPUT: &str = "any-client-input";
    const CLOSED_PORT_URL: &str = "http://127.0.0.1:1";

    fn http() -> MediaServerHttpClient { MediaServerHttpClient::new(reqwest::Client::new()) }

    fn emby_settings() -> EmbyClientSettings {
        EmbyClientSettings { token: Some("secret".to_string()), ..EmbyClientSettings::default() }
    }

    fn plex_settings() -> PlexClientSettings {
        PlexClientSettings { token: Some("secret".to_string()), ..PlexClientSettings::default() }
    }

    fn all_clients(url: &str) -> Vec<(&'static str, AnyMediaServerCatalogClient)> {
        let name: Arc<str> = Arc::from(INPUT);
        vec![
            (
                "plex",
                AnyMediaServerCatalogClient::Plex(PlexCatalogClient::new(name.clone(), url, &plex_settings(), http())),
            ),
            (
                "emby",
                AnyMediaServerCatalogClient::Emby(
                    EmbyCatalogClient::new(name.clone(), url, &emby_settings(), http()).expect("emby client"),
                ),
            ),
            (
                "jellyfin",
                AnyMediaServerCatalogClient::Jellyfin(
                    JellyfinCatalogClient::new(name, url, &emby_settings(), http()).expect("jellyfin client"),
                ),
            ),
        ]
    }

    fn foreign_stream_ref(provider: &str) -> MediaServerStreamRef {
        let (input_name, server_id, item_id): (Arc<str>, Arc<str>, Arc<str>) =
            (Arc::from(INPUT), Arc::from("server"), Arc::from("item"));
        // Each client is handed a ref of the next kind in the Plex -> Emby -> Jellyfin cycle.
        match provider {
            "plex" => MediaServerStreamRef::Emby { input_name, server_id, item_id, media_source_id: None },
            "emby" => MediaServerStreamRef::Jellyfin { input_name, server_id, item_id, media_source_id: None },
            _ => {
                MediaServerStreamRef::Plex { input_name, server_id, rating_key: item_id, part_key: Arc::from("/part") }
            }
        }
    }

    #[tokio::test]
    async fn open_stream_routes_to_each_variant_and_rejects_foreign_refs() {
        for (provider, client) in all_clients(CLOSED_PORT_URL) {
            let err = client
                .open_stream(&foreign_stream_ref(provider), None)
                .await
                .err()
                .unwrap_or_else(|| panic!("{provider} accepted a foreign stream ref"));

            assert_eq!(err.kind, MediaServerErrorKind::MediaServerStreamOpenFailed, "{provider}");
        }
    }

    #[tokio::test]
    async fn discover_routes_to_each_variant_and_fails_without_panicking_on_closed_port() {
        for (provider, client) in all_clients(CLOSED_PORT_URL) {
            assert!(client.discover().await.is_err(), "{provider} discovery must fail on a closed port");
        }
    }
}
