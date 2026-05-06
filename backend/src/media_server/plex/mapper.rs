use crate::media_server::plex::dto::{PlexDirectoryDto, PlexGuidDto, PlexMediaDto, PlexSectionDto, PlexVideoDto};
use crate::media_server::{
    MediaServerAudioTechnicalFacts, MediaServerDescriptiveFacts, MediaServerEpisode, MediaServerImageRef,
    MediaServerLibrary, MediaServerLibraryKind, MediaServerLibraryRef, MediaServerMovie, MediaServerProviderIdHint,
    MediaServerSeason, MediaServerSeries, MediaServerStreamRef, MediaServerTechnicalFacts,
    MediaServerVideoTechnicalFacts,
};
use shared::model::{MediaServerLibraryKindDto, MediaServerLibrarySelectorDto};
use std::cmp::Reverse;
use std::collections::HashSet;
use std::sync::Arc;

pub fn plex_section_to_library(
    input_name: &Arc<str>,
    server_id: &Arc<str>,
    section: &PlexSectionDto,
) -> Option<MediaServerLibrary> {
    let library_id = non_blank(section.key.as_deref())?;
    let name = non_blank(section.title.as_deref()).unwrap_or_else(|| library_id.clone());
    Some(MediaServerLibrary {
        reference: MediaServerLibraryRef {
            input_name: Arc::clone(input_name),
            server_id: Arc::clone(server_id),
            library_id,
        },
        name,
        kind: plex_library_kind(section.section_type.as_deref()),
    })
}

pub fn plex_section_matches_selector(section: &PlexSectionDto, selector: &MediaServerLibrarySelectorDto) -> bool {
    match selector {
        MediaServerLibrarySelectorDto::Name(name) => matches_trimmed(section.title.as_deref(), name),
        MediaServerLibrarySelectorDto::Detailed(details) => {
            let identity_matches = details.key.as_deref().is_some_and(|key| matches_trimmed(section.key.as_deref(), key))
                || details.id.as_deref().is_some_and(|id| matches_trimmed(section.key.as_deref(), id))
                || details.name.as_deref().is_some_and(|name| matches_trimmed(section.title.as_deref(), name));
            identity_matches && details.kind.is_none_or(|kind| plex_section_kind_matches(section.section_type.as_deref(), kind))
        }
    }
}

pub fn plex_video_to_movie(
    input_name: &Arc<str>,
    server_id: &Arc<str>,
    library_id: &Arc<str>,
    video: &PlexVideoDto,
) -> Option<MediaServerMovie> {
    let rating_key = non_blank(video.rating_key.as_deref())?;
    let title = non_blank(video.title.as_deref()).unwrap_or_else(|| Arc::<str>::from("Plex Movie"));
    let media = first_media(&video.media);

    Some(MediaServerMovie {
        input_name: Arc::clone(input_name),
        server_id: Arc::clone(server_id),
        library_id: Arc::clone(library_id),
        item_id: Arc::clone(&rating_key),
        title,
        year: video.year,
        release_date: non_blank(video.originally_available_at.as_deref()),
        source_version_hint: source_version_hint(video.updated_at, video.added_at),
        provider_hints: provider_hints(video.guid.as_deref(), &video.guids),
        descriptive_facts: video_descriptive_facts(video),
        technical_facts: media.and_then(media_technical_facts),
        stream_ref: plex_stream_ref(input_name, server_id, &rating_key, media),
        image_ref: plex_image_ref(input_name, server_id, &rating_key, [&video.thumb, &video.art]),
    })
}

