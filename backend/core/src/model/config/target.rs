use crate::{
    model::{
        config::{favourites::ConfigFavourites, trakt::TraktConfig},
        macros,
        mapping::CompiledMapping,
        ConfigRename, ConfigSort, CurationConfig,
    },
    utils::EpgIdOutputCase,
};
use arc_swap::ArcSwapOption;
use shared::{
    apply_flags, create_bitset,
    foundation::{Filter, ValueProvider},
    model::{
        ConfigTargetDto, ConfigTargetOptions, DeduplicateConfig, HdHomeRunTargetOutputDto, M3uTargetOutputDto,
        MappingStage, PlaylistItemType, PluginHook, PluginRef, PluginTargetOutputDto, ProcessingOrder, StrmExportStyle,
        StrmTargetOutputDto, TargetOutputDto, TargetType, TraktConfigDto, WatchContext, WatchContextSet,
        XtreamTargetOutputDto,
    },
};
use std::{borrow::Cow, sync::Arc};

create_bitset!(u8, XtreamTargetFlags, SkipLiveDirectSource, SkipVideoDirectSource, SkipSeriesDirectSource);
create_bitset!(u8, StrmTargetFlags, Flat, UnderscoreWhitespace, Cleanup, AddQualityToFilename, UseMetadata);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransformStage {
    Filter,
    Rename,
    Map,
}

#[derive(Debug, Clone)]
pub struct TargetExecutionPlan {
    pub transform_stages: Vec<TransformStage>,
    pub pre_transform_identity_dedup: bool,
    pub post_merge_content_dedup: Option<DeduplicateConfig>,
    /// The plugin strategies the target opted into. Unlike the always-on stage
    /// hooks these never run unless the config names a plugin (plan §6).
    pub plugin_strategies: PluginStrategies,
}

/// The four per-target plugin strategies, each `None` unless the config spells
/// `plugin:<id>`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginStrategies {
    /// `sort.strategy` (`sort-key`).
    pub sort_key: Option<PluginRef>,
    /// `options.deduplicate.keep` (`resolve-duplicates`).
    pub resolve_duplicates: Option<PluginRef>,
    /// `chno` (`assign-channel-numbers`).
    pub assign_chno: Option<PluginRef>,
    /// `group_merge` (`merge-groups`).
    pub merge_groups: Option<PluginRef>,
}

impl PluginStrategies {
    /// The strategies with the hook each one calls.
    pub fn refs(&self) -> impl Iterator<Item = (PluginHook, &str)> {
        [
            (PluginHook::SortKey, self.sort_key.as_ref()),
            (PluginHook::ResolveDuplicates, self.resolve_duplicates.as_ref()),
            (PluginHook::AssignChno, self.assign_chno.as_ref()),
            (PluginHook::MergeGroups, self.merge_groups.as_ref()),
        ]
        .into_iter()
        .filter_map(|(hook, plugin)| plugin.map(|plugin| (hook, plugin.id())))
    }
}

impl Default for TargetExecutionPlan {
    fn default() -> Self {
        Self {
            transform_stages: vec![TransformStage::Filter, TransformStage::Rename, TransformStage::Map],
            pre_transform_identity_dedup: false,
            post_merge_content_dedup: None,
            plugin_strategies: PluginStrategies::default(),
        }
    }
}

