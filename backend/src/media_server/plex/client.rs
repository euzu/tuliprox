use crate::{
    media_server::{
        plex::{
            dto::{PlexIdentityDto, PlexMediaContainerDto, PlexResourceDto, PlexResourcesDto, PlexSectionsDto},
            mapper::{map_plex_episode, map_plex_movie, map_plex_section},
        },
        MediaServerCatalogClient, MediaServerEpisode, MediaServerError, MediaServerErrorKind, MediaServerHttpClient,
        MediaServerImageRef, MediaServerKind, MediaServerLibrary, MediaServerLibraryKind, MediaServerLibraryRef,
        MediaServerMovie, MediaServerPage, MediaServerPageRequest, MediaServerResourceResponse, MediaServerStatus,
        MediaServerStreamRef, MediaServerStreamResponse,
    },
    model::{ConfigInput, MediaServerInputConfig},
};
use futures::StreamExt;
use reqwest::{
    header::{HeaderName, HeaderValue, RANGE},
    Method, StatusCode,
};
use shared::model::{InputType, MediaServerLibraryKindDto, MediaServerLibrarySelectorDto};
use std::{collections::HashSet, sync::Arc};
use url::Url;

const PLEX_TOKEN_HEADER: HeaderName = HeaderName::from_static("x-plex-token");
const MY_PLEX_RESOURCES_URL: &str = "https://plex.tv/api/resources?includeHttps=1&includeRelay=1";
const PLEX_PROVIDER_NAME: &str = "plex";

#[derive(Clone)]
pub struct PlexClient {
    http: MediaServerHttpClient,
    pms_url: Arc<str>,
    input_name: Arc<str>,
    server_id: Arc<str>,
    resource_token: Arc<str>,
    library_selectors: Vec<MediaServerLibrarySelectorDto>,
    owned: Option<bool>,
}

impl PlexClient {
    pub fn new(
        client: reqwest::Client,
        pms_url: impl Into<Arc<str>>,
        input_name: impl Into<Arc<str>>,
        server_id: impl Into<Arc<str>>,
        resource_token: impl Into<Arc<str>>,
        library_selectors: Vec<MediaServerLibrarySelectorDto>,
    ) -> Self {
        Self {
            http: MediaServerHttpClient::new(client),
            pms_url: pms_url.into(),
            input_name: input_name.into(),
            server_id: server_id.into(),
            resource_token: resource_token.into(),
            library_selectors,
            owned: None,
        }
    }

    pub async fn from_input(client: reqwest::Client, input: &ConfigInput) -> Result<Self, MediaServerError> {
        if input.input_type != InputType::Plex {
            return Err(MediaServerError::new(MediaServerErrorKind::MediaServerDiscoveryFailed)
                .provider(PLEX_PROVIDER_NAME)
                .detail("input type is not plex"));
        }
        let media_server = input.media_server.as_ref().ok_or_else(|| {
            MediaServerError::new(MediaServerErrorKind::MediaServerDiscoveryFailed)
                .provider(PLEX_PROVIDER_NAME)
                .detail("missing media_server configuration")
        })?;
        let pms_url = input.url.trim();
        if pms_url.is_empty() {
            Self::from_myplex_input(client, input, media_server).await
        } else {
            Self::from_direct_input(client, input, media_server, pms_url).await
        }
    }