pub fn plex_directory_to_series(
    input_name: &Arc<str>,
    server_id: &Arc<str>,
    library_id: &Arc<str>,
    directory: &PlexDirectoryDto,
) -> Option<MediaServerSeries> {
    let rating_key = non_blank(directory.rating_key.as_deref())?;
    let title = non_blank(directory.title.as_deref()).unwrap_or_else(|| Arc::<str>::from("Plex Series"));

    Some(MediaServerSeries {
        input_name: Arc::clone(input_name),
        server_id: Arc::clone(server_id),
        library_id: Arc::clone(library_id),
        item_id: Arc::clone(&rating_key),
        title,
        year: directory.year,
        release_date: non_blank(directory.originally_available_at.as_deref()),
        source_version_hint: source_version_hint(directory.updated_at, directory.added_at),
        provider_hints: provider_hints(directory.guid.as_deref(), &directory.guids),
        descriptive_facts: directory_descriptive_facts(directory),
        child_count: directory.child_count,
        episode_count: directory.leaf_count,
        image_ref: plex_image_ref(input_name, server_id, &rating_key, [&directory.thumb, &directory.art, &directory.theme]),
    })
}

pub fn plex_directory_to_season(
    input_name: &Arc<str>,
    server_id: &Arc<str>,
    library_id: &Arc<str>,
    directory: &PlexDirectoryDto,
) -> Option<MediaServerSeason> {
    let rating_key = non_blank(directory.rating_key.as_deref())?;
    let title = non_blank(directory.title.as_deref()).unwrap_or_else(|| Arc::<str>::from("Plex Season"));

    Some(MediaServerSeason {
        input_name: Arc::clone(input_name),
        server_id: Arc::clone(server_id),
        library_id: Arc::clone(library_id),
        item_id: Arc::clone(&rating_key),
        series_id: non_blank(directory.parent_rating_key.as_deref()),
        series_title: non_blank(directory.parent_title.as_deref()),
        title,
        season: directory.index,
        year: directory.year,
        release_date: non_blank(directory.originally_available_at.as_deref()),
        source_version_hint: source_version_hint(directory.updated_at, directory.added_at),
        provider_hints: provider_hints(directory.guid.as_deref(), &directory.guids),
        descriptive_facts: directory_descriptive_facts(directory),
        episode_count: directory.leaf_count,
        image_ref: plex_image_ref(input_name, server_id, &rating_key, [&directory.thumb, &directory.art]),
    })
}

pub fn plex_video_to_episode(
    input_name: &Arc<str>,
    server_id: &Arc<str>,
    library_id: &Arc<str>,
    video: &PlexVideoDto,
) -> Option<MediaServerEpisode> {
    let rating_key = non_blank(video.rating_key.as_deref())?;
    let title = non_blank(video.title.as_deref()).unwrap_or_else(|| Arc::<str>::from("Plex Episode"));
    let media = first_media(&video.media);

    Some(MediaServerEpisode {
        input_name: Arc::clone(input_name),
        server_id: Arc::clone(server_id),
        library_id: Arc::clone(library_id),
        item_id: Arc::clone(&rating_key),
        series_id: non_blank(video.grandparent_rating_key.as_deref()),
        series_title: non_blank(video.grandparent_title.as_deref()),
        title,
        season: video.parent_index,
        episode: video.index,
        release_date: non_blank(video.originally_available_at.as_deref()),
        source_version_hint: source_version_hint(video.updated_at, video.added_at),
        provider_hints: provider_hints(video.guid.as_deref(), &video.guids),
        descriptive_facts: video_descriptive_facts(video),
        technical_facts: media.and_then(media_technical_facts),
        stream_ref: plex_stream_ref(input_name, server_id, &rating_key, media),
        image_ref: plex_image_ref(input_name, server_id, &rating_key, [&video.thumb, &video.art]),
    })
}

fn plex_section_kind_matches(section_type: Option<&str>, expected: MediaServerLibraryKindDto) -> bool {
    let kind = plex_library_kind(section_type);
    match expected {
        MediaServerLibraryKindDto::Movies => matches!(kind, MediaServerLibraryKind::Movies),
        MediaServerLibraryKindDto::TvShows => matches!(kind, MediaServerLibraryKind::TvShows),
    }
}

fn plex_library_kind(section_type: Option<&str>) -> MediaServerLibraryKind {
    match section_type.map(str::trim) {
        Some(value) if value.eq_ignore_ascii_case("movie") => MediaServerLibraryKind::Movies,
        Some(value) if value.eq_ignore_ascii_case("show") => MediaServerLibraryKind::TvShows,
        _ => MediaServerLibraryKind::Unsupported,
    }
}

