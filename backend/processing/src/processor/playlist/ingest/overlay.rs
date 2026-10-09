use super::{cluster_selected, GroupCategoryIds, PlaylistDownloadResult, StagedOverlayAssignment, StreamOverlap};
use log::warn;
use shared::model::{ClusterFlags, InputType, PlaylistGroup, StagedInputType, XtreamCluster};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tuliprox_core::model::{ConfigInput, ConfigInputFlags};

fn prepare_staged_xtream_group(
    mut group: PlaylistGroup,
    provider: &ConfigInput,
    provider_urls: &HashMap<(XtreamCluster, u32), &Arc<str>>,
    username: &str,
    password: &str,
    live_stream_use_prefix: bool,
    live_stream_without_extension: bool,
) -> PlaylistGroup {
    let initial_count = group.channels.len();
    group.channels.retain_mut(|item| {
        // The staged item's ID identifies its original Xtream stream.
        let Ok(stream_id) = item.header.id.parse::<u32>() else {
            return false;
        };
        // Existing streams keep the provider's playback URL, including a direct source URL.
        item.header.url = provider_urls.get(&(group.xtream_cluster, stream_id)).map_or_else(
            || {
                let metadata_extension = item.header.get_container_extension();
                let container_extension = if group.xtream_cluster == XtreamCluster::Video {
                    metadata_extension.as_deref().or_else(|| {
                        shared::utils::extract_extension_from_url(&item.header.url)
                            .and_then(|extension| extension.strip_prefix('.'))
                    })
                } else {
                    None
                };
                tuliprox_parser::xtream::get_xtream_url(
                    group.xtream_cluster,
                    &provider.url,
                    username,
                    password,
                    stream_id,
                    container_extension,
                    live_stream_use_prefix,
                    live_stream_without_extension,
                )
                .into()
            },
            |url| Arc::clone(url),
        );
        item.header.input_name = Arc::clone(&provider.name);
        true
    });
    let dropped = initial_count - group.channels.len();
    if dropped > 0 {
        warn!("Skipped {dropped} staged channel(s) in group '{}' without a numeric Xtream stream ID", group.title);
    }
    group
}

/// Overlays the staged source on the provider playlist of the clusters it covers.
///
/// The staged playlist is the provider playlist restructured by an external tool: group and channel
/// names may differ, the ids do not. Staged groups are therefore resolved against the provider groups
/// by id, never by name ([`match_staged_groups`]). An overlaid group keeps the provider category id;
/// a staged group that introduces a new category keeps its own id only when that id is still free. A
/// staged group whose channels carry no numeric stream id leaves the provider category untouched for
/// Xtream staged input, while an M3U staged input remains authoritative for the selected clusters.
pub(crate) fn apply_staged_overlay_groups(
    provider: &ConfigInput,
    staged_type: StagedInputType,
    clusters: ClusterFlags,
    provider_groups: Vec<PlaylistGroup>,
    staged_groups: Vec<PlaylistGroup>,
) -> Vec<PlaylistGroup> {
    if provider.input_type != InputType::Xtream {
        let mut groups: Vec<PlaylistGroup> =
            provider_groups.into_iter().filter(|group| !cluster_selected(group.xtream_cluster, clusters)).collect();
        groups.extend(staged_groups.into_iter().filter(|group| cluster_selected(group.xtream_cluster, clusters)).map(
            |mut group| {
                for item in &mut group.channels {
                    item.header.input_name = Arc::clone(&provider.name);
                }
                group
            },
        ));
        return groups;
    }

    let (Some(username), Some(password)) = (provider.username.as_deref(), provider.password.as_deref()) else {
        warn!("Skipping staged channels for Xtream input '{}' without credentials", provider.name);
        return provider_groups;
    };
    let live_stream_use_prefix = provider.has_flag(ConfigInputFlags::XtreamLiveStreamUsePrefix);
    let live_stream_without_extension = provider.has_flag(ConfigInputFlags::XtreamLiveStreamWithoutExtension);

    let staged_stream_ids: HashSet<(XtreamCluster, u32)> = staged_groups
        .iter()
        .filter(|group| cluster_selected(group.xtream_cluster, clusters))
        .flat_map(|group| {
            group.channels.iter().filter_map(|item| {
                item.header.id.parse::<u32>().ok().map(|stream_id| (group.xtream_cluster, stream_id))
            })
        })
        .collect();
    let mut provider_urls = HashMap::with_capacity(staged_stream_ids.len());
    for group in &provider_groups {
        if !cluster_selected(group.xtream_cluster, clusters) {
            continue;
        }
        for item in &group.channels {
            if let Ok(stream_id) = item.header.id.parse::<u32>() {
                let key = (group.xtream_cluster, stream_id);
                if staged_stream_ids.contains(&key) {
                    provider_urls.entry(key).or_insert(&item.header.url);
                }
            }
        }
    }

    let mut staged_selected: Vec<Option<PlaylistGroup>> = staged_groups
        .into_iter()
        .filter(|group| cluster_selected(group.xtream_cluster, clusters))
        .map(|group| {
            Some(prepare_staged_xtream_group(
                group,
                provider,
                &provider_urls,
                username,
                password,
                live_stream_use_prefix,
                live_stream_without_extension,
            ))
        })
        .collect();

    // Category ids of the provider playlist are authoritative and stay reserved across the overlay:
    // persistence keys input groups by `(cluster, id)`, so two groups of one cluster sharing an id
    // would be collapsed into a single category. Only staged ids are renumbered; a provider playlist
    // that already repeats a category id is passed through unchanged.
    let mut used_ids = GroupCategoryIds::from_groups(&provider_groups);
    let assignment = match_staged_groups(&provider_groups, &staged_selected, staged_type, clusters);

    if staged_type == StagedInputType::M3u {
        return apply_staged_m3u_template(provider_groups, staged_selected, &assignment, clusters, used_ids);
    }

    let mut result: Vec<PlaylistGroup> = Vec::with_capacity(provider_groups.len() + staged_selected.len());

    for (provider_idx, provider_group) in provider_groups.into_iter().enumerate() {
        if !cluster_selected(provider_group.xtream_cluster, clusters) {
            result.push(provider_group);
            continue;
        }

        let matched = assignment.staged_of(provider_idx).and_then(|staged_idx| staged_selected[staged_idx].take());
        let Some(mut staged) = matched.filter(|staged| !staged.channels.is_empty()) else {
            result.push(provider_group);
            continue;
        };

        // The provider category id is the identity of the overlaid category: a staged group matched
        // by stream ids or title takes it over instead of keeping its own numbering.
        staged.id =
            if provider_group.id == 0 { used_ids.allocate(provider_group.xtream_cluster) } else { provider_group.id };
        sync_channel_category_id(&mut staged);
        result.push(staged);
    }

    // Append newly introduced staged groups that had no corresponding provider group.
    for mut staged in staged_selected.into_iter().flatten().filter(|staged| !staged.channels.is_empty()) {
        used_ids.assign_new_group(&mut staged);
        sync_channel_category_id(&mut staged);
        result.push(staged);
    }

    result
}

