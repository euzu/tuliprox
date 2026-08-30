#![allow(clippy::wildcard_imports)]
use super::*;

#[derive(Clone, Debug)]
pub(super) enum HlsOriginEntryUrl {
    DirectHttp { url: String },
    ProviderFailover { url: String, provider: Arc<ConfigProvider> },
}

impl HlsOriginEntryUrl {
    pub(super) fn direct_http(url: impl Into<String>) -> Self { Self::DirectHttp { url: url.into() } }

    pub(super) fn provider_failover(url: impl Into<String>, provider: Arc<ConfigProvider>) -> Self {
        Self::ProviderFailover { url: url.into(), provider }
    }

    pub(super) fn as_str(&self) -> &str {
        match self {
            Self::DirectHttp { url } | Self::ProviderFailover { url, .. } => url,
        }
    }

    pub(super) fn url_failover_provider(&self) -> Option<Arc<ConfigProvider>> {
        match self {
            Self::DirectHttp { .. } => None,
            Self::ProviderFailover { provider, .. } => Some(Arc::clone(provider)),
        }
    }
}

pub(super) fn resolve_hls_cache_origin_entry_url(input: &ConfigInput, url: &str) -> Option<HlsCacheOriginResolution> {
    if let Some(provider) = hls_url_failover_provider_for_origin_url(input, url) {
        return Some(HlsCacheOriginResolution {
            hls_url: url.to_string(),
            session_entry_url: HlsOriginEntryUrl::provider_failover(url, provider),
        });
    }

    let parsed = Url::parse(url).ok()?;
    if matches!(parsed.scheme(), "http" | "https") {
        return Some(HlsCacheOriginResolution {
            hls_url: url.to_string(),
            session_entry_url: HlsOriginEntryUrl::direct_http(url),
        });
    }

    warn!("HLS origin entry URL is not supported: url={}", sanitize_sensitive_info(url));
    None
}

pub(super) fn hls_url_failover_provider_for_origin_url(input: &ConfigInput, url: &str) -> Option<Arc<ConfigProvider>> {
    if !url.starts_with(PROVIDER_SCHEME_PREFIX) {
        return None;
    }
    input.get_resolve_provider(url).map(|provider| Arc::clone(&provider))
}

pub(super) fn is_http_hls_origin_url(url: &str) -> bool {
    Url::parse(url).is_ok_and(|parsed| matches!(parsed.scheme(), "http" | "https"))
}

pub(super) fn is_supported_hls_origin_url(input: &ConfigInput, url: &str) -> bool {
    input.get_resolve_provider(url).is_some() || is_http_hls_origin_url(url)
}

pub(super) fn build_hls_origin_source(input: &ConfigInput, stream_ref: impl Into<String>) -> HlsOriginSource {
    HlsOriginSource::new(input.id, Arc::clone(&input.name), stream_ref, hls_origin_source_kind(input.input_type))
}

pub(super) fn build_hls_origin_source_for_playback(
    input: &ConfigInput,
    stream_ref: impl Into<String>,
    archive_reference: Option<i64>,
    archive_url: Option<&str>,
) -> HlsOriginSource {
    let source = build_hls_origin_source(input, stream_ref);
    match (archive_reference, archive_url) {
        (Some(timestamp), Some(url)) => source.with_archive_request(timestamp, url),
        (Some(timestamp), None) => source.with_archive_reference(timestamp),
        (None, _) => source,
    }
}

/// Keeps target routing identity separate from the immutable input content identity.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(in crate::api) struct HlsEntryStreamIdentity {
    pub(super) virtual_id: u32,
    pub(super) input_stream_id: Arc<str>,
    pub(super) upstream_user_agent: Option<Arc<str>>,
}

impl HlsEntryStreamIdentity {
    pub(in crate::api) fn new(virtual_id: u32, input_stream_id: impl Into<Arc<str>>) -> Option<Self> {
        let input_stream_id = input_stream_id.into();
        if input_stream_id.trim().is_empty() {
            return None;
        }
        Some(Self { virtual_id, input_stream_id, upstream_user_agent: None })
    }

    pub(in crate::api) fn from_playlist_item(item: &impl PlaylistEntry) -> Option<Self> {
        let mut identity = Self::new(item.get_virtual_id().get(), item.get_input_stream_id()?)?;
        identity.upstream_user_agent = item.get_upstream_user_agent().map(Internable::intern);
        Some(identity)
    }

    pub(in crate::api) const fn virtual_id(&self) -> u32 { self.virtual_id }

    pub(super) fn stream_ref(&self) -> &str { self.input_stream_id.as_ref() }

    pub(super) fn upstream_user_agent(&self) -> Option<&str> { self.upstream_user_agent.as_deref() }
}

/// Immutable input identity plus bitrate metadata available at the virtual HLS entry.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(in crate::api) struct HlsEntryStreamContext {
    pub(super) identity: HlsEntryStreamIdentity,
    pub(super) known_bitrate_bps: Option<u32>,
}

impl HlsEntryStreamContext {
    pub(in crate::api) fn from_playlist_item(item: &impl PlaylistEntry) -> Option<Self> {
        let identity = HlsEntryStreamIdentity::from_playlist_item(item)?;
        let known_bitrate_bps = match item.get_additional_properties() {
            Some(StreamProperties::Live(properties)) if properties.bitrate > 0 => Some(properties.bitrate),
            Some(
                StreamProperties::Live(_)
                | StreamProperties::Video(_)
                | StreamProperties::Series(_)
                | StreamProperties::Episode(_),
            )
            | None => None,
        };
        Some(Self { identity, known_bitrate_bps })
    }

    pub(in crate::api) const fn virtual_id(&self) -> u32 { self.identity.virtual_id() }

    pub(in crate::api) fn stream_ref(&self) -> &str { self.identity.stream_ref() }

    pub(in crate::api) const fn known_bitrate_bps(&self) -> Option<u32> { self.known_bitrate_bps }

    pub(in crate::api) fn identity(&self) -> &HlsEntryStreamIdentity { &self.identity }
}

/// Resolves the configured input together with both identities of one target entry.
#[derive(Debug, Clone)]
pub(in crate::api) struct HlsResolvedVirtualSource {
    pub(in crate::api) input: Arc<ConfigInput>,
    pub(in crate::api) stream_context: HlsEntryStreamContext,
}

pub(super) fn hls_origin_source_kind(input_type: InputType) -> HlsOriginSourceKind {
    if input_type.is_xtream() {
        HlsOriginSourceKind::XtreamLive
    } else if input_type.is_m3u() {
        HlsOriginSourceKind::M3uMediaPlaylist
    } else {
        HlsOriginSourceKind::DirectMediaPlaylist
    }
}

pub(super) fn build_hls_origin_resolution(
    input: &ConfigInput,
    media_playlist_url: &str,
) -> Option<HlsCacheOriginResolution> {
    let candidate = match hls_origin_source_kind(input.input_type) {
        HlsOriginSourceKind::XtreamLive => {
            ensure_hls_manifest_extension(&normalize_xtream_live_hls_url(media_playlist_url, input))
        }
        HlsOriginSourceKind::M3uMediaPlaylist | HlsOriginSourceKind::DirectMediaPlaylist => {
            ensure_hls_manifest_extension(media_playlist_url)
        }
    };
    resolve_hls_cache_origin_entry_url(input, &candidate)
}

#[derive(Clone, Copy)]
pub(super) enum HlsOriginWorkKind {
    Manifest,
    Segment,
    Resource,
}

impl HlsOriginWorkKind {
    pub(super) const fn as_log_value(self) -> &'static str {
        match self {
            Self::Manifest => "manifest",
            Self::Segment => "segment",
            Self::Resource => "resource",
        }
    }
}

pub(super) fn build_hls_origin_fetch_url(
    input: &ConfigInput,
    raw_request_url: &str,
    session_entry_url: &str,
    provider_config: Option<&Arc<RuntimeProviderConfig>>,
) -> Option<String> {
    let provider_scheme_url = [session_entry_url, raw_request_url]
        .into_iter()
        .find(|url| hls_url_failover_provider_for_origin_url(input, url).is_some());
    let url = if let (Some(provider_config), Some(provider_scheme_url)) = (provider_config, provider_scheme_url) {
        rewrite_hls_provider_scheme_origin_account(provider_scheme_url, input, provider_config)?
    } else if let Some(provider_config) = provider_config {
        get_stream_alternative_url(raw_request_url, input, provider_config)
            .or_else(|| get_stream_alternative_url(session_entry_url, input, provider_config))
            .unwrap_or_else(|| session_entry_url.to_string())
    } else {
        session_entry_url.to_string()
    };

    if is_supported_hls_origin_url(input, &url) {
        Some(url)
    } else {
        None
    }
}

