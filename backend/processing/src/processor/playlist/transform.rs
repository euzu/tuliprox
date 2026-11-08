#![allow(clippy::wildcard_imports)]
use super::*;
use shared::{
    foundation::{NoPluginCalls, PluginCallBatch, PluginCallDecisions},
    model::{HookCtx, ItemOverride, PlaylistEntry, PluginAbort, PluginHook, PluginHost, PluginRunReport},
};

pub(crate) fn retain_playlist_items(
    source: &mut PlaylistSource,
    mut keep: impl FnMut(&PlaylistItem) -> bool,
) -> (Option<Vec<PlaylistGroup>>, FilterOutcome) {
    let mut groups: IndexMap<CategoryKey, PlaylistGroup> = IndexMap::new();
    let mut outcome = FilterOutcome::default();
    for pli in source.into_items() {
        if outcome.record(keep(&pli)) {
            let group_title = pli.header.group.clone();
            let cluster = pli.header.xtream_cluster;
            let cat_id = pli.header.category_id;
            let normalized_group = shared::utils::deunicode_string(&group_title).to_lowercase().intern();
            let key = (cluster, normalized_group);
            groups
                .entry(key)
                .or_insert_with(|| PlaylistGroup {
                    id: cat_id,
                    title: group_title,
                    channels: vec![],
                    xtream_cluster: cluster,
                })
                .channels
                .push(pli);
        }
    }

    let groups = if groups.is_empty() { None } else { Some(groups.into_values().collect()) };
    (groups, outcome)
}

pub fn apply_filter_to_source(source: &mut PlaylistSource, filter: &Filter) -> Option<Vec<PlaylistGroup>> {
    retain_playlist_items(source, |item| is_valid(item, filter, false)).0
}

