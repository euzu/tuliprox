use indexmap::IndexMap;
use log::{debug, info, trace, warn};
use shared::{
    error::TuliproxError,
    model::{
        FieldGet, FieldSet, HeaderField, PlaylistEntry, PlaylistGroup, PlaylistItem, PlaylistItemType,
        TraktContentType, UUIDType, XtreamCluster,
    },
    utils::{hash_string, Internable, CONSTANTS},
};
use std::{collections::HashMap, sync::Arc};
use strsim::normalized_levenshtein;
use tuliprox_core::{
    model::{ConfigTarget, TraktCategoryConfig, TraktConfig, TraktListItem, TraktMatchItem, TraktMatchResult},
    utils::{extract_year_from_title, normalize_title_for_matching, trace_if_enabled, with, TraktClient},
};

fn extract_quality(value: &str) -> Option<&str> {
    if let Some(caps) = CONSTANTS.re_quality.captures(value) {
        if let Some(val) = caps.get(0) {
            return Some(val.as_str());
        }
    }
    None
}

/// Utility functions for content type compatibility
fn should_include_item(item: &TraktListItem, content_type: TraktContentType) -> bool {
    match content_type {
        TraktContentType::Vod => item.content_type == TraktContentType::Vod,
        TraktContentType::Series => item.content_type == TraktContentType::Series,
        TraktContentType::Both => true,
    }
}

fn is_compatible_content_type(cluster: XtreamCluster, content_type: TraktContentType) -> bool {
    match content_type {
        TraktContentType::Vod => cluster == XtreamCluster::Video,
        TraktContentType::Series => cluster == XtreamCluster::Series,
        TraktContentType::Both => matches!(cluster, XtreamCluster::Video | XtreamCluster::Series),
    }
}

fn is_matchable_playlist_item(item_type: PlaylistItemType, content_type: TraktContentType) -> bool {
    match content_type {
        TraktContentType::Vod => item_type.is_video(),
        TraktContentType::Series => {
            matches!(item_type, PlaylistItemType::SeriesInfo | PlaylistItemType::LocalSeriesInfo)
        }
        TraktContentType::Both => matches!(
            item_type,
            PlaylistItemType::Video
                | PlaylistItemType::LocalVideo
                | PlaylistItemType::SeriesInfo
                | PlaylistItemType::LocalSeriesInfo
        ),
    }
}

fn calculate_year_bonus(playlist_year: Option<u32>, trakt_year: Option<u32>) -> f64 {
    if let (Some(p_year), Some(t_year)) = (playlist_year, trakt_year) {
        if p_year == t_year {
            // Perfect year match gets substantial bonus
            return 0.5;
        }
        return -0.5;
    }
    0.0
}

fn find_best_fuzzy_match_for_item<'a>(
    channel: (&'a PlaylistItem, String, Option<u32>, Option<u32>),
    trakt_items: &'a [TraktMatchItem],
    category_config: &'a TraktCategoryConfig,
) -> Option<TraktMatchResult<'a>> {
    // Try fuzzy matching if no exact match found
    let normalized_playlist_title = channel.1;
    let playlist_year = channel.2;
    let threshold = f64::from(category_config.fuzzy_match_threshold) / 100.0;
    let mut best_match: Option<(&TraktMatchItem, f64)> = None;

    for trakt_item in trakt_items {
        let title_score = normalized_levenshtein(&normalized_playlist_title, &trakt_item.normalized_title);

        if title_score >= threshold {
            // Calculate year bonus
            let year_bonus = calculate_year_bonus(playlist_year, trakt_item.year);
            let mut combined_score = title_score + year_bonus;

            // Clamp score to [0.0, 1.0]
            combined_score = combined_score.clamp(0.0, 1.0);

            // Check if this is the best match so far and meets threshold
            if combined_score >= threshold {
                if let Some((_, current_best_score)) = &best_match {
                    if combined_score > *current_best_score {
                        best_match = Some((trakt_item, combined_score));
                    }
                } else {
                    best_match = Some((trakt_item, combined_score));
                }
                // early exit strategy
                if combined_score >= 0.99 {
                    break;
                }
            }
        }
    }

    if let Some((trakt_item, combined_score)) = best_match {
        // let match_type = if playlist_year.is_some() && trakt_item.year.is_some() {
        //     MatchType::FuzzyTitleYear
        // } else {
        //     MatchType::FuzzyTitle
        // };

        trace_if_enabled!(
            "Fuzzy match: '{}' -> '{}' (final: {combined_score:.3}", /*, type: {match_type:?})"*/
            channel.0.header.title,
            trakt_item.title
        );

        return Some(TraktMatchResult {
            playlist_item: channel.0,
            trakt_item,
            match_score: combined_score,
            // match_type: match_type.clone(),
        });
    }

    None
}

