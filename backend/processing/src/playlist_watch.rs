use log::error;
use shared::model::{
    EventMessage, EventSink, PlaylistGroup, PlaylistGroupsChanged, WatchChanges, WatchDisabled, WatchDisabledReason,
};
use std::{collections::BTreeSet, path::Path, sync::Arc};
use tuliprox_core::{
    model::AppConfig,
    utils,
    utils::{binary_deserialize, binary_serialize, file_exists_async},
};

const WATCH_NOTIFICATION_LIST_LIMIT: usize = 120;
const WATCH_NOTIFICATION_SUMMARY_THRESHOLD: usize = 500;

/// Track which *groups* a target has, as opposed to which channels a group has.
///
/// `process_group_watch` below is blind to the group set in both directions.
/// A group appearing is silent - it finds no baseline, writes one and emits
/// nothing, so the whole channel list reads as "not new" from then on. A
/// group vanishing is never even looked at: it is absent from the refreshed
/// playlist, so nothing iterates it.
///
/// The index lives beside the per-group directory rather than inside it
/// (`<target>.groups.bin`, not `<target>/__groups.bin`) so it cannot collide
/// with a group whose sanitized title happens to match.
pub async fn process_target_groups_watch<E: EventSink>(
    app_config: &Arc<AppConfig>,
    events: &E,
    target_name: &str,
    playlist: &[PlaylistGroup],
) {
    let new_tree: BTreeSet<Arc<str>> = playlist.iter().map(|group| group.title.clone()).collect();

    let index_filename = format!("{}.groups.bin", utils::sanitize_filename(target_name));
    let cfg = app_config.config.load();
    let Some(path) = utils::get_file_path(&cfg.storage_dir, Some(std::path::PathBuf::from(&index_filename))) else {
        error!("failed to resolve a path for group index {index_filename}");
        emit_watch_disabled(events, target_name, "", format!("could not resolve a path for {index_filename}"));
        return;
    };

    // First sight writes the baseline and says nothing. Announcing every
    // group as new on the first refresh after an upgrade would be noise, not
    // news.
    let mut changed = true;
    if file_exists_async(&path).await {
        if let Some(loaded_tree) = load_watch_tree(&path).await {
            let added: BTreeSet<Arc<str>> = new_tree.difference(&loaded_tree).cloned().collect();
            let removed: BTreeSet<Arc<str>> = loaded_tree.difference(&new_tree).cloned().collect();
            if added.is_empty() && removed.is_empty() {
                changed = false;
            } else {
                emit_groups_changed(events, target_name, &added, &removed);
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
}

/// Same sampling discipline as the per-group watch: titles only in the lists,
/// counts that stay true whatever the lists carry.
fn emit_groups_changed<E: EventSink>(
    events: &E,
    target_name: &str,
    added: &BTreeSet<Arc<str>>,
    removed: &BTreeSet<Arc<str>>,
) {
    let added_total = added.len();
    let removed_total = removed.len();
    let mut added: Vec<String> = added.iter().map(std::string::ToString::to_string).collect();
    let mut removed: Vec<String> = removed.iter().map(std::string::ToString::to_string).collect();
    let mut truncated = false;
    for list in [&mut added, &mut removed] {
        if list.len() > WATCH_NOTIFICATION_LIST_LIMIT {
            list.truncate(WATCH_NOTIFICATION_LIST_LIMIT);
            truncated = true;
        }
    }

    events.emit(EventMessage::PlaylistGroupsChanged(PlaylistGroupsChanged {
        target: target_name.to_string(),
        added,
        removed,
        added_total,
        removed_total,
        truncated,
    }));
}

pub async fn process_group_watch<E: EventSink>(
    app_config: &Arc<AppConfig>,
    events: &E,
    target_name: &str,
    pl: &PlaylistGroup,
) {
    let mut new_tree = BTreeSet::new();
    pl.channels.iter().for_each(|chan| {
        let header = &chan.header;
        let title = if header.title.is_empty() { header.name.clone() } else { header.title.clone() };
        new_tree.insert(title);
    });

    let watch_filename =
        format!("{}/{}.bin", utils::sanitize_filename(target_name), utils::sanitize_filename(&pl.title));
    let cfg = app_config.config.load();
    match utils::get_file_path(&cfg.storage_dir, Some(std::path::PathBuf::from(&watch_filename))) {
        Some(path) => {
            let save_path = path.as_path();
            let mut changed = false;
            if file_exists_async(&path).await {
                if let Some(loaded_tree) = load_watch_tree(&path).await {
                    // Find elements in set2 but not in set1
                    let added_difference: BTreeSet<Arc<str>> = new_tree.difference(&loaded_tree).cloned().collect();
                    let removed_difference: BTreeSet<Arc<str>> = loaded_tree.difference(&new_tree).cloned().collect();
                    if !added_difference.is_empty() || !removed_difference.is_empty() {
                        changed = true;
                        handle_watch_notification(
                            events,
                            &added_difference,
                            &removed_difference,
                            target_name,
                            &pl.title,
                        );
                    }
                } else {
                    error!("failed to load watch_file {}", path.to_str().unwrap_or_default());
                    // The baseline is unreadable, so this refresh re-baselines
                    // and the diff it should have produced is gone. Silently
                    // doing that leaves the operator believing the group is
                    // being watched.
                    emit_watch_disabled(
                        events,
                        target_name,
                        &pl.title,
                        format!("could not read watch state at {}", path.to_str().unwrap_or_default()),
                    );
                    changed = true;
                }
            } else {
                changed = true;
            }
            if changed {
                match save_watch_tree(save_path, &new_tree).await {
                    Ok(()) => {}
                    Err(err) => {
                        error!("failed to write watch_file {}: {err}", save_path.to_str().unwrap_or_default());
                        emit_watch_disabled(
                            events,
                            target_name,
                            &pl.title,
                            format!("could not write watch state at {}: {err}", save_path.to_str().unwrap_or_default()),
                        );
                    }
                }
            }
        }
        None => {
            error!("failed to write watch_file {watch_filename}");
            emit_watch_disabled(
                events,
                target_name,
                &pl.title,
                format!("could not resolve a storage path for {watch_filename}"),
            );
        }
    }
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

/// Turn a group's membership delta into an event.
///
/// Large changes are sampled rather than listed in full, but the sampling is
/// reported in [`WatchChanges::added_total`] / [`WatchChanges::removed_total`]
/// and [`WatchChanges::truncated`] rather than written into the lists as prose.
fn handle_watch_notification<E: EventSink>(
    events: &E,
    added: &BTreeSet<Arc<str>>,
    removed: &BTreeSet<Arc<str>>,
    target_name: &str,
    group_name: &str,
) {
    let added_total = added.len();
    let removed_total = removed.len();
    let total_changed = added_total.saturating_add(removed_total);
    if total_changed == 0 {
        return;
    }

    let mut added: Vec<String> = added.iter().map(std::string::ToString::to_string).collect();
    let mut removed: Vec<String> = removed.iter().map(std::string::ToString::to_string).collect();
    let mut truncated = false;

    if total_changed > WATCH_NOTIFICATION_SUMMARY_THRESHOLD {
        // A whole-provider reshuffle. Nobody reads ten thousand titles, and
        // every channel routing this pays for the bytes, so send the counts
        // alone.
        added.clear();
        removed.clear();
        truncated = true;
    } else {
        for list in [&mut added, &mut removed] {
            if list.len() > WATCH_NOTIFICATION_LIST_LIMIT {
                list.truncate(WATCH_NOTIFICATION_LIST_LIMIT);
                truncated = true;
            }
        }
    }

    events.emit(EventMessage::PlaylistWatchChanged(WatchChanges {
        target: target_name.to_string(),
        group: group_name.to_string(),
        added,
        removed,
        added_total,
        removed_total,
        truncated,
    }));
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
    use super::{handle_watch_notification, WATCH_NOTIFICATION_LIST_LIMIT, WATCH_NOTIFICATION_SUMMARY_THRESHOLD};
    use shared::model::{EventMessage, EventSink, WatchChanges};
    use std::{
        collections::BTreeSet,
        sync::{Arc, Mutex},
    };

    #[derive(Default)]
    struct CollectSink(Mutex<Vec<WatchChanges>>);

    impl EventSink for CollectSink {
        fn emit(&self, event: EventMessage) {
            if let EventMessage::PlaylistWatchChanged(changes) = event {
                self.0.lock().unwrap().push(changes);
            }
        }
    }

    fn titles(count: usize) -> BTreeSet<Arc<str>> {
        // Zero-padded so the `BTreeSet` order matches the numeric order, which
        // keeps the truncation assertions readable.
        (0..count).map(|i| Arc::from(format!("Channel {i:05}").as_str())).collect()
    }

    fn emit(added: usize, removed: usize) -> WatchChanges {
        let sink = CollectSink::default();
        handle_watch_notification(&sink, &titles(added), &titles(removed), "target", "group");
        let mut captured = sink.0.lock().unwrap();
        assert_eq!(captured.len(), 1, "expected exactly one event");
        captured.pop().unwrap()
    }

    #[test]
    fn an_unchanged_group_emits_nothing() {
        let sink = CollectSink::default();
        handle_watch_notification(&sink, &BTreeSet::new(), &BTreeSet::new(), "target", "group");
        assert!(sink.0.lock().unwrap().is_empty());
    }

    #[test]
    fn a_small_change_lists_every_title() {
        let changes = emit(3, 2);
        assert_eq!(changes.added.len(), 3);
        assert_eq!(changes.removed.len(), 2);
        assert_eq!(changes.added_total, 3);
        assert_eq!(changes.removed_total, 2);
        assert!(!changes.truncated);
    }

    /// The regression this whole field set exists for: the lists used to carry
    /// a synthesised "... N more entries omitted" string beside real channel
    /// titles, which a plugin reading the JSON payload could not distinguish
    /// from a channel actually named that.
    #[test]
    fn a_truncated_list_carries_only_channel_titles() {
        let added = WATCH_NOTIFICATION_LIST_LIMIT + 40;
        let changes = emit(added, 0);

        assert_eq!(changes.added.len(), WATCH_NOTIFICATION_LIST_LIMIT);
        assert_eq!(changes.added_total, added, "the total must survive the truncation");
        assert!(changes.truncated);
        for title in &changes.added {
            assert!(title.starts_with("Channel "), "list carries prose, not a channel title: {title}");
        }
    }

    #[test]
    fn a_very_large_change_reports_counts_only() {
        let added = WATCH_NOTIFICATION_SUMMARY_THRESHOLD + 100;
        let changes = emit(added, 0);

        assert!(changes.added.is_empty(), "a change this size should not list titles at all");
        assert_eq!(changes.added_total, added);
        assert!(changes.truncated);
    }
}
