use crate::model::{macros, ConfigProvider, EpgConfig, PanelApiConfig};
use crate::repository::get_csv_file_path;
use chrono::Utc;
use log::warn;
use shared::foundation::Filter;
use shared::{apply_flags, create_bitset};
use shared::error::TuliproxError;
use shared::model::{ClusterFlags, ConfigInputAliasDto, ConfigInputDto, ConfigInputOptionsDto, ConfigInputStagedDto, InputFetchMethod, InputType, MediaServerCatalogConfigDto, MediaServerImagePolicy, MediaServerInputConfigDto, MediaServerLibrarySelector, MediaServerPlaybackConfigDto, StagedInputType};
use shared::utils::{
    get_credentials_from_url, get_credentials_from_url_str, is_non_blank_optional_string, parse_provider_scheme_url_parts, sanitize_sensitive_info, Internable,
    BATCH_SCHEME_PREFIX, PROVIDER_SCHEME_PREFIX,
};
use shared::{check_input_connections, write_if_some};
use shared::{check_input_credentials, concat_string };
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use url::Url;

create_bitset!(
    u16,
    ConfigInputFlags,
    XtreamSkipLive,
    XtreamSkipVod,
    XtreamSkipSeries,
    XtreamLiveStreamUsePrefix,
    XtreamLiveStreamWithoutExtension,
    ResolveTmdb,
    ResolveBackground,
    ResolveSeries,
    ResolveVod,
    ProbeSeries,
    ProbeVod,
    ProbeLive
);




#[derive(Debug, Clone)]
pub struct ConfigInputOptions {
    pub flags: ConfigInputFlagsSet,
    pub resolve_delay: u16,
    pub probe_delay: u16,
    pub probe_live_interval_hours: u32,
    pub resolve_filter: Option<Filter>,
    pub probe_filter: Option<Filter>,
}

macros::from_impl!(ConfigInputOptions);
impl ConfigInputOptions {
    #[inline]
    pub fn has_flag(&self, flag: ConfigInputFlags) -> bool {
        self.flags.contains(flag)
    }

    #[inline]
    pub fn has_any_flags(&self, flags: ConfigInputFlagsSet) -> bool {
        self.flags.contains_any(&flags)
    }

    #[inline]
    pub fn has_all_flags(&self, flags: ConfigInputFlagsSet) -> bool {
        self.flags.contains_all(&flags)
    }

    #[inline]
    pub fn defaults() -> &'static Self { &DEFAULT_CONFIG_INPUT_OPTIONS }
}
impl From<&ConfigInputOptionsDto> for ConfigInputOptions {
    fn from(dto: &ConfigInputOptionsDto) -> Self {
        let mut flags = ConfigInputFlagsSet::new();
        apply_flags!(
            dto, flags, ConfigInputFlags;
            (xtream_skip_live, XtreamSkipLive),
            (xtream_skip_vod, XtreamSkipVod),
            (xtream_skip_series, XtreamSkipSeries),
            (xtream_live_stream_use_prefix, XtreamLiveStreamUsePrefix),
            (xtream_live_stream_without_extension, XtreamLiveStreamWithoutExtension),
            (resolve_tmdb, ResolveTmdb),
            (resolve_background, ResolveBackground),
            (resolve_series, ResolveSeries),
            (resolve_vod, ResolveVod),
            (probe_series, ProbeSeries),
            (probe_vod, ProbeVod),
            (probe_live, ProbeLive)
        );

        Self {
            flags,
            resolve_delay: dto.resolve_delay,
            probe_delay: dto.probe_delay,
            probe_live_interval_hours: dto.probe_live_interval_hours,
            resolve_filter: dto.t_resolve_filter.clone(),
            probe_filter: dto.t_probe_filter.clone(),
        }
    }
}

static DEFAULT_CONFIG_INPUT_OPTIONS: LazyLock<ConfigInputOptions> =
    LazyLock::new(|| ConfigInputOptions::from(&ConfigInputOptionsDto::default()));

#[derive(Debug, Clone)]
pub struct MediaServerInputConfig {
    pub libraries: Vec<MediaServerLibrarySelector>,
    pub catalog: MediaServerCatalogConfigDto,
    pub playback: MediaServerPlaybackConfigDto,
    pub image_policy: MediaServerImagePolicy,
    pub token: Option<String>,
    pub api_key: Option<String>,
    pub user_id: Option<String>,
    pub account_token: Option<String>,
    pub server_id: Option<String>,
    pub server_name: Option<String>,
    pub prefer_https: bool,
    pub allow_relay: bool,
}

#[derive(Debug, Clone)]
pub struct ConfigInputStaged {
    pub for_input: Option<Arc<str>>,
    pub clusters: ClusterFlags,
}

impl From<&ConfigInputStagedDto> for ConfigInputStaged {
    fn from(dto: &ConfigInputStagedDto) -> Self {
        Self {
            for_input: dto.for_input.clone(),
            clusters: dto.clusters,
        }
    }
}

impl From<&MediaServerInputConfigDto> for MediaServerInputConfig {
    fn from(dto: &MediaServerInputConfigDto) -> Self {
        let mut normalized = dto.clone();
        normalized.normalize();
        Self {
            libraries: normalized.libraries,
            catalog: normalized.catalog,
            playback: normalized.playback,
            image_policy: normalized.image_policy,
            token: normalized.token,
            api_key: normalized.api_key,
            user_id: normalized.user_id,
            account_token: normalized.account_token,
            server_id: normalized.server_id,
            server_name: normalized.server_name,
            prefer_https: normalized.prefer_https,
            allow_relay: normalized.allow_relay,
        }
    }
}

