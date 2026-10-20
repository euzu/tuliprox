use crate::{
    emby::{
        dto::{EmbyItemDto, EmbyMediaSourceDto, EmbyMediaStreamDto, EmbyPersonDto, EmbyViewDto, EmbyVirtualFolderDto},
        flavor::EmbyFamilyFlavor,
    },
    library_selection::MediaServerLibraryCandidate,
    non_blank_arc, MediaServerAudioTechnicalFacts, MediaServerDescriptiveFacts, MediaServerEpisode,
    MediaServerImageRef, MediaServerMovie, MediaServerProviderIdHint, MediaServerSeason, MediaServerSeries,
    MediaServerStreamRef, MediaServerTechnicalFacts, MediaServerVideoTechnicalFacts,
};
use chrono::{DateTime, NaiveDate};
use shared::model::MediaServerLibraryKind;
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock},
};

const TICKS_PER_SECOND: u64 = 10_000_000;
const PRIMARY_IMAGE_KIND: &str = "Primary";
/// Shared by every image reference, so mapping a large library only bumps a refcount.
static PRIMARY_IMAGE_KIND_ARC: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from(PRIMARY_IMAGE_KIND));

/// Identity of the library a page of items was requested from.
pub struct EmbyMapContext<'a> {
    pub input_name: &'a Arc<str>,
    pub server_id: &'a Arc<str>,
    pub library_id: &'a Arc<str>,
    /// When `false` the item list was requested without `MediaSources`, so the
    /// stream reference falls back to the server's default source.
    pub include_media_sources: bool,
}

pub fn emby_item_to_movie<F: EmbyFamilyFlavor>(
    ctx: &EmbyMapContext<'_>,
    item: &EmbyItemDto,
) -> Option<MediaServerMovie> {
    let base = ItemBase::new::<F>(ctx, item, "Movie")?;
    let source = chosen_source(ctx, item);
    Some(MediaServerMovie {
        input_name: Arc::clone(ctx.input_name),
        server_id: Arc::clone(ctx.server_id),
        library_id: Arc::clone(ctx.library_id),
        stream_ref: stream_ref::<F>(ctx, &base.item_id, source),
        item_id: base.item_id,
        title: base.title,
        year: item.production_year,
        release_date: base.release_date,
        source_version_hint: base.source_version_hint,
        provider_hints: base.provider_hints,
        descriptive_facts: descriptive_facts(item, false),
        technical_facts: technical_facts(source, item.run_time_ticks),
        image_ref: base.image_ref,
    })
}

pub fn emby_item_to_series<F: EmbyFamilyFlavor>(
    ctx: &EmbyMapContext<'_>,
    item: &EmbyItemDto,
) -> Option<MediaServerSeries> {
    let base = ItemBase::new::<F>(ctx, item, "Series")?;
    Some(MediaServerSeries {
        input_name: Arc::clone(ctx.input_name),
        server_id: Arc::clone(ctx.server_id),
        library_id: Arc::clone(ctx.library_id),
        item_id: base.item_id,
        title: base.title,
        year: item.production_year,
        release_date: base.release_date,
        source_version_hint: base.source_version_hint,
        provider_hints: base.provider_hints,
        descriptive_facts: descriptive_facts(item, true),
        child_count: item.child_count,
        episode_count: item.recursive_item_count,
        image_ref: base.image_ref,
    })
}

pub fn emby_item_to_season<F: EmbyFamilyFlavor>(
    ctx: &EmbyMapContext<'_>,
    item: &EmbyItemDto,
) -> Option<MediaServerSeason> {
    let base = ItemBase::new::<F>(ctx, item, "Season")?;
    Some(MediaServerSeason {
        input_name: Arc::clone(ctx.input_name),
        server_id: Arc::clone(ctx.server_id),
        library_id: Arc::clone(ctx.library_id),
        item_id: base.item_id,
        series_id: non_blank_arc(item.series_id.as_deref()),
        series_title: non_blank_arc(item.series_name.as_deref()),
        title: base.title,
        season: item.index_number,
        year: item.production_year,
        release_date: base.release_date,
        source_version_hint: base.source_version_hint,
        provider_hints: base.provider_hints,
        descriptive_facts: descriptive_facts(item, false),
        episode_count: item.child_count,
        image_ref: base.image_ref,
    })
}

pub fn emby_item_to_episode<F: EmbyFamilyFlavor>(
    ctx: &EmbyMapContext<'_>,
    item: &EmbyItemDto,
) -> Option<MediaServerEpisode> {
    let base = ItemBase::new::<F>(ctx, item, "Episode")?;
    let source = chosen_source(ctx, item);
    Some(MediaServerEpisode {
        input_name: Arc::clone(ctx.input_name),
        server_id: Arc::clone(ctx.server_id),
        library_id: Arc::clone(ctx.library_id),
        stream_ref: stream_ref::<F>(ctx, &base.item_id, source),
        item_id: base.item_id,
        series_id: non_blank_arc(item.series_id.as_deref()),
        series_title: non_blank_arc(item.series_name.as_deref()),
        title: base.title,
        season: item.parent_index_number,
        episode: item.index_number,
        release_date: base.release_date,
        source_version_hint: base.source_version_hint,
        provider_hints: base.provider_hints,
        descriptive_facts: descriptive_facts(item, false),
        technical_facts: technical_facts(source, item.run_time_ticks),
        image_ref: base.image_ref,
    })
}

