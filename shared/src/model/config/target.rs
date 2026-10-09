use crate::{
    defaults::{
        default_as_default, default_as_true, is_config_target_options_empty, is_default_processing_order, is_false,
        is_true, is_zero_u16,
    },
    error::TuliproxError,
    model::{
        ConfigFavouritesDto, ConfigRenameDto, ConfigSortDto, CurationConfigDto, HdHomeRunDeviceOverview,
        PatternTemplate, Prepare, PrepareAll, ProcessingOrder, TargetType,
    },
};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StagedTargetFilterDto {
    #[serde(default)]
    processing: Option<String>,
    #[serde(default)]
    persist: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigTargetDto {
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub id: u16,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default = "default_as_default")]
    pub name: String,
    #[serde(default, skip_serializing_if = "is_config_target_options_empty")]
    pub options: Option<ConfigTargetOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<ConfigSortDto>,
    #[serde(default, skip_serializing_if = "ConfigTargetFilterDto::is_empty")]
    pub filter: ConfigTargetFilterDto,
    #[serde(default)]
    pub output: Vec<TargetOutputDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curation: Option<CurationConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rename: Option<Vec<ConfigRenameDto>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapping: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favourites: Option<Vec<ConfigFavouritesDto>>,
    #[serde(default, skip_serializing_if = "is_default_processing_order")]
    pub processing_order: ProcessingOrder,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub use_memory_cache: bool,
}

impl Default for ConfigTargetDto {
    fn default() -> Self {
        ConfigTargetDto {
            id: 0,
            enabled: default_as_true(),
            name: default_as_default(),
            options: None,
            sort: None,
            filter: ConfigTargetFilterDto::default(),
            output: Vec::new(),
            curation: None,
            rename: None,
            mapping: None,
            favourites: None,
            processing_order: ProcessingOrder::default(),
            watch: None,
            use_memory_cache: false,
        }
    }
}

