use crate::utils::{serialize_as_base64, serialize_option_string_as_null_if_empty};

#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
pub struct ShortEpgDto {
    pub id: String,
    pub epg_id: String,
    #[serde(serialize_with = "serialize_as_base64")]
    pub title: String,
    pub lang: String,
    pub start: String, // Format "2026-01-14 23:50:00"
    pub end: String, // Format "2026-01-14 00:45:00"
    #[serde(serialize_with = "serialize_as_base64")]
    pub description: String,
    pub channel_id : String,
    pub start_timestamp: String,  // Format "1768431000"
    pub stop_timestamp: String,  // Format "1768434300"
    pub stream_id: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
pub struct ShortEpgListingsDto {
    pub epg_listings : Vec<ShortEpgDto>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
pub struct ShortEpgResultDto {
    pub data: ShortEpgListingsDto,
    #[serde(default, serialize_with = "serialize_option_string_as_null_if_empty")]
    pub error: Option<String>,
}

impl ShortEpgResultDto {
    pub fn new(epg_listings: Vec<ShortEpgDto>) -> Self {
        Self {
            data: ShortEpgListingsDto {
                epg_listings,
            },
            error: None,
        }
    }
}