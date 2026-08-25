use crate::model::{ConfigInput, TVGuide};
use super::PlaylistSource;
use shared::error::TuliproxError;
use shared::model::UUIDType;
use shared::model::{PlaylistGroup, PlaylistItem};
use std::collections::HashSet;

pub struct FetchedPlaylist<'a> {
    pub input: &'a ConfigInput,
    pub source: PlaylistSource,
    pub epg: Option<TVGuide>,
}


impl FetchedPlaylist<'_> {
    pub fn sort_by_provider_ordinal(&mut self) {
        self.source.sort_by_provider_ordinal();
    }

    pub async fn update_playlist(&mut self, plg: &PlaylistGroup) {
        if self.source.is_memory() {
            self.source.update_playlist(plg).await;
        }
    }

    /// Merge a batch of groups in one indexed pass instead of calling
    /// [`Self::update_playlist`] once per group (which re-scans the whole
    /// in-memory playlist each time). Only affects in-memory sources.
    pub fn extend_playlist(&mut self, groups: Vec<PlaylistGroup>) {
        if self.source.is_memory() {
            self.source.extend_playlist(groups);
        }
    }

    pub fn is_memory(&self) -> bool {
        self.source.is_memory()
    }

    pub fn get_channel_count(&mut self) -> usize {
        self.source.get_channel_count()
    }

    pub fn get_group_count(&mut self) -> usize {
        self.source.get_group_count()
    }

    pub fn items_mut(&mut self) -> Box<dyn Iterator<Item=&mut PlaylistItem> + Send + '_> {
        self.source.items_mut()
    }

    pub(crate) fn items<'a>(&'a mut self) -> Box<dyn Iterator<Item=std::borrow::Cow<'a, PlaylistItem>> + Send + 'a> {
        self.source.items()
    }

    pub fn get_missing_vod_info_count(&mut self) -> usize {
        self.source.get_missing_vod_info_count()
    }

    pub fn get_missing_series_info_count(&mut self) -> usize {
        self.source.get_missing_series_info_count()
    }

    pub fn deduplicate(&mut self, duplicates: &mut HashSet<UUIDType>) {
        self.source.deduplicate(duplicates);
    }

    pub fn clone_source(&self) -> Result<PlaylistSource, TuliproxError> { self.source.clone_source() }
}
