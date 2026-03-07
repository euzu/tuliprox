use super::PanelApiConfigDto;
use crate::{
    check_input_connections, check_input_credentials,
    error::{TuliproxError, TuliproxErrorKind},
    info_err_res,
    model::EpgConfigDto,
    utils::{
        arc_str_serde, arc_str_vec_serde, default_as_true, default_probe_delay_secs, default_probe_live_interval,
        default_resolve_background, default_resolve_delay_secs, default_xtream_live_stream_use_prefix,
        deserialize_timestamp, get_credentials_from_url_str, get_trimmed_string, is_blank_optional_string,
        is_default_probe_delay_secs, is_default_probe_live_interval, is_default_resolve_delay_secs, is_false, is_true,
        is_zero_i16, is_zero_u16, parse_duration_seconds, parse_provider_scheme_url_parts, sanitize_sensitive_info,
        serialize_option_vec_flow_map_items, trim_last_slash, Internable, BATCH_SCHEME_PREFIX, PROVIDER_SCHEME_PREFIX,
    },
};
use enum_iterator::Sequence;
use log::warn;
use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
    net::IpAddr,
    str::FromStr,
    sync::Arc,
};

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

#[macro_export]
macro_rules! check_provider_scheme_url {
    ($url:expr, $provider_names:expr) => {
        if $url.starts_with(PROVIDER_SCHEME_PREFIX) {
            let (host, _path) = match parse_provider_scheme_url_parts(&$url) {
                Ok(parts) => parts,
                Err(err) => {
                    return info_err_res!(
                        "Malformed provider URL {}: {}",
                        sanitize_sensitive_info(&$url),
                        sanitize_sensitive_info(err.to_string().as_str())
                    );
                }
            };
            if !$provider_names.contains(host) {
                return info_err_res!("Provider name {host} is not defined");
            }
        }
    };
}

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, Sequence, PartialEq, Eq, Default)]
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
    pub fn is_xtream(&self) -> bool { matches!(self, Self::Xtream | Self::XtreamBatch) }
    pub fn is_m3u(&self) -> bool { matches!(self, Self::M3u | Self::M3uBatch) }

    pub fn is_library(&self) -> bool { matches!(self, Self::Library) }
}

impl Display for InputType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::M3u => Self::M3U,
                Self::Xtream => Self::XTREAM,
                Self::M3uBatch => Self::M3U_BATCH,
                Self::XtreamBatch => Self::XTREAM_BATCH,
                Self::Library => Self::LIBRARY,
            }
        )
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

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, Sequence, PartialEq, Eq, Default)]
pub enum InputFetchMethod {
    #[default]
    GET,
    POST,
}

impl InputFetchMethod {
    const GET_METHOD: &'static str = "GET";
    const POST_METHOD: &'static str = "POST";

    pub fn is_default(value: &InputFetchMethod) -> bool { matches!(value, Self::GET) }
}

impl Display for InputFetchMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::GET => Self::GET_METHOD,
                Self::POST => Self::POST_METHOD,
            }
        )
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
    #[serde(default = "default_xtream_live_stream_use_prefix", skip_serializing_if = "is_true")]
    pub xtream_live_stream_use_prefix: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub xtream_live_stream_without_extension: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub resolve_tmdb: bool,
    #[serde(default = "default_resolve_background", skip_serializing_if = "is_true")]
    pub resolve_background: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub resolve_series: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub resolve_vod: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub probe_series: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub probe_vod: bool,
    #[serde(default = "default_resolve_delay_secs", skip_serializing_if = "is_default_resolve_delay_secs")]
    pub resolve_delay: u16,
    #[serde(default = "default_probe_delay_secs", skip_serializing_if = "is_default_probe_delay_secs")]
    pub probe_delay: u16,
    #[serde(default, alias = "resolve_live", skip_serializing_if = "is_false")]
    pub probe_live: bool,
    #[serde(
        default = "default_probe_live_interval",
        alias = "resolve_live_interval_hours",
        skip_serializing_if = "is_default_probe_live_interval"
    )]
    pub probe_live_interval_hours: u32,
}

impl Default for ConfigInputOptionsDto {
    fn default() -> Self {
        ConfigInputOptionsDto {
            xtream_skip_live: false,
            xtream_skip_vod: false,
            xtream_skip_series: false,
            xtream_live_stream_use_prefix: default_xtream_live_stream_use_prefix(),
            xtream_live_stream_without_extension: false,
            resolve_tmdb: false,
            resolve_background: default_resolve_background(),
            resolve_series: false,
            resolve_vod: false,
            probe_series: false,
            probe_vod: false,
            resolve_delay: default_resolve_delay_secs(),
            probe_delay: default_probe_delay_secs(),
            probe_live: false,
            probe_live_interval_hours: default_probe_live_interval(),
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
            && !self.resolve_tmdb
            && self.resolve_background
            && !self.resolve_series
            && !self.resolve_vod
            && !self.probe_series
            && !self.probe_vod
            && is_default_resolve_delay_secs(&self.resolve_delay)
            && is_default_probe_delay_secs(&self.probe_delay)
            && !self.probe_live
            && is_default_probe_live_interval(&self.probe_live_interval_hours)
    }

    pub fn clean(&mut self) {
        self.xtream_skip_live = false;
        self.xtream_skip_vod = false;
        self.xtream_skip_series = false;
        self.xtream_live_stream_use_prefix = default_as_true();
        self.xtream_live_stream_without_extension = false;
        self.resolve_tmdb = false;
        self.resolve_background = default_as_true();
        self.resolve_series = false;
        self.resolve_vod = false;
        self.probe_series = false;
        self.probe_vod = false;
        self.resolve_delay = default_resolve_delay_secs();
        self.probe_delay = default_probe_delay_secs();
        self.probe_live = false;
        self.probe_live_interval_hours = default_probe_live_interval();
    }
}

#[derive(Debug, Default, Copy, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum ClusterSource {
    #[serde(rename = "staged")]
    #[default]
    Staged,
    #[serde(rename = "input")]
    Input,
    #[serde(rename = "skip")]
    Skip,
}

