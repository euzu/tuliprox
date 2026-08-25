use crate::media_server::plex::dto::{PlexMediaContainerDto, PlexResourcesDto, PlexSectionDto, PlexSectionsDto};
use crate::media_server::plex::mapper::{
    plex_directory_to_season, plex_directory_to_series, plex_section_matches_selector, plex_section_to_library,
    plex_video_to_episode, plex_video_to_movie,
};
use crate::media_server::{
    encode_url_path_segment, MediaServerCatalogClient, MediaServerEpisode, MediaServerError, MediaServerErrorKind,
    MediaServerHttpClient, MediaServerImageRef, MediaServerKind, MediaServerLibrary, MediaServerLibraryRef,
    MediaServerMovie, MediaServerPage, MediaServerPageRequest, MediaServerResourceResponse, MediaServerSeason,
    MediaServerSeries, MediaServerStatus, MediaServerStreamRef, MediaServerStreamResponse,
};
use futures::{StreamExt, TryStreamExt};
use reqwest::header::{HeaderName, HeaderValue, RANGE};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use shared::model::MediaServerLibrarySelector;
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock},
};
use tokio::sync::Mutex;
use url::Url;

const PLEX_TV_RESOURCES_URL: &str = "https://plex.tv/api/resources";
const X_PLEX_TOKEN: HeaderName = HeaderName::from_static("x-plex-token");
static PLEX_CONNECTION_CACHE: LazyLock<Mutex<HashMap<PlexConnectionCacheKey, PlexConnectionState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Everything a Plex catalog client needs in order to connect.
///
/// This is the boundary type: the caller resolves its own configuration - from a
/// file, from environment variables, from anywhere - and hands over the resolved
/// values. The client does not name the application's configuration model, which
/// is what keeps this module a leaf.
#[derive(Debug, Clone, Default)]
pub struct PlexClientSettings {
    pub token: Option<String>,
    pub account_token: Option<String>,
    pub server_id: Option<String>,
    pub server_name: Option<String>,
    pub prefer_https: bool,
    pub allow_relay: bool,
    pub libraries: Vec<MediaServerLibrarySelector>,
}

#[derive(Debug, Clone)]
struct PlexClientConfig {
    input_name: Arc<str>,
    direct_url: Option<Arc<str>>,
    token: Option<Arc<str>>,
    account_token: Option<Arc<str>>,
    server_id: Option<Arc<str>>,
    server_name: Option<Arc<str>>,
    prefer_https: bool,
    allow_relay: bool,
    libraries: Vec<MediaServerLibrarySelector>,
}

#[derive(Debug, Clone)]
struct PlexConnectionState {
    base_url: Arc<str>,
    token: Arc<str>,
    status: MediaServerStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PlexConnectionCacheKey {
    input_name: Arc<str>,
    direct_url: Option<Arc<str>>,
    token: Option<Arc<str>>,
    account_token: Option<Arc<str>>,
    server_id: Option<Arc<str>>,
    server_name: Option<Arc<str>>,
    prefer_https: bool,
    allow_relay: bool,
}

pub struct PlexCatalogClient {
    http: MediaServerHttpClient,
    config: PlexClientConfig,
    connection: Mutex<Option<PlexConnectionState>>,
}

impl PlexCatalogClient {
    pub fn new(
        input_name: Arc<str>,
        input_url: &str,
        media_server: &PlexClientSettings,
        http: MediaServerHttpClient,
    ) -> Self {
        Self {
            http,
            config: PlexClientConfig {
                input_name,
                direct_url: non_blank(input_url).map(Arc::<str>::from),
                token: media_server.token.as_deref().and_then(non_blank).map(Arc::<str>::from),
                account_token: media_server.account_token.as_deref().and_then(non_blank).map(Arc::<str>::from),
                server_id: media_server.server_id.as_deref().and_then(non_blank).map(Arc::<str>::from),
                server_name: media_server.server_name.as_deref().and_then(non_blank).map(Arc::<str>::from),
                prefer_https: media_server.prefer_https,
                allow_relay: media_server.allow_relay,
                libraries: media_server.libraries.clone(),
            },
            connection: Mutex::new(None),
        }
    }

    async fn connection(&self) -> Result<PlexConnectionState, MediaServerError> {
        if let Some(connection) = self.connection.lock().await.clone() {
            return Ok(connection);
        }

        let cache_key = self.connection_cache_key();
        if let Some(connection) = PLEX_CONNECTION_CACHE.lock().await.get(&cache_key).cloned() {
            *self.connection.lock().await = Some(connection.clone());
            return Ok(connection);
        }

        let connection = if self.config.direct_url.is_some() {
            self.discover_direct().await?
        } else {
            self.discover_via_myplex().await?
        };
        PLEX_CONNECTION_CACHE.lock().await.insert(cache_key, connection.clone());
        *self.connection.lock().await = Some(connection.clone());
        Ok(connection)
    }

    fn connection_cache_key(&self) -> PlexConnectionCacheKey {
        PlexConnectionCacheKey {
            input_name: Arc::clone(&self.config.input_name),
            direct_url: self.config.direct_url.clone(),
            token: self.config.token.clone(),
            account_token: self.config.account_token.clone(),
            server_id: self.config.server_id.clone(),
            server_name: self.config.server_name.clone(),
            prefer_https: self.config.prefer_https,
            allow_relay: self.config.allow_relay,
        }
    }

    async fn discover_direct(&self) -> Result<PlexConnectionState, MediaServerError> {
        let base_url = self.config.direct_url.clone().ok_or_else(|| {
            MediaServerError::new(MediaServerErrorKind::MediaServerDiscoveryFailed)
                .provider("plex")
                .detail("direct Plex discovery requires a PMS URL")
        })?;
        let token = self.config.token.clone().ok_or_else(|| {
            MediaServerError::new(MediaServerErrorKind::MediaServerAuthDenied)
                .provider("plex")
                .detail("direct Plex discovery requires media_server.token")
        })?;
        let identity = self.fetch_identity(&base_url, &token).await?;
        verify_direct_selectors(&self.config, &identity)?;
        let status = identity.into_status(None, None)?;
        Ok(PlexConnectionState { base_url, token, status })
    }

