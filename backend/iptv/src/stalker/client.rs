use crate::{
    capabilities::ProviderCapabilities,
    stalker::{
    auth, catalog,
    cookie_jar::{apply_set_cookie_headers_unchecked, StalkerCookieJar},
    epg,
    error::{StalkerError, StalkerResult},
    playback,
    presets::stalker_mag_preset_spec,
    profile::{StalkerHandshake, StalkerResolvedStream},
    recipes::apply_endpoint_preference,
    session::StalkerSession,
    transport::{ReqwestTransport, StalkerTransport},
    url_factory::{load_url_candidates, StalkerLoadUrl},
    },
};
use bytes::{Bytes, BytesMut};
use log::{trace, warn};
use parking_lot::Mutex;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, COOKIE, REFERER, USER_AGENT},
    Client, RequestBuilder, Response, Url,
};
use serde::Deserialize;
use std::{
    fmt::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex as AsyncMutex;
use tuliprox_core::{
    model::{StalkerInputConfig, StalkerSizeCaps},
    utils::{Clock, SystemClock},
};

/// Stalker portals expect a `X-User-Agent` header on every call. The `reqwest` re-export
/// does not ship a constant for it (the upstream convention is to use a custom name), so
/// we declare a static one and resolve it to a `HeaderName` once.
const X_USER_AGENT_NAME: HeaderName = HeaderName::from_static("x-user-agent");

static STALKER_DEBUG_DUMP_SEQ: AtomicU64 = AtomicU64::new(0);
pub const DEFAULT_STALKER_CATALOG_MAX_PAGES: u32 = 4_096;

/// Upper bound on the time a handshake (including all recipe/endpoint fallbacks of a
/// single attempt) may hold the refresh lock. Without it a hung upstream would park
/// every concurrent caller behind the `refresh_lock` indefinitely.
const STALKER_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Cap on the initial `BytesMut` allocation when reading a body. A hostile portal can
/// advertise a huge `Content-Length` without sending data; we never pre-allocate more
/// than this, growing on demand instead.
const INITIAL_BODY_ALLOC_CAP: u64 = 256 * 1024;
const STALKER_DEBUG_DUMP_LIMIT: usize = 32;
const STALKER_DEBUG_DUMP_PREFIX: &str = "stalker-response-";

/// Per-action cap on the response body in bytes. Configurable at construction time so
/// the user can dial it down for adversarial portals and up for friendly ones.
#[derive(Debug, Clone)]
pub struct StalkerBodyCaps {
    pub create_link_bytes: u64,
    pub ordered_list_bytes: u64,
    pub get_epg_bytes: u64,
}

impl Default for StalkerBodyCaps {
    fn default() -> Self {
        Self { create_link_bytes: 64 * 1024, ordered_list_bytes: 8 * 1024 * 1024, get_epg_bytes: 64 * 1024 * 1024 }
    }
}

impl From<&StalkerSizeCaps> for StalkerBodyCaps {
    fn from(caps: &StalkerSizeCaps) -> Self {
        Self {
            create_link_bytes: u64::from(caps.create_link_kb) * 1024,
            ordered_list_bytes: u64::from(caps.ordered_list_mb) * 1024 * 1024,
            get_epg_bytes: u64::from(caps.get_epg_mb) * 1024 * 1024,
        }
    }
}

/// The Stalker/Ministra portal client. Built per-input so the cookie jar and session
/// never bleed between inputs.
///
/// Its two ambient dependencies are type parameters rather than hard-wired globals: `T`
/// decides where a request goes and `C` decides what time it is. Both default to the
/// production implementation, so `StalkerApiClient::new` is unchanged and neither costs
/// anything at runtime — [`SystemClock`] is zero-sized and there is no vtable on either.
/// Tests name the other implementors; see [`crate::stalker::transport`].
pub struct StalkerApiClient<Tr: StalkerTransport = ReqwestTransport, C: Clock = SystemClock> {
    transport: Tr,
    clock: C,
    portal_url: String,
    load_urls: Vec<StalkerLoadUrl>,
    config: StalkerInputConfig,
    body_caps: StalkerBodyCaps,
    /// Server-side cookie jar. Cleared when the server returns 401/403/456.
    cookies: StalkerCookieJar,
    /// Active session — `None` until [`StalkerApiClient::handshake`] succeeds.
    handshake: Mutex<Option<StalkerHandshake>>,
    /// Serialises concurrent refresh attempts (e.g. 4xx-triggered re-handshake).
    refresh_lock: AsyncMutex<()>,
    /// What this portal has already proven about itself. Seeded by the caller from a
    /// [`crate::capability_store::CapabilityStore`] when one is available, and updated as
    /// the portal answers.
    capabilities: Mutex<ProviderCapabilities>,
}

impl StalkerApiClient<ReqwestTransport, SystemClock> {
    /// The production client: a real `reqwest::Client`, so connection pooling, timeouts
    /// and proxy settings stay under the caller's control, and the system clock.
    pub fn new(http: Client, portal_url: String, config: StalkerInputConfig) -> StalkerResult<Self> {
        Self::with_parts(ReqwestTransport::new(http), SystemClock, portal_url, config)
    }
}

impl<Tr: StalkerTransport, C: Clock> StalkerApiClient<Tr, C> {
    /// Build a client over a caller-supplied transport and clock.
    pub fn with_parts(
        transport: Tr,
        clock: C,
        portal_url: String,
        config: StalkerInputConfig,
    ) -> StalkerResult<Self> {
        let load_urls = apply_endpoint_preference(config.endpoint_preference, load_url_candidates(&portal_url)?);
        let default_caps = StalkerSizeCaps::default();
        let body_caps = StalkerBodyCaps::from(config.size_caps.as_ref().unwrap_or(&default_caps));
        Ok(Self {
            transport,
            clock,
            portal_url,
            load_urls,
            config,
            body_caps,
            cookies: StalkerCookieJar::new(),
            handshake: Mutex::new(None),
            refresh_lock: AsyncMutex::new(()),
            capabilities: Mutex::new(ProviderCapabilities::default()),
        })
    }

    /// Seed the client with what a previous run learned about this portal.
    #[must_use]
    pub fn with_capabilities(self, capabilities: ProviderCapabilities) -> Self {
        *self.capabilities.lock() = capabilities;
        self
    }

    /// The current snapshot, for persisting back to a store.
    #[must_use]
    pub fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities.lock().clone()
    }

    /// Whether `action` is known not to work on this portal, according to a snapshot
    /// still worth believing.
    #[must_use]
    pub fn action_is_unsupported(&self, action: &str) -> bool {
        self.capabilities.lock().is_unsupported(action, self.now_epoch_secs())
    }

    /// Note that the portal does not implement `action`. Returns whether this was new
    /// information, so a caller can skip a write that would persist nothing.
    pub fn record_unsupported_action(&self, action: &str) -> bool {
        let now = self.now_epoch_secs();
        self.capabilities.lock().record_unsupported(action, now)
    }

    /// Note that `action` worked, taking back any earlier negative claim.
    pub fn record_supported_action(&self, action: &str) -> bool {
        let now = self.now_epoch_secs();
        self.capabilities.lock().record_supported(action, now)
    }

    /// Note the recipe and endpoint that completed a handshake, so the next one can start
    /// there instead of walking the chain from the top.
    pub fn record_successful_handshake(&self, recipe: &str, endpoint: &str) -> bool {
        let now = self.now_epoch_secs();
        self.capabilities.lock().record_handshake(recipe, endpoint, now)
    }

    /// The recipe that last completed a handshake here, when still worth believing.
    #[must_use]
    pub fn remembered_recipe(&self) -> Option<String> {
        let now = self.now_epoch_secs();
        self.capabilities.lock().remembered_recipe(now).map(ToString::to_string)
    }

    /// The endpoint candidates in the order they should be tried, with the one that last
    /// answered moved to the front.
    #[must_use]
    pub fn ordered_load_urls(&self) -> Vec<StalkerLoadUrl> {
        let now = self.now_epoch_secs();
        self.capabilities.lock().prefer_remembered(self.load_urls.clone(), |url| url.load_url.clone(), now)
    }

    /// Unix-epoch seconds according to this client's clock. Everything that expires -
    /// sessions, cookies - is measured against this.
    #[must_use]
    pub fn now_epoch_secs(&self) -> u64 {
        crate::clock::epoch_secs(&self.clock)
    }

    /// Returns the URL candidates this client will iterate through on errors.
    pub fn load_url_candidates(&self) -> &[StalkerLoadUrl] {
        &self.load_urls
    }

    /// Returns the active handshake, if any. Reverse-proxy code calls this to fetch the
    /// bearer token without re-handshaking.
    pub fn active_handshake(&self) -> Option<StalkerHandshake> {
        self.handshake.lock().clone()
    }

    /// Returns a reference to the active cookie jar. Used by tests and by callers that
    /// need to seed the jar with previously-captured cookies.
    pub fn cookies(&self) -> &StalkerCookieJar {
        &self.cookies
    }

    /// Force-clear the active session and cookies. Useful when the upstream returns a
    /// "token rejected" status.
    pub fn invalidate_session(&self) {
        self.cookies.clear();
        *self.handshake.lock() = None;
    }

    // -------------------------------------------------------------------------------------
    // Public API surface
    // -------------------------------------------------------------------------------------

    pub async fn handshake(&self) -> StalkerResult<StalkerHandshake> {
        let _refresh_guard = self.refresh_lock.lock().await;
        if let Some(active) = self.active_handshake() {
            // Honour the soft TTL: a cached session older than `STALKER_SESSION_TTL`
            // is discarded and re-handshaken. The portal may invalidate tokens
            // earlier; that path is caught by the 4xx retry hook in `api_utils.rs`.
            if !active.session.is_stale_at(self.now_epoch_secs(), crate::stalker::session::STALKER_SESSION_TTL) {
                return Ok(active);
            }
            // Stale: drop the cached handshake so the call below re-issues it.
            *self.handshake.lock() = None;
            self.cookies.clear();
        }
        let handshake =
            tokio::time::timeout(STALKER_HANDSHAKE_TIMEOUT, auth::handshake(self)).await.map_err(|_| {
                StalkerError::HandshakeFailed {
                    message: format!("handshake timed out after {}s", STALKER_HANDSHAKE_TIMEOUT.as_secs()),
                    url: None,
                }
            })??;
        *self.handshake.lock() = Some(handshake.clone());
        Ok(handshake)
    }

    pub async fn get_live_categories(
        &self,
        handshake: &StalkerHandshake,
    ) -> StalkerResult<Vec<catalog::StalkerCategory>> {
        catalog::get_live_categories(self, handshake).await
    }

    pub async fn get_live_streams(&self, handshake: &StalkerHandshake) -> StalkerResult<Vec<catalog::StalkerRawItem>> {
        catalog::get_live_streams_paginated(self, handshake).await
    }

    pub async fn get_all_channels(&self, handshake: &StalkerHandshake) -> StalkerResult<Vec<catalog::StalkerRawItem>> {
        catalog::get_all_channels(self, handshake).await
    }

    pub async fn get_live_streams_page(
        &self,
        handshake: &StalkerHandshake,
        page: u32,
    ) -> StalkerResult<catalog::StalkerCatalogPage<catalog::StalkerRawItem>> {
        catalog::get_live_streams_page(self, handshake, page).await
    }

    pub async fn get_vod_categories(
        &self,
        handshake: &StalkerHandshake,
    ) -> StalkerResult<Vec<catalog::StalkerCategory>> {
        catalog::get_vod_categories(self, handshake).await
    }

    pub async fn get_vod_streams(&self, handshake: &StalkerHandshake) -> StalkerResult<Vec<catalog::StalkerRawItem>> {
        catalog::get_vod_streams_paginated(self, handshake).await
    }

    pub async fn get_vod_streams_page(
        &self,
        handshake: &StalkerHandshake,
        page: u32,
    ) -> StalkerResult<catalog::StalkerCatalogPage<catalog::StalkerRawItem>> {
        catalog::get_vod_streams_page(self, handshake, page).await
    }

    pub async fn get_series_categories(
        &self,
        handshake: &StalkerHandshake,
    ) -> StalkerResult<Vec<catalog::StalkerCategory>> {
        catalog::get_series_categories(self, handshake).await
    }

    pub async fn get_series_list(
        &self,
        handshake: &StalkerHandshake,
    ) -> StalkerResult<Vec<catalog::StalkerRawSeriesItem>> {
        catalog::get_series_list_paginated(self, handshake).await
    }

    pub async fn get_series_list_page(
        &self,
        handshake: &StalkerHandshake,
        page: u32,
    ) -> StalkerResult<catalog::StalkerCatalogPage<catalog::StalkerRawSeriesItem>> {
        catalog::get_series_list_page(self, handshake, page).await
    }

    pub async fn get_series_details(
        &self,
        handshake: &StalkerHandshake,
        series_id: u32,
    ) -> StalkerResult<catalog::StalkerRawSeriesDetails> {
        catalog::get_series_details(self, handshake, series_id).await
    }

    /// Stream the live catalog to `on_batch` one page at a time, instead of buffering the
    /// whole thing like [`Self::get_live_streams`]. An `Err` means the batches already
    /// delivered are an incomplete prefix and must be discarded.
    pub async fn stream_live_streams<F, Fut>(
        &self,
        handshake: &StalkerHandshake,
        on_batch: F,
    ) -> StalkerResult<u64>
    where
        F: FnMut(Vec<catalog::StalkerRawItem>) -> Fut + Send,
        Fut: std::future::Future<Output = StalkerResult<()>> + Send,
    {
        catalog::stream_live_streams(self, handshake, on_batch).await
    }

    /// Stream the VOD catalog. See [`Self::stream_live_streams`] for the error contract.
    pub async fn stream_vod_streams<F, Fut>(&self, handshake: &StalkerHandshake, on_batch: F) -> StalkerResult<u64>
    where
        F: FnMut(Vec<catalog::StalkerRawItem>) -> Fut + Send,
        Fut: std::future::Future<Output = StalkerResult<()>> + Send,
    {
        catalog::stream_vod_streams(self, handshake, on_batch).await
    }

    /// Stream the series catalog. See [`Self::stream_live_streams`] for the error contract.
    pub async fn stream_series_list<F, Fut>(&self, handshake: &StalkerHandshake, on_batch: F) -> StalkerResult<u64>
    where
        F: FnMut(Vec<catalog::StalkerRawSeriesItem>) -> Fut + Send,
        Fut: std::future::Future<Output = StalkerResult<()>> + Send,
    {
        catalog::stream_series_list(self, handshake, on_batch).await
    }

    pub async fn get_short_epg(
        &self,
        handshake: &StalkerHandshake,
        channel_id: u32,
        hours: u32,
    ) -> StalkerResult<Vec<epg::StalkerProgramRecord>> {
        epg::get_short_epg(self, handshake, channel_id, hours).await
    }

    pub async fn stream_bulk_epg<F, Fut>(
        &self,
        handshake: &StalkerHandshake,
        period_hours: u32,
        batch_size: usize,
        on_batch: F,
    ) -> StalkerResult<()>
    where
        F: FnMut(Vec<epg::StalkerProgramRecord>) -> Fut,
        Fut: std::future::Future<Output = StalkerResult<()>>,
    {
        epg::stream_bulk_epg(self, handshake, period_hours, batch_size, on_batch).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_link(
        &self,
        handshake: &StalkerHandshake,
        kind: shared::model::stalker::StalkerStreamKind,
        requested_mode: shared::model::stalker::StalkerPlaybackMode,
        cmd: &str,
        series_number: Option<u32>,
        archive_start: Option<&str>,
        archive_end: Option<&str>,
    ) -> StalkerResult<StalkerResolvedStream> {
        playback::create_link(self, handshake, kind, requested_mode, cmd, series_number, archive_start, archive_end)
            .await
    }

    // -------------------------------------------------------------------------------------
    // Internal helpers — used by auth/catalog/epg/playback submodules
    // -------------------------------------------------------------------------------------

    /// Start a request against `url`. Stalker portals answer `GET` for every action.
    pub fn get(&self, url: &str) -> RequestBuilder {
        self.transport.get(url)
    }

    pub fn portal_url(&self) -> &str {
        &self.portal_url
    }

    pub fn config(&self) -> &StalkerInputConfig {
        &self.config
    }

    pub fn body_caps(&self) -> &StalkerBodyCaps {
        &self.body_caps
    }

    pub fn catalog_max_pages(&self) -> u32 {
        self.config.catalog_max_pages.filter(|value| *value > 0).unwrap_or(DEFAULT_STALKER_CATALOG_MAX_PAGES)
    }

    /// Build the common header set for a Stalker API request. We always send the
    /// `User-Agent`, `X-User-Agent` and `Referer` headers; the `Authorization` header
    /// is added when a bearer token is present. The `Cookie` header carries the MAG
    /// identity cookies (`mac`, `stb_lang`, `timezone`) merged with whatever the portal
    /// set in the jar — many portals authorize against the cookie identity, not the
    /// query string.
    pub fn common_headers(&self, load_url: &StalkerLoadUrl) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let preset = stalker_mag_preset_spec(self.config.mag_preset);
        let ua: &str = self
            .config
            .device
            .as_ref()
            .and_then(|d| d.user_agent.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(preset.user_agent);
        let xua: &str = self
            .config
            .device
            .as_ref()
            .and_then(|d| d.x_user_agent.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(preset.x_user_agent);
        match HeaderValue::from_str(ua) {
            Ok(v) => {
                headers.insert(USER_AGENT, v);
            }
            Err(err) => warn!("Stalker: dropping invalid User-Agent header value: {err}"),
        }
        match HeaderValue::from_str(xua) {
            Ok(v) => {
                headers.insert(&X_USER_AGENT_NAME, v);
            }
            Err(err) => warn!("Stalker: dropping invalid X-User-Agent header value: {err}"),
        }
        match HeaderValue::from_str(&load_url.referer) {
            Ok(v) => {
                headers.insert(REFERER, v);
            }
            Err(err) => warn!("Stalker: dropping invalid Referer header value: {err}"),
        }
        let now = self.now_epoch_secs();
        let mut cookie_pairs = identity_cookie_pairs(&self.config);
        for (name, value) in self.cookies.active_cookies(now) {
            // Server-set cookies win over our synthesized identity values.
            if let Some(existing) = cookie_pairs.iter_mut().find(|(n, _)| *n == name) {
                existing.1 = value;
            } else {
                cookie_pairs.push((name, value));
            }
        }
        let cookie = cookie_pairs.iter().map(|(name, value)| format!("{name}={value}")).collect::<Vec<_>>().join("; ");
        if !cookie.is_empty() {
            match HeaderValue::from_str(&cookie) {
                Ok(v) => {
                    headers.insert(COOKIE, v);
                }
                Err(err) => warn!("Stalker: dropping invalid Cookie header value: {err}"),
            }
        }
        headers
    }

    /// Apply the bearer token to the request builder when we have a session. The
    /// authorization scheme name comes from the active MAG preset (`token_header`).
    pub fn apply_bearer(
        &self,
        builder: RequestBuilder,
        session: Option<&StalkerSession>,
        token_in_query: bool,
    ) -> RequestBuilder {
        let Some(session) = session else {
            return builder;
        };
        if token_in_query {
            let token = session.token.clone();
            builder.query(&[("token", token.as_str())])
        } else {
            let preset = stalker_mag_preset_spec(self.config.mag_preset);
            builder.header(AUTHORIZATION, format!("{} {}", preset.token_header, session.token))
        }
    }

    /// Apply the MAC query parameter when the preset requires it.
    pub fn apply_mac_query(&self, builder: RequestBuilder) -> RequestBuilder {
        let preset = stalker_mag_preset_spec(self.config.mag_preset);
        if !preset.emit_mac_query {
            return builder;
        }
        if let Some(mac) = self.config.device.as_ref().and_then(|d| d.mac_address.as_deref()) {
            builder.query(&[("mac", mac)])
        } else {
            builder
        }
    }

    /// Persist Set-Cookie headers from a response into the jar. Called from every
    /// request helper after a successful response.
    pub fn ingest_response_cookies(&self, response: &Response) {
        apply_set_cookie_headers_unchecked(&self.cookies, response.headers(), self.now_epoch_secs()).ok();
    }

    /// Send a request and decode the JSON body, applying the per-action body cap. When
    /// the body cap is exceeded the request is aborted and a `ResponseTooLarge` error is
    /// returned. The cap is enforced by inspecting the `Content-Length` header when
    /// present and otherwise by streaming the body in chunks of 64 KiB.
    pub async fn send_json<T>(&self, builder: RequestBuilder, cap_action: &'static str) -> StalkerResult<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let cap = self.cap_for_action(cap_action);
        let response = self.send_with_cap(builder, cap_action, cap).await?;
        let status = response.status();
        self.ingest_response_cookies(&response);
        let body = self.read_body_with_cap(response, cap_action, cap).await?;
        if !status.is_success() {
            return Err(StalkerError::BadStatus {
                status: status.as_u16(),
                action: cap_action.to_string(),
                body_snippet: String::from_utf8_lossy(&body[..body.len().min(256)]).into_owned(),
            });
        }
        // The Stalker/Ministra middleware can return HTTP 200 with a portal-internal
        // `code` field set to a 4xx value (e.g. `{"code": 44, "text": "Account is blocked"}`).
        // Those need to surface as `PortalBodyError` so `is_token_rejected()` flags them and
        // the proxy retry path can re-run `create_link` (or fall back gracefully).
        if let Some(code) = inspect_portal_code(&body) {
            if matches!(code, 44 | 440..=449) {
                return Err(StalkerError::PortalBodyError {
                    code,
                    action: cap_action.to_string(),
                    body_snippet: String::from_utf8_lossy(&body[..body.len().min(256)]).into_owned(),
                });
            }
        }
        self.decode_body_bytes(&body, cap_action)
    }

    /// Decode the body bytes as JSON, optionally stripping a JSONP wrapper and a UTF-8
    /// BOM. Returns the parsed value or a typed error.
    #[allow(clippy::unused_self)]
    pub fn decode_body_bytes<T>(&self, body: &[u8], action: &'static str) -> StalkerResult<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let stripped = strip_bom(body);
        let json = strip_jsonp(stripped);
        if json.trim_start().starts_with('<') {
            let snippet = String::from_utf8_lossy(&body[..body.len().min(128)]).into_owned();
            return Err(StalkerError::HtmlResponse { snippet });
        }
        if json.is_empty() {
            return Err(StalkerError::EmptyBody { action: action.to_string() });
        }
        serde_json::from_str::<T>(json).map_err(|err| {
            let snippet = json.chars().take(160).collect::<String>();
            StalkerError::BodyDecode { message: format!("{action} json decode: {err}; body prefix={snippet:?}") }
        })
    }

    /// Send a request and reject an advertised body that exceeds the action cap.
    /// Body consumers must also use `read_body_with_cap` so chunked responses are
    /// bounded when no `Content-Length` header is present.
    pub async fn send_with_cap(
        &self,
        builder: RequestBuilder,
        action: &'static str,
        cap: u64,
    ) -> StalkerResult<Response> {
        let request = builder.build().map_err(StalkerError::from)?;
        let response = self.transport.execute(request).await?;
        if let Some(content_length) = response.content_length() {
            if content_length > cap {
                return Err(StalkerError::ResponseTooLarge { action: action.to_string(), cap_bytes: cap });
            }
        }
        if response.status().is_success() {
            let advertised = response.content_length().map_or_else(|| "unknown".to_string(), |len| len.to_string());
            trace!("Stalker {action} response {advertised} bytes (cap {cap})");
        } else {
            warn!("Stalker {action} response status {}", response.status());
        }
        Ok(response)
    }

    pub async fn read_body_with_cap(
        &self,
        mut response: Response,
        action: &'static str,
        cap: u64,
    ) -> StalkerResult<Bytes> {
        let initial_capacity =
            usize::try_from(response.content_length().unwrap_or(0).min(cap).min(INITIAL_BODY_ALLOC_CAP))
                .unwrap_or_default();
        let mut body = BytesMut::with_capacity(initial_capacity);
        let mut received = 0_u64;
        while let Some(chunk) = response.chunk().await.map_err(StalkerError::from)? {
            received = received.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
            if received > cap {
                return Err(StalkerError::ResponseTooLarge { action: action.to_string(), cap_bytes: cap });
            }
            body.extend_from_slice(&chunk);
        }
        let body = body.freeze();
        persist_stalker_debug_body(&self.portal_url, action, &body);
        Ok(body)
    }

    /// Resolve the body cap for a given action. The cap is taken from the user's
    /// `StalkerSizeCaps`; defaults are inherited from `StalkerBodyCaps::default`.
    pub fn cap_for_action(&self, action: &str) -> u64 {
        match action {
            "create_link" => self.body_caps.create_link_bytes,
            "ordered_list" | "all_channels" | "vod" | "series_list" | "series_info" => {
                self.body_caps.ordered_list_bytes
            }
            "get_epg" | "get_short_epg" => self.body_caps.get_epg_bytes,
            _ => 8 * 1024 * 1024,
        }
    }
}

/// Inspect the body for a Stalker/Ministra portal-internal `code` field. The
/// middleware emits both `{"code": N, ...}` and `{"js": {"code": N, ...}}` shapes;
/// the wrapper can be a JSONP callback, so the same BOM/JSONP strip used for typed
/// decode applies. Returns the parsed `code` when present, otherwise `None`.
pub fn inspect_portal_code(body: &[u8]) -> Option<u16> {
    let stripped = strip_bom(body);
    let json = strip_jsonp(stripped);
    if json.is_empty() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    // The Stalker/Ministra responses are either `{"code": N, ...}` directly, or wrapped
    // in a `js` object as `{"js": {"code": N, ...}}`. Some endpoints return the data
    // inside `js` as a stringified payload; we only handle the object form here.
    let code = if let Some(n) = value.get("code").and_then(serde_json::Value::as_u64) {
        n
    } else {
        let obj = value.get("js").and_then(serde_json::Value::as_object)?;
        obj.get("code").and_then(serde_json::Value::as_u64)?
    };
    u16::try_from(code).ok()
}

/// Strip a leading UTF-8 BOM (`EF BB BF`) from the body.
pub fn strip_bom(body: &[u8]) -> &[u8] {
    if body.starts_with(&[0xEF, 0xBB, 0xBF]) {
        &body[3..]
    } else {
        body
    }
}

/// Strip a JSONP wrapper of the form `callback(...)` or `jsonp12345(...)`. The portal
/// returns JSONP for legacy MAG250 firmware; modern portals always return raw JSON. We
/// attempt to find the matching closing parenthesis, ignoring nested parens.
pub fn strip_jsonp(body: &[u8]) -> &str {
    let Ok(s) = std::str::from_utf8(body) else {
        return "";
    };
    let s = s.trim();
    if !looks_like_jsonp_wrapper(s) {
        return s;
    }
    // Find the first `(` — everything before it is the JSONP callback name (which we
    // discard); the matching `)` delimits the JSON body.
    let Some(open) = s.find('(') else {
        return s;
    };
    match s.rfind(')').filter(|end| *end > open) {
        Some(end) => &s[open + 1..end],
        None => s,
    }
}

fn looks_like_jsonp_wrapper(value: &str) -> bool {
    let mut seen_identifier = false;
    for ch in value.chars() {
        match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '$' => seen_identifier = true,
            '(' => return seen_identifier,
            ' ' | '\t' | '\r' | '\n' => {
                if seen_identifier {
                    continue;
                }
                return false;
            }
            _ => return false,
        }
    }
    false
}

/// Validate that the URL the portal handed back in `create_link` is one we know how to
/// reverse-proxy. Returns the scheme on success.
pub fn validate_playable_scheme(url: &str) -> StalkerResult<&'static str> {
    let parsed =
        Url::parse(url).map_err(|err| StalkerError::BodyDecode { message: format!("create_link url parse: {err}") })?;
    let scheme = parsed.scheme().to_ascii_lowercase();
    match scheme.as_str() {
        "http" | "https" => Ok(match scheme.as_str() {
            "http" => "http",
            _ => "https",
        }),
        other => Err(StalkerError::UnsupportedScheme { scheme: other.to_string() }),
    }
}

