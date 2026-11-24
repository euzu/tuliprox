use crate::model::PlaylistItemType;
use serde::{Deserialize, Serialize};

/// Summary of an active playback stream belonging to a playlist user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserStreamDto {
    pub uid: u32,
    pub virtual_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_virtual_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_vid: Option<u32>,
    pub title: String,
    pub item_type: PlaylistItemType,
    pub protocol: String,
    pub shared: bool,
    pub session_active: bool,
    pub started_at: u64,
    pub user_agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
}