    async fn discover_via_myplex(&self) -> Result<PlexConnectionState, MediaServerError> {
        let account_token = self.config.account_token.clone().ok_or_else(|| {
            MediaServerError::new(MediaServerErrorKind::MediaServerAuthDenied)
                .provider("plex")
                .detail("Plex resource discovery requires media_server.account_token")
        })?;
        let resources: PlexResourcesDto = self.get_xml(PLEX_TV_RESOURCES_URL, &account_token, PlexOperation::Discovery).await?;
        let resource = select_resource(&resources.devices, &self.config)?;
        let resource_token = resource.access_token.as_deref().and_then(non_blank).map(Arc::<str>::from).ok_or_else(|| {
            MediaServerError::new(MediaServerErrorKind::MediaServerAuthDenied)
                .provider("plex")
                .detail("selected Plex resource did not expose a PMS access token")
        })?;
        let candidates = selected_connection_urls(resource, self.config.prefer_https, self.config.allow_relay)?;

        let mut last_error = None;
        for candidate in candidates {
            match self.fetch_identity(&candidate, &resource_token).await {
                Ok(identity) => {
                    verify_resource_identity(resource, &identity)?;
                    let status = identity.into_status(resource.client_identifier.as_deref(), resource.owned.map(|owned| owned != 0))?;
                    return Ok(PlexConnectionState { base_url: candidate, token: resource_token, status });
                }
                Err(error) => last_error = Some(error),
            }
        }

        Err(last_error.unwrap_or_else(|| {
            MediaServerError::new(MediaServerErrorKind::MediaServerUnavailable)
                .provider("plex")
                .detail("no selected Plex connection could be verified")
        }))
    }

    async fn fetch_identity(&self, base_url: &Arc<str>, token: &Arc<str>) -> Result<PlexIdentityDto, MediaServerError> {
        let url = pms_url(base_url, "/identity")?;
        self.get_xml(&url, token, PlexOperation::Discovery).await
    }

    async fn fetch_sections(&self, connection: &PlexConnectionState) -> Result<PlexSectionsDto, MediaServerError> {
        let url = pms_url(&connection.base_url, "/library/sections")?;
        self.get_xml(&url, &connection.token, PlexOperation::Catalog).await
    }

    async fn fetch_catalog_page(
        &self,
        connection: &PlexConnectionState,
        library: &MediaServerLibraryRef,
        page: MediaServerPageRequest,
        metadata_type: Option<u8>,
        include_guids: bool,
    ) -> Result<PlexMediaContainerDto, MediaServerError> {
        let section = encode_url_path_segment(&library.library_id);
        let mut url = pms_url(&connection.base_url, &format!("/library/sections/{section}/all"))?;
        append_query_pair(&mut url, "X-Plex-Container-Start", &page.start.to_string());
        append_query_pair(&mut url, "X-Plex-Container-Size", &page.limit.to_string());
        if let Some(metadata_type) = metadata_type {
            append_query_pair(&mut url, "type", &metadata_type.to_string());
        }
        if include_guids {
            append_query_pair(&mut url, "includeGuids", "1");
        }
        self.get_xml(&url, &connection.token, PlexOperation::Catalog).await
    }

    async fn get_xml<T>(&self, url: &str, token: &Arc<str>, operation: PlexOperation) -> Result<T, MediaServerError>
    where
        T: DeserializeOwned,
    {
        let response = self.get_response(url, token, operation).await?;
        let body = response.text().await.map_err(|err| {
            MediaServerError::from_reqwest_error_with_fallback(
                &err,
                operation.not_found_kind(),
                operation.fallback_kind(),
            )
            .provider("plex")
            .detail("Plex response body read failed")
        })?;
        quick_xml::de::from_str(&body).map_err(|err| {
            MediaServerError::new(operation.fallback_kind())
                .provider("plex")
                .detail(format!("Plex XML decode failed: {err}"))
        })
    }

    async fn get_response(
        &self,
        url: &str,
        token: &Arc<str>,
        operation: PlexOperation,
    ) -> Result<reqwest::Response, MediaServerError> {
        let token = HeaderValue::from_str(token).map_err(|_| {
            MediaServerError::new(MediaServerErrorKind::MediaServerAuthDenied)
                .provider("plex")
                .detail("Plex token contains invalid header characters")
        })?;
        let request = match operation {
            PlexOperation::Discovery => self.http.request(Method::GET, url).discovery_errors(),
            PlexOperation::Catalog => self.http.request(Method::GET, url).catalog_errors(),
            PlexOperation::Playback => self.http.request(Method::GET, url).playback_errors(),
        };
        let response = request.header(X_PLEX_TOKEN, token).send_with_error_detail("Plex request failed").await?;
        let status = response.status();
        if !status.is_success() {
            return Err(operation.status_error(status).detail(format!("Plex request returned {status}")));
        }
        Ok(response)
    }
}

#[allow(async_fn_in_trait)]
impl MediaServerCatalogClient for PlexCatalogClient {
    async fn discover(&self) -> Result<MediaServerStatus, MediaServerError> {
        Ok(self.connection().await?.status)
    }

    async fn list_libraries(&self) -> Result<Vec<MediaServerLibrary>, MediaServerError> {
        let connection = self.connection().await?;
        let sections = self.fetch_sections(&connection).await?;
        select_libraries(&self.config, &connection.status.server_id, &sections.directories)
    }

    async fn list_movies(
        &self,
        library: &MediaServerLibraryRef,
        page: MediaServerPageRequest,
    ) -> Result<MediaServerPage<MediaServerMovie>, MediaServerError> {
        let connection = self.connection().await?;
        let container = self.fetch_catalog_page(&connection, library, page, Some(1), true).await?;
        let upstream_item_count = container.upstream_item_count();
        let items = container
            .videos
            .iter()
            .filter_map(|video| plex_video_to_movie(&self.config.input_name, &connection.status.server_id, &library.library_id, video))
            .collect();
        Ok(MediaServerPage::with_upstream_item_count(page, container.total_size, upstream_item_count, items))
    }

