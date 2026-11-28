//! The Explorer group order is a managed `sort.rules` group rule of the target: matched groups
//! publish in sequence order, unlisted groups keep their previous order after them.

use crate::processor::sort::sort_playlist;
use shared::{
    model::{ConfigTargetDto, PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType, XtreamCluster},
    utils::Internable,
};
use tuliprox_core::model::ConfigTarget;

const EXPLORER_SORT: &str = "\
name: Explorer
output:
  - type: m3u
    filename: explorer.m3u
sort:
  rules:
    - target: group
      field: group
      order: none
      filter: 'Type = live'
      sequence:
        - '^News$'
        - '^Sports$'
";

/// A live group whose only channel carries the group name and the position in the source.
///
/// Source ordinals start at 1: ordinal 0 means "not from a source order" and sorts last.
fn live_group(title: &str, ordinal: u32) -> PlaylistGroup {
    let header = PlaylistItemHeader {
        name: title.intern(),
        title: title.intern(),
        group: title.intern(),
        item_type: PlaylistItemType::Live,
        xtream_cluster: XtreamCluster::Live,
        source_ordinal: ordinal,
        ..Default::default()
    };
    PlaylistGroup {
        id: ordinal,
        title: title.intern(),
        channels: vec![PlaylistItem { header }],
        xtream_cluster: XtreamCluster::Live,
    }
}

fn target_with_sort(sort: bool) -> ConfigTarget {
    let mut dto: ConfigTargetDto = shared::utils::yaml_from_str(EXPLORER_SORT).expect("target fixture");
    if !sort {
        dto.sort = None;
    }
    dto.prepare(1, None, None).expect("prepared target");
    ConfigTarget::from(&dto)
}

fn titles(groups: &[PlaylistGroup]) -> Vec<String> { groups.iter().map(|group| group.title.to_string()).collect() }

#[test]
fn the_stored_order_publishes_listed_groups_first_and_keeps_the_rest() {
    let mut playlist = vec![
        live_group("Movies", 1),
        live_group("Zeta", 2),
        live_group("News", 3),
        live_group("Sports", 4),
        live_group("Alpha", 5),
    ];

    assert!(sort_playlist(&target_with_sort(true), &mut playlist, None));

    assert_eq!(titles(&playlist), ["News", "Sports", "Movies", "Zeta", "Alpha"]);
}

#[test]
fn a_target_without_the_rule_keeps_the_source_order() {
    let mut playlist = vec![
        live_group("Movies", 1),
        live_group("Zeta", 2),
        live_group("News", 3),
        live_group("Sports", 4),
        live_group("Alpha", 5),
    ];

    assert!(sort_playlist(&target_with_sort(false), &mut playlist, None));

    assert_eq!(titles(&playlist), ["Movies", "Zeta", "News", "Sports", "Alpha"]);
}
