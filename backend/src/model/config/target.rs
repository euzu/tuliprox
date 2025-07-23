use crate::model::mapping::Mapping;
use crate::model::config::trakt::TraktConfig;
use arc_swap::ArcSwapOption;
use shared::model::{ConfigTargetDto, ConfigTargetOptions, HdHomeRunTargetOutputDto, M3uTargetOutputDto, ProcessingOrder, StrmExportStyle, StrmTargetOutputDto, TargetOutputDto, TargetType, TraktConfigDto, XtreamTargetOutputDto};
use shared::model::PlaylistItemType;
use std::sync::Arc;
use regex::Regex;
use shared::foundation::filter::Filter;
use shared::foundation::filter::ValueProvider;
use crate::model::{macros, ConfigRename, ConfigSort};


#[derive(Clone, Debug)]
pub struct ProcessTargets {
    pub enabled: bool,
    pub inputs: Vec<u16>,
    pub targets: Vec<u16>,
    pub target_names: Vec<String>,
}

impl ProcessTargets {
    pub fn has_target(&self, tid: u16) -> bool {
        !self.enabled || self.targets.is_empty() || self.targets.contains(&tid)
    }

    pub fn has_input(&self, tid: u16) -> bool {
        !self.enabled || self.inputs.is_empty() || self.inputs.contains(&tid)
    }
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub struct XtreamTargetOutput {
    pub skip_live_direct_source: bool,
    pub skip_video_direct_source: bool,
    pub skip_series_direct_source: bool,
    pub resolve_series: bool,
    pub resolve_series_delay: u16,
    pub resolve_vod: bool,
    pub resolve_vod_delay: u16,
    pub trakt: Option<TraktConfig>,
}

macros::from_impl!(XtreamTargetOutput);
impl From<&XtreamTargetOutputDto> for XtreamTargetOutput {
    fn from(dto: &XtreamTargetOutputDto) -> Self {
        Self {
            skip_live_direct_source: dto.skip_live_direct_source,
            skip_video_direct_source: dto.skip_video_direct_source,
            skip_series_direct_source: dto.skip_series_direct_source,
            resolve_series: dto.resolve_series,
            resolve_series_delay: dto.resolve_series_delay,
            resolve_vod: dto.resolve_vod,
            resolve_vod_delay: dto.resolve_vod_delay,
            trakt: dto.trakt.as_ref().map(Into::into),
        }
    }
}

impl From<&XtreamTargetOutput> for XtreamTargetOutputDto {
    fn from(instance: &XtreamTargetOutput) -> Self {
        Self {
            skip_live_direct_source: instance.skip_live_direct_source,
            skip_video_direct_source: instance.skip_video_direct_source,
            skip_series_direct_source: instance.skip_series_direct_source,
            resolve_series: instance.resolve_series,
            resolve_series_delay: instance.resolve_series_delay,
            resolve_vod: instance.resolve_vod,
            resolve_vod_delay: instance.resolve_vod_delay,
            trakt: instance.trakt.as_ref().map(TraktConfigDto::from),
        }
    }
}


#[derive(Debug, Clone)]
pub struct M3uTargetOutput {
    pub filename: Option<String>,
    pub include_type_in_url: bool,
    pub mask_redirect_url: bool,
}

macros::from_impl!(M3uTargetOutput);
impl From<&M3uTargetOutputDto> for M3uTargetOutput {
    fn from(dto: &M3uTargetOutputDto) -> Self {
        Self {
            filename: dto.filename.clone(),
            include_type_in_url: dto.include_type_in_url,
            mask_redirect_url: dto.mask_redirect_url,
        }
    }
}
impl From<&M3uTargetOutput> for M3uTargetOutputDto {
    fn from(instance: &M3uTargetOutput) -> Self {
        Self {
            filename: instance.filename.clone(),
            include_type_in_url: instance.include_type_in_url,
            mask_redirect_url: instance.mask_redirect_url,
        }
    }
}


#[derive(Debug, Clone)]
pub struct StrmTargetOutput {
    pub directory: String,
    pub username: Option<String>,
    pub style: StrmExportStyle,
    pub flat: bool,
    pub underscore_whitespace: bool,
    pub cleanup: bool,
    pub strm_props: Option<Vec<String>>,
}

macros::from_impl!(StrmTargetOutput);
impl From<&StrmTargetOutputDto> for StrmTargetOutput {
    fn from(dto: &StrmTargetOutputDto) -> Self {
        Self {
            directory: dto.directory.to_string(),
            username: dto.username.clone(),
            style: dto.style,
            flat: dto.flat,
            underscore_whitespace: dto.underscore_whitespace,
            cleanup: dto.cleanup,
            strm_props: dto.strm_props.clone(),
        }
    }
}
impl From<&StrmTargetOutput> for StrmTargetOutputDto {
    fn from(instance: &StrmTargetOutput) -> Self {
        Self {
            directory: instance.directory.to_string(),
            username: instance.username.clone(),
            style: instance.style,
            flat: instance.flat,
            underscore_whitespace: instance.underscore_whitespace,
            cleanup: instance.cleanup,
            strm_props: instance.strm_props.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct HdHomeRunTargetOutput {
    pub device: String,
    pub username: String,
    pub use_output: Option<TargetType>,
}

macros::from_impl!(HdHomeRunTargetOutput);
impl From<&HdHomeRunTargetOutputDto> for HdHomeRunTargetOutput {
    fn from(dto: &HdHomeRunTargetOutputDto) -> Self {
        Self {
            device: dto.device.to_string(),
            username: dto.username.to_string(),
            use_output: dto.use_output,
        }
    }
}
impl From<&HdHomeRunTargetOutput> for HdHomeRunTargetOutputDto {
    fn from(instance: &HdHomeRunTargetOutput) -> Self {
        Self {
            device: instance.device.to_string(),
            username: instance.username.to_string(),
            use_output: instance.use_output,
        }
    }
}

#[derive(Debug, Clone)]
pub enum TargetOutput {
    Xtream(XtreamTargetOutput),
    M3u(M3uTargetOutput),
    Strm(StrmTargetOutput),
    HdHomeRun(HdHomeRunTargetOutput),
}

macros::from_impl!(TargetOutput);
impl From<&TargetOutputDto> for TargetOutput {
    fn from(dto: &TargetOutputDto) -> Self {
        match dto {
            TargetOutputDto::Xtream(o) => TargetOutput::Xtream(XtreamTargetOutput::from(o)),
            TargetOutputDto::M3u(o)  => TargetOutput::M3u(M3uTargetOutput::from(o)),
            TargetOutputDto::Strm(o)  => TargetOutput::Strm(StrmTargetOutput::from(o)),
            TargetOutputDto::HdHomeRun(o) =>  TargetOutput::HdHomeRun(HdHomeRunTargetOutput::from(o)),
        }
    }
}

impl From<&TargetOutput> for TargetOutputDto {
    fn from(instance: &TargetOutput) -> Self {
        match instance {
            TargetOutput::Xtream(o) => TargetOutputDto::Xtream(XtreamTargetOutputDto::from(o)),
            TargetOutput::M3u(o)  => TargetOutputDto::M3u(M3uTargetOutputDto::from(o)),
            TargetOutput::Strm(o)  => TargetOutputDto::Strm(StrmTargetOutputDto::from(o)),
            TargetOutput::HdHomeRun(o) =>  TargetOutputDto::HdHomeRun(HdHomeRunTargetOutputDto::from(o)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConfigTarget {
    pub id: u16,
    pub enabled: bool,
    pub name: String,
    pub options: Option<ConfigTargetOptions>,
    pub sort: Option<ConfigSort>,
    pub filter: Filter,
    pub output: Vec<TargetOutput>,
    pub rename: Option<Vec<ConfigRename>>,
    pub mapping_ids: Option<Vec<String>>,
    pub mapping: Arc<ArcSwapOption<Vec<Mapping>>>,
    pub processing_order: ProcessingOrder,
    pub watch: Option<Vec<regex::Regex>>,
}

impl ConfigTarget {

    pub fn filter(&self, provider: &ValueProvider) -> bool {
      self.filter.filter(provider)
    }

    pub(crate) fn get_xtream_output(&self) -> Option<&XtreamTargetOutput> {
        if let Some(TargetOutput::Xtream(output)) = self.output.iter().find(|o| matches!(o, TargetOutput::Xtream(_))) {
            Some(output)
        } else {
            None
        }
    }

    pub(crate) fn get_m3u_output(&self) -> Option<&M3uTargetOutput> {
        if let Some(TargetOutput::M3u(output)) = self.output.iter().find(|o| matches!(o, TargetOutput::M3u(_))) {
            Some(output)
        } else {
            None
        }
    }

    // pub(crate) fn get_strm_output(&self) -> Option<&StrmTargetOutput> {
    //     if let Some(TargetOutput::Strm(output)) = self.output.iter().find(|o| matches!(o, TargetOutput::Strm(_))) {
    //         Some(output)
    //     } else {
    //         None
    //     }
    // }

    pub(crate) fn get_hdhomerun_output(&self) -> Option<&HdHomeRunTargetOutput> {
        if let Some(TargetOutput::HdHomeRun(output)) = self.output.iter().find(|o| matches!(o, TargetOutput::HdHomeRun(_))) {
            Some(output)
        } else {
            None
        }
    }

    pub fn has_output(&self, tt: &TargetType) -> bool {
        for target_output in &self.output {
            match target_output {
                TargetOutput::Xtream(_) => { if tt == &TargetType::Xtream { return true; } }
                TargetOutput::M3u(_) => { if tt == &TargetType::M3u { return true; } }
                TargetOutput::Strm(_) => { if tt == &TargetType::Strm { return true; } }
                TargetOutput::HdHomeRun(_) => { if tt == &TargetType::HdHomeRun { return true; } }
            }
        }
        false
    }

    pub fn is_force_redirect(&self, item_type: PlaylistItemType) -> bool {
        self.options
            .as_ref()
            .and_then(|options| options.force_redirect.as_ref())
            .is_some_and(|flags| flags.has_cluster(item_type))
    }
}

macros::from_impl!(ConfigTarget);
impl From<&ConfigTargetDto> for ConfigTarget {
    fn from(dto: &ConfigTargetDto) -> Self {

        Self {
            id: dto.id,
            enabled: dto.enabled,
            name: dto.name.to_string(),
            options: dto.options.clone(),
            sort: dto.sort.as_ref().map(Into::into),
            filter: dto.t_filter.as_ref().unwrap().clone(),
            output: dto.output.iter().map(Into::into).collect(),
            rename: dto.rename.as_ref().map(|l| l.iter().map(Into::into).collect()),
            mapping_ids: dto.mapping.clone(),
            mapping: Arc::new(ArcSwapOption::new(None)),
            processing_order: dto.processing_order,
            watch: dto.watch.as_ref().map(|list| list.iter().filter_map(|s| Regex::new(s).ok()).collect())
        }
    }
}