    pub async fn list_myplex_resources(
        client: reqwest::Client,
        account_token: &str,
    ) -> Result<PlexResourcesDto, MediaServerError> {
        let http = MediaServerHttpClient::new(client);
        let request = http
            .request(Method::GET, MY_PLEX_RESOURCES_URL)
            .discovery_errors()
            .header(PLEX_TOKEN_HEADER, token_header(account_token)?);
        let safe_url = request.safe_url().to_string();
        let response = request.send().await?;
        ensure_status(
            response.status(),
            MediaServerErrorKind::MediaServerUnavailable,
            MediaServerErrorKind::MediaServerDiscoveryFailed,
            &safe_url,
        )?;
        let body = response.text().await.map_err(|err| {
            MediaServerError::from_reqwest_error_with_fallback(
                &err,
                MediaServerErrorKind::MediaServerUnavailable,
                MediaServerErrorKind::MediaServerDiscoveryFailed,
            )
            .provider(PLEX_PROVIDER_NAME)
            .detail(format!("request {safe_url} failed"))
        })?;
        quick_xml::de::from_str::<PlexResourcesDto>(&body).map_err(|err| {
            MediaServerError::new(MediaServerErrorKind::MediaServerDiscoveryFailed)
                .provider(PLEX_PROVIDER_NAME)
                .detail(err.to_string())
        })
    }

    async fn from_direct_input(
        client: reqwest::Client,
        input: &ConfigInput,
        media_server: &MediaServerInputConfig,
        pms_url: &str,
    ) -> Result<Self, MediaServerError> {
        let token = media_server.token.as_deref().ok_or_else(|| {
            MediaServerError::new(MediaServerErrorKind::MediaServerAuthDenied)
                .provider(PLEX_PROVIDER_NAME)
                .detail("direct Plex mode requires a PMS token")
        })?;
        let fallback_server_id = media_server
            .machine_id
            .as_deref()
            .or(media_server.server_id.as_deref())
            .unwrap_or("plex-direct");
        let mut plex = Self::new(
            client,
            pms_url.to_string(),
            input.name.clone(),
            fallback_server_id.to_string(),
            token.to_string(),
            media_server.libraries.clone(),
        );
        let status = plex.discover().await?;
        verify_direct_selectors(media_server, &status)?;
        plex.server_id = status.server_id;
        Ok(plex)
    }

    async fn from_myplex_input(
        client: reqwest::Client,
        input: &ConfigInput,
        media_server: &MediaServerInputConfig,
    ) -> Result<Self, MediaServerError> {
        if !media_server.has_plex_server_selector() {
            return Err(MediaServerError::new(MediaServerErrorKind::MediaServerDiscoveryFailed)
                .provider(PLEX_PROVIDER_NAME)
                .detail("MyPlex discovery requires a Plex server selector"));
        }
        let account_token = media_server.account_token.as_deref().ok_or_else(|| {
            MediaServerError::new(MediaServerErrorKind::MediaServerAuthDenied)
                .provider(PLEX_PROVIDER_NAME)
                .detail("MyPlex discovery requires an account token")
        })?;
        let resources = Self::list_myplex_resources(client.clone(), account_token).await?;
        let resource = select_resource(media_server, &resources)?;
        let connection = select_connection(&resource.connections, media_server.prefer_https, media_server.allow_relay)?;
        let mut plex = Self::new(
            client,
            connection.uri,
            input.name.clone(),
            resource.machine_id.to_string(),
            resource.access_token.to_string(),
            media_server.libraries.clone(),
        );
        plex.owned = resource.owned;
        let status = plex.discover().await?;
        if status.server_id.as_ref() != resource.machine_id.as_ref() {
            return Err(MediaServerError::new(MediaServerErrorKind::MediaServerDiscoveryFailed)
                .provider(PLEX_PROVIDER_NAME)
                .detail("selected Plex resource identity did not match PMS identity"));
        }
        Ok(plex)
    }

    fn url(&self, path: &str) -> String {
        if matches!(Url::parse(path), Ok(url) if matches!(url.scheme(), "http" | "https")) {
            return path.to_string();
        }
        format!("{}{}", self.pms_url.trim_end_matches('/'), path)
    }

    async fn get_discovery_xml<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, MediaServerError> {
        self.get_xml(
            self.url(path),
            MediaServerErrorKind::MediaServerUnavailable,
            MediaServerErrorKind::MediaServerDiscoveryFailed,
            MediaServerErrorKind::MediaServerDiscoveryFailed,
        )
        .await
    }

