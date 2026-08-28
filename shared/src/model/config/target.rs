use crate::{
    defaults::{
        default_as_default, default_as_true, is_config_target_options_empty, is_default_processing_order, is_false,
        is_true, is_zero_u16,
    },
    error::TuliproxError,
    foundation::{get_filter, Filter},
    model::{
        ClusterFlags, ConfigFavouritesDto, ConfigRenameDto, ConfigSortDto, HdHomeRunDeviceOverview, PatternTemplate,
        Prepare, PrepareAll, ProcessingOrder, StrmExportStyle, TargetType, TraktConfigDto,
    },
    utils::is_blank_optional_string,
};

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigTargetShareLiveStreams {
    #[serde(default, skip_serializing_if = "is_false")]
    pub hls: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub mpeg_ts: bool,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum ConfigTargetShareLiveStreamsCompat {
    Legacy(bool),
    Structured(ConfigTargetShareLiveStreams),
}

fn deserialize_share_live_streams<'de, D>(deserializer: D) -> Result<ConfigTargetShareLiveStreams, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match <ConfigTargetShareLiveStreamsCompat as serde::Deserialize>::deserialize(deserializer)? {
        ConfigTargetShareLiveStreamsCompat::Legacy(enabled) => {
            ConfigTargetShareLiveStreams { hls: false, mpeg_ts: enabled }
        }
        ConfigTargetShareLiveStreamsCompat::Structured(config) => config,
    })
}

impl ConfigTargetShareLiveStreams {
    pub fn is_empty(&self) -> bool {
        !self.hls && !self.mpeg_ts
    }
}

/// Controls optional canonicalization of EPG data emitted for a target.
#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EpgOutputOptions {
    #[serde(default, skip_serializing_if = "is_false")]
    pub lowercase_ids: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub lowercase_xmltv_display_names: bool,
}

impl EpgOutputOptions {
    pub const fn is_empty(&self) -> bool {
        !self.lowercase_ids && !self.lowercase_xmltv_display_names
    }
}

#[derive(Debug, Copy, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeduplicateMatchBy {
    #[default]
    Caption,
    Name,
    Title,
}

#[derive(Debug, Copy, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeduplicateKeep {
    #[default]
    BestQuality,
    First,
}

/// Quality-aware duplicate removal: channels with the same normalized match
/// value (quality tokens stripped) collapse to a single entry.
#[derive(Debug, Copy, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeduplicateConfig {
    #[serde(default)]
    pub match_by: DeduplicateMatchBy,
    #[serde(default)]
    pub keep: DeduplicateKeep,
    /// Normalize accented characters in match keys ("Café HD" matches "Cafe FHD").
    #[serde(default)]
    pub match_as_ascii: bool,
}

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigTargetOptions {
    #[serde(default, skip_serializing_if = "is_false")]
    pub ignore_logo: bool,
    #[serde(
        default,
        deserialize_with = "deserialize_share_live_streams",
        skip_serializing_if = "ConfigTargetShareLiveStreams::is_empty"
    )]
    pub share_live_streams: ConfigTargetShareLiveStreams,
    #[serde(default, skip_serializing_if = "is_false")]
    pub remove_duplicates: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deduplicate: Option<DeduplicateConfig>,
    #[serde(default, skip_serializing_if = "EpgOutputOptions::is_empty")]
    pub epg_output: EpgOutputOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force_redirect: Option<ClusterFlags>,
}

impl ConfigTargetOptions {
    pub fn is_empty(&self) -> bool {
        !self.ignore_logo
            && self.share_live_streams.is_empty()
            && !self.remove_duplicates
            && self.deduplicate.is_none()
            && self.epg_output.is_empty()
            && self.force_redirect.is_none_or(|f| f.has_full_flags() || f.is_empty())
    }

    pub const fn lowercase_epg_ids(&self) -> bool {
        self.epg_output.lowercase_ids
    }

    pub const fn lowercase_xmltv_display_names(&self) -> bool {
        self.epg_output.lowercase_xmltv_display_names
    }

    pub fn share_live_hls_enabled(&self) -> bool {
        self.share_live_streams.hls
    }

    pub fn share_live_mpeg_ts_enabled(&self) -> bool {
        self.share_live_streams.mpeg_ts
    }

