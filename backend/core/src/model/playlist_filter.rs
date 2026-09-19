//! Filtering playlist groups against a configured filter expression.
//!
//! Pure predicate work over shared playlist types. It lived in
//! `processing::processor::playlist`, which is what made `repository` reach up
//! into `processing` to filter a playlist before persisting it.

use shared::{
    foundation::{Filter, ValueProvider},
    model::{PlaylistGroup, PlaylistItem},
};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct FilterOutcome {
    pub inspected: usize,
    pub retained: usize,
    pub removed: usize,
}

impl FilterOutcome {
    pub fn record(&mut self, retained: bool) -> bool {
        self.inspected += 1;
        if retained {
            self.retained += 1;
        } else {
            self.removed += 1;
        }
        retained
    }
}

pub fn is_valid(pli: &PlaylistItem, filter: &Filter, match_as_ascii: bool) -> bool {
    let provider = ValueProvider { pli, match_as_ascii };
    filter.filter(&provider)
}

pub fn apply_filter_to_playlist(playlist: &[PlaylistGroup], filter: &Filter) -> Vec<PlaylistGroup> {
    // NOTE: the source `playlist` is intentionally cloned (not drained) here because
    // the caller reuses the same slice for every target output and for the no-filter
    // fallback path, so the survivors cannot be moved out of it. Cap the initial
    // allocation so selective filters do not retain capacity for every source item.
    const INITIAL_FILTERED_GROUP_CAPACITY: usize = 256;
    let mut new_playlist = Vec::with_capacity(playlist.len());
    for pg in playlist {
        let mut channels = Vec::with_capacity(pg.channels.len().min(INITIAL_FILTERED_GROUP_CAPACITY));
        channels.extend(pg.channels.iter().filter(|&pli| is_valid(pli, filter, false)).cloned());
        if !channels.is_empty() {
            new_playlist.push(PlaylistGroup {
                id: pg.id,
                title: pg.title.clone(),
                channels,
                xtream_cluster: pg.xtream_cluster,
            });
        }
    }
    new_playlist
}

pub fn retain_filtered_playlist(playlist: &mut Vec<PlaylistGroup>, filter: &Filter) -> FilterOutcome {
    let mut outcome = FilterOutcome::default();
    for group in playlist.iter_mut() {
        group.channels.retain(|item| outcome.record(is_valid(item, filter, false)));
    }
    playlist.retain(|group| !group.channels.is_empty());
    outcome
}

#[cfg(test)]
mod tests {
    use super::{apply_filter_to_playlist, retain_filtered_playlist, FilterOutcome};
    use shared::{
        foundation::get_filter,
        model::{PlaylistGroup, PlaylistItem, PlaylistItemHeader, XtreamCluster},
        utils::Internable,
    };

    fn playlist_with_name(name: &str) -> Vec<PlaylistGroup> {
        vec![PlaylistGroup {
            id: 1,
            title: "Group".intern(),
            channels: vec![PlaylistItem {
                header: PlaylistItemHeader { name: name.intern(), group: "Group".intern(), ..Default::default() },
            }],
            xtream_cluster: XtreamCluster::Live,
        }]
    }

    #[test]
    fn output_filter_with_no_match_returns_an_empty_playlist() {
        let playlist = playlist_with_name("kept-by-persist");
        let filter = get_filter(r#"Name = "not-present""#, None).expect("filter");

        let filtered = apply_filter_to_playlist(&playlist, &filter);

        assert!(filtered.is_empty());
    }

    #[test]
    fn retain_filter_removes_empty_groups_and_reports_outcome() {
        let mut playlist = playlist_with_name("removed");
        playlist.extend(playlist_with_name("kept"));
        let filter = get_filter(r#"Name = "kept""#, None).expect("filter");

        let outcome = retain_filtered_playlist(&mut playlist, &filter);

        assert_eq!(outcome, FilterOutcome { inspected: 2, retained: 1, removed: 1 });
        assert_eq!(playlist.len(), 1);
        assert_eq!(playlist[0].channels.len(), 1);
    }
}