    async fn get_catalog_xml<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, MediaServerError> {
        self.get_xml(
            self.url(path),
            MediaServerErrorKind::MediaServerLibraryUnavailable,
            MediaServerErrorKind::MediaServerCatalogDecodeFailed,
            MediaServerErrorKind::MediaServerCatalogDecodeFailed,
        )
        .await
    }

    async fn get_xml<T: serde::de::DeserializeOwned>(
        &self,
        url: String,
        not_found_kind: MediaServerErrorKind,
        fallback_kind: MediaServerErrorKind,
        decode_kind: MediaServerErrorKind,
    ) -> Result<T, MediaServerError> {
        let request = self
            .http
            .request(Method::GET, &url)
            .error_kinds(not_found_kind.clone(), fallback_kind.clone())
            .header(PLEX_TOKEN_HEADER, token_header(&self.resource_token)?);
        let safe_url = request.safe_url().to_string();
        let response = request.send().await?;
        ensure_status(response.status(), not_found_kind.clone(), fallback_kind.clone(), &safe_url)?;
        let body = response.text().await.map_err(|err| {
            MediaServerError::from_reqwest_error_with_fallback(&err, not_found_kind, fallback_kind)
                .provider(PLEX_PROVIDER_NAME)
                .detail(format!("request {safe_url} failed"))
        })?;
        quick_xml::de::from_str::<T>(&body).map_err(|err| {
            MediaServerError::new(decode_kind)
                .provider(PLEX_PROVIDER_NAME)
                .detail(err.to_string())
        })
    }

    async fn get_image_resource(&self, url: String) -> Result<MediaServerResourceResponse, MediaServerError> {
        let response = self.send_resource_request(url, None).await?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.bytes().await.map_err(|err| {
            MediaServerError::from_reqwest_error_with_fallback(
                &err,
                MediaServerErrorKind::MediaServerItemNotFound,
                MediaServerErrorKind::MediaServerStreamOpenFailed,
            )
            .provider(PLEX_PROVIDER_NAME)
        })?;
        Ok(MediaServerResourceResponse { status, headers, body })
    }

    async fn get_stream_resource(
        &self,
        url: String,
        range: Option<&str>,
    ) -> Result<MediaServerStreamResponse, MediaServerError> {
        let response = self.send_resource_request(url, range).await?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .bytes_stream()
            .map(|chunk| {
                chunk.map_err(|err| {
                    MediaServerError::from_reqwest_error_with_fallback(
                        &err,
                        MediaServerErrorKind::MediaServerItemNotFound,
                        MediaServerErrorKind::MediaServerStreamOpenFailed,
                    )
                    .provider(PLEX_PROVIDER_NAME)
                })
            })
            .boxed();
        Ok(MediaServerStreamResponse { status, headers, body })
    }

    async fn send_resource_request(
        &self,
        url: String,
        range: Option<&str>,
    ) -> Result<reqwest::Response, MediaServerError> {
        let mut request = self
            .http
            .request(Method::GET, &url)
            .playback_errors()
            .header(PLEX_TOKEN_HEADER, token_header(&self.resource_token)?);
        if let Some(range) = range {
            request = request.header(RANGE, HeaderValue::from_str(range).map_err(|err| {
                MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
                    .provider(PLEX_PROVIDER_NAME)
                    .detail(err.to_string())
            })?);
        }
        let safe_url = request.safe_url().to_string();
        let response = request.send().await?;
        ensure_status(
            response.status(),
            MediaServerErrorKind::MediaServerItemNotFound,
            MediaServerErrorKind::MediaServerStreamOpenFailed,
            &safe_url,
        )?;
        Ok(response)
    }

    fn selected_libraries(&self, all_libraries: &[MediaServerLibrary]) -> Result<Vec<MediaServerLibrary>, MediaServerError> {
        if self.library_selectors.is_empty() {
            return Ok(all_libraries.to_vec());
        }
        let mut selected = Vec::new();
        let mut seen = HashSet::<Arc<str>>::new();
        for selector in &self.library_selectors {
            let matches = libraries_matching_selector(all_libraries, selector)?;
            if matches.is_empty() {
                return Err(MediaServerError::new(MediaServerErrorKind::MediaServerLibraryUnavailable)
                    .provider(PLEX_PROVIDER_NAME)
                    .detail("configured Plex library selector did not match any PMS section"));
            }
            for library in matches {
                if seen.insert(library.reference.library_id.clone()) {
                    selected.push(library.clone());
                }
            }
        }
        Ok(selected)
    }
}