    pub fn share_live_any_enabled(&self) -> bool {
        self.share_live_hls_enabled() || self.share_live_mpeg_ts_enabled()
    }
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct XtreamTargetOutputDto {
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub skip_live_direct_source: bool,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub skip_video_direct_source: bool,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub skip_series_direct_source: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trakt: Option<TraktConfigDto>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub filter: Option<String>,
    #[serde(skip)]
    pub t_filter: Option<Filter>,
}

impl Default for XtreamTargetOutputDto {
    fn default() -> Self {
        XtreamTargetOutputDto {
            skip_live_direct_source: default_as_true(),
            skip_video_direct_source: default_as_true(),
            skip_series_direct_source: default_as_true(),
            trakt: None,
            filter: None,
            t_filter: None,
        }
    }
}

impl XtreamTargetOutputDto {
    pub fn has_any_option(&self) -> bool {
        self.skip_live_direct_source
            || self.skip_video_direct_source
            || self.skip_series_direct_source
            || self.trakt.is_some()
            || self.filter.is_some()
    }
}

impl Prepare for XtreamTargetOutputDto {
    type Ctx<'a> = Option<&'a [PatternTemplate]>;

    fn prepare(&mut self, templates: Self::Ctx<'_>) -> Result<(), TuliproxError> {
        if let Some(raw_filter) = &self.filter {
            self.t_filter = Some(get_filter(raw_filter, templates)?);
        }
        if let Some(trakt) = &mut self.trakt {
            trakt.prepare();
        }
        Ok(())
    }
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct M3uTargetOutputDto {
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_type_in_url: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub mask_redirect_url: bool,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub filter: Option<String>,
    #[serde(skip)]
    pub t_filter: Option<Filter>,
}

impl M3uTargetOutputDto {
    pub fn has_any_option(&self) -> bool {
        self.filename.is_some() || self.include_type_in_url || self.mask_redirect_url || self.filter.is_some()
    }
}

impl Prepare for M3uTargetOutputDto {
    type Ctx<'a> = Option<&'a [PatternTemplate]>;

    fn prepare(&mut self, templates: Self::Ctx<'_>) -> Result<(), TuliproxError> {
        if let Some(raw_filter) = &self.filter {
            self.t_filter = Some(get_filter(raw_filter, templates)?);
        }
        Ok(())
    }
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StrmTargetOutputDto {
    pub directory: String,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub username: Option<String>,
    #[serde(default)]
    pub style: StrmExportStyle,
    #[serde(default, skip_serializing_if = "is_false")]
    pub flat: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub underscore_whitespace: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub cleanup: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strm_props: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub filter: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub add_quality_to_filename: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub use_metadata: bool,

    // New Fields for Metadata and Probe
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_probe_size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_analyze_duration: Option<u64>,

    #[serde(skip)]
    pub t_filter: Option<Filter>,
}

impl Prepare for StrmTargetOutputDto {
    type Ctx<'a> = Option<&'a [PatternTemplate]>;

