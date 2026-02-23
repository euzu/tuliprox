use crate::model::config::favourites::ConfigFavourites;
use crate::model::config::trakt::TraktConfig;
use crate::model::mapping::Mapping;
use crate::model::{macros, ConfigRename, ConfigSort};
use arc_swap::ArcSwapOption;
use shared::foundation::Filter;
use shared::foundation::ValueProvider;
use shared::model::PlaylistItemType;
use shared::model::{
    ConfigTargetDto, ConfigTargetOptions, HdHomeRunTargetOutputDto, M3uTargetOutputDto, ProcessingOrder,
    StrmExportStyle, StrmTargetOutputDto, TargetOutputDto, TargetType, TraktConfigDto, XtreamTargetOutputDto,
};
use shared::{apply_flags, create_bitset};
use std::sync::Arc;

create_bitset!(u8, XtreamTargetFlags, SkipLiveDirectSource, SkipVideoDirectSource, SkipSeriesDirectSource);
create_bitset!(u8, StrmTargetFlags, Flat, UnderscoreWhitespace, Cleanup, AddQualityToFilename);

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

#[derive(Debug, Clone)]
pub struct XtreamTargetOutput {
    pub flags: XtreamTargetFlagsSet,
    pub trakt: Option<TraktConfig>,
    pub filter: Option<Filter>,
}

macros::from_impl!(XtreamTargetOutput);
impl From<&XtreamTargetOutputDto> for XtreamTargetOutput {
    fn from(dto: &XtreamTargetOutputDto) -> Self {
        let mut flags = XtreamTargetFlagsSet::new();
        apply_flags!(
            dto, flags, XtreamTargetFlags;
            (skip_live_direct_source, SkipLiveDirectSource),
            (skip_video_direct_source, SkipVideoDirectSource),
            (skip_series_direct_source, SkipSeriesDirectSource)
        );

        Self { flags, trakt: dto.trakt.as_ref().map(Into::into), filter: dto.t_filter.clone() }
    }
}