impl ClusterSource {
    const STAGED: &'static str = "staged";
    const INPUT: &'static str = "input";
    const SKIP: &'static str = "skip";
}

impl Display for ClusterSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Staged => Self::STAGED,
                Self::Input => Self::INPUT,
                Self::Skip => Self::SKIP,
            }
        )
    }
}

impl FromStr for ClusterSource {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        if s.eq(Self::STAGED) {
            Ok(Self::Staged)
        } else if s.eq(Self::INPUT) {
            Ok(Self::Input)
        } else if s.eq(Self::SKIP) {
            Ok(Self::Skip)
        } else {
            info_err_res!("Unknown ClusterSource: {}", s)
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StagedInputDto {
    #[serde(default = "default_as_true")]
    pub enabled: bool,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_source: Option<ClusterSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vod_source: Option<ClusterSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series_source: Option<ClusterSource>,
}

impl Default for StagedInputDto {
    fn default() -> Self {
        Self {
            enabled: true,
            name: Arc::default(),
            url: String::default(),
            username: Option::default(),
            password: Option::default(),
            method: InputFetchMethod::default(),
            input_type: InputType::default(),
            headers: HashMap::default(),
            live_source: None,
            vod_source: None,
            series_source: None,
        }
    }
}

impl StagedInputDto {
    pub fn is_empty(&self) -> bool {
        self.url.trim().is_empty()
            && self.username.as_ref().is_none_or(|u| u.trim().is_empty())
            && self.password.as_ref().is_none_or(|u| u.trim().is_empty())
            && self.method == InputFetchMethod::default()
            && self.input_type == InputType::default()
            && self.headers.is_empty()
            && self.live_source.is_none()
            && self.vod_source.is_none()
            && self.series_source.is_none()
    }

    pub fn clean(&mut self) {
        self.url = String::new();
        self.username = None;
        self.password = None;
        self.method = InputFetchMethod::default();
        self.input_type = InputType::default();
        self.headers.clear();
        self.live_source = None;
        self.vod_source = None;
        self.series_source = None;
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
    #[serde(default, skip_serializing_if = "is_zero_i16")]
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
    #[serde(default, skip_serializing_if = "Option::is_none", serialize_with = "serialize_option_vec_flow_map_items")]
    pub aliases: Option<Vec<ConfigInputAliasDto>>,
    #[serde(default, skip_serializing_if = "is_zero_i16")]
    pub priority: i16,
    #[serde(default)]
    pub max_connections: u16,
    #[serde(default, skip_serializing_if = "InputFetchMethod::is_default")]
    pub method: InputFetchMethod,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged: Option<StagedInputDto>,
    #[serde(default, deserialize_with = "deserialize_timestamp", skip_serializing_if = "Option::is_none")]
    pub exp_date: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel_api: Option<PanelApiConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Vec<ConfigProviderDto>>,
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
            provider: None,
        }
    }
}

impl ConfigInputDto {
    pub fn new_with_type(input_type: InputType) -> Self { Self { input_type, ..Self::default() } }

    fn normalize_input_type_from_aliases(&mut self) {
        let has_aliases = self.aliases.as_ref().is_some_and(|aliases| !aliases.is_empty());
        self.input_type = match self.input_type {
            InputType::M3u | InputType::M3uBatch => {
                if has_aliases {
                    InputType::M3uBatch
                } else {
                    InputType::M3u
                }
            }
            InputType::Xtream | InputType::XtreamBatch => {
                if has_aliases {
                    InputType::XtreamBatch
                } else {
                    InputType::Xtream
                }
            }
            InputType::Library => InputType::Library,
        };
    }