fn apply_staged_m3u_template(
    provider_groups: Vec<PlaylistGroup>,
    staged_groups: Vec<Option<PlaylistGroup>>,
    assignment: &StagedOverlayAssignment,
    clusters: ClusterFlags,
    mut used_ids: GroupCategoryIds,
) -> Vec<PlaylistGroup> {
    let mut result = Vec::with_capacity(provider_groups.len() + staged_groups.len());
    for (staged_idx, staged) in staged_groups.into_iter().enumerate() {
        let Some(mut staged) = staged.filter(|group| !group.channels.is_empty()) else {
            continue;
        };
        if let Some(provider_idx) = assignment.provider_by_staged[staged_idx] {
            let provider_group = &provider_groups[provider_idx];
            staged.id = if provider_group.id == 0 {
                used_ids.allocate(provider_group.xtream_cluster)
            } else {
                provider_group.id
            };
        } else {
            used_ids.assign_new_group(&mut staged);
        }
        sync_channel_category_id(&mut staged);
        result.push(staged);
    }
    result.extend(provider_groups.into_iter().filter(|group| !cluster_selected(group.xtream_cluster, clusters)));
    result
}

/// Keeps the channels of a group consistent with its final category id.
///
/// Persistence re-syncs this through [`PlaylistGroup::on_load`], but the overlay result is consumed as
/// a whole: a reader must not see the category id of the staged source on a group that was just given
/// the provider category id.
fn sync_channel_category_id(group: &mut PlaylistGroup) {
    let category_id = group.id;
    for item in &mut group.channels {
        item.header.category_id = category_id;
    }
}

impl StagedOverlayAssignment {
    pub(super) fn new(provider_len: usize, staged_len: usize) -> Self {
        Self {
            staged_by_provider: vec![None; provider_len],
            provider_by_staged: vec![None; staged_len],
            has_stream_id_evidence: vec![false; staged_len],
        }
    }

    /// Assigns `staged_idx` to `provider_idx`; returns `false` when either side is already taken.
    pub(super) fn assign(&mut self, provider_idx: usize, staged_idx: usize) -> bool {
        if self.staged_by_provider[provider_idx].is_some() || self.provider_by_staged[staged_idx].is_some() {
            return false;
        }
        self.staged_by_provider[provider_idx] = Some(staged_idx);
        self.provider_by_staged[staged_idx] = Some(provider_idx);
        true
    }

