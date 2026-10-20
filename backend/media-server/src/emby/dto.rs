use serde::{Deserialize, Serialize};
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
    #[serde(rename = "Type")]
    pub type_: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub struct EmbyNameDto {
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub struct EmbyPersonDto {
    pub name: Option<String>,
    #[serde(rename = "Type")]
    pub type_: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub struct EmbyMediaStreamDto {
    #[serde(rename = "Type")]
    pub type_: Option<String>,
    pub codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub channels: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub struct EmbyVirtualFolderDto {
    pub name: Option<String>,
    pub collection_type: Option<String>,
    pub item_id: Option<String>,
    pub id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub struct EmbyUserDto {
    pub id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub struct EmbyAuthenticationResultDto {
    pub access_token: Option<String>,
    pub user: Option<EmbyUserDto>,
    pub server_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct EmbyAuthenticateByNameRequest<'a> {
    pub username: &'a str,
    pub pw: &'a str,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct EmbyItemDto {
    pub id: String,
    pub name: Option<String>,
    #[serde(rename = "Type")]
    pub type_: Option<String>,
    pub original_title: Option<String>,
    pub sort_name: Option<String>,
    pub overview: Option<String>,
    #[serde(default)]
    pub taglines: Vec<String>,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub studios: Vec<EmbyNameDto>,
    #[serde(default)]
    pub people: Vec<EmbyPersonDto>,
    pub official_rating: Option<String>,
    pub community_rating: Option<f32>,
    pub premiere_date: Option<String>,
    pub date_created: Option<String>,
    #[serde(default)]
    pub production_locations: Vec<String>,
    pub production_year: Option<u32>,
    pub parent_id: Option<String>,
    pub series_id: Option<String>,
    pub series_name: Option<String>,
    pub season_id: Option<String>,
    pub parent_index_number: Option<u32>,
    pub index_number: Option<u32>,
    pub child_count: Option<u32>,
    pub recursive_item_count: Option<u32>,
    pub run_time_ticks: Option<u64>,
    pub location_type: Option<String>,
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
    pub protocol: Option<String>,
    pub is_remote: Option<bool>,
    pub bitrate: Option<u32>,
    pub run_time_ticks: Option<u64>,
    pub supports_direct_play: Option<bool>,
    pub supports_direct_stream: Option<bool>,
    #[serde(default)]
    pub media_streams: Vec<EmbyMediaStreamDto>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::{
        EMBY_AUTH_RESULT_JSON, EMBY_EPISODES_PAGE_JSON, EMBY_ITEMS_PAGE_JSON, EMBY_MOVIES_PAGE_JSON,
        EMBY_PUBLIC_SYSTEM_INFO_JSON, EMBY_SEASONS_PAGE_JSON, EMBY_SERIES_PAGE_JSON, EMBY_VIRTUAL_FOLDERS_JSON,
        JELLYFIN_VIEWS_JSON,
    };

    #[test]
    fn parses_emby_item_page_and_keeps_edge_only_fields_at_boundary() {
        let page: EmbyItemsPageDto<EmbyItemDto> = serde_json::from_str(EMBY_ITEMS_PAGE_JSON).expect("fixture parses");

        assert_eq!(page.total_record_count, 1);
        assert_eq!(page.items[0].id, "item-redacted-1");
        assert_eq!(page.items[0].provider_ids.get("Tmdb").map(String::as_str), Some("12345"));
        assert!(page.items[0].path.as_deref().is_some_and(|path| path.contains("/redacted/")));
        assert!(page.items[0].user_data.is_some());
    }

    #[test]
    fn parses_movie_page_with_media_streams() {
        let page: EmbyItemsPageDto<EmbyItemDto> = serde_json::from_str(EMBY_MOVIES_PAGE_JSON).expect("fixture parses");

        assert_eq!(page.total_record_count, 2);
        let movie = &page.items[0];
        assert_eq!(movie.type_.as_deref(), Some("Movie"));
        assert_eq!(movie.original_title.as_deref(), Some("Movie Redacted Original"));
        assert_eq!(movie.sort_name.as_deref(), Some("movie redacted"));
        assert_eq!(movie.taglines, vec!["Redacted tagline"]);
        assert_eq!(movie.genres, vec!["Action", "Drama"]);
        assert_eq!(movie.studios[0].name.as_deref(), Some("Studio Redacted"));
        assert_eq!(movie.people.len(), 2);
        assert_eq!(movie.people[0].name.as_deref(), Some("Actor Redacted"));
        assert_eq!(movie.people[0].type_.as_deref(), Some("Actor"));
        assert_eq!(movie.people[1].type_.as_deref(), Some("Director"));
        assert_eq!(movie.official_rating.as_deref(), Some("PG-13"));
        assert_eq!(movie.community_rating, Some(7.4));
        assert_eq!(movie.premiere_date.as_deref(), Some("2024-03-01T00:00:00.0000000Z"));
        assert_eq!(movie.date_created.as_deref(), Some("2024-04-02T10:15:30.1234567Z"));
        assert_eq!(movie.production_locations, vec!["Redacted Country"]);
        assert_eq!(movie.run_time_ticks, Some(72_000_000_000));
        assert_eq!(movie.location_type.as_deref(), Some("FileSystem"));
        assert_eq!(movie.image_tags.get("Primary").map(String::as_str), Some("image-tag-redacted"));

        let source = &movie.media_sources[0];
        assert_eq!(source.protocol.as_deref(), Some("File"));
        assert_eq!(source.is_remote, Some(false));
        assert_eq!(source.bitrate, Some(8_000_000));
        assert_eq!(source.run_time_ticks, Some(72_000_000_000));
        assert_eq!(source.media_streams.len(), 3);
        assert_eq!(source.media_streams[0].type_.as_deref(), Some("Video"));
        assert_eq!(source.media_streams[0].codec.as_deref(), Some("h264"));
        assert_eq!(source.media_streams[0].width, Some(1920));
        assert_eq!(source.media_streams[0].height, Some(1080));
        assert_eq!(source.media_streams[1].type_.as_deref(), Some("Audio"));
        assert_eq!(source.media_streams[1].channels, Some(6));

        let minimal = &page.items[1];
        assert_eq!(minimal.media_sources, Vec::new());
        assert_eq!(minimal.genres, Vec::<String>::new());
        assert_eq!(minimal.people, Vec::new());
        assert!(minimal.community_rating.is_none());
    }

    #[test]
    fn parses_series_and_episode_hierarchy_fields() {
        let series: EmbyItemsPageDto<EmbyItemDto> =
            serde_json::from_str(EMBY_SERIES_PAGE_JSON).expect("fixture parses");
        assert_eq!(series.items[0].child_count, Some(2));
        assert_eq!(series.items[0].recursive_item_count, Some(14));

        let episodes: EmbyItemsPageDto<EmbyItemDto> =
            serde_json::from_str(EMBY_EPISODES_PAGE_JSON).expect("fixture parses");
        assert_eq!(episodes.items.len(), 3);
        let first = &episodes.items[0];
        assert_eq!(first.series_id.as_deref(), Some("series-redacted-1"));
        assert_eq!(first.series_name.as_deref(), Some("Series Redacted"));
        assert_eq!(first.season_id.as_deref(), Some("season-redacted-1"));
        assert_eq!(first.index_number, Some(1));
        assert_eq!(first.parent_index_number, Some(1));
        assert_eq!(episodes.items[1].location_type.as_deref(), Some("Virtual"));
        let remote = &episodes.items[2].media_sources[0];
        assert_eq!(remote.protocol.as_deref(), Some("Http"));
        assert_eq!(remote.is_remote, Some(true));
    }

    #[test]
    fn parses_type_field_as_pascal_type() {
        let item: EmbyItemDto = serde_json::from_str(r#"{"Id":"i","Type":"Episode"}"#).expect("item parses");
        assert_eq!(item.type_.as_deref(), Some("Episode"));

        let page: EmbyItemsPageDto<EmbyViewDto> = serde_json::from_str(JELLYFIN_VIEWS_JSON).expect("fixture parses");
        assert_eq!(page.items[0].type_.as_deref(), Some("CollectionFolder"));

        let person: EmbyPersonDto = serde_json::from_str(r#"{"Name":"n","Type":"Actor"}"#).expect("person parses");
        assert_eq!(person.type_.as_deref(), Some("Actor"));
        let stream: EmbyMediaStreamDto = serde_json::from_str(r#"{"Type":"Audio"}"#).expect("stream parses");
        assert_eq!(stream.type_.as_deref(), Some("Audio"));
    }

    #[test]
    fn parses_virtual_folders_with_item_id() {
        let folders: Vec<EmbyVirtualFolderDto> =
            serde_json::from_str(EMBY_VIRTUAL_FOLDERS_JSON).expect("fixture parses");

        assert_eq!(folders.len(), 4);
        assert_eq!(folders[0].name.as_deref(), Some("Movies"));
        assert_eq!(folders[0].collection_type.as_deref(), Some("movies"));
        assert_eq!(folders[0].item_id.as_deref(), Some("library-redacted-movies"));
        assert_eq!(folders[0].id.as_deref(), Some("library-redacted-movies-guid"));
        assert_eq!(folders[1].collection_type.as_deref(), Some("tvshows"));
        assert!(folders[2].collection_type.is_none());
        assert_eq!(folders[3].collection_type.as_deref(), Some("boxsets"));
    }

    #[test]
    fn parses_authentication_result() {
        let auth: EmbyAuthenticationResultDto = serde_json::from_str(EMBY_AUTH_RESULT_JSON).expect("fixture parses");

        assert_eq!(auth.access_token.as_deref(), Some("token-redacted"));
        assert_eq!(auth.server_id.as_deref(), Some("server-redacted-id"));
        assert_eq!(auth.user.and_then(|user| user.id).as_deref(), Some("user-redacted-id"));
    }

    #[test]
    fn parses_public_system_info() {
        let info: EmbyPublicSystemInfoDto = serde_json::from_str(EMBY_PUBLIC_SYSTEM_INFO_JSON).expect("fixture parses");

        assert_eq!(info.id.as_deref(), Some("server-redacted-id"));
        assert_eq!(info.server_name.as_deref(), Some("Server Redacted"));
    }

    #[test]
    fn parses_seasons_page() {
        let seasons: EmbyItemsPageDto<EmbyItemDto> =
            serde_json::from_str(EMBY_SEASONS_PAGE_JSON).expect("fixture parses");

        assert_eq!(seasons.items.len(), 2);
        assert_eq!(seasons.items[0].type_.as_deref(), Some("Season"));
        assert_eq!(seasons.items[0].index_number, Some(1));
        assert_eq!(seasons.items[0].child_count, Some(8));
    }

    #[test]
    fn authenticate_request_serializes_username_and_pw() {
        let body = serde_json::to_string(&EmbyAuthenticateByNameRequest { username: "u", pw: "p" })
            .expect("request serializes");
        assert_eq!(body, r#"{"Username":"u","Pw":"p"}"#);
    }
}
