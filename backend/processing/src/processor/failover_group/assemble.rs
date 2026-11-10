use super::resolve::{
    group_uuid, resolve_failover_groups, CandidateOrigin, CatalogEntry, FailoverGroupResolution, ResolvedCandidate,
};
use crate::processor::match_key::{extract_normalized_match_key, MatchKeyNormalizer};
use log::{debug, info};
use shared::{
    model::{
        archive_info, CatchupProperties, ChannelMemberRefDto, FailoverGroupsConfig, FailoverOverrideDiagnosticsDto,
        LiveStreamProperties, PlaylistGroup, PlaylistItem, PlaylistItemHeader, StreamProperties,
        TargetFailoverOverridesDto, UUIDType, XtreamCluster,
    },
    utils::{create_alias_uuid, deunicode_string, token_quality, Internable},
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tuliprox_repository::{CatalogRecord, FailoverGroupBatch, FailoverGroupOrigin, PendingFailoverGroup};

impl From<CandidateOrigin> for FailoverGroupOrigin {
    fn from(origin: CandidateOrigin) -> Self {
        match origin {
            CandidateOrigin::Manual => Self::Manual,
            CandidateOrigin::Auto => Self::Auto,
            CandidateOrigin::AutoEdited => Self::AutoEdited,
        }
    }
}

pub(in crate::processor) fn strip_quality_words(value: &str) -> String {
    let words: Vec<&str> = value.split_whitespace().collect();
    let mut kept = Vec::with_capacity(words.len());
    for word in words {
        let core = word.trim_matches(|c: char| !c.is_alphanumeric());
        if !core.is_empty() && token_quality(core).is_some() {
            continue;
        }
        kept.push(word);
    }
    if kept.is_empty() {
        value.to_string()
    } else {
        kept.join(" ")
    }
}

fn is_eligible(item: &PlaylistItem) -> bool {
    item.header.xtream_cluster == XtreamCluster::Live && !item.header.item_type.is_local()
}

/// Playlist groups are compared by deunicode + lowercase in the flatten and transform stages; use the
/// same key here so copies land in the group the later stages will merge them into.
fn normalized_group_key(title: &str) -> String { deunicode_string(title).to_lowercase() }

/// Borrowed `(input, stream_id)` key of an eligible live channel, so the catalog pass can deduplicate
/// without allocating a `ChannelMemberRefDto` per channel.
fn item_member_ref_parts(item: &PlaylistItem) -> Option<(&str, &str)> {
    if !is_eligible(item) {
        return None;
    }
    let stream_id = item.header.input_stream_id.as_ref();
    if stream_id.is_empty() {
        return None;
    }
    Some((item.header.input_name.as_ref(), stream_id))
}

pub(in crate::processor) struct GroupCandidate {
    resolved: ResolvedCandidate,
    /// Playlist positions (group index, channel index) of `resolved.entries`.
    positions: Vec<(usize, usize)>,
}

pub(in crate::processor) struct ChannelCopy {
    pub position: (usize, usize),
    /// Index into the catalog records of the first occurrence of this member ref.
    pub member_ref_index: usize,
}

struct GroupSlot {
    id: u32,
    title: Arc<str>,
    xtream_cluster: XtreamCluster,
    had_channels: bool,
    channels: Vec<Option<PlaylistItem>>,
}

/// Natural auto key of a channel and whether the DSL forced it (`channel.header.failover_group`).
/// `None` means the channel opted out (`""`) or has no usable key.
pub(in crate::processor) fn natural_key_for(
    header: &PlaylistItemHeader,
    config: &FailoverGroupsConfig,
    normalizer: &MatchKeyNormalizer<'_>,
) -> (Option<String>, bool) {
    let (key, dsl_forced) = match header.failover_group.as_deref() {
        Some("") => return (None, false),
        Some(forced) => (normalizer.key(forced), true),
        None => (extract_normalized_match_key(header, config.match_by, config.match_as_ascii, Some(normalizer)), false),
    };
    if key.trim().is_empty() {
        (None, dsl_forced)
    } else {
        (Some(key), dsl_forced)
    }
}

/// The metadata of a group member that the group's own metadata can be derived from.
#[derive(Debug, Clone, Copy)]
pub struct MemberMetadata<'a> {
    pub name: &'a str,
    pub title: &'a str,
    pub logo: &'a str,
    pub epg_channel_id: Option<&'a str>,
    pub playlist_group: &'a str,
}

