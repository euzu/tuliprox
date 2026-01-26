use crate::model::{macros, EpgConfig, PanelApiConfig};
use crate::repository::get_csv_file_path;
use chrono::Utc;
use log::warn;
use shared::check_input_credentials;
use shared::error::TuliproxError;
use shared::model::{ConfigInputAliasDto, ConfigInputDto, ConfigInputOptionsDto, InputFetchMethod, InputType, StagedInputDto};
use shared::utils::{get_credentials_from_url, Internable};
use shared::{check_input_connections, info_err_res, write_if_some};
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use url::Url;

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub struct ConfigInputOptions {
    pub xtream_skip_live: bool,
    pub xtream_skip_vod: bool,
    pub xtream_skip_series: bool,
    pub xtream_live_stream_use_prefix: bool,
    pub xtream_live_stream_without_extension: bool,
}

macros::from_impl!(ConfigInputOptions);
impl From<&ConfigInputOptionsDto> for ConfigInputOptions {
    fn from(dto: &ConfigInputOptionsDto) -> Self {
        Self {
            xtream_skip_live: dto.xtream_skip_live,
            xtream_skip_vod: dto.xtream_skip_vod,
            xtream_skip_series: dto.xtream_skip_series,
            xtream_live_stream_use_prefix: dto.xtream_live_stream_use_prefix,
            xtream_live_stream_without_extension: dto.xtream_live_stream_without_extension,
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
}

impl ConfigInput {
    pub fn prepare(&mut self) -> Result<Option<PathBuf>, TuliproxError> {
        let batch_file_path = self.prepare_batch();
        self.name = self.name.trim().intern();
        check_input_credentials!(self, self.input_type, false, false);
        check_input_connections!(self, self.input_type, false);
        if let Some(staged_input) = &mut self.staged {
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
            }
        }

        if let Some(panel_api) = &mut self.panel_api {
            panel_api.prepare()?;
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

            if let Some(aliases) = self.aliases.as_mut() {
                for alias in aliases.iter() {
                    if is_input_expired(alias.exp_date) {
                        warn!("Alias-Account {} expired for provider: {}", alias.username.as_ref().map_or("?", |s| s.as_str()), alias.name);
                    }
                }

                if !aliases.is_empty() {
                    if let Some(index) = aliases.iter().position(|alias| alias.enabled) {
                        let mut first = aliases.remove(index);
                        self.id = first.id;
                        self.username = first.username.take();
                        self.password = first.password.take();
                        self.url = first.url.trim().to_string();
                        self.max_connections = first.max_connections;
                        self.priority = first.priority;
                        self.enabled = first.enabled;
                        if self.name.is_empty() {
                            self.name.clone_from(&first.name);
                        }
                    } else {
                        self.enabled = false;
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
        }
    }

    pub fn has_enabled_aliases(&self) -> bool {
        self.aliases
            .as_ref()
            .is_some_and(|aliases| aliases.iter().any(|a| a.enabled))
    }

    pub fn get_enabled_aliases(&self) -> Option<Vec<&ConfigInputAlias>> {
        self.aliases.as_ref().map_or(None, |aliases| {
            let result: Vec<_> = aliases.iter().filter(|alias| alias.enabled).collect();
            if result.is_empty() {
                None
            } else {
                Some(result)
            }
        })
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
