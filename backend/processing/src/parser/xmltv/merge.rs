use super::{
    ChannelMergeAcc, DiskEpgSource, EpgDiskChannelKey, EpgMergeAccumulator, PreferredAttributes, PreferredDummyPolicy,
    ProgrammeMergeEntry,
};
use log::error;
use quick_xml::events::BytesStart;
use shared::{
    model::{EpgChannel, EpgProgramme},
    utils::Internable,
};
use std::{
    collections::{HashMap, HashSet},
    io,
    sync::Arc,
};
use tuliprox_core::{
    model::{Epg, IcsDummyPolicy, EPG_TAG_CHANNEL, EPG_TAG_PROGRAMME, EPG_TAG_TV},
    utils::with_folded_epg_id,
};
use tuliprox_parser::ics;
use tuliprox_repository::BPlusTreeQuery;

/// Channels per `BPlusTree` write batch. Sized so the in-flight batch + its
/// prepared references stay under 1 MiB on the typical 100-KiB-per-channel
/// EPG feed. This is an educated guess; profile-driven tuning is fine.
pub(super) const EPG_DISK_BATCH_SIZE: usize = 100;

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub(super) enum XmlTagType {
    Ignored,
    Tv,
    Channel,
    Programme,
}

impl XmlTagType {
    #[inline]
    pub fn is_tv(self) -> bool { self == XmlTagType::Tv }
}

pub(super) fn get_tag_type(name: &str) -> XmlTagType {
    match name {
        EPG_TAG_TV => XmlTagType::Tv,
        EPG_TAG_CHANNEL => XmlTagType::Channel,
        EPG_TAG_PROGRAMME => XmlTagType::Programme,
        _ => XmlTagType::Ignored,
    }
}

pub(super) fn collect_tag_attributes(e: &BytesStart) -> HashMap<Arc<str>, Arc<str>> {
    let attributes = e
        .attributes()
        .filter_map(Result::ok)
        .filter_map(|a| {
            let key_binding = a.key;
            let key_raw = String::from_utf8_lossy(key_binding.as_ref());
            let key = key_raw.intern();
            if let Ok(value) = a.normalized_value(quick_xml::XmlVersion::Implicit1_0).as_ref() {
                if value.is_empty() {
                    None
                } else {
                    // Ids are no longer lowercased when parsed; EPG matching folds case
                    // at the comparison instead, so the guide's original-case <channel id>
                    // and <programme channel> are preserved in the output.
                    Some((key, value.intern()))
                }
            } else {
                None
            }
        })
        .collect::<HashMap<Arc<str>, Arc<str>>>();
    attributes
}

/// Carries the source rank required to select a preview dummy policy exactly like the main EPG merge.
#[derive(Debug)]
pub struct EpgDummyPolicySource {
    pub priority: i16,
    pub source_order: usize,
    pub channel_id: Arc<str>,
    pub policy: IcsDummyPolicy,
}

pub(super) type FinishedEpgChannels = (Option<HashMap<Arc<str>, Arc<str>>>, Vec<EpgChannel>);

pub type MergedEpgWithIconOverrides = (Epg, HashSet<Arc<str>>);

impl EpgMergeAccumulator {
    pub fn new() -> Self { Self::default() }

    pub(super) fn channel_ids_with_programmes(&self) -> HashSet<Arc<str>> {
        self.channels
            .iter()
            .filter(|(id, channel)| !channel.programmes.is_empty() || self.dummy_policies.contains_key(*id))
            .map(|(id, _)| Arc::clone(id))
            .collect()
    }

    pub(super) fn retain_channels(&mut self, selected_epg_ids: &HashSet<Arc<str>>) -> HashSet<Arc<str>> {
        self.channels.retain(|id, _| selected_epg_ids.contains(id));
        self.dummy_policies.retain(|id, _| selected_epg_ids.contains(id));
        self.channels.keys().cloned().collect()
    }

    pub fn set_attributes_if_preferred(
        &mut self,
        priority: i16,
        source_order: usize,
        attributes: Option<HashMap<Arc<str>, Arc<str>>>,
    ) {
        let Some(attributes) = attributes else {
            return;
        };
        let replace = self
            .attributes
            .as_ref()
            .is_none_or(|current| (priority, source_order) < (current.priority, current.source_order));
        if replace {
            self.attributes = Some(PreferredAttributes { priority, source_order, attributes });
        }
    }

