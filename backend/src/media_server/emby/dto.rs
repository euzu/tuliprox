use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub struct EmbyPublicSystemInfoDto {
    pub id: Option<String>,
    pub server_name: Option<String>,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase", bound(deserialize = "T: Deserialize<'de>"))]
pub struct EmbyItemsPageDto<T> {
    #[serde(default)]
    pub items: Vec<T>,
    #[serde(default)]
    pub total_record_count: usize,
    #[serde(default)]
    pub start_index: usize,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub struct EmbyViewDto {
    pub id: String,
    pub name: Option<String>,
    pub collection_type: Option<String>,
    pub type_: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct EmbyItemDto {
    pub id: String,
    pub name: Option<String>,
    pub type_: Option<String>,
    pub production_year: Option<u32>,
    pub parent_id: Option<String>,
    pub series_id: Option<String>,
    pub series_name: Option<String>,
    pub parent_index_number: Option<u32>,
    pub index_number: Option<u32>,
    #[serde(default)]
    pub provider_ids: HashMap<String, String>,
    #[serde(default)]
    pub image_tags: HashMap<String, String>,
    #[serde(default)]
    pub media_sources: Vec<EmbyMediaSourceDto>,
    // Parsed only so the boundary can explicitly ignore it by default.
    pub path: Option<String>,
    pub user_data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct EmbyMediaSourceDto {
    pub id: Option<String>,
    pub container: Option<String>,
    pub path: Option<String>,
    pub supports_direct_play: Option<bool>,
    pub supports_direct_stream: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct EmbyPlaybackInfoDto {
    #[serde(default)]
    pub media_sources: Vec<EmbyMediaSourceDto>,
    pub play_session_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_server::test_fixtures::EMBY_ITEMS_PAGE_JSON;

    #[test]
    fn parses_emby_item_page_and_keeps_edge_only_fields_at_boundary() {
        let page: EmbyItemsPageDto<EmbyItemDto> = serde_json::from_str(EMBY_ITEMS_PAGE_JSON).expect("fixture parses");

        assert_eq!(page.total_record_count, 1);
        assert_eq!(page.items[0].id, "item-redacted-1");
        assert_eq!(page.items[0].provider_ids.get("Tmdb").map(String::as_str), Some("12345"));
        assert!(page.items[0].path.as_deref().is_some_and(|path| path.contains("/redacted/")));
        assert!(page.items[0].user_data.is_some());
    }
}
