use crate::model::{ConfigTarget, TraktListItem, TraktMatchItem};
use crate::model::{TraktConfig, TraktListConfig, TraktMatchResult};
use crate::utils::{extract_year_from_title, normalize_title_for_matching, TraktClient};
use crate::utils::{trace_if_enabled, with};
use log::{debug, info, trace, warn};
use shared::error::TuliproxError;
use shared::utils::StringInterner;
use shared::model::{FieldGetAccessor, FieldSetAccessor, PlaylistGroup, PlaylistItem, TraktContentType, XtreamCluster};
use shared::utils::CONSTANTS;
use std::borrow::Cow;
use strsim::normalized_levenshtein;

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

fn find_best_fuzzy_match_for_item<'a>(channel: (&'a PlaylistItem, String, Option<u32>, Option<u32>), trakt_items: &'a [TraktMatchItem], list_config: &'a TraktListConfig) -> Option<TraktMatchResult<'a>> {
    // Try fuzzy matching if no exact match found
    let normalized_playlist_title = channel.1;
    let playlist_year = channel.2;
    let threshold = f64::from(list_config.fuzzy_match_threshold) / 100.0;
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

        trace_if_enabled!("Fuzzy match: '{}' -> '{}' (final: {combined_score:.3}" /*, type: {match_type:?})"*/, channel.0.header.title, trakt_item.title);

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
    list_config: &'a TraktListConfig,
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

    find_best_fuzzy_match_for_item(channel, trakt_items, list_config)
}

fn create_category_from_matches<'a>(
    matches: Vec<TraktMatchResult<'a>>,
    list_config: &'a TraktListConfig,
    interner: &mut StringInterner,
) -> Option<PlaylistGroup> {
    if matches.is_empty() { return None; }

    let mut matched_items = Vec::new();

    let mut sorted_matches = matches;
    sorted_matches.sort_by(|a, b| {
        (
            a.trakt_item.rank.unwrap_or(9999),
            a.trakt_item.title.to_lowercase(),
        ).cmp(&(
            b.trakt_item.rank.unwrap_or(9999),
            b.trakt_item.title.to_lowercase(),
        ))
    });

    let group_title = &list_config.category_name;

    for match_result in sorted_matches {
        let mut modified_item = match_result.playlist_item.clone();
        // Use the (possibly numbered) title from the match result (which now contains the original playlist title)
        with!(mut modified_item.header => header {
            // Synchronize name with title so both fields show the same value
            // header.title.clone_from(&match_result.trakt_item.title.to_string());
            // header.name.clone_from(&match_result.trakt_item.title.to_string());
            let title = header.get_field("caption").unwrap_or_else(|| Cow::Borrowed(&header.title));
            if extract_quality(&title).is_none() {
                if let Some(quality) = extract_quality(&header.group) {
                    let mut caption = String::with_capacity(title.len() + 6);
                    caption.push('[');
                    caption.push_str(quality);
                    caption.push_str("] ");
                    caption.push_str(&title);
                    header.set_field("caption", &caption);
                }
            }
            header.group = interner.intern(group_title);
            header.gen_uuid();
        });
        matched_items.push(modified_item);
    }

    if matched_items.is_empty() { return None; }


    let cluster = match list_config.content_type {
        TraktContentType::Vod => XtreamCluster::Video,
        TraktContentType::Series => XtreamCluster::Series,
        TraktContentType::Both => {
            matched_items.first()
                .map_or(XtreamCluster::Video, |item| item.header.xtream_cluster)
        }
    };

    Some(PlaylistGroup {
        id: 0,
        title: interner.intern(group_title),
        channels: matched_items,
        xtream_cluster: cluster,
    })
}

fn match_trakt_items_with_playlist<'a>(
    trakt_items: &'a [TraktListItem],
    playlist: &'a [PlaylistGroup],
    list_config: &'a TraktListConfig,
    interner: &mut StringInterner,
) -> Option<PlaylistGroup> {
    let trakt_match_items: Vec<TraktMatchItem<'a>> = trakt_items
        .iter()
        .filter(|item| should_include_item(item, list_config.content_type))
        .filter_map(TraktMatchItem::from_trakt_list_item)
        .collect();

    debug!("Matching {} Trakt items against playlist for content type {:?}", trakt_match_items.len(), list_config.content_type);

    let mut matches = Vec::new();
    for playlist_group in playlist {
        for channel in &playlist_group.channels {
            if is_compatible_content_type(channel.header.xtream_cluster, list_config.content_type) {
                let normalized_title = normalize_title_for_matching(&channel.header.title);
                let channel_year = extract_year_from_title(&channel.header.title);
                let channel_tmdb_id = channel.get_tmdb_id();
                if let Some(matched) = find_best_match_for_item((channel, normalized_title, channel_year, channel_tmdb_id), &trakt_match_items, list_config) {
                    matches.push(matched);
                }
            }
        }
    }

    create_category_from_matches(matches, list_config, interner)
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
        if trakt_config.lists.is_empty() {
            debug!("No Trakt lists configured for target {}", target.name);
            return Ok(None);
        }

        info!("Processing {} Trakt lists for target {}", trakt_config.lists.len(), target.name);
        let mut new_categories = Vec::new();
        let mut total_matches = 0;
        let mut interner = StringInterner::new();

        for list_config in &trakt_config.lists {
            let cache_key = format!("{}:{}", list_config.user, list_config.list_slug);

            match self.client.get_list_items(list_config).await {
                Ok(trakt_items) => {
                    debug!("Processing Trakt list {cache_key} with {} items", trakt_items.len());

                    if let Some(category) = match_trakt_items_with_playlist(&trakt_items, playlist, list_config, &mut interner) {
                        if !category.channels.is_empty() {
                            total_matches += category.channels.len();
                            let category_len = category.channels.len();
                            new_categories.push(category);
                            debug!("Created Trakt category '{}' with {category_len} items", list_config.category_name);
                        }
                    }
                }
                Err(err) => {
                    warn!("Failed to fetch Trakt list {cache_key}: {}", err.message);
                }
            }
        }


        info!("Trakt processing complete: created {} categories with {total_matches} total matches",
             new_categories.len());

        Ok(Some(new_categories))
    }
}
pub async fn process_trakt_categories_for_target(
    http_client: &reqwest::Client,
    playlist: &[PlaylistGroup],
    target: &ConfigTarget,
) -> Result<Option<Vec<PlaylistGroup>>, Vec<TuliproxError>> {
    let Some(trakt_config) = target.get_xtream_output().and_then(|output| output.trakt.as_ref()) else {
        debug!("No Trakt configuration found for target {}", target.name);
        return Ok(None);
    };

    let processor = TraktCategoriesProcessor::new(http_client, trakt_config);
    processor.process_trakt_categories(playlist, target, trakt_config).await
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    pub fn test_quality() {
        let quality = extract_quality("Hello HD UHD 720p");
        assert!(quality.is_some());
        assert_eq!("UHD", quality.unwrap());
    }
}