/// Missing episodes (`LocationType: Virtual`) have no media and must not become playable entries.
pub fn is_virtual_item(item: &EmbyItemDto) -> bool {
    item.location_type.as_deref().is_some_and(|value| value.trim().eq_ignore_ascii_case("virtual"))
}

impl MediaServerLibraryCandidate for EmbyVirtualFolderDto {
    fn library_key(&self) -> Option<&str> {
        [self.item_id.as_deref(), self.id.as_deref()].into_iter().flatten().find(|value| !value.trim().is_empty())
    }

    fn library_title(&self) -> Option<&str> { self.name.as_deref() }

    fn library_kind(&self) -> MediaServerLibraryKind { emby_library_kind(self.collection_type.as_deref()) }
}

impl MediaServerLibraryCandidate for EmbyViewDto {
    fn library_key(&self) -> Option<&str> { Some(self.id.as_str()) }

    fn library_title(&self) -> Option<&str> { self.name.as_deref() }

    fn library_kind(&self) -> MediaServerLibraryKind { emby_library_kind(self.collection_type.as_deref()) }
}

pub(crate) fn emby_library_kind(collection_type: Option<&str>) -> MediaServerLibraryKind {
    match collection_type.map(str::trim) {
        Some(value) if value.eq_ignore_ascii_case("movies") => MediaServerLibraryKind::Movies,
        Some(value) if value.eq_ignore_ascii_case("tvshows") => MediaServerLibraryKind::TvShows,
        _ => MediaServerLibraryKind::Unsupported,
    }
}

/// Fields every item kind shares.
struct ItemBase {
    item_id: Arc<str>,
    title: Arc<str>,
    release_date: Option<Arc<str>>,
    source_version_hint: Option<Arc<str>>,
    provider_hints: Vec<MediaServerProviderIdHint>,
    image_ref: Option<MediaServerImageRef>,
}

impl ItemBase {
    fn new<F: EmbyFamilyFlavor>(ctx: &EmbyMapContext<'_>, item: &EmbyItemDto, noun: &str) -> Option<Self> {
        let item_id = non_blank_arc(Some(&item.id))?;
        let title = non_blank_arc(item.name.as_deref()).unwrap_or_else(|| fallback_title::<F>(noun));
        let image_ref = item.image_tags.get(PRIMARY_IMAGE_KIND).and_then(|tag| non_blank_arc(Some(tag))).map(|tag| {
            F::image_ref(ctx.input_name, ctx.server_id, &item_id, Arc::clone(&PRIMARY_IMAGE_KIND_ARC), Some(tag))
        });
        Some(Self {
            title,
            release_date: release_date(item.premiere_date.as_deref()),
            source_version_hint: source_version_hint(item.date_created.as_deref()),
            provider_hints: provider_hints(&item.provider_ids),
            image_ref,
            item_id,
        })
    }
}

fn fallback_title<F: EmbyFamilyFlavor>(noun: &str) -> Arc<str> {
    let mut chars = F::PROVIDER.chars();
    let mut title = String::with_capacity(F::PROVIDER.len() + 1 + noun.len());
    title.extend(chars.next().map(|first| first.to_ascii_uppercase()));
    title.push_str(chars.as_str());
    title.push(' ');
    title.push_str(noun);
    Arc::<str>::from(title)
}

fn release_date(premiere_date: Option<&str>) -> Option<Arc<str>> {
    let date = premiere_date?.trim().get(..10)?;
    NaiveDate::parse_from_str(date, "%Y-%m-%d").ok().map(|_| Arc::<str>::from(date))
}

/// Unix seconds of `DateCreated`, the same meaning as Plex `addedAt`.
fn source_version_hint(date_created: Option<&str>) -> Option<Arc<str>> {
    let seconds = DateTime::parse_from_rfc3339(date_created?.trim()).ok()?.timestamp();
    (seconds > 0).then(|| Arc::<str>::from(seconds.to_string()))
}

fn provider_hints(provider_ids: &HashMap<String, String>) -> Vec<MediaServerProviderIdHint> {
    let mut hints: Vec<MediaServerProviderIdHint> = provider_ids
        .iter()
        .filter_map(|(namespace, value)| {
            let namespace = namespace.trim();
            let value = non_blank_arc(Some(value))?;
            (!namespace.is_empty()).then(|| MediaServerProviderIdHint {
                namespace: Arc::<str>::from(namespace.to_ascii_lowercase()),
                value,
            })
        })
        .collect();
    hints.sort_by(|a, b| a.namespace.cmp(&b.namespace).then_with(|| a.value.cmp(&b.value)));
    hints
}