    async fn list_series(
        &self,
        library: &MediaServerLibraryRef,
        page: MediaServerPageRequest,
    ) -> Result<MediaServerPage<MediaServerSeries>, MediaServerError> {
        let connection = self.connection().await?;
        let container = self.fetch_catalog_page(&connection, library, page, None, true).await?;
        let upstream_item_count = container.upstream_item_count();
        let items = container
            .directories
            .iter()
            .filter(|directory| directory.item_type.as_deref().is_none_or(|item_type| item_type.eq_ignore_ascii_case("show")))
            .filter_map(|directory| {
                plex_directory_to_series(&self.config.input_name, &connection.status.server_id, &library.library_id, directory)
            })
            .collect();
        Ok(MediaServerPage::with_upstream_item_count(page, container.total_size, upstream_item_count, items))
    }

    async fn list_seasons(
        &self,
        library: &MediaServerLibraryRef,
        page: MediaServerPageRequest,
    ) -> Result<MediaServerPage<MediaServerSeason>, MediaServerError> {
        let connection = self.connection().await?;
        let container = self.fetch_catalog_page(&connection, library, page, Some(3), true).await?;
        let upstream_item_count = container.upstream_item_count();
        let items = container
            .directories
            .iter()
            .filter(|directory| directory.item_type.as_deref().is_none_or(|item_type| item_type.eq_ignore_ascii_case("season")))
            .filter_map(|directory| {
                plex_directory_to_season(&self.config.input_name, &connection.status.server_id, &library.library_id, directory)
            })
            .collect();
        Ok(MediaServerPage::with_upstream_item_count(page, container.total_size, upstream_item_count, items))
    }

    async fn list_episodes(
        &self,
        library: &MediaServerLibraryRef,
        page: MediaServerPageRequest,
    ) -> Result<MediaServerPage<MediaServerEpisode>, MediaServerError> {
        let connection = self.connection().await?;
        let container = self.fetch_catalog_page(&connection, library, page, Some(4), false).await?;
        let upstream_item_count = container.upstream_item_count();
        let items = container
            .videos
            .iter()
            .filter_map(|video| plex_video_to_episode(&self.config.input_name, &connection.status.server_id, &library.library_id, video))
            .collect();
        Ok(MediaServerPage::with_upstream_item_count(page, container.total_size, upstream_item_count, items))
    }

    async fn open_stream(
        &self,
        stream_ref: &MediaServerStreamRef,
        range: Option<&str>,
    ) -> Result<MediaServerStreamResponse, MediaServerError> {
        let MediaServerStreamRef::Plex {
            input_name,
            server_id,
            rating_key,
            part_key,
        } = stream_ref else {
            return Err(MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
                .provider("plex")
                .detail("Plex stream open received a non-Plex stream ref"));
        };
        if input_name.as_ref() != self.config.input_name.as_ref() {
            return Err(MediaServerError::new(MediaServerErrorKind::MediaServerItemNotFound)
                .provider("plex")
                .detail("Plex stream ref did not belong to the selected input"));
        }
        if non_blank(rating_key.as_ref()).is_none() {
            return Err(MediaServerError::new(MediaServerErrorKind::NoDirectPlayableMediaServerSource)
                .provider("plex")
                .detail("Plex stream ref is missing a stable rating key"));
        }

        let connection = self.connection().await?;
        if server_id.as_ref() != connection.status.server_id.as_ref() {
            return Err(MediaServerError::new(MediaServerErrorKind::MediaServerItemNotFound)
                .provider("plex")
                .detail("Plex stream ref did not belong to the selected server"));
        }

        let url = pms_part_url(&connection.base_url, part_key)?;
        let token = HeaderValue::from_str(connection.token.as_ref()).map_err(|_| {
            MediaServerError::new(MediaServerErrorKind::MediaServerAuthDenied)
                .provider("plex")
                .detail("Plex token contains invalid header characters")
        })?;
        let mut request = self
            .http
            .request(Method::GET, &url)
            .playback_errors()
            .header(X_PLEX_TOKEN, token);
        if let Some(range) = range.and_then(non_blank) {
            let range = HeaderValue::from_str(range).map_err(|_| {
                MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
                    .provider("plex")
                    .detail("Plex Range header contains invalid characters")
            })?;
            request = request.header(RANGE, range);
        }

        let response = request.send_with_error_detail("Plex stream request failed").await?;
        let status = response.status();
        if !status.is_success() {
            return Err(PlexOperation::Playback
                .status_error(status)
                .detail(format!("Plex stream request returned {status}")));
        }
        let headers = response.headers().clone();
        let body = response
            .bytes_stream()
            .map_err(|err| {
                MediaServerError::from_reqwest_error_with_fallback(
                    &err,
                    PlexOperation::Playback.not_found_kind(),
                    PlexOperation::Playback.fallback_kind(),
                )
                .provider("plex")
                .detail("Plex stream body read failed")
            })
            .boxed();
        Ok(MediaServerStreamResponse { status, headers, body })
    }

    async fn open_image(&self, image_ref: &MediaServerImageRef) -> Result<MediaServerResourceResponse, MediaServerError> {
        let MediaServerImageRef::Plex {
            input_name,
            server_id,
            rating_key,
            image_path,
        } = image_ref else {
            return Err(MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
                .provider("plex")
                .detail("Plex image open received a non-Plex image ref"));
        };
        if input_name.as_ref() != self.config.input_name.as_ref() {
            return Err(MediaServerError::new(MediaServerErrorKind::MediaServerItemNotFound)
                .provider("plex")
                .detail("Plex image ref did not belong to the selected input"));
        }
        if non_blank(rating_key.as_ref()).is_none() {
            return Err(MediaServerError::new(MediaServerErrorKind::MediaServerItemNotFound)
                .provider("plex")
                .detail("Plex image ref is missing a stable rating key"));
        }

        let connection = self.connection().await?;
        if server_id.as_ref() != connection.status.server_id.as_ref() {
            return Err(MediaServerError::new(MediaServerErrorKind::MediaServerItemNotFound)
                .provider("plex")
                .detail("Plex image ref did not belong to the selected server"));
        }

        let url = pms_image_url(&connection.base_url, image_path)?;
        let response: reqwest::Response = self.get_response(&url, &connection.token, PlexOperation::Playback).await?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.bytes().await.map_err(|err| {
            MediaServerError::from_reqwest_error_with_fallback(
                &err,
                PlexOperation::Playback.not_found_kind(),
                PlexOperation::Playback.fallback_kind(),
            )
            .provider("plex")
            .detail("Plex image body read failed")
        })?;
        Ok(MediaServerResourceResponse { status, headers, body })
    }
}

#[derive(Debug, Copy, Clone)]
enum PlexOperation {
    Discovery,
    Catalog,
    Playback,
}

impl PlexOperation {
    const fn not_found_kind(self) -> MediaServerErrorKind {
        match self {
            Self::Discovery => MediaServerErrorKind::MediaServerUnavailable,
            Self::Catalog => MediaServerErrorKind::MediaServerLibraryUnavailable,
            Self::Playback => MediaServerErrorKind::MediaServerItemNotFound,
        }
    }