impl ConfigTargetDto {
    pub fn prepare(
        &mut self,
        id: u16,
        templates: Option<&[PatternTemplate]>,
        hdhr_config: Option<&HdHomeRunDeviceOverview>,
    ) -> Result<(), TuliproxError> {
        self.id = id;
        if self.output.is_empty() {
            return Err(TuliproxError::ConfigTarget(format!("Missing output format for {}", self.name)));
        }
        self.name = self.name.trim().to_string();
        if self.name.is_empty() {
            return Err(TuliproxError::ConfigTarget("target name required".to_string()));
        }

        if let Some(curation) = &mut self.curation {
            curation.prepare(&self.output)?;
        }

        let mut m3u_cnt = 0;
        let mut xtream_cnt = 0;
        let mut strm_cnt = 0;
        let mut hdhr_cnt = 0;
        let mut hdhomerun_needs_m3u = false;
        let mut hdhomerun_needs_xtream = false;

        //let mut strm_export_styles = vec![];
        let mut strm_directories: Vec<&str> = vec![];

        for target_output in &mut self.output {
            target_output.prepare(templates)?;
            match target_output {
                TargetOutputDto::Xtream(_) => {
                    xtream_cnt += 1;
                    if default_as_default().eq_ignore_ascii_case(&self.name) {
                        return Err(TuliproxError::ConfigTarget(format!(
                            "unique target name is required for xtream type output: {}",
                            self.name
                        )));
                    }
                }
                TargetOutputDto::M3u(m3u_output) => {
                    m3u_cnt += 1;
                    m3u_output.filename = m3u_output.filename.as_ref().and_then(|s| {
                        let trimmed = s.trim();
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed.to_string())
                        }
                    });
                }
                TargetOutputDto::Strm(strm_output) => {
                    strm_cnt += 1;
                    strm_output.directory = strm_output.directory.trim().to_string();
                    if strm_output.directory.trim().is_empty() {
                        return Err(TuliproxError::ConfigTarget(format!(
                            "directory is required for strm type: {}",
                            self.name
                        )));
                    }
                    if let Some(username) = &mut strm_output.username {
                        *username = username.trim().to_string();
                    }
                    // if strm_export_styles.contains(&strm_output.style) {
                    //     return Err(TuliproxError::ConfigTarget(format!("strm outputs with same export style are not allowed: {}", self.name)));
                    // }
                    // strm_export_styles.push(strm_output.style);
                    if strm_directories.contains(&strm_output.directory.as_str()) {
                        return Err(TuliproxError::ConfigTarget(format!(
                            "strm outputs with same export directory are not allowed: {}",
                            self.name
                        )));
                    }
                    strm_directories.push(strm_output.directory.as_str());
                }
                TargetOutputDto::HdHomeRun(hdhomerun_output) => {
                    hdhr_cnt += 1;
                    hdhomerun_output.username = hdhomerun_output.username.trim().to_string();
                    if hdhomerun_output.username.is_empty() {
                        return Err(TuliproxError::ConfigTarget(format!(
                            "Username is required for HdHomeRun type: {}",
                            self.name
                        )));
                    }

                    hdhomerun_output.device = hdhomerun_output.device.trim().to_string();
                    if hdhomerun_output.device.is_empty() {
                        return Err(TuliproxError::ConfigTarget(format!(
                            "Device is required for HdHomeRun type: {}",
                            self.name
                        )));
                    }

                    if let Some(use_output) = hdhomerun_output.use_output.as_ref() {
                        match &use_output {
                            TargetType::M3u => {
                                hdhomerun_needs_m3u = true;
                            }
                            TargetType::Xtream => {
                                hdhomerun_needs_xtream = true;
                            }
                            _ => {
                                return Err(TuliproxError::ConfigTarget(format!(
                                "HdHomeRun output option `use_output` only accepts `m3u` or `xtream` for target: {}",
                                self.name
                            )))
                            }
                        }
                    }
                    if let Some(hdhr_devices) = hdhr_config {
                        if !hdhr_devices.devices.contains(&hdhomerun_output.device) {
                            return Err(TuliproxError::ConfigTarget(format!(
                                "HdHomeRun output device is not defined: {}",
                                hdhomerun_output.device
                            )));
                        }
                    }
                }
            }
        }

        if m3u_cnt > 1 || xtream_cnt > 1 || hdhr_cnt > 1 {
            return Err(TuliproxError::ConfigTarget(format!("Multiple output formats with same type : {}", self.name)));
        }

        if strm_cnt > 0 && xtream_cnt == 0 && m3u_cnt == 0 {
            return Err(TuliproxError::ConfigTarget(format!(
                "strm output is only permitted when used in combination with xtream or m3u output: {}",
                self.name
            )));
        }

        if hdhr_cnt > 0 {
            if xtream_cnt == 0 && m3u_cnt == 0 {
                return Err(TuliproxError::ConfigTarget(format!(
                    "HdHomeRun output is only permitted when used in combination with xtream or m3u output: {}",
                    self.name
                )));
            }
            if hdhomerun_needs_m3u && m3u_cnt == 0 {
                return Err(TuliproxError::ConfigTarget(format!(
                    "HdHomeRun output has `use_output=m3u` but no `m3u` output defined: {}",
                    self.name
                )));
            }
            if hdhomerun_needs_xtream && xtream_cnt == 0 {
                return Err(TuliproxError::ConfigTarget(format!(
                    "HdHomeRun output has `use_output=xtream` but no `xtream` output defined: {}",
                    self.name
                )));
            }

            if let Some(hdhr_devices) = hdhr_config {
                if !hdhr_devices.enabled {
                    log::warn!("You have defined an HDHomeRun output, but HDHomeRun devices are disabled.");
                }
            }
        }

        self.favourites.prepare(templates)?;

        if let Some(watch) = &self.watch {
            for pat in watch {
                if let Err(err) = crate::model::REGEX_CACHE.get_or_compile(pat) {
                    return Err(TuliproxError::ConfigTarget(format!("Invalid watch regular expression: {err}")));
                }
            }
        }

        self.filter.prepare(templates)?;
        self.rename.prepare_all(templates)?;
        if let Some(sort) = self.sort.as_mut() {
            sort.prepare(templates)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;

mod filter;
mod options;
mod output;
pub use filter::ConfigTargetFilterDto;
pub use options::{
    ConfigTargetOptions, ConfigTargetShareLiveStreams, DeduplicateConfig, DeduplicateKeep, DeduplicateMatchBy,
    EpgOutputOptions,
};
pub use output::{
    HdHomeRunTargetOutputDto, M3uTargetOutputDto, StrmTargetOutputDto, TargetOutputDto, XtreamTargetOutputDto,
};
