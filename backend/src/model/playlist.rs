use std::collections::HashSet;
use crate::model::{ConfigInput, TVGuide};
use shared::model::{PlaylistGroup, PlaylistItem, UUIDType};
use crate::repository::PlaylistSource;

pub struct FetchedPlaylist<'a> {
    pub input: &'a ConfigInput,
    pub source: Box<dyn PlaylistSource>,
    pub epg: Option<TVGuide>,
}

impl FetchedPlaylist<'_> {
    pub async fn update_playlist(&mut self, plg: &PlaylistGroup) {
        if self.source.is_memory() {
            self.source.update_playlist(plg).await;
        }
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

    pub fn get_missing_vod_info_count(&mut self) -> usize {
        self.source.get_missing_vod_info_count()
    }

    pub fn get_missing_series_info_count(&mut self) -> usize {
        self.source.get_missing_series_info_count()
    }

    pub fn deduplicate(&mut self, duplicates: &mut HashSet<UUIDType>) {
        self.source.deduplicate(duplicates);
    }

    pub fn clone_source(&self) -> Box<dyn PlaylistSource> {
        self.source.clone_box()
    }
}