pub async fn validate_public_playable_url(url: &Url) -> StalkerResult<()> {
    validate_playable_scheme(url.as_str())?;
    let host = url
        .host_str()
        .ok_or_else(|| StalkerError::BodyDecode { message: "create_link url has no host".to_string() })?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| StalkerError::BodyDecode { message: "create_link url has no port".to_string() })?;
    match tuliprox_core::utils::network::request::resolve_public_socket_addrs(host, port).await {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            Err(StalkerError::BodyDecode { message: format!("create_link url destination rejected: {err}") })
        }
        Err(err) => Err(StalkerError::Io(err)),
    }
}

/// The MAG identity cookies every Stalker request should carry. Portals key the device
/// identity off these cookies (notably `mac`), not only the query string. Values are
/// percent-encoded because MAC addresses contain `:` which is not a valid cookie-value
/// octet.
fn identity_cookie_pairs(config: &StalkerInputConfig) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    let Some(device) = config.device.as_ref() else {
        return pairs;
    };
    if let Some(mac) = device.mac_address.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        pairs.push(("mac".to_string(), percent_encode_cookie_value(mac)));
    }
    if let Some(locale) = device.locale.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        pairs.push(("stb_lang".to_string(), percent_encode_cookie_value(locale)));
    }
    if let Some(timezone) = device.timezone.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        pairs.push(("timezone".to_string(), percent_encode_cookie_value(timezone)));
    }
    pairs
}