    pub(super) fn staged_of(&self, provider_idx: usize) -> Option<usize> { self.staged_by_provider[provider_idx] }

    pub(super) fn is_assigned(&self, staged_idx: usize) -> bool { self.provider_by_staged[staged_idx].is_some() }
}

/// Resolves every staged group against the provider groups it overlays.
///
/// The staged playlist is the provider playlist restructured by an external tool: group and channel
/// names may be rewritten, the ids do not. The passes therefore rank the id evidence above the name:
///
/// 1. the channel stream ids decide. A staged channel carries the provider stream id, and the provider
///    playlist itself states which category owns a stream, so this also covers staged sources whose
///    group ids are not provider category ids (a `m3u` staged playlist numbers its groups positionally).
/// 2. the category id decides for Xtream staged groups whose channels the provider playlist does not know.
///    M3U staged group ids are positional and cannot identify provider categories.
/// 3. the title remains the last resort for staged groups without provider stream-id evidence.
fn match_staged_groups(
    provider_groups: &[PlaylistGroup],
    staged_groups: &[Option<PlaylistGroup>],
    staged_type: StagedInputType,
    clusters: ClusterFlags,
) -> StagedOverlayAssignment {
    let mut assignment = StagedOverlayAssignment::new(provider_groups.len(), staged_groups.len());
    match_staged_by_stream_id(provider_groups, staged_groups, clusters, &mut assignment);
    if staged_type == StagedInputType::Xtream {
        match_staged_by_category_id(provider_groups, staged_groups, clusters, &mut assignment);
    }
    match_staged_by_title(provider_groups, staged_groups, clusters, &mut assignment);
    assignment
}

/// Pass 1: a staged group overlays the provider category owning its streams, strongest overlap first.
fn match_staged_by_stream_id(
    provider_groups: &[PlaylistGroup],
    staged_groups: &[Option<PlaylistGroup>],
    clusters: ClusterFlags,
    assignment: &mut StagedOverlayAssignment,
) {
    // Only the staged stream ids are indexed, so the memory cost is bounded by the overlay and not
    // by the size of the provider playlist.
    let staged_stream_ids: HashSet<(XtreamCluster, &str)> = staged_groups
        .iter()
        .flatten()
        .flat_map(|staged| staged.channels.iter().map(|item| (staged.xtream_cluster, item.header.id.as_ref())))
        .filter(|(_, stream_id)| !stream_id.is_empty())
        .collect();
    if staged_stream_ids.is_empty() {
        return;
    }

    let mut provider_idx_by_stream_id: HashMap<(XtreamCluster, &str), usize> = HashMap::new();
    for (provider_idx, provider_group) in provider_groups.iter().enumerate() {
        if !cluster_selected(provider_group.xtream_cluster, clusters) {
            continue;
        }
        for item in &provider_group.channels {
            let key = (provider_group.xtream_cluster, item.header.id.as_ref());
            if staged_stream_ids.contains(&key) {
                provider_idx_by_stream_id.entry(key).or_insert(provider_idx);
            }
        }
    }
    if provider_idx_by_stream_id.is_empty() {
        return;
    }

    // Every staged group is offered all provider categories owning its streams, ranked by overlap: when
    // a stronger overlap takes the best-matching category, the next candidate can still claim the group.
    let mut candidates: Vec<StreamOverlap> = Vec::new();
    let mut hits_by_provider: HashMap<usize, usize> = HashMap::new();
    for (staged_idx, staged) in staged_groups.iter().enumerate() {
        let Some(staged) = staged else {
            continue;
        };
        hits_by_provider.clear();
        for item in &staged.channels {
            if let Some(provider_idx) = provider_idx_by_stream_id.get(&(staged.xtream_cluster, item.header.id.as_ref()))
            {
                *hits_by_provider.entry(*provider_idx).or_insert(0) += 1;
            }
        }
        assignment.has_stream_id_evidence[staged_idx] = !hits_by_provider.is_empty();
        candidates.extend(hits_by_provider.iter().map(|(provider_idx, hits)| StreamOverlap {
            hits: *hits,
            staged_idx,
            provider_idx: *provider_idx,
        }));
    }

    // The strongest overlap wins first, so a staged group never loses its category to a weaker candidate.
    candidates
        .sort_unstable_by_key(|overlap| (std::cmp::Reverse(overlap.hits), overlap.staged_idx, overlap.provider_idx));
    for overlap in candidates {
        assignment.assign(overlap.provider_idx, overlap.staged_idx);
    }
}