pub(super) fn rewrite_hls_provider_scheme_origin_account(
    provider_scheme_url: &str,
    input: &ConfigInput,
    provider_config: &Arc<RuntimeProviderConfig>,
) -> Option<String> {
    if !provider_scheme_url.starts_with(PROVIDER_SCHEME_PREFIX) {
        return None;
    }
    let alt_input_user_info = provider_config.get_user_info()?;
    let Some((_source_base_url, source_username, source_password)) =
        input.get_matched_config_by_url(provider_scheme_url)
    else {
        return Some(provider_scheme_url.to_string());
    };
    let (Some(old_username), Some(old_password)) = (source_username, source_password) else {
        return Some(provider_scheme_url.to_string());
    };

    let mut url = Url::parse(provider_scheme_url).ok()?;
    if rewrite_hls_url_auth_fields(
        &mut url,
        old_username,
        old_password,
        &alt_input_user_info.username,
        &alt_input_user_info.password,
    ) {
        Some(url.to_string())
    } else {
        None
    }
}

pub(super) fn rewrite_hls_url_auth_fields(
    url: &mut Url,
    old_username: &str,
    old_password: &str,
    new_username: &str,
    new_password: &str,
) -> bool {
    if rewrite_hls_query_auth_fields(url, new_username, new_password) {
        return true;
    }

    if url.username() == old_username && url.password() == Some(old_password) {
        return url.set_username(new_username).is_ok() && url.set_password(Some(new_password)).is_ok();
    }

    rewrite_hls_path_auth_fields(url, old_username, old_password, new_username, new_password)
}

pub(super) fn rewrite_hls_query_auth_fields(url: &mut Url, new_username: &str, new_password: &str) -> bool {
    let mut has_username = false;
    let mut has_password = false;
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(key, value)| {
            if key.eq_ignore_ascii_case("username") {
                has_username = true;
                (key.into_owned(), new_username.to_string())
            } else if key.eq_ignore_ascii_case("password") {
                has_password = true;
                (key.into_owned(), new_password.to_string())
            } else {
                (key.into_owned(), value.into_owned())
            }
        })
        .collect();

    if !(has_username && has_password) {
        return false;
    }

    url.query_pairs_mut().clear().extend_pairs(pairs.iter().map(|(key, value)| (key.as_str(), value.as_str())));
    true
}

pub(super) fn rewrite_hls_path_auth_fields(
    url: &mut Url,
    old_username: &str,
    old_password: &str,
    new_username: &str,
    new_password: &str,
) -> bool {
    let Some(mut segments) = url.path_segments().map(|segments| segments.map(ToOwned::to_owned).collect::<Vec<_>>())
    else {
        return false;
    };

    let credential_index = if segments.len() >= 3
        && matches!(segments.first().map(String::as_str), Some("live" | "movie" | "series"))
        && segments.get(1).is_some_and(|segment| segment == old_username)
        && segments.get(2).is_some_and(|segment| segment == old_password)
    {
        Some(1)
    } else if segments.len() >= 2
        && segments.first().is_some_and(|segment| segment == old_username)
        && segments.get(1).is_some_and(|segment| segment == old_password)
    {
        Some(0)
    } else {
        None
    };

    let Some(credential_index) = credential_index else {
        return false;
    };

    segments[credential_index] = new_username.to_string();
    segments[credential_index + 1] = new_password.to_string();

    let Ok(mut path_segments) = url.path_segments_mut() else {
        return false;
    };
    path_segments.clear().extend(segments.iter().map(String::as_str));
    true
}

pub(super) fn hls_url_failover_provider_for_origin_context(
    input: &ConfigInput,
    raw_request_url: &str,
    session_entry_url: &str,
    fetch_url: &str,
) -> Option<Arc<ConfigProvider>> {
    hls_url_failover_provider_for_origin_url(input, session_entry_url)
        .or_else(|| hls_url_failover_provider_for_origin_url(input, raw_request_url))
        .or_else(|| hls_url_failover_provider_for_origin_url(input, fetch_url))
}

pub(super) struct PreparedHlsOriginRuntime {
    pub(super) fetch_url: String,
    // URL failover comes from source.yml provider:// resolution. Origin-account
    // binding/handles are runtime account reservations and must stay separate.
    pub(super) url_failover_provider: Option<Arc<ConfigProvider>>,
    pub(super) origin_account_binding_to_store: Option<HlsOriginAccountBinding>,
    pub(super) preacquired_origin_account_handle: Option<ProviderHandle>,
}

