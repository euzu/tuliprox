// `use super::*` brings `process_watch` into scope the same way `curation_effect_gate.rs` gets it.
use super::{curation_effect_gate::app_config, support::PlaylistRunCollectSink, *};
use shared::model::{ConfigTargetDto, ConfigWatchDto, WatchChange, WatchContext};
use tempfile::tempdir;

fn group(title: &str, channels: &[&str]) -> PlaylistGroup {
    PlaylistGroup {
        id: 1,
        title: title.intern(),
        xtream_cluster: XtreamCluster::Live,
        channels: channels
            .iter()
            .map(|name| PlaylistItem {
                header: PlaylistItemHeader { name: name.intern(), title: name.intern(), ..Default::default() },
            })
            .collect(),
    }
}

fn target(watch: Vec<ConfigWatchDto>) -> ConfigTarget {
    ConfigTarget::from(&ConfigTargetDto { name: "sports".to_string(), watch: Some(watch), ..Default::default() })
}

fn watch_events(sink: &PlaylistRunCollectSink) -> Vec<EventMessage> {
    sink.0
        .lock()
        .unwrap()
        .drain(..)
        .filter(|e| {
            matches!(
                e,
                EventMessage::PlaylistWatchChanged(_)
                    | EventMessage::PlaylistWatchUnmatched(_)
                    | EventMessage::PlaylistWatchDisabled(_)
            )
        })
        .collect()
}

#[tokio::test]
async fn first_run_is_silent_then_one_event_carries_groups_and_channels() {
    let dir = tempdir().expect("tempdir");
    let config = app_config(dir.path());
    let target = target(vec![
        ConfigWatchDto::channels("^DE Sport"),
        ConfigWatchDto::new("^Event", [WatchContext::GroupAdded]),
        ConfigWatchDto::new(".*", [WatchContext::GroupRemoved]),
    ]);
    let sink = PlaylistRunCollectSink::default();

    let first = vec![group("DE Sport HD", &["Sky 1"]), group("DE Kids", &["KiKa"])];
    assert!(process_watch(&config, &sink, &target, &first).await);
    assert!(watch_events(&sink).is_empty(), "first run must only write baselines");

    let second =
        vec![group("DE Sport HD", &["Sky 1", "Sky 5"]), group("Event Bundesliga", &["E1"]), group("News 2", &[])];
    assert!(process_watch(&config, &sink, &target, &second).await);
    let events = watch_events(&sink);
    assert_eq!(events.len(), 1, "{events:?}");
    let EventMessage::PlaylistWatchChanged(changes) = &events[0] else { panic!("{events:?}") };
    assert_eq!(changes.target, "sports");
    assert!(
        matches!(&changes.changes[0], WatchChange::GroupsAdded(i) if i.items == vec![Arc::from("Event Bundesliga")])
    );
    assert!(matches!(&changes.changes[1], WatchChange::GroupsRemoved(i) if i.items == vec![Arc::from("DE Kids")]));
    assert!(matches!(&changes.changes[2], WatchChange::ChannelsAdded(g)
        if g.group.as_ref() == "DE Sport HD" && g.items.items == vec![Arc::from("Sky 5")]));
    assert_eq!(changes.changes.len(), 3, "News 2 matches no group_added entry");
}

#[tokio::test]
async fn channel_only_watch_reports_no_group_changes() {
    let dir = tempdir().expect("tempdir");
    let config = app_config(dir.path());
    let target = target(vec![ConfigWatchDto::channels(".*")]);
    let sink = PlaylistRunCollectSink::default();
    assert!(process_watch(&config, &sink, &target, &[group("A", &["a"])]).await);
    assert!(process_watch(&config, &sink, &target, &[group("A", &["a"]), group("B", &["b"])]).await);
    assert!(watch_events(&sink).is_empty(), "a new group is only a new channel baseline here");
}

#[tokio::test]
async fn unmatched_warning_only_for_channel_entries_without_group_added() {
    let dir = tempdir().expect("tempdir");
    let config = app_config(dir.path());
    let target = target(vec![
        ConfigWatchDto::channels("^Nope1"),
        ConfigWatchDto::new("^Nope2", [WatchContext::Channels, WatchContext::GroupRemoved]),
        ConfigWatchDto::new("^Nope3", [WatchContext::Channels, WatchContext::GroupAdded]),
        ConfigWatchDto::new("^Nope4", [WatchContext::GroupAdded]),
        ConfigWatchDto::new("^Nope5", [WatchContext::GroupRemoved]),
    ]);
    let sink = PlaylistRunCollectSink::default();
    assert!(process_watch(&config, &sink, &target, &[group("A", &["a"])]).await);
    let events = watch_events(&sink);
    let EventMessage::PlaylistWatchUnmatched(unmatched) = &events[0] else { panic!("{events:?}") };
    assert_eq!(unmatched.patterns, vec!["^Nope1".to_string(), "^Nope2".to_string()]);
}

#[tokio::test]
async fn corrupt_group_index_still_reports_channel_changes() {
    let dir = tempdir().expect("tempdir");
    let config = app_config(dir.path());
    let target = target(vec![ConfigWatchDto::new(".*", [WatchContext::Channels, WatchContext::GroupAdded])]);
    let sink = PlaylistRunCollectSink::default();
    assert!(process_watch(&config, &sink, &target, &[group("A", &["a"])]).await);
    std::fs::write(dir.path().join("sports.groups.bin"), [0xff_u8; 3]).expect("corrupt index");

    assert!(process_watch(&config, &sink, &target, &[group("A", &["a", "b"]), group("B", &["x"])]).await);
    let events = watch_events(&sink);
    assert!(events.iter().any(|e| matches!(e, EventMessage::PlaylistWatchDisabled(_))), "{events:?}");
    let changes = events
        .iter()
        .find_map(|e| if let EventMessage::PlaylistWatchChanged(c) = e { Some(c) } else { None })
        .expect("channel change still reported");
    assert!(changes.changes.iter().all(|c| matches!(c, WatchChange::ChannelsAdded(_))), "no group change: {changes:?}");

    // The index was rewritten, so the next refresh diffs normally.
    let third = [group("A", &["a", "b"]), group("B", &["x"]), group("C", &[])];
    assert!(process_watch(&config, &sink, &target, &third).await);
    let events = watch_events(&sink);
    assert!(
        events.iter().any(|e| matches!(e, EventMessage::PlaylistWatchChanged(c)
            if matches!(&c.changes[0], WatchChange::GroupsAdded(i) if i.items == vec![Arc::from("C")]))),
        "{events:?}"
    );
}