fn matches_trimmed(candidate: Option<&str>, expected: &str) -> bool {
    candidate.is_some_and(|candidate| candidate.trim() == expected.trim())
}

fn first_media(media: &[PlexMediaDto]) -> Option<&PlexMediaDto> {
    media
        .iter()
        .enumerate()
        .filter(|(_, media)| media_has_catalog_facts(media))
        .max_by_key(|(index, media)| media_preference_key(media, *index))
        .map(|(_, media)| media)
}

fn media_preference_key(media: &PlexMediaDto, index: usize) -> (bool, u32, u64, Reverse<usize>) {
    (
        first_part_key(media).is_some(),
        media.bitrate.unwrap_or_default(),
        media_resolution_pixels(media),
        Reverse(index),
    )
}

fn media_resolution_pixels(media: &PlexMediaDto) -> u64 {
    u64::from(media.width.unwrap_or_default()) * u64::from(media.height.unwrap_or_default())
}

fn media_has_catalog_facts(media: &PlexMediaDto) -> bool {
    media.container.as_deref().is_some_and(|value| !value.trim().is_empty())
        || media.duration.is_some()
        || media.bitrate.is_some()
        || media.width.is_some()
        || media.height.is_some()
        || media.video_codec.as_deref().is_some_and(|value| !value.trim().is_empty())
        || media.audio_codec.as_deref().is_some_and(|value| !value.trim().is_empty())
        || media.audio_channels.is_some()
        || first_part_key(media).is_some()
}

fn plex_stream_ref(
    input_name: &Arc<str>,
    server_id: &Arc<str>,
    rating_key: &Arc<str>,
    media: Option<&PlexMediaDto>,
) -> Option<MediaServerStreamRef> {
    Some(MediaServerStreamRef::Plex {
        input_name: Arc::clone(input_name),
        server_id: Arc::clone(server_id),
        rating_key: Arc::clone(rating_key),
        part_key: first_part_key(media?)?,
    })
}

fn first_part_key(media: &PlexMediaDto) -> Option<Arc<str>> {
    media.parts.iter().find_map(|part| non_blank(part.key.as_deref()))
}

fn plex_image_ref<const N: usize>(
    input_name: &Arc<str>,
    server_id: &Arc<str>,
    rating_key: &Arc<str>,
    candidates: [&Option<String>; N],
) -> Option<MediaServerImageRef> {
    let image_path = candidates.into_iter().find_map(|candidate| non_blank(candidate.as_deref()))?;
    Some(MediaServerImageRef::Plex {
        input_name: Arc::clone(input_name),
        server_id: Arc::clone(server_id),
        rating_key: Arc::clone(rating_key),
        image_path,
    })
}

fn video_descriptive_facts(video: &PlexVideoDto) -> Option<MediaServerDescriptiveFacts> {
    let facts = MediaServerDescriptiveFacts {
        original_title: non_blank(video.original_title.as_deref()),
        sort_title: non_blank(video.title_sort.as_deref()),
        summary: non_blank(video.summary.as_deref()),
        tagline: non_blank(video.tagline.as_deref()),
        studio: non_blank(video.studio.as_deref()),
        content_rating: non_blank(video.content_rating.as_deref()),
        parental_age: video.content_rating_age,
        audience_rating: non_blank(video.audience_rating.as_deref()),
        genres: tag_values(&video.genres),
        countries: tag_values(&video.countries),
        directors: tag_values(&video.directors),
        writers: tag_values(&video.writers),
        cast: tag_values(&video.roles),
        ..MediaServerDescriptiveFacts::default()
    };
    has_descriptive_facts(&facts).then_some(facts)
}