pub(super) fn effective_hls_url_failover_provider_for_fetch_url(
    fetch_url: &str,
    prepared_url_failover_provider: Option<Arc<ConfigProvider>>,
    origin_url_failover_provider: Option<Arc<ConfigProvider>>,
) -> Option<Arc<ConfigProvider>> {
    if !fetch_url.starts_with(PROVIDER_SCHEME_PREFIX) {
        return None;
    }
    prepared_url_failover_provider.or(origin_url_failover_provider)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HlsOriginRuntimeAcquireError {
    NoAccountAvailable { reason: HlsOriginRuntimeNoAccountReason },
    Fatal(StatusCode),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HlsOriginRuntimeNoAccountReason {
    ProviderConnectionsExhausted,
    OriginBindingPreempted,
}

pub(super) fn hls_no_account_reason_for_binding(
    binding: Option<&HlsOriginAccountBinding>,
) -> HlsOriginRuntimeNoAccountReason {
    let Some(binding) = binding else {
        return HlsOriginRuntimeNoAccountReason::ProviderConnectionsExhausted;
    };
    match &binding.binding_mode {
        HlsOriginAccountBindingMode::Detached {
            reason: HlsOriginAccountDetachedReason::ReclaimedByOriginalOwner,
            ..
        }
        | HlsOriginAccountBindingMode::Detached {
            reason: HlsOriginAccountDetachedReason::PreemptedByHigherPriority,
            ..
        } => HlsOriginRuntimeNoAccountReason::OriginBindingPreempted,
        HlsOriginAccountBindingMode::Detached { .. }
        | HlsOriginAccountBindingMode::Active
        | HlsOriginAccountBindingMode::Speculative { .. } => {
            HlsOriginRuntimeNoAccountReason::ProviderConnectionsExhausted
        }
    }
}

#[derive(Clone)]
pub(super) struct HlsAccountOverlapCandidate {
    pub(super) proxy_session_id: ProxySessionId,
    pub(super) input_name: Arc<str>,
    pub(super) account_name: Arc<str>,
    pub(super) session_owner: String,
    pub(super) reclaim_until_ms: u64,
    pub(super) last_media_at_ms: u64,
    pub(super) soft_overlap_eligible_at_ms: u64,
    pub(super) soft_overlap_delay_ms: u64,
    pub(super) tuliprox_target_user_connection_capacity: u32,
    pub(super) origin_input_account_connection_capacity: u32,
}

#[derive(Clone)]
pub(super) struct HlsOriginPolicyPreemptCandidate {
    pub(super) session: HlsSessionHandle,
    pub(super) proxy_session_id: ProxySessionId,
    pub(super) account_name: Arc<str>,
    pub(super) session_owner: String,
    pub(super) reservation_ttl_secs: u64,
    pub(super) victim_policy: HlsEffectiveOriginAcquirePolicy,
    pub(super) last_media_at_ms: u64,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct HlsSoftOverlapCapacity {
    pub(super) tuliprox_target_user_connection_capacity: u32,
    pub(super) origin_input_account_connection_capacity: u32,
    pub(super) delay_ms: u64,
}

pub(super) fn hls_soft_overlap_capacity_for_target_duration(
    tuliprox_target_user_connection_capacity: u32,
    origin_input_account_connection_capacity: u32,
    target_duration_ms: u64,
) -> HlsSoftOverlapCapacity {
    let delay_ms = hls_soft_overlap_delay_ms(
        target_duration_ms,
        tuliprox_target_user_connection_capacity,
        origin_input_account_connection_capacity,
    );
    HlsSoftOverlapCapacity {
        tuliprox_target_user_connection_capacity,
        origin_input_account_connection_capacity,
        delay_ms,
    }
}

pub(super) async fn hls_origin_input_account_connection_capacity(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
) -> u32 {
    let capacities = app_state.active_provider.provider_capacities_for_input(&input.name).await;
    if capacities.is_empty() {
        return hls_configured_origin_input_account_connection_capacity(input);
    }
    capacities
        .into_iter()
        .map(|(_, _, max)| if max == 0 { u32::MAX } else { u32::try_from(max).unwrap_or(u32::MAX) })
        .fold(0u32, u32::saturating_add)
        .max(1)
}

pub(super) fn hls_configured_origin_input_account_connection_capacity(input: &ConfigInput) -> u32 {
    let input_capacity = if input.max_connections == 0 { 1 } else { u32::from(input.max_connections) };
    input
        .aliases
        .as_ref()
        .map_or(0, |aliases| {
            aliases
                .iter()
                .filter(|alias| alias.enabled)
                .map(|alias| if alias.max_connections == 0 { 1 } else { u32::from(alias.max_connections) })
                .fold(0u32, u32::saturating_add)
        })
        .saturating_add(input_capacity)
        .max(1)
}

pub(super) async fn hls_tuliprox_target_user_connection_capacity(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
) -> u32 {
    hls_configured_tuliprox_target_user_connection_capacity(app_state, input)
        .max(hls_active_tuliprox_target_user_connections_for_input(app_state, input).await)
        .max(1)
}

pub(super) fn hls_configured_tuliprox_target_user_connection_capacity(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
) -> u32 {
    let Some(api_proxy) = app_state.app_config.api_proxy.load().as_ref().cloned() else {
        return 1;
    };
    api_proxy
        .user
        .iter()
        .filter(|target_user| {
            app_state
                .app_config
                .get_inputs_for_target(&target_user.target)
                .is_some_and(|inputs| inputs.iter().any(|candidate| candidate.name == input.name))
        })
        .map(|target_user| {
            target_user
                .credentials
                .iter()
                .map(|user| {
                    if user.max_connections == 0 {
                        u32::MAX
                    } else {
                        user.max_connections.saturating_add(u32::from(user.soft_connections))
                    }
                })
                .fold(0u32, u32::saturating_add)
        })
        .max()
        .unwrap_or(1)
        .max(1)
}

pub(super) async fn hls_active_tuliprox_target_user_connections_for_input(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
) -> u32 {
    u32::try_from(
        app_state
            .active_users
            .active_streams()
            .await
            .iter()
            .filter(|stream| stream.channel.input_name == input.name)
            .count(),
    )
    .unwrap_or(u32::MAX)
}

pub(super) fn hls_soft_overlap_delay_ms(
    target_duration_ms: u64,
    tuliprox_target_user_connection_capacity: u32,
    origin_input_account_connection_capacity: u32,
) -> u64 {
    let target_duration_ms = target_duration_ms.max(1);
    let users = u64::from(tuliprox_target_user_connection_capacity.max(1));
    let origin = u64::from(origin_input_account_connection_capacity.max(1));
    if users >= origin.saturating_mul(2) {
        return target_duration_ms;
    }
    if users <= origin {
        return target_duration_ms.saturating_mul(2);
    }
    let numerator = origin.saturating_mul(3).saturating_sub(users);
    target_duration_ms.saturating_mul(numerator).saturating_add(origin.saturating_sub(1)) / origin
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) async fn prepare_hls_origin_runtime(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    input: &ConfigInput,
    raw_request_url: &str,
    session_entry_url: &str,
    proxy_session_id: &ProxySessionId,
    fingerprint: &Fingerprint,
    connection_kind: crate::api::model::ConnectionKind,
    priority: i8,
    work_kind: HlsOriginWorkKind,
    work_class: HlsOriginWorkClass,
    now_ms: u64,
) -> Result<PreparedHlsOriginRuntime, HlsOriginRuntimeAcquireError> {
    promote_elapsed_hls_account_overlaps(app_state, now_ms).await;
    detach_unprotected_hls_origin_account_bindings(app_state, now_ms).await;
    reclaim_hls_account_overlap_if_needed(app_state, session, now_ms).await;
    let existing_binding = session.read().await.origin_account_binding.clone();
    let reacquire_detached_binding = existing_binding.as_ref().is_some_and(HlsOriginAccountBinding::is_detached);
    let final_no_account_reason = hls_no_account_reason_for_binding(existing_binding.as_ref());
    if reacquire_detached_binding {
        log_hls_origin_binding_reacquire_started(session, work_kind).await;
    }
    if let Some(binding) = existing_binding {
        if binding.is_active() {
            match hls_origin_account_status(&app_state.hls_ctx(), &binding) {
                stale_status @ (HlsOriginAccountStatus::Missing | HlsOriginAccountStatus::Expired) => {
                    return rebind_hls_origin_account(
                        app_state,
                        session,
                        input,
                        raw_request_url,
                        session_entry_url,
                        &binding,
                        stale_status,
                        fingerprint,
                        connection_kind,
                        priority,
                        now_ms,
                    )
                    .await;
                }
                HlsOriginAccountStatus::Known => {
                    return Ok(prepared_hls_origin_runtime_for_known_binding(
                        app_state,
                        input,
                        raw_request_url,
                        session_entry_url,
                        &binding,
                    ));
                }
            }
        }
    }

    match prepare_hls_origin_runtime_with_new_account(
        app_state,
        input,
        raw_request_url,
        session_entry_url,
        proxy_session_id,
        fingerprint,
        connection_kind,
        priority,
        false,
        work_kind,
        work_class,
        now_ms,
    )
    .await
    {
        Ok(prepared) => {
            if reacquire_detached_binding {
                if let Some(binding) = prepared.origin_account_binding_to_store.as_ref() {
                    log_hls_origin_binding_reacquired(session, binding).await;
                }
            }
            return Ok(prepared);
        }
        Err(HlsOriginRuntimeAcquireError::Fatal(status)) => return Err(HlsOriginRuntimeAcquireError::Fatal(status)),
        Err(HlsOriginRuntimeAcquireError::NoAccountAvailable { .. }) => {}
    }

    if work_class.allows_speculative_overlap() {
        if let Ok(prepared) = prepare_hls_origin_policy_preempt_runtime(
            app_state,
            session,
            input,
            raw_request_url,
            session_entry_url,
            proxy_session_id,
            fingerprint,
            connection_kind,
            priority,
            now_ms,
        )
        .await
        {
            if reacquire_detached_binding {
                if let Some(binding) = prepared.origin_account_binding_to_store.as_ref() {
                    log_hls_origin_binding_reacquired(session, binding).await;
                }
            }
            return Ok(prepared);
        }

        if let Ok(prepared) = prepare_hls_speculative_origin_runtime(
            app_state,
            session,
            input,
            raw_request_url,
            session_entry_url,
            proxy_session_id,
            fingerprint,
            connection_kind,
            priority,
            now_ms,
        )
        .await
        {
            if reacquire_detached_binding {
                if let Some(binding) = prepared.origin_account_binding_to_store.as_ref() {
                    log_hls_origin_binding_reacquired(session, binding).await;
                }
            }
            return Ok(prepared);
        }
    } else {
        debug!("HLS account overlap skipped: work_class={} reason=background-origin-work", work_class.as_log_value());
    }

    if work_class.allows_grace() {
        match prepare_hls_origin_runtime_with_new_account(
            app_state,
            input,
            raw_request_url,
            session_entry_url,
            proxy_session_id,
            fingerprint,
            connection_kind,
            priority,
            true,
            work_kind,
            work_class,
            now_ms,
        )
        .await
        {
            Ok(prepared) => {
                if reacquire_detached_binding {
                    if let Some(binding) = prepared.origin_account_binding_to_store.as_ref() {
                        log_hls_origin_binding_reacquired(session, binding).await;
                    }
                }
                return Ok(prepared);
            }
            Err(HlsOriginRuntimeAcquireError::Fatal(status)) => {
                return Err(HlsOriginRuntimeAcquireError::Fatal(status))
            }
            Err(HlsOriginRuntimeAcquireError::NoAccountAvailable { .. }) => {}
        }
    } else {
        debug!(
            "HLS origin account grace skipped: work_class={} reason=background-origin-work",
            work_class.as_log_value()
        );
    }

    if reacquire_detached_binding {
        log_hls_origin_binding_reacquire_failed(session, "no-account-available").await;
    }
    Err(HlsOriginRuntimeAcquireError::NoAccountAvailable { reason: final_no_account_reason })
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn prepare_hls_origin_runtime_with_new_account(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    raw_request_url: &str,
    session_entry_url: &str,
    proxy_session_id: &ProxySessionId,
    fingerprint: &Fingerprint,
    connection_kind: crate::api::model::ConnectionKind,
    priority: i8,
    allow_grace: bool,
    work_kind: HlsOriginWorkKind,
    work_class: HlsOriginWorkClass,
    now_ms: u64,
) -> Result<PreparedHlsOriginRuntime, HlsOriginRuntimeAcquireError> {
    let session_owner = build_hls_origin_session_owner(proxy_session_id);
    let Some(provider_handle) = app_state
        .active_provider
        .acquire_connection_with_grace_for_session(
            &input.name,
            &fingerprint.addr,
            allow_grace,
            priority,
            connection_kind,
            Some(&session_owner),
        )
        .await
    else {
        debug!(
            "HLS origin account acquire unavailable: work={} work_class={} grace={}",
            work_kind.as_log_value(),
            work_class.as_log_value(),
            if allow_grace { "attempted" } else { "disabled" }
        );
        return Err(HlsOriginRuntimeAcquireError::NoAccountAvailable {
            reason: HlsOriginRuntimeNoAccountReason::ProviderConnectionsExhausted,
        });
    };

    let Some(provider_config) = provider_handle.allocation.get_provider_config() else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };

    let Some(fetch_url) = build_hls_origin_fetch_url(input, raw_request_url, session_entry_url, Some(&provider_config))
    else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };

    let Some(binding) = origin_account_binding_from_allocation(
        Arc::clone(&input.name),
        proxy_session_id,
        &provider_handle.allocation,
        now_ms,
    ) else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };

    let grace_state = if matches!(provider_handle.allocation, ProviderAllocation::GracePeriod(_)) {
        "granted"
    } else if allow_grace {
        "not-needed"
    } else {
        "disabled"
    };
    debug!(
        "HLS origin account binding created: account={} owner={} work={} work_class={} grace={}",
        sanitize_sensitive_info(binding.account_name.as_ref()),
        sanitize_sensitive_info(&binding.session_owner),
        work_kind.as_log_value(),
        work_class.as_log_value(),
        grace_state
    );

    Ok(PreparedHlsOriginRuntime {
        url_failover_provider: hls_url_failover_provider_for_origin_context(
            input,
            raw_request_url,
            session_entry_url,
            &fetch_url,
        ),
        fetch_url,
        origin_account_binding_to_store: Some(binding),
        preacquired_origin_account_handle: Some(provider_handle),
    })
}

pub(super) fn prepared_hls_origin_runtime_for_known_binding(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    raw_request_url: &str,
    session_entry_url: &str,
    binding: &HlsOriginAccountBinding,
) -> PreparedHlsOriginRuntime {
    let fetch_url = app_state
        .active_provider
        .find_provider_config(&binding.account_name)
        .as_ref()
        .and_then(|provider_config| {
            build_hls_origin_fetch_url(input, raw_request_url, session_entry_url, Some(provider_config))
        })
        .unwrap_or_else(|| session_entry_url.to_string());

    PreparedHlsOriginRuntime {
        url_failover_provider: hls_url_failover_provider_for_origin_context(
            input,
            raw_request_url,
            session_entry_url,
            &fetch_url,
        ),
        fetch_url,
        origin_account_binding_to_store: None,
        preacquired_origin_account_handle: None,
    }
}

pub(super) async fn log_hls_origin_binding_reacquire_started(session: &HlsSessionHandle, work_kind: HlsOriginWorkKind) {
    let session_guard = session.read().await;
    let mode = match session_guard.mode {
        HlsSessionMode::NormalCacheTimeline => "normal",
        HlsSessionMode::TransientPassthrough { .. } => "transient",
    };
    debug!(
        "HLS origin binding reacquire started: proxy_session={} mode={} work={}",
        safe_proxy_session_id(&session_guard.proxy_session_id),
        mode,
        work_kind.as_log_value()
    );
}

pub(super) async fn log_hls_origin_binding_reacquired(session: &HlsSessionHandle, binding: &HlsOriginAccountBinding) {
    let session_guard = session.read().await;
    debug!(
        "HLS origin binding reacquired: proxy_session={} account={}",
        safe_proxy_session_id(&session_guard.proxy_session_id),
        sanitize_sensitive_info(binding.account_name.as_ref())
    );
}

pub(super) async fn log_hls_origin_binding_reacquire_failed(session: &HlsSessionHandle, reason: &str) {
    let session_guard = session.read().await;
    debug!(
        "HLS origin binding reacquire failed: proxy_session={} reason={} retry_after_ms={}",
        safe_proxy_session_id(&session_guard.proxy_session_id),
        reason,
        cold_start_retry_after_seconds().saturating_mul(1_000)
    );
}

pub(super) async fn detach_unprotected_hls_origin_account_bindings(app_state: &Arc<AppState>, now_ms: u64) {
    let sessions = app_state.hls_proxy.sessions().list_sessions().await;
    for session in sessions {
        let binding = {
            let mut session_guard = session.write().await;
            let Some(binding) = session_guard.origin_account_binding.clone() else {
                continue;
            };
            if !matches!(binding.binding_mode, HlsOriginAccountBindingMode::Active) {
                continue;
            }
            let timing = session_guard.account_overlap_timing();
            let protection = session_guard.account_binding_protection(now_ms);
            debug!(
                "HLS account protection classified: proxy_session={} state={} target_duration_ms={}",
                safe_proxy_session_id(&session_guard.proxy_session_id),
                protection.as_log_state(),
                timing.target_duration_ms
            );
            if !matches!(protection, HlsAccountBindingProtection::Expired)
                || session_guard.activity.active_origin_work_count > 0
            {
                continue;
            }
            if !matches!(hls_origin_account_status(&app_state.hls_ctx(), &binding), HlsOriginAccountStatus::Known) {
                continue;
            }
            if let Some(binding) = session_guard.origin_account_binding.as_mut() {
                binding.detach(HlsOriginAccountDetachedReason::SoftWindowElapsed, now_ms);
            }
            session_guard.invalidate_queued_origin_work();
            debug!(
                "HLS origin binding detached: proxy_session={} account={} reason={}",
                safe_proxy_session_id(&session_guard.proxy_session_id),
                sanitize_sensitive_info(binding.account_name.as_ref()),
                HlsOriginAccountDetachedReason::SoftWindowElapsed.as_log_reason()
            );
            binding
        };
        app_state.active_provider.clear_provider_reservation(&binding.session_owner).await;
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) async fn prepare_hls_origin_policy_preempt_runtime(
    app_state: &Arc<AppState>,
    new_session: &HlsSessionHandle,
    input: &ConfigInput,
    raw_request_url: &str,
    session_entry_url: &str,
    proxy_session_id: &ProxySessionId,
    fingerprint: &Fingerprint,
    connection_kind: crate::api::model::ConnectionKind,
    priority: i8,
    now_ms: u64,
) -> Result<PreparedHlsOriginRuntime, HlsOriginRuntimeAcquireError> {
    let request_policy = HlsEffectiveOriginAcquirePolicy::new(connection_kind, priority, now_ms);
    let Some(candidate) =
        find_hls_origin_policy_preempt_candidate(app_state, input, proxy_session_id, request_policy, now_ms).await
    else {
        debug!("HLS origin policy preemption denied: reason=no-lower-origin-policy-candidate");
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };

    app_state.active_provider.clear_provider_reservation(&candidate.session_owner).await;
    let session_owner = build_hls_origin_session_owner(proxy_session_id);
    let Some(provider_handle) = app_state
        .active_provider
        .acquire_exact_connection_with_grace_for_session(
            &candidate.account_name,
            &fingerprint.addr,
            false,
            priority,
            connection_kind,
            Some(&session_owner),
        )
        .await
    else {
        restore_hls_origin_policy_preempt_candidate_reservation(app_state, &candidate).await;
        debug!("HLS origin policy preemption denied: reason=exact-acquire-failed");
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };
    let Some(provider_config) = provider_handle.allocation.get_provider_config() else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        restore_hls_origin_policy_preempt_candidate_reservation(app_state, &candidate).await;
        debug!("HLS origin policy preemption denied: reason=missing-provider-config");
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };
    let Some(fetch_url) = build_hls_origin_fetch_url(input, raw_request_url, session_entry_url, Some(&provider_config))
    else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        restore_hls_origin_policy_preempt_candidate_reservation(app_state, &candidate).await;
        debug!("HLS origin policy preemption denied: reason=invalid-origin-url");
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };
    let Some(binding) = origin_account_binding_from_allocation(
        Arc::clone(&input.name),
        proxy_session_id,
        &provider_handle.allocation,
        now_ms,
    ) else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        restore_hls_origin_policy_preempt_candidate_reservation(app_state, &candidate).await;
        debug!("HLS origin policy preemption denied: reason=invalid-allocation");
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };

    let mut detached_victim = false;
    {
        let mut victim = candidate.session.write().await;
        if let Some(victim_binding) = victim.origin_account_binding.as_mut() {
            if victim_binding.account_name == candidate.account_name
                && victim_binding.session_owner == candidate.session_owner
                && matches!(victim_binding.binding_mode, HlsOriginAccountBindingMode::Active)
            {
                victim_binding.detach(HlsOriginAccountDetachedReason::PreemptedByHigherPriority, now_ms);
                detached_victim = true;
            }
        }
        if detached_victim {
            victim.invalidate_queued_origin_work();
        }
    }
    if !detached_victim {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        restore_hls_origin_policy_preempt_candidate_reservation(app_state, &candidate).await;
        debug!("HLS origin policy preemption denied: reason=stale-candidate");
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    }

    {
        let mut session_guard = new_session.write().await;
        session_guard.replace_origin_account_binding(Some(binding.clone()));
    }
    debug!(
        "HLS origin policy preempted: account={} victim_proxy_session={} winner_proxy_session={} victim_kind={:?} victim_priority={} request_kind={:?} request_priority={}",
        sanitize_sensitive_info(candidate.account_name.as_ref()),
        safe_proxy_session_id(&candidate.proxy_session_id),
        safe_proxy_session_id(proxy_session_id),
        candidate.victim_policy.connection_kind,
        candidate.victim_policy.priority,
        request_policy.connection_kind,
        request_policy.priority
    );
    debug!(
        "HLS origin binding detached: proxy_session={} account={} reason={}",
        safe_proxy_session_id(&candidate.proxy_session_id),
        sanitize_sensitive_info(candidate.account_name.as_ref()),
        HlsOriginAccountDetachedReason::PreemptedByHigherPriority.as_log_reason()
    );

    Ok(PreparedHlsOriginRuntime {
        url_failover_provider: hls_url_failover_provider_for_origin_context(
            input,
            raw_request_url,
            session_entry_url,
            &fetch_url,
        ),
        fetch_url,
        origin_account_binding_to_store: Some(binding),
        preacquired_origin_account_handle: Some(provider_handle),
    })
}

