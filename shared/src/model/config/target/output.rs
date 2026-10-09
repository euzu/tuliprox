use crate::{
    defaults::{default_as_true, is_false, is_true},
    error::TuliproxError,
    foundation::{get_filter, Filter},
    model::{PatternTemplate, Prepare, StrmExportStyle, TargetType, TraktConfigDto},
    utils::is_blank_optional_string,
};

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
            trakt.prepare()?;
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
    fn default() -> Self { Self { device: String::new(), username: String::new(), use_output: Some(TargetType::M3u) } }
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
