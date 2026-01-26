use super::PanelApiConfigDto;
use crate::error::{TuliproxError, TuliproxErrorKind};
use crate::model::EpgConfigDto;
use crate::utils::{arc_str_serde, default_as_true, deserialize_timestamp, get_credentials_from_url_str, get_trimmed_string,
                   is_false, is_true, is_zero_u16, sanitize_sensitive_info,
                   serialize_option_vec_flow_map_items, trim_last_slash};
use crate::utils::{is_blank_optional_string, Internable};
use crate::{check_input_connections, check_input_credentials, info_err_res};

use enum_iterator::Sequence;
use log::warn;
use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::str::FromStr;
use std::sync::Arc;

#[macro_export]
macro_rules! apply_batch_aliases {
    ($source:expr, $batch_aliases:expr, $index:expr) => {{
        if $batch_aliases.is_empty() {
            $source.aliases = None;
            None
        } else {
            if let Some(aliases) = $source.aliases.as_mut() {
                let mut names = aliases.iter().map(|a| a.name.clone()).collect::<std::collections::HashSet<Arc<str>>>();
                names.insert($source.name.clone());

                for alias in $batch_aliases.into_iter() {
                    if !names.contains(&alias.name) {
                        aliases.push(alias)
                    }
                }
            } else {
                $source.aliases = Some($batch_aliases);
            }
                if let Some(index) = $index {
                let mut idx = index + 1;
                // set to the same id as the first alias, because the first alias is copied into this input
                $source.id = idx;
                if let Some(aliases) = $source.aliases.as_mut() {
                    for alias in aliases {
                        idx += 1;
                        alias.id = idx;
                    }
                }
                Some(idx)
            } else {
                None
            }
        }
    }};
}

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, Sequence,
    PartialEq, Eq, Default)]
pub enum InputType {
    #[serde(rename = "m3u")]
    #[default]
    M3u,
    #[serde(rename = "xtream")]
    Xtream,
    #[serde(rename = "m3u_batch")]
    M3uBatch,
    #[serde(rename = "xtream_batch")]
    XtreamBatch,
    #[serde(rename = "library")]
    Library,
}


impl InputType {
    const M3U: &'static str = "m3u";
    const XTREAM: &'static str = "xtream";
    const M3U_BATCH: &'static str = "m3u_batch";
    const XTREAM_BATCH: &'static str = "xtream_batch";
    const LIBRARY: &'static str = "library";
}

impl Display for InputType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", match self {
            Self::M3u => Self::M3U,
            Self::Xtream => Self::XTREAM,
            Self::M3uBatch => Self::M3U_BATCH,
            Self::XtreamBatch => Self::XTREAM_BATCH,
            Self::Library => Self::LIBRARY,
        })
    }
}

impl FromStr for InputType {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        if s.eq(Self::M3U) {
            Ok(Self::M3u)
        } else if s.eq(Self::XTREAM) {
            Ok(Self::Xtream)
        } else if s.eq(Self::M3U_BATCH) {
            Ok(Self::M3uBatch)
        } else if s.eq(Self::XTREAM_BATCH) {
            Ok(Self::XtreamBatch)
        } else if s.eq(Self::LIBRARY) {
            Ok(Self::Library)
        } else {
            info_err_res!("Unknown InputType: {}", s)
        }
    }
}

#[derive(
    Debug,
    Copy,
    Clone,
    serde::Serialize,
    serde::Deserialize,
    Sequence,
    PartialEq,
    Eq,
    Default
)]
pub enum InputFetchMethod {
    #[default]
    GET,
    POST,
}

impl InputFetchMethod {
    const GET_METHOD: &'static str = "GET";
    const POST_METHOD: &'static str = "POST";
}

impl Display for InputFetchMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", match self {
            Self::GET => Self::GET_METHOD,
            Self::POST => Self::POST_METHOD,
        })
    }
}

impl FromStr for InputFetchMethod {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        if s.eq(Self::GET_METHOD) {
            Ok(Self::GET)
        } else if s.eq(Self::POST_METHOD) {
            Ok(Self::POST)
        } else {
            info_err_res!("Unknown Fetch Method: {}", s)
        }
    }
}


#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigInputOptionsDto {
    #[serde(default, skip_serializing_if = "is_false")]
    pub xtream_skip_live: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub xtream_skip_vod: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub xtream_skip_series: bool,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub xtream_live_stream_use_prefix: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub xtream_live_stream_without_extension: bool,
}

impl Default for ConfigInputOptionsDto {
    fn default() -> Self {
        ConfigInputOptionsDto {
            xtream_skip_live: false,
            xtream_skip_vod: false,
            xtream_skip_series: false,
            xtream_live_stream_use_prefix: default_as_true(),
            xtream_live_stream_without_extension: false,
        }
    }
}

impl ConfigInputOptionsDto {
    pub fn is_empty(&self) -> bool {
        !self.xtream_skip_live
            && !self.xtream_skip_vod
            && !self.xtream_skip_series
            && self.xtream_live_stream_use_prefix
            && !self.xtream_live_stream_without_extension
    }