pub(super) async fn restore_hls_origin_policy_preempt_candidate_reservation(
    app_state: &Arc<AppState>,
    candidate: &HlsOriginPolicyPreemptCandidate,
) {
    app_state
        .active_provider
        .refresh_provider_reservation(&candidate.account_name, &candidate.session_owner, candidate.reservation_ttl_secs)
        .await;
}

pub(super) async fn find_hls_origin_policy_preempt_candidate(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    new_proxy_session_id: &ProxySessionId,
    request_policy: HlsEffectiveOriginAcquirePolicy,
    _now_ms: u64,
) -> Option<HlsOriginPolicyPreemptCandidate> {
    let sessions = app_state.hls_proxy.sessions().list_sessions().await;
    let mut best_candidate = None;
    for session in sessions {
        let session_guard = session.read().await;
        if session_guard.proxy_session_id == *new_proxy_session_id {
            continue;
        }
        let Some(binding) = session_guard.origin_account_binding.as_ref() else {
            continue;
        };
        if binding.input_name != input.name || !matches!(binding.binding_mode, HlsOriginAccountBindingMode::Active) {
            continue;
        }
        if session_guard.activity.active_origin_work_count > 0 {
            continue;
        }
        if !matches!(hls_origin_account_status(&app_state.hls_ctx(), binding), HlsOriginAccountStatus::Known) {
            continue;
        }
        let victim_policy = session_guard.effective_origin_acquire_policy_or_default();
        if !request_policy.is_better_than(victim_policy) {
            continue;
        }
        let candidate = HlsOriginPolicyPreemptCandidate {
            session: Arc::clone(&session),
            proxy_session_id: session_guard.proxy_session_id.clone(),
            account_name: Arc::clone(&binding.account_name),
            session_owner: binding.session_owner.clone(),
            reservation_ttl_secs: session_guard.account_overlap_timing().reservation_ttl_secs(),
            victim_policy,
            last_media_at_ms: session_guard.activity.last_authorized_media_at_ms.unwrap_or_default(),
        };
        if hls_origin_policy_preempt_candidate_is_better(best_candidate.as_ref(), &candidate) {
            best_candidate = Some(candidate);
        }
    }
    best_candidate
}

