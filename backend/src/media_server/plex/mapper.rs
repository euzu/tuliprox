use crate::media_server::{
    plex::dto::{PlexGuidDto, PlexSectionDto, PlexVideoDto}, MediaServerEpisode, MediaServerImageRef,
    MediaServerLibrary, MediaServerLibraryKind, MediaServerLibraryRef, MediaServerMovie, MediaServerProviderIdHint,
    MediaServerStreamRef,
};
use std::{collections::HashSet, sync::Arc};

pub fn map_plex_section(
    input_name: &Arc<str>,
    server_id: &Arc<str>,
    section: &PlexSectionDto,
) -> Option<MediaServerLibrary> {
    let library_id = non_blank(section.key.as_deref())?;
    let kind = match section.section_type.as_deref().map(str::trim) {
        Some("movie") => MediaServerLibraryKind::Movies,
        Some("show") => MediaServerLibraryKind::TvShows,
        _ => MediaServerLibraryKind::Unsupported,
    };

    Some(MediaServerLibrary {
        reference: MediaServerLibraryRef {
            input_name: input_name.clone(),
            server_id: server_id.clone(),
            library_id: Arc::<str>::from(library_id),
        },
        name: Arc::<str>::from(non_blank(section.title.as_deref()).unwrap_or("Plex Library")),
        kind,
    })
}

pub fn map_plex_movie(
    input_name: &Arc<str>,
    server_id: &Arc<str>,
    library_id: &Arc<str>,
    video: &PlexVideoDto,
) -> Option<MediaServerMovie> {
    let rating_key = non_blank(video.rating_key.as_deref())?;
    Some(MediaServerMovie {
        input_name: input_name.clone(),
        server_id: server_id.clone(),
        library_id: library_id.clone(),
        item_id: Arc::<str>::from(rating_key),
        title: Arc::<str>::from(non_blank(video.title.as_deref()).unwrap_or("Plex Movie")),
        year: video.year,
        source_version_hint: source_version_hint(video),
        provider_hints: provider_hints(video),
        stream_ref: first_part_key(video).map(|part_key| MediaServerStreamRef::Plex {
            input_name: input_name.clone(),
            server_id: server_id.clone(),
            rating_key: Arc::<str>::from(rating_key),
            part_key: Arc::<str>::from(part_key),
        }),
        image_ref: first_image_path(video).map(|image_path| MediaServerImageRef::Plex {
            input_name: input_name.clone(),
            server_id: server_id.clone(),
            rating_key: Arc::<str>::from(rating_key),
            image_path: Arc::<str>::from(image_path),
        }),
    })
}

pub fn map_plex_episode(
    input_name: &Arc<str>,
    server_id: &Arc<str>,
    library_id: &Arc<str>,
    video: &PlexVideoDto,
) -> Option<MediaServerEpisode> {
    let rating_key = non_blank(video.rating_key.as_deref())?;
    Some(MediaServerEpisode {
        input_name: input_name.clone(),
        server_id: server_id.clone(),
        library_id: library_id.clone(),
        item_id: Arc::<str>::from(rating_key),
        series_id: non_blank(video.grandparent_rating_key.as_deref()).map(Arc::<str>::from),
        series_title: non_blank(video.grandparent_title.as_deref()).map(Arc::<str>::from),
        title: Arc::<str>::from(non_blank(video.title.as_deref()).unwrap_or("Plex Episode")),
        season: video.parent_index,
        episode: video.index,
        source_version_hint: source_version_hint(video),
        provider_hints: provider_hints(video),
        stream_ref: first_part_key(video).map(|part_key| MediaServerStreamRef::Plex {
            input_name: input_name.clone(),
            server_id: server_id.clone(),
            rating_key: Arc::<str>::from(rating_key),
            part_key: Arc::<str>::from(part_key),
        }),
        image_ref: first_image_path(video).map(|image_path| MediaServerImageRef::Plex {
            input_name: input_name.clone(),
            server_id: server_id.clone(),
            rating_key: Arc::<str>::from(rating_key),
            image_path: Arc::<str>::from(image_path),
        }),
    })
}

fn non_blank(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|candidate| !candidate.is_empty())
}

fn first_part_key(video: &PlexVideoDto) -> Option<&str> {
    video
        .media
        .iter()
        .flat_map(|media| media.parts.iter())
        .find_map(|part| non_blank(part.key.as_deref()))
}

