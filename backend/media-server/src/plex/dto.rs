use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename = "MediaContainer")]
pub struct PlexResourcesDto {
    #[serde(rename = "Device", default)]
    pub devices: Vec<PlexResourceDto>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PlexResourceDto {
    #[serde(rename = "@name")]
    pub name: Option<String>,
    #[serde(rename = "@product")]
    pub product: Option<String>,
    #[serde(rename = "@productVersion")]
    pub product_version: Option<String>,
    #[serde(rename = "@clientIdentifier")]
    pub client_identifier: Option<String>,
    #[serde(rename = "@machineIdentifier")]
    pub machine_identifier: Option<String>,
    #[serde(rename = "@owned", default)]
    pub owned: Option<u8>,
    #[serde(rename = "@accessToken")]
    pub access_token: Option<String>,
    #[serde(rename = "Connection", default)]
    pub connections: Vec<PlexConnectionDto>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PlexConnectionDto {
    #[serde(rename = "@protocol")]
    pub protocol: Option<String>,
    #[serde(rename = "@uri")]
    pub uri: Option<String>,
    #[serde(rename = "@local", default)]
    pub local: Option<u8>,
    #[serde(rename = "@relay", default)]
    pub relay: Option<u8>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename = "MediaContainer")]
pub struct PlexSectionsDto {
    #[serde(rename = "Directory", default)]
    pub directories: Vec<PlexSectionDto>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PlexSectionDto {
    #[serde(rename = "@key")]
    pub key: Option<String>,
    #[serde(rename = "@title")]
    pub title: Option<String>,
    #[serde(rename = "@type")]
    pub section_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename = "MediaContainer")]
pub struct PlexMediaContainerDto {
    #[serde(rename = "@size")]
    pub size: Option<usize>,
    #[serde(rename = "@totalSize")]
    pub total_size: Option<usize>,
    #[serde(rename = "Video", default)]
    pub videos: Vec<PlexVideoDto>,
    #[serde(rename = "Directory", default)]
    pub directories: Vec<PlexDirectoryDto>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PlexDirectoryDto {
    #[serde(rename = "@ratingKey")]
    pub rating_key: Option<String>,
    #[serde(rename = "@key")]
    pub key: Option<String>,
    #[serde(rename = "@type")]
    pub item_type: Option<String>,
    #[serde(rename = "@title")]
    pub title: Option<String>,
    #[serde(rename = "@titleSort")]
    pub title_sort: Option<String>,
    #[serde(rename = "@originalTitle")]
    pub original_title: Option<String>,
    #[serde(rename = "@year")]
    pub year: Option<u32>,
    #[serde(rename = "@originallyAvailableAt")]
    pub originally_available_at: Option<String>,
    #[serde(rename = "@summary")]
    pub summary: Option<String>,
    #[serde(rename = "@tagline")]
    pub tagline: Option<String>,
    #[serde(rename = "@studio")]
    pub studio: Option<String>,
    #[serde(rename = "@contentRating")]
    pub content_rating: Option<String>,
    #[serde(rename = "@contentRatingAge")]
    pub content_rating_age: Option<u32>,
    #[serde(rename = "@audienceRating")]
    pub audience_rating: Option<String>,
    #[serde(rename = "@guid")]
    pub guid: Option<String>,
    #[serde(rename = "@thumb")]
    pub thumb: Option<String>,
    #[serde(rename = "@art")]
    pub art: Option<String>,
    #[serde(rename = "@theme")]
    pub theme: Option<String>,
    #[serde(rename = "@parentRatingKey")]
    pub parent_rating_key: Option<String>,
    #[serde(rename = "@parentTitle")]
    pub parent_title: Option<String>,
    #[serde(rename = "@parentGuid")]
    pub parent_guid: Option<String>,
    #[serde(rename = "@parentIndex")]
    pub parent_index: Option<u32>,
    #[serde(rename = "@index")]
    pub index: Option<u32>,
    #[serde(rename = "@childCount")]
    pub child_count: Option<u32>,
    #[serde(rename = "@leafCount")]
    pub leaf_count: Option<u32>,
    #[serde(rename = "@viewedLeafCount")]
    pub viewed_leaf_count: Option<u32>,
    #[serde(rename = "@addedAt")]
    pub added_at: Option<i64>,
    #[serde(rename = "@updatedAt")]
    pub updated_at: Option<i64>,
    #[serde(rename = "Guid", default)]
    pub guids: Vec<PlexGuidDto>,
    #[serde(rename = "Genre", default)]
    pub genres: Vec<PlexTagDto>,
    #[serde(rename = "Country", default)]
    pub countries: Vec<PlexTagDto>,
    #[serde(rename = "Role", default)]
    pub roles: Vec<PlexTagDto>,
    #[serde(rename = "Image", default)]
    pub images: Vec<PlexImageDto>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PlexVideoDto {
    #[serde(rename = "@ratingKey")]
    pub rating_key: Option<String>,
    #[serde(rename = "@key")]
    pub key: Option<String>,
    #[serde(rename = "@type")]
    pub item_type: Option<String>,
    #[serde(rename = "@title")]
    pub title: Option<String>,
    #[serde(rename = "@titleSort")]
    pub title_sort: Option<String>,
    #[serde(rename = "@originalTitle")]
    pub original_title: Option<String>,
    #[serde(rename = "@year")]
    pub year: Option<u32>,
    #[serde(rename = "@originallyAvailableAt")]
    pub originally_available_at: Option<String>,
    #[serde(rename = "@summary")]
    pub summary: Option<String>,
    #[serde(rename = "@tagline")]
    pub tagline: Option<String>,
    #[serde(rename = "@studio")]
    pub studio: Option<String>,
    #[serde(rename = "@contentRating")]
    pub content_rating: Option<String>,
    #[serde(rename = "@contentRatingAge")]
    pub content_rating_age: Option<u32>,
    #[serde(rename = "@audienceRating")]
    pub audience_rating: Option<String>,
    #[serde(rename = "@guid")]
    pub guid: Option<String>,
    #[serde(rename = "@thumb")]
    pub thumb: Option<String>,
    #[serde(rename = "@art")]
    pub art: Option<String>,
    #[serde(rename = "@parentRatingKey")]
    pub parent_rating_key: Option<String>,
    #[serde(rename = "@parentGuid")]
    pub parent_guid: Option<String>,
    #[serde(rename = "@parentTitle")]
    pub parent_title: Option<String>,
    #[serde(rename = "@grandparentRatingKey")]
    pub grandparent_rating_key: Option<String>,
    #[serde(rename = "@grandparentGuid")]
    pub grandparent_guid: Option<String>,
    #[serde(rename = "@grandparentTitle")]
    pub grandparent_title: Option<String>,
    #[serde(rename = "@parentIndex")]
    pub parent_index: Option<u32>,
    #[serde(rename = "@index")]
    pub index: Option<u32>,
    #[serde(rename = "@addedAt")]
    pub added_at: Option<i64>,
    #[serde(rename = "@updatedAt")]
    pub updated_at: Option<i64>,
    #[serde(rename = "Guid", default)]
    pub guids: Vec<PlexGuidDto>,
    #[serde(rename = "Genre", default)]
    pub genres: Vec<PlexTagDto>,
    #[serde(rename = "Country", default)]
    pub countries: Vec<PlexTagDto>,
    #[serde(rename = "Director", default)]
    pub directors: Vec<PlexTagDto>,
    #[serde(rename = "Writer", default)]
    pub writers: Vec<PlexTagDto>,
    #[serde(rename = "Role", default)]
    pub roles: Vec<PlexTagDto>,
    #[serde(rename = "Image", default)]
    pub images: Vec<PlexImageDto>,
    #[serde(rename = "Media", default)]
    pub media: Vec<PlexMediaDto>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PlexGuidDto {
    #[serde(rename = "@id")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PlexTagDto {
    #[serde(rename = "@tag")]
    pub tag: Option<String>,
    #[serde(rename = "@title")]
    pub title: Option<String>,
}

impl PlexTagDto {
    pub fn value(&self) -> Option<std::sync::Arc<str>> {
        self.tag
            .as_deref()
            .or(self.title.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(std::sync::Arc::<str>::from)
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PlexImageDto {
    #[serde(rename = "@type")]
    pub image_type: Option<String>,
    #[serde(rename = "@url")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PlexMediaDto {
    #[serde(rename = "@id")]
    pub id: Option<String>,
    #[serde(rename = "@container")]
    pub container: Option<String>,
    #[serde(rename = "@duration")]
    pub duration: Option<u64>,
    #[serde(rename = "@bitrate")]
    pub bitrate: Option<u32>,
    #[serde(rename = "@width")]
    pub width: Option<u32>,
    #[serde(rename = "@height")]
    pub height: Option<u32>,
    #[serde(rename = "@audioChannels")]
    pub audio_channels: Option<u32>,
    #[serde(rename = "@audioCodec")]
    pub audio_codec: Option<String>,
    #[serde(rename = "@videoCodec")]
    pub video_codec: Option<String>,
    #[serde(rename = "@videoResolution")]
    pub video_resolution: Option<String>,
    #[serde(rename = "Part", default)]
    pub parts: Vec<PlexPartDto>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PlexPartDto {
    #[serde(rename = "@id")]
    pub id: Option<String>,
    #[serde(rename = "@key")]
    pub key: Option<String>,
    #[serde(rename = "@size")]
    pub size: Option<u64>,
    #[serde(rename = "@file")]
    pub file: Option<String>,
    #[serde(rename = "@container")]
    pub container: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::{PLEX_MOVIES_XML, PLEX_RESOURCES_XML, PLEX_SECTIONS_XML};

    #[test]
    fn parses_plex_resources_without_exposing_resource_token() {
        let resources: PlexResourcesDto = quick_xml::de::from_str(PLEX_RESOURCES_XML).expect("fixture parses");

        assert_eq!(resources.devices.len(), 1);
        assert_eq!(resources.devices[0].connections.len(), 1);
        assert_eq!(resources.devices[0].access_token.as_deref(), Some("resource-token-redacted"));
    }

    #[test]
    fn parses_plex_sections_with_unsupported_kind_visible() {
        let sections: PlexSectionsDto = quick_xml::de::from_str(PLEX_SECTIONS_XML).expect("fixture parses");

        assert_eq!(sections.directories.len(), 3);
        assert!(sections.directories.iter().any(|section| section.section_type.as_deref() == Some("artist")));
    }

    #[test]
    fn parses_plex_movie_parts_and_optional_guids() {
        let container: PlexMediaContainerDto = quick_xml::de::from_str(PLEX_MOVIES_XML).expect("fixture parses");
        let movie = &container.videos[0];
        let part = &movie.media[0].parts[0];

        assert_eq!(container.total_size, Some(1));
        assert_eq!(movie.rating_key.as_deref(), Some("rating-redacted-1"));
        assert_eq!(part.key.as_deref(), Some("/library/parts/part-redacted/file.mkv"));
        assert!(part.file.as_deref().is_some_and(|file| file.contains("/redacted/")));
        assert_eq!(movie.originally_available_at.as_deref(), Some("2024-01-02"));
        assert_eq!(movie.media[0].video_codec.as_deref(), Some("hevc"));
        assert_eq!(movie.media[0].audio_channels, Some(6));
        assert_eq!(movie.genres[0].tag.as_deref(), Some("Drama"));
        assert_eq!(movie.guids.len(), 2);
    }
}