    const fn fallback_kind(self) -> MediaServerErrorKind {
        match self {
            Self::Discovery => MediaServerErrorKind::MediaServerDiscoveryFailed,
            Self::Catalog => MediaServerErrorKind::MediaServerCatalogDecodeFailed,
            Self::Playback => MediaServerErrorKind::MediaServerStreamOpenFailed,
        }
    }

    fn status_error(self, status: StatusCode) -> MediaServerError {
        MediaServerError::from_http_status_with_fallback(status, self.not_found_kind(), self.fallback_kind()).provider("plex")
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename = "MediaContainer")]
struct PlexIdentityDto {
    #[serde(rename = "@machineIdentifier")]
    machine_identifier: Option<String>,
    #[serde(rename = "@friendlyName")]
    friendly_name: Option<String>,
    #[serde(rename = "@version")]
    version: Option<String>,
}

impl PlexIdentityDto {
    fn into_status(self, fallback_server_id: Option<&str>, owned: Option<bool>) -> Result<MediaServerStatus, MediaServerError> {
        let server_id = self
            .machine_identifier
            .as_deref()
            .and_then(non_blank)
            .or_else(|| fallback_server_id.and_then(non_blank))
            .map(Arc::<str>::from)
            .ok_or_else(|| {
                MediaServerError::new(MediaServerErrorKind::MediaServerDiscoveryFailed)
                    .provider("plex")
                    .detail("Plex PMS identity did not expose a stable machine identifier")
            })?;
        Ok(MediaServerStatus {
            kind: MediaServerKind::Plex,
            server_id,
            display_name: self.friendly_name.as_deref().and_then(non_blank).map(Arc::<str>::from),
            version: self.version.as_deref().and_then(non_blank).map(Arc::<str>::from),
            owned,
        })
    }
}

impl PlexMediaContainerDto {
    fn upstream_item_count(&self) -> usize {
        self.size.unwrap_or_else(|| self.videos.len().saturating_add(self.directories.len()))
    }
}

fn select_libraries(
    config: &PlexClientConfig,
    server_id: &Arc<str>,
    sections: &[PlexSectionDto],
) -> Result<Vec<MediaServerLibrary>, MediaServerError> {
    let mut libraries = Vec::new();
    for selector in &config.libraries {
        let matches = sections.iter().filter(|section| plex_section_matches_selector(section, selector)).collect::<Vec<_>>();
        match matches.as_slice() {
            [] => {
                return Err(MediaServerError::new(MediaServerErrorKind::MediaServerLibraryUnavailable)
                    .provider("plex")
                    .detail("selected Plex library was not visible"));
            }
            [section] => {
                let library = plex_section_to_library(&config.input_name, server_id, section).ok_or_else(|| {
                    MediaServerError::new(MediaServerErrorKind::MediaServerLibraryUnavailable)
                        .provider("plex")
                        .detail("selected Plex library did not expose a stable key")
                })?;
                libraries.push(library);
            }
            _ => {
                return Err(MediaServerError::new(MediaServerErrorKind::MediaServerLibraryUnavailable)
                    .provider("plex")
                    .detail("selected Plex library name was ambiguous"));
            }
        }
    }
    Ok(libraries)
}

fn select_resource<'a>(
    resources: &'a [crate::media_server::plex::dto::PlexResourceDto],
    config: &PlexClientConfig,
) -> Result<&'a crate::media_server::plex::dto::PlexResourceDto, MediaServerError> {
    let plex_servers = resources
        .iter()
        .filter(|resource| {
            resource
                .product
                .as_deref()
                .is_some_and(|product| product.eq_ignore_ascii_case("Plex Media Server"))
        })
        .collect::<Vec<_>>();

    let matches = if let Some(server_id) = config.server_id.as_deref() {
        plex_servers.into_iter().filter(|resource| resource_matches_server_id(resource, server_id)).collect::<Vec<_>>()
    } else if let Some(server_name) = config.server_name.as_deref() {
        plex_servers
            .into_iter()
            .filter(|resource| resource.name.as_deref().is_some_and(|value| value == server_name))
            .collect::<Vec<_>>()
    } else {
        return Err(MediaServerError::new(MediaServerErrorKind::MediaServerDiscoveryFailed)
            .provider("plex")
            .detail("Plex resource discovery requires a server selector"));
    };

    match matches.as_slice() {
        [] => Err(MediaServerError::new(MediaServerErrorKind::MediaServerDiscoveryFailed)
            .provider("plex")
            .detail("no Plex resource matched the configured selector")),
        [resource] => Ok(resource),
        _ => Err(MediaServerError::new(MediaServerErrorKind::MediaServerDiscoveryFailed)
            .provider("plex")
            .detail("configured Plex server selector matched multiple resources")),
    }
}

