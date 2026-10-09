use super::{
    ics::{strip_prefix_ignore_ascii_case, validate_ics_config, validate_ics_url_scheme},
    EpgConfigDto, EpgSourceDto, EpgSourceTypeDto, IcsEpgSourceConfigDto, AUTO_URL,
};
use crate::error::TuliproxError;

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

    pub(super) fn prepare_xmltv(&self) -> Result<(), TuliproxError> {
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

    pub(super) fn prepare_ics(&mut self) -> Result<(), TuliproxError> {
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

    pub fn is_valid(&self) -> bool { !self.url.is_empty() }

    pub fn source_identity(&self) -> String {
        match self.source_type {
            EpgSourceTypeDto::Xmltv => format!("xmltv|{}", self.url),
            EpgSourceTypeDto::Ics => {
                format!("ics|{}|{}", self.url, self.channel_id.as_deref().unwrap_or_default())
            }
        }
    }
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

pub(super) fn is_absolute_local_path(value: &str) -> bool {
    if std::path::Path::new(value).is_absolute() {
        return true;
    }

    let bytes = value.as_bytes();
    let windows_drive_path =
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/');
    windows_drive_path || bytes.starts_with(b"\\\\")
}
