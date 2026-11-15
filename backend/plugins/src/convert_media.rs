//! WIT to domain model conversion for media catalog provider plugins.

use shared::model::MediaServerLibraryKind;
use std::{collections::HashSet, sync::Arc};
use tuliprox_media_server::{
    MediaServerAudioTechnicalFacts, MediaServerCatalogSnapshot, MediaServerDescriptiveFacts, MediaServerEpisode,
    MediaServerImageRef, MediaServerLibrary, MediaServerLibraryRef, MediaServerMovie, MediaServerProviderIdHint,
    MediaServerSeason, MediaServerSeries, MediaServerStreamRef, MediaServerTechnicalFacts,
    MediaServerVideoTechnicalFacts,
};
use tuliprox_plugin_api::bindings::tuliprox::plugin::media_types_v1_1 as wit;

pub const MAX_MEDIA_ITEMS: usize = 1_000_000;
pub const MAX_ID_LEN: usize = 128;
pub const MAX_TITLE_LEN: usize = 512;
pub const MAX_TEXT_LEN: usize = 16_384;
pub const MAX_HINT_LEN: usize = 128;
pub const MAX_LIST_ITEMS: usize = 50;
pub const MAX_PEOPLE_ITEMS: usize = 100;

fn cap_str(s: &str, max: usize) -> Arc<str> {
    if s.len() <= max {
        Arc::from(s)
    } else {
        let mut boundary = max;
        while boundary > 0 && !s.is_char_boundary(boundary) {
            boundary -= 1;
        }
        Arc::from(&s[..boundary])
    }
}

fn cap_opt_str(s: Option<String>, max: usize) -> Option<Arc<str>> { s.map(|val| cap_str(&val, max)) }

fn cap_list(list: Vec<String>, max_items: usize, max_len: usize) -> Vec<Arc<str>> {
    list.into_iter().take(max_items).map(|s| cap_str(&s, max_len)).collect()
}

fn convert_provider_ids(ids: Vec<wit::ProviderId>) -> Vec<MediaServerProviderIdHint> {
    ids.into_iter()
        .take(MAX_LIST_ITEMS)
        .map(|id| MediaServerProviderIdHint {
            namespace: cap_str(&id.namespace, MAX_HINT_LEN),
            value: cap_str(&id.value, MAX_HINT_LEN),
        })
        .collect()
}

fn convert_descriptive(d: wit::DescriptiveFacts) -> Option<MediaServerDescriptiveFacts> {
    let facts = MediaServerDescriptiveFacts {
        original_title: cap_opt_str(d.original_title, MAX_TITLE_LEN),
        sort_title: cap_opt_str(d.sort_title, MAX_TITLE_LEN),
        summary: cap_opt_str(d.summary, MAX_TEXT_LEN),
        tagline: cap_opt_str(d.tagline, MAX_TEXT_LEN),
        studio: cap_opt_str(d.studio, MAX_TITLE_LEN),
        network: cap_opt_str(d.network, MAX_TITLE_LEN),
        content_rating: cap_opt_str(d.content_rating, MAX_HINT_LEN),
        parental_age: d.parental_age,
        audience_rating: cap_opt_str(d.audience_rating, MAX_HINT_LEN),
        genres: cap_list(d.genres, MAX_LIST_ITEMS, MAX_HINT_LEN),
        countries: cap_list(d.countries, MAX_LIST_ITEMS, MAX_HINT_LEN),
        directors: cap_list(d.directors, MAX_LIST_ITEMS, MAX_HINT_LEN),
        writers: cap_list(d.writers, MAX_LIST_ITEMS, MAX_HINT_LEN),
        cast: cap_list(d.cast, MAX_PEOPLE_ITEMS, MAX_HINT_LEN),
        crew: cap_list(d.crew, MAX_PEOPLE_ITEMS, MAX_HINT_LEN),
    };
    if facts.is_empty() {
        None
    } else {
        Some(facts)
    }
}

fn convert_technical(t: wit::TechnicalFacts) -> Option<MediaServerTechnicalFacts> {
    let video = t.video.and_then(|v| {
        let vf = MediaServerVideoTechnicalFacts { codec: cap_opt_str(v.codec, 32), width: v.width, height: v.height };
        if vf.is_empty() {
            None
        } else {
            Some(vf)
        }
    });

    let audio = t.audio.and_then(|a| {
        let af = MediaServerAudioTechnicalFacts { codec: cap_opt_str(a.codec, 32), channels: a.channels };
        if af.is_empty() {
            None
        } else {
            Some(af)
        }
    });

    let facts = MediaServerTechnicalFacts {
        container: cap_opt_str(t.container, 32),
        duration_secs: t.duration_secs,
        bitrate: t.bitrate,
        video,
        audio,
    };
    if facts.is_empty() {
        None
    } else {
        Some(facts)
    }
}