pub(super) fn hls_origin_policy_preempt_candidate_is_better(
    current: Option<&HlsOriginPolicyPreemptCandidate>,
    candidate: &HlsOriginPolicyPreemptCandidate,
) -> bool {
    let Some(current) = current else {
        return true;
    };
    match (candidate.victim_policy.connection_kind, current.victim_policy.connection_kind) {
        (crate::api::model::ConnectionKind::Soft, crate::api::model::ConnectionKind::Normal) => return true,
        (crate::api::model::ConnectionKind::Normal, crate::api::model::ConnectionKind::Soft) => return false,
        _ => {}
    }
    candidate.victim_policy.priority > current.victim_policy.priority
        || (candidate.victim_policy.priority == current.victim_policy.priority
            && candidate.last_media_at_ms < current.last_media_at_ms)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn prepare_hls_speculative_origin_runtime(
    app_state: &Arc<AppState>,
    new_session: &HlsSessionHandle,
    input: &ConfigInput,
    raw_request_url: &str,
    session_entry_url: &str,
    proxy_session_id: &ProxySessionId,
    fingerprint: &Fingerprint,
    connection_kind: crate::api::model::ConnectionKind,
    priority: i8,
    now_ms: u64,
) -> Result<PreparedHlsOriginRuntime, HlsOriginRuntimeAcquireError> {
    let Some(candidate) = find_hls_account_overlap_candidate(app_state, input, proxy_session_id, now_ms).await else {
        debug!("HLS account overlap denied: reason=no-soft-active-candidate");
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };
    app_state.active_provider.clear_provider_reservation(&candidate.session_owner).await;
    let session_owner = build_hls_origin_session_owner(proxy_session_id);
    let Some(provider_handle) = app_state
        .active_provider
        .acquire_exact_connection_with_grace_for_session(
            &candidate.account_name,
            &fingerprint.addr,
            false,
            priority,
            connection_kind,
            Some(&session_owner),
        )
        .await
    else {
        debug!("HLS account overlap denied: reason=speculative-acquire-failed");
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };
    let Some(provider_config) = provider_handle.allocation.get_provider_config() else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        debug!("HLS account overlap denied: reason=missing-provider-config");
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };
    let Some(fetch_url) = build_hls_origin_fetch_url(input, raw_request_url, session_entry_url, Some(&provider_config))
    else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        debug!("HLS account overlap denied: reason=invalid-origin-url");
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };
    let binding = HlsOriginAccountBinding::speculative_from(
        Arc::clone(&input.name),
        Arc::clone(&candidate.account_name),
        proxy_session_id,
        candidate.proxy_session_id.clone(),
        candidate.reclaim_until_ms,
        now_ms,
    );
    {
        let mut session_guard = new_session.write().await;
        session_guard.replace_origin_account_binding(Some(binding.clone()));
    }
    debug!(
        "HLS account overlap granted: account={} victim_proxy_session={} winner_proxy_session={} reclaim_until_ms={} eligible_after_ms={} delay_ms={} tuliprox_target_user_connections={} origin_input_account_connections={}",
        sanitize_sensitive_info(candidate.account_name.as_ref()),
        safe_proxy_session_id(&candidate.proxy_session_id),
        safe_proxy_session_id(proxy_session_id),
        candidate.reclaim_until_ms,
        candidate.soft_overlap_eligible_at_ms,
        candidate.soft_overlap_delay_ms,
        candidate.tuliprox_target_user_connection_capacity,
        candidate.origin_input_account_connection_capacity
    );
    Ok(PreparedHlsOriginRuntime {
        url_failover_provider: hls_url_failover_provider_for_origin_context(
            input,
            raw_request_url,
            session_entry_url,
            &fetch_url,
        ),
        fetch_url,
        origin_account_binding_to_store: Some(binding),
        preacquired_origin_account_handle: Some(provider_handle),
    })
}