    #[allow(clippy::cast_possible_truncation)]
    pub fn prepare(
        &mut self,
        index: u16,
        _include_computed: bool,
        provider_names: &HashSet<String>,
    ) -> Result<u16, TuliproxError> {
        self.name = self.name.trim().intern();
        if self.name.is_empty() {
            return info_err_res!("name for input is mandatory");
        }

        if let Some(duration_str) = &self.cache_duration {
            self.cache_duration_seconds = self.parse_duration(duration_str)?;
        } else {
            self.cache_duration_seconds = 0;
        }

        self.url = self.url.trim().to_string();
        if self.url.starts_with(BATCH_SCHEME_PREFIX) {
            match self.input_type {
                InputType::M3u => {
                    self.input_type = InputType::M3uBatch;
                }
                InputType::Xtream => {
                    self.input_type = InputType::XtreamBatch;
                }
                _ => {}
            }
        } else if self.url.starts_with(PROVIDER_SCHEME_PREFIX)
            && matches!(self.input_type, InputType::M3uBatch | InputType::XtreamBatch)
        {
            return info_err_res!(
                "input type {} does not support provider:// URLs for batch definitions; use batch:// URL",
                self.input_type
            );
        }

        check_input_credentials!(self, self.input_type, true, false);
        check_input_connections!(self, self.input_type, false);
        if let Some(staged_input) = self.staged.as_mut() {
            if staged_input.enabled {
                check_input_credentials!(staged_input, staged_input.input_type, true, true);
                if !matches!(staged_input.input_type, InputType::M3u | InputType::Xtream) {
                    return info_err_res!("Staged input can only be of type m3u or xtream");
                }
                if self.input_type.is_xtream() {
                    let live = staged_input.live_source.unwrap_or(ClusterSource::Staged);
                    let vod_default =
                        if staged_input.input_type.is_m3u() { ClusterSource::Input } else { ClusterSource::Staged };
                    let series_default =
                        if staged_input.input_type.is_m3u() { ClusterSource::Input } else { ClusterSource::Staged };
                    let vod = staged_input.vod_source.unwrap_or(vod_default);
                    let series = staged_input.series_source.unwrap_or(series_default);
                    let (skip_live, skip_vod, skip_series) =
                        self.options.as_ref().map_or((false, false, false), |opts| {
                            (opts.xtream_skip_live, opts.xtream_skip_vod, opts.xtream_skip_series)
                        });

                    let live_uses_staged = matches!(live, ClusterSource::Staged) && !skip_live;
                    let vod_uses_staged = matches!(vod, ClusterSource::Staged) && !skip_vod;
                    let series_uses_staged = matches!(series, ClusterSource::Staged) && !skip_series;

                    if !live_uses_staged && !vod_uses_staged && !series_uses_staged {
                        return info_err_res!(
                            "Staged input is enabled but no cluster source uses 'staged'; set at least one of live_source/vod_source/series_source to 'staged'"
                        );
                    }

                    if staged_input.input_type.is_m3u() && (vod_uses_staged || series_uses_staged) {
                        return info_err_res!(
                            "Staged M3U input cannot provide VOD or Series clusters; use 'input' or 'skip'"
                        );
                    }
                }
            }
        }

        self.persist = get_trimmed_string(self.persist.as_deref());
        check_provider_scheme_url!(self.url, provider_names);

        if let Some(staged_input) = self.staged.as_ref().filter(|staged| staged.enabled) {
            check_provider_scheme_url!(staged_input.url, provider_names);
        }

        let mut current_index = index + 1;
        self.id = current_index;
        if let Some(aliases) = self.aliases.as_mut() {
            let input_type = &self.input_type;
            for alias in aliases {
                current_index = alias.prepare(current_index, input_type)?;
                check_provider_scheme_url!(alias.url.as_str(), provider_names);
            }
        }

        if let Some(panel_api) = self.panel_api.as_mut() {
            panel_api.prepare(&self.name)?;
        }

        // Validate provider:// URLs in EPG sources
        if let Some(epg) = self.epg.as_ref() {
            if let Some(sources) = epg.sources.as_ref() {
                for epg_source in sources {
                    let url = epg_source.url.trim();
                    check_provider_scheme_url!(url, provider_names);
                }
            }
        }

        Ok(current_index)
    }

    fn parse_duration(&self, duration_str: &str) -> Result<u64, TuliproxError> {
        match parse_duration_seconds(duration_str, false) {
            Some(seconds) => Ok(seconds),
            None => info_err_res!("Invalid cache_duration format in '{}': {}", self.name, duration_str),
        }
    }

    // Neue ausgelagerte Methode für die URL-Generierung
    fn generate_auto_epg_url(&self) -> Result<String, String> {
        let get_creds = || {
            if self.username.is_some() && self.password.is_some() {
                return (self.username.clone(), self.password.clone(), Some(self.url.clone()));
            }

            let (u, p, r) = self
                .aliases
                .as_ref()
                .and_then(|aliases| aliases.iter().find(|a| a.enabled))
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
                .and_then(|aliases| aliases.iter().find(|a| a.enabled))
                .map(|alias| {
                    let (u, p) = get_credentials_from_url_str(alias.url.as_str());
                    (u, p, Some(alias.url.clone()))
                })
                .unwrap_or((None, None, None))
        };

        let (username, password, base_url) = get_creds();

        if username.is_none() || password.is_none() || base_url.is_none() {
            Err(format!("auto_epg is enabled for input {}, but no credentials could be extracted", self.name))
        } else if let Some(base) = base_url {
            let clean_base = base.split('?').next().unwrap_or(&base);

            let provider_epg_url = format!(
                "{}/xmltv.php?username={}&password={}",
                trim_last_slash(clean_base),
                username.unwrap_or_default(),
                password.unwrap_or_default()
            );
            Ok(provider_epg_url)
        } else {
            Err(format!(
                "auto_epg is enabled for input {}, but url could not be parsed {}",
                self.name,
                sanitize_sensitive_info(&self.url)
            ))
        }
    }