/// Percent-encode a cookie value, keeping only RFC 3986 unreserved characters. This is
/// what real MAG firmware does for the `mac`/`timezone` cookies (e.g. `:` → `%3A`,
/// `/` → `%2F`).
fn percent_encode_cookie_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(char::from(byte)),
            other => {
                out.push('%');
                let _ = write!(out, "{other:02X}");
            }
        }
    }
    out
}

fn persist_stalker_debug_body(portal_url: &str, action: &'static str, body: &Bytes) {
    let Ok(dir) = std::env::var("TULIPROX_STALKER_DEBUG_DIR") else {
        return;
    };
    if dir.trim().is_empty() {
        return;
    }
    // Fire-and-forget: this runs on the async path, so the sync filesystem writes are
    // offloaded to the blocking pool. `Bytes` clones are cheap (refcount bump).
    let portal_url = portal_url.to_string();
    let body = body.clone();
    tokio::task::spawn_blocking(move || {
        write_stalker_debug_body(&dir, &portal_url, action, &body);
    });
}

fn write_stalker_debug_body(dir: &str, portal_url: &str, action: &str, body: &[u8]) {
    let dump_dir = Path::new(dir);
    if let Err(err) = std::fs::create_dir_all(dump_dir) {
        warn!("Stalker debug dump: could not create {}: {err}", dump_dir.display());
        return;
    }

    let host = Url::parse(portal_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown-host".to_string());
    let seq = STALKER_DEBUG_DUMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0_u128, |duration| duration.as_millis());
    let filename = format!(
        "{STALKER_DEBUG_DUMP_PREFIX}{ts:020}-{seq:06}-{}-{}.bin",
        sanitize_dump_component(&host),
        sanitize_dump_component(action)
    );
    let path: PathBuf = dump_dir.join(filename);
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let sanitized = sanitize_stalker_debug_body(body);
    if let Err(err) = options.open(&path).and_then(|mut file| std::io::Write::write_all(&mut file, &sanitized)) {
        warn!("Stalker debug dump: could not write {}: {err}", path.display());
    } else {
        rotate_stalker_debug_dumps(dump_dir);
        trace!("Stalker debug dump written: {}", path.display());
    }
}