    pub(super) fn upsert_channel(
        &mut self,
        priority: i16,
        source_order: usize,
        logo_override: bool,
        mut channel: EpgChannel,
    ) {
        let channel_key = with_folded_epg_id(&channel.id, |folded| folded.intern());
        match self.channels.entry(channel_key) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let acc = entry.get_mut();
                acc.needs_programme_merge = true;
                if (priority, source_order) < (acc.priority, acc.source_order) {
                    if channel.title.is_none() {
                        channel.title = acc.channel.title.take();
                    }
                    if channel.icon.is_none() {
                        channel.icon = acc.channel.icon.take();
                    } else {
                        acc.icon_logo_override = logo_override;
                    }
                    acc.priority = priority;
                    acc.source_order = source_order;
                    acc.logo_override = logo_override;
                    acc.channel.title = channel.title;
                    acc.channel.icon = channel.icon;
                } else {
                    if acc.channel.title.is_none() {
                        acc.channel.title = channel.title.take();
                    }
                    if acc.channel.icon.is_none() {
                        acc.channel.icon = channel.icon.take();
                        acc.icon_logo_override = logo_override;
                    }
                }
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                let icon_logo_override = channel.icon.is_some() && logo_override;
                entry.insert(ChannelMergeAcc {
                    priority,
                    source_order,
                    logo_override,
                    icon_logo_override,
                    needs_programme_merge: false,
                    channel,
                    programmes: Vec::new(),
                });
            }
        }
    }

    pub(super) fn push_programme(&mut self, priority: i16, source_order: usize, programme: EpgProgramme) {
        if let Some(channel) =
            with_folded_epg_id(programme.get_transient_channel_id(), |folded| self.channels.get_mut(folded))
        {
            channel.programmes.push(ProgrammeMergeEntry { priority, source_order, programme });
        } else {
            error!("Channel {} not found in EPG, dangling programme", programme.get_transient_channel_id());
        }
    }

    pub(super) fn register_dummy_policy(
        &mut self,
        channel_id: &Arc<str>,
        priority: i16,
        source_order: usize,
        policy: IcsDummyPolicy,
    ) {
        if !policy.config.enabled {
            return;
        }
        let key = with_folded_epg_id(channel_id, |folded| folded.intern());
        let replace = self
            .dummy_policies
            .get(&key)
            .is_none_or(|current| (priority, source_order) < (current.priority, current.source_order));
        if replace {
            self.dummy_policies.insert(key, PreferredDummyPolicy { priority, source_order, policy });
        }
    }

    pub fn add_channel_with_programmes(
        &mut self,
        priority: i16,
        source_order: usize,
        logo_override: bool,
        mut channel: EpgChannel,
    ) {
        let programmes = std::mem::take(&mut channel.programmes);
        let channel_key = with_folded_epg_id(&channel.id, |folded| folded.intern());
        self.upsert_channel(priority, source_order, logo_override, channel);
        if let Some(acc) = self.channels.get_mut(&channel_key) {
            if !acc.programmes.is_empty() {
                acc.needs_programme_merge = true;
            }
            acc.programmes.extend(programmes.into_iter().map(|programme| ProgrammeMergeEntry {
                priority,
                source_order,
                programme,
            }));
        }
    }

    pub(super) fn finish_channels(mut self) -> Option<FinishedEpgChannels> {
        if self.channels.is_empty() {
            return None;
        }

        let mut channels = self
            .channels
            .drain()
            .map(|(_, mut acc)| {
                normalize_channel_programmes(&mut acc);
                acc
            })
            .collect::<Vec<_>>();

        channels.sort_by(|left, right| left.channel.id.cmp(&right.channel.id));
        let channels = channels.into_iter().map(|acc| acc.channel).collect();
        Some((self.attributes.map(|attributes| attributes.attributes), channels))
    }

    pub(super) fn finish(self) -> Option<Epg> {
        self.finish_channels().map(|(attributes, channels)| Epg {
            logo_override: false,
            priority: 0,
            attributes,
            children: channels.into_iter().map(Arc::new).collect(),
        })
    }

    pub(super) fn finish_epg_with_icon_overrides(self) -> Option<MergedEpgWithIconOverrides> {
        let EpgMergeAccumulator { attributes, channels, dummy_policies } = self;
        let mut channels = channels.into_values().collect::<Vec<_>>();
        if channels.is_empty() {
            return None;
        }

        for acc in &mut channels {
            normalize_channel_programmes(acc);
        }

        apply_dummy_policies(&mut channels, &dummy_policies);

        channels.sort_by(|left, right| left.channel.id.cmp(&right.channel.id));
        let icon_override_channels = channels
            .iter()
            .filter(|acc| acc.icon_logo_override)
            .map(|acc| Arc::clone(&acc.channel.id))
            .collect::<HashSet<_>>();
        let children = channels.into_iter().map(|acc| Arc::new(acc.channel)).collect();

        Some((
            Epg {
                logo_override: false,
                priority: 0,
                attributes: attributes.map(|attributes| attributes.attributes),
                children,
            },
            icon_override_channels,
        ))
    }
}