/// Name, logo, EPG id and playlist group of a group as shown to clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMetadata {
    pub name: String,
    pub title: String,
    pub logo: String,
    pub epg_channel_id: Option<String>,
    /// The override, else the playlist group of the representative; `None` when neither is set.
    pub playlist_group: Option<String>,
}

/// Single source of the group metadata rules, shared by processing and the REST preview: an override wins,
/// otherwise the first member (the representative) supplies the value, with the first non-empty logo and
/// non-blank EPG id of the remaining members as fallback. `members` is in member order.
pub fn derive_group_metadata<'a>(
    candidate: &ResolvedCandidate,
    members: impl IntoIterator<Item = MemberMetadata<'a>>,
    name_strip_quality: bool,
) -> GroupMetadata {
    let mut representative: Option<MemberMetadata<'a>> = None;
    let mut first_logo: Option<&str> = None;
    let mut first_epg: Option<&str> = None;
    for (idx, member) in members.into_iter().enumerate() {
        if idx == 0 {
            representative = Some(member);
        }
        if first_logo.is_none() && !member.logo.is_empty() {
            first_logo = Some(member.logo);
        }
        if first_epg.is_none() {
            first_epg = member.epg_channel_id.filter(|epg| !epg.trim().is_empty());
        }
    }

    let (name, title) = match (&candidate.name, representative) {
        (Some(name), _) => (name.clone(), name.clone()),
        (None, Some(rep)) if name_strip_quality => (strip_quality_words(rep.name), strip_quality_words(rep.title)),
        (None, Some(rep)) => (rep.name.to_string(), rep.title.to_string()),
        (None, None) => (String::new(), String::new()),
    };

    let logo = candidate.logo.clone().unwrap_or_else(|| first_logo.unwrap_or_default().to_string());

    let epg_channel_id = candidate.epg_channel_id.clone().or_else(|| {
        let own = representative.and_then(|rep| rep.epg_channel_id);
        first_epg.or(own).map(str::to_string)
    });

    let playlist_group = candidate
        .playlist_group
        .clone()
        .or_else(|| representative.map(|rep| rep.playlist_group).filter(|group| !group.is_empty()).map(str::to_string));

    GroupMetadata { name, title, logo, epg_channel_id, playlist_group }
}

fn build_group_metadata(
    candidate: &GroupCandidate,
    rep_header: &PlaylistItemHeader,
    slots: &[GroupSlot],
    config: &FailoverGroupsConfig,
    group_uuid: UUIDType,
) -> PlaylistItemHeader {
    let mut group_header = rep_header.clone();
    group_header.uuid = group_uuid;
    group_header.failover_group = None;
    group_header.failover_group_uuid = Some(group_uuid);
    group_header.chno = 0;

    let member_headers = || {
        candidate
            .positions
            .iter()
            .filter_map(|&pos| slots.get(pos.0).and_then(|g| g.channels.get(pos.1)).and_then(Option::as_ref))
            .map(|item| &item.header)
    };
    let derived = derive_group_metadata(
        &candidate.resolved,
        member_headers().map(|header| MemberMetadata {
            name: &header.name,
            title: &header.title,
            logo: &header.logo,
            epg_channel_id: header.epg_channel_id.as_deref(),
            playlist_group: &header.group,
        }),
        config.name_strip_quality,
    );
    group_header.name = derived.name.intern();
    group_header.title = derived.title.intern();
    group_header.logo = derived.logo.intern();
    group_header.epg_channel_id = derived.epg_channel_id.as_deref().map(Internable::intern);

    apply_archive_metadata(&mut group_header, candidate, slots);
    group_header
}