fn descriptive_facts(item: &EmbyItemDto, studio_is_network: bool) -> Option<MediaServerDescriptiveFacts> {
    let first_studio = item.studios.iter().find_map(|studio| non_blank_arc(studio.name.as_deref()));
    let PeopleByRole { directors, writers, cast } = partition_people(&item.people);
    let (studio, network) = if studio_is_network { (None, first_studio) } else { (first_studio, None) };
    let facts = MediaServerDescriptiveFacts {
        original_title: non_blank_arc(item.original_title.as_deref()),
        sort_title: non_blank_arc(item.sort_name.as_deref()),
        summary: non_blank_arc(item.overview.as_deref()),
        tagline: item.taglines.iter().find_map(|tagline| non_blank_arc(Some(tagline))),
        studio,
        network,
        content_rating: non_blank_arc(item.official_rating.as_deref()),
        audience_rating: item.community_rating.map(|rating| Arc::<str>::from(format!("{rating:.1}"))),
        genres: item.genres.iter().filter_map(|genre| non_blank_arc(Some(genre))).collect(),
        countries: item.production_locations.iter().filter_map(|country| non_blank_arc(Some(country))).collect(),
        directors,
        writers,
        cast,
        ..MediaServerDescriptiveFacts::default()
    };
    (!facts.is_empty()).then_some(facts)
}

#[derive(Default)]
struct PeopleByRole {
    directors: Vec<Arc<str>>,
    writers: Vec<Arc<str>>,
    cast: Vec<Arc<str>>,
}

/// Directors, writers and cast in one pass over `People`; other roles are dropped.
fn partition_people(people: &[EmbyPersonDto]) -> PeopleByRole {
    let mut roles = PeopleByRole::default();
    for person in people {
        let Some(kind) = person.type_.as_deref().map(str::trim) else { continue };
        let target = if kind.eq_ignore_ascii_case("Director") {
            &mut roles.directors
        } else if kind.eq_ignore_ascii_case("Writer") {
            &mut roles.writers
        } else if kind.eq_ignore_ascii_case("Actor") || kind.eq_ignore_ascii_case("GuestStar") {
            &mut roles.cast
        } else {
            continue;
        };
        target.extend(non_blank_arc(person.name.as_deref()));
    }
    roles
}

/// A source Tuliprox can proxy: a local file, never a remote/`.strm` entry that could loop back into Tuliprox.
fn is_eligible_source(source: &EmbyMediaSourceDto) -> bool {
    source.is_remote != Some(true)
        && source
            .protocol
            .as_deref()
            .map(str::trim)
            .is_none_or(|protocol| protocol.is_empty() || protocol.eq_ignore_ascii_case("file"))
}

fn supports_direct_access(source: &EmbyMediaSourceDto) -> bool {
    source.supports_direct_play == Some(true) || source.supports_direct_stream == Some(true)
}

fn choose_source(sources: &[EmbyMediaSourceDto]) -> Option<&EmbyMediaSourceDto> {
    let mut eligible = sources.iter().filter(|source| is_eligible_source(source));
    let first = eligible.next()?;
    if supports_direct_access(first) {
        return Some(first);
    }
    Some(eligible.find(|source| supports_direct_access(source)).unwrap_or(first))
}

/// The source technical facts are taken from; `None` when sources were not requested.
fn chosen_source<'a>(ctx: &EmbyMapContext<'_>, item: &'a EmbyItemDto) -> Option<&'a EmbyMediaSourceDto> {
    if ctx.include_media_sources {
        choose_source(&item.media_sources)
    } else {
        None
    }
}

fn stream_ref<F: EmbyFamilyFlavor>(
    ctx: &EmbyMapContext<'_>,
    item_id: &Arc<str>,
    source: Option<&EmbyMediaSourceDto>,
) -> Option<MediaServerStreamRef> {
    let media_source_id = if ctx.include_media_sources { non_blank_arc(source?.id.as_deref()) } else { None };
    Some(F::stream_ref(ctx.input_name, ctx.server_id, item_id, media_source_id))
}

fn technical_facts(source: Option<&EmbyMediaSourceDto>, item_ticks: Option<u64>) -> Option<MediaServerTechnicalFacts> {
    let streams = source.map_or(&[][..], |source| source.media_streams.as_slice());
    let video = first_stream(streams, "video").map(|stream| MediaServerVideoTechnicalFacts {
        codec: non_blank_arc(stream.codec.as_deref()),
        width: stream.width,
        height: stream.height,
    });
    let audio = first_stream(streams, "audio").map(|stream| MediaServerAudioTechnicalFacts {
        codec: non_blank_arc(stream.codec.as_deref()),
        channels: stream.channels,
    });
    let facts = MediaServerTechnicalFacts {
        container: source.and_then(|source| non_blank_arc(source.container.as_deref())),
        duration_secs: ticks_to_secs(source.and_then(|source| source.run_time_ticks).or(item_ticks)),
        bitrate: source.and_then(|source| source.bitrate).filter(|bitrate| *bitrate > 0),
        video: video.filter(|video| !video.is_empty()),
        audio: audio.filter(|audio| !audio.is_empty()),
    };
    (!facts.is_empty()).then_some(facts)
}