    fn prepare(&mut self, templates: Self::Ctx<'_>) -> Result<(), TuliproxError> {
        if let Some(raw_filter) = &self.filter {
            self.t_filter = Some(get_filter(raw_filter, templates)?);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HdHomeRunTargetOutputDto {
    pub device: String,
    pub username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_output: Option<TargetType>,
}

impl Default for HdHomeRunTargetOutputDto {
    fn default() -> Self {
        Self { device: String::new(), username: String::new(), use_output: Some(TargetType::M3u) }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields, tag = "type", rename_all = "lowercase")]
pub enum TargetOutputDto {
    Xtream(XtreamTargetOutputDto),
    M3u(M3uTargetOutputDto),
    Strm(StrmTargetOutputDto),
    HdHomeRun(HdHomeRunTargetOutputDto),
}

impl Prepare for TargetOutputDto {
    type Ctx<'a> = Option<&'a [PatternTemplate]>;

    fn prepare(&mut self, templates: Self::Ctx<'_>) -> Result<(), TuliproxError> {
        match self {
            TargetOutputDto::Xtream(output) => output.prepare(templates),
            TargetOutputDto::M3u(output) => output.prepare(templates),
            TargetOutputDto::Strm(output) => output.prepare(templates),
            TargetOutputDto::HdHomeRun(_) => Ok(()),
        }
    }
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
    pub filter: String,
    #[serde(default)]
    pub output: Vec<TargetOutputDto>,
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
    #[serde(skip)]
    pub t_filter: Option<Filter>,
}

impl Default for ConfigTargetDto {
    fn default() -> Self {
        ConfigTargetDto {
            id: 0,
            enabled: default_as_true(),
            name: default_as_default(),
            options: None,
            sort: None,
            filter: String::new(),
            output: Vec::new(),
            rename: None,
            mapping: None,
            favourites: None,
            processing_order: ProcessingOrder::default(),
            watch: None,
            use_memory_cache: false,
            t_filter: None,
        }
    }
}

impl ConfigTargetDto {
    #[allow(clippy::too_many_lines)]
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

        match get_filter(&self.filter, templates) {
            Ok(fltr) => {
                // debug!("Filter: {}", fltr);
                self.t_filter = Some(fltr);
                self.rename.prepare_all(templates)?;
                if let Some(sort) = self.sort.as_mut() {
                    sort.prepare(templates)?;
                }
                Ok(())
            }
            Err(err) => Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConfigTargetDto, ConfigTargetOptions, ConfigTargetShareLiveStreams, EpgOutputOptions, M3uTargetOutputDto,
        StrmTargetOutputDto, TargetOutputDto, XtreamTargetOutputDto,
    };

    fn target_with_outputs(output: Vec<TargetOutputDto>) -> ConfigTargetDto {
        ConfigTargetDto {
            name: "target".to_string(),
            filter: "Group ~ \".*\"".to_string(),
            output,
            ..ConfigTargetDto::default()
        }
    }

    fn strm_with_username() -> TargetOutputDto {
        TargetOutputDto::Strm(StrmTargetOutputDto {
            directory: "/tmp/strm".to_string(),
            username: Some("alice".to_string()),
            ..StrmTargetOutputDto::default()
        })
    }

    fn strm_without_username() -> TargetOutputDto {
        TargetOutputDto::Strm(StrmTargetOutputDto {
            directory: "/tmp/strm".to_string(),
            ..StrmTargetOutputDto::default()
        })
    }

    fn xtream_output() -> TargetOutputDto {
        TargetOutputDto::Xtream(XtreamTargetOutputDto::default())
    }

    #[test]
    fn strm_with_username_is_allowed_with_m3u_output() {
        let mut target =
            target_with_outputs(vec![TargetOutputDto::M3u(M3uTargetOutputDto::default()), strm_with_username()]);

        assert!(target.prepare(1, None, None).is_ok());
    }

    #[test]
    fn strm_with_username_is_allowed_with_xtream_output() {
        let mut target = target_with_outputs(vec![xtream_output(), strm_with_username()]);

        assert!(target.prepare(1, None, None).is_ok());
    }

    #[test]
    fn strm_without_username_is_allowed_with_m3u_output() {
        let mut target =
            target_with_outputs(vec![TargetOutputDto::M3u(M3uTargetOutputDto::default()), strm_without_username()]);

        assert!(target.prepare(1, None, None).is_ok());
    }

    #[test]
    fn strm_with_username_requires_m3u_or_xtream_output() {
        let mut target = target_with_outputs(vec![strm_with_username()]);

        let err = target.prepare(1, None, None).expect_err("STRM username without stream output should fail");

        assert!(err.to_string().contains("xtream or m3u output"));
    }

    #[test]
    fn strm_without_username_requires_m3u_or_xtream_output() {
        let mut target = target_with_outputs(vec![strm_without_username()]);

        let err = target.prepare(1, None, None).expect_err("STRM without stream output should fail");

        assert!(err.to_string().contains("xtream or m3u output"));
    }

    #[test]
    fn target_options_deserialize_structured_share_live_streams() {
        let yaml = r"
share_live_streams:
  hls: true
  mpeg_ts: true
";

        let options: ConfigTargetOptions =
            serde_saphyr::from_str(yaml).expect("structured share_live_streams should deserialize");

        assert!(options.share_live_hls_enabled());
        assert!(options.share_live_mpeg_ts_enabled());
        assert!(options.share_live_any_enabled());
    }

    #[test]
    fn target_options_maps_legacy_true_share_live_streams_to_both_modes() {
        let yaml = r"
share_live_streams: true
";

        let options = serde_saphyr::from_str::<ConfigTargetOptions>(yaml);

        assert!(options.is_ok(), "legacy boolean should deserialize: {options:?}");
        if let Ok(options) = options {
            assert_eq!(options.share_live_streams, ConfigTargetShareLiveStreams { hls: false, mpeg_ts: true });
        }
    }

    #[test]
    fn target_options_maps_legacy_false_share_live_streams_to_both_modes() {
        let yaml = r"
share_live_streams: false
";

        let options: ConfigTargetOptions =
            serde_saphyr::from_str(yaml).expect("legacy false share_live_streams should deserialize");

        assert_eq!(options.share_live_streams, ConfigTargetShareLiveStreams { hls: false, mpeg_ts: false });
    }

    #[test]
    fn target_options_omit_default_share_live_streams() {
        let options = ConfigTargetOptions::default();

        assert!(options.is_empty());

        let serialized = serde_saphyr::to_string(&options).expect("default options should serialize");
        assert!(
            !serialized.contains("share_live_streams"),
            "default share_live_streams should be omitted, got: {serialized}"
        );
    }

    #[test]
    fn target_options_round_trips_partial_share_live_streams() {
        let options = ConfigTargetOptions {
            share_live_streams: ConfigTargetShareLiveStreams { hls: true, mpeg_ts: false },
            ..ConfigTargetOptions::default()
        };

        let serialized = serde_saphyr::to_string(&options).expect("partial share_live_streams should serialize");
        let reparsed: ConfigTargetOptions =
            serde_saphyr::from_str(&serialized).expect("partial share_live_streams should deserialize");

        assert_eq!(reparsed.share_live_streams, options.share_live_streams);
    }

    #[test]
    fn target_options_default_epg_output_is_disabled_and_omitted() {
        let options = serde_saphyr::from_str::<ConfigTargetOptions>("{}")
            .expect("target options without epg_output should deserialize");

        assert!(!options.lowercase_epg_ids());
        assert!(!options.lowercase_xmltv_display_names());
        assert!(options.epg_output.is_empty());
        assert!(options.is_empty());

        let serialized = serde_saphyr::to_string(&options).expect("default target options should serialize");
        assert!(!serialized.contains("epg_output"), "default epg_output should be omitted, got: {serialized}");
    }

    #[test]
    fn target_options_epg_output_roundtrips() {
        let yaml = r"
epg_output:
  lowercase_ids: true
  lowercase_xmltv_display_names: true
";

        let options =
            serde_saphyr::from_str::<ConfigTargetOptions>(yaml).expect("configured epg_output should deserialize");

        assert!(options.lowercase_epg_ids());
        assert!(options.lowercase_xmltv_display_names());
        assert!(!options.is_empty());

        let serialized = serde_saphyr::to_string(&options).expect("configured epg_output should serialize");
        let roundtripped = serde_saphyr::from_str::<ConfigTargetOptions>(&serialized)
            .expect("serialized epg_output should deserialize");
        assert_eq!(roundtripped, options);
    }

    #[test]
    fn target_options_epg_output_makes_options_nonempty() {
        let lowercase_ids = ConfigTargetOptions {
            epg_output: EpgOutputOptions { lowercase_ids: true, ..EpgOutputOptions::default() },
            ..ConfigTargetOptions::default()
        };
        let lowercase_display_names = ConfigTargetOptions {
            epg_output: EpgOutputOptions { lowercase_xmltv_display_names: true, ..EpgOutputOptions::default() },
            ..ConfigTargetOptions::default()
        };

        assert!(!lowercase_ids.is_empty());
        assert!(!lowercase_display_names.is_empty());
    }

    #[test]
    fn target_options_reject_unknown_epg_output_fields() {
        let yaml = r"
epg_output:
  lowercase_id: true
";

        let result = serde_saphyr::from_str::<ConfigTargetOptions>(yaml);

        assert!(result.is_err(), "unknown epg_output fields must be rejected");
    }

    #[test]
    fn target_options_mpeg_ts_helper_keeps_existing_stream_share_semantics() {
        let hls_only = ConfigTargetOptions {
            share_live_streams: ConfigTargetShareLiveStreams { hls: true, mpeg_ts: false },
            ..Default::default()
        };
        let mpeg_ts = ConfigTargetOptions {
            share_live_streams: ConfigTargetShareLiveStreams { hls: false, mpeg_ts: true },
            ..Default::default()
        };

        assert!(hls_only.share_live_hls_enabled());
        assert!(!hls_only.share_live_mpeg_ts_enabled());
        assert!(mpeg_ts.share_live_mpeg_ts_enabled());
    }
}
