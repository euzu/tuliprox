use crate::processor::match_key::{extract_normalized_match_key, MatchKeyNormalizer};
use indexmap::IndexMap;
use shared::{
    model::{
        ChannelGroupOverridesDto, ChannelGroupPolicy, ChannelGroupsConfig, ChannelMemberRefDto, EpgConflictMode,
        LiveStreamProperties, PlaylistGroup, PlaylistItem, StreamProperties, UUIDType, XtreamCluster,
        MIN_GROUP_MEMBERS,
    },
    utils::{hash_string, token_quality, Internable},
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tuliprox_repository::{ChannelGroupOrigin, PendingChannelGroup};

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

fn item_member_ref(item: &PlaylistItem) -> Option<ChannelMemberRefDto> {
    if !is_eligible(item) {
        return None;
    }
    let stream_id = item.header.get_input_stream_id()?;
    if stream_id.is_empty() {
        return None;
    }
    Some(ChannelMemberRefDto { input: item.header.input_name.to_string(), stream_id: stream_id.to_string() })
}

struct GroupCandidate {
    key: String,
    origin: ChannelGroupOrigin,
    policy: ChannelGroupPolicy,
    positions: Vec<(usize, usize)>,
    orphaned: Vec<ChannelMemberRefDto>,
    epg_split: bool,
    name_override: Option<String>,
    logo_override: Option<String>,
    epg_override: Option<String>,
    playlist_group_override: Option<String>,
}

struct AutoBucket {
    positions: Vec<(usize, usize)>,
    has_dsl_forced: bool,
    epg_split: bool,
}

struct GroupSlot {
    id: u32,
    title: Arc<str>,
    xtream_cluster: XtreamCluster,
    had_channels: bool,
    channels: Vec<Option<PlaylistItem>>,
}

fn build_member_index(playlist: &[PlaylistGroup]) -> HashMap<ChannelMemberRefDto, (usize, usize)> {
    let mut member_pos_map = HashMap::new();
    for (group_idx, group) in playlist.iter().enumerate() {
        if group.xtream_cluster != XtreamCluster::Live {
            continue;
        }
        for (channel_idx, channel) in group.channels.iter().enumerate() {
            if let Some(mref) = item_member_ref(channel) {
                member_pos_map.entry(mref).or_insert((group_idx, channel_idx));
            }
        }
    }
    member_pos_map
}

fn resolve_manual_groups(
    config: &ChannelGroupsConfig,
    overrides: Option<&ChannelGroupOverridesDto>,
    member_pos_map: &HashMap<ChannelMemberRefDto, (usize, usize)>,
    taken_positions: &mut HashSet<(usize, usize)>,
    candidates: &mut Vec<GroupCandidate>,
) {
    let Some(overrides) = overrides else {
        return;
    };
    for mg in &overrides.groups {
        let mut matched_positions = Vec::new();
        let mut orphaned = Vec::new();
        for mref in &mg.members {
            if let Some(&pos) = member_pos_map.get(mref) {
                if !taken_positions.contains(&pos) {
                    taken_positions.insert(pos);
                    matched_positions.push(pos);
                    continue;
                }
            }
            log::warn!(
                "Manual channel group '{}' member '{}:{}' not found or already taken",
                mg.id,
                mref.input,
                mref.stream_id
            );
            orphaned.push(mref.clone());
        }

        if matched_positions.len() < MIN_GROUP_MEMBERS {
            continue;
        }

        candidates.push(GroupCandidate {
            key: mg.id.clone(),
            origin: ChannelGroupOrigin::Manual,
            policy: mg.policy.unwrap_or(config.policy),
            positions: matched_positions,
            orphaned,
            epg_split: false,
            name_override: Some(mg.name.clone()),
            logo_override: mg.logo.clone(),
            epg_override: mg.epg_channel_id.clone(),
            playlist_group_override: mg.playlist_group.clone(),
        });
    }
}

fn collect_auto_buckets(
    config: &ChannelGroupsConfig,
    playlist: &[PlaylistGroup],
    excluded_set: &HashSet<&ChannelMemberRefDto>,
    taken_positions: &HashSet<(usize, usize)>,
) -> IndexMap<String, AutoBucket> {
    let normalizer = MatchKeyNormalizer::new(&config.match_normalize, config.match_as_ascii);
    let mut auto_buckets: IndexMap<String, AutoBucket> = IndexMap::new();

    for (group_idx, group) in playlist.iter().enumerate() {
        if group.xtream_cluster != XtreamCluster::Live {
            continue;
        }
        for (channel_idx, channel) in group.channels.iter().enumerate() {
            let pos = (group_idx, channel_idx);
            if taken_positions.contains(&pos) {
                continue;
            }
            let Some(mref) = item_member_ref(channel) else {
                continue;
            };
            if excluded_set.contains(&mref) {
                continue;
            }

            let mut dsl_forced = false;
            let key = match channel.header.channel_group.as_deref() {
                Some("") => continue,
                Some(forced) => {
                    dsl_forced = true;
                    normalizer.key(forced)
                }
                None => extract_normalized_match_key(
                    &channel.header,
                    config.match_by,
                    config.match_as_ascii,
                    Some(&normalizer),
                ),
            };

            if key.trim().is_empty() {
                continue;
            }

            let bucket = auto_buckets.entry(key).or_insert_with(|| AutoBucket {
                positions: Vec::new(),
                has_dsl_forced: false,
                epg_split: false,
            });
            bucket.positions.push(pos);
            if dsl_forced {
                bucket.has_dsl_forced = true;
            }
        }
    }
    auto_buckets
}

fn split_epg_buckets(
    playlist: &[PlaylistGroup],
    auto_buckets: IndexMap<String, AutoBucket>,
) -> IndexMap<String, AutoBucket> {
    let mut split_buckets: IndexMap<String, AutoBucket> = IndexMap::new();
    for (key, bucket) in auto_buckets {
        if bucket.has_dsl_forced {
            split_buckets.insert(key, bucket);
            continue;
        }

        let mut epg_groups: IndexMap<String, Vec<(usize, usize)>> = IndexMap::new();
        for &pos in &bucket.positions {
            let ch = &playlist[pos.0].channels[pos.1];
            if let Some(epg) = ch.header.epg_channel_id.as_deref() {
                let trimmed = epg.trim().to_lowercase();
                if !trimmed.is_empty() {
                    epg_groups.entry(trimmed).or_default().push(pos);
                }
            }
        }

        if epg_groups.len() >= 2 {
            for (epg_id, sub_positions) in epg_groups {
                let sub_key = format!("{key}|{epg_id}");
                split_buckets
                    .insert(sub_key, AutoBucket { positions: sub_positions, has_dsl_forced: false, epg_split: true });
            }
        } else {
            split_buckets.insert(key, bucket);
        }
    }
    split_buckets
}

fn apply_auto_override(
    bucket: &mut AutoBucket,
    ao: &shared::model::AutoGroupOverrideDto,
    playlist: &[PlaylistGroup],
    member_pos_map: &HashMap<ChannelMemberRefDto, (usize, usize)>,
    taken_positions: &HashSet<(usize, usize)>,
    excluded_set: &HashSet<&ChannelMemberRefDto>,
) {
    if !ao.remove.is_empty() {
        bucket.positions.retain(|&pos| {
            let ch = &playlist[pos.0].channels[pos.1];
            !ao.remove.iter().any(|rem| {
                ch.header.input_name.as_ref() == rem.input.as_str()
                    && ch.header.get_input_stream_id().as_deref() == Some(rem.stream_id.as_str())
            })
        });
    }

    for add_ref in &ao.add {
        if let Some(&pos) = member_pos_map.get(add_ref) {
            if !taken_positions.contains(&pos) && !excluded_set.contains(add_ref) && !bucket.positions.contains(&pos) {
                bucket.positions.push(pos);
            }
        }
    }

    if !ao.order.is_empty() {
        let mut ordered = Vec::with_capacity(bucket.positions.len());
        let mut used = HashSet::new();

        for ord_ref in &ao.order {
            if let Some(&pos) = bucket.positions.iter().find(|&&pos| {
                let ch = &playlist[pos.0].channels[pos.1];
                ch.header.input_name.as_ref() == ord_ref.input.as_str()
                    && ch.header.get_input_stream_id().as_deref() == Some(ord_ref.stream_id.as_str())
            }) {
                if used.insert(pos) {
                    ordered.push(pos);
                }
            }
        }
        for &pos in &bucket.positions {
            if !used.contains(&pos) {
                ordered.push(pos);
            }
        }
        bucket.positions = ordered;
    }
}

fn build_group_metadata(
    candidate: &GroupCandidate,
    rep_header: &shared::model::PlaylistItemHeader,
    slots: &[GroupSlot],
    config: &ChannelGroupsConfig,
    group_uuid: UUIDType,
) -> shared::model::PlaylistItemHeader {
    let mut group_header = rep_header.clone();
    group_header.uuid = group_uuid;
    group_header.channel_group = None;
    group_header.channel_group_uuid = Some(group_uuid);

    if let Some(name) = &candidate.name_override {
        group_header.name = name.as_str().intern();
        group_header.title = name.as_str().intern();
    } else if config.name_strip_quality {
        group_header.name = strip_quality_words(&rep_header.name).intern();
        group_header.title = strip_quality_words(&rep_header.title).intern();
    }

    if let Some(logo) = &candidate.logo_override {
        group_header.logo = logo.as_str().intern();
    } else if group_header.logo.is_empty() {
        for &pos in &candidate.positions {
            if let Some(item) = slots.get(pos.0).and_then(|g| g.channels.get(pos.1)).and_then(Option::as_ref) {
                if !item.header.logo.is_empty() {
                    group_header.logo = item.header.logo.clone();
                    break;
                }
            }
        }
    }

    if let Some(epg) = &candidate.epg_override {
        group_header.epg_channel_id = Some(epg.as_str().intern());
    } else if group_header.epg_channel_id.as_deref().is_none_or(|s| s.trim().is_empty()) {
        for &pos in &candidate.positions {
            if let Some(item) = slots.get(pos.0).and_then(|g| g.channels.get(pos.1)).and_then(Option::as_ref) {
                if let Some(epg) = &item.header.epg_channel_id {
                    if !epg.trim().is_empty() {
                        group_header.epg_channel_id = Some(epg.clone());
                        break;
                    }
                }
            }
        }
    }

    apply_archive_metadata(&mut group_header, candidate, slots);
    group_header
}

fn apply_archive_metadata(
    group_header: &mut shared::model::PlaylistItemHeader,
    candidate: &GroupCandidate,
    slots: &[GroupSlot],
) {
    let mut has_archive = false;
    let mut max_archive_duration: Option<i32> = None;
    for &pos in &candidate.positions {
        if let Some(item) = slots.get(pos.0).and_then(|g| g.channels.get(pos.1)).and_then(Option::as_ref) {
            if let Some(StreamProperties::Live(live)) = &item.header.additional_properties {
                if live.tv_archive.unwrap_or(0) > 0 {
                    has_archive = true;
                    if let Some(dur) = live.tv_archive_duration {
                        max_archive_duration = Some(max_archive_duration.map_or(dur, |m| m.max(dur)));
                    }
                }
            }
        }
    }

    if has_archive {
        match &mut group_header.additional_properties {
            Some(StreamProperties::Live(live)) => {
                live.tv_archive = Some(1);
                live.tv_archive_duration = max_archive_duration;
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
                    is_adult: 0,
                    ..Default::default()
                })));
            }
        }
    }
}