fn find_best_match_for_item<'a>(
    channel: (&'a PlaylistItem, String, Option<u32>, Option<u32>),
    trakt_items: &'a [TraktMatchItem<'a>],
    category_config: &'a TraktCategoryConfig,
) -> Option<TraktMatchResult<'a>> {
    // Try TMDB exact matching first
    if let Some(playlist_tmdb_id) = channel.3 {
        for trakt_item in trakt_items {
            if Some(playlist_tmdb_id) == trakt_item.tmdb_id {
                trace!("TMDB exact match: '{}' (TMDB: {})", channel.0.header.title, playlist_tmdb_id);
                return Some(TraktMatchResult {
                    playlist_item: channel.0,
                    trakt_item,
                    match_score: 1.0,
                    // match_type: MatchType::TmdbExact,
                });
            }
        }
    }

    if category_config.tmdb_only {
        return None;
    }

    find_best_fuzzy_match_for_item(channel, trakt_items, category_config)
}

fn create_category_from_matches<'a>(
    matches: Vec<TraktMatchResult<'a>>,
    category_config: &'a TraktCategoryConfig,
    series_children_by_parent_code: &HashMap<Arc<str>, Vec<&'a PlaylistItem>>,
) -> Vec<PlaylistGroup> {
    if matches.is_empty() {
        return vec![];
    }

    let mut matched_items_by_cluster: IndexMap<XtreamCluster, Vec<PlaylistItem>> = IndexMap::new();

    let mut sorted_matches = matches;
    sorted_matches.sort_by(|a, b| {
        (a.trakt_item.rank.unwrap_or(9999), a.trakt_item.title.to_lowercase())
            .cmp(&(b.trakt_item.rank.unwrap_or(9999), b.trakt_item.title.to_lowercase()))
    });

    let group_title = category_config.category_name.as_str().intern();

    for match_result in sorted_matches {
        let modified_item = clone_item_for_trakt_category(
            match_result.playlist_item,
            category_config.category_name.as_str(),
            &group_title,
        );
        let parent_uuid = modified_item.header.uuid.intern();
        let is_series_info =
            matches!(modified_item.header.item_type, PlaylistItemType::SeriesInfo | PlaylistItemType::LocalSeriesInfo);
        let child_lookup_keys =
            if is_series_info { series_info_child_lookup_keys(match_result.playlist_item) } else { Vec::new() };
        let cluster = modified_item.header.xtream_cluster;
        matched_items_by_cluster.entry(cluster).or_default().push(modified_item);

        if let Some(children) = child_lookup_keys.iter().find_map(|key| series_children_by_parent_code.get(key)) {
            for child in children {
                let mut child =
                    clone_item_for_trakt_category(child, category_config.category_name.as_str(), &group_title);
                child.header.parent_code = parent_uuid.clone();
                matched_items_by_cluster.entry(child.header.xtream_cluster).or_default().push(child);
            }
        }
    }

    matched_items_by_cluster
        .into_iter()
        .map(|(cluster, channels)| PlaylistGroup {
            id: 0,
            title: group_title.clone(),
            channels,
            xtream_cluster: cluster,
        })
        .collect()
}