fn apply_archive_metadata(group_header: &mut PlaylistItemHeader, candidate: &GroupCandidate, slots: &[GroupSlot]) {
    let mut has_archive = false;
    let mut max_archive_duration: Option<i32> = None;
    let mut first_archive_catchup: Option<CatchupProperties> = None;
    let mut first_archive_timeshift: Option<Arc<str>> = None;

    for &pos in &candidate.positions {
        if let Some(item) = slots.get(pos.0).and_then(|g| g.channels.get(pos.1)).and_then(Option::as_ref) {
            let (member_has_archive, member_duration) =
                archive_info(item.header.additional_properties.as_ref(), item.header.time_shift.as_ref());

            if member_has_archive {
                has_archive = true;
                if let Some(dur) = member_duration.and_then(|days| i32::try_from(days).ok()) {
                    max_archive_duration = Some(max_archive_duration.map_or(dur, |max| max.max(dur)));
                }
                if let Some(StreamProperties::Live(live)) = &item.header.additional_properties {
                    if first_archive_catchup.is_none() && live.catchup.is_some() {
                        first_archive_catchup.clone_from(&live.catchup);
                    }
                }
                if first_archive_timeshift.is_none() && !item.header.time_shift.trim().is_empty() {
                    first_archive_timeshift = Some(Arc::clone(&item.header.time_shift));
                }
            }
        }
    }

    if has_archive {
        if let Some(ts) = first_archive_timeshift {
            group_header.time_shift = ts;
        } else if let Some(ref catchup) = first_archive_catchup {
            if let Some(ref days) = catchup.days {
                group_header.time_shift = days.clone();
            } else if let Some(dur) = max_archive_duration {
                group_header.time_shift = dur.to_string().intern();
            }
        } else if let Some(dur) = max_archive_duration {
            group_header.time_shift = dur.to_string().intern();
        }

        match &mut group_header.additional_properties {
            Some(StreamProperties::Live(live)) => {
                live.tv_archive = Some(1);
                live.tv_archive_duration = max_archive_duration;
                live.catchup = first_archive_catchup;
            }
            _ => {
                group_header.additional_properties = Some(StreamProperties::Live(Box::new(LiveStreamProperties {
                    name: group_header.name.clone(),
                    category_id: group_header.category_id,
                    stream_id: 0,
                    stream_icon: group_header.logo.clone(),
                    direct_source: "".intern(),
                    custom_sid: None,
                    added: None,
                    stream_type: None,
                    epg_channel_id: group_header.epg_channel_id.clone(),
                    tv_archive: Some(1),
                    tv_archive_duration: max_archive_duration,
                    catchup: first_archive_catchup,
                    is_adult: 0,
                    ..Default::default()
                })));
            }
        }
    }
}

/// Catalog record of an eligible channel from the key the resolution used, so the persisted
/// catalog and the grouping can not diverge.
fn catalog_record(
    header: &PlaylistItemHeader,
    member_ref: ChannelMemberRefDto,
    (natural_key, dsl_forced): (Option<String>, bool),
) -> CatalogRecord {
    CatalogRecord {
        member_ref,
        natural_key: natural_key.as_deref().map(Internable::intern),
        dsl_forced,
        epg_channel_id: header.epg_channel_id.clone(),
        name: Arc::clone(&header.name),
        title: header.get_name(),
        logo: header.logo.clone(),
        playlist_group: header.group.clone(),
        provider_id: header.provider_id_or_default(),
        has_archive: archive_info(header.additional_properties.as_ref(), header.time_shift.as_ref()).0,
    }
}

/// Group candidates, override diagnostics, catalog records (each member ref once),
/// and copies (later rows of already-seen member refs) in playlist order.
pub(in crate::processor) fn collect_group_candidates(
    config: &FailoverGroupsConfig,
    overrides: Option<&TargetFailoverOverridesDto>,
    playlist: &[PlaylistGroup],
) -> (Vec<GroupCandidate>, FailoverOverrideDiagnosticsDto, Vec<CatalogRecord>, Vec<ChannelCopy>) {
    let normalizer = MatchKeyNormalizer::new(&config.match_normalize, config.match_as_ascii);
    let mut positions: Vec<(usize, usize)> = Vec::new();
    let mut member_refs: Vec<ChannelMemberRefDto> = Vec::new();
    let mut natural_keys: Vec<(Option<String>, bool)> = Vec::new();
    let mut seen_members: HashMap<(&str, &str), usize> = HashMap::new();
    let mut copies = Vec::new();

    for (group_idx, group) in playlist.iter().enumerate() {
        if group.xtream_cluster != XtreamCluster::Live {
            continue;
        }
        for (channel_idx, channel) in group.channels.iter().enumerate() {
            let Some((input, stream_id)) = item_member_ref_parts(channel) else {
                continue;
            };
            match seen_members.entry((input, stream_id)) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let member_ref_index = member_refs.len();
                    entry.insert(member_ref_index);
                    positions.push((group_idx, channel_idx));
                    member_refs
                        .push(ChannelMemberRefDto { input: input.to_string(), stream_id: stream_id.to_string() });
                    natural_keys.push(natural_key_for(&channel.header, config, &normalizer));
                }
                std::collections::hash_map::Entry::Occupied(entry) => {
                    copies.push(ChannelCopy { position: (group_idx, channel_idx), member_ref_index: *entry.get() });
                }
            }
        }
    }

    let catalog: Vec<CatalogEntry<'_>> = positions
        .iter()
        .zip(&member_refs)
        .zip(&natural_keys)
        .map(|((&(group_idx, channel_idx), member_ref), (natural_key, dsl_forced))| CatalogEntry {
            member_ref,
            natural_key: natural_key.as_deref(),
            dsl_forced: *dsl_forced,
            epg_channel_id: playlist[group_idx].channels[channel_idx].header.epg_channel_id.as_deref(),
        })
        .collect();

    let FailoverGroupResolution { candidates, diagnostics } = resolve_failover_groups(config, overrides, &catalog);
    let candidates = candidates
        .into_iter()
        .map(|resolved| GroupCandidate {
            positions: resolved.entries.iter().map(|&entry| positions[entry]).collect(),
            resolved,
        })
        .collect();
    let catalog_records = positions
        .iter()
        .zip(member_refs)
        .zip(natural_keys)
        .map(|((&(group_idx, channel_idx), member_ref), key)| {
            catalog_record(&playlist[group_idx].channels[channel_idx].header, member_ref, key)
        })
        .collect();
    (candidates, diagnostics, catalog_records, copies)
}