impl MediaServerInputConfig {
    pub fn has_any_emby_jellyfin_auth(&self) -> bool {
        is_non_blank_optional_string(&self.token) || is_non_blank_optional_string(&self.api_key)
    }

    pub fn has_any_plex_token(&self) -> bool {
        is_non_blank_optional_string(&self.account_token) || is_non_blank_optional_string(&self.token)
    }

    pub fn has_plex_server_selector(&self) -> bool {
        is_non_blank_optional_string(&self.server_id) || is_non_blank_optional_string(&self.server_name)
    }
}

pub struct InputUserInfo {
    pub base_url: String,
    pub username: String,
    pub password: String,
}

impl InputUserInfo {
    pub fn new(input_type: InputType, username: Option<&str>, password: Option<&str>, input_url: &str) -> Option<Self> {
        if input_type == InputType::Xtream {
            if let (Some(username), Some(password)) = (username, password) {
                return Some(Self {
                    base_url: input_url.to_string(),
                    username: username.to_owned(),
                    password: password.to_owned(),
                });
            }
        } else if let Ok(url) = Url::parse(input_url) {
            let base_url = url.origin().ascii_serialization();
            let (username, password) = get_credentials_from_url(&url);
            if username.is_some() || password.is_some() {
                if let (Some(username), Some(password)) = (username.as_ref(), password.as_ref()) {
                    return Some(Self {
                        base_url,
                        username: username.to_owned(),
                        password: password.to_owned(),
                    });
                }
            }
        }
        None
    }
}

#[derive(Debug, Clone)]
pub struct ConfigInputAlias {
    pub id: u16,
    pub name: Arc<str>,
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub priority: i16,
    pub max_connections: u16,
    pub exp_date: Option<i64>,
    pub enabled: bool,
}

