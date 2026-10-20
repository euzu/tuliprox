use crate::{
    emby::{
        dto::{
            EmbyAuthenticateByNameRequest, EmbyAuthenticationResultDto, EmbyItemDto, EmbyItemsPageDto,
            EmbyPublicSystemInfoDto, EmbyViewDto, EmbyVirtualFolderDto,
        },
        flavor::{validate_token, EmbyClientIdentity, EmbyFamilyFlavor, UserScope},
        mapper::{
            emby_item_to_episode, emby_item_to_movie, emby_item_to_season, emby_item_to_series, is_virtual_item,
            EmbyMapContext,
        },
    },
    library_selection::select_libraries,
    non_blank_arc,
    operation::MediaServerOperation,
    redaction::redacted_secret,
    MediaServerCatalogClient, MediaServerEpisode, MediaServerError, MediaServerErrorKind, MediaServerHttpClient,
    MediaServerImageRef, MediaServerLibrary, MediaServerLibraryRef, MediaServerMovie, MediaServerPage,
    MediaServerPageRequest, MediaServerSeason, MediaServerSeries, MediaServerStatus, MediaServerStreamRef,
    MediaServerStreamResponse,
};
use futures::{StreamExt, TryStreamExt};
use reqwest::{
    header::{HeaderValue, RANGE},
    Method,
};
use serde::de::DeserializeOwned;
use shared::{model::MediaServerLibrarySelector, utils::hash_string_as_hex};
use std::{
    collections::HashMap,
    marker::PhantomData,
    sync::{Arc, LazyLock},
};
use tokio::sync::Mutex;
use url::Url;

/// Fields requested for every item; `MediaSources` is appended for playable items when configured.
macro_rules! item_fields {
    () => {
        "ProviderIds,Overview,Genres,Studios,People,OfficialRating,CommunityRating,PremiereDate,DateCreated,\
         OriginalTitle,SortName,Taglines,ProductionLocations,ChildCount,RecursiveItemCount"
    };
}
/// Image kinds an item reference may ask for; anything else never reaches the server.
const IMAGE_KINDS: [&str; 5] = ["Primary", "Backdrop", "Thumb", "Logo", "Banner"];
const ITEM_FIELDS: &str = item_fields!();
const ITEM_FIELDS_WITH_MEDIA_SOURCES: &str = concat!(item_fields!(), ",MediaSources");

/// Session shared by every client with the same cache key. Its lock is the
/// single-flight guard for logging in, scoped to one key so a slow server only
/// delays the clients of its own input.
type SharedConnection = Arc<Mutex<Option<EmbyConnectionState>>>;

/// Only ever locked to look up or insert a cell, never across `.await`.
static EMBY_CONNECTION_CACHE: LazyLock<std::sync::Mutex<HashMap<EmbyConnectionCacheKey, SharedConnection>>> =
    LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

fn shared_connection(key: &EmbyConnectionCacheKey) -> SharedConnection {
    let mut cache = EMBY_CONNECTION_CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    prune_idle_empty_connections(&mut cache);
    Arc::clone(cache.entry(key.clone()).or_default())
}

/// Failed connects and invalidated sessions leave empty cells behind. A cell no
/// client holds anymore and that carries no session is dropped; cells in use stay,
/// so removing one can never split a running single-flight login into two.
fn prune_idle_empty_connections(cache: &mut HashMap<EmbyConnectionCacheKey, SharedConnection>) {
    cache.retain(|_, cell| Arc::strong_count(cell) > 1 || cell.try_lock().map_or(true, |state| state.is_some()));
}

/// Everything an Emby-family catalog client needs in order to connect.
///
/// This is the boundary type: the caller resolves its own configuration and hands
/// over the resolved values, so this crate never names the application's
/// configuration model.
#[derive(Clone, Default)]
pub struct EmbyClientSettings {
    pub token: Option<String>,
    pub api_key: Option<String>,
    pub user_id: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub include_media_sources: bool,
    pub libraries: Vec<MediaServerLibrarySelector>,
}

impl std::fmt::Debug for EmbyClientSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbyClientSettings")
            .field("token", &redacted_secret(self.token.as_deref()))
            .field("api_key", &redacted_secret(self.api_key.as_deref()))
            .field("user_id", &self.user_id)
            .field("username", &self.username)
            .field("password", &redacted_secret(self.password.as_deref()))
            .field("include_media_sources", &self.include_media_sources)
            .field("libraries", &self.libraries)
            .finish()
    }
}

struct EmbyClientConfig {
    input_name: Arc<str>,
    base_url: Url,
    token: Option<Arc<str>>,
    api_key: Option<Arc<str>>,
    user_id: Option<Arc<str>>,
    username: Option<Arc<str>>,
    password: Option<Arc<str>>,
    include_media_sources: bool,
    libraries: Vec<MediaServerLibrarySelector>,
}

/// Where the token came from decides whether a `401` can be healed by logging in again.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum TokenOrigin {
    Configured,
    Login,
}

#[derive(Debug, Clone)]
struct EmbyConnectionState {
    status: MediaServerStatus,
    token: Arc<str>,
    user_id: Option<Arc<str>>,
    origin: TokenOrigin,
}

/// Secrets are reduced to fingerprints so the process-wide cache holds no extra plaintext copies.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct EmbyConnectionCacheKey {
    kind: crate::MediaServerKind,
    input_name: Arc<str>,
    base_url: Arc<str>,
    token: Option<Arc<str>>,
    api_key: Option<Arc<str>>,
    user_id: Option<Arc<str>>,
    username: Option<Arc<str>>,
    password: Option<Arc<str>>,
}

impl EmbyConnectionCacheKey {
    fn new(kind: crate::MediaServerKind, config: &EmbyClientConfig) -> Self {
        Self {
            kind,
            input_name: Arc::clone(&config.input_name),
            base_url: Arc::from(config.base_url.as_str()),
            token: fingerprint(config.token.as_ref()),
            api_key: fingerprint(config.api_key.as_ref()),
            user_id: config.user_id.clone(),
            username: config.username.clone(),
            password: fingerprint(config.password.as_ref()),
        }
    }
}

fn fingerprint(secret: Option<&Arc<str>>) -> Option<Arc<str>> {
    secret.map(|secret| Arc::<str>::from(hash_string_as_hex(secret)))
}

pub struct EmbyFamilyCatalogClient<F: EmbyFamilyFlavor> {
    http: MediaServerHttpClient,
    config: EmbyClientConfig,
    identity: EmbyClientIdentity,
    shared: SharedConnection,
    connection: Mutex<Option<EmbyConnectionState>>,
    _flavor: PhantomData<fn() -> F>,
}

impl<F: EmbyFamilyFlavor> EmbyFamilyCatalogClient<F> {
    pub fn new(
        input_name: Arc<str>,
        base_url: &str,
        settings: &EmbyClientSettings,
        http: MediaServerHttpClient,
    ) -> Result<Self, MediaServerError> {
        let base_url = parse_base_url(base_url).ok_or_else(|| {
            MediaServerError::new(MediaServerErrorKind::MediaServerDiscoveryFailed)
                .provider(F::PROVIDER)
                .detail("server URL is missing or not a valid http(s) URL")
        })?;
        let username = non_blank_arc(settings.username.as_deref());
        let identity = EmbyClientIdentity::new(&input_name, username.as_deref());
        let config = EmbyClientConfig {
            input_name,
            base_url,
            token: non_blank_arc(settings.token.as_deref()),
            api_key: non_blank_arc(settings.api_key.as_deref()),
            user_id: non_blank_arc(settings.user_id.as_deref()),
            username,
            // Passwords are significant byte for byte, only an empty one counts as unset.
            password: settings.password.as_deref().filter(|password| !password.is_empty()).map(Arc::<str>::from),
            include_media_sources: settings.include_media_sources,
            libraries: settings.libraries.clone(),
        };
        let shared = shared_connection(&EmbyConnectionCacheKey::new(F::KIND, &config));
        Ok(Self { http, config, identity, shared, connection: Mutex::new(None), _flavor: PhantomData })
    }

    /// Cached session, or a fresh one. Connecting happens under the per-key lock:
    /// Jellyfin keeps a single token per `DeviceId`, so two concurrent logins
    /// would invalidate each other. The lock is never held during catalog or
    /// stream requests.
    async fn connection(&self) -> Result<EmbyConnectionState, MediaServerError> {
        if let Some(connection) = self.connection.lock().await.clone() {
            return Ok(connection);
        }

        let connection = {
            let mut shared = self.shared.lock().await;
            if let Some(connection) = shared.as_ref() {
                connection.clone()
            } else {
                let connection = self.establish().await?;
                *shared = Some(connection.clone());
                connection
            }
        };
        *self.connection.lock().await = Some(connection.clone());
        Ok(connection)
    }

    /// Drops the cached session, but only while it still holds `failed_token`, so a
    /// client that already re-logged in is not thrown away by a slower sibling.
    async fn invalidate(&self, failed_token: &str) {
        {
            let mut shared = self.shared.lock().await;
            if shared.as_ref().is_some_and(|connection| connection.token.as_ref() == failed_token) {
                *shared = None;
            }
        }
        let mut local = self.connection.lock().await;
        if local.as_ref().is_some_and(|connection| connection.token.as_ref() == failed_token) {
            *local = None;
        }
    }