fn log_diagnostics(target_name: &str, diagnostics: &FailoverOverrideDiagnosticsDto) {
    if diagnostics.is_empty() {
        return;
    }
    info!(
        "Failover group overrides of target '{target_name}': {} stale auto override(s), {} inactive manual group(s), {} conflicting ref(s), {} unresolved ref(s), {} dissolved auto group(s)",
        diagnostics.stale_auto_overrides.len(),
        diagnostics.inactive_manual_groups.len(),
        diagnostics.conflicting_refs.len(),
        diagnostics.unresolved_refs.len(),
        diagnostics.dissolved.len()
    );
    debug!("Failover group override diagnostics of target '{target_name}': {diagnostics:?}");
}

fn rebuild_playlist_from_slots(
    slots: Vec<GroupSlot>,
    deferred_placements: Vec<(String, PlaylistItem)>,
    playlist: &mut Vec<PlaylistGroup>,
) {
    for slot in slots {
        let channels: Vec<PlaylistItem> = slot.channels.into_iter().flatten().collect();
        if channels.is_empty() && slot.had_channels {
            continue;
        }
        playlist.push(PlaylistGroup { id: slot.id, title: slot.title, channels, xtream_cluster: slot.xtream_cluster });
    }

    let mut group_index: HashMap<Arc<str>, usize> = HashMap::with_capacity(playlist.len());
    for (idx, group) in playlist.iter().enumerate() {
        group_index.entry(Arc::clone(&group.title)).or_insert(idx);
    }

    for (pg_override, mut item) in deferred_placements {
        if let Some(&idx) = group_index.get(pg_override.as_str()) {
            let g = &mut playlist[idx];
            item.header.category_id = g.id;
            item.header.group = Arc::clone(&g.title);
            g.channels.push(item);
        } else {
            let next_id = playlist.iter().map(|g| g.id).max().unwrap_or(0) + 1;
            item.header.category_id = next_id;
            let title: Arc<str> = pg_override.as_str().intern();
            item.header.group = Arc::clone(&title);
            group_index.insert(Arc::clone(&title), playlist.len());
            playlist.push(PlaylistGroup {
                id: next_id,
                title,
                channels: vec![item],
                xtream_cluster: XtreamCluster::Live,
            });
        }
    }
}