/// Pass 2: a staged group overlays the provider category carrying its id.
fn match_staged_by_category_id(
    provider_groups: &[PlaylistGroup],
    staged_groups: &[Option<PlaylistGroup>],
    clusters: ClusterFlags,
    assignment: &mut StagedOverlayAssignment,
) {
    let mut provider_idx_by_id: HashMap<(XtreamCluster, u32), usize> = HashMap::new();
    for (provider_idx, provider_group) in provider_groups.iter().enumerate() {
        if provider_group.id != 0 && cluster_selected(provider_group.xtream_cluster, clusters) {
            provider_idx_by_id.entry((provider_group.xtream_cluster, provider_group.id)).or_insert(provider_idx);
        }
    }

    for (staged_idx, staged) in staged_groups.iter().enumerate() {
        let Some(staged) = staged else {
            continue;
        };
        if staged.id == 0 || assignment.is_assigned(staged_idx) || assignment.has_stream_id_evidence[staged_idx] {
            continue;
        }
        if let Some(provider_idx) = provider_idx_by_id.get(&(staged.xtream_cluster, staged.id)).copied() {
            assignment.assign(provider_idx, staged_idx);
        }
    }
}

/// Pass 3: a staged group overlays the provider category of the same name.
fn match_staged_by_title(
    provider_groups: &[PlaylistGroup],
    staged_groups: &[Option<PlaylistGroup>],
    clusters: ClusterFlags,
    assignment: &mut StagedOverlayAssignment,
) {
    // Staged groups of the same name are interchangeable, so one lookup per provider category is enough.
    let mut staged_by_title: HashMap<(XtreamCluster, &str), Vec<usize>> = HashMap::new();
    for (staged_idx, staged) in staged_groups.iter().enumerate() {
        if assignment.is_assigned(staged_idx) || assignment.has_stream_id_evidence[staged_idx] {
            continue;
        }
        if let Some(staged) = staged {
            staged_by_title.entry((staged.xtream_cluster, staged.title.as_ref())).or_default().push(staged_idx);
        }
    }

    for (provider_idx, provider_group) in provider_groups.iter().enumerate() {
        if assignment.staged_of(provider_idx).is_some() || !cluster_selected(provider_group.xtream_cluster, clusters) {
            continue;
        }
        let key = (provider_group.xtream_cluster, provider_group.title.as_ref());
        let Some(free_groups) = staged_by_title.get_mut(&key) else {
            continue;
        };
        if !free_groups.is_empty() {
            // Buckets stay tiny, so taking the first free group keeps the earlier first-match semantics.
            let staged_idx = free_groups.remove(0);
            assignment.assign(provider_idx, staged_idx);
        }
    }
}

impl GroupCategoryIds {
    pub(super) fn from_groups(groups: &[PlaylistGroup]) -> Self {
        let mut ids = Self::default();
        for group in groups {
            if group.id != 0 {
                ids.used.entry(group.xtream_cluster).or_default().insert(group.id);
            }
        }
        for (cluster, used) in &ids.used {
            ids.next_free.insert(*cluster, used.iter().copied().max().unwrap_or(0).saturating_add(1));
        }
        ids
    }

    /// Claims `id` for `cluster`; a zero id or an id already in use is not claimable.
    pub(super) fn claim(&mut self, cluster: XtreamCluster, id: u32) -> bool {
        id != 0 && self.used.entry(cluster).or_default().insert(id)
    }

    pub(super) fn assign_new_group(&mut self, group: &mut PlaylistGroup) {
        if group.id == 0 {
            group.id = self.allocate(group.xtream_cluster);
        } else if !self.claim(group.xtream_cluster, group.id) {
            let allocated = self.allocate(group.xtream_cluster);
            warn!(
                "Staged group '{}' uses category id {} which is already taken in {}; renumbered to {allocated}",
                group.title, group.id, group.xtream_cluster
            );
            group.id = allocated;
        }
    }

    /// Reserves and returns the next free id of `cluster`.
    pub(super) fn allocate(&mut self, cluster: XtreamCluster) -> u32 {
        let used = self.used.entry(cluster).or_default();
        let mut candidate = self.next_free.get(&cluster).copied().unwrap_or(1);
        // Zero is never a valid category id. The loop wraps at the u32 edge and ends at the latest
        // when it reaches the first free id, which the finite id set always contains.
        while candidate == 0 || used.contains(&candidate) {
            candidate = candidate.checked_add(1).unwrap_or(1);
        }
        used.insert(candidate);
        self.next_free.insert(cluster, candidate.saturating_add(1));
        candidate
    }
}

pub(crate) fn should_apply_staged_overlay(download_result: &PlaylistDownloadResult) -> bool {
    !download_result.was_cached
}
