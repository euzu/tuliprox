use log::error;
use shared::model::{
    EventMessage, EventSink, PlaylistGroup, WatchChange, WatchChanges, WatchContext, WatchDisabled,
    WatchDisabledReason, WatchGroupItems, WatchItems,
};
use std::{collections::BTreeSet, path::Path, sync::Arc};
use tuliprox_core::{
    model::ConfigWatch,
    utils,
    utils::{binary_deserialize, binary_serialize, file_exists_async},
};

const WATCH_NOTIFICATION_LIST_LIMIT: usize = 120;
const WATCH_NOTIFICATION_SUMMARY_THRESHOLD: usize = 500;

/// Groups that appeared or vanished, kept only where a `group_added` /
/// `group_removed` entry matches.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GroupChanges {
    pub added: Option<WatchItems>,
    pub removed: Option<WatchItems>,
}

/// One watched group's channel delta. An unchanged group yields no diff at
/// all, so at least one side is always set.
#[derive(Debug, PartialEq, Eq)]
pub struct GroupChannelDiff {
    pub group: Arc<str>,
    pub added: Option<WatchItems>,
    pub removed: Option<WatchItems>,
}

/// Sample sorted titles: the list is capped, the total is not.
///
/// Takes an iterator so a set difference is sampled as it is walked, without
/// collecting it first.
fn sample<'a>(titles: impl Iterator<Item = &'a Arc<str>>) -> WatchItems {
    let mut items = Vec::new();
    let mut total = 0usize;
    for title in titles {
        if items.len() < WATCH_NOTIFICATION_LIST_LIMIT {
            items.push(Arc::clone(title));
        }
        total += 1;
    }
    WatchItems { truncated: items.len() < total, items, total }
}

fn sample_non_empty<'a>(titles: impl Iterator<Item = &'a Arc<str>>) -> Option<WatchItems> {
    let items = sample(titles);
    (items.total > 0).then_some(items)
}

/// Diff the target's group titles against the persisted index.
///
/// The index holds *every* group, whatever the watch entries say, so editing
/// a pattern never reads as a change and a newly added `group_added` entry
/// already has a baseline. It lives beside the per-group directory
/// (`<target>.groups.bin`, not `<target>/__groups.bin`) so it cannot collide
/// with a group whose sanitized title matches.
///
/// First sight writes the baseline and reports nothing: announcing every
/// group as new on the first refresh would be noise, not news. An unreadable
/// index is reported and replaced - a corrupt file never becomes readable
/// again, and keeping it would switch group tracking off for good.
pub async fn diff_target_groups<E: EventSink>(
    storage_dir: &str,
    events: &E,
    target_name: &str,
    playlist: &[PlaylistGroup],
    watches: &[ConfigWatch],
) -> GroupChanges {
    let new_tree: BTreeSet<Arc<str>> = playlist.iter().map(|group| Arc::clone(&group.title)).collect();

    let index_filename = format!("{}.groups.bin", utils::sanitize_filename(target_name));
    let Some(path) = utils::get_file_path(storage_dir, Some(std::path::PathBuf::from(&index_filename))) else {
        error!("failed to resolve a path for group index {index_filename}");
        emit_watch_disabled(events, target_name, "", format!("could not resolve a path for {index_filename}"));
        return GroupChanges::default();
    };

    let mut reported = GroupChanges::default();
    let mut changed = true;
    if file_exists_async(&path).await {
        if let Some(loaded_tree) = load_watch_tree(&path).await {
            changed = new_tree != loaded_tree;
            if changed {
                reported = group_changes(&new_tree, &loaded_tree, watches);
            }
        } else {
            error!("failed to load group index {}", path.to_str().unwrap_or_default());
            emit_watch_disabled(
                events,
                target_name,
                "",
                format!("could not read the group index at {}", path.to_str().unwrap_or_default()),
            );
        }
    }

    if changed {
        if let Err(err) = save_watch_tree(path.as_path(), &new_tree).await {
            error!("failed to write group index {}: {err}", path.to_str().unwrap_or_default());
            emit_watch_disabled(
                events,
                target_name,
                "",
                format!("could not write the group index at {}: {err}", path.to_str().unwrap_or_default()),
            );
        }
    }
    reported
}