/// Converts raw accumulated WIT media catalogs into a domain `MediaServerCatalogSnapshot`.
///
/// Host-sets `input_name` and `server_id` (plugin ID).
/// Enforces bounds:
/// - Maximum 1,000,000 items total
/// - String length caps (titles 512, text 16384, hints 128)
/// - List length caps (genres/crews 50-100)
/// - Drops and logs duplicate item IDs
/// - Drops and logs items referencing unknown libraries or series
fn convert_libraries(
    input_name: &Arc<str>,
    plugin_id: &Arc<str>,
    catalogs: &[wit::MediaCatalog],
) -> (Vec<MediaServerLibrary>, HashSet<String>) {
    let mut libraries = Vec::new();
    let mut known = HashSet::new();
    for catalog in catalogs {
        for lib in &catalog.libraries {
            if lib.id.is_empty() {
                continue;
            }
            if lib.id.len() > MAX_ID_LEN {
                log::warn!("plugin `{plugin_id}`: dropping library `{}` exceeding id limit", lib.id);
                continue;
            }
            if known.insert(lib.id.clone()) {
                let kind = match lib.kind {
                    wit::LibraryKind::Movies => MediaServerLibraryKind::Movies,
                    wit::LibraryKind::TvShows => MediaServerLibraryKind::TvShows,
                };
                libraries.push(MediaServerLibrary {
                    reference: MediaServerLibraryRef {
                        input_name: Arc::clone(input_name),
                        server_id: Arc::clone(plugin_id),
                        library_id: Arc::from(lib.id.as_str()),
                    },
                    name: cap_str(&lib.name, MAX_TITLE_LEN),
                    kind,
                });
            }
        }
    }
    (libraries, known)
}

fn convert_series(
    input_name: &Arc<str>,
    plugin_id: &Arc<str>,
    raw_series: Vec<wit::MediaSeries>,
    known_libraries: &HashSet<String>,
) -> (Vec<MediaServerSeries>, HashSet<String>) {
    let mut series = Vec::new();
    let mut known_series = HashSet::new();
    let mut seen_ids = HashSet::new();

    for s in raw_series {
        if s.item_id.is_empty() {
            log::warn!("plugin `{plugin_id}`: dropping series with empty item id");
            continue;
        }
        if s.item_id.len() > MAX_ID_LEN {
            log::warn!("plugin `{plugin_id}`: dropping series `{}` exceeding id limit", s.item_id);
            continue;
        }
        if s.library_id.len() > MAX_ID_LEN {
            log::warn!("plugin `{plugin_id}`: dropping series `{}` with library id exceeding limit", s.item_id);
            continue;
        }
        if !seen_ids.insert(s.item_id.clone()) {
            log::warn!("plugin `{plugin_id}`: dropping duplicate series id `{}`", s.item_id);
            continue;
        }
        if !known_libraries.contains(&s.library_id) {
            log::warn!(
                "plugin `{plugin_id}`: dropping series `{}` referencing unknown library `{}`",
                s.item_id,
                s.library_id
            );
            continue;
        }

        let title = if s.title.is_empty() { Arc::from("Untitled Series") } else { cap_str(&s.title, MAX_TITLE_LEN) };
        let image_ref = s.image_key.as_deref().filter(|k| !k.is_empty()).map(|key| MediaServerImageRef::Plugin {
            input_name: Arc::clone(input_name),
            plugin_id: Arc::clone(plugin_id),
            item_id: Arc::from(s.item_id.as_str()),
            image_key: Arc::from(key),
        });

        known_series.insert(s.item_id.clone());
        series.push(MediaServerSeries {
            input_name: Arc::clone(input_name),
            server_id: Arc::clone(plugin_id),
            library_id: Arc::from(s.library_id.as_str()),
            item_id: Arc::from(s.item_id.as_str()),
            title,
            year: s.year,
            release_date: cap_opt_str(s.release_date, MAX_HINT_LEN),
            source_version_hint: cap_opt_str(s.source_version_hint, MAX_HINT_LEN),
            provider_hints: convert_provider_ids(s.provider_ids),
            descriptive_facts: s.descriptive.and_then(convert_descriptive),
            child_count: s.child_count,
            episode_count: s.episode_count,
            image_ref,
        });
    }
    (series, known_series)
}