pub(super) async fn find_hls_account_overlap_candidate(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    new_proxy_session_id: &ProxySessionId,
    now_ms: u64,
) -> Option<HlsAccountOverlapCandidate> {
    let sessions = app_state.hls_proxy.sessions().list_sessions().await;
    let tuliprox_target_user_connection_capacity = hls_tuliprox_target_user_connection_capacity(app_state, input).await;
    let origin_input_account_connection_capacity = hls_origin_input_account_connection_capacity(app_state, input).await;
    let mut speculative_accounts = Vec::new();
    for session in &sessions {
        let session = session.read().await;
        let Some(binding) = session.origin_account_binding.as_ref() else {
            continue;
        };
        if binding.input_name != input.name {
            continue;
        }
        if matches!(
            binding.binding_mode,
            HlsOriginAccountBindingMode::Speculative { reclaim_until_ms, .. } if now_ms <= reclaim_until_ms
        ) {
            speculative_accounts.push(Arc::clone(&binding.account_name));
        }
    }

    let mut candidates = Vec::new();
    for session in sessions {
        let session_guard = session.read().await;
        if session_guard.proxy_session_id == *new_proxy_session_id {
            continue;
        }
        let Some(binding) = session_guard.origin_account_binding.as_ref() else {
            continue;
        };
        if binding.input_name != input.name
            || speculative_accounts.iter().any(|account| account == &binding.account_name)
        {
            continue;
        }
        if !matches!(binding.binding_mode, HlsOriginAccountBindingMode::Active) {
            continue;
        }
        if session_guard.activity.active_origin_work_count > 0 {
            continue;
        }
        let timing = session_guard.account_overlap_timing();
        let protection = session_guard.account_binding_protection(now_ms);
        debug!(
            "HLS account protection classified: proxy_session={} state={} target_duration_ms={}",
            safe_proxy_session_id(&session_guard.proxy_session_id),
            protection.as_log_state(),
            timing.target_duration_ms
        );
        let HlsAccountBindingProtection::SoftActive { reclaim_until_ms } = protection else {
            continue;
        };
        let last_media_at_ms = session_guard.activity.last_authorized_media_at_ms.unwrap_or_default();
        let capacity = hls_soft_overlap_capacity_for_target_duration(
            tuliprox_target_user_connection_capacity,
            origin_input_account_connection_capacity,
            timing.target_duration_ms,
        );
        let eligible_at_ms = last_media_at_ms.saturating_add(capacity.delay_ms);
        if now_ms < eligible_at_ms {
            debug!(
                "HLS account overlap waiting: proxy_session={} account={} eligible_at_ms={} now_ms={} delay_ms={} tuliprox_target_user_connections={} origin_input_account_connections={}",
                safe_proxy_session_id(&session_guard.proxy_session_id),
                sanitize_sensitive_info(binding.account_name.as_ref()),
                eligible_at_ms,
                now_ms,
                capacity.delay_ms,
                capacity.tuliprox_target_user_connection_capacity,
                capacity.origin_input_account_connection_capacity
            );
            continue;
        }
        candidates.push(HlsAccountOverlapCandidate {
            proxy_session_id: session_guard.proxy_session_id.clone(),
            input_name: Arc::clone(&binding.input_name),
            account_name: Arc::clone(&binding.account_name),
            session_owner: binding.session_owner.clone(),
            reclaim_until_ms,
            last_media_at_ms,
            soft_overlap_eligible_at_ms: eligible_at_ms,
            soft_overlap_delay_ms: capacity.delay_ms,
            tuliprox_target_user_connection_capacity: capacity.tuliprox_target_user_connection_capacity,
            origin_input_account_connection_capacity: capacity.origin_input_account_connection_capacity,
        });
    }
    let mut eligible = filter_hls_account_overlap_cooldowns(app_state, candidates, now_ms).await;
    eligible.sort_by_key(|candidate| (candidate.last_media_at_ms, candidate.soft_overlap_eligible_at_ms));
    eligible.into_iter().next()
}

pub(super) async fn filter_hls_account_overlap_cooldowns(
    app_state: &Arc<AppState>,
    candidates: Vec<HlsAccountOverlapCandidate>,
    now_ms: u64,
) -> Vec<HlsAccountOverlapCandidate> {
    let mut eligible = Vec::new();
    for candidate in candidates {
        if app_state
            .hls_proxy
            .is_account_overlap_cooling_down(&candidate.input_name, &candidate.account_name, now_ms)
            .await
        {
            debug!(
                "HLS account overlap skipped: proxy_session={} account={} reason=cooldown-active",
                safe_proxy_session_id(&candidate.proxy_session_id),
                sanitize_sensitive_info(candidate.account_name.as_ref())
            );
            continue;
        }
        eligible.push(candidate);
    }
    eligible
}

pub(super) async fn reclaim_hls_account_overlap_if_needed(
    app_state: &Arc<AppState>,
    winner_session: &HlsSessionHandle,
    now_ms: u64,
) {
    let winner_proxy_session_id = winner_session.read().await.proxy_session_id.clone();
    let sessions = app_state.hls_proxy.sessions().list_sessions().await;
    for session in sessions {
        let (loser_proxy_session_id, loser_binding) = {
            let session_guard = session.read().await;
            let Some(binding) = session_guard.origin_account_binding.clone() else {
                continue;
            };
            let HlsOriginAccountBindingMode::Speculative { displaced_proxy_session_id, reclaim_until_ms } =
                &binding.binding_mode
            else {
                continue;
            };
            if displaced_proxy_session_id != &winner_proxy_session_id || now_ms > *reclaim_until_ms {
                continue;
            }
            (session_guard.proxy_session_id.clone(), binding)
        };
        app_state.active_provider.clear_provider_reservation(&loser_binding.session_owner).await;
        {
            let mut loser = session.write().await;
            if let Some(binding) = loser.origin_account_binding.as_mut() {
                binding.detach(HlsOriginAccountDetachedReason::ReclaimedByOriginalOwner, now_ms);
            }
            loser.invalidate_queued_origin_work();
        }
        {
            let mut winner = winner_session.write().await;
            if let Some(binding) = winner.origin_account_binding.as_mut() {
                binding.promote_to_active();
            }
        }
        let hard_active_window_ms = winner_session.read().await.account_overlap_timing().hard_active_window_ms;
        app_state
            .hls_proxy
            .mark_account_overlap_reclaimed_cooldown(
                Arc::clone(&loser_binding.input_name),
                Arc::clone(&loser_binding.account_name),
                now_ms,
                hard_active_window_ms,
            )
            .await;
        debug!(
            "HLS account overlap reclaimed: account={} winner={} loser={}",
            sanitize_sensitive_info(loser_binding.account_name.as_ref()),
            safe_proxy_session_id(&winner_proxy_session_id),
            safe_proxy_session_id(&loser_proxy_session_id)
        );
        debug!(
            "HLS origin binding detached: proxy_session={} account={} reason={}",
            safe_proxy_session_id(&loser_proxy_session_id),
            sanitize_sensitive_info(loser_binding.account_name.as_ref()),
            HlsOriginAccountDetachedReason::ReclaimedByOriginalOwner.as_log_reason()
        );
    }
}

