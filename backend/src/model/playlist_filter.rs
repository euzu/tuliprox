//! Filtering playlist groups against a configured filter expression.
//!
//! Pure predicate work over shared playlist types. It lived in
//! `processing::processor::playlist`, which is what made `repository` reach up
//! into `processing` to filter a playlist before persisting it.

use shared::foundation::{Filter, ValueProvider};
use shared::model::{PlaylistGroup, PlaylistItem};

pub fn is_valid(pli: &PlaylistItem, filter: &Filter, match_as_ascii: bool) -> bool {
    let provider = ValueProvider { pli, match_as_ascii };
    filter.filter(&provider)
}

pub fn apply_filter_to_playlist(playlist: &mut [PlaylistGroup], filter: &Filter) -> Option<Vec<PlaylistGroup>> {
    // NOTE: the source `playlist` is intentionally cloned (not drained) here because
    // the caller reuses the same slice for every target output and for the no-filter
    // fallback path, so the survivors cannot be moved out of it. Cap the initial
    // allocation so selective filters do not retain capacity for every source item.
    const INITIAL_FILTERED_GROUP_CAPACITY: usize = 256;
    let mut new_playlist = Vec::with_capacity(playlist.len());
    for pg in playlist.iter() {
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
    if new_playlist.is_empty() {
        None
    } else {
        Some(new_playlist)
    }
}