fn clone_item_for_trakt_category(item: &PlaylistItem, category_name: &str, group_title: &Arc<str>) -> PlaylistItem {
    let mut modified_item = item.clone();
    let source_uuid = if modified_item.header.uuid == UUIDType::default() {
        modified_item.get_uuid()
    } else {
        modified_item.header.uuid
    };

    with!(mut modified_item.header => header {
        let title = header
            .get(HeaderField::Caption)
            .map_or_else(|| Arc::clone(&header.title), |value| value.to_arc());
        if extract_quality(&title).is_none() {
            if let Some(quality) = extract_quality(&header.group) {
                let mut caption = String::with_capacity(title.len() + 6);
                caption.push('[');
                caption.push_str(quality);
                caption.push_str("] ");
                caption.push_str(&title);
                header.set(HeaderField::Caption, &caption);
            }
        }
        header.group = group_title.clone();
        header.uuid = hash_string(&format!("trakt-category:{category_name}:{source_uuid}"));
    });

    modified_item
}

fn series_info_child_lookup_keys(series_info: &PlaylistItem) -> Vec<Arc<str>> {
    match series_info.header.item_type {
        PlaylistItemType::LocalSeriesInfo => vec![series_info.header.id.clone(), series_info.header.uuid.intern()],
        PlaylistItemType::SeriesInfo => vec![series_info.get_uuid().intern(), series_info.header.uuid.intern()],
        _ => Vec::new(),
    }
}

fn series_children_by_parent_code(playlist: &[PlaylistGroup]) -> HashMap<Arc<str>, Vec<&PlaylistItem>> {
    let mut children = HashMap::<Arc<str>, Vec<&PlaylistItem>>::new();
    for playlist_group in playlist {
        for channel in &playlist_group.channels {
            if channel.header.item_type.is_series() && !channel.header.parent_code.is_empty() {
                children.entry(channel.header.parent_code.clone()).or_default().push(channel);
            }
        }
    }
    children
}

fn match_trakt_items_with_playlist<'a>(
    trakt_items: &'a [TraktListItem],
    playlist: &'a [PlaylistGroup],
    category_config: &'a TraktCategoryConfig,
) -> Vec<PlaylistGroup> {
    let trakt_match_items: Vec<TraktMatchItem<'a>> = trakt_items
        .iter()
        .filter(|item| should_include_item(item, category_config.content_type))
        .filter_map(TraktMatchItem::from_trakt_list_item)
        .collect();

    debug!(
        "Matching {} Trakt items against playlist for content type {:?}",
        trakt_match_items.len(),
        category_config.content_type
    );

    let mut matches = Vec::new();
    for playlist_group in playlist {
        for channel in &playlist_group.channels {
            if is_compatible_content_type(channel.header.xtream_cluster, category_config.content_type)
                && is_matchable_playlist_item(channel.header.item_type, category_config.content_type)
            {
                let normalized_title = normalize_title_for_matching(&channel.header.title);
                let channel_year = extract_year_from_title(&channel.header.title);
                let channel_tmdb_id = channel.get_tmdb_id();
                if let Some(matched) = find_best_match_for_item(
                    (channel, normalized_title, channel_year, channel_tmdb_id),
                    &trakt_match_items,
                    category_config,
                ) {
                    matches.push(matched);
                }
            }
        }
    }

    let series_children_by_parent_code = series_children_by_parent_code(playlist);
    create_category_from_matches(matches, category_config, &series_children_by_parent_code)
}

pub struct TraktCategoriesProcessor {
    client: TraktClient,
}

impl TraktCategoriesProcessor {
    pub fn new(http_client: &reqwest::Client, trakt_config: &TraktConfig) -> Self {
        let client = TraktClient::new(http_client.clone(), trakt_config.api.clone());
        Self { client }
    }