fn first_stream<'a>(streams: &'a [EmbyMediaStreamDto], stream_type: &str) -> Option<&'a EmbyMediaStreamDto> {
    streams
        .iter()
        .find(|stream| stream.type_.as_deref().is_some_and(|value| value.trim().eq_ignore_ascii_case(stream_type)))
}

fn ticks_to_secs(ticks: Option<u64>) -> Option<u32> {
    u32::try_from(ticks? / TICKS_PER_SECOND).ok().filter(|secs| *secs > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        emby::{dto::EmbyItemsPageDto, flavor::Emby},
        jellyfin::flavor::Jellyfin,
        media_server_image_ref_to_internal_url, media_server_stream_ref_to_internal_url, parse_media_server_image_ref,
        parse_media_server_stream_ref,
        test_fixtures::{
            EMBY_EPISODES_PAGE_JSON, EMBY_MOVIES_PAGE_JSON, EMBY_SEASONS_PAGE_JSON, EMBY_SERIES_PAGE_JSON,
            EMBY_VIRTUAL_FOLDERS_JSON, JELLYFIN_VIEWS_JSON,
        },
        MediaServerKind,
    };
    use shared::model::{MediaServerLibraryKind, MediaServerLibrarySelector};

    fn input_name() -> Arc<str> { Arc::<str>::from("media_server") }
    fn server_id() -> Arc<str> { Arc::<str>::from("server-redacted-id") }
    fn library_id() -> Arc<str> { Arc::<str>::from("library-redacted") }

    fn items(json: &str) -> Vec<EmbyItemDto> {
        serde_json::from_str::<EmbyItemsPageDto<EmbyItemDto>>(json).expect("fixture parses").items
    }

    fn bare_item(id: &str) -> EmbyItemDto {
        serde_json::from_value(serde_json::json!({ "Id": id })).expect("minimal item parses")
    }

    fn with_ctx<R>(include_media_sources: bool, run: impl FnOnce(&EmbyMapContext<'_>) -> R) -> R {
        let (input_name, server_id, library_id) = (input_name(), server_id(), library_id());
        run(&EmbyMapContext {
            input_name: &input_name,
            server_id: &server_id,
            library_id: &library_id,
            include_media_sources,
        })
    }

    fn strs(values: &[Arc<str>]) -> Vec<&str> { values.iter().map(AsRef::as_ref).collect() }

    fn check_movie_mapping<F: EmbyFamilyFlavor>() {
        let movie =
            with_ctx(true, |ctx| emby_item_to_movie::<F>(ctx, &items(EMBY_MOVIES_PAGE_JSON)[0])).expect("movie maps");

        assert_eq!(movie.item_id.as_ref(), "movie-redacted-1");
        assert_eq!(movie.title.as_ref(), "Movie Redacted");
        assert_eq!(movie.year, Some(2024));
        assert_eq!(movie.release_date.as_deref(), Some("2024-03-01"));
        assert_eq!(movie.source_version_hint.as_deref(), Some("1712052930"));
        assert_eq!(
            movie.provider_hints.iter().map(|h| (h.namespace.as_ref(), h.value.as_ref())).collect::<Vec<_>>(),
            vec![("imdb", "tt0000001"), ("tmdb", "12345")]
        );

        let descriptive = movie.descriptive_facts.as_ref().expect("descriptive facts");
        assert_eq!(descriptive.original_title.as_deref(), Some("Movie Redacted Original"));
        assert_eq!(descriptive.sort_title.as_deref(), Some("movie redacted"));
        assert_eq!(descriptive.summary.as_deref(), Some("Redacted overview text."));
        assert_eq!(descriptive.tagline.as_deref(), Some("Redacted tagline"));
        assert_eq!(descriptive.studio.as_deref(), Some("Studio Redacted"));
        assert!(descriptive.network.is_none());
        assert_eq!(descriptive.content_rating.as_deref(), Some("PG-13"));
        assert_eq!(descriptive.audience_rating.as_deref(), Some("7.4"));
        assert_eq!(strs(&descriptive.genres), vec!["Action", "Drama"]);
        assert_eq!(strs(&descriptive.countries), vec!["Redacted Country"]);
        assert_eq!(strs(&descriptive.directors), vec!["Director Redacted"]);
        assert_eq!(strs(&descriptive.cast), vec!["Actor Redacted"]);
        assert_eq!(descriptive.writers, Vec::<Arc<str>>::new());

        let technical = movie.technical_facts.as_ref().expect("technical facts");
        assert_eq!(technical.container.as_deref(), Some("mkv"));
        assert_eq!(technical.duration_secs, Some(7200));
        assert_eq!(technical.bitrate, Some(8_000_000));
        let video = technical.video.as_ref().expect("video");
        assert_eq!((video.codec.as_deref(), video.width, video.height), (Some("h264"), Some(1920), Some(1080)));
        let audio = technical.audio.as_ref().expect("audio");
        assert_eq!((audio.codec.as_deref(), audio.channels), (Some("aac"), Some(6)));

        assert_eq!(
            movie.stream_ref,
            Some(F::stream_ref(&input_name(), &server_id(), &movie.item_id, Some("media-source-redacted-1".into())))
        );
        assert_eq!(
            movie.image_ref,
            Some(F::image_ref(
                &input_name(),
                &server_id(),
                &movie.item_id,
                "Primary".into(),
                Some("image-tag-redacted".into())
            ))
        );
        assert!(!format!("{movie:?}").contains("/redacted/upstream/path"));
    }

    #[test]
    fn maps_movie_with_descriptive_technical_and_refs() {
        check_movie_mapping::<Emby>();
        check_movie_mapping::<Jellyfin>();
        let movie = with_ctx(true, |ctx| emby_item_to_movie::<Emby>(ctx, &items(EMBY_MOVIES_PAGE_JSON)[0])).unwrap();
        assert!(matches!(movie.stream_ref, Some(MediaServerStreamRef::Emby { .. })));
        assert!(matches!(movie.image_ref, Some(MediaServerImageRef::Emby { .. })));
    }

    #[test]
    fn jellyfin_flavor_produces_jellyfin_refs_and_round_trips_internal_url() {
        let movie = with_ctx(true, |ctx| emby_item_to_movie::<Jellyfin>(ctx, &items(EMBY_MOVIES_PAGE_JSON)[0]))
            .expect("movie maps");
        let stream_ref = movie.stream_ref.expect("stream ref");
        let image_ref = movie.image_ref.expect("image ref");
        assert!(matches!(stream_ref, MediaServerStreamRef::Jellyfin { .. }));
        assert!(matches!(image_ref, MediaServerImageRef::Jellyfin { .. }));

        let stream_url = media_server_stream_ref_to_internal_url(&stream_ref);
        assert!(stream_url.starts_with("media-server://jellyfin/"));
        assert_eq!(parse_media_server_stream_ref(&input_name(), &stream_url).expect("parses"), stream_ref);
        let image_url = media_server_image_ref_to_internal_url(&image_ref);
        assert!(image_url.starts_with("media-server://image/jellyfin/"));
        assert_eq!(parse_media_server_image_ref(&image_url).expect("parses"), image_ref);

        let emby = with_ctx(true, |ctx| emby_item_to_movie::<Emby>(ctx, &items(EMBY_MOVIES_PAGE_JSON)[0])).unwrap();
        let emby_url = media_server_stream_ref_to_internal_url(&emby.stream_ref.expect("stream ref"));
        assert!(emby_url.starts_with("media-server://emby/"));
        assert_eq!(Jellyfin::stream_ref_parts(&stream_ref).expect("parts").item_id, "movie-redacted-1");
        assert!(Emby::stream_ref_parts(&stream_ref).is_none());
        assert!(Emby::image_ref_parts(&image_ref).is_none());
        let image_parts = Jellyfin::image_ref_parts(&image_ref).expect("image parts");
        assert_eq!((image_parts.image_kind, image_parts.tag), ("Primary", Some("image-tag-redacted")));
    }

    fn check_series_season_episode<F: EmbyFamilyFlavor>() {
        with_ctx(true, |ctx| {
            let series = emby_item_to_series::<F>(ctx, &items(EMBY_SERIES_PAGE_JSON)[0]).expect("series maps");
            assert_eq!(series.item_id.as_ref(), "series-redacted-1");
            assert_eq!(series.title.as_ref(), "Series Redacted");
            assert_eq!(series.year, Some(2020));
            assert_eq!(series.release_date.as_deref(), Some("2020-01-10"));
            assert_eq!(series.child_count, Some(2));
            assert_eq!(series.episode_count, Some(14));
            let facts = series.descriptive_facts.as_ref().expect("series facts");
            assert_eq!(facts.network.as_deref(), Some("Network Redacted"));
            assert!(facts.studio.is_none());
            assert_eq!(facts.audience_rating.as_deref(), Some("8.1"));
            assert_eq!(
                series.provider_hints.iter().map(|h| h.namespace.as_ref()).collect::<Vec<_>>(),
                vec!["tmdb", "tvdb"]
            );
            assert!(series.image_ref.is_some());

            let seasons = items(EMBY_SEASONS_PAGE_JSON);
            let season = emby_item_to_season::<F>(ctx, &seasons[0]).expect("season maps");
            assert_eq!(season.item_id.as_ref(), "season-redacted-1");
            assert_eq!(season.series_id.as_deref(), Some("series-redacted-1"));
            assert_eq!(season.series_title.as_deref(), Some("Series Redacted"));
            assert_eq!(season.title.as_ref(), "Season 1");
            assert_eq!(season.season, Some(1));
            assert_eq!(season.episode_count, Some(8));
            assert_eq!(season.release_date.as_deref(), Some("2020-01-10"));
            assert!(season.image_ref.is_some());
            let season_two = emby_item_to_season::<F>(ctx, &seasons[1]).expect("season maps");
            assert_eq!(season_two.season, Some(2));
            assert!(season_two.image_ref.is_none());

            let episode = emby_item_to_episode::<F>(ctx, &items(EMBY_EPISODES_PAGE_JSON)[0]).expect("episode maps");
            assert_eq!(episode.item_id.as_ref(), "episode-redacted-1");
            assert_eq!(episode.series_id.as_deref(), Some("series-redacted-1"));
            assert_eq!(episode.series_title.as_deref(), Some("Series Redacted"));
            assert_eq!(episode.season, Some(1));
            assert_eq!(episode.episode, Some(1));
            assert_eq!(episode.release_date.as_deref(), Some("2020-01-10"));
            assert_eq!(episode.provider_hints.len(), 1);
            let technical = episode.technical_facts.as_ref().expect("technical facts");
            assert_eq!(technical.duration_secs, Some(2700));
            assert_eq!(technical.bitrate, Some(4_000_000));
            assert_eq!(
                episode.stream_ref,
                Some(F::stream_ref(
                    &input_name(),
                    &server_id(),
                    &episode.item_id,
                    Some("media-source-redacted-2".into())
                ))
            );
        });
    }

    #[test]
    fn maps_series_season_episode_links() {
        check_series_season_episode::<Emby>();
        check_series_season_episode::<Jellyfin>();
    }

    fn check_remote_source<F: EmbyFamilyFlavor>() {
        with_ctx(true, |ctx| {
            let episode =
                emby_item_to_episode::<F>(ctx, &items(EMBY_EPISODES_PAGE_JSON)[2]).expect("identity stays valid");
            assert!(episode.stream_ref.is_none());
            assert_eq!(episode.item_id.as_ref(), "episode-redacted-3");
            assert!(!format!("{episode:?}").contains("redacted.invalid"));
        });
    }

    #[test]
    fn remote_media_source_yields_no_stream_ref() {
        check_remote_source::<Emby>();
        check_remote_source::<Jellyfin>();
    }

    #[test]
    fn media_source_choice_filters_ineligible_and_prefers_direct_play() {
        let item: EmbyItemDto = serde_json::from_value(serde_json::json!({
            "Id": "item-1",
            "MediaSources": [
                { "Id": "remote", "Protocol": "Http", "SupportsDirectPlay": true },
                { "Id": "flagged-remote", "IsRemote": true, "SupportsDirectPlay": true },
                { "Id": "no-direct", "Protocol": "file", "SupportsDirectPlay": false },
                { "Id": "direct", "SupportsDirectStream": true }
            ]
        }))
        .unwrap();
        let movie = with_ctx(true, |ctx| emby_item_to_movie::<Emby>(ctx, &item)).unwrap();
        assert_eq!(Emby::stream_ref_parts(&movie.stream_ref.unwrap()).unwrap().media_source_id, Some("direct"));

        let item: EmbyItemDto = serde_json::from_value(serde_json::json!({
            "Id": "item-2",
            "MediaSources": [
                { "Id": "remote", "Protocol": "Http" },
                { "Id": "first-eligible", "Protocol": "File" },
                { "Id": "second-eligible" }
            ]
        }))
        .unwrap();
        let movie = with_ctx(true, |ctx| emby_item_to_movie::<Emby>(ctx, &item)).unwrap();
        assert_eq!(Emby::stream_ref_parts(&movie.stream_ref.unwrap()).unwrap().media_source_id, Some("first-eligible"));
    }

    #[test]
    fn requested_media_sources_missing_yields_no_stream_ref() {
        let movie = with_ctx(true, |ctx| emby_item_to_movie::<Emby>(ctx, &items(EMBY_MOVIES_PAGE_JSON)[1])).unwrap();
        assert!(movie.stream_ref.is_none());
    }

    #[test]
    fn local_media_source_without_id_yields_ref_for_the_server_default_source() {
        for source in [
            serde_json::json!({ "Protocol": "File", "SupportsDirectPlay": true }),
            serde_json::json!({ "Id": "   ", "Protocol": "File", "SupportsDirectPlay": true }),
        ] {
            let item: EmbyItemDto =
                serde_json::from_value(serde_json::json!({ "Id": "item-1", "MediaSources": [source] })).unwrap();
            let movie = with_ctx(true, |ctx| emby_item_to_movie::<Emby>(ctx, &item)).expect("movie maps");
            let parts = Emby::stream_ref_parts(movie.stream_ref.as_ref().expect("stream ref stays playable"))
                .expect("emby ref");
            assert_eq!(parts.item_id, "item-1");
            assert_eq!(parts.media_source_id, None);
        }
    }

    fn check_include_media_sources_false<F: EmbyFamilyFlavor>() {
        let movie =
            with_ctx(false, |ctx| emby_item_to_movie::<F>(ctx, &items(EMBY_MOVIES_PAGE_JSON)[1])).expect("movie maps");
        assert_eq!(movie.stream_ref, Some(F::stream_ref(&input_name(), &server_id(), &movie.item_id, None)));
    }

    #[test]
    fn include_media_sources_false_yields_ref_without_source_id() {
        check_include_media_sources_false::<Emby>();
        check_include_media_sources_false::<Jellyfin>();
        let movie = with_ctx(false, |ctx| emby_item_to_movie::<Emby>(ctx, &items(EMBY_MOVIES_PAGE_JSON)[0])).unwrap();
        assert_eq!(Emby::stream_ref_parts(&movie.stream_ref.unwrap()).unwrap().media_source_id, None);
    }

    #[test]
    fn virtual_items_are_dropped_but_counted_upstream() {
        let episodes = items(EMBY_EPISODES_PAGE_JSON);
        assert!(!is_virtual_item(&episodes[0]));
        assert!(is_virtual_item(&episodes[1]));
        assert!(!is_virtual_item(&episodes[2]));
        let mut item = bare_item("x");
        item.location_type = Some("virtual".to_string());
        assert!(is_virtual_item(&item));
        assert!(!is_virtual_item(&bare_item("y")));
    }

    fn check_minimal<F: EmbyFamilyFlavor>() {
        with_ctx(true, |ctx| {
            let item = bare_item("only-id");
            let capitalized = match F::KIND {
                MediaServerKind::Emby => "Emby",
                _ => "Jellyfin",
            };
            let movie = emby_item_to_movie::<F>(ctx, &item).expect("movie");
            assert_eq!(movie.title.as_ref(), format!("{capitalized} Movie"));
            assert!(movie.year.is_none() && movie.release_date.is_none() && movie.source_version_hint.is_none());
            assert_eq!(movie.provider_hints, Vec::new());
            assert!(movie.descriptive_facts.is_none());
            assert!(movie.technical_facts.is_none());
            assert!(movie.stream_ref.is_none());
            assert!(movie.image_ref.is_none());

            assert_eq!(emby_item_to_series::<F>(ctx, &item).unwrap().title.as_ref(), format!("{capitalized} Series"));
            assert_eq!(emby_item_to_season::<F>(ctx, &item).unwrap().title.as_ref(), format!("{capitalized} Season"));
            let episode = emby_item_to_episode::<F>(ctx, &item).unwrap();
            assert_eq!(episode.title.as_ref(), format!("{capitalized} Episode"));
            assert!(episode.series_id.is_none() && episode.season.is_none() && episode.episode.is_none());

            let mut blank_name = bare_item("blank-name");
            blank_name.name = Some("   ".to_string());
            assert_eq!(
                emby_item_to_movie::<F>(ctx, &blank_name).unwrap().title.as_ref(),
                format!("{capitalized} Movie")
            );
        });
    }

    #[test]
    fn minimal_item_maps_with_fallbacks() {
        check_minimal::<Emby>();
        check_minimal::<Jellyfin>();
    }

    #[test]
    fn blank_id_is_dropped() {
        with_ctx(true, |ctx| {
            for id in ["", "   "] {
                let item = bare_item(id);
                assert!(emby_item_to_movie::<Emby>(ctx, &item).is_none());
                assert!(emby_item_to_series::<Emby>(ctx, &item).is_none());
                assert!(emby_item_to_season::<Jellyfin>(ctx, &item).is_none());
                assert!(emby_item_to_episode::<Jellyfin>(ctx, &item).is_none());
            }
        });
    }

    #[test]
    fn library_kind_maps_movies_tvshows_only() {
        assert_eq!(emby_library_kind(Some("movies")), MediaServerLibraryKind::Movies);
        assert_eq!(emby_library_kind(Some(" TVShows ")), MediaServerLibraryKind::TvShows);
        for other in [Some("mixed"), Some("boxsets"), Some("music"), Some(""), None] {
            assert_eq!(emby_library_kind(other), MediaServerLibraryKind::Unsupported);
        }

        let folders: Vec<EmbyVirtualFolderDto> = serde_json::from_str(EMBY_VIRTUAL_FOLDERS_JSON).unwrap();
        assert_eq!(folders[0].library_key(), Some("library-redacted-movies"));
        assert_eq!(folders[0].library_title(), Some("Movies"));
        assert_eq!(folders[0].library_kind(), MediaServerLibraryKind::Movies);
        assert_eq!(folders[1].library_kind(), MediaServerLibraryKind::TvShows);
        assert_eq!(folders[2].library_kind(), MediaServerLibraryKind::Unsupported);
        assert_eq!(folders[3].library_kind(), MediaServerLibraryKind::Unsupported);

        let id_only: EmbyVirtualFolderDto =
            serde_json::from_value(serde_json::json!({ "Name": "X", "Id": "guid-1", "ItemId": " " })).unwrap();
        assert_eq!(id_only.library_key(), Some("guid-1"));

        let views: EmbyItemsPageDto<EmbyViewDto> = serde_json::from_str(JELLYFIN_VIEWS_JSON).unwrap();
        assert_eq!(views.items[0].library_key(), Some(views.items[0].id.as_str()));
        assert_eq!(views.items[0].library_kind(), MediaServerLibraryKind::Movies);
        assert_eq!(views.items[1].library_kind(), MediaServerLibraryKind::TvShows);
    }

    #[test]
    fn folders_resolve_through_shared_library_selection() {
        let folders: Vec<EmbyVirtualFolderDto> = serde_json::from_str(EMBY_VIRTUAL_FOLDERS_JSON).unwrap();
        let selectors = [MediaServerLibrarySelector::Name("TV".to_string())];
        let libraries =
            crate::library_selection::select_libraries("emby", &input_name(), &server_id(), &selectors, &folders)
                .expect("selected");
        assert_eq!(libraries[0].reference.library_id.as_ref(), "library-redacted-tv");
        assert_eq!(libraries[0].kind, MediaServerLibraryKind::TvShows);
    }

    #[test]
    fn provider_hints_are_lowercased_sorted_and_skip_blank_values() {
        let item: EmbyItemDto = serde_json::from_value(serde_json::json!({
            "Id": "p",
            "ProviderIds": { "Tvdb": "3", "Tmdb": "1", "IMDB": "tt2", "Zap2It": "  ", "Empty": "" }
        }))
        .unwrap();
        let hints = provider_hints(&item.provider_ids);
        assert_eq!(
            hints.iter().map(|h| (h.namespace.as_ref(), h.value.as_ref())).collect::<Vec<_>>(),
            vec![("imdb", "tt2"), ("tmdb", "1"), ("tvdb", "3")]
        );
    }

    #[test]
    fn release_date_and_source_version_hint_parse_defensively() {
        assert_eq!(release_date(Some("2024-03-01T00:00:00.0000000Z")).as_deref(), Some("2024-03-01"));
        assert_eq!(release_date(Some("2024-03-01")).as_deref(), Some("2024-03-01"));
        assert_eq!(release_date(Some("2024-13-45T00:00:00Z")), None);
        assert_eq!(release_date(Some("not a date")), None);
        assert_eq!(release_date(Some("20")), None);
        assert_eq!(release_date(Some("ääääääääää")), None);
        assert_eq!(release_date(None), None);

        assert_eq!(source_version_hint(Some("2024-04-02T10:15:30.1234567Z")).as_deref(), Some("1712052930"));
        assert_eq!(source_version_hint(Some("2024-04-02T12:15:30+02:00")).as_deref(), Some("1712052930"));
        assert_eq!(source_version_hint(Some("1969-12-31T00:00:00Z")), None);
        assert_eq!(source_version_hint(Some("garbage")), None);
        assert_eq!(source_version_hint(None), None);
    }

    #[test]
    fn audience_rating_uses_one_decimal() {
        let mut item = bare_item("r");
        item.community_rating = Some(8.0);
        let movie = with_ctx(true, |ctx| emby_item_to_movie::<Emby>(ctx, &item)).unwrap();
        assert_eq!(movie.descriptive_facts.unwrap().audience_rating.as_deref(), Some("8.0"));
    }

    #[test]
    fn technical_facts_fall_back_to_item_runtime_and_match_stream_type_case_insensitively() {
        let item: EmbyItemDto = serde_json::from_value(serde_json::json!({
            "Id": "t",
            "RunTimeTicks": 36_000_000_000u64,
            "MediaSources": [{
                "Id": "s",
                "MediaStreams": [
                    { "Type": "audio", "Codec": "eac3", "Channels": 8 },
                    { "Type": "VIDEO", "Codec": "av1", "Width": 3840, "Height": 2160 }
                ]
            }]
        }))
        .unwrap();
        let movie = with_ctx(true, |ctx| emby_item_to_movie::<Emby>(ctx, &item)).unwrap();
        let technical = movie.technical_facts.expect("technical facts");
        assert_eq!(technical.duration_secs, Some(3600));
        assert_eq!(technical.video.unwrap().codec.as_deref(), Some("av1"));
        assert_eq!(technical.audio.unwrap().channels, Some(8));
    }
}