macros::from_impl!(ConfigInputAlias);
impl From<&ConfigInputAliasDto> for ConfigInputAlias {
    fn from(dto: &ConfigInputAliasDto) -> Self {
        Self {
            id: dto.id,
            name: dto.name.clone(),
            url: dto.url.clone(),
            username: dto.username.clone(),
            password: dto.password.clone(),
            priority: dto.priority,
            max_connections: dto.max_connections,
            exp_date: dto.exp_date,
            enabled: dto.enabled,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConfigInput {
    pub id: u16,
    pub name: Arc<str>,
    pub input_type: InputType,
    pub headers: HashMap<String, String>,
    pub url: String,
    pub epg: Option<EpgConfig>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub persist: Option<String>,
    pub enabled: bool,
    pub options: Option<ConfigInputOptions>,
    pub media_server: Option<MediaServerInputConfig>,
    pub aliases: Option<Vec<ConfigInputAlias>>,
    pub priority: i16,
    pub max_connections: u16,
    pub method: InputFetchMethod,
    pub staged_type: StagedInputType,
    pub staged: Option<ConfigInputStaged>,
    pub exp_date: Option<i64>,
    pub t_batch_url: Option<String>,
    pub panel_api: Option<PanelApiConfig>,
    pub cache_duration_seconds: u64,
    pub provider_configs: Option<Vec<Arc<ConfigProvider>>>,
}

impl ConfigInput {
    fn resolve_provider_config(url: &str, provider_configs: &[Arc<ConfigProvider>]) -> Result<Arc<ConfigProvider>, TuliproxError> {
        let (host, _path) = parse_provider_scheme_url_parts(url).map_err(|err| {
            TuliproxError::ConfigInput(format!(
                "Malformed provider URL {}: {}",
                sanitize_sensitive_info(url),
                sanitize_sensitive_info(&err.to_string())
            ))
        })?;

        provider_configs
            .iter()
            .find(|p| p.name.as_ref() == host)
            .cloned()
            .ok_or_else(|| TuliproxError::ConfigInput(format!("Failed to resolve provider config for {}", sanitize_sensitive_info(url))))
    }

    fn prepare_aliases(
        &mut self,
        provider_configs: &[Arc<ConfigProvider>],
        used_provider_configs: &mut Vec<Arc<ConfigProvider>>,
    ) -> Result<(), TuliproxError> {
        if let Some(aliases) = &mut self.aliases {
            for alias in aliases {
                if is_input_expired(alias.exp_date) {
                    warn!(
                        "Account {} expired for provider: {}",
                        alias.username.as_ref().map_or("?", |s| s.as_str()),
                        alias.name
                    );
                    alias.enabled = false;
                }

                if alias.url.starts_with(PROVIDER_SCHEME_PREFIX) {
                    let provider_cfg = Self::resolve_provider_config(&alias.url, provider_configs)?;
                    if !used_provider_configs.iter().any(|p| p.name == provider_cfg.name) {
                        used_provider_configs.push(provider_cfg);
                    }
                }
            }
        }

        Ok(())
    }

    fn apply_expiration(&mut self) {
        if is_input_expired(self.exp_date) {
            warn!(
                "Account {} expired for provider: {}",
                self.username.as_ref().map_or("?", |s| s.as_str()),
                self.name
            );
            self.enabled = false;
        }
    }

    #[inline]
    pub fn get_download_input_type(&self) -> InputType {
        self.input_type
    }

    pub fn resolve_staged_download_type(&mut self) {
        self.input_type = self.staged_type.input_type();

        // For m3u inputs credentials may live in the URL itself.
        let (username, password) = get_credentials_from_url_str(&self.url);
        if username.is_some() {
            self.username = username;
        }
        if password.is_some() {
            self.password = password;
        }
    }

    #[inline]
    pub fn has_flag(&self, flag: ConfigInputFlags) -> bool {
        self.has_flag_or(flag, false)
    }

    #[inline]
    /// Returns `default` when `self.options` is `None`; unlike `has_flag`, which returns
    /// `false` for missing options. For `ConfigInput::default()` without `prepare()`, use
    /// this `_or` variant when an explicit fallback is required.
    pub fn has_flag_or(&self, flag: ConfigInputFlags, default: bool) -> bool {
        self.options.as_ref().map_or(default, |o| o.has_flag(flag))
    }

    #[inline]
    pub fn has_any_flags(&self, flags: ConfigInputFlagsSet) -> bool {
        self.has_any_flags_or(flags, false)
    }

    #[inline]
    /// Returns `default` when `self.options` is `None`; unlike `has_any_flags`, which returns
    /// `false` for missing options. For `ConfigInput::default()` without `prepare()`, use
    /// this `_or` variant when an explicit fallback is required.
    pub fn has_any_flags_or(&self, flags: ConfigInputFlagsSet, default: bool) -> bool {
        self.options.as_ref().map_or(default, |o| o.has_any_flags(flags))
    }

    #[inline]
    pub fn has_all_flags(&self, flags: ConfigInputFlagsSet) -> bool {
        self.has_all_flags_or(flags, false)
    }

    #[inline]
    /// Returns `default` when `self.options` is `None`; unlike `has_all_flags`, which returns
    /// `false` for missing options. For `ConfigInput::default()` without `prepare()`,
    /// prefer this `_or` variant when an explicit fallback is required.
    pub fn has_all_flags_or(&self, flags: ConfigInputFlagsSet, default: bool) -> bool {
        self.options.as_ref().map_or(default, |o| o.has_all_flags(flags))
    }

    fn prepare_media_server_input(&self) -> Result<(), TuliproxError> {
        if !self.input_type.is_media_server() {
            return Ok(());
        }

        let trimmed_url = self.url.trim();
        if trimmed_url.starts_with(BATCH_SCHEME_PREFIX) || trimmed_url.starts_with(PROVIDER_SCHEME_PREFIX) {
            return Err(TuliproxError::ConfigInput(format!(
                "media-server input does not support batch:// or provider:// URLs (input: {})",
                self.name
            )));
        }
        if self.aliases.as_ref().is_some_and(|aliases| !aliases.is_empty()) {
            return Err(TuliproxError::ConfigInput(format!(
                "media-server input does not support aliases (input: {})",
                self.name
            )));
        }
        if self.epg.is_some() {
            return Err(TuliproxError::ConfigInput(format!(
                "media-server input does not support EPG configuration (input: {})",
                self.name
            )));
        }
        if self.panel_api.is_some() {
            return Err(TuliproxError::ConfigInput(format!(
                "media-server input does not support panel_api configuration (input: {})",
                self.name
            )));
        }
        let Some(media_server) = self.media_server.as_ref() else {
            return Err(TuliproxError::ConfigInput(format!(
                "media_server configuration is mandatory for input type {} (input: {})",
                self.input_type, self.name
            )));
        };
        if media_server.libraries.is_empty() {
            return Err(TuliproxError::ConfigInput(format!(
                "media-server input requires at least one selected library (input: {})",
                self.name
            )));
        }
        if media_server.libraries.iter().any(MediaServerLibrarySelector::is_empty) {
            return Err(TuliproxError::ConfigInput(format!(
                "media_server library selectors must not be empty (input: {})",
                self.name
            )));
        }
        if media_server.catalog.page_size == 0 {
            return Err(TuliproxError::ConfigInput(format!(
                "media server catalog page_size must be greater than zero (input: {})",
                self.name
            )));
        }
        match self.input_type {
            InputType::Emby | InputType::Jellyfin => {
                if trimmed_url.is_empty() {
                    return Err(TuliproxError::ConfigInput(format!(
                        "url is mandatory for input type {} (input: {})",
                        self.input_type, self.name
                    )));
                }
                let has_login = self.username.as_ref().is_some_and(|u| !u.trim().is_empty())
                    && self.password.as_ref().is_some_and(|p| !p.trim().is_empty());
                if !media_server.has_any_emby_jellyfin_auth() && !has_login {
                    return Err(TuliproxError::ConfigInput(format!(
                        "media-server input type {} requires media_server token/api_key or username/password bootstrap credentials (input: {})",
                        self.input_type, self.name
                    )));
                }
            }
            InputType::Plex => {
                if trimmed_url.is_empty() {
                    if !is_non_blank_optional_string(&media_server.account_token) {
                        return Err(TuliproxError::ConfigInput(format!(
                            "media-server input type plex without input.url requires media_server.account_token for MyPlex discovery (input: {})",
                            self.name
                        )));
                    }
                    if !media_server.has_plex_server_selector() {
                        return Err(TuliproxError::ConfigInput(format!(
                            "media-server input type plex requires a server selector such as media_server.server_id or media_server.server_name when input.url is omitted (input: {})",
                            self.name
                        )));
                    }
                } else if !is_non_blank_optional_string(&media_server.token) {
                    return Err(TuliproxError::ConfigInput(format!(
                        "media-server input type plex with input.url requires media_server.token for direct PMS access (input: {})",
                        self.name
                    )));
                }
            }
            InputType::M3u | InputType::Xtream | InputType::M3uBatch | InputType::XtreamBatch | InputType::Library | InputType::Staged => {}
        }

        Ok(())
    }

    pub fn prepare(&mut self, provider_configs: &[Arc<ConfigProvider>]) -> Result<Option<PathBuf>, TuliproxError> {
        // Defensive fallback: From<&ConfigInputDto> for ConfigInput sets options, but ConfigInput can
        // still be built via Default::default(), batch/internal/test paths, so prepare() normalizes
        // missing options with ConfigInputOptions::defaults().
        if self.options.is_none() {
            self.options = Some(ConfigInputOptions::defaults().clone());
        }

        // For batch definitions, validate root URL/credentials before alias promotion in prepare_batch().
        if self.enabled && self.input_type.is_batch() {
            check_input_credentials!(self, self.input_type, false, false);
            check_input_connections!(self, self.input_type, false);
        }

        let mut used_provider_configs: Vec<Arc<ConfigProvider>> = vec![];
        let batch_file_path = self.prepare_batch();
        self.name = self.name.trim().intern();

        if self.enabled {
            self.prepare_media_server_input()?;
        }

        if self.url.starts_with(PROVIDER_SCHEME_PREFIX) {
            let provider_cfg = Self::resolve_provider_config(&self.url, provider_configs)?;
            used_provider_configs.push(provider_cfg);
        }

        if self.enabled {
            check_input_credentials!(self, self.input_type, false, false);
            check_input_connections!(self, self.input_type, false);
            self.apply_expiration();
            self.prepare_aliases(provider_configs, &mut used_provider_configs)?;

            if !used_provider_configs.is_empty() {
                self.provider_configs = Some(used_provider_configs);
            }

            if let Some(panel_api) = &mut self.panel_api {
                panel_api.prepare()?;
            }
        }
        Ok(batch_file_path)
    }

    pub fn get_user_info(&self) -> Option<InputUserInfo> {
        InputUserInfo::new(self.input_type, self.username.as_deref(), self.password.as_deref(), &self.url)
    }

    pub fn get_matched_config_by_url<'a>(&'a self, url: &str) -> Option<(&'a str, Option<&'a String>, Option<&'a String>)> {
        if url.starts_with(&self.url) {
            return Some((&self.url, self.username.as_ref(), self.password.as_ref()));
        }

        if let Some(aliases) = &self.aliases {
            for alias in aliases {
                if url.starts_with(&alias.url) {
                    return Some((&alias.url, alias.username.as_ref(), alias.password.as_ref()));
                }
            }
        }
        None
    }

    fn prepare_batch(&mut self) -> Option<PathBuf> {
        if self.input_type.is_batch() {
            let input_type = if self.input_type == InputType::M3uBatch {
                InputType::M3u
            } else {
                InputType::Xtream
            };

            self.t_batch_url = Some(self.url.clone());
            let file_path = get_csv_file_path(self.url.as_str()).ok();
            if self.enabled {
                if let Some(aliases) = self.aliases.as_mut() {
                    if !aliases.is_empty() {
                        for alias in aliases.iter_mut() {
                            if is_input_expired(alias.exp_date) {
                                alias.enabled = false;
                                warn!("Alias-Account {} expired for provider: {}", alias.username.as_ref().map_or("?", |s| s.as_str()), alias.name);
                            }
                        }

                        if let Some(index) = aliases.iter().position(|alias| alias.enabled) {
                            let mut first = aliases.remove(index);
                            self.id = first.id;
                            self.username = first.username.take();
                            self.password = first.password.take();
                            self.url = first.url.trim().to_string();
                            self.max_connections = first.max_connections;
                            self.priority = first.priority;
                            self.enabled = first.enabled;
                            self.exp_date = first.exp_date;
                            if self.name.is_empty() {
                                self.name.clone_from(&first.name);
                            }
                        } else {
                            self.enabled = false;
                        }
                    }
                }
            }

            self.input_type = input_type;
            file_path
        } else {
            None
        }
    }

    pub fn as_input(&self, alias: &ConfigInputAlias) -> ConfigInput {
        ConfigInput {
            id: alias.id,
            name: alias.name.clone(),
            input_type: self.input_type,
            headers: self.headers.clone(),
            url: alias.url.clone(),
            epg: self.epg.clone(),
            username: alias.username.clone(),
            password: alias.password.clone(),
            persist: self.persist.clone(),
            enabled: self.enabled,
            options: self.options.clone(),
            media_server: self.media_server.clone(),
            aliases: None,
            priority: alias.priority,
            max_connections: alias.max_connections,
            method: self.method,
            staged_type: self.staged_type,
            staged: None,
            exp_date: None,
            t_batch_url: None,
            panel_api: self.panel_api.clone(),
            cache_duration_seconds: self.cache_duration_seconds,
            provider_configs: self.provider_configs.clone(),
        }
    }

    pub fn has_enabled_aliases(&self) -> bool {
        self.aliases
            .as_ref()
            .is_some_and(|aliases| aliases.iter().any(|a| a.enabled))
    }

    pub fn get_enabled_aliases(&self) -> Option<Vec<&ConfigInputAlias>> {
        self.aliases.as_ref().and_then(|aliases| {
            let result: Vec<_> = aliases.iter().filter(|alias| alias.enabled).collect();
            if result.is_empty() {
                None
            } else {
                Some(result)
            }
        })
    }

    pub fn resolve_url<'a>(&self, url: &'a str) -> Result<Cow<'a, str>, TuliproxError> {
        if !url.starts_with(PROVIDER_SCHEME_PREFIX) {
            return Ok(Cow::Borrowed(url));
        }

        let (host, _path) = parse_provider_scheme_url_parts(url)?;

        let provider_config = self.provider_configs
            .as_ref()
            .and_then(|configs| configs.iter().find(|p| p.name.as_ref() == host))
            .cloned();

        if let Some(provider) = provider_config {
            let (_, resolved) = resolve_provider_scheme_url_with_provider(url, Some(provider))?;
            Ok(resolved)
        } else {
            Err(TuliproxError::ConfigInput(format!("Provider config for '{}' not found in input '{}'", host, self.name)))
        }
    }

