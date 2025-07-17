use crate::model::{macros, EpgConfig};
use crate::utils;
use log::debug;
use shared::{apply_batch_aliases, check_input_credentials};
use shared::error::{TuliproxError, TuliproxErrorKind};
use shared::info_err;
use shared::model::{ConfigInputAliasDto, ConfigInputDto, ConfigInputOptionsDto, InputFetchMethod, InputType};
use shared::utils::get_credentials_from_url_str;
use shared::utils::{get_base_url_from_str, get_credentials_from_url};
use std::collections::HashMap;
use std::path::PathBuf;
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

#[derive(Debug, Clone)]
pub struct ConfigInputAlias {
    pub id: u16,
    pub name: String,
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub priority: i16,
    pub max_connections: u16,
}

macros::from_impl!(ConfigInputAlias);
impl From<&ConfigInputAliasDto> for ConfigInputAlias {
    fn from(dto: &ConfigInputAliasDto) -> Self {
        Self {
            id: dto.id,
            name: dto.name.to_string(),
            url: get_base_url_from_str(&dto.url).map_or_else(|| dto.url.to_string(), |base_url| base_url),
            username: dto.username.clone(),
            password: dto.password.clone(),
            priority: dto.priority,
            max_connections: dto.max_connections,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConfigInput {
    pub id: u16,
    pub name: String,
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
}

impl ConfigInput {
    pub fn prepare(&mut self) -> Result<Option<PathBuf>, TuliproxError> {
        let batch_file_path = self.prepare_batch()?;
        check_input_credentials!(self, self.input_type, false);
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

    fn prepare_batch(&mut self) -> Result<Option<PathBuf>, TuliproxError> {
        if self.input_type == InputType::M3uBatch || self.input_type == InputType::XtreamBatch {
            let input_type = if self.input_type == InputType::M3uBatch {
                InputType::M3u
            } else {
                InputType::Xtream
            };
            if let Some((file_path, batch_aliases)) = get_batch_aliases(self.input_type, self.url.as_str())? {
                let mut aliases: Vec<ConfigInputAlias> = batch_aliases.into_iter()
                    .map(ConfigInputAlias::from)
                    .collect();
                if let Some(mut first) = aliases.pop() {
                    self.username = first.username.take();
                    self.password = first.password.take();
                    self.url = first.url.trim().to_string();
                    self.max_connections = first.max_connections;
                    self.priority = first.priority;
                    if self.name.is_empty() {
                        self.name = first.name.to_string();
                    }
                }
                apply_batch_aliases!(self, aliases);
                self.input_type = input_type;
                return Ok(Some(file_path));
            }
            self.input_type = input_type;
        }
        Ok(None)
    }
}

macros::from_impl!(ConfigInput);
impl From<&ConfigInputDto> for ConfigInput {
    fn from(dto: &ConfigInputDto) -> Self {
        Self {
            id: dto.id,
            name: dto.name.to_string(),
            input_type: dto.input_type,
            headers: dto.headers.clone(),
            url: dto.url.clone(), //get_base_url_from_str(&dto.url).map_or_else(|| dto.url.to_string(), |base_url| base_url),
            epg: dto.epg.as_ref().map(std::convert::Into::into),
            username: dto.username.clone(),
            password: dto.password.clone(),
            persist: dto.persist.clone(),
            enabled: dto.enabled,
            options: dto.options.as_ref().map(std::convert::Into::into),
            aliases: dto.aliases.as_ref().map(|list| list.iter().map(std::convert::Into::into).collect()),
            priority: dto.priority,
            max_connections: dto.max_connections,
            method: dto.method,
        }
    }
}

pub fn get_batch_aliases(input_type: InputType, url: &str) -> Result<Option<(PathBuf, Vec<ConfigInputAliasDto>)>, TuliproxError> {
    if input_type == InputType::M3uBatch || input_type == InputType::XtreamBatch {
        return match utils::csv_read_inputs(input_type, url) {
            Ok((file_path, mut batch_aliases)) => {
                if !batch_aliases.is_empty() {
                    batch_aliases.reverse();
                }
                Ok(Some((file_path, batch_aliases)))
            }
            Err(err) => {
                Err(TuliproxError::new(TuliproxErrorKind::Info, err.to_string()))
            }
        }
    }
    Ok(None)
}