fn apply_dummy_policies(channels: &mut [ChannelMergeAcc], dummy_policies: &HashMap<Arc<str>, PreferredDummyPolicy>) {
    let now = chrono::Utc::now();
    for acc in channels {
        let key = with_folded_epg_id(&acc.channel.id, |folded| folded.intern());
        if let Some(preferred) = dummy_policies.get(&key) {
            let policy = &preferred.policy;
            if let Err(err) = ics::fill_dummy_gaps(
                &mut acc.channel.programmes,
                &acc.channel.id,
                &policy.timezone,
                &policy.config,
                now,
            ) {
                log::warn!("Failed to apply ICS dummy policy for {}: {err}", acc.channel.id);
            }
        }
    }
}

fn backfill_programme_metadata(existing: &mut EpgProgramme, incoming: EpgProgramme) {
    if existing.title.is_none() {
        existing.title = incoming.title;
    }
    if existing.desc.is_none() {
        existing.desc = incoming.desc;
    }
    if existing.icon.is_none() {
        existing.icon = incoming.icon;
    }
    if existing.catchup_id.is_none() {
        existing.catchup_id = incoming.catchup_id;
    }
    if existing.categories.is_empty() {
        existing.categories = incoming.categories;
    }
    existing.is_live |= incoming.is_live;
    existing.is_new |= incoming.is_new;
}

pub(super) fn normalize_channel_programmes(acc: &mut ChannelMergeAcc) {
    // `acc.programmes` is the cross-source list of `ProgrammeMergeEntry` records
    // (populated by `add_channel_with_programmes` and `push_programme`).
    // When the list is empty, the channel's own programmes — set by the
    // vacant path of `upsert_channel` — are authoritative; the previous
    // implementation unconditionally overwrote `acc.channel.programmes` with
    // an empty vector here, which silently dropped every programme on the
    // single-source and disk-spill paths.
    if acc.programmes.is_empty() {
        return;
    }
    acc.programmes
        .sort_by_key(|entry| (entry.programme.start, entry.programme.stop, entry.priority, entry.source_order));

    let programme_entries = std::mem::take(&mut acc.programmes);
    let mut merged_programmes = Vec::with_capacity(programme_entries.len());
    let mut entries = programme_entries.into_iter();
    if let Some(first_entry) = entries.next() {
        let mut current = first_entry.programme;
        for entry in entries {
            if entry.programme.start == current.start && entry.programme.stop == current.stop {
                backfill_programme_metadata(&mut current, entry.programme);
            } else {
                merged_programmes.push(current);
                current = entry.programme;
            }
        }
        merged_programmes.push(current);
    }

    merged_programmes.sort_by_key(|programme| (programme.start, programme.stop));
    acc.channel.programmes = merged_programmes;
}

#[cfg(test)]
pub fn merge_epg_channels_by_priority(channels_by_source: Vec<(i16, Vec<EpgChannel>)>) -> Vec<EpgChannel> {
    let mut accumulator = EpgMergeAccumulator::new();
    for (source_order, (priority, channels)) in channels_by_source.into_iter().enumerate() {
        for channel in channels {
            accumulator.add_channel_with_programmes(priority, source_order, false, channel);
        }
    }
    accumulator.finish_channels().map(|(_, channels)| channels).unwrap_or_default()
}