fn convert_seasons(
    input_name: &Arc<str>,
    plugin_id: &Arc<str>,
    raw_seasons: Vec<wit::MediaSeason>,
    known_libraries: &HashSet<String>,
    known_series: &HashSet<String>,
) -> (Vec<MediaServerSeason>, HashSet<String>) {
    let mut seasons = Vec::new();
    let mut known_seasons = HashSet::new();
    let mut seen_ids = HashSet::new();

    for s in raw_seasons {
        if s.item_id.is_empty() {
            log::warn!("plugin `{plugin_id}`: dropping season with empty item id");
            continue;
        }
        if s.item_id.len() > MAX_ID_LEN {
            log::warn!("plugin `{plugin_id}`: dropping season `{}` exceeding id limit", s.item_id);
            continue;
        }
        if s.library_id.len() > MAX_ID_LEN {
            log::warn!("plugin `{plugin_id}`: dropping season `{}` with library id exceeding limit", s.item_id);
            continue;
        }
        if s.series_id.as_deref().is_some_and(|id| id.len() > MAX_ID_LEN) {
            log::warn!("plugin `{plugin_id}`: dropping season `{}` with series id exceeding limit", s.item_id);
            continue;
        }
        if !seen_ids.insert(s.item_id.clone()) {
            log::warn!("plugin `{plugin_id}`: dropping duplicate season id `{}`", s.item_id);
            continue;
        }
        if !known_libraries.contains(&s.library_id) {
            log::warn!(
                "plugin `{plugin_id}`: dropping season `{}` referencing unknown library `{}`",
                s.item_id,
                s.library_id
            );
            continue;
        }
        if let Some(series_id) = &s.series_id {
            if !known_series.contains(series_id) {
                log::warn!(
                    "plugin `{plugin_id}`: dropping season `{}` referencing unknown series `{series_id}`",
                    s.item_id
                );
                continue;
            }
        }

        let title = if s.title.is_empty() { Arc::from("Untitled Season") } else { cap_str(&s.title, MAX_TITLE_LEN) };
        let image_ref = s.image_key.as_deref().filter(|k| !k.is_empty()).map(|key| MediaServerImageRef::Plugin {
            input_name: Arc::clone(input_name),
            plugin_id: Arc::clone(plugin_id),
            item_id: Arc::from(s.item_id.as_str()),
            image_key: Arc::from(key),
        });

        known_seasons.insert(s.item_id.clone());
        seasons.push(MediaServerSeason {
            input_name: Arc::clone(input_name),
            server_id: Arc::clone(plugin_id),
            library_id: Arc::from(s.library_id.as_str()),
            item_id: Arc::from(s.item_id.as_str()),
            series_id: s.series_id.map(Arc::from),
            series_title: cap_opt_str(s.series_title, MAX_TITLE_LEN),
            title,
            season: s.season,
            year: s.year,
            release_date: cap_opt_str(s.release_date, MAX_HINT_LEN),
            source_version_hint: cap_opt_str(s.source_version_hint, MAX_HINT_LEN),
            provider_hints: convert_provider_ids(s.provider_ids),
            descriptive_facts: s.descriptive.and_then(convert_descriptive),
            episode_count: s.episode_count,
            image_ref,
        });
    }
    (seasons, known_seasons)
}

