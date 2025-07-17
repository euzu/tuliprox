use crate::model::{TVGuide, ConfigInput};
use shared::model::{PlaylistGroup};

#[derive(Debug, Clone)]
pub struct FetchedPlaylist<'a> { // Contains playlist for one input
    pub input: &'a ConfigInput,
    pub playlistgroups: Vec<PlaylistGroup>,
    pub epg: Option<TVGuide>,
}


impl FetchedPlaylist<'_> {
    pub fn update_playlist(&mut self, plg: &PlaylistGroup) {
        for grp in &mut self.playlistgroups {
            if grp.id == plg.id {
                plg.channels.iter().for_each(|item| grp.channels.push(item.clone()));
                return;
            }
        }
    }
}
