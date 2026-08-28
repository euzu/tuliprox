use crate::{
    defaults::{
        default_ics_dummy_block_hours, default_ics_dummy_days_future, default_ics_dummy_days_past,
        default_ics_dummy_min_gap_minutes, default_ics_dummy_title, default_ics_event_description,
        default_ics_event_title, default_ics_max_decompressed_bytes, default_ics_max_download_bytes,
        default_ics_max_events, default_ics_timezone, is_default_ics_dummy_block_hours,
        is_default_ics_dummy_days_future, is_default_ics_dummy_days_past, is_default_ics_dummy_min_gap_minutes,
        is_default_ics_dummy_title, is_default_ics_event_description, is_default_ics_event_title,
        is_default_ics_max_decompressed_bytes, is_default_ics_max_download_bytes, is_default_ics_max_events,
        is_default_ics_timezone, is_false, MAX_ICS_DAYS_FUTURE, MAX_ICS_DAYS_PAST,
        MAX_ICS_DECOMPRESSED_BYTES_HARD_LIMIT, MAX_ICS_DESCRIPTION_LENGTH, MAX_ICS_DOWNLOAD_BYTES_HARD_LIMIT,
        MAX_ICS_EVENTS_HARD_LIMIT, MAX_ICS_SUMMARY_LENGTH,
    },
    error::TuliproxError,
    model::EpgSmartMatchConfigDto,
    utils::{is_blank_optional_string, sanitize_sensitive_info},
};