fn convert_episodes(
    input_name: &Arc<str>,
    plugin_id: &Arc<str>,
    raw_episodes: Vec<wit::MediaEpisode>,
    known_libraries: &HashSet<String>,
    known_series: &HashSet<String>,
    known_seasons: &HashSet<String>,
) -> Vec<MediaServerEpisode> {
    let mut episodes = Vec::new();
    let mut seen_ids = HashSet::new();

    for e in raw_episodes {
        if e.item_id.is_empty() {
            log::warn!("plugin `{plugin_id}`: dropping episode with empty item id");
            continue;
        }
        if e.item_id.len() > MAX_ID_LEN {
            log::warn!("plugin `{plugin_id}`: dropping episode `{}` exceeding id limit", e.item_id);
            continue;
        }
        if e.library_id.len() > MAX_ID_LEN {
            log::warn!("plugin `{plugin_id}`: dropping episode `{}` with library id exceeding limit", e.item_id);
            continue;
        }
        if e.series_id.as_deref().is_some_and(|id| id.len() > MAX_ID_LEN) {
            log::warn!("plugin `{plugin_id}`: dropping episode `{}` with series id exceeding limit", e.item_id);
            continue;
        }
        if e.season_id.as_deref().is_some_and(|id| id.len() > MAX_ID_LEN) {
            log::warn!("plugin `{plugin_id}`: dropping episode `{}` with season id exceeding limit", e.item_id);
            continue;
        }
        if !seen_ids.insert(e.item_id.clone()) {
            log::warn!("plugin `{plugin_id}`: dropping duplicate episode id `{}`", e.item_id);
            continue;
        }
        if !known_libraries.contains(&e.library_id) {
            log::warn!(
                "plugin `{plugin_id}`: dropping episode `{}` referencing unknown library `{}`",
                e.item_id,
                e.library_id
            );
            continue;
        }
        if let Some(series_id) = &e.series_id {
            if !known_series.contains(series_id) {
                log::warn!(
                    "plugin `{plugin_id}`: dropping episode `{}` referencing unknown series `{series_id}`",
                    e.item_id
                );
                continue;
            }
        }
        if let Some(season_id) = &e.season_id {
            if !known_seasons.contains(season_id) {
                log::warn!(
                    "plugin `{plugin_id}`: dropping episode `{}` referencing unknown season `{season_id}`",
                    e.item_id
                );
                continue;
            }
        }

        let title = if e.title.is_empty() { Arc::from("Untitled Episode") } else { cap_str(&e.title, MAX_TITLE_LEN) };
        let stream_ref = Some(MediaServerStreamRef::Plugin {
            input_name: Arc::clone(input_name),
            plugin_id: Arc::clone(plugin_id),
            item_id: Arc::from(e.item_id.as_str()),
            stream_key: e.stream_key.as_deref().map(Arc::from),
        });
        let image_ref = e.image_key.as_deref().filter(|k| !k.is_empty()).map(|key| MediaServerImageRef::Plugin {
            input_name: Arc::clone(input_name),
            plugin_id: Arc::clone(plugin_id),
            item_id: Arc::from(e.item_id.as_str()),
            image_key: Arc::from(key),
        });

        episodes.push(MediaServerEpisode {
            input_name: Arc::clone(input_name),
            server_id: Arc::clone(plugin_id),
            library_id: Arc::from(e.library_id.as_str()),
            item_id: Arc::from(e.item_id.as_str()),
            season_id: e.season_id.map(Arc::from),
            series_id: e.series_id.map(Arc::from),
            series_title: cap_opt_str(e.series_title, MAX_TITLE_LEN),
            title,
            season: e.season,
            episode: e.episode,
            release_date: cap_opt_str(e.release_date, MAX_HINT_LEN),
            source_version_hint: cap_opt_str(e.source_version_hint, MAX_HINT_LEN),
            provider_hints: convert_provider_ids(e.provider_ids),
            descriptive_facts: e.descriptive.and_then(convert_descriptive),
            technical_facts: e.technical.and_then(convert_technical),
            stream_ref,
            image_ref,
        });
    }
    episodes
}

fn convert_movies(
    input_name: &Arc<str>,
    plugin_id: &Arc<str>,
    raw_movies: Vec<wit::MediaMovie>,
    known_libraries: &HashSet<String>,
) -> Vec<MediaServerMovie> {
    let mut movies = Vec::new();
    let mut seen_ids = HashSet::new();

    for m in raw_movies {
        if m.item_id.is_empty() {
            log::warn!("plugin `{plugin_id}`: dropping movie with empty item id");
            continue;
        }
        if m.item_id.len() > MAX_ID_LEN {
            log::warn!("plugin `{plugin_id}`: dropping movie `{}` exceeding id limit", m.item_id);
            continue;
        }
        if m.library_id.len() > MAX_ID_LEN {
            log::warn!("plugin `{plugin_id}`: dropping movie `{}` with library id exceeding limit", m.item_id);
            continue;
        }
        if !seen_ids.insert(m.item_id.clone()) {
            log::warn!("plugin `{plugin_id}`: dropping duplicate movie id `{}`", m.item_id);
            continue;
        }
        if !known_libraries.contains(&m.library_id) {
            log::warn!(
                "plugin `{plugin_id}`: dropping movie `{}` referencing unknown library `{}`",
                m.item_id,
                m.library_id
            );
            continue;
        }

        let title = if m.title.is_empty() { Arc::from("Untitled Movie") } else { cap_str(&m.title, MAX_TITLE_LEN) };
        let stream_ref = Some(MediaServerStreamRef::Plugin {
            input_name: Arc::clone(input_name),
            plugin_id: Arc::clone(plugin_id),
            item_id: Arc::from(m.item_id.as_str()),
            stream_key: m.stream_key.as_deref().map(Arc::from),
        });
        let image_ref = m.image_key.as_deref().filter(|k| !k.is_empty()).map(|key| MediaServerImageRef::Plugin {
            input_name: Arc::clone(input_name),
            plugin_id: Arc::clone(plugin_id),
            item_id: Arc::from(m.item_id.as_str()),
            image_key: Arc::from(key),
        });

        movies.push(MediaServerMovie {
            input_name: Arc::clone(input_name),
            server_id: Arc::clone(plugin_id),
            library_id: Arc::from(m.library_id.as_str()),
            item_id: Arc::from(m.item_id.as_str()),
            title,
            year: m.year,
            release_date: cap_opt_str(m.release_date, MAX_HINT_LEN),
            source_version_hint: cap_opt_str(m.source_version_hint, MAX_HINT_LEN),
            provider_hints: convert_provider_ids(m.provider_ids),
            descriptive_facts: m.descriptive.and_then(convert_descriptive),
            technical_facts: m.technical.and_then(convert_technical),
            stream_ref,
            image_ref,
        });
    }
    movies
}