/// The groups in `current` but not `previous` that a `group_added` entry
/// matches, and the reverse for `group_removed`.
///
/// The set differences are filtered and sampled while they are walked, so no
/// intermediate set is built, and a direction no entry asks for is never
/// walked at all.
pub fn group_changes(
    current: &BTreeSet<Arc<str>>,
    previous: &BTreeSet<Arc<str>>,
    watches: &[ConfigWatch],
) -> GroupChanges {
    let kept = |from: &BTreeSet<Arc<str>>, without: &BTreeSet<Arc<str>>, context: WatchContext| {
        if !watches.iter().any(|watch| watch.contexts.contains(context)) {
            return None;
        }
        sample_non_empty(from.difference(without).filter(|title| {
            watches.iter().any(|watch| watch.contexts.contains(context) && watch.pattern.is_match(title))
        }))
    };
    GroupChanges {
        added: kept(current, previous, WatchContext::GroupAdded),
        removed: kept(previous, current, WatchContext::GroupRemoved),
    }
}

/// Turn one refresh's group and channel changes into a single event payload.
///
/// Order: group additions, group removals, then channel changes by group
/// title (added before removed), so notifications and tests do not depend on
/// which concurrent group diff finished first. Above the summary threshold,
/// summed over every group, channel lists are dropped and only counts remain:
/// nobody reads thousands of titles, and channels such as Telegram reject a
/// body that large outright.
pub fn assemble_watch_changes(
    target_name: &str,
    groups: GroupChanges,
    mut channel_diffs: Vec<GroupChannelDiff>,
) -> Option<WatchChanges> {
    channel_diffs.sort_unstable_by(|a, b| a.group.cmp(&b.group));
    let channel_total: usize = channel_diffs
        .iter()
        .flat_map(|diff| diff.added.iter().chain(diff.removed.iter()))
        .map(|items| items.total)
        .sum();
    let counts_only = channel_total > WATCH_NOTIFICATION_SUMMARY_THRESHOLD;

    let mut changes = Vec::with_capacity(2 + channel_diffs.len() * 2);
    changes.extend(groups.added.map(WatchChange::GroupsAdded));
    changes.extend(groups.removed.map(WatchChange::GroupsRemoved));
    let listed = |mut items: WatchItems| {
        if counts_only {
            items.items = Vec::new();
            items.truncated = true;
        }
        items
    };
    for GroupChannelDiff { group, added, removed } in channel_diffs {
        if let Some(items) = added {
            changes
                .push(WatchChange::ChannelsAdded(WatchGroupItems { group: Arc::clone(&group), items: listed(items) }));
        }
        if let Some(items) = removed {
            changes.push(WatchChange::ChannelsRemoved(WatchGroupItems { group, items: listed(items) }));
        }
    }

    (!changes.is_empty()).then(|| WatchChanges::new(target_name.to_string(), changes))
}