impl MediaServerCatalogClient for PlexClient {
    async fn discover(&self) -> Result<MediaServerStatus, MediaServerError> {
        let identity: PlexIdentityDto = self.get_discovery_xml("/identity").await?;
        Ok(MediaServerStatus {
            kind: MediaServerKind::Plex,
            server_id: identity
                .machine_identifier
                .as_deref()
                .filter(|id| !id.trim().is_empty())
                .map_or_else(|| self.server_id.clone(), Arc::<str>::from),
            display_name: identity.friendly_name.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(Arc::<str>::from),
            version: identity.version.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(Arc::<str>::from),
            owned: self.owned,
        })
    }

    async fn list_libraries(&self) -> Result<Vec<MediaServerLibrary>, MediaServerError> {
        let status = self.discover().await?;
        let sections: PlexSectionsDto = self.get_catalog_xml("/library/sections").await?;
        let libraries: Vec<_> = sections
            .directories
            .iter()
            .filter_map(|section| map_plex_section(&self.input_name, &status.server_id, section))
            .collect();
        self.selected_libraries(&libraries)
    }

    async fn list_movies(
        &self,
        library: &MediaServerLibraryRef,
        page: MediaServerPageRequest,
    ) -> Result<MediaServerPage<MediaServerMovie>, MediaServerError> {
        let path = format!(
            "/library/sections/{}/all?type=1&X-Plex-Container-Start={}&X-Plex-Container-Size={}",
            escape_path_component(&library.library_id),
            page.start,
            page.limit
        );
        let container: PlexMediaContainerDto = self.get_catalog_xml(&path).await?;
        let upstream_item_count = container.upstream_item_count();
        let items = container
            .videos
            .iter()
            .filter_map(|video| map_plex_movie(&self.input_name, &library.server_id, &library.library_id, video))
            .collect();
        Ok(MediaServerPage::with_upstream_item_count(page, container.total_size, upstream_item_count, items))
    }

    async fn list_episodes(
        &self,
        library: &MediaServerLibraryRef,
        page: MediaServerPageRequest,
    ) -> Result<MediaServerPage<MediaServerEpisode>, MediaServerError> {
        let path = format!(
            "/library/sections/{}/all?type=4&X-Plex-Container-Start={}&X-Plex-Container-Size={}",
            escape_path_component(&library.library_id),
            page.start,
            page.limit
        );
        let container: PlexMediaContainerDto = self.get_catalog_xml(&path).await?;
        let upstream_item_count = container.upstream_item_count();
        let items = container
            .videos
            .iter()
            .filter_map(|video| map_plex_episode(&self.input_name, &library.server_id, &library.library_id, video))
            .collect();
        Ok(MediaServerPage::with_upstream_item_count(page, container.total_size, upstream_item_count, items))
    }

    async fn open_stream(
        &self,
        stream_ref: &MediaServerStreamRef,
        range: Option<&str>,
    ) -> Result<MediaServerStreamResponse, MediaServerError> {
        let MediaServerStreamRef::Plex { part_key, .. } = stream_ref else {
            return Err(MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
                .provider(PLEX_PROVIDER_NAME)
                .detail("stream ref is not for Plex"));
        };
        self.get_stream_resource(self.url(part_key), range).await
    }