    async fn establish(&self) -> Result<EmbyConnectionState, MediaServerError> {
        let info: EmbyPublicSystemInfoDto = self.discover_public_info().await?;

        // Token, then API key, then login with the input's username and password.
        let (token, origin, mut user_id, login_server_id) =
            if let Some(token) = self.config.token.as_ref().or(self.config.api_key.as_ref()) {
                validate_token(token, F::PROVIDER)?;
                (Arc::clone(token), TokenOrigin::Configured, None, None)
            } else if let Some(username) = self.config.username.as_deref() {
                let password = self.config.password.as_deref().unwrap_or_default();
                let login = self.login(username, password).await?;
                let token = non_blank_arc(login.access_token.as_deref()).ok_or_else(|| {
                    MediaServerError::new(MediaServerErrorKind::MediaServerAuthDenied)
                        .provider(F::PROVIDER)
                        .detail("login did not return an access token")
                })?;
                let user_id = login.user.as_ref().and_then(|user| non_blank_arc(user.id.as_deref()));
                (token, TokenOrigin::Login, user_id, non_blank_arc(login.server_id.as_deref()))
            } else {
                return Err(MediaServerError::new(MediaServerErrorKind::MediaServerAuthDenied)
                    .provider(F::PROVIDER)
                    .detail("no token, api_key or username/password is configured"));
            };
        if let Some(configured) = self.config.user_id.as_ref() {
            user_id = Some(Arc::clone(configured));
        }

        let server_id = non_blank_arc(info.id.as_deref()).or(login_server_id).ok_or_else(|| {
            MediaServerError::new(MediaServerErrorKind::MediaServerDiscoveryFailed)
                .provider(F::PROVIDER)
                .detail("server did not expose a stable id")
        })?;
        let status = MediaServerStatus {
            kind: F::KIND,
            server_id,
            display_name: non_blank_arc(info.server_name.as_deref()),
            version: non_blank_arc(info.version.as_deref()),
            owned: None,
        };
        Ok(EmbyConnectionState { status, token, user_id, origin })
    }

    async fn discover_public_info(&self) -> Result<EmbyPublicSystemInfoDto, MediaServerError> {
        let url = self.url(&["System", "Info", "Public"], &[])?;
        let response = self
            .identity_request(Method::GET, &url)?
            .send_with_error_detail(format!("{} discovery request failed", F::PROVIDER))
            .await?;
        self.read_json(response, MediaServerOperation::Discovery, "discovery").await
    }

    async fn login(&self, username: &str, password: &str) -> Result<EmbyAuthenticationResultDto, MediaServerError> {
        let url = self.url(&["Users", "AuthenticateByName"], &[])?;
        let response = self
            .identity_request(Method::POST, &url)?
            .json(&EmbyAuthenticateByNameRequest { username, pw: password })
            .send_with_error_detail(format!("{} login request failed", F::PROVIDER))
            .await?;
        self.read_json(response, MediaServerOperation::Discovery, "login").await
    }

    /// Request carrying only the client identity: discovery and login are made without a token.
    fn identity_request(
        &self,
        method: Method,
        url: &Url,
    ) -> Result<crate::client::MediaServerHttpRequestBuilder, MediaServerError> {
        let headers = F::auth_headers(&self.identity, None)?;
        Ok(self.http.request_for(method, url.as_str(), MediaServerOperation::Discovery).headers(headers))
    }

    /// Decodes a JSON body after checking the status; `what` names the call in error details.
    async fn read_json<T: DeserializeOwned>(
        &self,
        response: reqwest::Response,
        operation: MediaServerOperation,
        what: &str,
    ) -> Result<T, MediaServerError> {
        let status = response.status();
        if !status.is_success() {
            return Err(operation
                .status_error(status, F::PROVIDER)
                .detail(format!("{} {what} returned {status}", F::PROVIDER)));
        }
        response.json::<T>().await.map_err(|err| {
            operation.reqwest_error(&err, F::PROVIDER).detail(format!("{} {what} response decode failed", F::PROVIDER))
        })
    }

    fn url(&self, segments: &[&str], query: &[(&str, &str)]) -> Result<Url, MediaServerError> {
        endpoint_url(&self.config.base_url, segments, query).ok_or_else(|| {
            MediaServerError::new(MediaServerErrorKind::MediaServerDiscoveryFailed)
                .provider(F::PROVIDER)
                .detail("server URL cannot carry a request path")
        })
    }

    /// Sends an authenticated request. A `401` on a token obtained by logging in
    /// (expired, or revoked by a server restart) evicts the session, logs in again
    /// and retries once; any other `401` is `MediaServerAuthDenied`.
    async fn send_authenticated(
        &self,
        operation: MediaServerOperation,
        method: Method,
        url: &Url,
        range: Option<&HeaderValue>,
    ) -> Result<reqwest::Response, MediaServerError> {
        let mut connection = self.connection().await?;
        let mut relogged = false;
        loop {
            let mut headers = F::auth_headers(&self.identity, Some(&connection.token))?;
            if let Some(range) = range {
                headers.insert(RANGE, range.clone());
            }
            let response = self
                .http
                .request_for(method.clone(), url.as_str(), operation)
                .headers(headers)
                .send_with_error_detail(format!("{} request failed", F::PROVIDER))
                .await?;
            let status = response.status();
            if status.is_success() {
                return Ok(response);
            }
            if status == reqwest::StatusCode::UNAUTHORIZED && connection.origin == TokenOrigin::Login && !relogged {
                relogged = true;
                self.invalidate(&connection.token).await;
                connection = self.connection().await?;
                continue;
            }
            return Err(operation
                .status_error(status, F::PROVIDER)
                .detail(format!("{} request returned {status}", F::PROVIDER)));
        }
    }

    async fn get_json<T: DeserializeOwned>(&self, url: &Url) -> Result<T, MediaServerError> {
        let response = self.send_authenticated(MediaServerOperation::Catalog, Method::GET, url, None).await?;
        self.read_json(response, MediaServerOperation::Catalog, "catalog").await
    }

    async fn fetch_item_page(
        &self,
        connection: &EmbyConnectionState,
        library: &MediaServerLibraryRef,
        page: MediaServerPageRequest,
        item_type: &'static str,
    ) -> Result<EmbyItemsPageDto<EmbyItemDto>, MediaServerError> {
        let user_id = connection.user_id.as_deref();
        let endpoint = F::items_path(user_id);
        let fields = if self.config.include_media_sources && matches!(item_type, "Movie" | "Episode") {
            ITEM_FIELDS_WITH_MEDIA_SOURCES
        } else {
            ITEM_FIELDS
        };
        let start = page.start.to_string();
        let limit = page.limit.to_string();
        let mut query = vec![
            ("IncludeItemTypes", item_type),
            ("ParentId", library.library_id.as_ref()),
            ("Recursive", "true"),
            ("Fields", fields),
            ("StartIndex", start.as_str()),
            ("Limit", limit.as_str()),
            ("EnableTotalRecordCount", "true"),
            ("SortBy", "SortName"),
            ("SortOrder", "Ascending"),
            ("CollapseBoxSetItems", "false"),
        ];
        if let (UserScope::Query, Some(user_id)) = (endpoint.user_scope, user_id) {
            query.push(("UserId", user_id));
        }
        let url = self.url(endpoint.path.segments(), &query)?;
        self.get_json(&url).await
    }

    async fn list_items<T>(
        &self,
        library: &MediaServerLibraryRef,
        page: MediaServerPageRequest,
        item_type: &'static str,
        skip_virtual: bool,
        map: impl Fn(&EmbyMapContext<'_>, &EmbyItemDto) -> Option<T>,
    ) -> Result<MediaServerPage<T>, MediaServerError> {
        let connection = self.connection().await?;
        let upstream = self.fetch_item_page(&connection, library, page, item_type).await?;
        let ctx = EmbyMapContext {
            input_name: &self.config.input_name,
            server_id: &connection.status.server_id,
            library_id: &library.library_id,
            include_media_sources: self.config.include_media_sources,
        };
        let upstream_item_count = upstream.items.len();
        // A missing TotalRecordCount deserializes as 0; treating that as "unknown"
        // keeps paging until the server returns an empty page instead of truncating.
        let total =
            (upstream.total_record_count > 0 || upstream.items.is_empty()).then_some(upstream.total_record_count);
        let items = upstream
            .items
            .iter()
            .filter(|item| !(skip_virtual && is_virtual_item(item)))
            .filter_map(|item| map(&ctx, item))
            .collect();
        Ok(MediaServerPage::with_upstream_item_count(page, total, upstream_item_count, items))
    }
}

#[allow(async_fn_in_trait)]
impl<F: EmbyFamilyFlavor> MediaServerCatalogClient for EmbyFamilyCatalogClient<F> {
    async fn discover(&self) -> Result<MediaServerStatus, MediaServerError> { Ok(self.connection().await?.status) }

