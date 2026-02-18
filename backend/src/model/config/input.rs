use crate::model::{macros, ConfigProvider, EpgConfig, PanelApiConfig};
use crate::repository::get_csv_file_path;
use chrono::Utc;
use log::warn;
use shared::{apply_flags, create_bitset};
use shared::error::TuliproxError;
use shared::model::{ConfigInputAliasDto, ConfigInputDto, ConfigInputOptionsDto, InputFetchMethod, InputType, StagedInputDto};
use shared::utils::{get_credentials_from_url, parse_provider_scheme_url_parts, sanitize_sensitive_info, Internable, PROVIDER_SCHEME_PREFIX};
use shared::{check_input_connections, info_err_res, write_if_some};
use shared::{check_input_credentials, concat_string, info_err};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
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

impl ConfigInputFlagsSet {
    #[inline]
    pub fn contains_any(&self, other: Self) -> bool {
        (self.0 & other.0) != 0
    }

    #[inline]
    pub fn contains_all(&self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

impl std::ops::BitOr for ConfigInputFlags {
    type Output = ConfigInputFlagsSet;

    #[inline]
    fn bitor(self, rhs: Self) -> Self::Output {
        ConfigInputFlagsSet::from_variants(&[self, rhs])
    }
}

impl std::ops::BitOr<ConfigInputFlags> for ConfigInputFlagsSet {
    type Output = Self;

    #[inline]
    fn bitor(mut self, rhs: ConfigInputFlags) -> Self::Output {
        self.set(rhs);
        self
    }
}

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
        self.flags.contains_any(flags)
    }

    #[inline]
    pub fn has_all_flags(&self, flags: ConfigInputFlagsSet) -> bool {
        self.flags.contains_all(flags)
    }
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

#[derive(Debug, Clone, Default)]
pub struct StagedInput {
    pub name: Arc<str>,
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub method: InputFetchMethod,
    pub input_type: InputType,
    pub headers: HashMap<String, String>,
    /// Provider configuration for failover support when using `provider://` scheme.
    pub provider_config: Option<Arc<ConfigProvider>>,
}

macros::from_impl!(StagedInput);
impl From<&StagedInputDto> for StagedInput {
    fn from(dto: &StagedInputDto) -> Self {
        Self {
            name: dto.name.clone(),
            input_type: dto.input_type,
            url: dto.url.clone(),
            username: dto.username.clone(),
            password: dto.password.clone(),
            method: dto.method,
            headers: dto.headers.clone(),
            provider_config: None, // Resolved later in ConfigInput::prepare()
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
    #[inline]
    pub fn has_flag(&self, flag: ConfigInputFlags) -> bool {
        self.options.as_ref().is_some_and(|o| o.has_flag(flag))
    }

    #[inline]
    pub fn has_flag_or(&self, flag: ConfigInputFlags, default: bool) -> bool {
        self.options.as_ref().map_or(default, |o| o.has_flag(flag))
    }

    #[inline]
    pub fn has_any_flags(&self, flags: ConfigInputFlagsSet) -> bool {
        self.options.as_ref().is_some_and(|o| o.has_any_flags(flags))
    }

    #[inline]
    pub fn has_any_flags_or(&self, flags: ConfigInputFlagsSet, default: bool) -> bool {
        self.options.as_ref().map_or(default, |o| o.has_any_flags(flags))
    }

    #[inline]
    pub fn has_all_flags(&self, flags: ConfigInputFlagsSet) -> bool {
        self.options.as_ref().is_some_and(|o| o.has_all_flags(flags))
    }

    #[inline]
    pub fn has_all_flags_or(&self, flags: ConfigInputFlagsSet, default: bool) -> bool {
        self.options.as_ref().map_or(default, |o| o.has_all_flags(flags))
    }

    pub fn prepare(&mut self, provider_configs: &[Arc<ConfigProvider>]) -> Result<Option<PathBuf>, TuliproxError> {
        let mut used_provider_configs: Vec<Arc<ConfigProvider>> = vec![];
        let batch_file_path = self.prepare_batch();
        self.name = self.name.trim().intern();

        let resolve_provider_config = |url: &str| -> Result<Arc<ConfigProvider>, TuliproxError> {
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
        };

        if self.url.starts_with(PROVIDER_SCHEME_PREFIX) {
            let provider_cfg = resolve_provider_config(&self.url)?;
            used_provider_configs.push(provider_cfg);
        }

        if self.enabled {
            check_input_credentials!(self, self.input_type, false, false);
            check_input_connections!(self, self.input_type, false);
            if let Some(staged_input) = &mut self.staged {
                if staged_input.url.starts_with(PROVIDER_SCHEME_PREFIX) {
                    let provider_cfg = resolve_provider_config(&staged_input.url)?;
                    staged_input.provider_config = Some(provider_cfg.clone());
                    used_provider_configs.push(provider_cfg);
                }

                check_input_credentials!(staged_input, staged_input.input_type, false, true);
                if !matches!(staged_input.input_type, InputType::M3u | InputType::Xtream) {
                    return info_err_res!("Staged input can only be from type m3u or xtream");
                }
            }

            if is_input_expired(self.exp_date) {
                warn!("Account {} expired for provider: {}", self.username.as_ref().map_or("?", |s| s.as_str()), self.name);
                self.enabled = false;
            }

            if let Some(aliases) = &mut self.aliases {
                for alias in aliases {
                    if is_input_expired(alias.exp_date) {
                        warn!("Account {} expired for provider: {}", alias.username.as_ref().map_or("?", |s| s.as_str()), alias.name);
                        alias.enabled = false;
                    }

                    if alias.url.starts_with(PROVIDER_SCHEME_PREFIX) {
                        let provider_cfg = resolve_provider_config(&alias.url)?;
                        if !used_provider_configs.iter().any(|p| p.name == provider_cfg.name) {
                            used_provider_configs.push(provider_cfg);
                        }
                    }
                }
            }

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
            options: dto.options.as_ref().map(ConfigInputOptions::from),
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
    use std::borrow::Cow;
    use std::sync::atomic::AtomicUsize;
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
        let provider = ConfigProvider {
            name: "myprovider".into(),
            urls: vec!["http://provider.com".into()],
            current_url_index: AtomicUsize::new(0),
        };
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
}