    async fn open_image(&self, image_ref: &MediaServerImageRef) -> Result<MediaServerResourceResponse, MediaServerError> {
        let MediaServerImageRef::Plex { image_path, .. } = image_ref else {
            return Err(MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
                .provider(PLEX_PROVIDER_NAME)
                .detail("image ref is not for Plex"));
        };
        self.get_image_resource(self.url(image_path)).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlexConnectionCandidate {
    uri: String,
    relay: bool,
    https: bool,
    local: bool,
    ordinal: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectedPlexResource {
    machine_id: Arc<str>,
    access_token: Arc<str>,
    connections: Vec<PlexConnectionCandidate>,
    owned: Option<bool>,
}

fn select_resource(
    media_server: &MediaServerInputConfig,
    resources: &PlexResourcesDto,
) -> Result<SelectedPlexResource, MediaServerError> {
    let mut matches = resources
        .devices
        .iter()
        .filter(|resource| is_plex_media_server_resource(resource))
        .filter(|resource| resource_matches_selectors(resource, media_server))
        .collect::<Vec<_>>();

    if matches.is_empty() {
        return Err(MediaServerError::new(MediaServerErrorKind::MediaServerDiscoveryFailed)
            .provider(PLEX_PROVIDER_NAME)
            .detail("no Plex Media Server resource matched configured selector"));
    }
    if matches.len() > 1 {
        return Err(MediaServerError::new(MediaServerErrorKind::MediaServerDiscoveryFailed)
            .provider(PLEX_PROVIDER_NAME)
            .detail("Plex server selector matched multiple resources"));
    }

    let resource = matches.swap_remove(0);
    let machine_id = resource.machine_identifier.as_deref().and_then(non_blank).ok_or_else(|| {
        MediaServerError::new(MediaServerErrorKind::MediaServerDiscoveryFailed)
            .provider(PLEX_PROVIDER_NAME)
            .detail("selected Plex resource did not expose a machine identifier")
    })?;
    let access_token = resource.access_token.as_deref().and_then(non_blank).ok_or_else(|| {
        MediaServerError::new(MediaServerErrorKind::MediaServerAuthDenied)
            .provider(PLEX_PROVIDER_NAME)
            .detail("selected Plex resource did not expose a PMS access token")
    })?;
    let connections = resource
        .connections
        .iter()
        .enumerate()
        .filter_map(|(ordinal, connection)| connection_candidate(connection, ordinal))
        .collect();
    Ok(SelectedPlexResource {
        machine_id: Arc::<str>::from(machine_id),
        access_token: Arc::<str>::from(access_token),
        connections,
        owned: resource.owned.map(|owned| owned != 0),
    })
}

fn select_connection(
    connections: &[PlexConnectionCandidate],
    prefer_https: bool,
    allow_relay: bool,
) -> Result<PlexConnectionCandidate, MediaServerError> {
    let mut candidates = connections
        .iter()
        .filter(|connection| allow_relay || !connection.relay)
        .cloned()
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Err(MediaServerError::new(MediaServerErrorKind::MediaServerUnavailable)
            .provider(PLEX_PROVIDER_NAME)
            .detail("selected Plex resource has no usable PMS connection"));
    }
    candidates.sort_by_key(|connection| {
        (
            connection.relay,
            prefer_https && !connection.https,
            !connection.local,
            connection.ordinal,
        )
    });
    Ok(candidates.remove(0))
}

fn connection_candidate(
    connection: &crate::media_server::plex::dto::PlexConnectionDto,
    ordinal: usize,
) -> Option<PlexConnectionCandidate> {
    let uri = connection.uri.as_deref().and_then(non_blank)?;
    let parsed = Url::parse(uri).ok()?;
    let scheme = parsed.scheme();
    if !matches!(scheme, "http" | "https") {
        return None;
    }
    Some(PlexConnectionCandidate {
        uri: uri.to_string(),
        relay: plex_flag(connection.relay),
        https: scheme == "https",
        local: plex_flag(connection.local),
        ordinal,
    })
}

fn is_plex_media_server_resource(resource: &PlexResourceDto) -> bool {
    resource
        .product
        .as_deref()
        .map(str::trim)
        .is_none_or(|product| product.eq_ignore_ascii_case("Plex Media Server"))
}

fn resource_matches_selectors(resource: &PlexResourceDto, media_server: &MediaServerInputConfig) -> bool {
    selector_matches(resource.machine_identifier.as_deref(), media_server.machine_id.as_deref(), false)
        && selector_matches(resource.client_identifier.as_deref(), media_server.server_id.as_deref(), false)
        && selector_matches(resource.name.as_deref(), media_server.server_name.as_deref(), true)
}

fn selector_matches(candidate: Option<&str>, selector: Option<&str>, ignore_ascii_case: bool) -> bool {
    let Some(selector) = selector.and_then(non_blank) else { return true };
    let Some(candidate) = candidate.and_then(non_blank) else { return false };
    if ignore_ascii_case {
        candidate.eq_ignore_ascii_case(selector)
    } else {
        candidate == selector
    }
}

fn verify_direct_selectors(
    media_server: &MediaServerInputConfig,
    status: &MediaServerStatus,
) -> Result<(), MediaServerError> {
    if !selector_matches(Some(status.server_id.as_ref()), media_server.machine_id.as_deref(), false)
        || !selector_matches(Some(status.server_id.as_ref()), media_server.server_id.as_deref(), false)
        || !selector_matches(status.display_name.as_deref(), media_server.server_name.as_deref(), true)
    {
        return Err(MediaServerError::new(MediaServerErrorKind::MediaServerDiscoveryFailed)
            .provider(PLEX_PROVIDER_NAME)
            .detail("direct Plex selector did not match PMS identity"));
    }
    Ok(())
}

fn libraries_matching_selector<'a>(
    libraries: &'a [MediaServerLibrary],
    selector: &MediaServerLibrarySelectorDto,
) -> Result<Vec<&'a MediaServerLibrary>, MediaServerError> {
    match selector {
        MediaServerLibrarySelectorDto::Name(value) => libraries_matching_name(libraries, value),
        MediaServerLibrarySelectorDto::Detailed(details) => {
            let requested_keys = [details.id.as_deref(), details.key.as_deref()]
                .into_iter()
                .flatten()
                .filter_map(non_blank)
                .collect::<Vec<_>>();
            let requested_name = details.name.as_deref().and_then(non_blank);
            let requested_kind = details.kind.map(media_server_library_kind);
            let matches = libraries
                .iter()
                .filter(|library| {
                    (requested_keys.is_empty()
                        || requested_keys.iter().any(|key| library.reference.library_id.as_ref() == *key))
                        && requested_name.is_none_or(|name| library.name.as_ref() == name)
                        && requested_kind.is_none_or(|kind| library.kind == kind)
                })
                .collect::<Vec<_>>();
            if requested_keys.is_empty() && requested_name.is_some() && matches.len() > 1 {
                return Err(MediaServerError::new(MediaServerErrorKind::MediaServerLibraryUnavailable)
                    .provider(PLEX_PROVIDER_NAME)
                    .detail("Plex library title selector matched multiple PMS sections"));
            }
            Ok(matches)
        }
    }
}