    async fn list_libraries(&self) -> Result<Vec<MediaServerLibrary>, MediaServerError> {
        let connection = self.connection().await?;
        let server_id = &connection.status.server_id;
        let input_name = &self.config.input_name;
        if let Some(user_id) = connection.user_id.as_deref() {
            let endpoint = F::user_views_path(user_id);
            let query: &[(&str, &str)] = match endpoint.user_scope {
                UserScope::Query => &[("UserId", user_id)],
                UserScope::None | UserScope::InPath => &[],
            };
            let views: EmbyItemsPageDto<EmbyViewDto> =
                self.get_json(&self.url(endpoint.path.segments(), query)?).await?;
            select_libraries(F::PROVIDER, input_name, server_id, &self.config.libraries, &views.items)
        } else {
            let folders: Vec<EmbyVirtualFolderDto> =
                self.get_json(&self.url(&["Library", "VirtualFolders"], &[])?).await?;
            select_libraries(F::PROVIDER, input_name, server_id, &self.config.libraries, &folders)
        }
    }

    async fn list_movies(
        &self,
        library: &MediaServerLibraryRef,
        page: MediaServerPageRequest,
    ) -> Result<MediaServerPage<MediaServerMovie>, MediaServerError> {
        self.list_items(library, page, "Movie", true, emby_item_to_movie::<F>).await
    }

    async fn list_series(
        &self,
        library: &MediaServerLibraryRef,
        page: MediaServerPageRequest,
    ) -> Result<MediaServerPage<MediaServerSeries>, MediaServerError> {
        self.list_items(library, page, "Series", false, emby_item_to_series::<F>).await
    }

    async fn list_seasons(
        &self,
        library: &MediaServerLibraryRef,
        page: MediaServerPageRequest,
    ) -> Result<MediaServerPage<MediaServerSeason>, MediaServerError> {
        self.list_items(library, page, "Season", false, emby_item_to_season::<F>).await
    }

    async fn list_episodes(
        &self,
        library: &MediaServerLibraryRef,
        page: MediaServerPageRequest,
    ) -> Result<MediaServerPage<MediaServerEpisode>, MediaServerError> {
        self.list_items(library, page, "Episode", true, emby_item_to_episode::<F>).await
    }

    async fn open_stream(
        &self,
        stream_ref: &MediaServerStreamRef,
        range: Option<&str>,
    ) -> Result<MediaServerStreamResponse, MediaServerError> {
        let Some(parts) = F::stream_ref_parts(stream_ref) else {
            return Err(MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
                .provider(F::PROVIDER)
                .detail("stream open received a stream ref of another media server kind"));
        };
        if parts.input_name != self.config.input_name.as_ref() {
            return Err(MediaServerError::new(MediaServerErrorKind::MediaServerItemNotFound)
                .provider(F::PROVIDER)
                .detail("stream ref did not belong to the selected input"));
        }
        if parts.item_id.trim().is_empty() {
            return Err(MediaServerError::new(MediaServerErrorKind::NoDirectPlayableMediaServerSource)
                .provider(F::PROVIDER)
                .detail("stream ref is missing a stable item id"));
        }
        let range = range
            .map(str::trim)
            .filter(|range| !range.is_empty())
            .map(|range| {
                HeaderValue::from_str(range).map_err(|_| {
                    MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
                        .provider(F::PROVIDER)
                        .detail("Range header contains invalid characters")
                })
            })
            .transpose()?;

        let connection = self.connection().await?;
        if parts.server_id != connection.status.server_id.as_ref() {
            return Err(MediaServerError::new(MediaServerErrorKind::MediaServerItemNotFound)
                .provider(F::PROVIDER)
                .detail("stream ref did not belong to the selected server"));
        }

        let mut query = vec![("static", "true")];
        if let Some(media_source_id) = parts.media_source_id.map(str::trim).filter(|id| !id.is_empty()) {
            query.push(("mediaSourceId", media_source_id));
        }
        let url = self.url(&["Videos", parts.item_id, "stream"], &query)?;
        let response =
            self.send_authenticated(MediaServerOperation::Playback, Method::GET, &url, range.as_ref()).await?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .bytes_stream()
            .map_err(|err| {
                MediaServerOperation::Playback
                    .reqwest_error(&err, F::PROVIDER)
                    .detail(format!("{} stream body read failed", F::PROVIDER))
            })
            .boxed();
        Ok(MediaServerStreamResponse { status, headers, body })
    }

    async fn open_image(&self, image_ref: &MediaServerImageRef) -> Result<MediaServerStreamResponse, MediaServerError> {
        let Some(parts) = F::image_ref_parts(image_ref) else {
            return Err(MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
                .provider(F::PROVIDER)
                .detail("image open received an image ref of another media server kind"));
        };
        let not_found = |detail: &'static str| {
            MediaServerError::new(MediaServerErrorKind::MediaServerItemNotFound).provider(F::PROVIDER).detail(detail)
        };
        if parts.input_name != self.config.input_name.as_ref() {
            return Err(not_found("image ref did not belong to the selected input"));
        }
        if parts.item_id.trim().is_empty() {
            return Err(not_found("image ref is missing a stable item id"));
        }
        if !IMAGE_KINDS.contains(&parts.image_kind) {
            return Err(not_found("image kind is not served"));
        }

        let connection = self.connection().await?;
        if parts.server_id != connection.status.server_id.as_ref() {
            return Err(not_found("image ref did not belong to the selected server"));
        }

        let tag = parts.tag.map(str::trim).filter(|tag| !tag.is_empty());
        let query: &[(&str, &str)] = if let Some(tag) = tag { &[("tag", tag)] } else { &[] };
        let url = self.url(&["Items", parts.item_id, "Images", parts.image_kind], query)?;
        let response = self.send_authenticated(MediaServerOperation::Playback, Method::GET, &url, None).await?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .bytes_stream()
            .map_err(|err| {
                MediaServerOperation::Playback
                    .reqwest_error(&err, F::PROVIDER)
                    .detail(format!("{} image body read failed", F::PROVIDER))
            })
            .boxed();
        Ok(MediaServerStreamResponse { status, headers, body })
    }
}