fn selected_connection_urls(
    resource: &crate::media_server::plex::dto::PlexResourceDto,
    prefer_https: bool,
    allow_relay: bool,
) -> Result<Vec<Arc<str>>, MediaServerError> {
    let mut candidates = resource
        .connections
        .iter()
        .filter_map(|connection| {
            let uri = connection.uri.as_deref().and_then(non_blank).map(Arc::<str>::from)?;
            let parsed = Url::parse(&uri).ok()?;
            let scheme = parsed.scheme();
            if !matches!(scheme, "http" | "https") {
                return None;
            }
            let relay = connection.relay.unwrap_or_default() != 0;
            if relay && !allow_relay {
                return None;
            }
            Some((uri, relay, scheme == "https", connection.local.unwrap_or_default() != 0))
        })
        .collect::<Vec<_>>();

    candidates.sort_by_key(|(_, relay, https, local)| (*relay, https_sort_key(*https, prefer_https), !*local));
    let urls = candidates.into_iter().map(|(uri, _, _, _)| uri).collect::<Vec<_>>();
    if urls.is_empty() {
        return Err(MediaServerError::new(MediaServerErrorKind::MediaServerUnavailable)
            .provider("plex")
            .detail("selected Plex resource did not expose a usable PMS connection"));
    }
    Ok(urls)
}

const fn https_sort_key(is_https: bool, prefer_https: bool) -> u8 {
    if prefer_https && is_https {
        0
    } else if prefer_https {
        1
    } else {
        0
    }
}

fn verify_direct_selectors(config: &PlexClientConfig, identity: &PlexIdentityDto) -> Result<(), MediaServerError> {
    if let Some(server_id) = config.server_id.as_deref() {
        verify_selector(identity.machine_identifier.as_deref(), server_id, "configured Plex server selector did not match PMS identity")?;
    }
    if let Some(server_name) = config.server_name.as_deref() {
        verify_selector(identity.friendly_name.as_deref(), server_name, "configured Plex server name did not match PMS identity")?;
    }
    Ok(())
}

fn resource_matches_server_id(resource: &crate::media_server::plex::dto::PlexResourceDto, server_id: &str) -> bool {
    // For Plex Media Server resources, MyPlex exposes the stable server identity as
    // clientIdentifier. PMS /identity exposes the same identity as machineIdentifier.
    resource.client_identifier.as_deref().is_some_and(|value| value == server_id)
        || resource.machine_identifier.as_deref().is_some_and(|value| value == server_id)
}

fn verify_resource_identity(
    resource: &crate::media_server::plex::dto::PlexResourceDto,
    identity: &PlexIdentityDto,
) -> Result<(), MediaServerError> {
    let expected_server_id = resource.machine_identifier.as_deref().or(resource.client_identifier.as_deref());
    if let Some(expected_server_id) = expected_server_id {
        verify_selector(
            identity.machine_identifier.as_deref(),
            expected_server_id,
            "selected Plex resource identity did not match PMS identity",
        )?;
    }
    Ok(())
}

fn verify_selector(actual: Option<&str>, expected: &str, detail: &'static str) -> Result<(), MediaServerError> {
    if actual.is_some_and(|actual| actual == expected) {
        Ok(())
    } else {
        Err(MediaServerError::new(MediaServerErrorKind::MediaServerDiscoveryFailed)
            .provider("plex")
            .detail(detail))
    }
}

fn pms_url(base_url: &Arc<str>, path: &str) -> Result<String, MediaServerError> {
    let base = Url::parse(base_url).map_err(|_| plex_invalid_pms_url())?;
    let mut url = base;
    url.set_path(path);
    url.set_query(None);
    Ok(url.to_string())
}

fn pms_part_url(base_url: &Arc<str>, part_key: &str) -> Result<String, MediaServerError> {
    resolve_pms_resource_url(
        base_url,
        part_key,
        "/library/parts/",
        MediaServerErrorKind::NoDirectPlayableMediaServerSource,
        "part_key",
    )
}

fn pms_image_url(base_url: &Arc<str>, image_path: &str) -> Result<String, MediaServerError> {
    resolve_pms_resource_url(
        base_url,
        image_path,
        "/library/metadata/",
        MediaServerErrorKind::MediaServerItemNotFound,
        "image_path",
    )
}

/// Shared body of `pms_part_url` and `pms_image_url`. Both resolve a path
/// from `base_url`, validate the same origin/prefix/credential invariants, and
/// only differ in the expected PMS path prefix, the `MediaServerErrorKind`
/// they surface, and the human-readable field name used in error details.
fn resolve_pms_resource_url(
    base_url: &Arc<str>,
    resource_path: &str,
    expected_prefix: &str,
    error_kind: MediaServerErrorKind,
    field_name: &str,
) -> Result<String, MediaServerError> {
    let resource_path = non_blank(resource_path).ok_or_else(|| {
        MediaServerError::new(error_kind)
            .provider("plex")
            .detail(format!("Plex {field_name} is missing"))
    })?;
    if !resource_path.starts_with(expected_prefix)
        || resource_path.starts_with("//")
        || resource_path.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(MediaServerError::new(error_kind)
            .provider("plex")
            .detail(format!("Plex {field_name} is not a {expected_prefix} resource")));
    }

    let base = Url::parse(base_url).map_err(|_| plex_invalid_pms_url())?;
    let expected_origin = base.origin().ascii_serialization();
    let url = base.join(resource_path).map_err(|_| {
        MediaServerError::new(error_kind)
            .provider("plex")
            .detail(format!("Plex {field_name} could not be resolved against the selected PMS"))
    })?;
    if url.origin().ascii_serialization() != expected_origin || url.fragment().is_some() {
        return Err(MediaServerError::new(error_kind)
            .provider("plex")
            .detail(format!("Plex {field_name} did not resolve to the selected PMS origin")));
    }
    if !url.path().starts_with(expected_prefix) {
        return Err(MediaServerError::new(error_kind)
            .provider("plex")
            .detail(format!("Plex {field_name} did not resolve to a {expected_prefix} resource")));
    }
    if url.query_pairs().any(|(name, _)| is_sensitive_plex_part_query_name(&name)) {
        return Err(MediaServerError::new(error_kind)
            .provider("plex")
            .detail(format!("Plex {field_name} must not carry credential query parameters")));
    }
    Ok(url.to_string())
}

fn is_sensitive_plex_part_query_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "x-plex-token" || lower == "token" || lower.ends_with("_token") || lower == "api_key" || lower == "apikey"
}