fn collect_group_candidates(
    config: &ChannelGroupsConfig,
    overrides: Option<&ChannelGroupOverridesDto>,
    playlist: &[PlaylistGroup],
) -> Vec<GroupCandidate> {
    let member_pos_map = build_member_index(playlist);
    let mut taken_positions = HashSet::new();
    let mut candidates = Vec::new();

    resolve_manual_groups(config, overrides, &member_pos_map, &mut taken_positions, &mut candidates);

    let excluded_set: HashSet<&ChannelMemberRefDto> =
        overrides.map_or_else(HashSet::new, |o| o.excluded.iter().collect());

    let mut auto_buckets = collect_auto_buckets(config, playlist, &excluded_set, &taken_positions);

    if config.epg_conflict == EpgConflictMode::Split {
        auto_buckets = split_epg_buckets(playlist, auto_buckets);
    }

    for (key, mut bucket) in auto_buckets {
        let auto_override = overrides.and_then(|o| o.auto_overrides.iter().find(|ao| ao.auto_key == key));
        let origin = if auto_override.is_some() { ChannelGroupOrigin::AutoEdited } else { ChannelGroupOrigin::Auto };

        if let Some(ao) = auto_override {
            apply_auto_override(&mut bucket, ao, playlist, &member_pos_map, &taken_positions, &excluded_set);
        }

        if bucket.positions.len() < MIN_GROUP_MEMBERS {
            continue;
        }

        for &pos in &bucket.positions {
            taken_positions.insert(pos);
        }

        candidates.push(GroupCandidate {
            key: key.clone(),
            origin,
            policy: auto_override.and_then(|ao| ao.policy).unwrap_or(config.policy),
            positions: bucket.positions,
            orphaned: Vec::new(),
            epg_split: bucket.epg_split,
            name_override: auto_override.and_then(|ao| ao.name.clone()),
            logo_override: auto_override.and_then(|ao| ao.logo.clone()),
            epg_override: auto_override.and_then(|ao| ao.epg_channel_id.clone()),
            playlist_group_override: auto_override.and_then(|ao| ao.playlist_group.clone()),
        });
    }

    candidates
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

    for (pg_override, mut item) in deferred_placements {
        if let Some(g) = playlist.iter_mut().find(|g| g.title.as_ref() == pg_override.as_str()) {
            item.header.category_id = g.id;
            item.header.group = g.title.clone();
            g.channels.push(item);
        } else {
            let next_id = playlist.iter().map(|g| g.id).max().unwrap_or(0) + 1;
            item.header.category_id = next_id;
            item.header.group = pg_override.as_str().intern();
            playlist.push(PlaylistGroup {
                id: next_id,
                title: pg_override.as_str().intern(),
                channels: vec![item],
                xtream_cluster: XtreamCluster::Live,
            });
        }
    }
}