/// Configured server URL reduced to an http(s) base that requests are appended to.
/// Query and fragment are dropped; a path prefix such as `/jellyfin` is kept.
fn parse_base_url(base_url: &str) -> Option<Url> {
    let mut url = Url::parse(base_url.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() || url.cannot_be_a_base() {
        return None;
    }
    url.set_query(None);
    url.set_fragment(None);
    Some(url)
}

/// `base` with `segments` appended (percent-encoded, a trailing slash on the base
/// path does not produce an empty segment) and `query` as encoded pairs.
fn endpoint_url(base: &Url, segments: &[&str], query: &[(&str, &str)]) -> Option<Url> {
    let mut url = base.clone();
    url.path_segments_mut().ok()?.pop_if_empty().extend(segments);
    if !query.is_empty() {
        url.query_pairs_mut().extend_pairs(query);
    }
    Some(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        emby::flavor::Emby,
        jellyfin::flavor::Jellyfin,
        refresh_media_server_catalog_complete_before_publish,
        test_fixtures::{
            EMBY_AUTH_RESULT_JSON, EMBY_EPISODES_PAGE_JSON, EMBY_ITEMS_PAGE_JSON, EMBY_PUBLIC_SYSTEM_INFO_JSON,
            EMBY_SEASONS_PAGE_JSON, EMBY_SERIES_PAGE_JSON, EMBY_VIRTUAL_FOLDERS_JSON, JELLYFIN_VIEWS_JSON,
        },
        test_server::start_test_server,
        MediaServerCatalogRefreshPolicy, MediaServerCatalogSnapshot,
    };
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper::{body::Incoming, http::request::Parts, Request, Response};
    use reqwest::StatusCode;
    use shared::model::MediaServerLibrarySelectorDetailsDto;
    use std::{
        convert::Infallible,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Mutex as StdMutex,
        },
    };
    use tokio::task::JoinHandle;

    const SERVER_ID: &str = "server-redacted-id";
    const MOVIES_LIBRARY_ID: &str = "library-redacted-movies";
    const TV_LIBRARY_ID: &str = "library-redacted-tv";

    /// Scriptable Emby/Jellyfin server. It enforces the auth scheme of `provider`
    /// strictly and records every request.
    struct MockServer {
        provider: &'static str,
        prefix: &'static str,
        valid_token: StdMutex<String>,
        login_issues_valid_token: AtomicBool,
        login_count: AtomicUsize,
        login_bodies: StdMutex<Vec<String>>,
        legacy_auth_violations: AtomicUsize,
        log: StdMutex<Vec<String>>,
        movie_total: Option<usize>,
        last_range: StdMutex<Option<String>>,
    }

    impl MockServer {
        fn new(provider: &'static str, valid_token: &str) -> Self {
            Self {
                provider,
                prefix: "",
                valid_token: StdMutex::new(valid_token.to_string()),
                login_issues_valid_token: AtomicBool::new(true),
                login_count: AtomicUsize::new(0),
                login_bodies: StdMutex::new(Vec::new()),
                legacy_auth_violations: AtomicUsize::new(0),
                log: StdMutex::new(Vec::new()),
                movie_total: None,
                last_range: StdMutex::new(None),
            }
        }

        fn with_prefix(mut self, prefix: &'static str) -> Self {
            self.prefix = prefix;
            self
        }

        fn with_movie_total(mut self, total: usize) -> Self {
            self.movie_total = Some(total);
            self
        }

        fn set_valid_token(&self, token: &str) { *self.valid_token.lock().expect("lock") = token.to_string(); }

        fn log(&self) -> Vec<String> { self.log.lock().expect("lock").clone() }

        fn logins(&self) -> usize { self.login_count.load(Ordering::SeqCst) }

        fn is_jellyfin(&self) -> bool { self.provider == "jellyfin" }

        /// `(identity header present, token)` as the client sent them.
        fn credentials(&self, parts: &Parts) -> (bool, Option<String>) {
            let header = |name: &str| parts.headers.get(name).and_then(|value| value.to_str().ok()).map(str::to_string);
            if self.is_jellyfin() {
                let authorization = header("authorization");
                let identity = authorization
                    .as_deref()
                    .is_some_and(|value| value.starts_with("MediaBrowser ") && value.contains("Client=\"Tuliprox\""));
                let token = authorization.as_deref().and_then(|value| {
                    let rest = value.split_once("Token=\"")?.1;
                    Some(rest.split_once('"')?.0.to_string())
                });
                (identity, token)
            } else {
                let identity = header("x-emby-authorization")
                    .is_some_and(|value| value.starts_with("MediaBrowser ") && value.contains("Device=\"Tuliprox\""));
                (identity, header("x-emby-token"))
            }
        }

        fn legacy_auth_used(&self, parts: &Parts) -> bool {
            self.is_jellyfin()
                && (parts.headers.contains_key("x-emby-token")
                    || parts.headers.contains_key("x-emby-authorization")
                    || parts.headers.contains_key("x-mediabrowser-token")
                    || parts.uri.query().is_some_and(|query| query.to_ascii_lowercase().contains("api_key")))
        }

        fn route(&self, parts: &Parts, body: &[u8]) -> Response<Full<Bytes>> {
            let Some(path) = parts.uri.path().strip_prefix(self.prefix) else {
                return respond(StatusCode::NOT_FOUND, "{}");
            };
            if self.legacy_auth_used(parts) {
                self.legacy_auth_violations.fetch_add(1, Ordering::SeqCst);
                return respond(StatusCode::UNAUTHORIZED, "{}");
            }
            let (has_identity, token) = self.credentials(parts);
            if !has_identity {
                return respond(StatusCode::BAD_REQUEST, "missing client identity");
            }
            let query: Vec<(String, String)> =
                url::form_urlencoded::parse(parts.uri.query().unwrap_or_default().as_bytes()).into_owned().collect();
            let segments: Vec<&str> = path.trim_matches('/').split('/').collect();

            match (parts.method.as_str(), segments.as_slice()) {
                ("GET", ["System", "Info", "Public"]) if token.is_none() => {
                    respond(StatusCode::OK, EMBY_PUBLIC_SYSTEM_INFO_JSON)
                }
                ("POST", ["Users", "AuthenticateByName"]) if token.is_none() => {
                    let count = self.login_count.fetch_add(1, Ordering::SeqCst) + 1;
                    self.login_bodies.lock().expect("lock").push(String::from_utf8_lossy(body).into_owned());
                    let issued = format!("login-token-{count}");
                    if self.login_issues_valid_token.load(Ordering::SeqCst) {
                        self.set_valid_token(&issued);
                    }
                    respond(StatusCode::OK, &EMBY_AUTH_RESULT_JSON.replace("token-redacted", &issued))
                }
                ("GET", ["System", "Info", "Public"] | ["Users", "AuthenticateByName"]) => {
                    respond(StatusCode::BAD_REQUEST, "anonymous endpoint received a token")
                }
                _ if token.as_deref() != Some(self.valid_token.lock().expect("lock").as_str()) => {
                    respond(StatusCode::UNAUTHORIZED, "{}")
                }
                ("GET", ["Library", "VirtualFolders"]) => respond(StatusCode::OK, EMBY_VIRTUAL_FOLDERS_JSON),
                ("GET", ["UserViews"] | ["Users", _, "Views"]) => respond(StatusCode::OK, JELLYFIN_VIEWS_JSON),
                ("GET", ["Items"] | ["Users", _, "Items"]) => respond(StatusCode::OK, &self.items_body(&query)),
                ("GET", ["Videos", _, "stream"]) => {
                    let range = parts.headers.get("range").and_then(|value| value.to_str().ok()).map(str::to_string);
                    let ranged = range.is_some();
                    *self.last_range.lock().expect("lock") = range;
                    // Headers the proxy must never pass on to a player.
                    let mut headers = vec![
                        ("content-type", "video/x-matroska"),
                        ("accept-ranges", "bytes"),
                        ("x-emby-token", "leaked-token"),
                        ("authorization", "leaked-authorization"),
                        ("set-cookie", "leaked=cookie"),
                    ];
                    if ranged {
                        headers.push(("content-range", "bytes 5-8/9"));
                    }
                    let status = if ranged { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK };
                    respond_with(status, "data", &headers)
                }
                ("GET", ["Items", _, "Images", _]) => {
                    respond_with(StatusCode::OK, "image-bytes", &[("content-type", "image/jpeg")])
                }
                _ => respond(StatusCode::NOT_FOUND, "{}"),
            }
        }

        fn items_body(&self, query: &[(String, String)]) -> String {
            let param = |name: &str| query.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str());
            match param("IncludeItemTypes") {
                Some("Movie") => match self.movie_total {
                    Some(total) => {
                        let start: usize = param("StartIndex").and_then(|value| value.parse().ok()).unwrap_or(0);
                        let limit: usize = param("Limit").and_then(|value| value.parse().ok()).unwrap_or(0);
                        let items = (start..total.min(start + limit))
                            .map(|index| format!(r#"{{"Id":"movie-{index}","Name":"Movie {index}","Type":"Movie"}}"#))
                            .collect::<Vec<_>>()
                            .join(",");
                        format!(r#"{{"Items":[{items}],"TotalRecordCount":{total},"StartIndex":{start}}}"#)
                    }
                    None => EMBY_ITEMS_PAGE_JSON.to_string(),
                },
                Some("Series") => EMBY_SERIES_PAGE_JSON.to_string(),
                Some("Season") => EMBY_SEASONS_PAGE_JSON.to_string(),
                Some("Episode") => EMBY_EPISODES_PAGE_JSON.to_string(),
                _ => r#"{"Items":[],"TotalRecordCount":0}"#.to_string(),
            }
        }
    }

    fn respond(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
        respond_with(status, body, &[("content-type", "application/json")])
    }

    fn respond_with(status: StatusCode, body: &str, headers: &[(&str, &str)]) -> Response<Full<Bytes>> {
        let mut builder = Response::builder().status(status);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Full::new(Bytes::from(body.to_string()))).expect("response builds")
    }

    async fn handle(state: Arc<MockServer>, request: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
        let (parts, body) = request.into_parts();
        let body = body.collect().await.map(http_body_util::Collected::to_bytes).unwrap_or_default();
        let target = parts.uri.path_and_query().map(ToString::to_string).unwrap_or_default();
        state.log.lock().expect("lock").push(format!("{} {target}", parts.method));
        Ok(state.route(&parts, &body))
    }

    async fn serve(state: &Arc<MockServer>) -> (String, JoinHandle<()>) {
        let state = Arc::clone(state);
        start_test_server(move |request| handle(Arc::clone(&state), request)).await
    }

    fn http() -> MediaServerHttpClient {
        MediaServerHttpClient::new(reqwest::Client::builder().no_proxy().build().expect("client builds"))
    }

    fn movies_only() -> Vec<MediaServerLibrarySelector> { vec![MediaServerLibrarySelector::Name("Movies".to_string())] }

    fn token_settings(token: &str) -> EmbyClientSettings {
        EmbyClientSettings { token: Some(token.to_string()), libraries: movies_only(), ..EmbyClientSettings::default() }
    }

    fn login_settings() -> EmbyClientSettings {
        EmbyClientSettings {
            username: Some("alice".to_string()),
            password: Some("pw".to_string()),
            libraries: movies_only(),
            ..EmbyClientSettings::default()
        }
    }

    fn client<F: EmbyFamilyFlavor>(
        input_name: &str,
        base_url: &str,
        settings: &EmbyClientSettings,
    ) -> EmbyFamilyCatalogClient<F> {
        EmbyFamilyCatalogClient::new(Arc::from(input_name), base_url, settings, http()).expect("client builds")
    }

    async fn refresh<C: MediaServerCatalogClient>(
        client: &C,
        page_size: usize,
    ) -> Result<MediaServerCatalogSnapshot, MediaServerError> {
        refresh_media_server_catalog_complete_before_publish(
            client,
            MediaServerCatalogRefreshPolicy { page_size, request_delay_ms: 0 },
        )
        .await
    }

    fn library_ref(input_name: &str, library_id: &str) -> MediaServerLibraryRef {
        MediaServerLibraryRef {
            input_name: Arc::from(input_name),
            server_id: Arc::from(SERVER_ID),
            library_id: Arc::from(library_id),
        }
    }

    fn query_value(entry: &str, name: &str) -> Option<String> {
        let query = entry.split_once('?')?.1;
        url::form_urlencoded::parse(query.as_bytes()).find(|(key, _)| key == name).map(|(_, value)| value.into_owned())
    }

    fn item_requests(log: &[String]) -> Vec<&String> { log.iter().filter(|entry| entry.contains("/Items?")).collect() }

    async fn token_import_succeeds<F: EmbyFamilyFlavor>(input_name: &str) {
        let state = Arc::new(MockServer::new(F::PROVIDER, "secret"));
        let (base_url, server) = serve(&state).await;
        let client = client::<F>(input_name, &base_url, &token_settings("secret"));

        let snapshot = refresh(&client, 100).await.expect("refresh succeeds");

        assert_eq!(snapshot.movies.len(), 1);
        assert_eq!(snapshot.movies[0].server_id.as_ref(), SERVER_ID);
        assert_eq!(state.legacy_auth_violations.load(Ordering::SeqCst), 0);
        assert_eq!(state.logins(), 0);
        assert!(state.log().iter().all(|entry| !entry.contains("secret")), "token must never be part of a URL");
        server.abort();
    }

    #[tokio::test]
    async fn jellyfin_import_uses_authorization_header_only() {
        token_import_succeeds::<Jellyfin>("jellyfin-header-only").await;
    }

    #[tokio::test]
    async fn emby_import_uses_x_emby_token() { token_import_succeeds::<Emby>("emby-x-emby-token").await; }

    async fn prefix_is_preserved<F: EmbyFamilyFlavor>(input_name: &str, suffix: &str) {
        let state = Arc::new(MockServer::new(F::PROVIDER, "secret").with_prefix("/media/pre"));
        let (base_url, server) = serve(&state).await;
        let client = client::<F>(input_name, &format!("{base_url}/media/pre{suffix}"), &token_settings("secret"));

        let snapshot = refresh(&client, 100).await.expect("refresh succeeds under the prefix");

        assert_eq!(snapshot.movies.len(), 1);
        let log = state.log();
        assert_eq!(log.len(), 3, "discovery, libraries and items were requested: {log:?}");
        assert!(log.iter().all(|entry| entry.split(' ').nth(1).is_some_and(|path| path.starts_with("/media/pre/"))));
        server.abort();
    }

    #[tokio::test]
    async fn catalog_requests_keep_base_url_path_prefix() {
        prefix_is_preserved::<Jellyfin>("prefix-jellyfin-slash", "/").await;
        prefix_is_preserved::<Jellyfin>("prefix-jellyfin-bare", "").await;
        prefix_is_preserved::<Emby>("prefix-emby-slash", "/").await;
        prefix_is_preserved::<Emby>("prefix-emby-bare", "").await;
    }

    async fn api_key_without_user<F: EmbyFamilyFlavor>(input_name: &str) {
        let state = Arc::new(MockServer::new(F::PROVIDER, "api-secret"));
        let (base_url, server) = serve(&state).await;
        let settings = EmbyClientSettings {
            api_key: Some("api-secret".to_string()),
            libraries: movies_only(),
            ..EmbyClientSettings::default()
        };
        let client = client::<F>(input_name, &base_url, &settings);

        let snapshot = refresh(&client, 100).await.expect("refresh succeeds");

        assert_eq!(snapshot.movies.len(), 1);
        let log = state.log();
        assert!(log.iter().any(|entry| entry == "GET /Library/VirtualFolders"));
        assert!(log.iter().all(|entry| !entry.contains("/UserViews") && !entry.contains("/Users/")));
        let items = item_requests(&log);
        assert_eq!(items.len(), 1);
        assert_eq!(query_value(items[0], "UserId"), None);
        assert_eq!(query_value(items[0], "ParentId").as_deref(), Some(MOVIES_LIBRARY_ID));
        server.abort();
    }

    #[tokio::test]
    async fn api_key_without_user_uses_virtual_folders_and_unscoped_items() {
        api_key_without_user::<Jellyfin>("api-key-jellyfin").await;
        api_key_without_user::<Emby>("api-key-emby").await;
    }

    #[tokio::test]
    async fn user_id_uses_user_views_and_user_scoped_items() {
        for provider in ["jellyfin", "emby"] {
            let state = Arc::new(MockServer::new(provider, "secret"));
            let (base_url, server) = serve(&state).await;
            let settings = EmbyClientSettings { user_id: Some("user 1".to_string()), ..token_settings("secret") };
            let name = format!("user-scoped-{provider}");
            let snapshot = if provider == "jellyfin" {
                refresh(&client::<Jellyfin>(&name, &base_url, &settings), 100).await
            } else {
                refresh(&client::<Emby>(&name, &base_url, &settings), 100).await
            }
            .expect("refresh succeeds");

            assert_eq!(snapshot.movies.len(), 1);
            let log = state.log();
            assert!(log.iter().all(|entry| !entry.contains("VirtualFolders")));
            let items = item_requests(&log);
            assert_eq!(items.len(), 1);
            if provider == "jellyfin" {
                assert!(log.iter().any(|entry| entry == "GET /UserViews?UserId=user+1"));
                assert!(items[0].starts_with("GET /Items?"));
                assert_eq!(query_value(items[0], "UserId").as_deref(), Some("user 1"));
            } else {
                assert!(log.iter().any(|entry| entry == "GET /Users/user%201/Views"));
                assert!(items[0].starts_with("GET /Users/user%201/Items?"));
                assert_eq!(query_value(items[0], "UserId"), None);
            }
            server.abort();
        }
    }

    #[tokio::test]
    async fn item_requests_carry_the_pinned_query() {
        let state = Arc::new(MockServer::new("jellyfin", "secret"));
        let (base_url, server) = serve(&state).await;
        let settings = EmbyClientSettings { include_media_sources: true, ..token_settings("secret") };
        let client = client::<Jellyfin>("pinned-query", &base_url, &settings);

        refresh(&client, 7).await.expect("refresh succeeds");

        let log = state.log();
        let items = item_requests(&log);
        let entry = items[0];
        let expected = [
            ("IncludeItemTypes", "Movie"),
            ("Recursive", "true"),
            ("StartIndex", "0"),
            ("Limit", "7"),
            ("EnableTotalRecordCount", "true"),
            ("SortBy", "SortName"),
            ("SortOrder", "Ascending"),
            ("CollapseBoxSetItems", "false"),
        ];
        for (name, value) in expected {
            assert_eq!(query_value(entry, name).as_deref(), Some(value), "query parameter {name}");
        }
        let fields = query_value(entry, "Fields").expect("Fields requested");
        assert!(fields.starts_with("ProviderIds,Overview,") && fields.ends_with(",MediaSources"));
        server.abort();
    }

    async fn concurrent_login<F: EmbyFamilyFlavor>(input_name: &str) {
        let state = Arc::new(MockServer::new(F::PROVIDER, "unused"));
        let (base_url, server) = serve(&state).await;
        let first = client::<F>(input_name, &base_url, &login_settings());
        let second = client::<F>(input_name, &base_url, &login_settings());

        let (first_snapshot, second_snapshot) = tokio::join!(refresh(&first, 100), refresh(&second, 100));

        assert_eq!(first_snapshot.expect("first refresh").movies.len(), 1);
        assert_eq!(second_snapshot.expect("second refresh").movies.len(), 1);
        assert_eq!(state.logins(), 1, "login is single-flight");
        assert_eq!(state.login_bodies.lock().expect("lock").as_slice(), [r#"{"Username":"alice","Pw":"pw"}"#]);
        // The login result carries the user id, so the user-scoped endpoints are used.
        assert!(state.log().iter().any(|entry| entry.contains("user-redacted-id")));
        assert_eq!(state.legacy_auth_violations.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[tokio::test]
    async fn login_bootstraps_token_and_user_id_once_for_concurrent_clients() {
        concurrent_login::<Jellyfin>("concurrent-login-jellyfin").await;
        concurrent_login::<Emby>("concurrent-login-emby").await;
    }

    async fn relogin<F: EmbyFamilyFlavor>(input_name: &str) {
        let state = Arc::new(MockServer::new(F::PROVIDER, "unused"));
        let (base_url, server) = serve(&state).await;
        let client = client::<F>(input_name, &base_url, &login_settings());
        let library = library_ref(input_name, MOVIES_LIBRARY_ID);
        client.list_libraries().await.expect("first login works");
        assert_eq!(state.logins(), 1);

        state.set_valid_token("rotated-by-server");
        let page = client.list_movies(&library, MediaServerPageRequest::new(0, 100)).await.expect("re-login heals");

        assert_eq!(page.items.len(), 1);
        assert_eq!(state.logins(), 2);
        // The healed session is reused instead of logging in per request.
        client.list_movies(&library, MediaServerPageRequest::new(0, 100)).await.expect("session is reused");
        assert_eq!(state.logins(), 2);
        server.abort();
    }

    #[tokio::test]
    async fn expired_login_token_relogs_once() {
        relogin::<Jellyfin>("relogin-jellyfin").await;
        relogin::<Emby>("relogin-emby").await;
    }

    #[tokio::test]
    async fn second_unauthorized_after_relogin_is_auth_denied_without_third_login() {
        let state = Arc::new(MockServer::new("jellyfin", "unused"));
        let (base_url, server) = serve(&state).await;
        let client = client::<Jellyfin>("relogin-denied", &base_url, &login_settings());
        let library = library_ref("relogin-denied", MOVIES_LIBRARY_ID);
        client.list_libraries().await.expect("first login works");
        state.login_issues_valid_token.store(false, Ordering::SeqCst);
        state.set_valid_token("rotated-by-server");

        let error = client
            .list_movies(&library, MediaServerPageRequest::new(0, 100))
            .await
            .expect_err("second 401 is not retried");

        assert_eq!(error.kind, MediaServerErrorKind::MediaServerAuthDenied);
        assert_eq!(error.status, Some(StatusCode::UNAUTHORIZED));
        assert_eq!(state.logins(), 2);
        server.abort();
    }

    #[tokio::test]
    async fn configured_token_401_is_auth_denied_without_login() {
        for provider in ["jellyfin", "emby"] {
            let state = Arc::new(MockServer::new(provider, "server-side-token"));
            let (base_url, server) = serve(&state).await;
            let settings = EmbyClientSettings {
                username: Some("alice".to_string()),
                password: Some("pw".to_string()),
                ..token_settings("stale-token")
            };
            let name = format!("configured-401-{provider}");
            let error = if provider == "jellyfin" {
                client::<Jellyfin>(&name, &base_url, &settings).list_libraries().await
            } else {
                client::<Emby>(&name, &base_url, &settings).list_libraries().await
            }
            .expect_err("configured token is rejected");

            assert_eq!(error.kind, MediaServerErrorKind::MediaServerAuthDenied);
            assert_eq!(state.logins(), 0);
            assert_eq!(state.log().iter().filter(|entry| entry.contains("VirtualFolders")).count(), 1);
            server.abort();
        }
    }

    #[tokio::test]
    async fn rejected_login_is_auth_denied() {
        let (base_url, server) = start_test_server(|request| async move {
            let status = if request.uri().path() == "/Users/AuthenticateByName" {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::OK
            };
            let body = if status == StatusCode::OK { EMBY_PUBLIC_SYSTEM_INFO_JSON } else { "{}" };
            Ok(respond(status, body))
        })
        .await;
        let client = client::<Jellyfin>("login-rejected", &base_url, &login_settings());

        let error = client.discover().await.expect_err("login is rejected");

        assert_eq!(error.kind, MediaServerErrorKind::MediaServerAuthDenied);
        server.abort();
    }

    #[tokio::test]
    async fn paging_follows_total_record_count() {
        let state = Arc::new(MockServer::new("jellyfin", "secret").with_movie_total(5));
        let (base_url, server) = serve(&state).await;
        let client = client::<Jellyfin>("paging", &base_url, &token_settings("secret"));

        let snapshot = refresh(&client, 2).await.expect("refresh succeeds");

        assert_eq!(snapshot.movies.len(), 5);
        let log = state.log();
        let starts: Vec<_> =
            item_requests(&log).into_iter().filter_map(|entry| query_value(entry, "StartIndex")).collect();
        assert_eq!(starts, ["0", "2", "4"]);
        server.abort();
    }

    #[tokio::test]
    async fn selector_for_mixed_library_is_reported_unsupported() {
        let state = Arc::new(MockServer::new("emby", "api-secret"));
        let (base_url, server) = serve(&state).await;
        let settings = EmbyClientSettings {
            api_key: Some("api-secret".to_string()),
            libraries: vec![MediaServerLibrarySelector::Name("Mixed".to_string())],
            ..EmbyClientSettings::default()
        };
        let client = client::<Emby>("mixed-library", &base_url, &settings);

        let snapshot = refresh(&client, 100).await.expect("refresh succeeds");

        assert_eq!(snapshot.unsupported_libraries.len(), 1);
        assert_eq!(snapshot.unsupported_libraries[0].reference.library_id.as_ref(), "library-redacted-mixed");
        assert_eq!(snapshot.movies.len(), 0);
        assert_eq!(item_requests(&state.log()).len(), 0);
        server.abort();
    }

    #[tokio::test]
    async fn detailed_selector_with_wrong_kind_is_library_unavailable() {
        let state = Arc::new(MockServer::new("emby", "api-secret"));
        let (base_url, server) = serve(&state).await;
        let settings = EmbyClientSettings {
            api_key: Some("api-secret".to_string()),
            libraries: vec![MediaServerLibrarySelector::Detailed(MediaServerLibrarySelectorDetailsDto {
                name: Some("Movies".to_string()),
                kind: Some(shared::model::MediaServerLibraryKind::TvShows),
                ..MediaServerLibrarySelectorDetailsDto::default()
            })],
            ..EmbyClientSettings::default()
        };
        let client = client::<Emby>("wrong-kind", &base_url, &settings);

        let error = client.list_libraries().await.expect_err("kind mismatch is not visible");

        assert_eq!(error.kind, MediaServerErrorKind::MediaServerLibraryUnavailable);
        server.abort();
    }

    #[tokio::test]
    async fn virtual_episode_filtered_but_upstream_counted_in_page() {
        let state = Arc::new(MockServer::new("jellyfin", "secret"));
        let (base_url, server) = serve(&state).await;
        let settings = EmbyClientSettings { include_media_sources: true, ..token_settings("secret") };
        let client = client::<Jellyfin>("virtual-episode", &base_url, &settings);

        let page = client
            .list_episodes(&library_ref("virtual-episode", TV_LIBRARY_ID), MediaServerPageRequest::new(0, 10))
            .await
            .expect("episodes load");

        assert_eq!(page.upstream_item_count(), 3);
        assert_eq!(page.total, Some(3));
        assert_eq!(page.items.len(), 2);
        assert!(page.items.iter().all(|episode| !episode.title.contains("missing")));
        assert!(page.next_request().is_none());
        server.abort();
    }

    #[tokio::test]
    async fn invalid_token_chars_rejected_as_auth_denied() {
        for token in ["bad\"token", "bad token", "bad\ntoken"] {
            let state = Arc::new(MockServer::new("jellyfin", "unused"));
            let (base_url, server) = serve(&state).await;
            let client = client::<Jellyfin>("invalid-token-chars", &base_url, &token_settings(token));

            let error = client.discover().await.expect_err("token is rejected");

            assert_eq!(error.kind, MediaServerErrorKind::MediaServerAuthDenied, "token {token:?}");
            assert!(state.log().iter().all(|entry| !entry.contains("/Items") && !entry.contains("/Library")));
            server.abort();
        }
    }

    #[tokio::test]
    async fn missing_credentials_are_auth_denied() {
        let state = Arc::new(MockServer::new("emby", "unused"));
        let (base_url, server) = serve(&state).await;
        let client = client::<Emby>("no-credentials", &base_url, &EmbyClientSettings::default());

        let error = client.discover().await.expect_err("no credentials");

        assert_eq!(error.kind, MediaServerErrorKind::MediaServerAuthDenied);
        server.abort();
    }

    #[tokio::test]
    async fn discovery_without_any_server_id_fails() {
        let (base_url, server) =
            start_test_server(|_request| async move { Ok(respond(StatusCode::OK, r#"{"ServerName":"x"}"#)) }).await;
        let client = client::<Emby>("no-server-id", &base_url, &token_settings("secret"));

        let error = client.discover().await.expect_err("server id is mandatory");

        assert_eq!(error.kind, MediaServerErrorKind::MediaServerDiscoveryFailed);
        server.abort();
    }

    const ITEM_ID: &str = "movie 1";

    fn stream_ref_for<F: EmbyFamilyFlavor>(
        input_name: &str,
        server_id: &str,
        item_id: &str,
        media_source_id: Option<&str>,
    ) -> MediaServerStreamRef {
        F::stream_ref(
            &Arc::from(input_name),
            &Arc::from(server_id),
            &Arc::from(item_id),
            media_source_id.map(Arc::from),
        )
    }

    fn image_ref_for<F: EmbyFamilyFlavor>(
        input_name: &str,
        server_id: &str,
        item_id: &str,
        image_kind: &str,
        tag: Option<&str>,
    ) -> MediaServerImageRef {
        F::image_ref(
            &Arc::from(input_name),
            &Arc::from(server_id),
            &Arc::from(item_id),
            Arc::from(image_kind),
            tag.map(Arc::from),
        )
    }

    async fn collect_body(response: MediaServerStreamResponse) -> Vec<u8> {
        let chunks: Vec<_> = response.body.collect().await;
        chunks.into_iter().flat_map(|chunk| chunk.expect("chunk reads").to_vec()).collect()
    }

    async fn stream_forwards_range<F: EmbyFamilyFlavor>(input_name: &str) {
        let state = Arc::new(MockServer::new(F::PROVIDER, "secret"));
        let (base_url, server) = serve(&state).await;
        let client = client::<F>(input_name, &base_url, &token_settings("secret"));
        let stream_ref = stream_ref_for::<F>(input_name, SERVER_ID, ITEM_ID, Some("ms-1"));

        let response = client.open_stream(&stream_ref, Some("bytes=5-8")).await.expect("stream opens");

        assert_eq!(response.status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            response.headers.get(reqwest::header::CONTENT_RANGE).and_then(|value| value.to_str().ok()),
            Some("bytes 5-8/9")
        );
        assert_eq!(collect_body(response).await, b"data");
        assert_eq!(state.last_range.lock().expect("lock").as_deref(), Some("bytes=5-8"));
        let log = state.log();
        assert_eq!(log.last().map(String::as_str), Some("GET /Videos/movie%201/stream?static=true&mediaSourceId=ms-1"));
        assert!(log.iter().all(|entry| !entry.contains("secret")), "token must never be part of a URL");
        assert_eq!(state.legacy_auth_violations.load(Ordering::SeqCst), 0);

        // No Range and no media source: plain 200 and a bare `static=true` query.
        let plain = stream_ref_for::<F>(input_name, SERVER_ID, ITEM_ID, None);
        let response = client.open_stream(&plain, Some("  ")).await.expect("plain stream opens");
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(state.last_range.lock().expect("lock").as_deref(), None);
        assert_eq!(state.log().last().map(String::as_str), Some("GET /Videos/movie%201/stream?static=true"));
        server.abort();
    }

    #[test]
    fn settings_debug_hides_secrets() {
        let settings = EmbyClientSettings {
            token: Some("tok-secret".to_string()),
            api_key: Some("key-secret".to_string()),
            user_id: Some("user-1".to_string()),
            username: Some("alice".to_string()),
            password: Some("pw-secret".to_string()),
            ..EmbyClientSettings::default()
        };

        let debug = format!("{settings:?}");

        for secret in ["tok-secret", "key-secret", "pw-secret"] {
            assert!(!debug.contains(secret), "{secret} leaked into {debug}");
        }
        assert!(debug.contains("token: Some(\"***\")") && debug.contains("password: Some(\"***\")"), "{debug}");
        assert!(debug.contains("user-1") && debug.contains("alice"), "{debug}");
        assert!(format!("{:?}", EmbyClientSettings::default()).contains("token: None"));
    }

    #[tokio::test]
    async fn open_stream_forwards_range_and_media_source_id() {
        stream_forwards_range::<Jellyfin>("stream-range-jellyfin").await;
        stream_forwards_range::<Emby>("stream-range-emby").await;
    }

    #[tokio::test]
    async fn open_stream_under_path_prefix() {
        let state = Arc::new(MockServer::new("jellyfin", "secret").with_prefix("/media/pre"));
        let (base_url, server) = serve(&state).await;
        let client = client::<Jellyfin>("stream-prefix", &format!("{base_url}/media/pre/"), &token_settings("secret"));
        let stream_ref = stream_ref_for::<Jellyfin>("stream-prefix", SERVER_ID, "movie-1", None);

        let response = client.open_stream(&stream_ref, None).await.expect("stream opens under the prefix");

        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(state.log().last().map(String::as_str), Some("GET /media/pre/Videos/movie-1/stream?static=true"));
        server.abort();
    }

    #[tokio::test]
    async fn open_stream_rejects_foreign_input_server_and_flavor() {
        let state = Arc::new(MockServer::new("emby", "secret"));
        let (base_url, server) = serve(&state).await;
        let client = client::<Emby>("stream-foreign", &base_url, &token_settings("secret"));
        let kind_of =
            |result: Result<MediaServerStreamResponse, MediaServerError>| result.expect_err("stream is rejected").kind;

        assert_eq!(
            kind_of(client.open_stream(&stream_ref_for::<Emby>("other-input", SERVER_ID, "item", None), None).await),
            MediaServerErrorKind::MediaServerItemNotFound
        );
        assert_eq!(
            kind_of(
                client.open_stream(&stream_ref_for::<Emby>("stream-foreign", "other-server", "item", None), None).await
            ),
            MediaServerErrorKind::MediaServerItemNotFound
        );
        assert_eq!(
            kind_of(client.open_stream(&stream_ref_for::<Emby>("stream-foreign", SERVER_ID, "  ", None), None).await),
            MediaServerErrorKind::NoDirectPlayableMediaServerSource
        );
        assert_eq!(
            kind_of(
                client.open_stream(&stream_ref_for::<Jellyfin>("stream-foreign", SERVER_ID, "item", None), None).await
            ),
            MediaServerErrorKind::MediaServerStreamOpenFailed
        );
        assert_eq!(
            kind_of(
                client
                    .open_stream(
                        &stream_ref_for::<Emby>("stream-foreign", SERVER_ID, "item", None),
                        Some("bytes=0-\n1")
                    )
                    .await
            ),
            MediaServerErrorKind::MediaServerStreamOpenFailed
        );
        assert!(
            state.log().iter().all(|entry| !entry.contains("/Videos/")),
            "no rejected ref reaches the stream endpoint"
        );
        server.abort();
    }

    #[tokio::test]
    async fn open_stream_relogs_once_on_expired_login_token() {
        let state = Arc::new(MockServer::new("jellyfin", "unused"));
        let (base_url, server) = serve(&state).await;
        let client = client::<Jellyfin>("stream-relogin", &base_url, &login_settings());
        let stream_ref = stream_ref_for::<Jellyfin>("stream-relogin", SERVER_ID, "movie-1", None);
        client.open_stream(&stream_ref, None).await.expect("first stream opens");
        assert_eq!(state.logins(), 1);

        state.set_valid_token("rotated-by-server");
        let response = client.open_stream(&stream_ref, Some("bytes=5-8")).await.expect("re-login heals the stream");

        assert_eq!(response.status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(state.logins(), 2);
        assert_eq!(state.last_range.lock().expect("lock").as_deref(), Some("bytes=5-8"), "Range survives the retry");
        server.abort();
    }

    #[tokio::test]
    async fn stream_errors_never_contain_the_token() {
        for (provider, status_token) in [("jellyfin", "secret-stream-token"), ("emby", "secret-stream-token")] {
            let state = Arc::new(MockServer::new(provider, "server-side-token"));
            let (base_url, server) = serve(&state).await;
            let name = format!("stream-denied-{provider}");
            let settings = token_settings(status_token);
            let error = if provider == "jellyfin" {
                let client = client::<Jellyfin>(&name, &base_url, &settings);
                client.open_stream(&stream_ref_for::<Jellyfin>(&name, SERVER_ID, "movie-1", None), None).await
            } else {
                let client = client::<Emby>(&name, &base_url, &settings);
                client.open_stream(&stream_ref_for::<Emby>(&name, SERVER_ID, "movie-1", None), None).await
            }
            .expect_err("configured token is rejected");

            assert_eq!(error.kind, MediaServerErrorKind::MediaServerAuthDenied);
            assert!(!error.to_string().contains(status_token), "{error}");
            assert!(!format!("{error:?}").contains(status_token));
            server.abort();
        }
    }

    async fn image_with_tag<F: EmbyFamilyFlavor>(input_name: &str) {
        let state = Arc::new(MockServer::new(F::PROVIDER, "secret"));
        let (base_url, server) = serve(&state).await;
        let client = client::<F>(input_name, &base_url, &token_settings("secret"));

        let tagged = image_ref_for::<F>(input_name, SERVER_ID, ITEM_ID, "Primary", Some("image tag"));
        let response = client.open_image(&tagged).await.expect("image opens");

        assert_eq!(response.status, StatusCode::OK);
        let chunks = response.body.collect::<Vec<_>>().await;
        let body: Vec<u8> = chunks.into_iter().flat_map(|chunk| chunk.expect("image chunk").to_vec()).collect();
        assert_eq!(body, b"image-bytes");
        assert_eq!(
            response.headers.get(reqwest::header::CONTENT_TYPE).and_then(|value| value.to_str().ok()),
            Some("image/jpeg")
        );
        assert_eq!(state.log().last().map(String::as_str), Some("GET /Items/movie%201/Images/Primary?tag=image+tag"));

        let untagged = image_ref_for::<F>(input_name, SERVER_ID, ITEM_ID, "Backdrop", None);
        client.open_image(&untagged).await.expect("untagged image opens");
        assert_eq!(state.log().last().map(String::as_str), Some("GET /Items/movie%201/Images/Backdrop"));
        assert_eq!(state.legacy_auth_violations.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[tokio::test]
    async fn open_image_requests_primary_with_tag() {
        image_with_tag::<Jellyfin>("image-jellyfin").await;
        image_with_tag::<Emby>("image-emby").await;
    }

    #[tokio::test]
    async fn open_image_rejects_unknown_kind() {
        let state = Arc::new(MockServer::new("emby", "secret"));
        let (base_url, server) = serve(&state).await;
        let client = client::<Emby>("image-kind", &base_url, &token_settings("secret"));

        for kind in ["primary", "Chapter", "Primary/../Users", "", "Primary?x=1"] {
            let image_ref = image_ref_for::<Emby>("image-kind", SERVER_ID, "movie-1", kind, None);

            let error = client.open_image(&image_ref).await.expect_err("kind is not served");

            assert_eq!(error.kind, MediaServerErrorKind::MediaServerItemNotFound, "kind {kind:?}");
        }
        for (input, server_id, item) in
            [("other", SERVER_ID, "movie-1"), ("image-kind", "other-server", "movie-1"), ("image-kind", SERVER_ID, " ")]
        {
            let image_ref = image_ref_for::<Emby>(input, server_id, item, "Primary", None);

            let error = client.open_image(&image_ref).await.expect_err("ref is rejected");

            assert_eq!(error.kind, MediaServerErrorKind::MediaServerItemNotFound);
        }
        let foreign = image_ref_for::<Jellyfin>("image-kind", SERVER_ID, "movie-1", "Primary", None);
        assert_eq!(
            client.open_image(&foreign).await.expect_err("flavor mismatch").kind,
            MediaServerErrorKind::MediaServerStreamOpenFailed
        );
        assert!(state.log().iter().all(|entry| !entry.contains("/Images/")));
        server.abort();
    }

    #[tokio::test]
    async fn proxied_responses_do_not_carry_credential_headers() {
        use crate::{media_server_image_response, media_server_stream_response};

        for provider in ["jellyfin", "emby"] {
            let state = Arc::new(MockServer::new(provider, "secret"));
            let (base_url, server) = serve(&state).await;
            let name = format!("proxy-{provider}");
            let (stream_headers, status, image_headers) = if provider == "jellyfin" {
                let client = client::<Jellyfin>(&name, &base_url, &token_settings("secret"));
                let stream_ref = stream_ref_for::<Jellyfin>(&name, SERVER_ID, "movie-1", None);
                let stream =
                    media_server_stream_response(&client, &stream_ref, Some("bytes=5-8")).await.expect("proxy");
                let image_ref = image_ref_for::<Jellyfin>(&name, SERVER_ID, "movie-1", "Primary", None);
                let image = media_server_image_response(&client, &image_ref).await.expect("image proxy");
                (stream.headers, stream.status, image.headers)
            } else {
                let client = client::<Emby>(&name, &base_url, &token_settings("secret"));
                let stream_ref = stream_ref_for::<Emby>(&name, SERVER_ID, "movie-1", None);
                let stream =
                    media_server_stream_response(&client, &stream_ref, Some("bytes=5-8")).await.expect("proxy");
                let image_ref = image_ref_for::<Emby>(&name, SERVER_ID, "movie-1", "Primary", None);
                let image = media_server_image_response(&client, &image_ref).await.expect("image proxy");
                (stream.headers, stream.status, image.headers)
            };

            assert_eq!(status, StatusCode::PARTIAL_CONTENT);
            assert!(stream_headers.contains_key(reqwest::header::CONTENT_RANGE));
            for headers in [&stream_headers, &image_headers] {
                for name in headers.keys() {
                    let name = name.as_str();
                    assert!(
                        name != "authorization" && !name.starts_with("x-emby-") && !name.starts_with("x-mediabrowser-"),
                        "credential header {name} leaked into the proxied response"
                    );
                }
                assert!(!headers.contains_key("set-cookie"));
            }
            server.abort();
        }
    }

    #[tokio::test]
    async fn slow_server_does_not_delay_connecting_other_inputs() {
        use tokio::sync::Notify;

        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (slow_url, slow_server) = {
            let (entered, release) = (Arc::clone(&entered), Arc::clone(&release));
            start_test_server(move |_request| {
                let (entered, release) = (Arc::clone(&entered), Arc::clone(&release));
                async move {
                    entered.notify_one();
                    release.notified().await;
                    Ok(respond(StatusCode::OK, EMBY_PUBLIC_SYSTEM_INFO_JSON))
                }
            })
            .await
        };
        let fast_state = Arc::new(MockServer::new("emby", "secret"));
        let (fast_url, fast_server) = serve(&fast_state).await;
        let slow_client = client::<Emby>("isolation-slow", &slow_url, &token_settings("secret"));
        let fast_client = client::<Emby>("isolation-fast", &fast_url, &token_settings("secret"));

        let slow_task = tokio::spawn(async move { slow_client.discover().await });
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
            .await
            .expect("slow discovery reached its server");

        let fast = tokio::time::timeout(std::time::Duration::from_secs(5), fast_client.discover())
            .await
            .expect("other input connects while the slow server is still stalled")
            .expect("fast discovery succeeds");

        assert_eq!(fast.server_id.as_ref(), SERVER_ID);
        assert!(!slow_task.is_finished(), "the slow connection is still pending");
        release.notify_one();
        let slow = slow_task.await.expect("task joins").expect("slow discovery succeeds after release");
        assert_eq!(slow.server_id.as_ref(), SERVER_ID);
        slow_server.abort();
        fast_server.abort();
    }
    #[test]
    fn endpoint_url_keeps_path_prefix_with_and_without_trailing_slash() {
        for base in ["https://host.example.invalid/jellyfin", "https://host.example.invalid/jellyfin/"] {
            let base = parse_base_url(base).expect("base parses");

            let url = endpoint_url(&base, &["System", "Info", "Public"], &[]).expect("url builds");

            assert_eq!(url.as_str(), "https://host.example.invalid/jellyfin/System/Info/Public");
        }
    }

    #[test]
    fn endpoint_url_on_bare_host_has_no_empty_segment() {
        let base = parse_base_url("http://127.0.0.1:8096").expect("base parses");

        let url = endpoint_url(&base, &["Items"], &[("ParentId", "a b&c")]).expect("url builds");

        assert_eq!(url.as_str(), "http://127.0.0.1:8096/Items?ParentId=a+b%26c");
    }

    #[test]
    fn endpoint_url_encodes_segments_and_ignores_traversal() {
        let base = parse_base_url("http://127.0.0.1:8096/emby/").expect("base parses");

        let url = endpoint_url(&base, &["Users", "a/b?c", "..", "Items"], &[]).expect("url builds");

        assert_eq!(url.as_str(), "http://127.0.0.1:8096/emby/Users/a%2Fb%3Fc/Items");
    }

    #[test]
    fn base_url_drops_query_and_fragment_and_rejects_non_http() {
        let base = parse_base_url("http://host.example.invalid/emby?api_key=secret#frag").expect("base parses");

        assert_eq!(base.as_str(), "http://host.example.invalid/emby");
        for invalid in ["", "   ", "not a url", "ftp://host.example.invalid", "mailto:user@host.example.invalid"] {
            assert!(parse_base_url(invalid).is_none(), "{invalid:?}");
        }
    }

    #[test]
    fn new_rejects_invalid_base_url_as_discovery_failed() {
        let result = EmbyFamilyCatalogClient::<Emby>::new(Arc::from("x"), "", &token_settings("t"), http());

        assert_eq!(result.err().map(|error| error.kind), Some(MediaServerErrorKind::MediaServerDiscoveryFailed));
    }

    #[test]
    fn cache_key_holds_no_plaintext_secrets_and_changes_with_credentials() {
        let settings = EmbyClientSettings {
            token: Some("plain-token".to_string()),
            api_key: Some("plain-api-key".to_string()),
            password: Some("plain-password".to_string()),
            username: Some("alice".to_string()),
            ..EmbyClientSettings::default()
        };
        let first = client::<Emby>("key", "http://127.0.0.1:1", &settings);
        let other =
            client::<Emby>("key", "http://127.0.0.1:1", &EmbyClientSettings { token: None, ..settings.clone() });
        let key = |client: &EmbyFamilyCatalogClient<Emby>| EmbyConnectionCacheKey::new(Emby::KIND, &client.config);
        let debug = format!("{:?}", key(&first));

        assert!(!debug.contains("plain-"), "{debug}");
        assert_ne!(key(&first), key(&other));
        assert_eq!(key(&first), key(&client::<Emby>("key", "http://127.0.0.1:1", &settings)));
    }

    #[tokio::test]
    async fn idle_empty_connection_cells_are_pruned_but_sessions_and_held_cells_stay() {
        let key = |name: &str| EmbyConnectionCacheKey {
            kind: crate::MediaServerKind::Jellyfin,
            input_name: Arc::from(name),
            base_url: Arc::from("http://127.0.0.1:1/"),
            token: None,
            api_key: None,
            user_id: None,
            username: None,
            password: None,
        };
        let held = shared_connection(&key("prune-held"));
        drop(shared_connection(&key("prune-idle-empty")));
        let with_session = shared_connection(&key("prune-idle-session"));
        *with_session.lock().await = Some(EmbyConnectionState {
            status: MediaServerStatus {
                kind: crate::MediaServerKind::Jellyfin,
                server_id: Arc::from("server"),
                display_name: None,
                version: None,
                owned: None,
            },
            token: Arc::from("token"),
            user_id: None,
            origin: TokenOrigin::Configured,
        });
        drop(with_session);

        drop(shared_connection(&key("prune-trigger")));

        let cache = EMBY_CONNECTION_CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(cache.contains_key(&key("prune-held")));
        assert!(!cache.contains_key(&key("prune-idle-empty")));
        assert!(cache.contains_key(&key("prune-idle-session")));
        drop(cache);
        drop(held);
    }
}