impl TargetExecutionPlan {
    fn from_dto(dto: &ConfigTargetDto) -> Self {
        let transform_stages = match dto.processing_order {
            ProcessingOrder::Frm => vec![TransformStage::Filter, TransformStage::Rename, TransformStage::Map],
            ProcessingOrder::Fmr => vec![TransformStage::Filter, TransformStage::Map, TransformStage::Rename],
            ProcessingOrder::Rfm => vec![TransformStage::Rename, TransformStage::Filter, TransformStage::Map],
            ProcessingOrder::Rmf => vec![TransformStage::Rename, TransformStage::Map, TransformStage::Filter],
            ProcessingOrder::Mfr => vec![TransformStage::Map, TransformStage::Filter, TransformStage::Rename],
            ProcessingOrder::Mrf => vec![TransformStage::Map, TransformStage::Rename, TransformStage::Filter],
        };
        Self {
            transform_stages,
            pre_transform_identity_dedup: dto.options.as_ref().is_some_and(|options| options.remove_duplicates),
            post_merge_content_dedup: dto.options.as_ref().and_then(|options| options.deduplicate.clone()),
            plugin_strategies: PluginStrategies {
                sort_key: dto.sort.as_ref().and_then(|sort| sort.strategy.clone()),
                resolve_duplicates: dto
                    .options
                    .as_ref()
                    .and_then(|options| options.deduplicate.as_ref())
                    .and_then(|dedupe| dedupe.keep.plugin().cloned()),
                assign_chno: dto.chno.clone(),
                merge_groups: dto.group_merge.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct StagedFilter {
    pub processing: Option<Filter>,
    pub persist: Option<Filter>,
}

impl From<Filter> for StagedFilter {
    fn from(processing: Filter) -> Self { Self { processing: Some(processing), ..Self::default() } }
}

#[derive(Debug)]
pub struct CompiledTargetMappings {
    pub all: Vec<Arc<CompiledMapping>>,
    processing: Vec<Arc<CompiledMapping>>,
    after_epg: Vec<Arc<CompiledMapping>>,
}

impl CompiledTargetMappings {
    pub fn new(all: Vec<Arc<CompiledMapping>>) -> Self {
        let processing = all
            .iter()
            .filter(|mapping| mapping.stage == MappingStage::Processing && !mapping.rules.is_empty())
            .cloned()
            .collect();
        let after_epg = all
            .iter()
            .filter(|mapping| mapping.stage == MappingStage::AfterEpg && !mapping.rules.is_empty())
            .cloned()
            .collect();
        Self { all, processing, after_epg }
    }

    pub fn for_stage(&self, stage: MappingStage) -> &[Arc<CompiledMapping>] {
        match stage {
            MappingStage::Processing => &self.processing,
            MappingStage::AfterEpg => &self.after_epg,
        }
    }
}

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

    pub fn has_input(&self, tid: u16) -> bool { !self.enabled || self.inputs.is_empty() || self.inputs.contains(&tid) }
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
            (add_quality_to_filename, AddQualityToFilename),
            (use_metadata, UseMetadata)
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
            use_metadata: instance.flags.contains(StrmTargetFlags::UseMetadata),
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

/// A `type: plugin` output; see [`PluginTargetOutputDto`].
#[derive(Debug, Clone)]
pub struct PluginTargetOutput {
    pub plugin: String,
    pub format: Option<String>,
    pub filename: Option<String>,
    /// The plugin's settings for this output, as configured. Handed to the
    /// plugin as `output-request.options` (see [`shared::model::plugin_options`]).
    pub plugin_config: serde_json::Map<String, serde_json::Value>,
    pub filter: Option<Filter>,
}

impl PluginTargetOutput {
    /// The file name in the target directory: `filename`, else `<plugin>.out`.
    #[must_use]
    pub fn file_name(&self) -> String { self.filename.clone().unwrap_or_else(|| format!("{}.out", self.plugin)) }
}

macros::from_impl!(PluginTargetOutput);
impl From<&PluginTargetOutputDto> for PluginTargetOutput {
    fn from(dto: &PluginTargetOutputDto) -> Self {
        Self {
            plugin: dto.plugin.clone(),
            format: dto.format.clone(),
            filename: dto.filename.clone(),
            plugin_config: dto.plugin_config.clone(),
            filter: dto.t_filter.clone(),
        }
    }
}
impl From<&PluginTargetOutput> for PluginTargetOutputDto {
    fn from(instance: &PluginTargetOutput) -> Self {
        Self {
            plugin: instance.plugin.clone(),
            format: instance.format.clone(),
            filename: instance.filename.clone(),
            plugin_config: instance.plugin_config.clone(),
            filter: instance.filter.as_ref().map(ToString::to_string),
            t_filter: instance.filter.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum TargetOutput {
    Xtream(XtreamTargetOutput),
    M3u(M3uTargetOutput),
    Strm(StrmTargetOutput),
    HdHomeRun(HdHomeRunTargetOutput),
    Plugin(PluginTargetOutput),
}

impl From<&TargetOutput> for TargetType {
    fn from(output: &TargetOutput) -> Self {
        match output {
            TargetOutput::Xtream(_) => TargetType::Xtream,
            TargetOutput::M3u(_) => TargetType::M3u,
            TargetOutput::Strm(_) => TargetType::Strm,
            TargetOutput::HdHomeRun(_) => TargetType::HdHomeRun,
            TargetOutput::Plugin(_) => TargetType::Plugin,
        }
    }
}

impl TargetOutput {
    /// Output format of this configured target output.
    #[must_use]
    pub fn target_type(&self) -> TargetType { TargetType::from(self) }

    /// The optional playlist filter configured for this output, co-located so
    /// the per-format filter access lives in one place rather than being
    /// re-matched at every call site.
    #[must_use]
    pub fn filter(&self) -> Option<&Filter> {
        match self {
            TargetOutput::Xtream(out) => out.filter.as_ref(),
            TargetOutput::M3u(out) => out.filter.as_ref(),
            TargetOutput::Strm(out) => out.filter.as_ref(),
            TargetOutput::Plugin(out) => out.filter.as_ref(),
            TargetOutput::HdHomeRun(_) => None,
        }
    }
}

macros::from_impl!(TargetOutput);
impl From<&TargetOutputDto> for TargetOutput {
    fn from(dto: &TargetOutputDto) -> Self {
        match dto {
            TargetOutputDto::Xtream(o) => TargetOutput::Xtream(XtreamTargetOutput::from(o)),
            TargetOutputDto::M3u(o) => TargetOutput::M3u(M3uTargetOutput::from(o)),
            TargetOutputDto::Strm(o) => TargetOutput::Strm(StrmTargetOutput::from(o)),
            TargetOutputDto::HdHomeRun(o) => TargetOutput::HdHomeRun(HdHomeRunTargetOutput::from(o)),
            TargetOutputDto::Plugin(o) => TargetOutput::Plugin(PluginTargetOutput::from(o)),
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
            TargetOutput::Plugin(o) => TargetOutputDto::Plugin(PluginTargetOutputDto::from(o)),
        }
    }
}

/// One compiled `watch` entry.
///
/// `contexts` is the DTO's `u8` bitset, copied as is: "does this entry apply
/// to X" is asked for every group, and a bit test is all it costs.
#[derive(Debug, Clone)]
pub struct ConfigWatch {
    pub pattern: Arc<regex::Regex>,
    pub contexts: WatchContextSet,
}

impl ConfigWatch {
    /// Whether matching no current group means something is wrong.
    ///
    /// An entry waiting for a group to appear matches nothing by design, and a
    /// `group_removed`-only entry is matched against the previous titles, not
    /// the current playlist.
    #[must_use]
    pub fn reports_unmatched(&self) -> bool {
        self.contexts.contains(WatchContext::Channels) && !self.contexts.contains(WatchContext::GroupAdded)
    }
}

#[derive(Debug, Clone)]
pub struct ConfigTarget {
    pub id: u16,
    pub enabled: bool,
    pub name: String,
    pub options: Option<ConfigTargetOptions>,
    pub sort: Option<ConfigSort>,
    pub filter: StagedFilter,
    pub output: Vec<TargetOutput>,
    pub curation: Option<CurationConfig>,
    pub rename: Option<Vec<ConfigRename>>,
    pub mapping_ids: Option<Vec<String>>,
    pub mapping: Arc<ArcSwapOption<CompiledTargetMappings>>,
    pub favourites: Option<Vec<ConfigFavourites>>,
    pub processing_order: ProcessingOrder,
    pub execution_plan: TargetExecutionPlan,
    pub watch: Option<Vec<ConfigWatch>>,
    pub use_memory_cache: bool,
    pub managed_channels: Vec<shared::model::ManagedChannelBinding>,
}

impl ConfigTarget {
    /// Normalize only for execution; the original config declaration remains untouched.
    pub fn effective_curation(&self) -> Option<Cow<'_, CurationConfig>> {
        self.curation.as_ref().map(Cow::Borrowed).or_else(|| {
            self.get_xtream_output()
                .and_then(|output| output.trakt.as_ref())
                .map(|legacy| Cow::Owned(CurationConfig::from(legacy)))
        })
    }

    pub fn filter(&self, provider: &ValueProvider) -> bool {
        self.filter.processing.as_ref().is_none_or(|filter| filter.filter(provider))
    }

    pub fn get_xtream_output(&self) -> Option<&XtreamTargetOutput> {
        self.output.iter().find_map(|o| match o {
            TargetOutput::Xtream(output) => Some(output),
            _ => None,
        })
    }

    pub fn get_m3u_output(&self) -> Option<&M3uTargetOutput> {
        self.output.iter().find_map(|o| match o {
            TargetOutput::M3u(output) => Some(output),
            _ => None,
        })
    }

    pub fn get_hdhomerun_output(&self) -> Option<&HdHomeRunTargetOutput> {
        self.output.iter().find_map(|o| match o {
            TargetOutput::HdHomeRun(output) => Some(output),
            _ => None,
        })
    }

    /// The `type: plugin` outputs, in config order.
    pub fn plugin_outputs(&self) -> impl Iterator<Item = &PluginTargetOutput> {
        self.output.iter().filter_map(|o| match o {
            TargetOutput::Plugin(output) => Some(output),
            _ => None,
        })
    }

    pub fn has_output(&self, tt: TargetType) -> bool { self.output.iter().any(|o| TargetType::from(o) == tt) }

    /// The plugin strategies this target names, with the hook each one calls.
    ///
    /// Plugins load after the config, so the orchestrator checks these against
    /// the registry (`PluginHost::provides`) right after plugin load; core does
    /// not validate them. A `type: plugin` output names its plugin with
    /// `RenderOutput`; its `format` is checked against `provides.output_format`
    /// separately (see [`PluginTargetOutput::format`]).
    pub fn plugin_refs(&self) -> impl Iterator<Item = (PluginHook, &str)> {
        let outputs = self.plugin_outputs().map(|output| (PluginHook::RenderOutput, output.plugin.as_str()));
        self.execution_plan
            .plugin_strategies
            .refs()
            .chain(outputs)
            .chain(self.managed_channels.iter().map(|binding| (PluginHook::ManagedChannels, binding.plugin.id())))
    }

    pub fn epg_id_case(&self) -> EpgIdOutputCase {
        EpgIdOutputCase::from_lowercase(self.options.as_ref().is_some_and(ConfigTargetOptions::lowercase_epg_ids))
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
            filter: StagedFilter { processing: dto.filter.t_processing.clone(), persist: dto.filter.t_persist.clone() },
            output: dto.output.iter().map(Into::into).collect(),
            curation: dto.curation.as_ref().map(Into::into),
            rename: dto.rename.as_ref().map(|l| l.iter().map(Into::into).collect()),
            mapping_ids: dto.mapping.clone(),
            mapping: Arc::new(ArcSwapOption::new(None)),
            favourites: dto.favourites.as_ref().map(|f| f.iter().map(Into::into).collect()),
            processing_order: dto.processing_order,
            execution_plan: TargetExecutionPlan::from_dto(dto),
            // An empty `Some` is deliberate and load-bearing: it means the
            // operator configured `watch` and every pattern failed to
            // compile, which is different from not configuring it at all.
            // Collapsing both to `None` is how the feature used to turn
            // itself off on a typo with nothing but one `warn!` to show for
            // it - `process_watch` now reports the empty case as
            // `playlist.watch.disabled`.
            watch: dto.watch.as_ref().map(|list| {
                list.iter()
                    .filter_map(|entry| match shared::model::REGEX_CACHE.get_or_compile(&entry.pattern) {
                        Ok(pattern) => Some(ConfigWatch { pattern, contexts: entry.on }),
                        Err(e) => {
                            log::warn!("Invalid watch regex pattern '{}': {e}", entry.pattern);
                            None
                        }
                    })
                    .collect()
            }),
            use_memory_cache: dto.use_memory_cache,
            managed_channels: dto.managed_channels.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_contexts_compile_to_flags() {
        let dto = ConfigTargetDto {
            name: "t".to_string(),
            watch: Some(vec![
                shared::model::ConfigWatchDto::channels("^A"),
                shared::model::ConfigWatchDto::new(
                    "^B",
                    [shared::model::WatchContext::GroupAdded, shared::model::WatchContext::Channels],
                ),
            ]),
            ..ConfigTargetDto::default()
        };
        let target = ConfigTarget::from(&dto);
        let watch = target.watch.expect("watch");
        assert_eq!(watch[0].contexts, dto.watch.as_ref().unwrap()[0].on);
        assert!(watch[0].reports_unmatched());
        assert!(watch[1].contexts.contains(WatchContext::GroupAdded));
        assert!(!watch[1].reports_unmatched());
    }

    #[test]
    fn effective_curation_retains_canonical_owner_and_credentials_without_legacy_output() {
        let mut dto: ConfigTargetDto = serde_json::from_value(serde_json::json!({"name": "discovery", "output": [{"type": "m3u"}],
            "curation": {"catalog_selection": "curated", "tmdb": {"api": {"access_token": "test-token"}, "trending": [{"kind": "tv", "time_window": "day", "limit": 37, "create_xtream_category": false}]}}})).unwrap();
        dto.prepare(1, None, None).unwrap();
        let target = ConfigTarget::from(&dto);
        assert!(matches!(target.effective_curation(), Some(Cow::Borrowed(_))));
        assert_eq!(shared::model::CurationConfigDto::from(target.curation.as_ref().unwrap()), dto.curation.unwrap());
        assert!(target.get_xtream_output().is_none());
        assert!(!format!("{target:?}").contains("test-token"));
    }

    #[test]
    fn effective_curation_normalizes_legacy_only_for_execution() {
        let mut dto: ConfigTargetDto = serde_json::from_value(serde_json::json!({"name": "legacy", "output": [{"type": "xtream", "trakt": {"enabled": false, "catalog_selection": "curated"}}]})).unwrap();
        dto.prepare(1, None, None).unwrap();
        let target = ConfigTarget::from(&dto);
        assert!(matches!(target.effective_curation(), Some(Cow::Owned(_))));
        assert!(target.curation.is_none());
        assert!(!target.effective_curation().unwrap().enabled);
        assert!(target.effective_curation().unwrap().catalog_selection.is_curated());
        assert_eq!(TargetOutputDto::from(&target.output[0]), dto.output[0]);
    }

    #[test]
    fn epg_id_case_follows_lowercase_epg_ids_option() {
        let mut target = ConfigTarget::from(&ConfigTargetDto::default());
        target.options = None;
        assert_eq!(target.epg_id_case(), EpgIdOutputCase::Preserve);

        target.options = Some(ConfigTargetOptions::default());
        assert_eq!(target.epg_id_case(), EpgIdOutputCase::Preserve);

        let mut options = ConfigTargetOptions::default();
        options.epg_output.lowercase_ids = true;
        target.options = Some(options);
        assert_eq!(target.epg_id_case(), EpgIdOutputCase::LowercaseAscii);
    }

    #[test]
    fn execution_plan_compiles_processing_order() {
        let dto = ConfigTargetDto { processing_order: ProcessingOrder::Mrf, ..Default::default() };

        let target = ConfigTarget::from(&dto);

        assert_eq!(
            target.execution_plan.transform_stages,
            [TransformStage::Map, TransformStage::Rename, TransformStage::Filter]
        );
    }

    #[test]
    fn hdhomerun_keeps_its_configured_underlying_output_delegation() {
        let dto = ConfigTargetDto {
            output: vec![
                TargetOutputDto::Xtream(XtreamTargetOutputDto::default()),
                TargetOutputDto::M3u(M3uTargetOutputDto::default()),
                TargetOutputDto::HdHomeRun(HdHomeRunTargetOutputDto {
                    device: "device".to_string(),
                    username: "viewer".to_string(),
                    use_output: Some(TargetType::Xtream),
                }),
            ],
            ..ConfigTargetDto::default()
        };

        let target = ConfigTarget::from(&dto);

        assert_eq!(target.get_hdhomerun_output().and_then(|output| output.use_output), Some(TargetType::Xtream));
        assert!(target.has_output(TargetType::Xtream));
        assert!(target.has_output(TargetType::M3u));
    }

    #[test]
    fn plugin_output_round_trips_and_names_its_plugin_for_the_render_hook() {
        let mut dto: ConfigTargetDto = serde_json::from_value(serde_json::json!({"name": "t1", "output": [
            {"type": "m3u"},
            {"type": "plugin", "plugin": "my-plugin", "format": "my-format", "filename": "out.txt",
             "plugin_config": {"b": "2", "a": 1, "on": true}, "filter": "Group ~ \"News\""},
        ]}))
        .unwrap();
        dto.prepare(1, None, None).unwrap();
        let target = ConfigTarget::from(&dto);

        assert_eq!(target.output[1].target_type(), TargetType::Plugin);
        assert!(target.has_output(TargetType::Plugin));
        let output = target.plugin_outputs().next().unwrap();
        // `plugin_options` is the one place the typed values become the plugin's strings.
        let mut options = shared::model::plugin_options(&output.plugin_config);
        options.sort();
        assert_eq!(
            options,
            [("a".to_string(), "1".to_string()), ("b".to_string(), "2".to_string()), ("on".to_string(), "true".into())],
            "typed values reach the plugin as their JSON text; the order is the document's"
        );
        assert_eq!(output.file_name(), "out.txt");
        assert!(target.output[1].filter().is_some(), "the output filter is the plugin output's");
        assert_eq!(TargetOutputDto::from(&target.output[1]), dto.output[1]);

        let refs: Vec<_> = target.plugin_refs().collect();
        assert_eq!(refs, [(PluginHook::RenderOutput, "my-plugin")]);
    }

    #[test]
    fn plugin_refs_lists_strategies_and_outputs_together() {
        let mut dto: ConfigTargetDto = serde_json::from_value(serde_json::json!({"name": "t1",
            "sort": {"strategy": "plugin:sorter"},
            "output": [{"type": "plugin", "plugin": "my-plugin"}]}))
        .unwrap();
        dto.prepare(1, None, None).unwrap();
        let target = ConfigTarget::from(&dto);
        let refs: Vec<_> = target.plugin_refs().collect();
        assert!(refs.contains(&(PluginHook::RenderOutput, "my-plugin")), "{refs:?}");
        assert_eq!(refs.len(), 2, "{refs:?}");
    }

    #[test]
    fn execution_plan_preserves_both_deduplication_passes() {
        let deduplicate = DeduplicateConfig::default();
        let dto = ConfigTargetDto {
            options: Some(ConfigTargetOptions {
                remove_duplicates: true,
                deduplicate: Some(deduplicate.clone()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let target = ConfigTarget::from(&dto);

        assert!(target.execution_plan.pre_transform_identity_dedup);
        assert_eq!(target.execution_plan.post_merge_content_dedup, Some(deduplicate));
    }
}