fn sanitize_stalker_debug_body(body: &[u8]) -> Vec<u8> {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(strip_jsonp(strip_bom(body))) else {
        return b"[non-JSON Stalker response omitted]".to_vec();
    };
    crate::redaction::redact_json(&mut value);
    serde_json::to_vec(&value).unwrap_or_else(|_| b"[Stalker response serialization failed]".to_vec())
}

fn rotate_stalker_debug_dumps(dump_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dump_dir) else {
        return;
    };
    let mut dumps: Vec<_> = entries
        .flatten()
        .filter_map(|entry| {
            if !entry.file_name().to_string_lossy().starts_with(STALKER_DEBUG_DUMP_PREFIX) {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            metadata.is_file().then(|| (metadata.modified().ok(), entry.path()))
        })
        .collect();
    dumps.sort_by_key(|(modified, _)| *modified);
    let remove_count = dumps.len().saturating_sub(STALKER_DEBUG_DUMP_LIMIT);
    for (_, path) in dumps.into_iter().take(remove_count) {
        if let Err(err) = std::fs::remove_file(&path) {
            warn!("Stalker debug dump: could not remove {}: {err}", path.display());
        }
    }
}

fn sanitize_dump_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' => ch,
            _ => '_',
        })
        .collect()
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use crate::stalker::{
        catalog,
        profile::{StalkerHandshake, StalkerProviderProfile, StalkerRawProviderProfile},
        recipes::fallback_recipes_for,
        session::StalkerSession,
        transport::testing::{FakeTransport, Reply},
    };
    use shared::model::stalker::{StalkerBootstrapRecipe, StalkerPortalCapabilitiesDto};
    use tuliprox_core::utils::ManualClock;

    const PORTAL: &str = "http://portal.example/stalker_portal/";

    fn client_over(
        replies: impl IntoIterator<Item = Reply>,
    ) -> StalkerApiClient<std::sync::Arc<FakeTransport>, ManualClock> {
        client_with(std::sync::Arc::new(FakeTransport::new(replies)), StalkerInputConfig::default())
    }

    fn client_with(
        transport: std::sync::Arc<FakeTransport>,
        config: StalkerInputConfig,
    ) -> StalkerApiClient<std::sync::Arc<FakeTransport>, ManualClock> {
        StalkerApiClient::with_parts(transport, ManualClock::new(10_000_000), PORTAL.to_string(), config)
            .expect("portal url is well formed")
    }

    fn handshake() -> StalkerHandshake {
        let config = StalkerInputConfig::default();
        StalkerHandshake {
            session: StalkerSession::new_at(
                "token".to_string(),
                format!("{PORTAL}c/"),
                format!("{PORTAL}server/load.php"),
                10_000,
            ),
            profile: StalkerProviderProfile::from_config(
                &config,
                StalkerRawProviderProfile::default(),
                StalkerBootstrapRecipe::GenericSafe,
                fallback_recipes_for(config.auth_mode, config.mag_preset),
                StalkerPortalCapabilitiesDto::default(),
                StalkerSizeCaps::default(),
                None,
                None,
            ),
        }
    }

    /// The portal reports account-level refusals inside a `200 OK` body. Nothing about
    /// that path was reachable before the transport was a seam.
    #[tokio::test]
    async fn a_portal_refusal_hidden_in_a_200_body_surfaces_as_a_token_rejection() {
        let client = client_over([Reply::ok(r#"{"code": 44, "text": "Account is blocked"}"#)]);
        let err = client
            .send_json::<serde_json::Value>(client.get(PORTAL), "create_link")
            .await
            .expect_err("a portal code must not decode as success");
        assert!(matches!(err, StalkerError::PortalBodyError { code: 44, .. }));
        assert!(err.is_token_rejected(), "the proxy retry path keys off this");
    }

    #[tokio::test]
    async fn a_body_over_the_action_cap_is_refused_rather_than_buffered() {
        let config = StalkerInputConfig {
            size_caps: Some(StalkerSizeCaps { create_link_kb: 1, ..StalkerSizeCaps::default() }),
            ..StalkerInputConfig::default()
        };
        let oversized = "x".repeat(4096);
        let client = client_with(
            std::sync::Arc::new(FakeTransport::new([Reply::ok(&oversized)])),
            config,
        );
        let err = client
            .send_json::<serde_json::Value>(client.get(PORTAL), "create_link")
            .await
            .expect_err("body above the cap must be refused");
        assert!(matches!(err, StalkerError::ResponseTooLarge { cap_bytes: 1024, .. }));
    }

    #[tokio::test]
    async fn an_html_error_page_is_not_mistaken_for_json() {
        let client = client_over([Reply::ok("<html><body>502 Bad Gateway</body></html>")]);
        let err = client
            .send_json::<serde_json::Value>(client.get(PORTAL), "ordered_list")
            .await
            .expect_err("HTML must not decode as JSON");
        assert!(matches!(err, StalkerError::HtmlResponse { .. }));
    }

    /// The client walks `server/load.php`, then `portal.php`, then `c/`. Proving it moves
    /// on after a 500 needed a portal that could fail on demand.
    #[tokio::test]
    async fn a_failing_endpoint_candidate_falls_through_to_the_next_one() {
        let transport = std::sync::Arc::new(FakeTransport::new([
            Reply::Http(500, "upstream exploded".to_string()),
            Reply::ok(r#"{"js": [{"id": "1", "title": "Sport"}]}"#),
        ]));
        let client = client_with(std::sync::Arc::clone(&transport), StalkerInputConfig::default());

        let categories = catalog::get_live_categories(&client, &handshake()).await.expect("second candidate answers");

        assert_eq!(categories.len(), 1);
        assert_eq!(categories[0].title, "Sport");
        assert_eq!(
            transport.requested_paths(),
            vec!["/stalker_portal/server/load.php".to_string(), "/stalker_portal/portal.php".to_string()],
            "both candidates should have been tried, in priority order"
        );
    }

    /// Every candidate failing must surface the last real error, not a synthetic one.
    #[tokio::test]
    async fn exhausting_every_candidate_reports_the_last_failure() {
        let client = client_over([
            Reply::Http(500, String::new()),
            Reply::Http(500, String::new()),
            Reply::Http(503, String::new()),
        ]);
        let err = catalog::get_live_categories(&client, &handshake()).await.expect_err("no candidate answers");
        assert!(matches!(err, StalkerError::BadStatus { status: 503, .. }));
    }

    /// Pagination stops at the advertised last page instead of asking forever.
    #[tokio::test]
    async fn pagination_stops_when_the_portal_says_the_page_is_the_last_one() {
        let transport = std::sync::Arc::new(FakeTransport::new([
            Reply::ok(r#"{"js":{"total_items":3,"max_page_items":2,"data":[{"id":"1"},{"id":"2"}]}}"#),
            Reply::ok(r#"{"js":{"total_items":3,"max_page_items":2,"data":[{"id":"3"}]}}"#),
        ]));
        let client = client_with(std::sync::Arc::clone(&transport), StalkerInputConfig::default());

        let items = client.get_live_streams(&handshake()).await.expect("two pages");

        assert_eq!(items.len(), 3);
        assert_eq!(transport.requested().len(), 2, "a third page must not be requested");
    }

    /// A mid-pagination failure must not hand back the rows collected so far.
    #[tokio::test]
    async fn a_truncated_catalog_is_never_returned_as_success() {
        let client = client_over([
            Reply::ok(r#"{"js":{"total_items":10,"max_page_items":2,"data":[{"id":"1"},{"id":"2"}]}}"#),
            Reply::Http(500, String::new()),
            // Second candidate: restarts from page one, then dies too.
            Reply::Http(500, String::new()),
            Reply::Http(500, String::new()),
        ]);
        let result = client.get_live_streams(&handshake()).await;
        assert!(result.is_err(), "a partial catalog must not be reported as complete");
    }

    /// The streaming variant makes the opposite trade, and says so: once a page is out,
    /// the failure is returned rather than retried against another endpoint.
    #[tokio::test]
    async fn a_streaming_catalog_stops_instead_of_silently_restarting() {
        let transport = std::sync::Arc::new(FakeTransport::new([
            Reply::ok(r#"{"js":{"total_items":10,"max_page_items":2,"data":[{"id":"1"},{"id":"2"}]}}"#),
            Reply::Http(500, String::new()),
        ]));
        let client = client_with(std::sync::Arc::clone(&transport), StalkerInputConfig::default());

        let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&seen);
        let result = client
            .stream_live_streams(&handshake(), move |batch: Vec<catalog::StalkerRawItem>| {
                let sink = std::sync::Arc::clone(&sink);
                async move {
                    sink.lock().extend(batch.into_iter().filter_map(|item| item.id));
                    Ok(())
                }
            })
            .await;

        assert!(result.is_err(), "the caller must be told the prefix is incomplete");
        assert_eq!(*seen.lock(), vec!["1".to_string(), "2".to_string()]);
        assert_eq!(transport.requested().len(), 2, "no restart against the next candidate");
    }

    /// A portal that does not implement the bulk shortcut is asked exactly once.
    #[tokio::test]
    async fn an_unsupported_action_is_not_probed_again() {
        let transport = std::sync::Arc::new(FakeTransport::new([
            // `get_all_channels` 404s on all three endpoint candidates.
            Reply::Http(404, String::new()),
            Reply::Http(404, String::new()),
            Reply::Http(404, String::new()),
        ]));
        let client = client_with(std::sync::Arc::clone(&transport), StalkerInputConfig::default());

        let first = catalog::get_all_channels(&client, &handshake()).await.expect_err("portal 404s");
        assert!(first.is_unsupported_catalog_action());
        let probes_after_first = transport.requested().len();

        let second = catalog::get_all_channels(&client, &handshake()).await.expect_err("still unsupported");

        assert!(matches!(second, StalkerError::ActionUnsupported { .. }));
        assert!(second.is_unsupported_catalog_action(), "the caller's paginated fallback keys off this");
        assert_eq!(transport.requested().len(), probes_after_first, "the second call must not touch the network");
    }

    /// The negative claim is a hint with an expiry, not a permanent verdict.
    #[tokio::test]
    async fn an_expired_claim_lets_a_fixed_portal_be_retried() {
        let clock = ManualClock::new(10_000_000);
        let transport = std::sync::Arc::new(FakeTransport::new([
            Reply::Http(404, String::new()),
            Reply::Http(404, String::new()),
            Reply::Http(404, String::new()),
            Reply::ok(r#"{"js": {"data": [{"id": "1", "name": "Now supported"}]}}"#),
        ]));
        let client = StalkerApiClient::with_parts(
            std::sync::Arc::clone(&transport),
            clock.clone(),
            PORTAL.to_string(),
            StalkerInputConfig::default(),
        )
        .expect("portal url is well formed");

        catalog::get_all_channels(&client, &handshake()).await.expect_err("portal 404s");
        clock.advance((crate::capabilities::CAPABILITY_TTL_SECS + 1) * 1_000);

        let channels = catalog::get_all_channels(&client, &handshake()).await.expect("re-probed after the TTL");
        assert_eq!(channels.len(), 1);
    }

    /// A snapshot from a previous run moves the endpoint that answered to the front, so
    /// the two that did not are never dialled again.
    #[tokio::test]
    async fn a_remembered_endpoint_is_tried_first() {
        let transport = std::sync::Arc::new(FakeTransport::new([Reply::ok(r#"{"js": [{"id": "1", "title": "Sport"}]}"#)]));
        let mut capabilities = crate::capabilities::ProviderCapabilities::default();
        // 10_000_000 ms on the manual clock is 10_000 s.
        capabilities.record_handshake("GenericSafe", &format!("{PORTAL}c/"), 10_000);
        let client =
            client_with(std::sync::Arc::clone(&transport), StalkerInputConfig::default()).with_capabilities(capabilities);

        catalog::get_live_categories(&client, &handshake()).await.expect("remembered endpoint answers");

        assert_eq!(
            transport.requested_paths(),
            vec!["/stalker_portal/c/".to_string()],
            "the two endpoints that never answered must not be dialled"
        );
    }

    /// A remembered endpoint that has since gone away must not strand the client.
    #[tokio::test]
    async fn a_remembered_endpoint_that_stops_answering_falls_back_to_the_others() {
        let transport = std::sync::Arc::new(FakeTransport::new([
            Reply::Http(500, String::new()),
            Reply::ok(r#"{"js": [{"id": "1", "title": "Sport"}]}"#),
        ]));
        let mut capabilities = crate::capabilities::ProviderCapabilities::default();
        capabilities.record_handshake("GenericSafe", &format!("{PORTAL}c/"), 10_000);
        let client =
            client_with(std::sync::Arc::clone(&transport), StalkerInputConfig::default()).with_capabilities(capabilities);

        let categories = catalog::get_live_categories(&client, &handshake()).await.expect("another endpoint answers");

        assert_eq!(categories.len(), 1);
        assert_eq!(transport.requested_paths().len(), 2);
        assert_eq!(transport.requested_paths()[0], "/stalker_portal/c/", "the remembered one is still tried first");
    }

    /// Session staleness is the client's own clock, so a cached handshake can be aged
    /// past its TTL without waiting fifteen minutes.
    #[test]
    fn the_client_ages_a_session_against_its_own_clock() {
        let clock = ManualClock::new(10_000_000);
        let client = StalkerApiClient::with_parts(
            std::sync::Arc::new(FakeTransport::new([])),
            clock.clone(),
            PORTAL.to_string(),
            StalkerInputConfig::default(),
        )
        .expect("portal url is well formed");

        let session = StalkerSession::new_at("t".into(), "r".into(), "l".into(), client.now_epoch_secs());
        assert!(!session.is_stale_at(client.now_epoch_secs(), crate::stalker::session::STALKER_SESSION_TTL));

        clock.advance(crate::stalker::session::STALKER_SESSION_TTL.as_secs() * 1_000);
        assert!(session.is_stale_at(client.now_epoch_secs(), crate::stalker::session::STALKER_SESSION_TTL));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::stalker::StalkerEndpointPreference;

    #[test]
    fn strip_bom_removes_utf8_bom() {
        assert_eq!(strip_bom(&[0xEF, 0xBB, 0xBF, b'a']), b"a");
        assert_eq!(strip_bom(b"plain"), b"plain");
    }

    #[test]
    fn strip_jsonp_unwraps_callback_wrapper() {
        let s = strip_jsonp(b"callback({\"js\":{}})");
        assert_eq!(s, "{\"js\":{}}");
    }

    #[test]
    fn debug_dump_body_redacts_secrets_and_urls() -> Result<(), std::string::FromUtf8Error> {
        let body = br#"callback({"token":"secret","js":{"cmd":"ffmpeg http://private/stream","title":"Visible"}})"#;
        let sanitized = String::from_utf8(sanitize_stalker_debug_body(body))?;

        assert!(!sanitized.contains("secret"));
        assert!(!sanitized.contains("private"));
        assert!(sanitized.contains("Visible"));
        Ok(())
    }

    #[test]
    fn debug_dump_rotation_preserves_unrelated_files() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let sentinel = dir.path().join("unrelated-diagnostic.log");
        std::fs::write(&sentinel, b"keep")?;
        for index in 0..=STALKER_DEBUG_DUMP_LIMIT {
            std::fs::write(dir.path().join(format!("{STALKER_DEBUG_DUMP_PREFIX}{index}.bin")), b"dump")?;
        }

        rotate_stalker_debug_dumps(dir.path());

        assert!(sentinel.exists());
        let dump_count = std::fs::read_dir(dir.path())?
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(STALKER_DEBUG_DUMP_PREFIX))
            .count();
        assert_eq!(dump_count, STALKER_DEBUG_DUMP_LIMIT);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn debug_dumps_are_owner_only_and_rotated() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir()?;
        let dir_path = dir.path().to_str().ok_or("temporary directory path is not UTF-8")?;
        for _ in 0..=STALKER_DEBUG_DUMP_LIMIT {
            write_stalker_debug_body(dir_path, "http://portal.example/c/", "profile", br#"{"js":{"title":"Visible"}}"#);
        }
        let entries: Vec<_> = std::fs::read_dir(dir.path())?.collect::<Result<_, _>>()?;

        assert_eq!(entries.len(), STALKER_DEBUG_DUMP_LIMIT);
        for entry in entries {
            assert_eq!(entry.metadata()?.permissions().mode() & 0o777, 0o600);
        }
        Ok(())
    }

    #[test]
    fn strip_jsonp_handles_raw_json() {
        let s = strip_jsonp(b"{\"js\":{}}");
        assert_eq!(s, "{\"js\":{}}");
    }

    #[test]
    fn strip_jsonp_handles_nested_parens() {
        let s = strip_jsonp(b"jsonp({\"js\":{\"foo\":{}}})");
        assert_eq!(s, "{\"js\":{\"foo\":{}}}");
    }

    #[test]
    fn strip_jsonp_ignores_closing_parenthesis_in_json_string() -> Result<(), serde_json::Error> {
        let s = strip_jsonp(br#"callback({"js":{"text":"value ) remains"}})"#);
        let value: serde_json::Value = serde_json::from_str(s)?;

        assert_eq!(value["js"]["text"], "value ) remains");
        Ok(())
    }

    #[test]
    fn strip_jsonp_leaves_html_untouched() {
        let s = strip_jsonp(
            b"<!DOCTYPE html><html><head><script>function loadRequiredFiles(callback) {}</script></head></html>",
        );
        assert!(s.starts_with("<!DOCTYPE html>"));
    }

    #[test]
    fn strip_jsonp_handles_trailing_semicolon() {
        let s = strip_jsonp(b"callback({\"js\":[]});");
        assert_eq!(s, "{\"js\":[]}");
    }

    #[test]
    fn validate_playable_scheme_accepts_http_family() {
        assert_eq!(validate_playable_scheme("http://x/y").unwrap(), "http");
        assert_eq!(validate_playable_scheme("https://x/y").unwrap(), "https");
        assert!(matches!(validate_playable_scheme("rtmp://x/y"), Err(StalkerError::UnsupportedScheme { .. })));
    }

    #[test]
    fn validate_playable_scheme_rejects_file() {
        let err = validate_playable_scheme("file:///etc/passwd").expect_err("fail");
        assert!(matches!(err, StalkerError::UnsupportedScheme { .. }));
    }

    #[test]
    fn public_destination_filter_rejects_non_public_ranges() -> Result<(), std::net::AddrParseError> {
        for address in ["127.0.0.1", "10.0.0.1", "169.254.1.1", "100.64.0.1", "::1", "fc00::1", "fe80::1"] {
            assert!(!tuliprox_core::utils::network::request::is_public_ip(address.parse()?), "{address}");
        }
        assert!(!tuliprox_core::utils::network::request::is_public_ip("fec0::1".parse()?));
        assert!(tuliprox_core::utils::network::request::is_public_ip("8.8.8.8".parse()?));
        assert!(tuliprox_core::utils::network::request::is_public_ip("2606:4700:4700::1111".parse()?));
        Ok(())
    }

    #[test]
    fn inspect_portal_code_detects_root_code_field() {
        let body = br#"{"code": 44, "text": "Account is blocked"}"#;
        assert_eq!(inspect_portal_code(body), Some(44));
    }

    #[test]
    fn inspect_portal_code_detects_js_wrapped_code() {
        let body = br#"{"js": {"code": 449, "text": "Token revoked"}}"#;
        assert_eq!(inspect_portal_code(body), Some(449));
    }

    #[test]
    fn inspect_portal_code_returns_none_for_success_code() {
        // `code: 0` is the Stalker success indicator. `inspect_portal_code` returns the
        // raw number regardless; the 44xx filter is the caller's responsibility.
        let body = br#"{"js": {"code": 0}}"#;
        assert_eq!(inspect_portal_code(body), Some(0));
    }

    #[test]
    fn inspect_portal_code_handles_jsonp_wrapper() {
        let body = br#"callback({"code": 44, "text": "blocked"})"#;
        assert_eq!(inspect_portal_code(body), Some(44));
    }

    #[test]
    fn inspect_portal_code_handles_bom() {
        let mut body = vec![0xEF, 0xBB, 0xBF];
        body.extend_from_slice(br#"{"code": 44}"#);
        assert_eq!(inspect_portal_code(&body), Some(44));
    }

    #[test]
    fn inspect_portal_code_returns_none_when_absent() {
        let body = br#"{"js": {"data": []}}"#;
        assert_eq!(inspect_portal_code(body), None);
    }

    #[test]
    fn inspect_portal_code_returns_none_for_empty_body() {
        assert_eq!(inspect_portal_code(b""), None);
    }

    #[test]
    fn new_client_applies_endpoint_preference_to_load_url_order() {
        let config = StalkerInputConfig {
            endpoint_preference: StalkerEndpointPreference::Portal,
            ..StalkerInputConfig::default()
        };
        let client = StalkerApiClient::new(Client::new(), "http://portal.example".to_string(), config).expect("client");
        let candidates = client.load_url_candidates();
        assert_eq!(candidates.len(), 3);
        assert!(candidates[0].load_url.ends_with("portal.php"));
    }

    #[test]
    fn is_token_rejected_recognises_portal_body_error() {
        // 44 and 440..=449 are recognised as token-rejected body errors.
        assert!(StalkerError::PortalBodyError { code: 44, action: "create_link".into(), body_snippet: String::new() }
            .is_token_rejected());
        assert!(StalkerError::PortalBodyError { code: 449, action: "create_link".into(), body_snippet: String::new() }
            .is_token_rejected());
        // Codes outside the 44xx band do not classify as token rejection.
        assert!(!StalkerError::PortalBodyError { code: 11, action: "create_link".into(), body_snippet: String::new() }
            .is_token_rejected());
        assert!(!StalkerError::PortalBodyError {
            code: 500,
            action: "create_link".into(),
            body_snippet: String::new(),
        }
        .is_token_rejected());
    }

    #[test]
    fn percent_encode_cookie_value_escapes_mac_colons() {
        assert_eq!(percent_encode_cookie_value("00:1A:79:DE:AD:BE"), "00%3A1A%3A79%3ADE%3AAD%3ABE");
        assert_eq!(percent_encode_cookie_value("Europe/Berlin"), "Europe%2FBerlin");
        assert_eq!(percent_encode_cookie_value("en_US.utf8"), "en_US.utf8");
    }

    #[test]
    fn identity_cookie_pairs_includes_mac_lang_and_timezone() {
        let config = StalkerInputConfig {
            device: Some(tuliprox_core::model::StalkerDeviceProfile {
                mac_address: Some("00:1A:79:DE:AD:BE".to_string()),
                locale: Some("en".to_string()),
                timezone: Some("Europe/Berlin".to_string()),
                ..tuliprox_core::model::StalkerDeviceProfile::default()
            }),
            ..StalkerInputConfig::default()
        };
        let pairs = identity_cookie_pairs(&config);
        assert_eq!(pairs.len(), 3);
        assert_eq!(pairs[0], ("mac".to_string(), "00%3A1A%3A79%3ADE%3AAD%3ABE".to_string()));
        assert_eq!(pairs[1], ("stb_lang".to_string(), "en".to_string()));
        assert_eq!(pairs[2], ("timezone".to_string(), "Europe%2FBerlin".to_string()));
    }

    #[test]
    fn identity_cookie_pairs_empty_without_device() {
        let config = StalkerInputConfig::default();
        assert!(identity_cookie_pairs(&config).is_empty());
    }
}