pub(super) async fn promote_elapsed_hls_account_overlaps(app_state: &Arc<AppState>, now_ms: u64) {
    let sessions = app_state.hls_proxy.sessions().list_sessions().await;
    for session in sessions {
        let (input_name, account_name, promoted_session_id, displaced_session_id, hard_active_window_ms) = {
            let mut session_guard = session.write().await;
            let hard_active_window_ms = session_guard.account_overlap_timing().hard_active_window_ms;
            let Some(binding) = session_guard.origin_account_binding.as_mut() else {
                continue;
            };
            let HlsOriginAccountBindingMode::Speculative { displaced_proxy_session_id, reclaim_until_ms } =
                &binding.binding_mode
            else {
                continue;
            };
            if now_ms <= *reclaim_until_ms {
                continue;
            }
            let displaced_session_id = displaced_proxy_session_id.clone();
            let input_name = Arc::clone(&binding.input_name);
            let account_name = Arc::clone(&binding.account_name);
            binding.promote_to_active();
            (
                input_name,
                account_name,
                session_guard.proxy_session_id.clone(),
                displaced_session_id,
                hard_active_window_ms,
            )
        };
        app_state
            .hls_proxy
            .mark_account_overlap_promoted_cooldown(
                Arc::clone(&input_name),
                Arc::clone(&account_name),
                now_ms,
                hard_active_window_ms,
            )
            .await;
        if let Some(displaced) = app_state.hls_proxy.sessions().get_by_proxy_session_id(&displaced_session_id).await {
            let mut detached = false;
            let mut displaced = displaced.write().await;
            if displaced.origin_account_binding.as_ref().is_some_and(|binding| binding.account_name == account_name) {
                if let Some(binding) = displaced.origin_account_binding.as_mut() {
                    binding.detach(HlsOriginAccountDetachedReason::SoftWindowElapsed, now_ms);
                    detached = true;
                }
                displaced.invalidate_queued_origin_work();
            }
            if detached {
                debug!(
                    "HLS origin binding detached: proxy_session={} account={} reason={}",
                    safe_proxy_session_id(&displaced_session_id),
                    sanitize_sensitive_info(account_name.as_ref()),
                    HlsOriginAccountDetachedReason::SoftWindowElapsed.as_log_reason()
                );
            }
        }
        debug!(
            "HLS account overlap promoted: account={} proxy_session={}",
            sanitize_sensitive_info(account_name.as_ref()),
            safe_proxy_session_id(&promoted_session_id)
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn rebind_hls_origin_account(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    input: &ConfigInput,
    raw_request_url: &str,
    session_entry_url: &str,
    stale_binding: &HlsOriginAccountBinding,
    stale_status: HlsOriginAccountStatus,
    fingerprint: &Fingerprint,
    connection_kind: crate::api::model::ConnectionKind,
    priority: i8,
    now_ms: u64,
) -> Result<PreparedHlsOriginRuntime, HlsOriginRuntimeAcquireError> {
    {
        let mut session_guard = session.write().await;
        if !session_guard.origin_account_rebind.is_allowed_now(now_ms) {
            debug!(
                "HLS origin account rebind skipped by backoff: proxy_session={} old_account={} retry_after_ms=2000",
                safe_proxy_session_id(&session_guard.proxy_session_id),
                sanitize_sensitive_info(stale_binding.account_name.as_ref())
            );
            return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
        }
        session_guard.origin_account_rebind.mark_attempt_started(Arc::clone(&stale_binding.account_name), now_ms);
    }

    let safe_proxy_session = {
        let session_guard = session.read().await;
        safe_proxy_session_id(&session_guard.proxy_session_id)
    };
    debug!(
        "HLS origin account rebind started: proxy_session={} old_account={} reason={stale_status:?}",
        safe_proxy_session,
        sanitize_sensitive_info(stale_binding.account_name.as_ref())
    );
    app_state.active_provider.clear_provider_reservation(&stale_binding.session_owner).await;
    {
        let mut session_guard = session.write().await;
        if let Some(binding) = session_guard.origin_account_binding.as_mut().filter(|binding| {
            binding.account_name == stale_binding.account_name && binding.session_owner == stale_binding.session_owner
        }) {
            binding.detach(HlsOriginAccountDetachedReason::AccountMissingOrExpired, now_ms);
            debug!(
                "HLS origin binding detached: proxy_session={} account={} reason={}",
                safe_proxy_session_id(&session_guard.proxy_session_id),
                sanitize_sensitive_info(stale_binding.account_name.as_ref()),
                HlsOriginAccountDetachedReason::AccountMissingOrExpired.as_log_reason()
            );
        }
        session_guard.invalidate_queued_origin_work();
    }

    let Some(provider_handle) = app_state
        .active_provider
        .acquire_connection_with_grace_for_session(
            &input.name,
            &fingerprint.addr,
            false,
            priority,
            connection_kind,
            Some(&stale_binding.session_owner),
        )
        .await
    else {
        mark_hls_origin_rebind_failed(session, stale_binding, now_ms, "no_account_available").await;
        return Err(HlsOriginRuntimeAcquireError::NoAccountAvailable {
            reason: hls_no_account_reason_for_binding(Some(stale_binding)),
        });
    };

    let Some(provider_config) = provider_handle.allocation.get_provider_config() else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        mark_hls_origin_rebind_failed(session, stale_binding, now_ms, "no_provider_config").await;
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };

    let Some(fetch_url) = build_hls_origin_fetch_url(input, raw_request_url, session_entry_url, Some(&provider_config))
    else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        mark_hls_origin_rebind_failed(session, stale_binding, now_ms, "invalid_origin_url").await;
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };

    let Some(new_account_name) = provider_handle.allocation.get_provider_name() else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        mark_hls_origin_rebind_failed(session, stale_binding, now_ms, "missing_account_name").await;
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };

    let new_binding = HlsOriginAccountBinding::rebound(
        Arc::clone(&input.name),
        new_account_name,
        stale_binding.session_owner.clone(),
        stale_binding.generation.saturating_add(1),
        now_ms,
    );
    {
        let mut session_guard = session.write().await;
        session_guard.replace_origin_account_binding(Some(new_binding.clone()));
        session_guard.origin_account_rebind.mark_success();
    }
    debug!(
        "HLS origin account rebound: old_account={} new_account={}",
        sanitize_sensitive_info(stale_binding.account_name.as_ref()),
        sanitize_sensitive_info(new_binding.account_name.as_ref())
    );

    Ok(PreparedHlsOriginRuntime {
        url_failover_provider: hls_url_failover_provider_for_origin_context(
            input,
            raw_request_url,
            session_entry_url,
            &fetch_url,
        ),
        fetch_url,
        origin_account_binding_to_store: None,
        preacquired_origin_account_handle: Some(provider_handle),
    })
}

pub(super) async fn mark_hls_origin_rebind_failed(
    session: &HlsSessionHandle,
    stale_binding: &HlsOriginAccountBinding,
    now_ms: u64,
    reason: &str,
) {
    let mut session_guard = session.write().await;
    session_guard.origin_account_rebind.mark_failed(now_ms);
    debug!(
        "HLS origin account rebind failed: proxy_session={} old_account={} reason={reason} retry_after_ms=2000",
        safe_proxy_session_id(&session_guard.proxy_session_id),
        sanitize_sensitive_info(stale_binding.account_name.as_ref())
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn prepare_hls_cache_user_session(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    session_token: &str,
    virtual_id: u32,
    request_url: &str,
    input: &ConfigInput,
    connection_permission: UserConnectionPermission,
    connection_kind: Option<crate::api::model::ConnectionKind>,
) -> String {
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user,
            session_token,
            virtual_id,
            provider: input.name.as_ref(),
            stream_url: request_url,
            addr: &fingerprint.addr,
            connection_permission,
            connection_kind,
            socket_bound: PlaylistItemType::LiveHls.uses_socket_bound_session(),
        })
        .await
}

pub(super) fn hls_entry_origin_connection_kind(
    connection_permission: UserConnectionPermission,
    connection_kind: Option<crate::api::model::ConnectionKind>,
) -> Option<crate::api::model::ConnectionKind> {
    match connection_permission {
        UserConnectionPermission::Allowed | UserConnectionPermission::GracePeriod => connection_kind,
        UserConnectionPermission::Exhausted => None,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn create_hls_cache_entry_master_playlist_response(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    origin_source: HlsOriginSource,
    virtual_id: u32,
    existing_user_session: Option<&UserSession>,
    known_bitrate_bps: Option<u32>,
    session_token_hint: Option<&str>,
    request_url: &str,
    input: &ConfigInput,
    connection_permission: UserConnectionPermission,
    connection_kind: Option<crate::api::model::ConnectionKind>,
    server_path: Option<&str>,
) -> axum::response::Response {
    let item_bandwidth = HlsMasterBandwidth::new(known_bitrate_bps);
    let database_bitrate_bps = if item_bandwidth.is_unknown() {
        match load_input_live_bitrate_bps(&app_state.app_config, input, &origin_source.stream_ref).await {
            Ok(known_bitrate_bps) => known_bitrate_bps,
            Err(err) => {
                warn!("HLS entry live bitrate lookup failed; using fallback: input_id={} error={err}", input.id);
                None
            }
        }
    } else {
        None
    };
    let bandwidth = HlsMasterBandwidthSelection::resolve(known_bitrate_bps, database_bitrate_bps);
    let known_bitrate_bps = bandwidth.known_bitrate_bps();

    let session_key = origin_source.session_key();
    let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
    let family_key = HlsPlaybackFamilyKey::new(user.username.clone(), fingerprint.key.clone());
    let now_ms = current_time_millis();
    let origin_connection_kind = hls_entry_origin_connection_kind(connection_permission, connection_kind);
    let access_lease_id = new_hls_access_lease_id();
    let existing_token = existing_user_session.map(|session| session.token.as_str()).or(session_token_hint);
    let session_token = create_hls_cache_user_session_token(
        fingerprint,
        &user.username,
        virtual_id,
        existing_token,
        origin_source.archive_reference,
    );
    let session_token = prepare_hls_cache_user_session(
        app_state,
        fingerprint,
        user,
        &session_token,
        virtual_id,
        request_url,
        input,
        connection_permission,
        origin_connection_kind,
    )
    .await;
    let mut lease = HlsAccessLease::pending(
        access_lease_id.clone(),
        family_key,
        proxy_session_id.clone(),
        user.username.clone(),
        session_token.clone(),
        origin_source.input_id,
        origin_source.stream_ref.clone(),
        virtual_id,
        now_ms,
        hls_pending_bootstrap_window_ms(app_state),
    )
    .with_known_bitrate_bps(known_bitrate_bps)
    .with_archive_playback(
        origin_source.archive_reference,
        origin_source.archive_reference.map(|_| request_url.to_string()),
    );
    if let Some(connection_kind) = origin_connection_kind {
        lease = lease.with_origin_acquire_policy(connection_kind, connection_priority_for_kind(user, connection_kind));
    } else {
        lease.state = HlsAccessLeaseState::Denied;
    }
    app_state.hls_proxy.prepare_access_lease(lease).await;
    debug!(
        "HLS access lease prepared: lease={} session={} proxy_session={} user_session={} action=created reason=new-playback",
        safe_hls_access_lease_id(&access_lease_id),
        safe_session_key(&session_key),
        safe_proxy_session_id(&proxy_session_id),
        safe_user_session_token(&session_token)
    );
    let response =
        hls_entry_master_playlist_response(&proxy_session_id, &access_lease_id, bandwidth.bandwidth(), server_path);
    app_state.hls_proxy.startup_observability().record_entry_master_response(
        access_lease_id.clone(),
        HlsLogIdentity::new(&session_key, &proxy_session_id),
        current_time_millis(),
    );
    debug!(
        "HLS master playlist response: {}",
        HlsMasterPlaylistResponseDiagnostic {
            lease: safe_hls_access_lease_id(&access_lease_id),
            session: safe_session_key(&session_key),
            proxy_session: safe_proxy_session_id(&proxy_session_id),
            user_session: safe_user_session_token(&session_token),
            virtual_id,
            bandwidth_bps: bandwidth.bandwidth().advertised_bps(),
            bandwidth_source: bandwidth.source().as_log_value(),
            content_length: response.content_length,
        }
    );
    response.response
}

pub(super) struct HlsEntryMasterPlaylistResponse {
    pub(super) response: axum::response::Response,
    pub(super) content_length: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HlsMasterPlaylistResponseDiagnostic {
    pub(super) lease: String,
    pub(super) session: String,
    pub(super) proxy_session: String,
    pub(super) user_session: String,
    pub(super) virtual_id: u32,
    pub(super) bandwidth_bps: u32,
    pub(super) bandwidth_source: &'static str,
    pub(super) content_length: usize,
}

impl std::fmt::Display for HlsMasterPlaylistResponseDiagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "lease={} session={} proxy_session={} user_session={} virtual_id={} bandwidth_bps={} bandwidth_source={} status=200 content_length={}",
            self.lease,
            self.session,
            self.proxy_session,
            self.user_session,
            self.virtual_id,
            self.bandwidth_bps,
            self.bandwidth_source,
            self.content_length
        )
    }
}