    pub fn clean(&mut self) {
        self.xtream_skip_live = false;
        self.xtream_skip_vod = false;
        self.xtream_skip_series = false;
        self.xtream_live_stream_use_prefix = default_as_true();
        self.xtream_live_stream_without_extension = false;
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StagedInputDto {
    #[serde(with = "arc_str_serde")]
    pub name: Arc<str>,
    pub url: String,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub password: Option<String>,
    #[serde(default)]
    pub method: InputFetchMethod,
    #[serde(default, rename = "type")]
    pub input_type: InputType,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
}

impl StagedInputDto {
    pub fn is_empty(&self) -> bool {
        self.url.trim().is_empty()
            && self.username.as_ref().is_none_or(|u| u.trim().is_empty())
            && self.password.as_ref().is_none_or(|u| u.trim().is_empty())
            && self.method == InputFetchMethod::default()
            && self.input_type == InputType::default()
            && self.headers.is_empty()
    }

    pub fn clean(&mut self) {
        self.url = String::new();
        self.username = None;
        self.password = None;
        self.method = InputFetchMethod::default();
        self.input_type = InputType::default();
        self.headers.clear();
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigInputAliasDto {
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub id: u16,
    #[serde(with = "arc_str_serde")]
    pub name: Arc<str>,
    pub url: String,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub password: Option<String>,
    #[serde(default)]
    pub priority: i16,
    #[serde(default)]
    pub max_connections: u16,
    #[serde(default, deserialize_with = "deserialize_timestamp", skip_serializing_if = "Option::is_none")]
    pub exp_date: Option<i64>,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub enabled: bool,

}

impl ConfigInputAliasDto {
    pub fn prepare(&mut self, index: u16, input_type: &InputType) -> Result<u16, TuliproxError> {
        self.id = index + 1;
        self.name = self.name.trim().intern();
        if self.name.is_empty() {
            return info_err_res!("name for input is mandatory");
        }
        self.url = self.url.trim().to_string();
        if self.url.is_empty() {
            return info_err_res!("url for input is mandatory");
        }
        check_input_credentials!(self, input_type, true, true);
        check_input_connections!(self, input_type, true);

        Ok(self.id)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigInputDto {
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub id: u16,
    #[serde(with = "arc_str_serde")]
    pub name: Arc<str>,
    #[serde(default, rename = "type")]
    pub input_type: InputType,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epg: Option<EpgConfigDto>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub persist: Option<String>,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<ConfigInputOptionsDto>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub cache_duration: Option<String>,
    #[serde(skip)]
    pub cache_duration_seconds: u64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_option_vec_flow_map_items"
    )]
    pub aliases: Option<Vec<ConfigInputAliasDto>>,
    #[serde(default)]
    pub priority: i16,
    #[serde(default)]
    pub max_connections: u16,
    #[serde(default)]
    pub method: InputFetchMethod,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged: Option<StagedInputDto>,
    #[serde(
        default,
        deserialize_with = "deserialize_timestamp",
        skip_serializing_if = "Option::is_none"
    )]
    pub exp_date: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel_api: Option<PanelApiConfigDto>,
}

impl Default for ConfigInputDto {
    fn default() -> Self {
        ConfigInputDto {
            id: 0,
            name: "".intern(),
            input_type: InputType::default(),
            headers: HashMap::new(),
            url: String::new(),
            epg: None,
            username: None,
            password: None,
            persist: None,
            enabled: default_as_true(),
            options: None,
            cache_duration: None,
            cache_duration_seconds: 0,
            aliases: None,
            priority: 0,
            max_connections: 0,
            method: InputFetchMethod::default(),
            staged: None,
            exp_date: None,
            panel_api: None,
        }
    }
}

impl ConfigInputDto {
    #[allow(clippy::cast_possible_truncation)]
    pub fn prepare(&mut self, index: u16, _include_computed: bool) -> Result<u16, TuliproxError> {
        self.name = self.name.trim().intern();
        if self.name.is_empty() {
            return info_err_res!("name for input is mandatory");
        }

        if let Some(duration_str) = &self.cache_duration {
            self.cache_duration_seconds = self.parse_duration(duration_str)?;
        } else {
            self.cache_duration_seconds = 0;
        }

        check_input_credentials!(self, self.input_type, true, false);
        check_input_connections!(self, self.input_type, false);
        if let Some(staged_input) = self.staged.as_mut() {
            check_input_credentials!(staged_input, staged_input.input_type, true, true);
            if !matches!(staged_input.input_type, InputType::M3u | InputType::Xtream) {
                return info_err_res!("Staged input can only be of type m3u or xtream");
            }
        }

        self.persist = get_trimmed_string(self.persist.as_deref());

        let mut current_index = index + 1;
        self.id = current_index;
        if let Some(aliases) = self.aliases.as_mut() {
            let input_type = &self.input_type;
            for alias in aliases {
                current_index = alias.prepare(current_index, input_type)?;
            }
        }

        if let Some(panel_api) = self.panel_api.as_mut() {
            panel_api.prepare(&self.name)?;
        }

        Ok(current_index)
    }

    fn parse_duration(&self, duration_str: &str) -> Result<u64, TuliproxError> {
        Ok(match duration_str.parse::<u64>() {
            Ok(secs) => secs,
            Err(_) => {
                let len = duration_str.len();
                if len > 1 {
                    let (num_str, unit) = duration_str.split_at(len - 1);
                    match num_str.parse::<u64>() {
                        Ok(val) => match unit {
                            "s" => val,
                            "m" => val * 60,
                            "h" => val * 3600,
                            "d" => val * 86400,
                            _ => return info_err_res!("Invalid cache_duration unit in '{}': {}", self.name, unit),
                        },
                        Err(_) => return info_err_res!("Invalid cache_duration format in '{}': {}", self.name, duration_str),
                    }
                } else {
                    return info_err_res!("Invalid cache_duration format in '{}'", self.name);
                }
            }
        })
    }

    pub fn prepare_epg(&mut self, include_computed: bool) -> Result<(), TuliproxError> {
        if let Some(epg) = self.epg.as_mut() {
            if self.input_type == InputType::Library {
                warn!("EPG is not supported for library inputs {}, skipping", self.name);
                self.epg = None;
                return Ok(());
            }

            let create_auto_url = || {
                let get_creds = || {
                    if self.username.is_some() && self.password.is_some() {
                        return (self.username.clone(), self.password.clone(), Some(self.url.clone()));
                    }

                    let (u, p, r) = self.aliases
                        .as_ref()
                        .and_then(|aliases| aliases.first())
                        .map(|alias| (alias.username.clone(), alias.password.clone(), Some(alias.url.clone())))
                        .unwrap_or((None, None, None));

                    if u.is_some() && p.is_some() && r.is_some() {
                        return (u, p, r);
                    }

                    let (u, p) = get_credentials_from_url_str(&self.url);
                    if u.is_some() && p.is_some() {
                        return (u, p, Some(self.url.clone()));
                    }

                    self.aliases
                        .as_ref()
                        .and_then(|aliases| aliases.first())
                        .map(|alias| {
                            let (u, p) = get_credentials_from_url_str(alias.url.as_str());
                            (u, p, Some(alias.url.clone()))
                        })
                        .unwrap_or((None, None, None))
                };

                let (username, password, base_url) = get_creds();

                if username.is_none() || password.is_none() || base_url.is_none() {
                    Err(format!("auto_epg is enabled for input {}, but no credentials could be extracted", self.name))
                } else if base_url.is_some() {
                    let provider_epg_url = format!("{}/xmltv.php?username={}&password={}",
                                                   trim_last_slash(&base_url.unwrap_or_default()),
                                                   username.unwrap_or_default(),
                                                   password.unwrap_or_default());
                    Ok(provider_epg_url)
                } else {
                    Err(format!("auto_epg is enabled for input {}, but url could not be parsed {}", self.name, sanitize_sensitive_info(&self.url)))
                }
            };

            epg.prepare(create_auto_url, include_computed)?;
            epg.t_sources = {
                let mut seen_urls = HashSet::new();
                epg.t_sources
                    .drain(..)
                    .filter(|src| seen_urls.insert(src.url.clone()))
                    .collect()
            };
        }
        Ok(())
    }

    pub fn prepare_batch(&mut self, batch_aliases: Vec<ConfigInputAliasDto>, index: u16) -> Result<Option<u16>, TuliproxError> {
        let idx = apply_batch_aliases!(self, batch_aliases, Some(index));
        Ok(idx)
    }

    pub fn upsert_alias(&mut self, mut alias: ConfigInputAliasDto) -> Result<(), TuliproxError> {
        check_input_credentials!(alias, self.input_type, true, true);
        check_input_connections!(alias, self.input_type, true);
        let aliases = self.aliases.get_or_insert_with(Vec::new);
        if let Some(existing) = aliases.iter_mut().find(|a| a.id == alias.id) {
            *existing = alias;
        } else {
            aliases.push(alias);
        }
        Ok(())
    }

    pub fn update_account_expiration_date(&mut self, input_name: &Arc<str>, username: &str, exp_date: i64) -> Result<(), TuliproxError> {
        if &self.name == input_name {
            if let Some(input_username) = &self.username {
                if input_username == username {
                    self.exp_date = Some(exp_date);
                    return Ok(());
                }
            }
        }

        if let Some(aliases) = &mut self.aliases {
            if let Some(alias) = aliases
                .iter_mut()
                .find(|a| a.username.as_deref() == Some(username))
            {
                alias.exp_date = Some(exp_date);
                return Ok(());
            }
        }

        Err(TuliproxError::new(TuliproxErrorKind::Info, format!("No matching input or alias found for input '{input_name}' with username '{username}'")))
    }
}