fn libraries_matching_name<'a>(
    libraries: &'a [MediaServerLibrary],
    selector: &str,
) -> Result<Vec<&'a MediaServerLibrary>, MediaServerError> {
    let selector = selector.trim();
    let by_key = libraries
        .iter()
        .filter(|library| library.reference.library_id.as_ref() == selector)
        .collect::<Vec<_>>();
    if !by_key.is_empty() {
        return Ok(by_key);
    }
    let by_name = libraries.iter().filter(|library| library.name.as_ref() == selector).collect::<Vec<_>>();
    if by_name.len() > 1 {
        return Err(MediaServerError::new(MediaServerErrorKind::MediaServerLibraryUnavailable)
            .provider(PLEX_PROVIDER_NAME)
            .detail("Plex library title selector matched multiple PMS sections"));
    }
    Ok(by_name)
}

fn media_server_library_kind(kind: MediaServerLibraryKindDto) -> MediaServerLibraryKind {
    match kind {
        MediaServerLibraryKindDto::Movies => MediaServerLibraryKind::Movies,
        MediaServerLibraryKindDto::TvShows => MediaServerLibraryKind::TvShows,
    }
}

fn ensure_status(
    status: StatusCode,
    not_found_kind: MediaServerErrorKind,
    fallback_kind: MediaServerErrorKind,
    safe_url: &str,
) -> Result<(), MediaServerError> {
    if status.is_success() {
        return Ok(());
    }
    Err(MediaServerError::from_http_status_with_fallback(status, not_found_kind, fallback_kind)
        .provider(PLEX_PROVIDER_NAME)
        .detail(format!("request {safe_url} returned {status}")))
}