    pub fn prepare_epg(&mut self, include_computed: bool) -> Result<(), TuliproxError> {
        if let Some(mut epg) = self.epg.take() {
            if self.input_type == InputType::Library {
                warn!("EPG is not supported for library inputs {}, skipping", self.name);
                self.epg = None;
                return Ok(());
            }

            epg.prepare(|| self.generate_auto_epg_url(), include_computed)?;
            epg.t_sources = {
                let mut seen_urls = HashSet::new();
                epg.t_sources.drain(..).filter(|src| seen_urls.insert(src.url.clone())).collect()
            };
            self.epg = Some(epg);
        }
        Ok(())
    }

    pub fn prepare_batch(
        &mut self,
        batch_aliases: Vec<ConfigInputAliasDto>,
        index: u16,
    ) -> Result<Option<u16>, TuliproxError> {
        let idx = apply_batch_aliases!(self, batch_aliases, Some(index));
        Ok(idx)
    }

    pub fn prepare_type(&mut self) -> Result<(), TuliproxError> {
        self.normalize_input_type_from_aliases();
        if self.url.starts_with(PROVIDER_SCHEME_PREFIX)
            && matches!(self.input_type, InputType::M3uBatch | InputType::XtreamBatch)
        {
            return info_err_res!(
                "input type {} does not support provider:// URLs for batch definitions; use batch:// URL",
                self.input_type
            );
        }
        Ok(())
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

    pub fn update_account_expiration_date(
        &mut self,
        input_name: &Arc<str>,
        username: &str,
        exp_date: i64,
    ) -> Result<(), TuliproxError> {
        if &self.name == input_name {
            if let Some(input_username) = &self.username {
                if input_username == username {
                    self.exp_date = Some(exp_date);
                    return Ok(());
                }
            }
        }

        if let Some(aliases) = &mut self.aliases {
            if let Some(alias) = aliases.iter_mut().find(|a| a.username.as_deref() == Some(username)) {
                alias.exp_date = Some(exp_date);
                return Ok(());
            }
        }

        Err(TuliproxError::new(
            TuliproxErrorKind::Info,
            format!("No matching input or alias found for input '{input_name}' with username '{username}'"),
        ))
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigProviderDto {
    #[serde(with = "arc_str_serde")]
    pub name: Arc<str>,
    #[serde(with = "arc_str_vec_serde")]
    pub urls: Vec<Arc<str>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns: Option<ProviderDnsDto>,
}

impl ConfigProviderDto {
    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        self.name = self.name.trim().intern();
        if self.name.is_empty() {
            return info_err_res!("Name for provider is mandatory");
        }
        self.urls = self.urls.drain(..).filter(|url| !url.trim().is_empty()).map(|u| u.trim().intern()).collect();
        if self.urls.is_empty() {
            return info_err_res!("Urls for provider is mandatory");
        }
        if let Some(dns) = self.dns.as_mut() {
            dns.prepare()?;
        }
        Ok(())
    }
}

pub const fn default_provider_dns_refresh_secs() -> u64 { 300 }
pub const fn is_default_provider_dns_refresh_secs(v: &u64) -> bool { *v == default_provider_dns_refresh_secs() }
pub fn is_default_dns_prefer(v: &DnsPrefer) -> bool { *v == DnsPrefer::default() }
pub fn is_default_on_resolve_error(v: &OnResolveErrorPolicy) -> bool { *v == OnResolveErrorPolicy::default() }
pub fn is_default_on_connect_error(v: &OnConnectErrorPolicy) -> bool { *v == OnConnectErrorPolicy::default() }

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, Sequence, PartialEq, Eq, Default)]
pub enum DnsPrefer {
    #[serde(rename = "ipv4")]
    Ipv4,
    #[serde(rename = "ipv6")]
    Ipv6,
    #[serde(rename = "system")]
    #[default]
    System,
}

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, Sequence, PartialEq, Eq)]
pub enum DnsScheme {
    #[serde(rename = "http")]
    Http,
    #[serde(rename = "https")]
    Https,
}

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, Sequence, PartialEq, Eq, Default)]
pub enum OnResolveErrorPolicy {
    #[serde(rename = "keep_last_good")]
    #[default]
    KeepLastGood,
    #[serde(rename = "fallback_to_hostname")]
    FallbackToHostname,
}

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, Sequence, PartialEq, Eq, Default)]
pub enum OnConnectErrorPolicy {
    #[serde(rename = "try_next_ip")]
    #[default]
    TryNextIp,
    #[serde(rename = "rotate_provider_url")]
    RotateProviderUrl,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProviderDnsDto {
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,
    #[serde(
        default = "default_provider_dns_refresh_secs",
        skip_serializing_if = "is_default_provider_dns_refresh_secs"
    )]
    pub refresh_secs: u64,
    #[serde(default, skip_serializing_if = "is_default_dns_prefer")]
    pub prefer: DnsPrefer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_addrs: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schemes: Option<Vec<DnsScheme>>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub keep_vhost: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrides: Option<HashMap<String, Vec<IpAddr>>>,
    #[serde(default, skip_serializing_if = "is_default_on_resolve_error")]
    pub on_resolve_error: OnResolveErrorPolicy,
    #[serde(default, skip_serializing_if = "is_default_on_connect_error")]
    pub on_connect_error: OnConnectErrorPolicy,
}