fn materialize_groups(
    candidates: &[GroupCandidate],
    config: &FailoverGroupsConfig,
    target_name: &str,
    mut slots: Vec<GroupSlot>,
    copies: Vec<ChannelCopy>,
    playlist: &mut Vec<PlaylistGroup>,
) -> Vec<PendingFailoverGroup> {
    let mut deferred_placements = Vec::new();
    let mut pending_groups = Vec::with_capacity(candidates.len());
    let mut member_to_group = HashMap::new();
    let mut group_headers: Vec<PlaylistItemHeader> = Vec::with_capacity(candidates.len());
    let mut placed_in_groups = HashSet::new();

    for candidate in candidates {
        let rep_pos = candidate.positions[0];
        let Some(rep_item) = slots.get(rep_pos.0).and_then(|g| g.channels.get(rep_pos.1)).and_then(Option::as_ref)
        else {
            continue;
        };

        let group_uuid = group_uuid(target_name, candidate.resolved.origin, &candidate.resolved.key);
        let group_header = build_group_metadata(candidate, &rep_item.header, &slots, config, group_uuid);

        let dest_group: Arc<str> = candidate
            .resolved
            .playlist_group
            .as_ref()
            .map_or_else(|| slots[rep_pos.0].title.clone(), |s| s.as_str().intern());

        placed_in_groups.insert((group_uuid, normalized_group_key(&dest_group)));

        let mut members = Vec::with_capacity(candidate.positions.len());
        for (&catalog_index, &pos) in candidate.resolved.entries.iter().zip(&candidate.positions) {
            if let Some(item) = slots.get_mut(pos.0).and_then(|g| g.channels.get_mut(pos.1)).and_then(Option::take) {
                member_to_group.insert(catalog_index, group_headers.len());
                members.push(item);
            }
        }

        let mut group_item = PlaylistItem { header: group_header.clone() };

        if let Some(pg_override) = &candidate.resolved.playlist_group {
            deferred_placements.push((pg_override.clone(), group_item));
        } else if let Some(slot_group) = slots.get_mut(rep_pos.0) {
            // The group channel takes the playlist group of the slot it replaces.
            group_item.header.group = Arc::clone(&slot_group.title);
            group_item.header.category_id = slot_group.id;
            if let Some(slot) = slot_group.channels.get_mut(rep_pos.1) {
                *slot = Some(group_item);
            }
        }

        group_headers.push(group_header);

        pending_groups.push(PendingFailoverGroup {
            group_uuid,
            key: candidate.resolved.key.as_str().intern(),
            origin: candidate.resolved.origin.into(),
            policy: candidate.resolved.policy,
            members,
            orphaned: candidate.resolved.unresolved.clone(),
            epg_split: candidate.resolved.epg_split,
        });
    }

    for copy in copies {
        let Some(&group_idx) = member_to_group.get(&copy.member_ref_index) else {
            // Copies of ungrouped channels stay unchanged.
            continue;
        };
        let (pos_group, pos_channel) = copy.position;
        let copy_group = slots[pos_group].title.clone();
        let copy_category_id = slots[pos_group].id;
        let group_header = &group_headers[group_idx];
        let group_uuid = group_header.uuid;

        if !placed_in_groups.insert((group_uuid, normalized_group_key(&copy_group))) {
            // Drop copy sitting in the same playlist group as the group channel
            // (or if another copy of the same group is already in this playlist group).
            if let Some(slot) = slots.get_mut(pos_group).and_then(|g| g.channels.get_mut(pos_channel)) {
                *slot = None;
            }
        } else if let Some(slot) = slots.get_mut(pos_group).and_then(|g| g.channels.get_mut(pos_channel)) {
            let mut copy_header = group_header.clone();
            copy_header.uuid = create_alias_uuid(&group_uuid, copy_group.as_ref());
            copy_header.failover_group_uuid = Some(group_uuid);
            copy_header.category_id = copy_category_id;
            copy_header.group = copy_group;
            if let Some(StreamProperties::Live(live)) = &mut copy_header.additional_properties {
                live.category_id = copy_category_id;
            }
            *slot = Some(PlaylistItem { header: copy_header });
        }
    }

    rebuild_playlist_from_slots(slots, deferred_placements, playlist);
    pending_groups
}

pub(in crate::processor) fn group_playlist(
    config: &FailoverGroupsConfig,
    overrides: Option<&TargetFailoverOverridesDto>,
    target_name: &str,
    playlist: &mut Vec<PlaylistGroup>,
) -> FailoverGroupBatch {
    if playlist.is_empty() {
        return FailoverGroupBatch { enabled: true, ..FailoverGroupBatch::default() };
    }

    let (candidates, diagnostics, catalog, copies) = collect_group_candidates(config, overrides, playlist);
    log_diagnostics(target_name, &diagnostics);
    if candidates.is_empty() {
        return FailoverGroupBatch { catalog, enabled: true, ..FailoverGroupBatch::default() };
    }

    let slots: Vec<GroupSlot> = playlist
        .drain(..)
        .map(|g| GroupSlot {
            id: g.id,
            title: g.title,
            xtream_cluster: g.xtream_cluster,
            had_channels: !g.channels.is_empty(),
            channels: g.channels.into_iter().map(Some).collect(),
        })
        .collect();

    let groups = materialize_groups(&candidates, config, target_name, slots, copies, playlist);
    FailoverGroupBatch { groups, catalog, enabled: true, ..FailoverGroupBatch::default() }
}