    pub async fn process_trakt_categories(
        &self,
        playlist: &[PlaylistGroup],
        target: &ConfigTarget,
        trakt_config: &TraktConfig,
    ) -> Result<Option<Vec<PlaylistGroup>>, Vec<TuliproxError>> {
        if trakt_config.lists.is_empty() && trakt_config.charts.is_empty() {
            debug!("No Trakt lists or charts configured for target {}", target.name);
            return Ok(None);
        }

        info!(
            "Processing {} Trakt lists and {} Trakt charts for target {}",
            trakt_config.lists.len(),
            trakt_config.charts.len(),
            target.name
        );
        let mut new_categories = Vec::new();
        let mut total_matches = 0;

        for list_config in &trakt_config.lists {
            let cache_key = format!("{}:{}", list_config.user, list_config.list_slug);
            let category_config = TraktCategoryConfig::from(list_config);

            match self.client.get_list_items(list_config).await {
                Ok(trakt_items) => {
                    debug!("Processing Trakt list {cache_key} with {} items", trakt_items.len());

                    let categories = match_trakt_items_with_playlist(&trakt_items, playlist, &category_config);
                    for category in categories {
                        if !category.channels.is_empty() {
                            total_matches += category.channels.len();
                            let category_len = category.channels.len();
                            new_categories.push(category);
                            debug!(
                                "Created Trakt category '{}' with {category_len} items",
                                category_config.category_name
                            );
                        }
                    }
                }
                Err(err) => {
                    warn!("Failed to fetch Trakt list {cache_key}: {}", err.message());
                }
            }
        }

        for chart_config in &trakt_config.charts {
            let cache_key = format!("{}:{}", chart_config.kind, chart_config.chart);
            let category_config = TraktCategoryConfig::from(chart_config);

            match self.client.get_chart_items(chart_config).await {
                Ok(trakt_items) => {
                    debug!("Processing Trakt chart {cache_key} with {} items", trakt_items.len());

                    let categories = match_trakt_items_with_playlist(&trakt_items, playlist, &category_config);
                    for category in categories {
                        if !category.channels.is_empty() {
                            total_matches += category.channels.len();
                            let category_len = category.channels.len();
                            new_categories.push(category);
                            debug!(
                                "Created Trakt category '{}' with {category_len} items",
                                category_config.category_name
                            );
                        }
                    }
                }
                Err(err) => {
                    warn!("Failed to fetch Trakt chart {cache_key}: {}", err.message());
                }
            }
        }

        info!(
            "Trakt processing complete: created {} categories with {total_matches} total matches",
            new_categories.len()
        );

        Ok(Some(new_categories))
    }
}
pub async fn process_trakt_categories_for_target(
    http_client: &reqwest::Client,
    playlist: &[PlaylistGroup],
    target: &ConfigTarget,
) -> Result<Option<Vec<PlaylistGroup>>, Vec<TuliproxError>> {
    let Some(trakt_config) = target.get_xtream_output().and_then(|output| output.trakt.as_ref()) else {
        trace!("No Trakt configuration found for target {}", target.name);
        return Ok(None);
    };
    if !trakt_config.enabled {
        return Ok(None);
    }

    let processor = TraktCategoriesProcessor::new(http_client, trakt_config);
    processor.process_trakt_categories(playlist, target, trakt_config).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{
        EpisodeStreamProperties, PlaylistItemHeader, SeriesStreamProperties, StreamProperties, TraktContentType,
        VideoStreamProperties,
    };

    #[test]
    pub fn test_quality() {
        let quality = extract_quality("Hello HD UHD 720p");
        assert!(quality.is_some());
        assert_eq!("UHD", quality.unwrap());
    }

    #[test]
    fn tmdb_only_list_skips_title_fallback_matches() {
        let playlist_item = video_item("The Captive", None);
        let trakt_items = vec![trakt_movie("The Captive", Some(1915), Some(123), 1)];
        let list_config = list_config(true);

        let matched = find_best_match_for_item(
            (&playlist_item, normalize_title_for_matching("The Captive"), None, playlist_item.get_tmdb_id()),
            &trakt_items,
            &list_config,
        );

        assert!(matched.is_none());
    }

    #[test]
    fn tmdb_only_list_keeps_tmdb_exact_matches() {
        let playlist_item = video_item("Cautivos", Some(456));
        let trakt_items = vec![trakt_movie("The Captive", Some(2014), Some(456), 1)];
        let list_config = list_config(true);

        let matched = find_best_match_for_item(
            (&playlist_item, normalize_title_for_matching("Cautivos"), None, playlist_item.get_tmdb_id()),
            &trakt_items,
            &list_config,
        );

        assert!(matched.is_some());
        assert_eq!(matched.expect("tmdb match").trakt_item.tmdb_id, Some(456));
    }

    #[test]
    fn same_playlist_item_can_appear_in_multiple_trakt_categories() {
        let playlist = vec![PlaylistGroup {
            id: 1,
            title: "Original".intern(),
            channels: vec![video_item("The Smashing Machine", Some(760_329))],
            xtream_cluster: XtreamCluster::Video,
        }];
        let trakt_items = vec![trakt_list_movie("The Smashing Machine", Some(2025), Some(760_329), 1)];
        let a24_config = named_list_config("▸ A24", true);
        let renoir_config = named_list_config("▸ Cines Renoir", true);

        let a24 = match_trakt_items_with_playlist(&trakt_items, &playlist, &a24_config);
        let renoir = match_trakt_items_with_playlist(&trakt_items, &playlist, &renoir_config);

        assert_eq!(a24.len(), 1);
        assert_eq!(renoir.len(), 1);
        let a24_item = &a24[0].channels[0];
        let renoir_item = &renoir[0].channels[0];
        assert_eq!(a24_item.header.group.as_ref(), "▸ A24");
        assert_eq!(renoir_item.header.group.as_ref(), "▸ Cines Renoir");
        assert_ne!(a24_item.header.uuid, renoir_item.header.uuid);
    }

    #[test]
    fn trakt_series_categories_clone_episode_children() {
        let mut series = series_item("Slow Horses", Some(12345));
        let source_parent_code = series.get_uuid().intern();
        series.header.uuid = series.get_uuid();
        let episode = episode_item("Old Scores", &source_parent_code, 7001);
        let playlist = vec![PlaylistGroup {
            id: 1,
            title: "Media Server Series".intern(),
            channels: vec![series, episode],
            xtream_cluster: XtreamCluster::Series,
        }];
        let trakt_items = vec![trakt_list_show("Slow Horses", Some(2022), Some(12345), 1)];
        let config = named_series_config("Trending", true);

        let categories = match_trakt_items_with_playlist(&trakt_items, &playlist, &config);

        assert_eq!(categories.len(), 1);
        assert_eq!(categories[0].channels.len(), 2);
        let cloned_series = categories[0]
            .channels
            .iter()
            .find(|item| item.header.item_type == PlaylistItemType::SeriesInfo)
            .expect("series info clone");
        let cloned_episode = categories[0]
            .channels
            .iter()
            .find(|item| item.header.item_type == PlaylistItemType::Series)
            .expect("episode clone");
        assert_eq!(cloned_series.header.group.as_ref(), "Trending");
        assert_eq!(cloned_episode.header.group.as_ref(), "Trending");
        assert_eq!(cloned_episode.header.parent_code, cloned_series.header.uuid.intern());
    }

    #[test]
    fn trakt_series_matching_ignores_episode_rows() {
        let episode = episode_item("Slow Horses", &"series-parent".intern(), 7001);
        let playlist = vec![PlaylistGroup {
            id: 1,
            title: "Media Server Series".intern(),
            channels: vec![episode],
            xtream_cluster: XtreamCluster::Series,
        }];
        let trakt_items = vec![trakt_list_show("Slow Horses", Some(2022), Some(12345), 1)];
        let config = TraktCategoryConfig {
            category_name: "Trending".to_string(),
            content_type: TraktContentType::Series,
            tmdb_only: false,
            fuzzy_match_threshold: 100,
        };

        let categories = match_trakt_items_with_playlist(&trakt_items, &playlist, &config);

        assert!(categories.is_empty());
    }

    fn list_config(tmdb_only: bool) -> TraktCategoryConfig {
        named_list_config("category", tmdb_only)
    }

    fn named_list_config(category_name: &str, tmdb_only: bool) -> TraktCategoryConfig {
        TraktCategoryConfig {
            category_name: category_name.to_string(),
            content_type: TraktContentType::Vod,
            tmdb_only,
            fuzzy_match_threshold: 100,
        }
    }

    fn named_series_config(category_name: &str, tmdb_only: bool) -> TraktCategoryConfig {
        TraktCategoryConfig {
            category_name: category_name.to_string(),
            content_type: TraktContentType::Series,
            tmdb_only,
            fuzzy_match_threshold: 100,
        }
    }

    fn video_item(title: &str, tmdb: Option<u32>) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                title: title.intern(),
                xtream_cluster: XtreamCluster::Video,
                item_type: PlaylistItemType::Video,
                additional_properties: Some(StreamProperties::Video(Box::new(VideoStreamProperties {
                    name: title.intern(),
                    tmdb,
                    ..VideoStreamProperties::default()
                }))),
                ..PlaylistItemHeader::default()
            },
        }
    }

    fn series_item(title: &str, tmdb: Option<u32>) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                id: format!("series-{title}").intern(),
                input_name: "input".intern(),
                title: title.intern(),
                name: title.intern(),
                url: format!("media-server://unavailable/server/shows/{title}").intern(),
                xtream_cluster: XtreamCluster::Series,
                item_type: PlaylistItemType::SeriesInfo,
                additional_properties: Some(StreamProperties::Series(Box::new(SeriesStreamProperties {
                    name: title.intern(),
                    tmdb,
                    ..SeriesStreamProperties::default()
                }))),
                ..PlaylistItemHeader::default()
            },
        }
    }

    fn episode_item(title: &str, parent_code: &Arc<str>, virtual_id: u32) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                uuid: hash_string(&format!("episode:{title}:{virtual_id}")),
                id: format!("episode-{virtual_id}").intern(),
                input_name: "input".intern(),
                parent_code: parent_code.clone(),
                title: title.intern(),
                name: title.intern(),
                url: format!("media-server://plex/server/{virtual_id}?part_key=%2Flibrary%2Fparts%2Fredacted").intern(),
                virtual_id: shared::model::VirtualId::new(virtual_id),
                xtream_cluster: XtreamCluster::Series,
                item_type: PlaylistItemType::Series,
                additional_properties: Some(StreamProperties::Episode(Box::new(EpisodeStreamProperties {
                    episode_id: virtual_id,
                    episode: 1,
                    season: 1,
                    added: None,
                    release_date: None,
                    series_release_date: None,
                    tmdb: None,
                    movie_image: "".intern(),
                    container_extension: "mkv".intern(),
                    video: None,
                    audio: None,
                    plot: None,
                }))),
                ..PlaylistItemHeader::default()
            },
        }
    }

    fn trakt_movie(
        title: &'static str,
        year: Option<u32>,
        tmdb_id: Option<u32>,
        trakt_id: u32,
    ) -> TraktMatchItem<'static> {
        TraktMatchItem {
            title,
            normalized_title: normalize_title_for_matching(title),
            year,
            tmdb_id,
            trakt_id,
            content_type: TraktContentType::Vod,
            rank: Some(trakt_id),
        }
    }

    fn trakt_list_movie(title: &str, year: Option<u32>, tmdb_id: Option<u32>, trakt_id: u32) -> TraktListItem {
        TraktListItem {
            id: u64::from(trakt_id),
            rank: Some(trakt_id),
            listed_at: String::new(),
            notes: None,
            item_type: "movie".to_string(),
            movie: Some(tuliprox_core::model::TraktMovie {
                ids: trakt_ids(title, tmdb_id, trakt_id),
                title: title.to_string(),
                year,
            }),
            show: None,
            content_type: TraktContentType::Vod,
        }
    }

    fn trakt_list_show(title: &str, year: Option<u32>, tmdb_id: Option<u32>, trakt_id: u32) -> TraktListItem {
        TraktListItem {
            id: u64::from(trakt_id),
            rank: Some(trakt_id),
            listed_at: String::new(),
            notes: None,
            item_type: "show".to_string(),
            movie: None,
            show: Some(tuliprox_core::model::TraktShow {
                ids: trakt_ids(title, tmdb_id, trakt_id),
                title: title.to_string(),
                year,
            }),
            content_type: TraktContentType::Series,
        }
    }

    fn trakt_ids(title: &str, tmdb_id: Option<u32>, trakt_id: u32) -> tuliprox_core::model::TraktIds {
        tuliprox_core::model::TraktIds {
            trakt: trakt_id,
            slug: title.to_string(),
            tvdb: None,
            imdb: None,
            tmdb: tmdb_id,
            tvrage: None,
        }
    }
}
