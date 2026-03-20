use crate::model::{macros, ConfigProvider, EpgConfig, PanelApiConfig};
use crate::repository::get_csv_file_path;
use chrono::Utc;
use log::warn;
use shared::{apply_flags, create_bitset};
use shared::error::TuliproxError;
use shared::model::{ClusterSource, ConfigInputAliasDto, ConfigInputDto, ConfigInputOptionsDto, InputFetchMethod, InputType, StagedInputDto, XtreamCluster};
use shared::utils::{get_credentials_from_url, parse_provider_scheme_url_parts, sanitize_sensitive_info, Internable, PROVIDER_SCHEME_PREFIX};
use shared::{check_input_connections, info_err_res, write_if_some};
use shared::{check_input_credentials, concat_string, info_err};
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
        }
    }
}

static DEFAULT_CONFIG_INPUT_OPTIONS: LazyLock<ConfigInputOptions> =
    LazyLock::new(|| ConfigInputOptions::from(&ConfigInputOptionsDto::default()));

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
pub struct StagedInput {
    pub enabled: bool,
    pub name: Arc<str>,
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub method: InputFetchMethod,
    pub input_type: InputType,
    pub headers: HashMap<String, String>,
    /// Provider configuration for failover support when using `provider://` scheme.
    pub provider_config: Option<Arc<ConfigProvider>>,
    pub live_source: ClusterSource,
    pub vod_source: ClusterSource,
    pub series_source: ClusterSource,
    pub cluster_sources_configured: bool,
}

impl Default for StagedInput {
    fn default() -> Self {
        Self {
            enabled: false,
            name: Arc::default(),
            url: String::new(),
            username: None,
            password: None,
            method: InputFetchMethod::default(),
            input_type: InputType::default(),
            headers: HashMap::default(),
            provider_config: None,
            live_source: ClusterSource::Staged,
            vod_source: ClusterSource::Input,
            series_source: ClusterSource::Input,
            cluster_sources_configured: false,
        }
    }
}

impl StagedInput {
    /// Returns the `ClusterSource` for the given `XtreamCluster`.
    pub fn get_cluster_source(&self, cluster: XtreamCluster) -> ClusterSource {
        match cluster {
            XtreamCluster::Live => self.live_source,
            XtreamCluster::Video => self.vod_source,
            XtreamCluster::Series => self.series_source,
        }
    }
}

macros::from_impl!(StagedInput);
impl From<&StagedInputDto> for StagedInput {
    fn from(dto: &StagedInputDto) -> Self {
        let resolve = |opt: Option<ClusterSource>, default: ClusterSource| -> ClusterSource {
            opt.unwrap_or(default)
        };
        let (live_default, vod_default, series_default) = if dto.input_type.is_m3u() {
            (ClusterSource::Staged, ClusterSource::Input, ClusterSource::Input)
        } else {
            (ClusterSource::Staged, ClusterSource::Staged, ClusterSource::Staged)
        };

        Self {
            enabled: dto.enabled,
            name: dto.name.clone(),
            input_type: dto.input_type,
            url: dto.url.clone(),
            username: dto.username.clone(),
            password: dto.password.clone(),
            method: dto.method,
            headers: dto.headers.clone(),
            provider_config: None, // Resolved later in ConfigInput::prepare()
            live_source: resolve(dto.live_source, live_default),
            vod_source: resolve(dto.vod_source, vod_default),
            series_source: resolve(dto.series_source, series_default),
            cluster_sources_configured: dto.live_source.is_some() || dto.vod_source.is_some() || dto.series_source.is_some(),
        }
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
    pub aliases: Option<Vec<ConfigInputAlias>>,
    pub priority: i16,
    pub max_connections: u16,
    pub method: InputFetchMethod,
    pub staged: Option<StagedInput>,
    pub exp_date: Option<i64>,
    pub t_batch_url: Option<String>,
    pub panel_api: Option<PanelApiConfig>,
    pub cache_duration_seconds: u64,
    pub provider_configs: Option<Vec<Arc<ConfigProvider>>>,
}

impl ConfigInput {
    fn resolve_provider_config(url: &str, provider_configs: &[Arc<ConfigProvider>]) -> Result<Arc<ConfigProvider>, TuliproxError> {
        let (host, _path) = parse_provider_scheme_url_parts(url).map_err(|err| {
            info_err!(
                "Malformed provider URL {}: {}",
                sanitize_sensitive_info(url),
                sanitize_sensitive_info(err.to_string().as_str())
            )
        })?;

        provider_configs
            .iter()
            .find(|p| p.name.as_ref() == host)
            .cloned()
            .ok_or_else(|| info_err!("Failed to resolve provider config for {}", sanitize_sensitive_info(url)))
    }