pub(crate) fn assign_channel_no_playlist(new_playlist: &mut [PlaylistGroup]) {
    let assigned_chnos: HashSet<u32> =
        new_playlist.iter().flat_map(|g| &g.channels).filter(|c| c.header.chno != 0).map(|c| c.header.chno).collect();
    let mut chno = 1;
    for group in new_playlist {
        for chan in &mut group.channels {
            if chan.header.chno == 0 {
                while assigned_chnos.contains(&chno) {
                    chno += 1;
                }
                chan.header.chno = chno;
                chno += 1;
            }
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct RenameOutcome {
    pub inspected: usize,
    pub changed_items: usize,
    pub changed_fields: usize,
}

#[derive(Debug, Default)]
pub struct PipelineOutcome {
    pub filter: Option<FilterOutcome>,
    pub rename: Option<RenameOutcome>,
    pub mapping: Option<MappingStageOutcome>,
    pub plugins: PluginRunReport,
}

impl PipelineOutcome {
    pub(crate) fn merge(&mut self, other: Self) {
        self.plugins.merge(other.plugins);
        if let Some(value) = other.filter {
            let outcome = self.filter.get_or_insert_with(FilterOutcome::default);
            outcome.inspected += value.inspected;
            outcome.retained += value.retained;
            outcome.removed += value.removed;
        }
        if let Some(value) = other.rename {
            let outcome = self.rename.get_or_insert_with(RenameOutcome::default);
            outcome.inspected += value.inspected;
            outcome.changed_items += value.changed_items;
            outcome.changed_fields += value.changed_fields;
        }
        if let Some(value) = other.mapping {
            let outcome = self.mapping.get_or_insert_with(MappingStageOutcome::default);
            outcome.inspected += value.inspected;
            outcome.matched_rules += value.matched_rules;
            outcome.emitted_items += value.emitted_items;
            outcome.changed_fields.extend(value.changed_fields);
            outcome.diagnostics += value.diagnostics;
            outcome.reported_diagnostics += value.reported_diagnostics;
        }
    }

    pub(crate) fn to_stats(&self) -> PipelineStats {
        PipelineStats {
            inspected: self.filter.as_ref().map_or(0, |outcome| outcome.inspected),
            retained: self.filter.as_ref().map_or(0, |outcome| outcome.retained),
            removed: self.filter.as_ref().map_or(0, |outcome| outcome.removed),
            renamed_items: self.rename.as_ref().map_or(0, |outcome| outcome.changed_items),
            renamed_fields: self.rename.as_ref().map_or(0, |outcome| outcome.changed_fields),
            matched_mapping_rules: self.mapping.as_ref().map_or(0, |outcome| outcome.matched_rules),
            emitted_items: self.mapping.as_ref().map_or(0, |outcome| outcome.emitted_items),
            mapping_diagnostics: self.mapping.as_ref().map_or(0, |outcome| outcome.diagnostics),
            plugins: self.plugins.clone(),
        }
    }
}

pub(crate) fn exec_rename(pli: &mut PlaylistItem, rename: Option<&Vec<ConfigRename>>) -> usize {
    let mut changed_fields = 0;
    if let Some(renames) = rename {
        if !renames.is_empty() {
            let result = pli;
            for r in renames {
                let value = get_field_value(result, r.field);
                let cap = r.pattern.replace_all(&value, &r.new_name);
                if log_enabled!(log::Level::Debug) && *value != *cap {
                    trace_if_enabled!("Renamed {}={value} to {cap}", &r.field);
                }
                if *value != *cap && set_field_value(result, r.field, cap.as_ref()) {
                    changed_fields += 1;
                }
            }
        }
    }
    changed_fields
}

pub(crate) struct ChannelMappingOutcome {
    /// `None` only when a Rhai script overwrote the reserved `item` variable. The item cannot be
    /// recovered in that case, so the channel is discarded instead of reappearing unmodified.
    pub(crate) channel: Option<PlaylistItem>,
    pub(crate) virtual_items: Vec<PlaylistItem>,
    pub(crate) matched_rules: usize,
    pub(crate) changed_fields: HashSet<String>,
    pub(crate) diagnostics: Vec<String>,
}

pub(crate) const MAPPING_DIAGNOSTIC_LIMIT: usize = 10;

#[derive(Debug, Default)]
pub struct MappingStageOutcome {
    pub inspected: usize,
    pub matched_rules: usize,
    pub emitted_items: usize,
    pub changed_fields: HashSet<String>,
    pub diagnostics: usize,
    pub reported_diagnostics: usize,
}

impl MappingStageOutcome {
    pub(crate) fn record(&mut self, mapping_id: &str, outcome: &ChannelMappingOutcome) {
        self.inspected += 1;
        self.matched_rules += outcome.matched_rules;
        self.emitted_items += outcome.virtual_items.len();
        self.changed_fields.extend(outcome.changed_fields.iter().cloned());
        self.diagnostics += outcome.diagnostics.len();
        for diagnostic in &outcome.diagnostics {
            if self.reported_diagnostics >= MAPPING_DIAGNOSTIC_LIMIT {
                break;
            }
            warn!("Mapping '{mapping_id}' {diagnostic}");
            self.reported_diagnostics += 1;
        }
    }
}

pub(crate) fn map_channel(mut channel: PlaylistItem, mapping: &CompiledMapping) -> ChannelMappingOutcome {
    let mut matched_rules = 0;
    let mut virtual_items = vec![];
    let mut changed_fields = HashSet::new();
    let mut diagnostics = Vec::new();
    if !mapping.rules.is_empty() {
        let templates = mapping.templates.as_deref();
        let script_templates = mapping.templates.as_ref().map(Arc::clone);
        for (rule_index, rule) in mapping.rules.iter().enumerate() {
            let matched = {
                let provider = ValueProvider { pli: &channel, match_as_ascii: mapping.match_as_ascii };
                rule.filter.filter(&provider)
            };
            if !matched {
                continue;
            }
            matched_rules += 1;
            let rule_label = rule.name.as_deref().map_or_else(|| (rule_index + 1).to_string(), str::to_string);
            match &rule.program {
                MappingProgram::Dsl(script) => {
                    let mut accessor = ValueAccessor {
                        pli: &mut channel,
                        virtual_items: vec![],
                        match_as_ascii: mapping.match_as_ascii,
                        changed_fields: vec![],
                    };
                    let outcome = script.eval(&mut accessor, templates);
                    changed_fields.extend(outcome.changed_fields.iter().cloned());
                    for diagnostic in outcome.diagnostics {
                        diagnostics.push(format!(
                            "rule '{rule_label}' failed for channel '{}' at statement {}: {}",
                            accessor.pli.header.name,
                            diagnostic.statement + 1,
                            diagnostic.message
                        ));
                    }
                    virtual_items.extend(accessor.virtual_items.into_iter().map(|(_, pli)| pli));
                }
                MappingProgram::Rhai(script) => {
                    let context = MappingScriptContext::new(script_templates.clone(), mapping.match_as_ascii);
                    let channel_name = Arc::clone(&channel.header.name);
                    let outcome = script.eval(channel, context);
                    let Some(mapped) = outcome.item else {
                        virtual_items.extend(outcome.virtual_items);
                        diagnostics.extend(outcome.diagnostics.into_iter().map(|diagnostic| {
                            format!("rule '{rule_label}' failed for channel '{channel_name}': {diagnostic}")
                        }));
                        return ChannelMappingOutcome {
                            channel: None,
                            virtual_items,
                            matched_rules,
                            changed_fields,
                            diagnostics,
                        };
                    };
                    channel = mapped;
                    changed_fields.extend(outcome.changed_fields);
                    virtual_items.extend(outcome.virtual_items);
                    // Neutral verb: a Rhai diagnostic covers both a failed evaluation and an
                    // assignment the item refused while the script kept running.
                    diagnostics.extend(outcome.diagnostics.into_iter().map(|diagnostic| {
                        format!("rule '{rule_label}' on channel '{}': {diagnostic}", channel.header.name)
                    }));
                }
            }
        }
    }
    ChannelMappingOutcome { channel: Some(channel), virtual_items, matched_rules, changed_fields, diagnostics }
}

pub(crate) fn map_playlist_at_stage(
    source: &mut PlaylistSource,
    target: &ConfigTarget,
    stage: MappingStage,
    duplicates: Option<&mut HashSet<UUIDType>>,
) -> Option<Vec<PlaylistGroup>> {
    let mapping_binding = target.mapping.load();
    let mappings = mapping_binding.as_ref()?;
    if mappings.for_stage(stage).is_empty() {
        return None;
    }
    let items = source.into_items().collect::<Vec<_>>();
    let (mapped_items, _outcome) = map_items_with_mappings_at_stage(items, mappings, stage, duplicates);
    Some(group_mapped_items(mapped_items))
}

pub(crate) fn has_mapping_stage(target: &ConfigTarget, stage: MappingStage) -> bool {
    target.mapping.load().as_ref().is_some_and(|mappings| !mappings.for_stage(stage).is_empty())
}

pub(crate) fn map_items_at_stage(
    mapped_items: Vec<PlaylistItem>,
    target: &ConfigTarget,
    stage: MappingStage,
    duplicates: Option<&mut HashSet<UUIDType>>,
) -> Option<(Vec<PlaylistItem>, MappingStageOutcome)> {
    let mapping_binding = target.mapping.load();
    let mappings = mapping_binding.as_ref()?;
    (!mappings.for_stage(stage).is_empty())
        .then(|| map_items_with_mappings_at_stage(mapped_items, mappings, stage, duplicates))
}

fn map_items_with_mappings_at_stage(
    mut mapped_items: Vec<PlaylistItem>,
    mappings: &tuliprox_core::model::CompiledTargetMappings,
    stage: MappingStage,
    duplicates: Option<&mut HashSet<UUIDType>>,
) -> (Vec<PlaylistItem>, MappingStageOutcome) {
    let valid_mappings = mappings.for_stage(stage);
    let original_ids = if duplicates.is_some() {
        Some(mapped_items.iter().map(|item| *item.header.get_uuid()).collect::<HashSet<_>>())
    } else {
        None
    };
    let mut stage_outcome = MappingStageOutcome::default();
    for mapping in valid_mappings {
        let mut next_items = Vec::with_capacity(mapped_items.len());
        for channel in mapped_items {
            let outcome = map_channel(channel, mapping);
            stage_outcome.record(&mapping.id, &outcome);
            if let Some(channel) = outcome.channel {
                next_items.push(channel);
            }
            next_items.extend(outcome.virtual_items);
        }
        mapped_items = next_items;
    }
    debug!(
        "Mapping stage {stage:?}: inspected={}, matched_rules={}, emitted={}, changed_fields={}, diagnostics={}, suppressed_diagnostics={}",
        stage_outcome.inspected,
        stage_outcome.matched_rules,
        stage_outcome.emitted_items,
        stage_outcome.changed_fields.len(),
        stage_outcome.diagnostics,
        stage_outcome.diagnostics.saturating_sub(stage_outcome.reported_diagnostics)
    );
    let suppressed = stage_outcome.diagnostics.saturating_sub(stage_outcome.reported_diagnostics);
    if suppressed > 0 {
        warn!("Mapping stage {stage:?} suppressed {suppressed} additional diagnostics");
    }
    if let (Some(original_ids), Some(duplicates)) = (original_ids, duplicates) {
        mapped_items.retain(|item| {
            let uuid = *item.header.get_uuid();
            original_ids.contains(&uuid) || duplicates.insert(uuid)
        });
    }
    (mapped_items, stage_outcome)
}

pub(crate) fn group_mapped_items(items: Vec<PlaylistItem>) -> Vec<PlaylistGroup> {
    let mut groups: IndexMap<CategoryKey, PlaylistGroup> = IndexMap::new();
    let mut group_id = 0;
    for channel in items {
        let group_title = channel.header.group.clone();
        let cluster = channel.header.xtream_cluster;
        groups
            .entry((cluster, group_title.clone()))
            .or_insert_with(|| {
                group_id += 1;
                PlaylistGroup { id: group_id, title: group_title, channels: Vec::new(), xtream_cluster: cluster }
            })
            .channels
            .push(channel);
    }
    groups.into_values().collect()
}

pub(crate) fn map_playlist_counter(target: &ConfigTarget, playlist: &mut [PlaylistGroup]) {
    if let Some(guard) = &*target.mapping.load() {
        for mapping in &guard.all {
            for counter in &mapping.counters {
                // fresh per target/call. No shared atomic, no cross-refresh carry-over.
                let mut current = counter.start;
                for plg in &mut *playlist {
                    for channel in &mut plg.channels {
                        let provider = ValueProvider { pli: channel, match_as_ascii: mapping.match_as_ascii };
                        if counter.filter.filter(&provider) {
                            let cntval = current;
                            current += 1;
                            let padded_cntval = if counter.padding > 0 {
                                format!("{:0width$}", cntval, width = counter.padding as usize)
                            } else {
                                cntval.to_string()
                            };
                            let new_value = if counter.modifier == CounterModifier::Assign {
                                padded_cntval
                            } else {
                                let value = channel
                                    .header
                                    .get(counter.field)
                                    .map_or_else(String::new, |field_value| field_value.as_cow().into_owned());
                                if counter.modifier == CounterModifier::Suffix {
                                    format!("{value}{}{padded_cntval}", counter.concat)
                                } else {
                                    format!("{padded_cntval}{}{value}", counter.concat)
                                }
                            };
                            channel.header.set(counter.field, new_value.as_str());
                        }
                    }
                }
            }
        }
    }
}

pub type ProcessingPipe = Vec<TransformStage>;

pub(crate) fn get_processing_pipe(target: &ConfigTarget) -> ProcessingPipe {
    target.execution_plan.transform_stages.clone()
}

#[derive(Clone, Copy)]
pub(crate) enum GroupingPolicy {
    NormalizedCategory,
    ExactCategory,
    ExactSequential,
}

#[derive(Default)]
pub(crate) struct ManagedStageItems {
    pub(crate) before: Vec<PlaylistItem>,
    pub(crate) after: Vec<PlaylistItem>,
}
impl ManagedStageItems {
    pub(crate) fn is_empty(&self) -> bool { self.before.is_empty() && self.after.is_empty() }
}

pub(crate) struct TransformBuffer<'a> {
    pub(crate) items: Vec<PlaylistItem>,
    pub(crate) grouping: GroupingPolicy,
    pub(crate) managed: ManagedStageItems,
    pub(crate) locked: Option<&'a HashMap<UUIDType, Arc<ItemOverride>>>,
}

impl<'a> TransformBuffer<'a> {
    pub(crate) fn new(items: Vec<PlaylistItem>, locked: Option<&'a HashMap<UUIDType, Arc<ItemOverride>>>) -> Self {
        Self { items, grouping: GroupingPolicy::ExactCategory, locked, managed: ManagedStageItems::default() }
    }

    pub(crate) fn apply_filter(&mut self, target: &ConfigTarget) -> FilterOutcome {
        self.apply_filter_with(target, &NoPluginCalls)
    }

    /// [`Self::apply_filter`] with the answers of the filter's `plugin.<id>.<fn>(…)`
    /// terms, prepared by [`Self::prepare_filter_calls`] over the items in their
    /// current order. `retain` visits them in that order, so the running count is
    /// the item index the decisions are aligned to.
    pub(crate) fn apply_filter_with<D: PluginCallDecisions>(
        &mut self,
        target: &ConfigTarget,
        decisions: &D,
    ) -> FilterOutcome {
        let mut outcome = FilterOutcome::default();
        let mut index = 0;
        self.items.retain(|item| {
            let provider = ValueProvider { pli: item, match_as_ascii: false };
            let keep =
                target.filter.processing.as_ref().is_none_or(|filter| filter.filter_with(&provider, decisions, index));
            index += 1;
            outcome.record(keep)
        });
        self.normalize_filter_grouping();
        outcome
    }

    /// The prepare half of the processing filter's plugin terms (plan §7,
    /// pattern 2): one `eval-filter-batch` per term over the buffered items, in
    /// order. `None` when the filter has no plugin term, so ordinary filters
    /// never build a batch. A failed call answers `false` for every item and is
    /// counted in `report`.
    pub(crate) async fn prepare_filter_calls<P: PluginHost>(
        &self,
        target: &ConfigTarget,
        plugins: &P,
        ctx: &HookCtx<'_>,
        report: &mut PluginRunReport,
    ) -> Option<PluginCallBatch> {
        let mut calls = target.filter.processing.as_ref()?.plugin_calls().peekable();
        calls.peek()?;
        let items: Vec<&PlaylistItem> = self.items.iter().collect();
        let mut batch = PluginCallBatch::new(items.len());
        for call in calls {
            let decisions = plugins.eval_filter_batch(call.plugin, call.func, call.args, ctx, &items, report).await;
            if !batch.set(call.index, decisions) {
                log::warn!(
                    "plugin {} filter function {} returned the wrong number of decisions; the term is false",
                    call.plugin,
                    call.func
                );
            }
        }
        Some(batch)
    }

    pub(crate) fn normalize_filter_grouping(&mut self) {
        self.grouping = GroupingPolicy::NormalizedCategory;
        self.reorder_for_grouping();
    }

    pub(crate) fn apply_rename(&mut self, target: &ConfigTarget) -> Option<RenameOutcome> {
        let renames = target.rename.as_ref().filter(|renames| !renames.is_empty())?;
        let mut outcome = RenameOutcome::default();
        for item in &mut self.items {
            outcome.inspected += 1;
            let changed_fields = exec_rename(item, Some(renames));
            outcome.changed_fields += changed_fields;
            outcome.changed_items += usize::from(changed_fields > 0);
        }
        self.grouping = GroupingPolicy::ExactCategory;
        self.reorder_for_grouping();
        Some(outcome)
    }

    pub(crate) fn apply_mapping(&mut self, target: &ConfigTarget, stage: MappingStage) -> Option<MappingStageOutcome> {
        if !has_mapping_stage(target, stage) {
            return None;
        }
        let items = std::mem::take(&mut self.items);
        let (items, outcome) = map_items_at_stage(items, target, stage, None)
            .expect("mapping stage applicability was checked before consuming the buffer");
        self.items = items;
        self.grouping = GroupingPolicy::ExactSequential;
        self.reorder_for_grouping();
        Some(outcome)
    }

    pub(crate) fn reapply_locked(&mut self) -> bool {
        let Some(locked) = self.locked.filter(|l| !l.is_empty()) else {
            return false;
        };
        let moved = reapply_target_overrides(self.items.iter_mut(), locked);
        if moved {
            self.reorder_for_grouping();
        }
        moved
    }

    pub(crate) fn reorder_for_grouping(&mut self) {
        let mut buckets: IndexMap<CategoryKey, Vec<PlaylistItem>> = IndexMap::new();
        for item in std::mem::take(&mut self.items) {
            let title = item.header.group.clone();
            let key_title = match self.grouping {
                GroupingPolicy::NormalizedCategory => shared::utils::deunicode_string(&title).to_lowercase().intern(),
                GroupingPolicy::ExactCategory | GroupingPolicy::ExactSequential => title,
            };
            buckets.entry((item.header.xtream_cluster, key_title)).or_default().push(item);
        }
        self.items = buckets.into_values().flatten().collect();
    }

    pub(crate) fn into_groups(self) -> Vec<PlaylistGroup> { group_items(self.items, self.grouping) }

    /// Run a plugin stage hook over the buffer (plan §7 *Dispatcher*). The
    /// host drops and rewrites items in place; a rewritten `group` is
    /// regrouped here, by the buffer, never by the host. The buffer keeps the
    /// grouping policy of the stage it is in: a hook before the filter or
    /// between stages must not switch it.
    pub(crate) async fn run_playlist_hook<P: PluginHost>(
        &mut self,
        plugins: &P,
        hook: PluginHook,
        ctx: &HookCtx<'_>,
        report: &mut PluginRunReport,
    ) -> Result<(), PluginAbort> {
        let mut run = PluginRunReport::default();
        plugins.on_stage(hook, ctx, &mut self.items, &mut run).await?;
        if run.hooks.iter().any(|stats| stats.modified > 0) {
            self.reorder_for_grouping();
        }
        report.merge(run);
        Ok(())
    }
}

pub(crate) fn group_items(items: Vec<PlaylistItem>, policy: GroupingPolicy) -> Vec<PlaylistGroup> {
    let mut groups: IndexMap<CategoryKey, PlaylistGroup> = IndexMap::new();
    let mut next_group_id = 0;
    for item in items {
        let title = item.header.group.clone();
        let cluster = item.header.xtream_cluster;
        let key_title = match policy {
            GroupingPolicy::NormalizedCategory => shared::utils::deunicode_string(&title).to_lowercase().intern(),
            GroupingPolicy::ExactCategory | GroupingPolicy::ExactSequential => title.clone(),
        };
        groups
            .entry((cluster, key_title))
            .or_insert_with(|| {
                let id = match policy {
                    GroupingPolicy::ExactSequential => {
                        next_group_id += 1;
                        next_group_id
                    }
                    GroupingPolicy::NormalizedCategory | GroupingPolicy::ExactCategory => item.header.category_id,
                };
                PlaylistGroup { id, title, channels: Vec::new(), xtream_cluster: cluster }
            })
            .channels
            .push(item);
    }
    groups.into_values().collect()
}

fn apply_stage(
    buffer: &mut TransformBuffer<'_>,
    outcome: &mut PipelineOutcome,
    target: &ConfigTarget,
    stage: TransformStage,
    filter_calls: Option<&PluginCallBatch>,
) {
    match stage {
        TransformStage::Filter => {
            if target.filter.processing.is_some() {
                outcome.filter = Some(match filter_calls {
                    Some(decisions) => buffer.apply_filter_with(target, decisions),
                    None => buffer.apply_filter(target),
                });
            } else {
                buffer.normalize_filter_grouping();
            }
        }
        TransformStage::Rename => {
            outcome.rename = buffer.apply_rename(target);
            buffer.reapply_locked();
        }
        TransformStage::Map => {
            outcome.mapping = buffer.apply_mapping(target, MappingStage::Processing);
            buffer.reapply_locked();
        }
    }
}

/// Runs the target's transform stages over one input's items, awaiting plugin
/// stage hooks between them.
///
/// The stages themselves stay synchronous; only the loop is async. With
/// [`NoPlugins`] `wants` is constant `false`, so no hook future is ever built.
pub(crate) async fn execute_pipeline_on_items<P: PluginHost>(
    items: Vec<PlaylistItem>,
    target: &ConfigTarget,
    input: &str,
    pipe: &[TransformStage],
    plugins: &P,
    locked: Option<&HashMap<UUIDType, Arc<ItemOverride>>>,
) -> Result<(Vec<PlaylistGroup>, PipelineOutcome), PluginAbort> {
    let ctx = HookCtx::new(&target.name).with_input(input);
    run_stages(TransformBuffer::new(items, locked), target, pipe, plugins, &ctx, true).await
}

/// The stage loop of both async pipelines. Stage hooks run only when
/// `stage_hooks` is set; the processing filter's plugin terms are always
/// prepared right before the Filter stage, over the items as that stage sees
/// them.
async fn run_stages<P: PluginHost>(
    mut buffer: TransformBuffer<'_>,
    target: &ConfigTarget,
    pipe: &[TransformStage],
    plugins: &P,
    ctx: &HookCtx<'_>,
    stage_hooks: bool,
) -> Result<(Vec<PlaylistGroup>, PipelineOutcome), PluginAbort> {
    let mut outcome = PipelineOutcome::default();
    for &stage in pipe {
        if stage == TransformStage::Filter {
            buffer.items.append(&mut buffer.managed.before);
        }
        let (before, after) = stage_hook_pair(stage);
        if stage_hooks && plugins.wants(before, &target.name) {
            buffer.run_playlist_hook(plugins, before, ctx, &mut outcome.plugins).await?;
        }
        let filter_calls = if stage == TransformStage::Filter {
            buffer.prepare_filter_calls(target, plugins, ctx, &mut outcome.plugins).await
        } else {
            None
        };
        apply_stage(&mut buffer, &mut outcome, target, stage, filter_calls.as_ref());
        if stage_hooks && plugins.wants(after, &target.name) {
            buffer.run_playlist_hook(plugins, after, ctx, &mut outcome.plugins).await?;
        }
        // Inventory hooks see only the filtered provider view; injected channels enter afterwards.
        if stage == TransformStage::Filter {
            buffer.items.append(&mut buffer.managed.after);
        }
    }
    Ok((buffer.into_groups(), outcome))
}

/// The `(before, after)` plugin hooks around a transform stage, so the hooks
/// follow `execution_plan.transform_stages` whatever order the target uses.
const fn stage_hook_pair(stage: TransformStage) -> (PluginHook, PluginHook) {
    match stage {
        TransformStage::Filter => (PluginHook::BeforeFilter, PluginHook::AfterFilter),
        TransformStage::Rename => (PluginHook::BeforeRename, PluginHook::AfterRename),
        TransformStage::Map => (PluginHook::BeforeMap, PluginHook::AfterMap),
    }
}

/// The stage pipeline for already-grouped items, such as resolved series
/// episodes. Deliberately hook-free and synchronous: stage hooks fire once per
/// input in [`execute_pipeline_on_items`], not again for every episode list.
pub(crate) fn execute_pipeline_on_groups(
    groups: Vec<PlaylistGroup>,
    target: &ConfigTarget,
    pipe: &[TransformStage],
    locked: Option<&HashMap<UUIDType, Arc<ItemOverride>>>,
) -> (Vec<PlaylistGroup>, PipelineOutcome) {
    if pipe.is_empty() {
        return (groups, PipelineOutcome::default());
    }
    let mut buffer = TransformBuffer::new(groups.into_iter().flat_map(|group| group.channels).collect(), locked);
    let mut outcome = PipelineOutcome::default();
    for &stage in pipe {
        apply_stage(&mut buffer, &mut outcome, target, stage, None);
    }
    (buffer.into_groups(), outcome)
}

/// [`execute_pipeline_on_groups`] for a caller that has the plugin host, such
/// as series-episode resolution. Still no stage hooks, but the processing
/// filter's `plugin.<id>.<fn>(…)` terms get real decisions; through the sync
/// variant they answer `false`, and an `AND` term would drop every episode.
/// Filter terms are fail-open, so this cannot abort.
pub(crate) async fn execute_pipeline_on_groups_with_filter_calls<P: PluginHost>(
    groups: Vec<PlaylistGroup>,
    target: &ConfigTarget,
    input: &str,
    pipe: &[TransformStage],
    plugins: &P,
    locked: Option<&HashMap<UUIDType, Arc<ItemOverride>>>,
) -> (Vec<PlaylistGroup>, PipelineOutcome) {
    let has_plugin_terms =
        target.filter.processing.as_ref().is_some_and(|filter| filter.plugin_calls().next().is_some());
    if !has_plugin_terms {
        return execute_pipeline_on_groups(groups, target, pipe, locked);
    }
    let buffer = TransformBuffer::new(groups.into_iter().flat_map(|group| group.channels).collect(), locked);
    let ctx = HookCtx::new(&target.name).with_input(input);
    match run_stages(buffer, target, pipe, plugins, &ctx, false).await {
        Ok(result) => result,
        Err(_) => unreachable!("only stage hooks can abort, and none run here"),
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_pipe<'a, P: PluginHost>(
    target: &ConfigTarget,
    pipe: &ProcessingPipe,
    fpl: &mut FetchedPlaylist<'a>,
    duplicates: &mut HashSet<UUIDType>,
    consume_source: bool,
    bouquet_filter: Option<&tuliprox_core::model::TargetBouquetFilter>,
    plugins: &P,
    overrides: Option<&TargetOverrideSet>,
) -> Result<(FetchedPlaylist<'a>, PipelineOutcome), TuliproxError> {
    execute_pipe_with_insertions(
        target,
        pipe,
        fpl,
        duplicates,
        consume_source,
        bouquet_filter,
        plugins,
        overrides,
        ManagedStageItems::default(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_pipe_with_insertions<'a, P: PluginHost>(
    target: &ConfigTarget,
    pipe: &ProcessingPipe,
    fpl: &mut FetchedPlaylist<'a>,
    duplicates: &mut HashSet<UUIDType>,
    consume_source: bool,
    bouquet_filter: Option<&tuliprox_core::model::TargetBouquetFilter>,
    plugins: &P,
    overrides: Option<&TargetOverrideSet>,
    managed: ManagedStageItems,
) -> Result<(FetchedPlaylist<'a>, PipelineOutcome), TuliproxError> {
    let mut source = if consume_source {
        if fpl.is_memory() {
            MemoryPlaylistSource::new(fpl.source.take_groups()).into_source()
        } else {
            std::mem::replace(&mut fpl.source, MemoryPlaylistSource::default().into_source())
        }
    } else {
        fpl.clone_source()?
    };

    let is_memory = source.is_memory();
    let pre_transform_dedup_enabled = target.execution_plan.pre_transform_identity_dedup;
    if !is_memory && pre_transform_dedup_enabled {
        warn!(
            "Target '{}' input '{}': pre_transform_identity_dedup has no effect for disk-based playlist sources",
            target.name, fpl.input.name
        );
    }
    // Memory sources dedup here; disk sources cannot, because persisted B+Tree
    // leaves are read-only at the pipeline boundary. Cross-input dedup for disk
    // sources runs later in `deduplicate_playlist` during target finalization.
    let deduplicate_memory = is_memory && pre_transform_dedup_enabled;
    let mut rejected = 0usize;
    let mut duplicates_removed = 0usize;

    let mut items: Vec<PlaylistItem> = source.into_items().collect();
    let total_items = items.len();

    let (applied_count, ambiguous_count) =
        if let Some(set) = overrides.filter(|set| set.has_overrides_for(fpl.input.name.as_ref())) {
            let outcome = apply_overrides(&mut items, set);
            set.target_locked.lock().extend(outcome.target_locked);
            set.record_ambiguous_keys(outcome.ambiguous.len());
            (outcome.applied, outcome.ambiguous.len())
        } else {
            (0, 0)
        };

    items.retain_mut(|item| {
        if is_memory {
            item.header.freeze_input_stream_id();
        }
        if bouquet_filter.is_some_and(|filter| !filter.allows(item.header.xtream_cluster, &item.header.group)) {
            rejected += 1;
            return false;
        }
        if deduplicate_memory && !duplicates.insert(item.get_uuid()) {
            duplicates_removed += 1;
            return false;
        }
        true
    });

    if applied_count > 0 || ambiguous_count > 0 {
        debug!(
            "Target '{}' input '{}': applied {} override(s), {} ambiguous item(s)",
            target.name, fpl.input.name, applied_count, ambiguous_count
        );
    }
    if rejected > 0 {
        debug!(
            "Target '{}' bouquet prefilter for input '{}': rejected {}/{} items",
            target.name, fpl.input.name, rejected, total_items
        );
    }
    if duplicates_removed > 0 {
        debug!(
            "Target '{}' input '{}': pre-transform dedup removed {} duplicate items",
            target.name, fpl.input.name, duplicates_removed
        );
    }

    // Owned copy: a lock guard must not be held across the plugin hooks' await points.
    let locked_owned = overrides.map(|set| set.target_locked().clone());
    let result = if managed.is_empty() {
        execute_pipeline_on_items(items, target, &fpl.input.name, pipe, plugins, locked_owned.as_ref()).await
    } else {
        let mut buffer = TransformBuffer::new(items, locked_owned.as_ref());
        buffer.managed = managed;
        run_stages(buffer, target, pipe, plugins, &HookCtx::new(&target.name).with_input(&fpl.input.name), true).await
    };
    let (groups, outcome) = result.map_err(|abort| TuliproxError::Plugin(abort.to_string()))?;
    let new_fpl = FetchedPlaylist {
        input: fpl.input,
        source: MemoryPlaylistSource::new(groups).into_source(),
        epg: fpl.epg.clone(),
    };
    Ok((new_fpl, outcome))
}

// This method is needed, because of duplicate group names in different inputs.
// We merge the same group names considering cluster together.
pub(crate) fn flatten_groups(playlistgroups: Vec<PlaylistGroup>) -> Vec<PlaylistGroup> {
    let upper_bound = playlistgroups.len();
    let mut sort_order: Vec<PlaylistGroup> = Vec::with_capacity(upper_bound);
    let mut idx: usize = 0;
    let mut group_map: HashMap<CategoryKey, usize> = HashMap::with_capacity(upper_bound);
    for group in playlistgroups {
        let normalized_title: Arc<str> = shared::utils::deunicode_string(&group.title).to_lowercase().intern();
        let key = (group.xtream_cluster, normalized_title);
        match group_map.entry(key) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(idx);
                idx += 1;
                sort_order.push(group);
            }
            std::collections::hash_map::Entry::Occupied(o) => {
                if let Some(pl_group) = sort_order.get_mut(*o.get()) {
                    pl_group.channels.extend(group.channels);
                }
            }
        }
    }
    sort_order
}