const AUTO_URL: &str = "auto";

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum EpgSourceTypeDto {
    #[default]
    Xmltv,
    Ics,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EpgSourceDto {
    #[serde(default, rename = "type")]
    pub source_type: EpgSourceTypeDto,
    pub url: String,
    #[serde(default)]
    pub priority: i16,
    #[serde(default, skip_serializing_if = "is_false")]
    pub logo_override: bool,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub channel_id: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub channel_title: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub match_names: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ics: Option<IcsEpgSourceConfigDto>,
}

impl Default for EpgSourceDto {
    fn default() -> Self {
        Self {
            source_type: EpgSourceTypeDto::Xmltv,
            url: String::new(),
            priority: 0,
            logo_override: false,
            channel_id: None,
            channel_title: None,
            match_names: Vec::new(),
            ics: None,
        }
    }
}

impl EpgSourceDto {
    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        self.url = self.url.trim().to_string();
        self.channel_id =
            self.channel_id.take().map(|value| value.trim().to_string()).filter(|value| !value.is_empty());
        self.channel_title =
            self.channel_title.take().map(|value| value.trim().to_string()).filter(|value| !value.is_empty());
        self.match_names = self
            .match_names
            .drain(..)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect();

        match self.source_type {
            EpgSourceTypeDto::Xmltv => self.prepare_xmltv(),
            EpgSourceTypeDto::Ics => self.prepare_ics(),
        }
    }

    fn prepare_xmltv(&self) -> Result<(), TuliproxError> {
        if self.url.is_empty() {
            return Err(TuliproxError::ConfigEpg("XMLTV EPG source url is empty".to_string()));
        }
        if self.channel_id.is_some()
            || self.channel_title.is_some()
            || !self.match_names.is_empty()
            || self.ics.is_some()
        {
            return Err(TuliproxError::ConfigEpg(
                "channel_id, channel_title, match_names, and ics are only supported for ICS EPG sources".to_string(),
            ));
        }
        Ok(())
    }

    fn prepare_ics(&mut self) -> Result<(), TuliproxError> {
        if self.url.is_empty() {
            return Err(TuliproxError::ConfigEpg("ICS EPG source url is empty".to_string()));
        }
        if self.url.eq_ignore_ascii_case(AUTO_URL) {
            return Err(TuliproxError::ConfigEpg(
                "url: auto is only supported for XMLTV EPG sources, not for ICS".to_string(),
            ));
        }
        if let Some(rest) = strip_prefix_ignore_ascii_case(&self.url, "webcal://") {
            self.url = format!("https://{rest}");
        }
        if self.channel_id.as_deref().is_none_or(str::is_empty) {
            return Err(TuliproxError::ConfigEpg("channel_id is required for ICS EPG sources".to_string()));
        }
        let ics = self.ics.get_or_insert_with(IcsEpgSourceConfigDto::default);
        ics.timezone = ics.timezone.trim().to_string();
        validate_ics_config(ics)?;
        validate_ics_url_scheme(&self.url)
    }

    pub fn is_valid(&self) -> bool {
        !self.url.is_empty()
    }

    pub fn source_identity(&self) -> String {
        match self.source_type {
            EpgSourceTypeDto::Xmltv => format!("xmltv|{}", self.url),
            EpgSourceTypeDto::Ics => {
                format!("ics|{}|{}", self.url, self.channel_id.as_deref().unwrap_or_default())
            }
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct IcsEpgSourceConfigDto {
    #[serde(default = "default_ics_timezone", skip_serializing_if = "is_default_ics_timezone")]
    pub timezone: String,
    #[serde(default, skip_serializing_if = "IcsEventMappingDto::is_default")]
    pub event: IcsEventMappingDto,
    #[serde(default, skip_serializing_if = "IcsDummyConfigDto::is_default")]
    pub dummy: IcsDummyConfigDto,
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_cancelled: bool,
    #[serde(default = "default_ics_max_events", skip_serializing_if = "is_default_ics_max_events")]
    pub max_events: usize,
    #[serde(default = "default_ics_max_download_bytes", skip_serializing_if = "is_default_ics_max_download_bytes")]
    pub max_download_bytes: u64,
    #[serde(
        default = "default_ics_max_decompressed_bytes",
        skip_serializing_if = "is_default_ics_max_decompressed_bytes"
    )]
    pub max_decompressed_bytes: usize,
}

impl Default for IcsEpgSourceConfigDto {
    fn default() -> Self {
        Self {
            timezone: default_ics_timezone(),
            event: IcsEventMappingDto::default(),
            dummy: IcsDummyConfigDto::default(),
            include_cancelled: false,
            max_events: default_ics_max_events(),
            max_download_bytes: default_ics_max_download_bytes(),
            max_decompressed_bytes: default_ics_max_decompressed_bytes(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct IcsEventMappingDto {
    #[serde(default = "default_ics_event_title", skip_serializing_if = "is_default_ics_event_title")]
    pub title: String,
    #[serde(default = "default_ics_event_description", skip_serializing_if = "is_default_ics_event_description")]
    pub description: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_location: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_categories: bool,
}

impl Default for IcsEventMappingDto {
    fn default() -> Self {
        Self {
            title: default_ics_event_title(),
            description: default_ics_event_description(),
            include_location: false,
            include_categories: false,
        }
    }
}

impl IcsEventMappingDto {
    pub fn is_default(value: &Self) -> bool {
        value == &Self::default()
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct IcsDummyConfigDto {
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,
    #[serde(default = "default_ics_dummy_title", skip_serializing_if = "is_default_ics_dummy_title")]
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default = "default_ics_dummy_days_past", skip_serializing_if = "is_default_ics_dummy_days_past")]
    pub days_past: u16,
    #[serde(default = "default_ics_dummy_days_future", skip_serializing_if = "is_default_ics_dummy_days_future")]
    pub days_future: u16,
    #[serde(default = "default_ics_dummy_block_hours", skip_serializing_if = "is_default_ics_dummy_block_hours")]
    pub block_hours: u8,
    #[serde(
        default = "default_ics_dummy_min_gap_minutes",
        skip_serializing_if = "is_default_ics_dummy_min_gap_minutes"
    )]
    pub min_gap_minutes: u16,
}

impl Default for IcsDummyConfigDto {
    fn default() -> Self {
        Self {
            enabled: false,
            title: default_ics_dummy_title(),
            description: String::new(),
            days_past: default_ics_dummy_days_past(),
            days_future: default_ics_dummy_days_future(),
            block_hours: default_ics_dummy_block_hours(),
            min_gap_minutes: default_ics_dummy_min_gap_minutes(),
        }
    }
}

impl IcsDummyConfigDto {
    pub fn is_default(value: &Self) -> bool {
        value == &Self::default()
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct EpgConfigDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<EpgSourceDto>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smart_match: Option<EpgSmartMatchConfigDto>,
    #[serde(skip)]
    pub t_sources: Vec<EpgSourceDto>,
}

impl EpgConfigDto {
    /// Prepares the EPG configuration by resolving all source URLs into `t_sources`.
    ///
    /// - `create_auto_url` derives an XMLTV URL from the parent input when XMLTV `url` is `auto`.
    /// - `include_computed` skips resolution when serialisation round-trips do not need computed URLs.
    pub fn prepare<F>(&mut self, create_auto_url: F, include_computed: bool) -> Result<(), TuliproxError>
    where
        F: Fn() -> Result<String, String>,
    {
        if include_computed {
            self.t_sources = Vec::new();
            if let Some(epg_sources) = self.sources.as_mut() {
                for epg_source in epg_sources.iter_mut() {
                    epg_source.prepare()?;
                    if !epg_source.is_valid() {
                        continue;
                    }

                    if epg_source.source_type == EpgSourceTypeDto::Xmltv
                        && epg_source.url.eq_ignore_ascii_case(AUTO_URL)
                    {
                        match create_auto_url() {
                            Ok(provider_url) => {
                                let mut resolved = epg_source.clone();
                                resolved.url = provider_url;
                                self.t_sources.push(resolved);
                            }
                            Err(err) => return Err(TuliproxError::ConfigEpg(err.clone())),
                        }
                    } else {
                        self.t_sources.push(epg_source.clone());
                    }
                }
            }

            if let Some(smart_match) = self.smart_match.as_mut() {
                smart_match.prepare()?;
            }
        }
        Ok(())
    }
}

fn validate_ics_config(config: &IcsEpgSourceConfigDto) -> Result<(), TuliproxError> {
    validate_ics_timezone(&config.timezone)?;
    let block_hours = config.dummy.block_hours;
    if block_hours == 0 || block_hours > 24 || 24 % block_hours != 0 {
        return Err(TuliproxError::ConfigEpg(format!(
            "ics.dummy.block_hours must divide 24 evenly, got {block_hours}"
        )));
    }
    if config.max_events == 0 {
        return Err(TuliproxError::ConfigEpg("ics.max_events must be greater than 0".to_string()));
    }
    if config.max_events > MAX_ICS_EVENTS_HARD_LIMIT {
        return Err(TuliproxError::ConfigEpg(format!("ics.max_events must not exceed {MAX_ICS_EVENTS_HARD_LIMIT}")));
    }
    if config.max_download_bytes == 0 {
        return Err(TuliproxError::ConfigEpg("ics.max_download_bytes must be greater than 0".to_string()));
    }
    if config.max_download_bytes > MAX_ICS_DOWNLOAD_BYTES_HARD_LIMIT {
        return Err(TuliproxError::ConfigEpg(format!(
            "ics.max_download_bytes must not exceed {MAX_ICS_DOWNLOAD_BYTES_HARD_LIMIT}"
        )));
    }
    if config.max_decompressed_bytes == 0 {
        return Err(TuliproxError::ConfigEpg("ics.max_decompressed_bytes must be greater than 0".to_string()));
    }
    if config.max_decompressed_bytes > MAX_ICS_DECOMPRESSED_BYTES_HARD_LIMIT {
        return Err(TuliproxError::ConfigEpg(format!(
            "ics.max_decompressed_bytes must not exceed {MAX_ICS_DECOMPRESSED_BYTES_HARD_LIMIT}"
        )));
    }
    if config.dummy.days_past > MAX_ICS_DAYS_PAST {
        return Err(TuliproxError::ConfigEpg(format!("ics.dummy.days_past must not exceed {MAX_ICS_DAYS_PAST}")));
    }
    if config.dummy.days_future > MAX_ICS_DAYS_FUTURE {
        return Err(TuliproxError::ConfigEpg(format!("ics.dummy.days_future must not exceed {MAX_ICS_DAYS_FUTURE}")));
    }
    validate_text_limit("ics.event.title", &config.event.title, MAX_ICS_SUMMARY_LENGTH)?;
    validate_text_limit("ics.event.description", &config.event.description, MAX_ICS_DESCRIPTION_LENGTH)?;
    validate_text_limit("ics.dummy.title", &config.dummy.title, MAX_ICS_SUMMARY_LENGTH)?;
    validate_text_limit("ics.dummy.description", &config.dummy.description, MAX_ICS_DESCRIPTION_LENGTH)?;
    Ok(())
}

fn validate_ics_timezone(timezone: &str) -> Result<(), TuliproxError> {
    if timezone.is_empty() {
        return Err(TuliproxError::ConfigEpg("ics.timezone must not be empty".to_string()));
    }
    timezone
        .parse::<chrono_tz::Tz>()
        .map(|_| ())
        .map_err(|_| TuliproxError::ConfigEpg(format!("ics.timezone '{timezone}' is not a valid IANA timezone")))
}

fn validate_text_limit(field: &str, value: &str, max_len: usize) -> Result<(), TuliproxError> {
    if value.len() > max_len {
        return Err(TuliproxError::ConfigEpg(format!("{field} must not exceed {max_len} bytes")));
    }
    Ok(())
}

fn validate_ics_url_scheme(url: &str) -> Result<(), TuliproxError> {
    if is_absolute_local_path(url) {
        return Ok(());
    }

    let Ok(parsed) = url::Url::parse(url) else {
        return Ok(());
    };

    match parsed.scheme() {
        "https" | "http" | "file" | "provider" => Ok(()),
        scheme => Err(TuliproxError::ConfigEpg(format!(
            "Unsupported ICS url scheme '{scheme}' for {}",
            sanitize_sensitive_info(url)
        ))),
    }
}

fn strip_prefix_ignore_ascii_case<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let candidate = value.get(..prefix.len())?;
    if !candidate.eq_ignore_ascii_case(prefix) {
        return None;
    }
    value.get(prefix.len()..)
}

fn is_absolute_local_path(value: &str) -> bool {
    if std::path::Path::new(value).is_absolute() {
        return true;
    }

    let bytes = value.as_bytes();
    let windows_drive_path =
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/');
    windows_drive_path || bytes.starts_with(b"\\\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xmltv_source(url: &str) -> EpgSourceDto {
        EpgSourceDto { url: url.to_owned(), ..EpgSourceDto::default() }
    }

    fn ics_source(url: &str, channel_id: Option<&str>) -> EpgSourceDto {
        EpgSourceDto {
            source_type: EpgSourceTypeDto::Ics,
            url: url.to_owned(),
            channel_id: channel_id.map(ToOwned::to_owned),
            ..EpgSourceDto::default()
        }
    }

    #[test]
    fn xmltv_source_without_type_remains_valid() {
        let mut cfg =
            EpgConfigDto { sources: Some(vec![xmltv_source("http://example.com/xmltv.php")]), ..Default::default() };
        cfg.prepare(|| Err("no auto".to_owned()), true).expect("prepare failed");
        assert_eq!(cfg.t_sources.len(), 1);
        assert_eq!(cfg.t_sources[0].source_type, EpgSourceTypeDto::Xmltv);
        assert_eq!(cfg.t_sources[0].url, "http://example.com/xmltv.php");
    }

    #[test]
    fn provider_scheme_kept_unresolved() {
        let mut source = xmltv_source("provider://myprovider/xmltv.php?username=u&password=p");
        source.priority = 1;
        source.logo_override = true;
        let mut cfg = EpgConfigDto { sources: Some(vec![source]), ..Default::default() };
        cfg.prepare(|| Err("no auto".to_owned()), true).expect("prepare failed");
        assert_eq!(cfg.t_sources.len(), 1);
        assert_eq!(cfg.t_sources[0].url, "provider://myprovider/xmltv.php?username=u&password=p");
        assert_eq!(cfg.t_sources[0].priority, 1);
        assert!(cfg.t_sources[0].logo_override);
    }

    #[test]
    fn xmltv_auto_url_used() {
        let mut cfg = EpgConfigDto { sources: Some(vec![xmltv_source(AUTO_URL)]), ..Default::default() };
        cfg.prepare(|| Ok("http://auto.example.com/xmltv.php?username=u&password=p".to_owned()), true)
            .expect("prepare failed");
        assert_eq!(cfg.t_sources.len(), 1);
        assert!(cfg.t_sources[0].url.starts_with("http://auto.example.com/"));
    }

    #[test]
    fn include_computed_false_skips_resolution() {
        let mut cfg =
            EpgConfigDto { sources: Some(vec![xmltv_source("provider://myprovider/xmltv.php")]), ..Default::default() };
        cfg.prepare(|| Err("no auto".to_owned()), false).expect("prepare with include_computed=false should succeed");
        assert!(cfg.t_sources.is_empty());
    }

    #[test]
    fn ics_auto_url_is_rejected() {
        let mut cfg =
            EpgConfigDto { sources: Some(vec![ics_source(AUTO_URL, Some("f1.calendar"))]), ..Default::default() };
        let err = cfg.prepare(|| Ok("http://auto.example.com/xmltv.php".to_owned()), true).unwrap_err();
        assert!(err.to_string().contains("only supported for XMLTV"));
    }

    #[test]
    fn ics_requires_channel_id() {
        let mut cfg =
            EpgConfigDto { sources: Some(vec![ics_source("https://example.com/f1.ics", None)]), ..Default::default() };
        let err = cfg.prepare(|| Err("no auto".to_owned()), true).unwrap_err();
        assert!(err.to_string().contains("channel_id is required"));
    }

    #[test]
    fn ics_webcal_url_is_normalized_to_https() {
        for url in ["webcal://example.com/f1.ics", "WEBcal://example.com/f1.ics"] {
            let mut cfg =
                EpgConfigDto { sources: Some(vec![ics_source(url, Some("f1.calendar"))]), ..Default::default() };
            cfg.prepare(|| Err("no auto".to_owned()), true).expect("prepare failed");
            assert_eq!(cfg.t_sources[0].url, "https://example.com/f1.ics");
        }
    }

    #[test]
    fn ics_unicode_local_path_is_accepted_without_panicking() {
        let mut source = ics_source("äääää/calendar.ics", Some("calendar"));
        source.prepare().expect("unicode local path should be valid");
    }

    #[test]
    fn ics_absolute_windows_paths_are_local_sources() {
        for path in [r"C:\calendar.ics", "D:/calendar.ics", r"\\server\share\calendar.ics"] {
            let mut source = ics_source(path, Some("calendar"));
            source.prepare().expect("absolute Windows path should be valid");
        }
    }

    #[test]
    fn xmltv_rejects_ics_only_fields() {
        let mut with_ics_block = xmltv_source("https://example.com/epg.xml");
        with_ics_block.ics = Some(IcsEpgSourceConfigDto::default());
        let err = with_ics_block.prepare().unwrap_err();
        assert!(err.to_string().contains("only supported for ICS"));

        let mut with_channel_id = xmltv_source("https://example.com/epg.xml");
        with_channel_id.channel_id = Some("not-an-xmltv-option".to_string());
        let err = with_channel_id.prepare().unwrap_err();
        assert!(err.to_string().contains("only supported for ICS"));
    }

    #[test]
    fn ics_unsupported_scheme_is_rejected() {
        let mut cfg = EpgConfigDto {
            sources: Some(vec![ics_source("ftp://example.com/f1.ics", Some("f1.calendar"))]),
            ..Default::default()
        };
        let err = cfg.prepare(|| Err("no auto".to_owned()), true).unwrap_err();
        assert!(err.to_string().contains("Unsupported ICS url scheme"));
    }

    #[test]
    fn ics_match_names_are_trimmed_and_empty_values_removed() {
        let mut source = ics_source("https://example.com/f1.ics", Some(" f1.calendar "));
        source.channel_title = Some(" Formula 1 ".to_owned());
        source.match_names = vec![" F1 ".to_owned(), String::new(), "  ".to_owned(), "Formel 1".to_owned()];
        let mut cfg = EpgConfigDto { sources: Some(vec![source]), ..Default::default() };
        cfg.prepare(|| Err("no auto".to_owned()), true).expect("prepare failed");
        assert_eq!(cfg.t_sources[0].channel_id.as_deref(), Some("f1.calendar"));
        assert_eq!(cfg.t_sources[0].channel_title.as_deref(), Some("Formula 1"));
        assert_eq!(cfg.t_sources[0].match_names, vec!["F1", "Formel 1"]);
    }

    #[test]
    fn aliases_field_is_rejected() {
        let yaml = r"
type: ics
url: https://example.com/f1.ics
channel_id: f1.calendar
aliases:
  - F1
";
        let err = serde_saphyr::from_str::<EpgSourceDto>(yaml).unwrap_err();
        assert!(err.to_string().contains("aliases") || err.to_string().contains("unknown field"));
    }

    #[test]
    fn ics_block_hours_must_divide_day_evenly() {
        let mut valid = ics_source("https://example.com/f1.ics", Some("f1.calendar"));
        valid.ics = Some(IcsEpgSourceConfigDto {
            dummy: IcsDummyConfigDto { block_hours: 4, ..IcsDummyConfigDto::default() },
            ..IcsEpgSourceConfigDto::default()
        });
        valid.prepare().expect("valid block size");

        let mut invalid = ics_source("https://example.com/f1.ics", Some("f1.calendar"));
        invalid.ics = Some(IcsEpgSourceConfigDto {
            dummy: IcsDummyConfigDto { block_hours: 5, ..IcsDummyConfigDto::default() },
            ..IcsEpgSourceConfigDto::default()
        });
        let err = invalid.prepare().unwrap_err();
        assert!(err.to_string().contains("must divide 24 evenly"));
    }

    #[test]
    fn invalid_ics_timezone_is_rejected() {
        let mut source = ics_source("https://example.com/f1.ics", Some("f1.calendar"));
        source.ics =
            Some(IcsEpgSourceConfigDto { timezone: "Mars/Olympus".to_string(), ..IcsEpgSourceConfigDto::default() });

        let err = source.prepare().unwrap_err();

        assert!(err.to_string().contains("ics.timezone"));
    }

    #[test]
    fn ics_max_download_bytes_above_hard_cap_is_rejected() {
        let mut source = ics_source("https://example.com/f1.ics", Some("f1.calendar"));
        source.ics = Some(IcsEpgSourceConfigDto {
            max_download_bytes: MAX_ICS_DOWNLOAD_BYTES_HARD_LIMIT + 1,
            ..IcsEpgSourceConfigDto::default()
        });

        let err = source.prepare().unwrap_err();

        assert!(err.to_string().contains("ics.max_download_bytes"));
    }

    #[test]
    fn ics_max_decompressed_bytes_above_hard_cap_is_rejected() {
        let mut source = ics_source("https://example.com/f1.ics", Some("f1.calendar"));
        source.ics = Some(IcsEpgSourceConfigDto {
            max_decompressed_bytes: MAX_ICS_DECOMPRESSED_BYTES_HARD_LIMIT + 1,
            ..IcsEpgSourceConfigDto::default()
        });

        let err = source.prepare().unwrap_err();

        assert!(err.to_string().contains("ics.max_decompressed_bytes"));
    }

    #[test]
    fn ics_max_events_above_hard_cap_is_rejected() {
        let mut source = ics_source("https://example.com/f1.ics", Some("f1.calendar"));
        source.ics = Some(IcsEpgSourceConfigDto {
            max_events: MAX_ICS_EVENTS_HARD_LIMIT + 1,
            ..IcsEpgSourceConfigDto::default()
        });

        let err = source.prepare().unwrap_err();

        assert!(err.to_string().contains("ics.max_events"));
    }

    #[test]
    fn ics_dummy_days_above_hard_caps_are_rejected() {
        let mut too_much_past = ics_source("https://example.com/f1.ics", Some("f1.calendar"));
        too_much_past.ics = Some(IcsEpgSourceConfigDto {
            dummy: IcsDummyConfigDto { days_past: MAX_ICS_DAYS_PAST + 1, ..IcsDummyConfigDto::default() },
            ..IcsEpgSourceConfigDto::default()
        });
        let err = too_much_past.prepare().unwrap_err();
        assert!(err.to_string().contains("ics.dummy.days_past"));

        let mut too_much_future = ics_source("https://example.com/f1.ics", Some("f1.calendar"));
        too_much_future.ics = Some(IcsEpgSourceConfigDto {
            dummy: IcsDummyConfigDto { days_future: MAX_ICS_DAYS_FUTURE + 1, ..IcsDummyConfigDto::default() },
            ..IcsEpgSourceConfigDto::default()
        });
        let err = too_much_future.prepare().unwrap_err();
        assert!(err.to_string().contains("ics.dummy.days_future"));
    }

    #[test]
    fn ics_extreme_template_and_dummy_texts_are_rejected() {
        let mut source = ics_source("https://example.com/f1.ics", Some("f1.calendar"));
        source.ics = Some(IcsEpgSourceConfigDto {
            event: IcsEventMappingDto {
                title: "x".repeat(MAX_ICS_SUMMARY_LENGTH + 1),
                ..IcsEventMappingDto::default()
            },
            ..IcsEpgSourceConfigDto::default()
        });
        let err = source.prepare().unwrap_err();
        assert!(err.to_string().contains("ics.event.title"));

        let mut source = ics_source("https://example.com/f1.ics", Some("f1.calendar"));
        source.ics = Some(IcsEpgSourceConfigDto {
            dummy: IcsDummyConfigDto {
                description: "x".repeat(MAX_ICS_DESCRIPTION_LENGTH + 1),
                ..IcsDummyConfigDto::default()
            },
            ..IcsEpgSourceConfigDto::default()
        });
        let err = source.prepare().unwrap_err();
        assert!(err.to_string().contains("ics.dummy.description"));
    }
}
