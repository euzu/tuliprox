use crate::model::{VideoCodec, VideoResolution};
use serde::{Deserialize, Serialize};

/// High-level health evaluation of a failover group member for playlist users.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserMemberHealthState {
    Ok,
    Degraded,
    Failing,
}

/// Redacted health and quality information of a failover member for playlist users.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMemberHealthDto {
    pub state: UserMemberHealthState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<VideoResolution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_codec: Option<VideoCodec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qos_24h: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qos_7d: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_secs: Option<u64>,
}

/// Redacted member information of a failover group for playlist user clients.
///
/// Contains no provider identifiers, input names, or internal stream ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserFailoverMemberDto {
    pub member_vid: u32,
    pub index: usize,
    pub title: String,
    pub has_archive: bool,
    pub current: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<UserMemberHealthDto>,
}

/// Redacted failover group response for playlist user clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserFailoverMembersDto {
    pub group_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_member_vid: Option<u32>,
    pub members: Vec<UserFailoverMemberDto>,
}
