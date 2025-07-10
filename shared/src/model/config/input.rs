use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::str::FromStr;
use enum_iterator::Sequence;
use crate::{check_input_credentials, create_tuliprox_error_result, handle_tuliprox_error_result_list, info_err};
use crate::error::{TuliproxError, TuliproxErrorKind};
use crate::model::{EpgConfigDto};
use crate::utils::{default_as_true, get_base_url_from_str, get_credentials_from_url_str, get_trimmed_string, sanitize_sensitive_info, trim_last_slash};
use log::debug;


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
}


impl InputType {
    const M3U: &'static str = "m3u";
    const XTREAM: &'static str = "xtream";
    const M3U_BATCH: &'static str = "m3u_batch";
    const XTREAM_BATCH: &'static str = "xtream_batch";
}

impl Display for InputType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", match self {
            Self::M3u => Self::M3U,
            Self::Xtream => Self::XTREAM,
            Self::M3uBatch => Self::M3U_BATCH,
            Self::XtreamBatch => Self::XTREAM_BATCH,
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
        } else {
            create_tuliprox_error_result!(TuliproxErrorKind::Info, "Unknown InputType: {}", s)
        }
    }
}

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, Sequence, PartialEq, Eq, Default)]
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
            create_tuliprox_error_result!(TuliproxErrorKind::Info, "Unknown Fetch Method: {}", s)
        }
    }
}


#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ConfigInputOptionsDto {
    #[serde(default)]
    pub xtream_skip_live: bool,
    #[serde(default)]
    pub xtream_skip_vod: bool,
    #[serde(default)]
    pub xtream_skip_series: bool,
    #[serde(default = "default_as_true")]
    pub xtream_live_stream_use_prefix: bool,
    #[serde(default)]
    pub xtream_live_stream_without_extension: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ConfigInputAliasDto {
    #[serde(skip)]
    pub id: u16,
    pub name: String,
    pub url: String,
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default)]
    pub priority: i16,
    #[serde(default)]
    pub max_connections: u16,
}

impl ConfigInputAliasDto {
    pub fn prepare(&mut self, index: u16, input_type: &InputType) -> Result<(), TuliproxError> {
        self.id = index;
        self.name = self.name.trim().to_string();
        if self.name.is_empty() {
            return Err(info_err!("name for input is mandatory".to_string()));
        }
        self.url = self.url.trim().to_string();
        if self.url.is_empty() {
            return Err(info_err!("url for input is mandatory".to_string()));
        }
        self.username = get_trimmed_string(&self.username);
        self.password = get_trimmed_string(&self.password);
        check_input_credentials!(self, input_type, true);

        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ConfigInputDto {
    #[serde(skip)]
    pub id: u16,
    pub name: String,
    #[serde(default, rename = "type")]
    pub input_type: InputType,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epg: Option<EpgConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persist: Option<String>,
    #[serde(default = "default_as_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<ConfigInputOptionsDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aliases: Option<Vec<ConfigInputAliasDto>>,
    #[serde(default)]
    pub priority: i16,
    #[serde(default)]
    pub max_connections: u16,
    #[serde(default)]
    pub method: InputFetchMethod,
}

impl ConfigInputDto {
    #[allow(clippy::cast_possible_truncation)]
    pub fn prepare(&mut self, index: u16, include_computed: bool) -> Result<u16, TuliproxError> {
        self.id = index;
        self.check_url()?;

        self.name = self.name.trim().to_string();
        if self.name.is_empty() {
            return Err(info_err!("name for input is mandatory".to_string()));
        }

        self.username = get_trimmed_string(&self.username);
        self.password = get_trimmed_string(&self.password);
        check_input_credentials!(self, self.input_type, true);
        self.persist = get_trimmed_string(&self.persist);

        if let Some(epg) = self.epg.as_mut() {
            let create_auto_url = || {
                let (username, password) = if self.username.is_none() || self.password.is_none() {
                    get_credentials_from_url_str(&self.url)
                } else {
                    (self.username.clone(), self.password.clone())
                };

                if username.is_none() || password.is_none() {
                    Err(format!("auto_epg is enabled for input {}, but no credentials could be extracted", self.name))
                } else {
                    let base_url = get_base_url_from_str(&self.url);
                    if base_url.is_some() {
                        let provider_epg_url = format!("{}/xmltv.php?username={}&password={}",
                                                       trim_last_slash(&base_url.unwrap_or_default()),
                                                       username.unwrap_or_default(),
                                                       password.unwrap_or_default());
                        Ok(provider_epg_url)
                    } else {
                        Err(format!("auto_epg is enabled for input {}, but url could not be parsed {}", self.name, sanitize_sensitive_info(&self.url)))
                    }
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

        if let Some(aliases) = self.aliases.as_mut() {
            let input_type = &self.input_type;
            handle_tuliprox_error_result_list!(TuliproxErrorKind::Info, aliases.iter_mut().enumerate().map(|(idx, i)| i.prepare(index+1+(idx as u16), input_type)));
        }
        Ok(index + self.aliases.as_ref().map_or(0, std::vec::Vec::len) as u16)
    }

    fn check_url(&mut self) -> Result<(), TuliproxError> {
        self.url = self.url.trim().to_string();
        if self.url.is_empty() {
            return Err(info_err!("url for input is mandatory".to_string()));
        }
        Ok(())
    }


}