pub(super) async fn hls_segment_request_requires_origin_work(
    session: &HlsSessionHandle,
    segment_file: &HlsSegmentFile,
) -> bool {
    let session = session.read().await;
    let Some(entry) = session.segments.get(&segment_file.proxy_seq) else {
        return false;
    };
    if entry.proxy_file_ext != segment_file.extension {
        return false;
    }
    matches!(entry.status, SegmentCacheStatus::Discovered | SegmentCacheStatus::Queued { .. })
        && entry.origin_fetch_ref.is_some()
}

pub(super) async fn hls_origin_binding_needs_reacquire(session: &HlsSessionHandle) -> bool {
    let session = session.read().await;
    session.origin_account_binding.as_ref().is_some_and(HlsOriginAccountBinding::is_detached)
}

pub(super) fn hls_transient_origin_binding_requires_runtime_prepare(
    hls_ctx: &HlsCtx,
    binding: &HlsOriginAccountBinding,
) -> bool {
    binding.is_detached()
        || (binding.is_active()
            && matches!(
                hls_origin_account_status(hls_ctx, binding),
                HlsOriginAccountStatus::Missing | HlsOriginAccountStatus::Expired
            ))
}

pub(super) async fn prepare_hls_origin_binding_for_authorized_resource_work(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    access_context: &HlsAccessContext,
    fingerprint: &Fingerprint,
    req_headers: &HeaderMap,
    work_kind: HlsOriginWorkKind,
    now_ms: u64,
) -> Result<Option<ProviderHandle>, HlsOriginRuntimeAcquireError> {
    if !hls_origin_binding_needs_reacquire(session).await {
        return Ok(None);
    }
    if session.read().await.activity.active_origin_work_count > 0 {
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    }
    let request_context = resolve_hls_playback_manifest_request_context(app_state, access_context, req_headers)
        .await
        .map_err(HlsOriginRuntimeAcquireError::Fatal)?;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let origin_policy = hls_effective_origin_acquire_policy(session).await;
    let prepared_origin = prepare_hls_origin_runtime(
        app_state,
        session,
        &request_context.input,
        &request_context.hls_url,
        request_context.session_entry_url.as_str(),
        &proxy_session_id,
        fingerprint,
        origin_policy.connection_kind,
        origin_policy.priority,
        work_kind,
        HlsOriginWorkClass::Demand,
        now_ms,
    )
    .await?;
    if let Some(binding) = prepared_origin.origin_account_binding_to_store {
        session.write().await.replace_origin_account_binding(Some(binding));
    }
    Ok(prepared_origin.preacquired_origin_account_handle)
}

#[allow(clippy::too_many_lines)]
pub(super) async fn prepare_hls_transient_origin_io_for_authorized_resource_work(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    access_context: &HlsAccessContext,
    fingerprint: &Fingerprint,
    req_headers: &HeaderMap,
    now_ms: u64,
) -> Result<Option<HlsTransientOriginIoGuard>, HlsOriginRuntimeAcquireError> {
    let hls_ctx = app_state.hls_ctx();
    let existing_binding = session.read().await.origin_account_binding.clone();
    let origin_policy = hls_effective_origin_acquire_policy(session).await;
    let reservation_ttl_secs = hls_origin_account_reservation_ttl_secs_for_session(session).await;
    if let Some(binding) = existing_binding.as_ref().filter(|binding| binding.is_active()) {
        match hls_origin_account_status(&hls_ctx, binding) {
            HlsOriginAccountStatus::Known => {
                let origin_io = HlsOriginIoContext {
                    ctx: hls_ctx.clone(),
                    client_addr: fingerprint.addr,
                    allow_grace: HlsOriginWorkClass::Demand.allows_grace(),
                    priority: origin_policy.priority,
                    connection_kind: origin_policy.connection_kind,
                    reservation_ttl_secs,
                    preacquired_provider_handle: None,
                    started_generation: None,
                };
                let started_generation = session.write().await.start_origin_work();
                if let Ok(lease_guard) = begin_hls_origin_account_io_bounded(
                    &origin_io,
                    session,
                    binding,
                    hls_object_body_deadline(app_state.hls_proxy.segment_fetch_policy().origin_segment_timeout_ms),
                )
                .await
                {
                    return Ok(Some(HlsTransientOriginIoGuard::new(
                        Arc::clone(session),
                        origin_io,
                        lease_guard,
                        started_generation,
                    )));
                }
                session.write().await.finish_origin_work(started_generation);
                return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
            }
            HlsOriginAccountStatus::Missing | HlsOriginAccountStatus::Expired => {}
        }
    }

    if !existing_binding
        .as_ref()
        .is_some_and(|binding| hls_transient_origin_binding_requires_runtime_prepare(&hls_ctx, binding))
    {
        return Ok(None);
    }
    if session.read().await.activity.active_origin_work_count > 0 {
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    }
    let request_context = resolve_hls_playback_manifest_request_context(app_state, access_context, req_headers)
        .await
        .map_err(HlsOriginRuntimeAcquireError::Fatal)?;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let prepared_origin = prepare_hls_origin_runtime(
        app_state,
        session,
        &request_context.input,
        &request_context.hls_url,
        request_context.session_entry_url.as_str(),
        &proxy_session_id,
        fingerprint,
        origin_policy.connection_kind,
        origin_policy.priority,
        HlsOriginWorkKind::Resource,
        HlsOriginWorkClass::Demand,
        now_ms,
    )
    .await?;
    if let Some(binding) = prepared_origin.origin_account_binding_to_store {
        session.write().await.replace_origin_account_binding(Some(binding));
    }
    let Some(provider_handle) = prepared_origin.preacquired_origin_account_handle else {
        return Ok(None);
    };
    let Some(binding) = session.read().await.origin_account_binding.clone().filter(HlsOriginAccountBinding::is_active)
    else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };
    let origin_io = HlsOriginIoContext {
        ctx: hls_ctx,
        client_addr: fingerprint.addr,
        allow_grace: HlsOriginWorkClass::Demand.allows_grace(),
        priority: origin_policy.priority,
        connection_kind: origin_policy.connection_kind,
        reservation_ttl_secs,
        preacquired_provider_handle: None,
        started_generation: None,
    }
    .with_preacquired_provider_handle(provider_handle);
    let started_generation = session.write().await.start_origin_work();
    let Ok(lease_guard) = begin_hls_origin_account_io_bounded(
        &origin_io,
        session,
        &binding,
        hls_object_body_deadline(app_state.hls_proxy.segment_fetch_policy().origin_segment_timeout_ms),
    )
    .await
    else {
        session.write().await.finish_origin_work(started_generation);
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };
    Ok(Some(HlsTransientOriginIoGuard::new(Arc::clone(session), origin_io, lease_guard, started_generation)))
}
