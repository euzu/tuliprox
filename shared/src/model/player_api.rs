use serde::{Deserialize, Serialize};

/// Live TV channel visible to an authenticated player API user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserChannelDto {
    pub virtual_id: u32,
    pub name: String,
    pub group: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epg_channel_id: Option<String>,
    pub is_group: bool,
}

/// EPG programme listing for a visible live channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserProgrammeDto {
    pub start: String,
    pub start_ts: i64,
    pub stop: String,
    pub stop_ts: i64,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desc: Option<String>,
    pub categories: Vec<String>,
    pub is_live: bool,
    pub is_new: bool,
}

/// Response returned by `POST /api/v1/me/streams/{uid}/split`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitStreamResponseDto {
    pub reconnect_required: bool,
    pub url: String,
}
