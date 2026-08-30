#![allow(clippy::wildcard_imports)]
use super::*;

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
}

impl PipelineOutcome {
    pub(crate) fn merge(&mut self, other: Self) {
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
    pub(crate) channel: PlaylistItem,
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
        let ref_chan = &mut channel;
        let templates = mapping.templates.as_deref();
        for (rule_index, rule) in mapping.rules.iter().enumerate() {
            let provider = ValueProvider { pli: ref_chan, match_as_ascii: mapping.match_as_ascii };
            if rule.filter.filter(&provider) {
                matched_rules += 1;
                let mut accessor = ValueAccessor {
                    pli: ref_chan,
                    virtual_items: vec![],
                    match_as_ascii: mapping.match_as_ascii,
                    changed_fields: vec![],
                };
                let outcome = match &rule.program {
                    MappingProgram::Script(script) => script.eval(&mut accessor, templates),
                };
                changed_fields.extend(outcome.changed_fields.iter().cloned());
                for diagnostic in outcome.diagnostics {
                    let rule_label = rule.name.as_deref().map_or_else(|| (rule_index + 1).to_string(), str::to_string);
                    diagnostics.push(format!(
                        "rule '{rule_label}' failed for channel '{}' at statement {}: {}",
                        accessor.pli.header.name,
                        diagnostic.statement + 1,
                        diagnostic.message
                    ));
                }
                virtual_items.extend(accessor.virtual_items.into_iter().map(|(_, pli)| pli));
            }
        }
    }
    ChannelMappingOutcome { channel, virtual_items, matched_rules, changed_fields, diagnostics }
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
            next_items.push(outcome.channel);
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

pub(crate) struct TransformBuffer {
    pub(crate) items: Vec<PlaylistItem>,
    pub(crate) grouping: GroupingPolicy,
}

impl TransformBuffer {
    pub(crate) fn new(items: Vec<PlaylistItem>) -> Self { Self { items, grouping: GroupingPolicy::ExactCategory } }

    pub(crate) fn apply_filter(&mut self, target: &ConfigTarget) -> FilterOutcome {
        let mut outcome = FilterOutcome::default();
        self.items.retain(|item| outcome.record(target.filter(&ValueProvider { pli: item, match_as_ascii: false })));
        self.normalize_filter_grouping();
        outcome
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

pub(crate) fn execute_pipeline_on_items(
    items: Vec<PlaylistItem>,
    target: &ConfigTarget,
    pipe: &[TransformStage],
) -> (Vec<PlaylistGroup>, PipelineOutcome) {
    let mut buffer = TransformBuffer::new(items);
    let mut outcome = PipelineOutcome::default();
    for stage in pipe {
        match stage {
            TransformStage::Filter => {
                if target.filter.processing.is_some() {
                    outcome.filter = Some(buffer.apply_filter(target));
                } else {
                    buffer.normalize_filter_grouping();
                }
            }
            TransformStage::Rename => outcome.rename = buffer.apply_rename(target),
            TransformStage::Map => outcome.mapping = buffer.apply_mapping(target, MappingStage::Processing),
        }
    }
    (buffer.into_groups(), outcome)
}

pub(crate) fn execute_pipeline_on_groups(
    groups: Vec<PlaylistGroup>,
    target: &ConfigTarget,
    pipe: &[TransformStage],
) -> (Vec<PlaylistGroup>, PipelineOutcome) {
    if pipe.is_empty() {
        return (groups, PipelineOutcome::default());
    }
    execute_pipeline_on_items(groups.into_iter().flat_map(|group| group.channels).collect(), target, pipe)
}

pub(crate) fn execute_pipe<'a>(
    target: &ConfigTarget,
    pipe: &ProcessingPipe,
    fpl: &mut FetchedPlaylist<'a>,
    duplicates: &mut HashSet<UUIDType>,
    consume_source: bool,
) -> Result<(FetchedPlaylist<'a>, PipelineOutcome), TuliproxError> {
    let source = if consume_source {
        if fpl.is_memory() {
            MemoryPlaylistSource::new(fpl.source.take_groups()).into_source()
        } else {
            std::mem::replace(&mut fpl.source, MemoryPlaylistSource::default().into_source())
        }
    } else {
        fpl.clone_source()?
    };

    let mut new_fpl = FetchedPlaylist { input: fpl.input, source, epg: fpl.epg.clone() };
    // In-memory items are frozen here at the target-processing boundary. Read-only disk sources
    // capture the same identity when their persisted M3U/Xtream items are converted to PlaylistItem.
    if new_fpl.is_memory() {
        for item in new_fpl.items_mut() {
            item.header.freeze_input_stream_id();
        }
    }
    if target.execution_plan.pre_transform_identity_dedup {
        new_fpl.deduplicate(duplicates);
    }

    let items = new_fpl.source.into_items().collect();
    let (groups, outcome) = execute_pipeline_on_items(items, target, pipe);
    new_fpl.source = MemoryPlaylistSource::new(groups).into_source();
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