pub fn merge_epg_channels_by_priority_with_dummy_policies(
    channels_by_source: Vec<(i16, Vec<EpgChannel>)>,
    dummy_policies: Vec<EpgDummyPolicySource>,
) -> Vec<EpgChannel> {
    let mut accumulator = EpgMergeAccumulator::new();
    for (source_order, (priority, channels)) in channels_by_source.into_iter().enumerate() {
        for channel in channels {
            accumulator.add_channel_with_programmes(priority, source_order, false, channel);
        }
    }
    for source in dummy_policies {
        accumulator.register_dummy_policy(&source.channel_id, source.priority, source.source_order, source.policy);
    }
    accumulator
        .finish_epg_with_icon_overrides()
        .map(|(epg, _)| epg.children.into_iter().map(Arc::unwrap_or_clone).collect())
        .unwrap_or_default()
}

pub fn flatten_tvguide(tv_guides: Vec<Epg>) -> Option<Epg> {
    let mut accumulator = EpgMergeAccumulator::new();
    for (source_order, guide) in tv_guides.into_iter().enumerate() {
        accumulator.set_attributes_if_preferred(guide.priority, source_order, guide.attributes);
        for channel in guide.children {
            accumulator.add_channel_with_programmes(
                guide.priority,
                source_order,
                guide.logo_override,
                Arc::unwrap_or_clone(channel),
            );
        }
    }
    accumulator.finish()
}

/// Merge N temp `BPlusTree`s (one per EPG source) into a single `Epg`,
/// applying dummy policies and the existing priority/sort rules. Sources
/// are consumed one at a time — each `DiskEpgSource`'s temp file is
/// removed by its `Drop` as soon as its iterator is exhausted, so peak RAM
/// is bounded by the largest single source plus the accumulator's
/// per-channel `HashMap`, not the total feed size.
///
/// Complexity: `O(n_sources + total_channels)`. The per-channel priority
/// resolution happens inside `EpgMergeAccumulator::upsert_channel`
/// (`HashMap` insert), not in a heap. Sources are processed in iteration
/// order, so earlier sources win priority ties for shared channels.
///
/// The constant-memory guarantee lives on the write side
/// (`EpgMergeAccumulator::finish_into_disk`); this function is bounded by
/// the union's metadata `HashMap` in the accumulator, which is the same
/// shape the in-memory path already carries.
///
/// The icon-override channel set returned by `finish_epg_with_icon_overrides`
/// is intentionally dropped — the wire-up path persists channels directly,
/// not the override metadata. If a caller later needs that set, the
/// `MergedEpgWithIconOverrides` tuple is already in scope via this
/// function's return type.
pub fn merge_epg_trees(sources: Vec<DiskEpgSource>) -> io::Result<Option<MergedEpgWithIconOverrides>> {
    let mut accumulator = EpgMergeAccumulator::new();
    for source in sources {
        if let Some(attrs) = &source.attributes {
            accumulator.set_attributes_if_preferred(
                source.source_priority,
                source.source_order as usize,
                Some(attrs.clone()),
            );
        }
        let mut query = BPlusTreeQuery::<EpgDiskChannelKey, EpgChannel>::try_new(&source.path)
            .map_err(|e| io::Error::other(format!("open temp EPG tree at {}: {e}", source.path.display())))?;
        for entry in query.iter() {
            let (key, channel) = entry.map_err(|e| io::Error::other(format!("read temp EPG entry: {e}")))?;
            // `add_channel_with_programmes` preserves the channel's programmes
            // on the merge accumulator; plain `upsert_channel` would drop them
            // because the vacant-entry path constructs a fresh `ChannelMergeAcc`
            // with `programmes: Vec::new()`.
            accumulator.add_channel_with_programmes(key.priority, key.source_order as usize, false, channel);
        }
        // `source` drops at the end of this iteration, which removes the
        // temp file. The query's mmap/buffer is closed first because `query`
        // goes out of scope before `source`.
    }
    Ok(accumulator.finish_epg_with_icon_overrides())
}