impl From<&XtreamTargetOutput> for XtreamTargetOutputDto {
    fn from(instance: &XtreamTargetOutput) -> Self {
        Self {
            skip_live_direct_source: instance.flags.contains(XtreamTargetFlags::SkipLiveDirectSource),
            skip_video_direct_source: instance.flags.contains(XtreamTargetFlags::SkipVideoDirectSource),
            skip_series_direct_source: instance.flags.contains(XtreamTargetFlags::SkipSeriesDirectSource),
            trakt: instance.trakt.as_ref().map(TraktConfigDto::from),
            filter: instance.filter.as_ref().map(ToString::to_string),
            t_filter: instance.filter.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct M3uTargetOutput {
    pub filename: Option<String>,
    pub include_type_in_url: bool,
    pub mask_redirect_url: bool,
    pub filter: Option<Filter>,
}

macros::from_impl!(M3uTargetOutput);
impl From<&M3uTargetOutputDto> for M3uTargetOutput {
    fn from(dto: &M3uTargetOutputDto) -> Self {
        Self {
            filename: dto.filename.clone(),
            include_type_in_url: dto.include_type_in_url,
            mask_redirect_url: dto.mask_redirect_url,
            filter: dto.t_filter.clone(),
        }
    }
}
impl From<&M3uTargetOutput> for M3uTargetOutputDto {
    fn from(instance: &M3uTargetOutput) -> Self {
        Self {
            filename: instance.filename.clone(),
            include_type_in_url: instance.include_type_in_url,
            mask_redirect_url: instance.mask_redirect_url,
            filter: instance.filter.as_ref().map(ToString::to_string),
            t_filter: instance.filter.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StrmTargetOutput {
    pub directory: String,
    pub username: Option<String>,
    pub style: StrmExportStyle,
    pub flags: StrmTargetFlagsSet,
    pub strm_props: Option<Vec<String>>,
    pub filter: Option<Filter>,
    pub probe_probe_size_bytes: Option<u64>,
    pub probe_analyze_duration: Option<u64>,
}

macros::from_impl!(StrmTargetOutput);
impl From<&StrmTargetOutputDto> for StrmTargetOutput {
    fn from(dto: &StrmTargetOutputDto) -> Self {
        let mut flags = StrmTargetFlagsSet::new();
        apply_flags!(
            dto, flags, StrmTargetFlags;
            (flat, Flat),
            (underscore_whitespace, UnderscoreWhitespace),
            (cleanup, Cleanup),
            (add_quality_to_filename, AddQualityToFilename)
        );
        Self {
            directory: dto.directory.clone(),
            username: dto.username.clone(),
            style: dto.style,
            flags,
            strm_props: dto.strm_props.clone(),
            filter: dto.t_filter.clone(),
            probe_probe_size_bytes: dto.probe_probe_size_bytes,
            probe_analyze_duration: dto.probe_analyze_duration,
        }
    }
}
impl From<&StrmTargetOutput> for StrmTargetOutputDto {
    fn from(instance: &StrmTargetOutput) -> Self {
        Self {
            directory: instance.directory.clone(),
            username: instance.username.clone(),
            style: instance.style,
            flat: instance.flags.contains(StrmTargetFlags::Flat),
            underscore_whitespace: instance.flags.contains(StrmTargetFlags::UnderscoreWhitespace),
            cleanup: instance.flags.contains(StrmTargetFlags::Cleanup),
            strm_props: instance.strm_props.clone(),
            filter: instance.filter.as_ref().map(ToString::to_string),
            t_filter: instance.filter.clone(),
            add_quality_to_filename: instance.flags.contains(StrmTargetFlags::AddQualityToFilename),
            probe_probe_size_bytes: instance.probe_probe_size_bytes,
            probe_analyze_duration: instance.probe_analyze_duration,
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
        Self { device: dto.device.clone(), username: dto.username.clone(), use_output: dto.use_output }
    }
}
impl From<&HdHomeRunTargetOutput> for HdHomeRunTargetOutputDto {
    fn from(instance: &HdHomeRunTargetOutput) -> Self {
        Self { device: instance.device.clone(), username: instance.username.clone(), use_output: instance.use_output }
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
            TargetOutputDto::M3u(o) => TargetOutput::M3u(M3uTargetOutput::from(o)),
            TargetOutputDto::Strm(o) => TargetOutput::Strm(StrmTargetOutput::from(o)),
            TargetOutputDto::HdHomeRun(o) => TargetOutput::HdHomeRun(HdHomeRunTargetOutput::from(o)),
        }
    }
}

impl From<&TargetOutput> for TargetOutputDto {
    fn from(instance: &TargetOutput) -> Self {
        match instance {
            TargetOutput::Xtream(o) => TargetOutputDto::Xtream(XtreamTargetOutputDto::from(o)),
            TargetOutput::M3u(o) => TargetOutputDto::M3u(M3uTargetOutputDto::from(o)),
            TargetOutput::Strm(o) => TargetOutputDto::Strm(StrmTargetOutputDto::from(o)),
            TargetOutput::HdHomeRun(o) => TargetOutputDto::HdHomeRun(HdHomeRunTargetOutputDto::from(o)),
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
    pub favourites: Option<Vec<ConfigFavourites>>,
    pub processing_order: ProcessingOrder,
    pub watch: Option<Vec<Arc<regex::Regex>>>,
    pub use_memory_cache: bool,
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

    pub(crate) fn get_hdhomerun_output(&self) -> Option<&HdHomeRunTargetOutput> {
        if let Some(TargetOutput::HdHomeRun(output)) =
            self.output.iter().find(|o| matches!(o, TargetOutput::HdHomeRun(_)))
        {
            Some(output)
        } else {
            None
        }
    }

    pub fn has_output(&self, tt: TargetType) -> bool {
        for target_output in &self.output {
            match target_output {
                TargetOutput::Xtream(_) => {
                    if tt == TargetType::Xtream {
                        return true;
                    }
                }
                TargetOutput::M3u(_) => {
                    if tt == TargetType::M3u {
                        return true;
                    }
                }
                TargetOutput::Strm(_) => {
                    if tt == TargetType::Strm {
                        return true;
                    }
                }
                TargetOutput::HdHomeRun(_) => {
                    if tt == TargetType::HdHomeRun {
                        return true;
                    }
                }
            }
        }
        false
    }

    pub fn is_force_redirect(&self, item_type: PlaylistItemType) -> bool {
        if item_type.is_local() {
            return false;
        }
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
            name: dto.name.clone(),
            options: dto.options.clone(),
            sort: dto.sort.as_ref().map(Into::into),
            filter: dto.t_filter.clone().unwrap_or_default(),
            output: dto.output.iter().map(Into::into).collect(),
            rename: dto.rename.as_ref().map(|l| l.iter().map(Into::into).collect()),
            mapping_ids: dto.mapping.clone(),
            mapping: Arc::new(ArcSwapOption::new(None)),
            favourites: dto.favourites.as_ref().map(|f| f.iter().map(Into::into).collect()),
            processing_order: dto.processing_order,
            watch: dto.watch.as_ref().map(|list| {
                list.iter()
                    .filter_map(|s| match shared::model::REGEX_CACHE.get_or_compile(s) {
                        Ok(re) => Some(re),
                        Err(e) => {
                            log::warn!("Invalid watch regex pattern '{s}': {e}");
                            None
                        }
                    })
                    .collect()
            }),
            use_memory_cache: dto.use_memory_cache,
        }
    }
}
