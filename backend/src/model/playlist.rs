use crate::model::{ConfigInput, TVGuide};
use crate::repository::provider_source::ProviderPlaylistSource;
use shared::model::PlaylistGroup;

#[derive(Debug)]
pub struct FetchedPlaylist<'a> {
    pub input: &'a ConfigInput,
    pub source: ProviderPlaylistSource,
    pub epg: Option<TVGuide>,
}

impl FetchedPlaylist<'_> {
    pub fn clone_schema(&self) -> Self {
        Self {
            input: self.input,
            source: ProviderPlaylistSource::Memory(Box::default()),
            epg: self.epg.clone(),
        }
    }

    pub fn update_playlist(&mut self, plg: &PlaylistGroup) {
        if let ProviderPlaylistSource::Memory(groups) = &mut self.source {
            for grp in groups.iter_mut() {
                if grp.id == plg.id {
                    plg.channels.iter().for_each(|item| grp.channels.push(item.clone()));
                    return;
                }
            }
        }
    }
}