fn first_image_path(video: &PlexVideoDto) -> Option<&str> {
    non_blank(video.thumb.as_deref()).or_else(|| non_blank(video.art.as_deref()))
}

fn source_version_hint(video: &PlexVideoDto) -> Option<Arc<str>> {
    video.updated_at.or(video.added_at).map(|timestamp| Arc::<str>::from(timestamp.to_string()))
}

fn provider_hints(video: &PlexVideoDto) -> Vec<MediaServerProviderIdHint> {
    let mut seen = HashSet::<(Arc<str>, Arc<str>)>::new();
    let mut hints = Vec::new();
    if let Some(hint) = non_blank(video.guid.as_deref()).and_then(guid_to_hint) {
        seen.insert((hint.namespace.clone(), hint.value.clone()));
        hints.push(hint);
    }
    for hint in guid_children_to_hints(&video.guids) {
        if seen.insert((hint.namespace.clone(), hint.value.clone())) {
            hints.push(hint);
        }
    }
    hints
}

fn guid_children_to_hints(guids: &[PlexGuidDto]) -> Vec<MediaServerProviderIdHint> {
    guids.iter().filter_map(|guid| non_blank(guid.id.as_deref())).filter_map(guid_to_hint).collect()
}

fn guid_to_hint(guid: &str) -> Option<MediaServerProviderIdHint> {
    let (namespace, value) = guid.split_once("://")?;
    let namespace = namespace.trim();
    let value = value.trim();
    if namespace.is_empty() || value.is_empty() {
        return None;
    }
    Some(MediaServerProviderIdHint { namespace: Arc::<str>::from(namespace), value: Arc::<str>::from(value) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_server::{plex::dto::PlexMediaContainerDto, test_fixtures::{PLEX_EPISODES_XML, PLEX_MOVIES_WITH_MALFORMED_ROW_XML, PLEX_MOVIES_XML}};

    #[test]
    fn maps_plex_movie_without_leaking_part_file() {
        let container: PlexMediaContainerDto = quick_xml::de::from_str(PLEX_MOVIES_XML).expect("fixture parses");
        let input_name = Arc::<str>::from("plex_media_server");
        let server_id = Arc::<str>::from("server-redacted");
        let library_id = Arc::<str>::from("library-redacted");

        let movie = map_plex_movie(&input_name, &server_id, &library_id, &container.videos[0]).expect("movie maps");

        assert_eq!(movie.item_id.as_ref(), "rating-redacted-1");
        assert!(matches!(movie.stream_ref, Some(MediaServerStreamRef::Plex { .. })));
        assert!(movie.provider_hints.iter().any(|hint| hint.namespace.as_ref() == "tmdb"));
        let debug = format!("{movie:?}");
        assert!(!debug.contains("/redacted/upstream/path"));
        assert!(!debug.contains("resource-token"));
    }

    #[test]
    fn maps_plex_episode_relationship_hints() {
        let container: PlexMediaContainerDto = quick_xml::de::from_str(PLEX_EPISODES_XML).expect("fixture parses");
        let input_name = Arc::<str>::from("plex_media_server");
        let server_id = Arc::<str>::from("server-redacted");
        let library_id = Arc::<str>::from("library-redacted");

        let episode = map_plex_episode(&input_name, &server_id, &library_id, &container.videos[0]).expect("episode maps");

        assert_eq!(episode.item_id.as_ref(), "episode-redacted-1");
        assert_eq!(episode.series_id.as_deref(), Some("series-redacted-1"));
        assert_eq!(episode.season, Some(1));
        assert_eq!(episode.episode, Some(2));
        assert!(episode.provider_hints.iter().any(|hint| hint.namespace.as_ref() == "tvdb"));
    }

    #[test]
    fn missing_rating_key_does_not_create_unknown_identity() {
        let container: PlexMediaContainerDto =
            quick_xml::de::from_str(PLEX_MOVIES_WITH_MALFORMED_ROW_XML).expect("fixture parses");
        let input_name = Arc::<str>::from("plex_media_server");
        let server_id = Arc::<str>::from("server-redacted");
        let library_id = Arc::<str>::from("library-redacted");

        let movies: Vec<_> = container
            .videos
            .iter()
            .filter_map(|video| map_plex_movie(&input_name, &server_id, &library_id, video))
            .collect();

        assert_eq!(container.upstream_item_count(), 2);
        assert_eq!(movies.len(), 1);
        assert_eq!(movies[0].item_id.as_ref(), "rating-redacted-2");
    }
}