pub fn convert_media_catalog(
    input_name: &str,
    plugin_id: &str,
    catalogs: Vec<wit::MediaCatalog>,
) -> Result<MediaServerCatalogSnapshot, String> {
    let input_name_arc: Arc<str> = Arc::from(input_name);
    let plugin_id_arc: Arc<str> = Arc::from(plugin_id);

    let total_items: usize =
        catalogs.iter().map(|c| c.movies.len() + c.series.len() + c.seasons.len() + c.episodes.len()).sum();
    if total_items > MAX_MEDIA_ITEMS {
        return Err(format!("media catalog returned {total_items} items (exceeds cap of {MAX_MEDIA_ITEMS})"));
    }

    let (libraries, known_libraries) = convert_libraries(&input_name_arc, &plugin_id_arc, &catalogs);

    let mut raw_movies = Vec::new();
    let mut raw_series = Vec::new();
    let mut raw_seasons = Vec::new();
    let mut raw_episodes = Vec::new();
    for catalog in catalogs {
        raw_movies.extend(catalog.movies);
        raw_series.extend(catalog.series);
        raw_seasons.extend(catalog.seasons);
        raw_episodes.extend(catalog.episodes);
    }

    let (series, known_series) = convert_series(&input_name_arc, &plugin_id_arc, raw_series, &known_libraries);
    let (seasons, known_seasons) =
        convert_seasons(&input_name_arc, &plugin_id_arc, raw_seasons, &known_libraries, &known_series);
    let episodes = convert_episodes(
        &input_name_arc,
        &plugin_id_arc,
        raw_episodes,
        &known_libraries,
        &known_series,
        &known_seasons,
    );
    let movies = convert_movies(&input_name_arc, &plugin_id_arc, raw_movies, &known_libraries);

    Ok(MediaServerCatalogSnapshot { libraries, movies, series, seasons, episodes, unsupported_libraries: Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_with_ids_exceeding_max_id_len_are_dropped_and_valid_ids_kept_intact() {
        let long_id = "a".repeat(MAX_ID_LEN + 1);
        let normal_id = "valid-id";
        let catalog = wit::MediaCatalog {
            libraries: vec![
                wit::MediaLibrary { id: "lib-1".into(), name: "Movies".into(), kind: wit::LibraryKind::Movies },
                wit::MediaLibrary { id: long_id.clone(), name: "Too Long".into(), kind: wit::LibraryKind::Movies },
            ],
            movies: vec![
                wit::MediaMovie {
                    library_id: "lib-1".into(),
                    item_id: normal_id.into(),
                    title: "Good Movie".into(),
                    year: None,
                    release_date: None,
                    source_version_hint: None,
                    provider_ids: vec![],
                    descriptive: None,
                    technical: None,
                    stream_key: None,
                    image_key: None,
                },
                wit::MediaMovie {
                    library_id: "lib-1".into(),
                    item_id: long_id.clone(),
                    title: "Bad Movie".into(),
                    year: None,
                    release_date: None,
                    source_version_hint: None,
                    provider_ids: vec![],
                    descriptive: None,
                    technical: None,
                    stream_key: None,
                    image_key: None,
                },
            ],
            series: vec![],
            seasons: vec![],
            episodes: vec![],
        };

        let snapshot = convert_media_catalog("test_input", "test_plugin", vec![catalog]).unwrap();
        assert_eq!(snapshot.libraries.len(), 1);
        assert_eq!(&*snapshot.libraries[0].reference.library_id, "lib-1");
        assert_eq!(snapshot.movies.len(), 1);
        assert_eq!(&*snapshot.movies[0].item_id, normal_id);
    }
}