fn token_header(token: &str) -> Result<HeaderValue, MediaServerError> {
    HeaderValue::from_str(token).map_err(|err| {
        MediaServerError::new(MediaServerErrorKind::MediaServerAuthDenied)
            .provider(PLEX_PROVIDER_NAME)
            .detail(err.to_string())
    })
}

fn plex_flag(value: Option<u8>) -> bool { value.unwrap_or_default() != 0 }

fn non_blank(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn escape_path_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_server::test_fixtures::{
        PLEX_AMBIGUOUS_RESOURCES_XML, PLEX_RESOURCES_WITH_RELAY_XML, PLEX_RESOURCES_XML, PLEX_SECTIONS_XML,
    };
    use shared::model::MediaServerLibrarySelectorDetailsDto;

    fn media_config_with_selector() -> MediaServerInputConfig {
        MediaServerInputConfig {
            account_token: Some("account-token-redacted".to_string()),
            machine_id: Some("machine-redacted".to_string()),
            libraries: vec![MediaServerLibrarySelectorDto::Name("Movies".to_string())],
            ..MediaServerInputConfig::from(&shared::model::MediaServerInputConfigDto::default())
        }
    }

    #[test]
    fn direct_pms_discovery_accepts_missing_selector() {
        let config = MediaServerInputConfig { token: Some("pms-token-redacted".to_string()), ..media_config_with_selector() };
        let config = MediaServerInputConfig { machine_id: None, server_id: None, server_name: None, ..config };
        let status = MediaServerStatus {
            kind: MediaServerKind::Plex,
            server_id: "machine-redacted".into(),
            display_name: Some("Server Redacted".into()),
            version: Some("1.0.0".into()),
            owned: None,
        };

        verify_direct_selectors(&config, &status).expect("direct PMS mode does not require MyPlex selector");
    }

    #[tokio::test]
    async fn myplex_discovery_rejects_missing_selector_before_network() {
        let mut input = ConfigInput {
            name: "plex_media_server".into(),
            input_type: InputType::Plex,
            media_server: Some(MediaServerInputConfig {
                account_token: Some("account-token-redacted".to_string()),
                machine_id: None,
                server_id: None,
                server_name: None,
                ..media_config_with_selector()
            }),
            ..ConfigInput::default()
        };
        input.url.clear();

        let error = match PlexClient::from_input(reqwest::Client::new(), &input).await {
            Ok(_) => panic!("missing selector should fail"),
            Err(error) => error,
        };

        assert_eq!(error.kind, MediaServerErrorKind::MediaServerDiscoveryFailed);
    }

    #[test]
    fn myplex_resource_selection_matches_machine_id() {
        let resources: PlexResourcesDto = quick_xml::de::from_str(PLEX_RESOURCES_XML).expect("resources parse");
        let config = media_config_with_selector();

        let selected = select_resource(&config, &resources).expect("resource selected");

        assert_eq!(selected.machine_id.as_ref(), "machine-redacted");
        assert_eq!(selected.access_token.as_ref(), "resource-token-redacted");
    }

    #[test]
    fn myplex_server_name_selector_rejects_ambiguous_resources() {
        let resources: PlexResourcesDto = quick_xml::de::from_str(PLEX_AMBIGUOUS_RESOURCES_XML).expect("resources parse");
        let mut config = media_config_with_selector();
        config.machine_id = None;
        config.server_name = Some("Duplicated Server".to_string());

        let error = select_resource(&config, &resources).expect_err("ambiguous selector should fail");

        assert_eq!(error.kind, MediaServerErrorKind::MediaServerDiscoveryFailed);
    }

    #[test]
    fn relay_connections_are_ignored_by_default() {
        let resources: PlexResourcesDto = quick_xml::de::from_str(PLEX_RESOURCES_WITH_RELAY_XML).expect("resources parse");
        let config = media_config_with_selector();
        let selected = select_resource(&config, &resources).expect("resource selected");

        let connection = select_connection(&selected.connections, true, false).expect("non-relay selected");

        assert_eq!(connection.uri, "http://pms.example.invalid");
        assert!(!connection.relay);
    }

    #[test]
    fn relay_connection_can_be_selected_with_explicit_opt_in_when_no_direct_candidate_exists() {
        let connections = vec![PlexConnectionCandidate {
            uri: "https://relay.example.invalid".to_string(),
            relay: true,
            https: true,
            local: false,
            ordinal: 0,
        }];

        let connection = select_connection(&connections, true, true).expect("https relay selected");

        assert_eq!(connection.uri, "https://relay.example.invalid");
        assert!(connection.relay);
    }

    #[test]
    fn library_title_duplicates_are_rejected() {
        let input_name = Arc::<str>::from("plex_media_server");
        let server_id = Arc::<str>::from("machine-redacted");
        let sections: PlexSectionsDto = quick_xml::de::from_str(
            r#"<MediaContainer size="2"><Directory key="1" title="Movies" type="movie" /><Directory key="2" title="Movies" type="movie" /></MediaContainer>"#,
        )
        .expect("sections parse");
        let libraries: Vec<_> = sections
            .directories
            .iter()
            .filter_map(|section| map_plex_section(&input_name, &server_id, section))
            .collect();

        let error = libraries_matching_selector(&libraries, &MediaServerLibrarySelectorDto::Name("Movies".to_string()))
            .expect_err("duplicate title should fail");

        assert_eq!(error.kind, MediaServerErrorKind::MediaServerLibraryUnavailable);
    }

    #[test]
    fn library_key_selector_wins_over_duplicate_titles() {
        let input_name = Arc::<str>::from("plex_media_server");
        let server_id = Arc::<str>::from("machine-redacted");
        let sections: PlexSectionsDto = quick_xml::de::from_str(
            r#"<MediaContainer size="2"><Directory key="1" title="Movies" type="movie" /><Directory key="2" title="Movies" type="movie" /></MediaContainer>"#,
        )
        .expect("sections parse");
        let libraries: Vec<_> = sections
            .directories
            .iter()
            .filter_map(|section| map_plex_section(&input_name, &server_id, section))
            .collect();
        let selector = MediaServerLibrarySelectorDto::Detailed(MediaServerLibrarySelectorDetailsDto {
            key: Some("2".to_string()),
            ..MediaServerLibrarySelectorDetailsDto::default()
        });

        let matches = libraries_matching_selector(&libraries, &selector).expect("key selector succeeds");

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].reference.library_id.as_ref(), "2");
    }

    #[test]
    fn unsupported_sections_are_not_coerced() {
        let input_name = Arc::<str>::from("plex_media_server");
        let server_id = Arc::<str>::from("machine-redacted");
        let sections: PlexSectionsDto = quick_xml::de::from_str(PLEX_SECTIONS_XML).expect("sections parse");
        let libraries: Vec<_> = sections
            .directories
            .iter()
            .filter_map(|section| map_plex_section(&input_name, &server_id, section))
            .collect();

        assert!(libraries.iter().any(|library| library.kind == MediaServerLibraryKind::Unsupported));
        assert!(libraries.iter().any(|library| library.kind == MediaServerLibraryKind::Movies));
        assert!(libraries.iter().any(|library| library.kind == MediaServerLibraryKind::TvShows));
    }
}
