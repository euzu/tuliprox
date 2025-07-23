use crate::services::request_post;
use log::error;

const TARGET_UPDATE_API_PATH: &str = "/api/v1/playlist/update";

pub struct PlaylistService {}
impl Default for PlaylistService {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaylistService {
    pub fn new() -> Self {
        Self {}
    }

    pub async fn update_targets(&self, targets: &[&str]) -> bool {
        request_post::<&[&str], ()>(TARGET_UPDATE_API_PATH, targets).await.map_or_else(|err| {
            error!("{err}");
            false
        }, |_| true)
    }
}