    pub fn resolve(&self) -> Result<Cow<'_, str>, TuliproxError> {
        self.resolve_url(&self.url)
    }

    pub fn get_resolve_provider(&self, url: &str) -> Option<Arc<ConfigProvider>> {
        if !url.starts_with(PROVIDER_SCHEME_PREFIX) {
            return None;
        }
        if let Some(provider) = self.provider_configs.as_ref() {
            if let Ok((host, _path)) = parse_provider_scheme_url_parts(url) {
                return provider.iter().find(|pc| pc.name.as_ref() == host).cloned();
            }
        }
        None
    }
}

macros::from_impl!(ConfigInput);
impl From<&ConfigInputDto> for ConfigInput {
    fn from(dto: &ConfigInputDto) -> Self {
        let options = dto
            .options
            .as_ref()
            .map_or_else(|| ConfigInputOptions::defaults().clone(), ConfigInputOptions::from);

        Self {
            id: dto.id,
            name: dto.name.clone(),
            input_type: dto.input_type,
            headers: dto.headers.clone(),
            url: dto.url.clone(),
            epg: dto.epg.as_ref().map(EpgConfig::from),
            username: dto.username.clone(),
            password: dto.password.clone(),
            persist: dto.persist.clone(),
            enabled: dto.enabled,
            options: Some(options),
            media_server: dto.media_server.as_ref().map(MediaServerInputConfig::from),
            aliases: dto.aliases.as_ref().map(|list| list.iter().map(ConfigInputAlias::from).collect()),
            priority: dto.priority,
            max_connections: dto.max_connections,
            method: dto.method,
            staged_type: dto.staged_type,
            exp_date: dto.exp_date,
            staged: dto.staged.as_ref().map(ConfigInputStaged::from),
            t_batch_url: None,
            panel_api: dto.panel_api.as_ref().map(PanelApiConfig::from),
            cache_duration_seconds: dto.cache_duration_seconds,
            provider_configs: None,
        }
    }
}