/// Diff one watched group's channels against its baseline.
pub async fn process_group_watch<E: EventSink>(
    storage_dir: &str,
    events: &E,
    target_name: &str,
    pl: &PlaylistGroup,
) -> Option<GroupChannelDiff> {
    let new_tree: BTreeSet<Arc<str>> = pl
        .channels
        .iter()
        .map(|chan| {
            let header = &chan.header;
            Arc::clone(if header.title.is_empty() { &header.name } else { &header.title })
        })
        .collect();

    let watch_filename =
        format!("{}/{}.bin", utils::sanitize_filename(target_name), utils::sanitize_filename(&pl.title));
    let Some(path) = utils::get_file_path(storage_dir, Some(std::path::PathBuf::from(&watch_filename))) else {
        error!("failed to write watch_file {watch_filename}");
        emit_watch_disabled(
            events,
            target_name,
            &pl.title,
            format!("could not resolve a storage path for {watch_filename}"),
        );
        return None;
    };

    let mut diff = None;
    let mut changed = true;
    if file_exists_async(&path).await {
        if let Some(loaded_tree) = load_watch_tree(&path).await {
            let added = sample_non_empty(new_tree.difference(&loaded_tree));
            let removed = sample_non_empty(loaded_tree.difference(&new_tree));
            changed = added.is_some() || removed.is_some();
            if changed {
                diff = Some(GroupChannelDiff { group: Arc::clone(&pl.title), added, removed });
            }
        } else {
            error!("failed to load watch_file {}", path.to_str().unwrap_or_default());
            // The baseline is unreadable, so this refresh re-baselines and the
            // diff it should have produced is gone. Silently doing that leaves
            // the operator believing the group is being watched.
            emit_watch_disabled(
                events,
                target_name,
                &pl.title,
                format!("could not read watch state at {}", path.to_str().unwrap_or_default()),
            );
        }
    }

    if changed {
        if let Err(err) = save_watch_tree(path.as_path(), &new_tree).await {
            error!("failed to write watch_file {}: {err}", path.to_str().unwrap_or_default());
            emit_watch_disabled(
                events,
                target_name,
                &pl.title,
                format!("could not write watch state at {}: {err}", path.to_str().unwrap_or_default()),
            );
        }
    }
    diff
}

/// Report that a group's watch state could not be read or written.
///
/// Each of these paths used to log once and carry on, which leaves the group
/// either silently re-baselining - losing the change it should have reported
/// - or not tracked at all.
fn emit_watch_disabled<E: EventSink>(events: &E, target_name: &str, group_name: &str, detail: String) {
    let mut disabled =
        WatchDisabled::new(target_name.to_string(), WatchDisabledReason::StorageFailure).with_detail(detail);
    // The target-level group index has no group of its own; leaving the field
    // unset is more honest than naming an empty one.
    if !group_name.is_empty() {
        disabled = disabled.with_group(group_name.to_string());
    }
    events.emit(EventMessage::PlaylistWatchDisabled(disabled));
}

async fn load_watch_tree(path: &Path) -> Option<BTreeSet<Arc<str>>> {
    let encoded = tokio::fs::read(path).await.ok()?;
    binary_deserialize(&encoded[..]).ok()
}

async fn save_watch_tree(path: &Path, tree: &BTreeSet<Arc<str>>) -> std::io::Result<()> {
    // Ensure the parent directory exists unconditionally
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let encoded: Vec<u8> = binary_serialize(&tree)?;
    tokio::fs::write(path, encoded).await
}

#[cfg(test)]
mod tests {
    use super::{
        assemble_watch_changes, group_changes, sample, GroupChanges, GroupChannelDiff, WATCH_NOTIFICATION_LIST_LIMIT,
    };
    use shared::model::{ConfigWatchDto, WatchChange, WatchContext, WatchItems};
    use std::{collections::BTreeSet, sync::Arc};
    use tuliprox_core::model::ConfigWatch;

    fn set(names: &[&str]) -> BTreeSet<Arc<str>> { names.iter().map(|n| Arc::from(*n)).collect() }

    fn titles(count: usize) -> BTreeSet<Arc<str>> {
        // Zero-padded so the `BTreeSet` order matches the numeric order.
        (0..count).map(|i| Arc::from(format!("Channel {i:05}").as_str())).collect()
    }

    fn watch(pattern: &str, contexts: &[WatchContext]) -> ConfigWatch {
        ConfigWatch {
            pattern: Arc::new(regex::Regex::new(pattern).expect("regex")),
            contexts: ConfigWatchDto::new(pattern, contexts.iter().copied()).on,
        }
    }

    fn diff(group: &str, added: usize, removed: usize) -> GroupChannelDiff {
        GroupChannelDiff {
            group: Arc::from(group),
            added: (added > 0).then(|| sample(titles(added).iter())),
            removed: (removed > 0).then(|| sample(titles(removed).iter())),
        }
    }