fn directory_descriptive_facts(directory: &PlexDirectoryDto) -> Option<MediaServerDescriptiveFacts> {
    let facts = MediaServerDescriptiveFacts {
        original_title: non_blank(directory.original_title.as_deref()),
        sort_title: non_blank(directory.title_sort.as_deref()),
        summary: non_blank(directory.summary.as_deref()),
        tagline: non_blank(directory.tagline.as_deref()),
        studio: non_blank(directory.studio.as_deref()),
        content_rating: non_blank(directory.content_rating.as_deref()),
        parental_age: directory.content_rating_age,
        audience_rating: non_blank(directory.audience_rating.as_deref()),
        genres: tag_values(&directory.genres),
        countries: tag_values(&directory.countries),
        cast: tag_values(&directory.roles),
        ..MediaServerDescriptiveFacts::default()
    };
    has_descriptive_facts(&facts).then_some(facts)
}

fn has_descriptive_facts(facts: &MediaServerDescriptiveFacts) -> bool {
    facts.original_title.is_some()
        || facts.sort_title.is_some()
        || facts.summary.is_some()
        || facts.tagline.is_some()
        || facts.studio.is_some()
        || facts.network.is_some()
        || facts.content_rating.is_some()
        || facts.parental_age.is_some()
        || facts.audience_rating.is_some()
        || !facts.genres.is_empty()
        || !facts.countries.is_empty()
        || !facts.directors.is_empty()
        || !facts.writers.is_empty()
        || !facts.cast.is_empty()
        || !facts.crew.is_empty()
}

fn media_technical_facts(media: &PlexMediaDto) -> Option<MediaServerTechnicalFacts> {
    let video = MediaServerVideoTechnicalFacts {
        codec: non_blank(media.video_codec.as_deref()),
        width: media.width,
        height: media.height,
    };
    let audio = MediaServerAudioTechnicalFacts {
        codec: non_blank(media.audio_codec.as_deref()),
        channels: media.audio_channels,
    };
    let facts = MediaServerTechnicalFacts {
        container: non_blank(media.container.as_deref()).or_else(|| first_part_container(media)),
        duration_secs: plex_duration_ms_to_secs(media.duration),
        bitrate: media.bitrate.and_then(plex_bitrate_kbps_to_bps),
        video: has_video_facts(&video).then_some(video),
        audio: has_audio_facts(&audio).then_some(audio),
    };
    has_technical_facts(&facts).then_some(facts)
}

fn first_part_container(media: &PlexMediaDto) -> Option<Arc<str>> {
    media.parts.iter().find_map(|part| non_blank(part.container.as_deref()))
}

fn has_video_facts(facts: &MediaServerVideoTechnicalFacts) -> bool {
    facts.codec.is_some() || facts.width.is_some() || facts.height.is_some()
}

fn has_audio_facts(facts: &MediaServerAudioTechnicalFacts) -> bool {
    facts.codec.is_some() || facts.channels.is_some()
}

fn has_technical_facts(facts: &MediaServerTechnicalFacts) -> bool {
    facts.container.is_some()
        || facts.duration_secs.is_some()
        || facts.bitrate.is_some()
        || facts.video.is_some()
        || facts.audio.is_some()
}

fn plex_duration_ms_to_secs(duration: Option<u64>) -> Option<u32> {
    let duration_ms = duration?;
    let rounded_secs = duration_ms.saturating_add(500) / 1000;
    u32::try_from(rounded_secs).ok().filter(|value| *value > 0)
}

fn plex_bitrate_kbps_to_bps(bitrate_kbps: u32) -> Option<u32> {
    bitrate_kbps.checked_mul(1000).filter(|value| *value > 0)
}

fn provider_hints(guid: Option<&str>, guids: &[PlexGuidDto]) -> Vec<MediaServerProviderIdHint> {
    let mut seen = HashSet::<(String, String)>::new();
    let mut hints = Vec::new();

    for candidate in guid.into_iter().chain(guids.iter().filter_map(|guid| guid.id.as_deref())) {
        let Some((namespace, value)) = parse_guid(candidate) else { continue };
        if seen.insert((namespace.to_string(), value.to_string())) {
            hints.push(MediaServerProviderIdHint { namespace, value });
        }
    }

    hints
}