impl fmt::Display for ConfigInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ConfigInput: {{")?;
        write!(f, "  id: {}", self.id)?;
        write!(f, ", name: {}", self.name)?;
        write!(f, ", input_type: {:?}", self.input_type)?;
        write!(f, ", url: {}", self.url)?;
        write!(f, ", enabled: {}", self.enabled)?;
        write!(f, ", priority: {}", self.priority)?;
        write!(f, ", max_connections: {}", self.max_connections)?;
        write!(f, ", method: {:?}", self.method)?;

        // headers, epg etc. unchanged

        write_if_some!(f, self,
            ", username: " => username,
            ", password: " => password,
            ", persist: " => persist
        );
        write!(f, " }}")?;

        Ok(())
    }
}


pub fn is_input_expired(exp_date: Option<i64>) -> bool {
    match exp_date {
        Some(ts) => {
            let now = Utc::now().timestamp();
            ts <= now
        }
        None => false,
    }
}

/// Resolves a custom "provider://" URL using a pre-provided provider configuration.
/// If the URL does not use the custom scheme, it returns the original URL.
pub fn resolve_provider_scheme_url_with_provider(
    stream_url: &str,
    provider_config: Option<Arc<ConfigProvider>>,
) -> Result<(Option<Arc<ConfigProvider>>, Cow<'_, str>), TuliproxError> {
    resolve_provider_scheme_url_with_provider_index(stream_url, provider_config, 0)
}

pub fn resolve_provider_scheme_url_with_provider_index(
    stream_url: &str,
    provider_config: Option<Arc<ConfigProvider>>,
    provider_url_index: usize,
) -> Result<(Option<Arc<ConfigProvider>>, Cow<'_, str>), TuliproxError> {
    if !stream_url.starts_with(PROVIDER_SCHEME_PREFIX) {
        return Ok((None, Cow::Borrowed(stream_url)));
    }

    let (_host, path_and_query) = parse_provider_scheme_url_parts(stream_url)?;

    let provider = provider_config.ok_or_else(|| {
        TuliproxError::ConfigInput(format!("Provider config missing for resolution of: '{}'", sanitize_sensitive_info(stream_url)))
    })?;

    let final_url = assemble_provider_url_at_index(&provider, path_and_query, provider_url_index)?;
    Ok((Some(provider), Cow::Owned(final_url)))
}