impl Default for ProviderDnsDto {
    fn default() -> Self {
        Self {
            enabled: false,
            refresh_secs: default_provider_dns_refresh_secs(),
            prefer: DnsPrefer::default(),
            max_addrs: None,
            schemes: None,
            keep_vhost: false,
            overrides: None,
            on_resolve_error: OnResolveErrorPolicy::default(),
            on_connect_error: OnConnectErrorPolicy::default(),
        }
    }
}

impl ProviderDnsDto {
    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        self.refresh_secs = self.refresh_secs.max(10);
        if self.max_addrs == Some(0) {
            return info_err_res!("Provider dns max_addrs must be >= 1 when set");
        }
        if let Some(schemes) = self.schemes.as_mut() {
            let mut unique = Vec::with_capacity(schemes.len());
            for scheme in schemes.drain(..) {
                if !unique.contains(&scheme) {
                    unique.push(scheme);
                }
            }
            *schemes = unique;
            if schemes.is_empty() {
                self.schemes = None;
            }
        }

        if let Some(overrides) = self.overrides.as_mut() {
            let mut normalized: HashMap<String, Vec<IpAddr>> = HashMap::new();
            for (host, ips) in std::mem::take(overrides) {
                let host = host.trim().to_ascii_lowercase();
                if host.is_empty() {
                    return info_err_res!("Provider dns overrides hostname must not be empty");
                }
                if ips.is_empty() {
                    return info_err_res!("Provider dns overrides for host '{host}' must not be empty");
                }
                let entry = normalized.entry(host.clone()).or_default();
                for ip in ips {
                    if !entry.contains(&ip) {
                        entry.push(ip);
                    }
                }
            }
            if normalized.is_empty() {
                self.overrides = None;
            } else {
                *overrides = normalized;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_dto() -> ConfigInputDto {
        ConfigInputDto { name: "test_input".intern(), ..ConfigInputDto::default() }
    }

    #[test]
    fn test_epg_url_from_explicit_main_credentials() {
        let mut dto = create_test_dto();
        // Hier testen wir auch gleich mit, ob der Trailing Slash sauber entfernt wird!
        dto.url = "http://myprovider.com/".to_string();
        dto.username = Some("hello".to_string());
        dto.password = Some("mello".to_string());

        let result = dto.generate_auto_epg_url().unwrap();
        assert_eq!(result, "http://myprovider.com/xmltv.php?username=hello&password=mello");
    }

    #[test]
    fn test_epg_url_from_enabled_alias_explicit_credentials() {
        let mut dto = create_test_dto();
        dto.url = "http://main.com".to_string(); // Haupt-URL hat keine Credentials

        let alias = ConfigInputAliasDto {
            enabled: true,
            url: "http://alias.com".to_string(),
            username: Some("alias_user".to_string()),
            password: Some("alias_pass".to_string()),
            ..ConfigInputAliasDto::default()
        };

        dto.aliases = Some(vec![alias]);

        let result = dto.generate_auto_epg_url().unwrap();
        // Er muss die URL und die Credentials vom Alias nehmen
        assert_eq!(result, "http://alias.com/xmltv.php?username=alias_user&password=alias_pass");
    }

    #[test]
    fn test_epg_url_skips_disabled_aliases() {
        let mut dto = create_test_dto();

        let alias = ConfigInputAliasDto {
            enabled: false, // Alias ist deaktiviert!
            url: "http://alias.com".to_string(),
            username: Some("alias_user".to_string()),
            password: Some("alias_pass".to_string()),
            ..ConfigInputAliasDto::default()
        };

        dto.aliases = Some(vec![alias]);

        let result = dto.generate_auto_epg_url();
        // Da Haupt-DTO leer ist und Alias deaktiviert, muss ein Fehler kommen
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no credentials could be extracted"));
    }

    #[test]
    fn test_epg_url_fails_without_credentials() {
        let mut dto = create_test_dto();
        dto.url = "http://nocreds.com".to_string();

        let result = dto.generate_auto_epg_url();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no credentials could be extracted"));
    }

    #[test]
    fn test_epg_url_from_main_url_query_credentials() {
        let mut dto = create_test_dto();
        // Credentials stecken als Query-Parameter in der URL
        dto.url = "http://myprovider.com?username=hello&password=mello".to_string();

        let result = dto.generate_auto_epg_url().unwrap();

        // Durch unseren sauberen "clean_base" Fix sieht die URL jetzt richtig aus!
        assert_eq!(result, "http://myprovider.com/xmltv.php?username=hello&password=mello");
    }

    #[test]
    fn test_epg_url_from_alias_url_query_credentials() {
        let mut dto = create_test_dto();
        dto.url = "http://main.com".to_string();

        let alias = ConfigInputAliasDto {
            enabled: true,
            // Credentials im Alias als Query-Parameter
            url: "http://alias.com?username=alias_user&password=alias_pass".to_string(),
            ..ConfigInputAliasDto::default()
        };

        dto.aliases = Some(vec![alias]);

        let result = dto.generate_auto_epg_url().unwrap();
        assert_eq!(result, "http://alias.com/xmltv.php?username=alias_user&password=alias_pass");
    }

    #[test]
    fn test_epg_url_from_provider_scheme_url_query_credentials() {
        let mut dto = create_test_dto();
        dto.url = "provider://myprovider".to_string();
        dto.username = Some("test".to_string());
        dto.password = Some("secret".to_string());

        let result = dto.generate_auto_epg_url().unwrap();
        assert_eq!(result, "provider://myprovider/xmltv.php?username=test&password=secret");
    }

    #[test]
    fn test_provider_dns_defaults() {
        let dns = ProviderDnsDto::default();
        assert!(!dns.enabled);
        assert_eq!(dns.refresh_secs, 300);
        assert_eq!(dns.prefer, DnsPrefer::System);
        assert_eq!(dns.on_resolve_error, OnResolveErrorPolicy::KeepLastGood);
        assert_eq!(dns.on_connect_error, OnConnectErrorPolicy::TryNextIp);
        assert!(dns.schemes.is_none());
    }

    #[test]
    fn test_provider_dns_prepare_normalizes_overrides_and_clamps_refresh() {
        let mut dns = ProviderDnsDto {
            refresh_secs: 1,
            schemes: Some(vec![DnsScheme::Http, DnsScheme::Http, DnsScheme::Https]),
            overrides: Some(HashMap::from([(
                "  EXAMPLE.COM ".to_string(),
                vec![
                    "203.0.113.10".parse::<IpAddr>().expect("valid ip"),
                    "203.0.113.10".parse::<IpAddr>().expect("valid ip"),
                ],
            )])),
            ..ProviderDnsDto::default()
        };

        dns.prepare().expect("dns prepare should succeed");

        assert_eq!(dns.refresh_secs, 10);
        assert_eq!(dns.schemes, Some(vec![DnsScheme::Http, DnsScheme::Https]));
        let overrides = dns.overrides.expect("overrides should exist");
        assert_eq!(overrides.len(), 1);
        assert!(overrides.contains_key("example.com"));
        assert_eq!(overrides["example.com"].len(), 1);
    }

    #[test]
    fn prepare_switches_xtream_to_xtream_batch_when_alias_exists() {
        let mut dto = ConfigInputDto {
            name: "input_alias".intern(),
            input_type: InputType::Xtream,
            url: "batch:///tmp/input_alias.csv".to_string(),
            aliases: Some(vec![ConfigInputAliasDto {
                id: 1,
                name: "alias_1".intern(),
                url: "http://provider.example/stream".to_string(),
                username: Some("u".to_string()),
                password: Some("p".to_string()),
                enabled: true,
                ..ConfigInputAliasDto::default()
            }]),
            ..ConfigInputDto::default()
        };

        dto.prepare_type().expect("prepare type should succeed");
        dto.prepare(0, true, &HashSet::new()).expect("prepare should succeed and infer batch type from batch:// URL");
        assert_eq!(dto.input_type, InputType::XtreamBatch);
    }

    #[test]
    fn prepare_batch_url_does_not_require_xtream_credentials() {
        let mut dto = ConfigInputDto {
            name: "batch_no_creds".intern(),
            input_type: InputType::Xtream,
            url: "batch:///tmp/no-creds.csv".to_string(),
            username: None,
            password: None,
            ..ConfigInputDto::default()
        };

        dto.prepare(0, true, &HashSet::new()).expect("batch:// input must be normalized before credential validation");
        assert_eq!(dto.input_type, InputType::XtreamBatch);
    }

    #[test]
    fn prepare_fails_for_provider_scheme_on_batch_input() {
        let mut dto = ConfigInputDto {
            name: "batch_provider".intern(),
            input_type: InputType::XtreamBatch,
            url: "provider://myprovider".to_string(),
            username: Some("root_user".to_string()),
            password: Some("root_pass".to_string()),
            aliases: Some(vec![ConfigInputAliasDto {
                id: 1,
                name: "alias_1".intern(),
                url: "http://provider.example/stream".to_string(),
                username: Some("u".to_string()),
                password: Some("p".to_string()),
                enabled: true,
                ..ConfigInputAliasDto::default()
            }]),
            ..ConfigInputDto::default()
        };

        let err = dto.prepare(0, true, &HashSet::new()).expect_err("prepare must reject provider:// for batch input");
        assert!(err.to_string().contains("does not support provider:// URLs"), "Error: {err}");
    }

    #[test]
    fn prepare_rejects_missing_input_url_even_with_aliases() {
        let mut dto = ConfigInputDto {
            name: "xtream_missing_root_url".intern(),
            input_type: InputType::Xtream,
            url: "".to_string(),
            username: Some("root_user".to_string()),
            password: Some("root_pass".to_string()),
            aliases: Some(vec![ConfigInputAliasDto {
                id: 1,
                name: "alias_1".intern(),
                url: "http://alias.example".to_string(),
                username: Some("alias_user".to_string()),
                password: Some("alias_pass".to_string()),
                enabled: true,
                ..ConfigInputAliasDto::default()
            }]),
            ..ConfigInputDto::default()
        };

        let err = dto
            .prepare(0, true, &HashSet::new())
            .expect_err("prepare must require root input url even when aliases are present");
        assert!(err.to_string().contains("url for input is mandatory"), "Error: {err}");
    }

    #[test]
    fn prepare_rejects_xtream_batch_without_root_credentials_even_with_aliases() {
        let mut dto = ConfigInputDto {
            name: "xtream_batch_missing_root_creds".intern(),
            input_type: InputType::XtreamBatch,
            url: "http://root.example".to_string(),
            aliases: Some(vec![ConfigInputAliasDto {
                id: 1,
                name: "alias_1".intern(),
                url: "http://alias.example".to_string(),
                username: Some("alias_user".to_string()),
                password: Some("alias_pass".to_string()),
                enabled: true,
                ..ConfigInputAliasDto::default()
            }]),
            ..ConfigInputDto::default()
        };

        let err = dto
            .prepare(0, true, &HashSet::new())
            .expect_err("prepare must require root credentials for non-batch xtream-batch input");
        assert!(err.to_string().contains("xtream-batch without batch:// URL"), "Error: {err}");
    }

    #[test]
    fn prepare_rejects_xtream_batch_batch_url_with_root_credentials_even_with_aliases() {
        let mut dto = ConfigInputDto {
            name: "xtream_batch_with_root_creds".intern(),
            input_type: InputType::XtreamBatch,
            url: "batch:///tmp/aliases.csv".to_string(),
            username: Some("root_user".to_string()),
            password: Some("root_pass".to_string()),
            aliases: Some(vec![ConfigInputAliasDto {
                id: 1,
                name: "alias_1".intern(),
                url: "http://alias.example".to_string(),
                username: Some("alias_user".to_string()),
                password: Some("alias_pass".to_string()),
                enabled: true,
                ..ConfigInputAliasDto::default()
            }]),
            ..ConfigInputDto::default()
        };

        let err = dto
            .prepare(0, true, &HashSet::new())
            .expect_err("prepare must reject root credentials when using batch:// for xtream-batch");
        assert!(err.to_string().contains("with batch:// URL should not define username or password"), "Error: {err}");
    }

    #[test]
    fn test_cluster_source_serde_roundtrip() {
        let json = r#""staged""#;
        let cs: ClusterSource = serde_json::from_str(json).expect("deserialize staged");
        assert_eq!(cs, ClusterSource::Staged);
        assert_eq!(serde_json::to_string(&cs).expect("serialize"), json);

        let cs: ClusterSource = serde_json::from_str(r#""input""#).expect("deserialize input");
        assert_eq!(cs, ClusterSource::Input);

        let cs: ClusterSource = serde_json::from_str(r#""skip""#).expect("deserialize skip");
        assert_eq!(cs, ClusterSource::Skip);
    }

    #[test]
    fn test_staged_m3u_vod_source_staged_rejected() {
        let mut dto = create_test_dto();
        dto.input_type = InputType::Xtream;
        dto.url = "http://main.com".to_string();
        dto.username = Some("u".to_string());
        dto.password = Some("p".to_string());
        dto.staged = Some(StagedInputDto {
            name: "staged".into(),
            input_type: InputType::M3u,
            url: "http://staged.com/playlist.m3u".to_string(),
            vod_source: Some(ClusterSource::Staged),
            ..StagedInputDto::default()
        });

        let err = dto.prepare(0, true, &HashSet::new()).expect_err("should reject vod_source=staged for M3U staged");
        assert!(err.to_string().contains("Staged M3U input cannot provide VOD or Series"), "Error: {err}");
    }

    #[test]
    fn test_staged_m3u_series_source_staged_rejected() {
        let mut dto = create_test_dto();
        dto.input_type = InputType::Xtream;
        dto.url = "http://main.com".to_string();
        dto.username = Some("u".to_string());
        dto.password = Some("p".to_string());
        dto.staged = Some(StagedInputDto {
            name: "staged".into(),
            input_type: InputType::M3u,
            url: "http://staged.com/playlist.m3u".to_string(),
            series_source: Some(ClusterSource::Staged),
            ..StagedInputDto::default()
        });

        let err = dto.prepare(0, true, &HashSet::new()).expect_err("should reject series_source=staged for M3U staged");
        assert!(err.to_string().contains("Staged M3U input cannot provide VOD or Series"), "Error: {err}");
    }

    #[test]
    fn test_staged_xtream_with_cluster_sources_accepted() {
        let mut dto = create_test_dto();
        dto.input_type = InputType::Xtream;
        dto.url = "http://main.com".to_string();
        dto.username = Some("u".to_string());
        dto.password = Some("p".to_string());
        dto.staged = Some(StagedInputDto {
            name: "staged".into(),
            input_type: InputType::Xtream,
            url: "http://staged.com".to_string(),
            username: Some("su".to_string()),
            password: Some("sp".to_string()),
            live_source: Some(ClusterSource::Staged),
            vod_source: Some(ClusterSource::Input),
            series_source: Some(ClusterSource::Skip),
            ..StagedInputDto::default()
        });

        dto.prepare(0, true, &HashSet::new()).expect("xtream staged with all cluster sources should succeed");
    }

    #[test]
    fn test_staged_enabled_requires_at_least_one_staged_cluster_source() {
        let mut dto = create_test_dto();
        dto.input_type = InputType::Xtream;
        dto.url = "http://main.com".to_string();
        dto.username = Some("u".to_string());
        dto.password = Some("p".to_string());
        dto.staged = Some(StagedInputDto {
            enabled: true,
            name: "staged".into(),
            input_type: InputType::Xtream,
            url: "http://staged.com".to_string(),
            username: Some("su".to_string()),
            password: Some("sp".to_string()),
            live_source: Some(ClusterSource::Input),
            vod_source: Some(ClusterSource::Skip),
            series_source: Some(ClusterSource::Input),
            ..StagedInputDto::default()
        });

        let err =
            dto.prepare(0, true, &HashSet::new()).expect_err("expected validation error for missing staged source");
        assert!(err.to_string().contains("no cluster source uses 'staged'"), "Error: {err}");
    }

    #[test]
    fn test_staged_skip_flag_excludes_cluster_from_staged_requirement() {
        let mut dto = create_test_dto();
        dto.input_type = InputType::Xtream;
        dto.url = "http://main.com".to_string();
        dto.username = Some("u".to_string());
        dto.password = Some("p".to_string());
        dto.options = Some(ConfigInputOptionsDto { xtream_skip_live: true, ..ConfigInputOptionsDto::default() });
        dto.staged = Some(StagedInputDto {
            enabled: true,
            name: "staged".into(),
            input_type: InputType::Xtream,
            url: "http://staged.com".to_string(),
            username: Some("su".to_string()),
            password: Some("sp".to_string()),
            live_source: Some(ClusterSource::Staged),
            vod_source: Some(ClusterSource::Input),
            series_source: Some(ClusterSource::Input),
            ..StagedInputDto::default()
        });

        let err = dto
            .prepare(0, true, &HashSet::new())
            .expect_err("skipped staged cluster must not satisfy staged-source requirement");
        assert!(err.to_string().contains("no cluster source uses 'staged'"), "Error: {err}");
    }

    #[test]
    fn test_staged_m3u_vod_staged_allowed_when_vod_is_skipped() {
        let mut dto = create_test_dto();
        dto.input_type = InputType::Xtream;
        dto.url = "http://main.com".to_string();
        dto.username = Some("u".to_string());
        dto.password = Some("p".to_string());
        dto.options = Some(ConfigInputOptionsDto { xtream_skip_vod: true, ..ConfigInputOptionsDto::default() });
        dto.staged = Some(StagedInputDto {
            name: "staged".into(),
            input_type: InputType::M3u,
            url: "http://staged.com/playlist.m3u".to_string(),
            vod_source: Some(ClusterSource::Staged),
            ..StagedInputDto::default()
        });

        dto.prepare(0, true, &HashSet::new())
            .expect("staged M3U vod_source=staged is valid when VOD cluster is skipped");
    }

    #[test]
    fn test_staged_disabled_skips_cluster_source_validation() {
        let mut dto = create_test_dto();
        dto.input_type = InputType::Xtream;
        dto.url = "http://main.com".to_string();
        dto.username = Some("u".to_string());
        dto.password = Some("p".to_string());
        dto.staged = Some(StagedInputDto {
            enabled: false,
            name: "staged".into(),
            input_type: InputType::Xtream,
            url: "http://staged.com".to_string(),
            live_source: Some(ClusterSource::Input),
            vod_source: Some(ClusterSource::Skip),
            series_source: Some(ClusterSource::Input),
            ..StagedInputDto::default()
        });

        dto.prepare(0, true, &HashSet::new())
            .expect("disabled staged input should not enforce cluster source validation");
    }

    #[test]
    fn test_staged_disabled_skips_m3u_cluster_constraints() {
        let mut dto = create_test_dto();
        dto.input_type = InputType::Xtream;
        dto.url = "http://main.com".to_string();
        dto.username = Some("u".to_string());
        dto.password = Some("p".to_string());
        dto.staged = Some(StagedInputDto {
            enabled: false,
            name: "staged".into(),
            input_type: InputType::M3u,
            url: "http://staged.com/playlist.m3u".to_string(),
            vod_source: Some(ClusterSource::Staged),
            series_source: Some(ClusterSource::Staged),
            ..StagedInputDto::default()
        });

        dto.prepare(0, true, &HashSet::new())
            .expect("disabled staged input should not enforce staged M3U cluster validation");
    }

    #[test]
    fn test_staged_disabled_skips_provider_url_validation() {
        let mut dto = create_test_dto();
        dto.input_type = InputType::Xtream;
        dto.url = "http://main.com".to_string();
        dto.username = Some("u".to_string());
        dto.password = Some("p".to_string());
        dto.staged = Some(StagedInputDto {
            enabled: false,
            name: "staged".into(),
            input_type: InputType::Xtream,
            url: "provider://missing-provider".to_string(),
            ..StagedInputDto::default()
        });

        dto.prepare(0, true, &HashSet::new())
            .expect("disabled staged input should not enforce provider URL validation");
    }

    #[test]
    fn test_staged_dto_defaults_none() {
        let staged = StagedInputDto::default();
        assert!(staged.live_source.is_none());
        assert!(staged.vod_source.is_none());
        assert!(staged.series_source.is_none());
    }

    #[test]
    fn test_staged_dto_is_empty_with_cluster_source() {
        let mut staged = StagedInputDto::default();
        assert!(staged.is_empty());

        staged.live_source = Some(ClusterSource::Input);
        assert!(!staged.is_empty());
    }

    #[test]
    fn test_staged_dto_clean_resets_cluster_sources() {
        let mut staged = StagedInputDto {
            live_source: Some(ClusterSource::Input),
            vod_source: Some(ClusterSource::Skip),
            series_source: Some(ClusterSource::Staged),
            ..StagedInputDto::default()
        };
        staged.clean();
        assert!(staged.live_source.is_none());
        assert!(staged.vod_source.is_none());
        assert!(staged.series_source.is_none());
    }
}
