use shared::model::{EpgChannel, EpgProgramme};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

#[derive(Clone, Hash, Eq, PartialEq)]
pub(crate) struct ProgrammeMergeKey {
    start: i64,
    stop: i64,
}

impl From<&EpgProgramme> for ProgrammeMergeKey {
    fn from(programme: &EpgProgramme) -> Self {
        Self { start: programme.start, stop: programme.stop }
    }
}

pub(crate) fn dedupe_channel_programmes(channel: &mut EpgChannel) -> HashSet<ProgrammeMergeKey> {
    let mut seen = HashMap::new();
    let mut deduped = Vec::with_capacity(channel.programmes.len());

    for mut programme in std::mem::take(&mut channel.programmes) {
        let key = ProgrammeMergeKey::from(&programme);
        if let Some(existing_idx) = seen.get(&key) {
            backfill_programme_metadata(&mut deduped[*existing_idx], &mut programme);
        } else {
            seen.insert(key.clone(), deduped.len());
            deduped.push(programme);
        }
    }

    channel.programmes = deduped;
    seen.into_keys().collect()
}

pub(crate) fn merge_missing_channel_programmes<I>(
    channel: &mut EpgChannel,
    programmes: &mut HashSet<ProgrammeMergeKey>,
    incoming: I,
) where
    I: IntoIterator<Item = EpgProgramme>,
{
    let mut programme_index = channel
        .programmes
        .iter()
        .enumerate()
        .map(|(idx, programme)| (ProgrammeMergeKey::from(programme), idx))
        .collect::<HashMap<_, _>>();

    for mut programme in incoming {
        let key = ProgrammeMergeKey::from(&programme);
        if programmes.insert(key.clone()) {
            programme_index.insert(key, channel.programmes.len());
            channel.programmes.push(programme);
        } else if let Some(existing_idx) = programme_index.get(&key) {
            backfill_programme_metadata(&mut channel.programmes[*existing_idx], &mut programme);
        }
    }
}

pub(crate) fn merge_prioritized_channels(mut channels_by_source: Vec<(i16, Vec<EpgChannel>)>) -> Vec<EpgChannel> {
    struct ChannelMergeAcc {
        priority: i16,
        channel: EpgChannel,
        programmes: HashSet<ProgrammeMergeKey>,
    }

    let mut merged: HashMap<Arc<str>, ChannelMergeAcc> = HashMap::new();
    channels_by_source.sort_by_key(|(priority, _)| *priority);

    for (priority, channels) in channels_by_source.drain(..) {
        for mut channel in channels {
            match merged.entry(channel.id.clone()) {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    let acc = entry.get_mut();
                    debug_assert!(priority >= acc.priority);
                    backfill_channel_metadata(&mut acc.channel, &mut channel);
                    merge_missing_channel_programmes(&mut acc.channel, &mut acc.programmes, channel.programmes);
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let programmes = dedupe_channel_programmes(&mut channel);
                    entry.insert(ChannelMergeAcc { priority, channel, programmes });
                }
            }
        }
    }

    for entry in merged.values_mut() {
        entry.channel.programmes.sort_by_key(|programme| programme.start);
    }

    let mut channels = merged.into_values().map(|entry| entry.channel).collect::<Vec<_>>();
    channels.sort_by(|left, right| left.id.cmp(&right.id));
    channels
}

fn backfill_programme_metadata(existing: &mut EpgProgramme, incoming: &mut EpgProgramme) {
    if existing.title.is_none() {
        existing.title = incoming.title.take();
    }
    if existing.desc.is_none() {
        existing.desc = incoming.desc.take();
    }
}