fn assemble_provider_url_at_index(
    provider: &ConfigProvider,
    path_and_query: &str,
    provider_url_index: usize,
) -> Result<String, TuliproxError> {
    let base = provider
        .urls
        .get(provider_url_index)
        .or_else(|| provider.urls.first())
        .ok_or_else(|| TuliproxError::ConfigInput(format!("Provider '{}' has no URLs available", provider.name)))?;

    // Add http:// scheme if no scheme is present
    let base_with_scheme = if base.contains("://") {
        base.to_string()
    } else {
        concat_string!("http://", base)
    };

    let mut final_url = base_with_scheme.trim_end_matches('/').to_string();
    if !path_and_query.is_empty() {
        final_url.push_str(path_and_query);
    }
    Ok(final_url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ConfigProvider;
    use shared::model::{ConfigProviderDto, MediaServerInputConfigDto, MediaServerLibrarySelector, ProviderUrlSelectionPolicy};
    use std::borrow::Cow;
    use std::sync::Arc;

    fn media_server_config_with_library() -> MediaServerInputConfig {
        MediaServerInputConfig::from(&MediaServerInputConfigDto {
            libraries: vec![MediaServerLibrarySelector::Name("Movies".to_string())],
            ..MediaServerInputConfigDto::default()
        })
    }

    #[test]
    fn media_server_runtime_mapping_preserves_safe_defaults() {
        let dto = ConfigInputDto {
            name: "emby_media_server".into(),
            input_type: InputType::Emby,
            url: "https://media.example.invalid".to_string(),
            media_server: Some(MediaServerInputConfigDto {
                token: Some(" token ".to_string()),
                api_key: Some(" api-key ".to_string()),
                user_id: Some(" user ".to_string()),
                account_token: Some(" account-token ".to_string()),
                server_id: Some(" server ".to_string()),
                server_name: Some(" server-name ".to_string()),
                ..MediaServerInputConfigDto {
                    libraries: vec![MediaServerLibrarySelector::Name(" Movies ".to_string())],
                    ..MediaServerInputConfigDto::default()
                }
            }),
            ..ConfigInputDto::default()
        };

        let input = ConfigInput::from(&dto);
        let media_server = input.media_server.expect("media_server config should map to runtime");

        assert_eq!(media_server.libraries, vec![MediaServerLibrarySelector::Name("Movies".to_string())]);
        assert_eq!(media_server.catalog.page_size, 100);
        assert!(media_server.playback.direct_play_only);
        assert!(!media_server.playback.allow_transcode);
        assert_eq!(media_server.token.as_deref(), Some("token"));
        assert_eq!(media_server.api_key.as_deref(), Some("api-key"));
        assert_eq!(media_server.user_id.as_deref(), Some("user"));
        assert_eq!(media_server.account_token.as_deref(), Some("account-token"));
        assert_eq!(media_server.server_id.as_deref(), Some("server"));
        assert_eq!(media_server.server_name.as_deref(), Some("server-name"));
    }

    #[test]
    fn prepare_accepts_plex_media_server_without_input_url() {
        let mut input = ConfigInput {
            name: "plex_media_server".into(),
            input_type: InputType::Plex,
            media_server: Some(MediaServerInputConfig {
                account_token: Some("token".to_string()),
                server_id: Some("server".to_string()),
                ..media_server_config_with_library()
            }),
            enabled: true,
            ..Default::default()
        };

        input.prepare(&[]).expect("plex discovery config should prepare without input.url");
    }

    #[test]
    fn prepare_accepts_plex_media_server_with_direct_url_without_selector() {
        let mut input = ConfigInput {
            name: "plex_media_server".into(),
            input_type: InputType::Plex,
            url: "https://plex.example.invalid".to_string(),
            media_server: Some(MediaServerInputConfig {
                token: Some("token".to_string()),
                ..media_server_config_with_library()
            }),
            enabled: true,
            ..Default::default()
        };

        input.prepare(&[]).expect("direct Plex URL should not require MyPlex server selector");
    }

    #[test]
    fn prepare_rejects_plex_direct_url_without_server_token() {
        let mut input = ConfigInput {
            name: "plex_media_server".into(),
            input_type: InputType::Plex,
            url: "https://plex.example.invalid".to_string(),
            media_server: Some(MediaServerInputConfig {
                account_token: Some("account-token".to_string()),
                ..media_server_config_with_library()
            }),
            enabled: true,
            ..Default::default()
        };

        let err = input.prepare(&[]).expect_err("direct Plex URL requires the PMS server token");
        assert!(err.to_string().contains("requires media_server.token"));
    }

    #[test]
    fn prepare_rejects_plex_discovery_without_account_token() {
        let mut input = ConfigInput {
            name: "plex_media_server".into(),
            input_type: InputType::Plex,
            media_server: Some(MediaServerInputConfig {
                token: Some("server-token".to_string()),
                server_id: Some("server".to_string()),
                ..media_server_config_with_library()
            }),
            enabled: true,
            ..Default::default()
        };

        let err = input.prepare(&[]).expect_err("Plex discovery requires the MyPlex account token");
        assert!(err.to_string().contains("requires media_server.account_token"));
    }

    #[test]
    fn prepare_rejects_emby_media_server_without_input_url() {
        let mut input = ConfigInput {
            name: "emby_media_server".into(),
            input_type: InputType::Emby,
            media_server: Some(MediaServerInputConfig {
                token: Some("token".to_string()),
                ..media_server_config_with_library()
            }),
            enabled: true,
            ..Default::default()
        };

        let err = input.prepare(&[]).expect_err("emby media_server input should require a direct server URL");
        assert!(err.to_string().contains("url is mandatory for input type emby"));
    }

    #[test]
    fn prepare_rejects_blank_media_server_credentials_and_selectors() {
        let mut emby = ConfigInput {
            name: "emby_media_server".into(),
            input_type: InputType::Emby,
            url: "https://media.example.invalid".to_string(),
            media_server: Some(MediaServerInputConfig {
                token: Some("   ".to_string()),
                api_key: Some(String::new()),
                ..media_server_config_with_library()
            }),
            enabled: true,
            ..Default::default()
        };
        let err = emby.prepare(&[]).expect_err("blank token/api_key should be rejected");
        assert!(err.to_string().contains("requires media_server token/api_key"));

        let mut plex = ConfigInput {
            name: "plex_media_server".into(),
            input_type: InputType::Plex,
            media_server: Some(MediaServerInputConfig {
                account_token: Some("   ".to_string()),
                server_id: Some("   ".to_string()),
                ..media_server_config_with_library()
            }),
            enabled: true,
            ..Default::default()
        };
        let err = plex.prepare(&[]).expect_err("blank plex token should be rejected");
        assert!(err.to_string().contains("requires media_server.account_token"));
    }

    #[test]
    fn prepare_rejects_blank_media_server_library_selector() {
        let mut input = ConfigInput {
            name: "emby_media_server".into(),
            input_type: InputType::Emby,
            url: "https://media.example.invalid".to_string(),
            media_server: Some(MediaServerInputConfig {
                token: Some("token".to_string()),
                libraries: vec![MediaServerLibrarySelector::Name("   ".to_string())],
                ..media_server_config_with_library()
            }),
            enabled: true,
            ..Default::default()
        };

        let err = input.prepare(&[]).expect_err("blank library selector should be rejected");
        assert!(err.to_string().contains("media_server library selectors must not be empty"));
    }

    #[test]
    fn prepare_accepts_media_server_max_connections_as_stream_limit() {
        let mut input = ConfigInput {
            name: "emby_media_server".into(),
            input_type: InputType::Emby,
            url: "https://media.example.invalid".to_string(),
            media_server: Some(MediaServerInputConfig {
                token: Some("token".to_string()),
                ..media_server_config_with_library()
            }),
            max_connections: 1,
            enabled: true,
            ..Default::default()
        };

        input.prepare(&[]).expect("media_server inputs reuse max_connections stream-limit semantics");
    }

    #[test]
    fn prepare_rejects_media_server_provider_url() {
        let mut input = ConfigInput {
            name: "emby_media_server".into(),
            input_type: InputType::Emby,
            url: " provider://media-server ".to_string(),
            media_server: Some(MediaServerInputConfig {
                token: Some("token".to_string()),
                ..media_server_config_with_library()
            }),
            enabled: true,
            ..Default::default()
        };

        let err = input.prepare(&[]).expect_err("media_server provider URLs should be rejected");
        assert!(err.to_string().contains("does not support batch:// or provider://"));
    }

    #[test]
    fn test_resolve_url_normal() {
        let input = ConfigInput {
            url: "http://example.com/stream".to_string(),
            ..Default::default()
        };
        let resolved = input.resolve_url("http://example.com/stream").unwrap();
        assert_eq!(resolved, "http://example.com/stream");
        assert!(matches!(resolved, Cow::Borrowed(_)));
    }

    #[test]
    fn test_resolve_url_provider() {
        let provider = ConfigProvider::from(&ConfigProviderDto {
            name: "myprovider".into(),
            urls: vec!["http://provider.com".into()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
            dns: None,
        });
        let input = ConfigInput {
            name: "test_input".into(),
            provider_configs: Some(vec![Arc::new(provider)]),
            ..Default::default()
        };

        let resolved = input.resolve_url("provider://myprovider/stream").unwrap();
        assert_eq!(resolved, "http://provider.com/stream");
        assert!(matches!(resolved, Cow::Owned(_)));
    }

    #[test]
    fn test_resolve_url_provider_stream_path_requires_provider_name() {
        let provider = ConfigProvider::from(&ConfigProviderDto {
            name: "myprovider".into(),
            urls: vec!["http://provider.com".into()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
            dns: None,
        });
        let input = ConfigInput {
            name: "test_input".into(),
            provider_configs: Some(vec![Arc::new(provider)]),
            ..Default::default()
        };

        let err = input.resolve_url("provider://live/user/pass/813294.ts").unwrap_err();

        assert!(err.to_string().contains("Provider config for 'live' not found"));
    }

    #[test]
    fn test_resolve_provider_scheme_url_starts_from_first_url_for_new_resolution() {
        let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
            name: "myprovider".into(),
            urls: vec!["http://provider-a.example".into(), "http://provider-b.example".into()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
            dns: None,
        }));
        let _ = provider.rotate_to_next_url_with_cycle_check(0);
        assert_eq!(provider.get_current_index(), 1);

        let (_provider, resolved) =
            resolve_provider_scheme_url_with_provider("provider://myprovider/stream", Some(Arc::clone(&provider)))
                .expect("provider url should resolve");

        assert_eq!(resolved, "http://provider-a.example/stream");
    }

    #[test]
    fn test_resolve_url_provider_missing() {
        let input = ConfigInput {
            name: "test_input".into(),
            provider_configs: Some(vec![]),
            ..Default::default()
        };

        let err = input.resolve_url("provider://myprovider/stream").unwrap_err();
        assert!(err.to_string().contains("Provider config for 'myprovider' not found"));
    }

    #[test]
    fn test_resolve_default() {
        let input = ConfigInput {
            url: "http://example.com/stream".to_string(),
            ..Default::default()
        };
        let resolved = input.resolve().unwrap();
        assert_eq!(resolved, "http://example.com/stream");
    }

    #[test]
    fn test_prepare_fails_on_malformed_provider_url_in_main_input() {
        let mut input = ConfigInput {
            name: "test_input".into(),
            input_type: InputType::M3u,
            url: "provider:///bad".to_string(),
            enabled: false,
            ..Default::default()
        };

        let err = input.prepare(&[]).unwrap_err();
        assert!(err.to_string().contains("Malformed provider URL"));
    }

    #[test]
    fn test_prepare_fails_on_malformed_provider_url_in_alias() {
        let mut input = ConfigInput {
            name: "test_input".into(),
            input_type: InputType::M3u,
            url: "http://example.com/playlist.m3u".to_string(),
            enabled: true,
            aliases: Some(vec![ConfigInputAlias {
                id: 1,
                name: "alias".into(),
                url: "provider:///bad".to_string(),
                username: None,
                password: None,
                priority: 0,
                max_connections: 0,
                exp_date: None,
                enabled: true,
            }]),
            ..Default::default()
        };

        let err = input.prepare(&[]).unwrap_err();
        assert!(err.to_string().contains("Malformed provider URL"));
    }

    #[test]
    fn test_get_download_input_type_returns_input_type() {
        let input = ConfigInput { input_type: InputType::Xtream, ..Default::default() };
        assert_eq!(input.get_download_input_type(), InputType::Xtream);
    }

    #[test]
    fn test_resolve_staged_download_type_does_not_inherit_provider_connection_profile() {
        let mut staged = ConfigInput {
            name: "staged_a".into(),
            input_type: InputType::Staged,
            url: "http://staged.example".to_string(),
            ..Default::default()
        };

        staged.resolve_staged_download_type();

        assert_eq!(staged.input_type, InputType::M3u);
        assert_eq!(staged.username, None);
        assert_eq!(staged.password, None);
        assert!(staged.headers.is_empty());
        assert!(InputFetchMethod::is_default(&staged.method));
        assert_eq!(staged.url, "http://staged.example");
    }

    #[test]
    fn test_resolve_staged_download_type_preserves_xtream_staged_type() {
        let mut staged = ConfigInput {
            name: "staged_a".into(),
            input_type: InputType::Staged,
            staged_type: StagedInputType::Xtream,
            url: "http://staged.example".to_string(),
            ..Default::default()
        };

        staged.resolve_staged_download_type();

        assert_eq!(staged.input_type, InputType::Xtream);
    }

    #[test]
    fn test_resolve_staged_download_type_keeps_own_credentials() {
        let mut staged = ConfigInput {
            name: "staged_a".into(),
            input_type: InputType::Staged,
            url: "http://staged.example".to_string(),
            username: Some("own_user".to_string()),
            password: Some("own_pass".to_string()),
            ..Default::default()
        };

        staged.resolve_staged_download_type();

        assert_eq!(staged.username.as_deref(), Some("own_user"));
        assert_eq!(staged.password.as_deref(), Some("own_pass"));
    }

    #[test]
    fn test_resolve_staged_download_type_m3u_extracts_url_credentials() {
        let mut staged = ConfigInput {
            name: "staged_a".into(),
            input_type: InputType::Staged,
            url: "http://staged.example/get.php?username=urluser&password=urlpass".to_string(),
            ..Default::default()
        };

        staged.resolve_staged_download_type();

        assert_eq!(staged.input_type, InputType::M3u);
        assert_eq!(staged.username.as_deref(), Some("urluser"));
        assert_eq!(staged.password.as_deref(), Some("urlpass"));
    }

    #[test]
    fn test_prepare_xtream_batch_requires_root_url_even_with_aliases() {
        let mut input = ConfigInput {
            name: "xtream_batch_missing_root_url".into(),
            input_type: InputType::XtreamBatch,
            url: String::new(),
            enabled: true,
            aliases: Some(vec![ConfigInputAlias {
                id: 1,
                name: "alias".into(),
                url: "http://alias.example".to_string(),
                username: Some("alias_user".to_string()),
                password: Some("alias_pass".to_string()),
                priority: 0,
                max_connections: 0,
                exp_date: None,
                enabled: true,
            }]),
            ..Default::default()
        };

        let err = input
            .prepare(&[])
            .expect_err("prepare must require root URL even when aliases are attached directly");
        assert!(err.to_string().contains("url for input is mandatory"), "Error: {err}");
        assert!(err.to_string().contains("xtream_batch_missing_root_url"), "Error: {err}");
    }

    #[test]
    fn test_prepare_xtream_batch_requires_root_credentials_for_non_batch_url() {
        let mut input = ConfigInput {
            name: "xtream_batch_missing_root_creds".into(),
            input_type: InputType::XtreamBatch,
            url: "http://root.example".to_string(),
            enabled: true,
            aliases: Some(vec![ConfigInputAlias {
                id: 1,
                name: "alias".into(),
                url: "http://alias.example".to_string(),
                username: Some("alias_user".to_string()),
                password: Some("alias_pass".to_string()),
                priority: 0,
                max_connections: 0,
                exp_date: None,
                enabled: true,
            }]),
            ..Default::default()
        };

        let err = input
            .prepare(&[])
            .expect_err("prepare must require root credentials for non-batch xtream-batch URL");
        assert!(err.to_string().contains("xtream-batch without batch:// URL"), "Error: {err}");
        assert!(err.to_string().contains("xtream_batch_missing_root_creds"), "Error: {err}");
    }

    #[test]
    fn test_prepare_xtream_batch_rejects_root_credentials_for_batch_url() {
        let mut input = ConfigInput {
            name: "xtream_batch_root_creds_not_allowed".into(),
            input_type: InputType::XtreamBatch,
            url: "batch:///tmp/aliases.csv".to_string(),
            username: Some("root_user".to_string()),
            password: Some("root_pass".to_string()),
            enabled: true,
            ..Default::default()
        };

        let err = input
            .prepare(&[])
            .expect_err("prepare must reject root credentials for batch:// xtream-batch URL");
        assert!(err.to_string().contains("with batch:// URL should not define username or password"), "Error: {err}");
        assert!(err.to_string().contains("xtream_batch_root_creds_not_allowed"), "Error: {err}");
    }
}