fn parse_guid(value: &str) -> Option<(Arc<str>, Arc<str>)> {
    let value = value.trim();
    let (namespace, id) = value.split_once("://")?;
    let namespace = normalize_guid_namespace(namespace)?;
    let id = id.split('?').next().unwrap_or(id).trim();
    if id.is_empty() {
        return None;
    }
    Some((namespace, Arc::<str>::from(id)))
}

fn normalize_guid_namespace(namespace: &str) -> Option<Arc<str>> {
    let namespace = namespace.trim().to_ascii_lowercase();
    let namespace = namespace.strip_prefix("com.plexapp.agents.").unwrap_or(&namespace);
    let namespace = match namespace {
        "themoviedb" => "tmdb",
        "thetvdb" => "tvdb",
        value => value,
    };
    (!namespace.is_empty()).then(|| Arc::<str>::from(namespace))
}

fn tag_values(tags: &[crate::media_server::plex::dto::PlexTagDto]) -> Vec<Arc<str>> {
    tags.iter().filter_map(crate::media_server::plex::dto::PlexTagDto::value).collect()
}

fn non_blank(value: Option<&str>) -> Option<Arc<str>> {
    value.map(str::trim).filter(|value| !value.is_empty()).map(Arc::<str>::from)
}

fn source_version_hint(updated_at: Option<i64>, added_at: Option<i64>) -> Option<Arc<str>> {
    updated_at.or(added_at).filter(|value| *value > 0).map(|value| Arc::<str>::from(value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_server::plex::dto::{PlexMediaContainerDto, PlexMediaDto, PlexPartDto};
    use crate::media_server::test_fixtures::{PLEX_EPISODES_XML, PLEX_MOVIES_XML, PLEX_SEASONS_XML, PLEX_SHOWS_XML};

    fn input_name() -> Arc<str> { Arc::<str>::from("media_server") }
    fn server_id() -> Arc<str> { Arc::<str>::from("machine-redacted") }
    fn library_id() -> Arc<str> { Arc::<str>::from("library-redacted") }

    #[test]
    fn maps_plex_movie_catalog_row_to_media_server_movie_without_leaking_part_file() {
        let container: PlexMediaContainerDto = quick_xml::de::from_str(PLEX_MOVIES_XML).expect("fixture parses");
        let movie = plex_video_to_movie(&input_name(), &server_id(), &library_id(), &container.videos[0])
            .expect("movie maps");

        assert_eq!(movie.item_id.as_ref(), "rating-redacted-1");
        assert_eq!(movie.title.as_ref(), "Movie Redacted");
        assert_eq!(movie.year, Some(2024));
        assert_eq!(movie.release_date.as_deref(), Some("2024-01-02"));
        assert_eq!(movie.source_version_hint.as_deref(), Some("1700000001"));
        assert!(movie.provider_hints.iter().any(|hint| hint.namespace.as_ref() == "tmdb" && hint.value.as_ref() == "12345"));
        assert!(movie.provider_hints.iter().any(|hint| hint.namespace.as_ref() == "imdb" && hint.value.as_ref() == "tt-redacted"));

        let descriptive = movie.descriptive_facts.as_ref().expect("descriptive facts");
        assert_eq!(descriptive.original_title.as_deref(), Some("Original Movie Redacted"));
        assert_eq!(descriptive.summary.as_deref(), Some("Movie summary redacted"));
        assert_eq!(descriptive.content_rating.as_deref(), Some("PG-13"));
        assert_eq!(descriptive.parental_age, Some(13));
        assert_eq!(descriptive.audience_rating.as_deref(), Some("8.2"));
        assert_eq!(descriptive.genres.iter().map(AsRef::as_ref).collect::<Vec<_>>(), vec!["Drama"]);
        assert_eq!(descriptive.directors.iter().map(AsRef::as_ref).collect::<Vec<_>>(), vec!["Director Redacted"]);
        assert_eq!(descriptive.cast.iter().map(AsRef::as_ref).collect::<Vec<_>>(), vec!["Actor Redacted"]);

        let technical = movie.technical_facts.as_ref().expect("technical facts");
        assert_eq!(technical.container.as_deref(), Some("mkv"));
        assert_eq!(technical.duration_secs, Some(7200));
        assert_eq!(technical.bitrate, Some(8_000_000));
        assert_eq!(technical.video.as_ref().and_then(|video| video.codec.as_deref()), Some("hevc"));
        assert_eq!(technical.video.as_ref().and_then(|video| video.width), Some(1920));
        assert_eq!(technical.audio.as_ref().and_then(|audio| audio.codec.as_deref()), Some("eac3"));
        assert_eq!(technical.audio.as_ref().and_then(|audio| audio.channels), Some(6));

        assert_eq!(
            movie.stream_ref,
            Some(MediaServerStreamRef::Plex {
                input_name: input_name(),
                server_id: server_id(),
                rating_key: "rating-redacted-1".into(),
                part_key: "/library/parts/part-redacted/file.mkv".into(),
            })
        );
        assert!(format!("{movie:?}").contains("/library/parts/part-redacted/file.mkv"));
        assert!(!format!("{movie:?}").contains("/redacted/upstream/path"));
    }

    #[test]
    fn maps_plex_show_and_season_rows_as_catalog_anchors() {
        let shows: PlexMediaContainerDto = quick_xml::de::from_str(PLEX_SHOWS_XML).expect("show fixture parses");
        let seasons: PlexMediaContainerDto = quick_xml::de::from_str(PLEX_SEASONS_XML).expect("season fixture parses");

        let series = plex_directory_to_series(&input_name(), &server_id(), &library_id(), &shows.directories[0])
            .expect("series maps");
        assert_eq!(series.item_id.as_ref(), "series-redacted-1");
        assert_eq!(series.title.as_ref(), "Show Redacted");
        assert_eq!(series.episode_count, Some(2));
        assert_eq!(series.child_count, Some(1));
        assert!(series.provider_hints.iter().any(|hint| hint.namespace.as_ref() == "tmdb" && hint.value.as_ref() == "222"));
        let series_facts = series.descriptive_facts.as_ref().expect("series descriptive facts");
        assert_eq!(series_facts.summary.as_deref(), Some("Show summary redacted"));
        assert_eq!(series_facts.genres.iter().map(AsRef::as_ref).collect::<Vec<_>>(), vec!["Mystery"]);
        assert!(matches!(series.image_ref, Some(MediaServerImageRef::Plex { .. })));

        let season = plex_directory_to_season(&input_name(), &server_id(), &library_id(), &seasons.directories[0])
            .expect("season maps");
        assert_eq!(season.item_id.as_ref(), "season-redacted-1");
        assert_eq!(season.series_id.as_deref(), Some("series-redacted-1"));
        assert_eq!(season.series_title.as_deref(), Some("Show Redacted"));
        assert_eq!(season.season, Some(1));
        assert_eq!(season.episode_count, Some(2));
        assert!(season.provider_hints.iter().any(|hint| hint.namespace.as_ref() == "tvdb" && hint.value.as_ref() == "333"));
    }

    #[test]
    fn maps_plex_episode_catalog_row_to_playable_episode() {
        let container: PlexMediaContainerDto = quick_xml::de::from_str(PLEX_EPISODES_XML).expect("fixture parses");
        let episode = plex_video_to_episode(&input_name(), &server_id(), &library_id(), &container.videos[0])
            .expect("episode maps");

        assert_eq!(episode.item_id.as_ref(), "episode-redacted-1");
        assert_eq!(episode.series_id.as_deref(), Some("series-redacted-1"));
        assert_eq!(episode.series_title.as_deref(), Some("Show Redacted"));
        assert_eq!(episode.season, Some(1));
        assert_eq!(episode.episode, Some(2));
        assert_eq!(episode.release_date.as_deref(), Some("2024-02-03"));
        assert!(episode.provider_hints.iter().any(|hint| hint.namespace.as_ref() == "tmdb" && hint.value.as_ref() == "67890"));
        assert!(!episode.provider_hints.iter().any(|hint| hint.value.as_ref() == "222"));
        assert_eq!(
            episode.stream_ref,
            Some(MediaServerStreamRef::Plex {
                input_name: input_name(),
                server_id: server_id(),
                rating_key: "episode-redacted-1".into(),
                part_key: "/library/parts/episode-part-redacted/file.mkv".into(),
            })
        );
    }

    #[test]
    fn skips_rows_without_rating_key_and_keeps_identity_without_part_key() {
        let mut container: PlexMediaContainerDto = quick_xml::de::from_str(PLEX_MOVIES_XML).expect("fixture parses");
        container.videos[0].rating_key = None;
        assert!(plex_video_to_movie(&input_name(), &server_id(), &library_id(), &container.videos[0]).is_none());

        let mut container: PlexMediaContainerDto = quick_xml::de::from_str(PLEX_MOVIES_XML).expect("fixture parses");
        container.videos[0].media[0].parts[0].key = None;
        let movie = plex_video_to_movie(&input_name(), &server_id(), &library_id(), &container.videos[0])
            .expect("movie identity remains valid");
        assert!(movie.stream_ref.is_none());
        assert!(movie.technical_facts.is_some());
    }

    #[test]
    fn first_media_prefers_best_deterministic_playable_version() {
        let low_bitrate = media_with_part("low", Some(1_000), Some(1920), Some(1080));
        let high_bitrate = media_with_part("high", Some(4_000), Some(1280), Some(720));
        let high_resolution = media_with_part("resolution", Some(4_000), Some(3840), Some(2160));
        let metadata_only = PlexMediaDto {
            id: Some("metadata".to_string()),
            container: Some("mkv".to_string()),
            duration: None,
            bitrate: Some(10_000),
            width: Some(7680),
            height: Some(4320),
            audio_channels: None,
            audio_codec: None,
            video_codec: None,
            video_resolution: None,
            parts: Vec::new(),
        };
        let media = vec![metadata_only, low_bitrate, high_resolution, high_bitrate];

        let preferred = first_media(&media).expect("preferred media");

        assert_eq!(preferred.parts[0].key.as_deref(), Some("/library/parts/resolution/file.mkv"));
    }

    fn media_with_part(id: &str, bitrate: Option<u32>, width: Option<u32>, height: Option<u32>) -> PlexMediaDto {
        PlexMediaDto {
            id: Some(id.to_string()),
            container: Some("mkv".to_string()),
            duration: None,
            bitrate,
            width,
            height,
            audio_channels: None,
            audio_codec: None,
            video_codec: None,
            video_resolution: None,
            parts: vec![PlexPartDto {
                id: Some(id.to_string()),
                key: Some(format!("/library/parts/{id}/file.mkv")),
                size: None,
                file: Some("/redacted/upstream/path/file.mkv".to_string()),
                container: Some("mkv".to_string()),
            }],
        }
    }

    #[test]
    fn parses_guid_hints_case_normalized_and_ignores_malformed_values() {
        let hints = provider_hints(
            Some("TmDb://123?lang=en"),
            &[
                PlexGuidDto { id: Some("com.plexapp.agents.imdb://tt-redacted?lang=en".to_string()) },
                PlexGuidDto { id: Some("malformed".to_string()) },
                PlexGuidDto { id: Some("tmdb://123".to_string()) },
                PlexGuidDto { id: Some("tvdb://  ".to_string()) },
                PlexGuidDto { id: Some("com.plexapp.agents.themoviedb://456".to_string()) },
                PlexGuidDto { id: Some("com.plexapp.agents.thetvdb://789?lang=en".to_string()) },
            ],
        );

        assert_eq!(
            hints.iter().map(|hint| (hint.namespace.as_ref(), hint.value.as_ref())).collect::<Vec<_>>(),
            vec![
                ("tmdb", "123"),
                ("imdb", "tt-redacted"),
                ("tmdb", "456"),
                ("tvdb", "789")
            ]
        );
    }
}
