use std::sync::Arc;
use crate::utils::{serialize_as_base64_padded, arc_str_serde};
#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
pub struct ShortEpgDto {
    #[serde(with = "arc_str_serde")]
    pub id: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub epg_id: Arc<str>,
    #[serde(serialize_with = "serialize_as_base64_padded")]
    pub title: String,
    pub lang: String,
    pub start: String, // Format "2026-01-14 23:50:00"
    pub end: String, // Format "2026-01-14 00:45:00"
    #[serde(serialize_with = "serialize_as_base64_padded")]
    pub description: String,
    #[serde(with = "arc_str_serde")]
    pub channel_id: Arc<str>,
    pub start_timestamp: String,  // Format "1768431000"
    pub stop_timestamp: String,  // Format "1768434300"
    #[serde(with = "arc_str_serde")]
    pub stream_id: Arc<str>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
pub struct ShortEpgResultDto {
    pub epg_listings: Vec<ShortEpgDto>,
}

impl ShortEpgResultDto {
    pub fn new(epg_listings: Vec<ShortEpgDto>) -> Self {
        Self {
            epg_listings,
        }
    }
}