fn materialize_groups(
    candidates: &[GroupCandidate],
    config: &ChannelGroupsConfig,
    target_name: &str,
    mut slots: Vec<GroupSlot>,
    playlist: &mut Vec<PlaylistGroup>,
) -> Vec<PendingChannelGroup> {
    let mut deferred_placements = Vec::new();
    let mut pending_groups = Vec::with_capacity(candidates.len());

    for candidate in candidates {
        let rep_pos = candidate.positions[0];
        let Some(rep_item) = slots.get(rep_pos.0).and_then(|g| g.channels.get(rep_pos.1)).and_then(Option::as_ref)
        else {
            continue;
        };

        let group_uuid = hash_string(&format!("channel-group|{target_name}|{}", candidate.key));
        let group_header = build_group_metadata(candidate, &rep_item.header, &slots, config, group_uuid);

        let mut members = Vec::with_capacity(candidate.positions.len());
        for &pos in &candidate.positions {
            if let Some(item) = slots.get_mut(pos.0).and_then(|g| g.channels.get_mut(pos.1)).and_then(Option::take) {
                members.push(item);
            }
        }

        let group_item = PlaylistItem { header: group_header };

        if let Some(pg_override) = &candidate.playlist_group_override {
            deferred_placements.push((pg_override.clone(), group_item));
        } else if let Some(slot) = slots.get_mut(rep_pos.0).and_then(|g| g.channels.get_mut(rep_pos.1)) {
            *slot = Some(group_item);
        }

        pending_groups.push(PendingChannelGroup {
            group_uuid,
            key: candidate.key.as_str().intern(),
            origin: candidate.origin,
            policy: candidate.policy,
            members,
            orphaned: candidate.orphaned.clone(),
            epg_split: candidate.epg_split,
        });
    }

    rebuild_playlist_from_slots(slots, deferred_placements, playlist);
    pending_groups
}

pub(in crate::processor) fn group_playlist(
    config: &ChannelGroupsConfig,
    overrides: Option<&ChannelGroupOverridesDto>,
    target_name: &str,
    playlist: &mut Vec<PlaylistGroup>,
) -> Vec<PendingChannelGroup> {
    if playlist.is_empty() {
        return Vec::new();
    }

    let candidates = collect_group_candidates(config, overrides, playlist);
    if candidates.is_empty() {
        return Vec::new();
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

    materialize_groups(&candidates, config, target_name, slots, playlist)
}