fn plex_invalid_pms_url() -> MediaServerError {
    MediaServerError::new(MediaServerErrorKind::MediaServerDiscoveryFailed)
        .provider("plex")
        .detail("Plex PMS URL is invalid")
}

fn append_query_pair(url: &mut String, key: &str, value: &str) {
    let separator = if url.contains('?') { '&' } else { '?' };
    url.push(separator);
    url.push_str(key);
    url.push('=');
    url.push_str(value);
}

fn non_blank(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_server::plex::dto::{PlexConnectionDto, PlexResourceDto};
    use bytes::Bytes;
    use futures::StreamExt;
    use http_body_util::Full;
    use hyper::{body::Incoming, http, service::service_fn, Request, Response};
    use hyper_util::{
        rt::{TokioExecutor, TokioIo},
        server::conn::auto::Builder,
    };
    use shared::model::MediaServerLibrarySelectorDetailsDto;
    use std::{
        convert::Infallible,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc as StdArc,
        },
    };
    use tokio::{net::TcpListener, sync::Mutex as TokioMutex};

    const PLEX_TEST_TOKEN: &str = "pms-token-redacted";

    #[derive(Default)]
    struct PlexPlaybackServerState {
        identity_requests: AtomicUsize,
        stream_requests: AtomicUsize,
        last_range: TokioMutex<Option<String>>,
        last_uri: TokioMutex<Option<String>>,
    }

    fn config() -> PlexClientConfig {
        PlexClientConfig {
            input_name: "media_server".into(),
            direct_url: None,
            token: None,
            account_token: Some("account-token-redacted".into()),
            server_id: Some("machine-redacted".into()),
            server_name: None,
            prefer_https: true,
            allow_relay: false,
            libraries: vec![MediaServerLibrarySelector::Name("Movies".to_string())],
        }
    }

    #[test]
    fn server_id_selector_matches_myplex_machine_identifier_without_exposing_token() {
        let resource = PlexResourceDto {
            name: Some("Server Redacted".to_string()),
            product: Some("Plex Media Server".to_string()),
            product_version: None,
            client_identifier: Some("resource-redacted".to_string()),
            machine_identifier: Some("machine-redacted".to_string()),
            owned: Some(0),
            access_token: Some("resource-token-redacted".to_string()),
            connections: Vec::new(),
        };
        let resources = [resource];
        let selected = select_resource(&resources, &config()).expect("resource selected");

        assert_eq!(selected.machine_identifier.as_deref(), Some("machine-redacted"));
    }

    #[test]
    fn server_id_selector_matches_myplex_client_identifier_and_verifies_pms_identity() {
        let config = config();
        let resource = PlexResourceDto {
            name: Some("Server Redacted".to_string()),
            product: Some("Plex Media Server".to_string()),
            product_version: None,
            client_identifier: Some("machine-redacted".to_string()),
            machine_identifier: None,
            owned: Some(1),
            access_token: Some("resource-token-redacted".to_string()),
            connections: Vec::new(),
        };
        let resources = [resource];
        let selected = select_resource(&resources, &config).expect("resource selected by server_id");
        let identity = PlexIdentityDto {
            machine_identifier: Some("machine-redacted".to_string()),
            friendly_name: Some("PMS Redacted".to_string()),
            version: Some("1.0".to_string()),
        };

        verify_resource_identity(selected, &identity).expect("MyPlex resource matches PMS identity");
        let status = identity
            .into_status(selected.client_identifier.as_deref(), selected.owned.map(|owned| owned != 0))
            .expect("status maps");

        assert_eq!(status.server_id.as_ref(), "machine-redacted");
        assert_eq!(status.owned, Some(true));
    }

    #[test]
    fn selected_resource_identity_rejects_client_identifier_mismatch() {
        let resource = PlexResourceDto {
            name: Some("Server Redacted".to_string()),
            product: Some("Plex Media Server".to_string()),
            product_version: None,
            client_identifier: Some("resource-redacted".to_string()),
            machine_identifier: None,
            owned: Some(0),
            access_token: Some("resource-token-redacted".to_string()),
            connections: Vec::new(),
        };
        let identity = PlexIdentityDto {
            machine_identifier: Some("machine-redacted".to_string()),
            friendly_name: Some("PMS Redacted".to_string()),
            version: Some("1.0".to_string()),
        };

        let error = verify_resource_identity(&resource, &identity).expect_err("identity mismatch is rejected");

        assert_eq!(error.kind, MediaServerErrorKind::MediaServerDiscoveryFailed);
    }

    #[test]
    fn rejects_ambiguous_plex_resource_name() {
        let mut config = config();
        config.server_id = None;
        config.server_name = Some("Server Redacted".into());
        let resource = PlexResourceDto {
            name: Some("Server Redacted".to_string()),
            product: Some("Plex Media Server".to_string()),
            product_version: None,
            client_identifier: Some("resource-redacted".to_string()),
            machine_identifier: Some("machine-redacted".to_string()),
            owned: Some(0),
            access_token: Some("resource-token-redacted".to_string()),
            connections: Vec::new(),
        };

        let error = select_resource(&[resource.clone(), resource], &config).expect_err("name is ambiguous");
        assert_eq!(error.kind, MediaServerErrorKind::MediaServerDiscoveryFailed);
    }

    #[test]
    fn connection_selection_rejects_relay_by_default_and_prefers_https() {
        let resource = PlexResourceDto {
            name: Some("Server Redacted".to_string()),
            product: Some("Plex Media Server".to_string()),
            product_version: None,
            client_identifier: Some("resource-redacted".to_string()),
            machine_identifier: Some("machine-redacted".to_string()),
            owned: Some(0),
            access_token: Some("resource-token-redacted".to_string()),
            connections: vec![
                PlexConnectionDto {
                    protocol: Some("http".to_string()),
                    uri: Some("http://pms.example.invalid".to_string()),
                    local: Some(0),
                    relay: Some(0),
                },
                PlexConnectionDto {
                    protocol: Some("https".to_string()),
                    uri: Some("https://pms.example.invalid".to_string()),
                    local: Some(0),
                    relay: Some(0),
                },
                PlexConnectionDto {
                    protocol: Some("https".to_string()),
                    uri: Some("https://relay.example.invalid".to_string()),
                    local: Some(0),
                    relay: Some(1),
                },
            ],
        };

        let urls = selected_connection_urls(&resource, true, false).expect("usable connection");
        assert_eq!(urls.iter().map(AsRef::as_ref).collect::<Vec<_>>(), vec!["https://pms.example.invalid", "http://pms.example.invalid"]);
    }

    #[test]
    fn selected_libraries_reject_duplicate_title_selectors() {
        let sections = vec![
            PlexSectionDto { key: Some("1".to_string()), title: Some("Movies".to_string()), section_type: Some("movie".to_string()) },
            PlexSectionDto { key: Some("2".to_string()), title: Some("Movies".to_string()), section_type: Some("movie".to_string()) },
        ];
        let error = select_libraries(&config(), &"machine-redacted".into(), &sections).expect_err("duplicate title");

        assert_eq!(error.kind, MediaServerErrorKind::MediaServerLibraryUnavailable);
    }

    #[test]
    fn selected_libraries_allow_stable_key_selector() {
        let mut config = config();
        config.libraries = vec![MediaServerLibrarySelector::Detailed(MediaServerLibrarySelectorDetailsDto {
            key: Some("2".to_string()),
            ..MediaServerLibrarySelectorDetailsDto::default()
        })];
        let sections = vec![
            PlexSectionDto { key: Some("1".to_string()), title: Some("Movies".to_string()), section_type: Some("movie".to_string()) },
            PlexSectionDto { key: Some("2".to_string()), title: Some("Movies".to_string()), section_type: Some("movie".to_string()) },
        ];
        let libraries = select_libraries(&config, &"machine-redacted".into(), &sections).expect("stable key disambiguates");

        assert_eq!(libraries[0].reference.library_id.as_ref(), "2");
    }

    #[test]
    fn pms_part_url_accepts_only_same_origin_part_resources() {
        let base = StdArc::<str>::from("http://127.0.0.1:32400/base");

        assert_eq!(
            pms_part_url(&base, "/library/parts/part-redacted/file.mkv?download=1").expect("part key resolves"),
            "http://127.0.0.1:32400/library/parts/part-redacted/file.mkv?download=1"
        );
        assert_eq!(
            pms_part_url(&base, "/library/metadata/rating-redacted").expect_err("metadata paths are not direct part refs").kind,
            MediaServerErrorKind::NoDirectPlayableMediaServerSource
        );
        assert_eq!(
            pms_part_url(&base, "//evil.example.invalid/library/parts/part-redacted/file.mkv")
                .expect_err("network-path refs must not escape the selected PMS")
                .kind,
            MediaServerErrorKind::NoDirectPlayableMediaServerSource
        );
        assert_eq!(
            pms_part_url(&base, "/library/parts/../../identity")
                .expect_err("normalized paths must stay under direct part refs")
                .kind,
            MediaServerErrorKind::NoDirectPlayableMediaServerSource
        );
        assert_eq!(
            pms_part_url(&base, "/library/parts/part-redacted/file.mkv?X-Plex-Token=should-not-leak")
                .expect_err("part refs must not carry credentials")
                .kind,
            MediaServerErrorKind::NoDirectPlayableMediaServerSource
        );
    }

    /// `pms_image_url` has the same origin/prefix/credential guards as
    /// `pms_part_url`. This test mirrors the part-key coverage to lock in the
    /// `MediaServerItemNotFound` error kind and to catch any future regression
    /// in the shared `resolve_pms_resource_url` helper.
    #[test]
    fn pms_image_url_accepts_only_same_origin_metadata_images() {
        let base = StdArc::<str>::from("http://127.0.0.1:32400/base");

        assert_eq!(
            pms_image_url(&base, "/library/metadata/rating-redacted/thumb?width=320").expect("image path resolves"),
            "http://127.0.0.1:32400/library/metadata/rating-redacted/thumb?width=320"
        );
        assert_eq!(
            pms_image_url(&base, "").expect_err("blank paths must be rejected").kind,
            MediaServerErrorKind::MediaServerItemNotFound
        );
        assert_eq!(
            pms_image_url(&base, "/library/parts/part-redacted/file.mkv")
                .expect_err("part paths are not metadata images").kind,
            MediaServerErrorKind::MediaServerItemNotFound
        );
        assert_eq!(
            pms_image_url(&base, "//evil.example.invalid/library/metadata/rating-redacted/thumb")
                .expect_err("network-path refs must not escape the selected PMS")
                .kind,
            MediaServerErrorKind::MediaServerItemNotFound
        );
        assert_eq!(
            pms_image_url(&base, "/library/metadata/../../identity")
                .expect_err("normalized paths must stay under metadata images")
                .kind,
            MediaServerErrorKind::MediaServerItemNotFound
        );
        assert_eq!(
            pms_image_url(&base, "/library/metadata/rating-redacted/thumb?X-Plex-Token=should-not-leak")
                .expect_err("image refs must not carry credentials")
                .kind,
            MediaServerErrorKind::MediaServerItemNotFound
        );
    }

    fn plex_test_response(status: StatusCode, body: &'static [u8]) -> Response<Full<Bytes>> {
        Response::builder().status(status).body(Full::new(Bytes::from_static(body))).expect("response builds")
    }

    fn plex_test_media_server_config() -> PlexClientSettings {
        PlexClientSettings {
            token: Some(PLEX_TEST_TOKEN.to_string()),
            prefer_https: true,
            ..PlexClientSettings::default()
        }
    }

    fn plex_test_token_is_valid(req: &Request<Incoming>) -> bool {
        req.headers()
            .get(X_PLEX_TOKEN)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value == PLEX_TEST_TOKEN)
    }

    async fn plex_playback_handler(
        req: Request<Incoming>,
        state: StdArc<PlexPlaybackServerState>,
    ) -> Result<Response<Full<Bytes>>, Infallible> {
        if !plex_test_token_is_valid(&req) {
            return Ok(plex_test_response(StatusCode::UNAUTHORIZED, b""));
        }

        match req.uri().path() {
            "/identity" => {
                state.identity_requests.fetch_add(1, Ordering::SeqCst);
                Ok(plex_test_response(
                    StatusCode::OK,
                    br#"<MediaContainer machineIdentifier="machine-redacted" friendlyName="PMS Redacted" version="1.0"/>"#,
                ))
            }
            "/library/parts/part-redacted/file.mkv" => {
                state.stream_requests.fetch_add(1, Ordering::SeqCst);
                *state.last_uri.lock().await = Some(req.uri().to_string());
                *state.last_range.lock().await = req
                    .headers()
                    .get(http::header::RANGE)
                    .and_then(|value| value.to_str().ok())
                    .map(ToOwned::to_owned);

                Ok(Response::builder()
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header(http::header::CONTENT_TYPE, "video/x-matroska")
                    .header(http::header::CONTENT_RANGE, "bytes 5-8/9")
                    .header(http::header::CONTENT_LENGTH, "4")
                    .header(http::header::ACCEPT_RANGES, "bytes")
                    .body(Full::new(Bytes::from_static(b"data")))
                    .expect("response builds"))
            }
            "/library/metadata/rating-redacted/thumb" => Ok(Response::builder()
                .status(StatusCode::OK)
                .header(http::header::CONTENT_TYPE, "image/jpeg")
                .header(http::header::CONTENT_LENGTH, "4")
                .body(Full::new(Bytes::from_static(b"jpeg")))
                .expect("response builds")),
            _ => Ok(plex_test_response(StatusCode::NOT_FOUND, b"")),
        }
    }

    async fn start_plex_playback_server(state: StdArc<PlexPlaybackServerState>) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test server binds");
        let addr = listener.local_addr().expect("local addr");
        let handle = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else { continue };
                let state = StdArc::clone(&state);
                tokio::spawn(async move {
                    let io = TokioIo::new(socket);
                    let service = service_fn(move |req| plex_playback_handler(req, StdArc::clone(&state)));
                    let builder = Builder::new(TokioExecutor::new());
                    let _ = builder.serve_connection(io, service).await;
                });
            }
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn open_stream_proxies_plex_part_key_with_token_header_and_range() {
        let state = StdArc::new(PlexPlaybackServerState::default());
        let (base_url, server) = start_plex_playback_server(StdArc::clone(&state)).await;
        let media_server = plex_test_media_server_config();
        let client = PlexCatalogClient::new(
            "media_server".into(),
            &base_url,
            &media_server,
            MediaServerHttpClient::new(reqwest::Client::new()),
        );
        let stream_ref = MediaServerStreamRef::Plex {
            input_name: "media_server".into(),
            server_id: "machine-redacted".into(),
            rating_key: "rating-redacted".into(),
            part_key: "/library/parts/part-redacted/file.mkv?download=1".into(),
        };

        let response = client
            .open_stream(&stream_ref, Some("bytes=5-8"))
            .await
            .expect("Plex stream opens");

        assert_eq!(response.status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers.get(http::header::CONTENT_RANGE).and_then(|value| value.to_str().ok()), Some("bytes 5-8/9"));
        let chunks = response.body.collect::<Vec<_>>().await;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].as_ref().map(Bytes::as_ref), Ok(b"data".as_slice()));
        assert_eq!(state.stream_requests.load(Ordering::SeqCst), 1);
        assert_eq!(state.last_range.lock().await.as_deref(), Some("bytes=5-8"));
        assert_eq!(
            state.last_uri.lock().await.as_deref(),
            Some("/library/parts/part-redacted/file.mkv?download=1")
        );
        assert!(!state.last_uri.lock().await.as_deref().unwrap_or_default().contains(PLEX_TEST_TOKEN));
        assert!(!state.last_uri.lock().await.as_deref().unwrap_or_default().contains("X-Plex-Token"));

        server.abort();
    }

    #[tokio::test]
    async fn open_image_proxies_plex_metadata_image_with_token_header() {
        let state = StdArc::new(PlexPlaybackServerState::default());
        let (base_url, server) = start_plex_playback_server(StdArc::clone(&state)).await;
        let media_server = plex_test_media_server_config();
        let client = PlexCatalogClient::new(
            "media_server".into(),
            &base_url,
            &media_server,
            MediaServerHttpClient::new(reqwest::Client::new()),
        );
        let image_ref = MediaServerImageRef::Plex {
            input_name: "media_server".into(),
            server_id: "machine-redacted".into(),
            rating_key: "rating-redacted".into(),
            image_path: "/library/metadata/rating-redacted/thumb".into(),
        };

        let response = client.open_image(&image_ref).await.expect("Plex image opens");

        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.headers.get(http::header::CONTENT_TYPE).and_then(|value| value.to_str().ok()), Some("image/jpeg"));
        assert_eq!(response.body.as_ref(), b"jpeg");

        server.abort();
    }

    #[tokio::test]
    async fn direct_playback_reuses_discovered_connection_across_client_instances() {
        let state = StdArc::new(PlexPlaybackServerState::default());
        let (base_url, server) = start_plex_playback_server(StdArc::clone(&state)).await;
        let media_server = plex_test_media_server_config();
        let stream_ref = MediaServerStreamRef::Plex {
            input_name: "media_server".into(),
            server_id: "machine-redacted".into(),
            rating_key: "rating-redacted".into(),
            part_key: "/library/parts/part-redacted/file.mkv?download=1".into(),
        };

        for _ in 0..2 {
            let client = PlexCatalogClient::new(
                "media_server".into(),
                &base_url,
                &media_server,
                MediaServerHttpClient::new(reqwest::Client::new()),
            );
            let response = client.open_stream(&stream_ref, None).await.expect("Plex stream opens");
            let chunks = response.body.collect::<Vec<_>>().await;
            assert_eq!(chunks[0].as_ref().map(Bytes::as_ref), Ok(b"data".as_slice()));
        }

        assert_eq!(state.stream_requests.load(Ordering::SeqCst), 2);
        assert_eq!(state.identity_requests.load(Ordering::SeqCst), 1);

        server.abort();
    }
}