    #[test]
    fn a_small_list_is_complete() {
        let items = sample(titles(3).iter());
        assert_eq!((items.items.len(), items.total, items.truncated), (3, 3, false));
    }

    /// The lists used to carry a synthesised "... N more entries omitted"
    /// string beside real channel titles, which a plugin reading the JSON
    /// payload could not tell from a channel actually named that.
    #[test]
    fn a_long_list_keeps_titles_only_and_the_true_total() {
        let items = sample(titles(WATCH_NOTIFICATION_LIST_LIMIT + 40).iter());
        assert_eq!(items.items.len(), WATCH_NOTIFICATION_LIST_LIMIT);
        assert_eq!(items.total, WATCH_NOTIFICATION_LIST_LIMIT + 40);
        assert!(items.truncated);
        assert!(items.items.iter().all(|t| t.starts_with("Channel ")));
    }

    #[test]
    fn group_changes_are_filtered_per_context() {
        let previous = set(&["DE Kids", "Event B", "Stays"]);
        let current = set(&["Event A", "News 2", "Stays"]);
        let watches = [watch("^Event", &[WatchContext::GroupAdded]), watch(".*", &[WatchContext::GroupRemoved])];
        let changes = group_changes(&current, &previous, &watches);
        assert_eq!(changes.added.map(|i| i.items), Some(vec![Arc::from("Event A")]));
        assert_eq!(changes.removed.map(|i| i.items), Some(vec![Arc::from("DE Kids"), Arc::from("Event B")]));
    }

    #[test]
    fn channel_only_watches_report_no_group_changes() {
        let changes = group_changes(&set(&["X"]), &set(&["Y"]), &[watch(".*", &[WatchContext::Channels])]);
        assert_eq!(changes, GroupChanges::default());
    }

    #[test]
    fn nothing_changed_assembles_nothing() {
        assert!(assemble_watch_changes("t", GroupChanges::default(), Vec::new()).is_none());
    }

    #[test]
    fn changes_are_ordered_groups_first_then_channels_by_group() {
        let groups =
            GroupChanges { added: Some(sample(set(&["New"]).iter())), removed: Some(sample(set(&["Old"]).iter())) };
        let changes = assemble_watch_changes("t", groups, vec![diff("B", 1, 1), diff("A", 1, 0)]).expect("changes");
        let kinds: Vec<String> = changes
            .changes
            .iter()
            .map(|c| match c {
                WatchChange::GroupsAdded(_) => "ga".to_string(),
                WatchChange::GroupsRemoved(_) => "gr".to_string(),
                WatchChange::ChannelsAdded(g) => format!("ca:{}", g.group),
                WatchChange::ChannelsRemoved(g) => format!("cr:{}", g.group),
            })
            .collect();
        assert_eq!(kinds, ["ga", "gr", "ca:A", "ca:B", "cr:B"]);
    }

    #[test]
    fn a_large_aggregate_reports_counts_only_for_channels() {
        // 20 groups x 30 changes = 600 > WATCH_NOTIFICATION_SUMMARY_THRESHOLD.
        let diffs = (0..20).map(|i| diff(&format!("G{i:02}"), 30, 0)).collect();
        let groups = GroupChanges { added: Some(sample(set(&["New"]).iter())), removed: None };
        let changes = assemble_watch_changes("t", groups, diffs).expect("changes");
        for change in &changes.changes {
            match change {
                WatchChange::ChannelsAdded(g) => {
                    assert!(g.items.items.is_empty() && g.items.truncated && g.items.total == 30);
                }
                WatchChange::GroupsAdded(items) => assert_eq!(items, &WatchItems::complete(vec![Arc::from("New")])),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn a_small_aggregate_keeps_channel_lists() {
        let changes = assemble_watch_changes("t", GroupChanges::default(), vec![diff("G", 3, 2)]).expect("changes");
        assert!(changes.changes.iter().all(|c| match c {
            WatchChange::ChannelsAdded(g) | WatchChange::ChannelsRemoved(g) => !g.items.truncated,
            _ => false,
        }));
    }
}