fn backfill_channel_metadata(existing: &mut EpgChannel, incoming: &mut EpgChannel) {
    if existing.title.is_none() {
        existing.title = incoming.title.take();
    }
    if existing.icon.is_none() {
        existing.icon = incoming.icon.take();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        dedupe_channel_programmes, merge_missing_channel_programmes, merge_prioritized_channels, ProgrammeMergeKey,
    };
    use shared::model::{EpgChannel, EpgProgramme};
    use shared::utils::Internable;
    use std::collections::HashSet;

    #[test]
    fn dedupe_channel_programmes_backfills_missing_metadata_from_duplicates() {
        let mut channel = EpgChannel {
            id: "demo.channel".intern(),
            title: None,
            icon: None,
            programmes: vec![
                EpgProgramme::new_all(10, 20, "demo.channel".intern(), None, None),
                EpgProgramme::new_all(
                    10,
                    20,
                    "demo.channel".intern(),
                    Some("Recovered title".intern()),
                    Some("Recovered desc".intern()),
                ),
            ],
        };

        let keys = dedupe_channel_programmes(&mut channel);

        assert_eq!(keys.len(), 1);
        assert_eq!(channel.programmes.len(), 1);
        assert_eq!(channel.programmes[0].title.as_deref(), Some("Recovered title"));
        assert_eq!(channel.programmes[0].desc.as_deref(), Some("Recovered desc"));
    }

    #[test]
    fn merge_missing_channel_programmes_backfills_missing_metadata_from_duplicates() {
        let mut channel = EpgChannel {
            id: "demo.channel".intern(),
            title: None,
            icon: None,
            programmes: vec![EpgProgramme::new_all(10, 20, "demo.channel".intern(), None, None)],
        };
        let mut programmes = HashSet::from([ProgrammeMergeKey::from(&channel.programmes[0])]);

        merge_missing_channel_programmes(
            &mut channel,
            &mut programmes,
            vec![EpgProgramme::new_all(
                10,
                20,
                "demo.channel".intern(),
                Some("Recovered title".intern()),
                Some("Recovered desc".intern()),
            )],
        );

        assert_eq!(channel.programmes.len(), 1);
        assert_eq!(channel.programmes[0].title.as_deref(), Some("Recovered title"));
        assert_eq!(channel.programmes[0].desc.as_deref(), Some("Recovered desc"));
    }

    #[test]
    fn merge_prioritized_channels_prefers_higher_priority_metadata_and_merges_all_programmes() {
        let low_priority = EpgChannel {
            id: "demo.channel".intern(),
            title: Some("Low".intern()),
            icon: Some("http://low/icon.png".intern()),
            programmes: vec![EpgProgramme::new_all(10, 20, "demo.channel".intern(), Some("Low Show".intern()), None)],
        };
        let high_priority = EpgChannel {
            id: "demo.channel".intern(),
            title: Some("High".intern()),
            icon: Some("http://high/icon.png".intern()),
            programmes: vec![EpgProgramme::new_all(30, 40, "demo.channel".intern(), Some("High Show".intern()), None)],
        };
        let same_priority = EpgChannel {
            id: "demo.channel".intern(),
            title: Some("Same".intern()),
            icon: Some("http://same/icon.png".intern()),
            programmes: vec![
                EpgProgramme::new_all(30, 40, "demo.channel".intern(), Some("Duplicate".intern()), None),
                EpgProgramme::new_all(50, 60, "demo.channel".intern(), Some("Second Show".intern()), None),
            ],
        };

        let channels = merge_prioritized_channels(vec![
            (10, vec![low_priority]),
            (0, vec![high_priority]),
            (0, vec![same_priority]),
        ]);

        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].title.as_deref(), Some("High"));
        assert_eq!(channels[0].icon.as_deref(), Some("http://high/icon.png"));
        assert_eq!(channels[0].programmes.len(), 3);
        assert_eq!(
            channels[0].programmes.iter().map(|programme| (programme.start, programme.stop)).collect::<Vec<_>>(),
            vec![(10, 20), (30, 40), (50, 60)],
        );
    }

    #[test]
    fn merge_prioritized_channels_backfills_missing_channel_metadata() {
        let high_priority = EpgChannel {
            id: "demo.channel".intern(),
            title: None,
            icon: None,
            programmes: vec![EpgProgramme::new_all(30, 40, "demo.channel".intern(), Some("High Show".intern()), None)],
        };
        let low_priority = EpgChannel {
            id: "demo.channel".intern(),
            title: Some("Recovered Title".intern()),
            icon: Some("http://recovered/icon.png".intern()),
            programmes: vec![EpgProgramme::new_all(50, 60, "demo.channel".intern(), Some("Low Show".intern()), None)],
        };

        let channels = merge_prioritized_channels(vec![(0, vec![high_priority]), (10, vec![low_priority])]);

        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].title.as_deref(), Some("Recovered Title"));
        assert_eq!(channels[0].icon.as_deref(), Some("http://recovered/icon.png"));
        assert_eq!(
            channels[0].programmes.iter().map(|programme| (programme.start, programme.stop)).collect::<Vec<_>>(),
            vec![(30, 40), (50, 60)],
        );
    }
}