    fn prepare_staged_input(
        &mut self,
        provider_configs: &[Arc<ConfigProvider>],
        used_provider_configs: &mut Vec<Arc<ConfigProvider>>,
        skip_live: bool,
        skip_vod: bool,
        skip_series: bool,
    ) -> Result<(), TuliproxError> {
        if let Some(staged_input) = &mut self.staged {
            if staged_input.enabled {
                if staged_input.url.starts_with(PROVIDER_SCHEME_PREFIX) {
                    let provider_cfg = Self::resolve_provider_config(&staged_input.url, provider_configs)?;
                    staged_input.provider_config = Some(provider_cfg.clone());
                    used_provider_configs.push(provider_cfg);
                }

                check_input_credentials!(staged_input, staged_input.input_type, false, true);
                if !matches!(staged_input.input_type, InputType::M3u | InputType::Xtream) {
                    return info_err_res!(
                        "Staged input can only be from type m3u or xtream (input: {}, staged: {})",
                        self.name,
                        staged_input.name
                    );
                }

                if self.input_type.is_xtream() {
                    let live_uses_staged = matches!(staged_input.live_source, ClusterSource::Staged) && !skip_live;
                    let vod_uses_staged = matches!(staged_input.vod_source, ClusterSource::Staged) && !skip_vod;
                    let series_uses_staged = matches!(staged_input.series_source, ClusterSource::Staged) && !skip_series;

                    if !live_uses_staged && !vod_uses_staged && !series_uses_staged {
                        return info_err_res!(
                            "Staged input is enabled but no cluster source uses 'staged'; set at least one of live_source/vod_source/series_source to 'staged' (input: {}, staged: {})",
                            self.name,
                            staged_input.name
                        );
                    }

                    if staged_input.input_type.is_m3u() && (vod_uses_staged || series_uses_staged) {
                        return info_err_res!(
                            "Staged M3U input cannot provide VOD or Series clusters; use 'input' or 'skip' (input: {}, staged: {})",
                            self.name,
                            staged_input.name
                        );
                    }
                }

                if self.input_type.is_m3u() {
                    if staged_input.cluster_sources_configured {
                        warn!(
                            "Input '{}': cluster source fields (live_source/vod_source/series_source) are ignored for M3U main inputs",
                            self.name
                        );
                    }
                    staged_input.live_source = ClusterSource::Staged;
                    staged_input.vod_source = ClusterSource::Staged;
                    staged_input.series_source = ClusterSource::Staged;
                    staged_input.cluster_sources_configured = false;
                }
            }
        }

        Ok(())
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
        self.staged
            .as_ref()
            .filter(|staged| staged.enabled)
            .map_or(self.input_type, |staged| staged.input_type)
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

    pub fn prepare(&mut self, provider_configs: &[Arc<ConfigProvider>]) -> Result<Option<PathBuf>, TuliproxError> {
        // Defensive fallback: From<&ConfigInputDto> for ConfigInput sets options, but ConfigInput can
        // still be built via Default::default(), batch/internal/test paths, so prepare() normalizes
        // missing options with ConfigInputOptions::defaults().
        if self.options.is_none() {
            self.options = Some(ConfigInputOptions::defaults().clone());
        }

        // For batch definitions, validate root URL/credentials before alias promotion in prepare_batch().
        if self.enabled && matches!(self.input_type, InputType::M3uBatch | InputType::XtreamBatch) {
            check_input_credentials!(self, self.input_type, false, false);
            check_input_connections!(self, self.input_type, false);
        }

        let mut used_provider_configs: Vec<Arc<ConfigProvider>> = vec![];
        let batch_file_path = self.prepare_batch();
        self.name = self.name.trim().intern();

        if self.url.starts_with(PROVIDER_SCHEME_PREFIX) {
            let provider_cfg = Self::resolve_provider_config(&self.url, provider_configs)?;
            used_provider_configs.push(provider_cfg);
        }

        if self.enabled {
            check_input_credentials!(self, self.input_type, false, false);
            check_input_connections!(self, self.input_type, false);
            let skip_live = self.has_flag(ConfigInputFlags::XtreamSkipLive);
            let skip_vod = self.has_flag(ConfigInputFlags::XtreamSkipVod);
            let skip_series = self.has_flag(ConfigInputFlags::XtreamSkipSeries);
            self.prepare_staged_input(provider_configs, &mut used_provider_configs, skip_live, skip_vod, skip_series)?;
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
        if matches!(self.input_type, InputType::M3uBatch | InputType::XtreamBatch) {
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
            aliases: None,
            priority: alias.priority,
            max_connections: alias.max_connections,
            method: self.method,
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
            info_err_res!("Provider config for '{}' not found in input '{}'", host, self.name)
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
            aliases: dto.aliases.as_ref().map(|list| list.iter().map(ConfigInputAlias::from).collect()),
            priority: dto.priority,
            max_connections: dto.max_connections,
            method: dto.method,
            exp_date: dto.exp_date,
            staged: dto.staged.as_ref().map(StagedInput::from),
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

        // headers, epg etc. wie gehabt…

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
    if !stream_url.starts_with(PROVIDER_SCHEME_PREFIX) {
        return Ok((None, Cow::Borrowed(stream_url)));
    }

    let (_host, path_and_query) = parse_provider_scheme_url_parts(stream_url)?;

    let provider = provider_config.ok_or_else(|| {
        info_err!("Provider config missing for resolution of: '{}'", sanitize_sensitive_info(stream_url))
    })?;

    let final_url = assemble_provider_url(&provider, path_and_query)?;
    Ok((Some(provider), Cow::Owned(final_url)))
}

/// Internal helper to build the final URL string
fn assemble_provider_url(provider: &ConfigProvider, path_and_query: &str) -> Result<String, TuliproxError> {
    let base = provider.get_current_url()
        .ok_or_else(|| info_err!("Provider '{}' has no URLs available", provider.name))?;

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
    use shared::model::ConfigProviderDto;
    use std::borrow::Cow;
    use std::sync::Arc;

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
    fn test_prepare_fails_on_malformed_provider_url_in_staged_input() {
        let mut input = ConfigInput {
            name: "test_input".into(),
            input_type: InputType::M3u,
            url: "http://example.com/playlist.m3u".to_string(),
            enabled: true,
            staged: Some(StagedInput {
                enabled: true,
                name: "staged".into(),
                input_type: InputType::M3u,
                url: "provider:///bad".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };

        let err = input.prepare(&[]).unwrap_err();
        assert!(err.to_string().contains("Malformed provider URL"));
    }

    #[test]
    fn test_prepare_ignores_disabled_staged_provider_url() {
        let mut input = ConfigInput {
            name: "test_input".into(),
            input_type: InputType::M3u,
            url: "http://example.com/playlist.m3u".to_string(),
            enabled: true,
            staged: Some(StagedInput {
                enabled: false,
                name: "staged".into(),
                input_type: InputType::M3u,
                url: "provider:///bad".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };

        input
            .prepare(&[])
            .expect("disabled staged input should not enforce provider URL validation");
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
    fn test_get_download_input_type_uses_staged_when_enabled() {
        let input = ConfigInput {
            input_type: InputType::Xtream,
            staged: Some(StagedInput {
                enabled: true,
                input_type: InputType::M3u,
                ..Default::default()
            }),
            ..Default::default()
        };

        assert_eq!(input.get_download_input_type(), InputType::M3u);
    }

    #[test]
    fn test_get_download_input_type_uses_primary_when_staged_disabled() {
        let input = ConfigInput {
            input_type: InputType::Xtream,
            staged: Some(StagedInput {
                enabled: false,
                input_type: InputType::M3u,
                ..Default::default()
            }),
            ..Default::default()
        };

        assert_eq!(input.get_download_input_type(), InputType::Xtream);
    }

    #[test]
    fn test_staged_from_dto_defaults_m3u() {
        let dto = StagedInputDto {
            input_type: InputType::M3u,
            ..StagedInputDto::default()
        };
        let staged = StagedInput::from(&dto);
        assert_eq!(staged.live_source, ClusterSource::Staged);
        assert_eq!(staged.vod_source, ClusterSource::Input);
        assert_eq!(staged.series_source, ClusterSource::Input);
    }

    #[test]
    fn test_staged_from_dto_defaults_xtream() {
        let dto = StagedInputDto {
            input_type: InputType::Xtream,
            ..StagedInputDto::default()
        };
        let staged = StagedInput::from(&dto);
        assert_eq!(staged.live_source, ClusterSource::Staged);
        assert_eq!(staged.vod_source, ClusterSource::Staged);
        assert_eq!(staged.series_source, ClusterSource::Staged);
    }

    #[test]
    fn test_staged_from_dto_explicit_overrides() {
        let dto = StagedInputDto {
            input_type: InputType::Xtream,
            live_source: Some(ClusterSource::Input),
            vod_source: Some(ClusterSource::Skip),
            series_source: Some(ClusterSource::Staged),
            ..StagedInputDto::default()
        };
        let staged = StagedInput::from(&dto);
        assert_eq!(staged.live_source, ClusterSource::Input);
        assert_eq!(staged.vod_source, ClusterSource::Skip);
        assert_eq!(staged.series_source, ClusterSource::Staged);
    }

    #[test]
    fn test_get_cluster_source() {
        let staged = StagedInput {
            live_source: ClusterSource::Staged,
            vod_source: ClusterSource::Input,
            series_source: ClusterSource::Skip,
            ..Default::default()
        };
        assert_eq!(staged.get_cluster_source(XtreamCluster::Live), ClusterSource::Staged);
        assert_eq!(staged.get_cluster_source(XtreamCluster::Video), ClusterSource::Input);
        assert_eq!(staged.get_cluster_source(XtreamCluster::Series), ClusterSource::Skip);
    }

    #[test]
    fn test_staged_from_dto_tracks_cluster_sources_configured_flag() {
        let dto = StagedInputDto {
            input_type: InputType::Xtream,
            live_source: Some(ClusterSource::Input),
            ..StagedInputDto::default()
        };
        let staged = StagedInput::from(&dto);
        assert!(staged.cluster_sources_configured);
    }

    #[test]
    fn test_prepare_m3u_main_input_ignores_cluster_sources() {
        let mut input = ConfigInput {
            name: "m3u_main".into(),
            input_type: InputType::M3u,
            url: "http://main.example/playlist.m3u".to_string(),
            enabled: true,
            staged: Some(StagedInput {
                enabled: true,
                input_type: InputType::Xtream,
                url: "http://staged.example".to_string(),
                username: Some("u".to_string()),
                password: Some("p".to_string()),
                live_source: ClusterSource::Input,
                vod_source: ClusterSource::Skip,
                series_source: ClusterSource::Input,
                cluster_sources_configured: true,
                ..StagedInput::default()
            }),
            ..Default::default()
        };

        input.prepare(&[]).expect("prepare should succeed");

        let staged = input.staged.expect("staged should exist");
        assert_eq!(staged.live_source, ClusterSource::Staged);
        assert_eq!(staged.vod_source, ClusterSource::Staged);
        assert_eq!(staged.series_source, ClusterSource::Staged);
        assert!(!staged.cluster_sources_configured);
    }

    #[test]
    fn test_prepare_enabled_staged_requires_at_least_one_staged_cluster_source() {
        let mut input = ConfigInput {
            name: "xtream_main".into(),
            input_type: InputType::Xtream,
            url: "http://main.example".to_string(),
            username: Some("main_user".to_string()),
            password: Some("main_pass".to_string()),
            enabled: true,
            staged: Some(StagedInput {
                enabled: true,
                input_type: InputType::Xtream,
                url: "http://staged.example".to_string(),
                username: Some("staged_user".to_string()),
                password: Some("staged_pass".to_string()),
                live_source: ClusterSource::Input,
                vod_source: ClusterSource::Skip,
                series_source: ClusterSource::Input,
                ..StagedInput::default()
            }),
            ..Default::default()
        };

        let err = input
            .prepare(&[])
            .expect_err("expected validation error for staged source selection");
        assert!(err.to_string().contains("no cluster source uses 'staged'"));
    }

    #[test]
    fn test_prepare_staged_m3u_rejects_vod_series_staged() {
        let mut input = ConfigInput {
            name: "xtream_main".into(),
            input_type: InputType::Xtream,
            url: "http://main.example".to_string(),
            username: Some("main_user".to_string()),
            password: Some("main_pass".to_string()),
            enabled: true,
            staged: Some(StagedInput {
                enabled: true,
                input_type: InputType::M3u,
                url: "http://staged.example/playlist.m3u".to_string(),
                vod_source: ClusterSource::Staged,
                ..StagedInput::default()
            }),
            ..Default::default()
        };

        let err = input
            .prepare(&[])
            .expect_err("expected staged M3U validation error for vod_source=staged");
        assert!(err.to_string().contains("Staged M3U input cannot provide VOD or Series"));
    }

    #[test]
    fn test_prepare_staged_m3u_vod_staged_allowed_when_vod_skipped() {
        let mut input = ConfigInput {
            name: "xtream_main".into(),
            input_type: InputType::Xtream,
            url: "http://main.example".to_string(),
            username: Some("main_user".to_string()),
            password: Some("main_pass".to_string()),
            enabled: true,
            options: Some(ConfigInputOptions::from(&ConfigInputOptionsDto {
                xtream_skip_vod: true,
                ..ConfigInputOptionsDto::default()
            })),
            staged: Some(StagedInput {
                enabled: true,
                input_type: InputType::M3u,
                url: "http://staged.example/playlist.m3u".to_string(),
                vod_source: ClusterSource::Staged,
                ..StagedInput::default()
            }),
            ..Default::default()
        };

        input
            .prepare(&[])
            .expect("staged M3U vod_source=staged is valid when VOD is skipped");
    }

    #[test]
    fn test_prepare_xtream_batch_requires_root_url_even_with_aliases() {
        let mut input = ConfigInput {
            name: "xtream_batch_missing_root_url".into(),
            input_type: InputType::XtreamBatch,
            url: "".to_string(),
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
