use crate::model::{TraktConfig, TraktListConfig, TraktListItem, TraktMatchItem, TraktMatchResult, TraktContentType, MatchType};
use crate::model::{PlaylistGroup, PlaylistItem, XtreamCluster, ConfigTarget};
use crate::tuliprox_error::TuliproxError;
use crate::utils::trakt::client::TraktClient;
use crate::utils::trakt::extract_year_from_title;
use crate::utils::trakt::normalize_title_for_matching;
use crate::utils::{get_u32_from_serde_value};
use std::sync::Arc;
use std::collections::HashMap;
use log::{debug, info, warn};
use strsim::jaro_winkler;
use rayon::prelude::*;

/// Utility functions for content type compatibility
fn should_include_item(item: &TraktMatchItem, content_type: &TraktContentType) -> bool {
    match content_type {
        TraktContentType::Vod => item.content_type == TraktContentType::Vod,
        TraktContentType::Series => item.content_type == TraktContentType::Series,
        TraktContentType::Both => true,
    }
}

fn is_compatible_content_type(cluster: &XtreamCluster, content_type: &TraktContentType) -> bool {
    match content_type {
        TraktContentType::Vod => *cluster == XtreamCluster::Video,
        TraktContentType::Series => *cluster == XtreamCluster::Series,
        TraktContentType::Both => matches!(cluster, XtreamCluster::Video | XtreamCluster::Series),
    }
}

/// Extract TMDB ID from playlist item
fn extract_tmdb_id_from_playlist_item(item: &PlaylistItem) -> Option<u32> {
    if let Some(additional_props) = &item.header.additional_properties {
        if let Some(props_str) = additional_props.as_str() {
            if let Ok(props) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(props_str) {
                if let Some(tmdb_value) = props.get("tmdb") {
                    return get_u32_from_serde_value(tmdb_value);
                }
                if let Some(tmdb_id_value) = props.get("tmdb_id") {
                    return get_u32_from_serde_value(tmdb_id_value);
                }
            }
        }
    }
    None
}

/// Create playlist index for faster lookups
fn create_playlist_index(playlist: &[PlaylistGroup]) -> HashMap<String, &PlaylistItem> {
    let mut playlist_index = HashMap::new();
    
    for group in playlist {
        for item in &group.channels {
            playlist_index.insert(format!("{:?}", item.header.uuid), item);
        }
    }
    
    playlist_index
}

pub struct TraktCategoriesProcessor {
    client: TraktClient,
}

impl TraktCategoriesProcessor {
    pub fn new(http_client: Arc<reqwest::Client>, trakt_config: &TraktConfig) -> Result<Self, TuliproxError> {
        let client = TraktClient::new(http_client, trakt_config.api.clone())?;
        Ok(Self { client })
    }

    pub async fn process_trakt_categories(
        &self,
        playlist: &mut [PlaylistGroup],
        target: &ConfigTarget,
        trakt_config: &TraktConfig,
    ) -> Result<Vec<PlaylistGroup>, Vec<TuliproxError>> {
        if trakt_config.lists.is_empty() {
            debug!("No Trakt lists configured for target {}", target.name);
            return Ok(vec![]);
        }

        info!("Processing {} Trakt lists for target {}", trakt_config.lists.len(), target.name);

        let trakt_lists = match self.client.get_all_lists(&trakt_config.lists).await {
            Ok(lists) => lists,
            Err(errors) => {
                warn!("Failed to fetch some Trakt lists: {} errors", errors.len());
                return Err(errors);
            }
        };

        let mut new_categories = Vec::new();
        let mut total_matches = 0;

        for list_config in &trakt_config.lists {
            let cache_key = format!("{}:{}", list_config.user, list_config.list_slug);
            
            if let Some(trakt_items) = trakt_lists.get(&cache_key) {
                info!("Processing Trakt list {} with {} items", cache_key, trakt_items.len());

                let matches = self.match_trakt_items_with_playlist(
                    trakt_items,
                    playlist,
                    list_config,
                ).await;

                if !matches.is_empty() {
                    let category = self.create_category_from_matches(
                        matches,
                        playlist,
                        list_config,
                    );
                    
                    if !category.channels.is_empty() {
                        total_matches += category.channels.len();
                        let category_len = category.channels.len();
                        new_categories.push(category);
                        info!("Created Trakt category '{}' with {} items", 
                             list_config.category_name, category_len);
                    }
                }
            } else {
                warn!("No items found for Trakt list {}", cache_key);
            }
        }

        info!("Trakt processing complete: created {} categories with {} total matches", 
             new_categories.len(), total_matches);

        Ok(new_categories)
    }

    async fn match_trakt_items_with_playlist(
        &self,
        trakt_items: &[TraktListItem],
        playlist: &[PlaylistGroup],
        list_config: &TraktListConfig,
    ) -> Vec<TraktMatchResult> {
        // Convert Trakt items into a matching structure
        let trakt_match_items: Vec<TraktMatchItem> = trakt_items
            .iter()
            .map(TraktMatchItem::from)
            .filter(|item| should_include_item(item, &list_config.content_type))
            .collect();

        info!("Matching {} Trakt items against playlist for content type {:?}", 
             trakt_match_items.len(), list_config.content_type);

        // Collect all playlist items that are compatible with the content type
        let mut compatible_items: Vec<&PlaylistItem> = Vec::new();
        for group in playlist {
            for channel in &group.channels {
                if is_compatible_content_type(&channel.header.xtream_cluster, &list_config.content_type) {
                    compatible_items.push(channel);
                }
            }
        }

        info!("Processing {} compatible playlist items", compatible_items.len());

        // Process all items without truncation
        let all_matches: Vec<TraktMatchResult> = if compatible_items.len() > 1000 {
            compatible_items
                .par_iter()
                .flat_map(|channel| {
                    self.find_matches_for_channel(channel, &trakt_match_items, list_config)
                })
                .collect()
        } else {
            compatible_items
                .iter()
                .flat_map(|channel| {
                    self.find_matches_for_channel(channel, &trakt_match_items, list_config)
                })
                .collect()
        };

        // Now deduplicate by keeping only the best matches
        let total_matches = all_matches.len();
        let best_matches = self.select_best_matches(all_matches, playlist);
        
        let display_key = format!("{}:{}", list_config.user, list_config.list_slug);
        info!("Found {} unique matches (from {} total) for Trakt list {}", 
             best_matches.len(), total_matches, display_key);
        
        best_matches
    }

    fn find_matches_for_channel(
        &self,
        channel: &PlaylistItem,
        trakt_items: &[TraktMatchItem],
        list_config: &TraktListConfig,
    ) -> Vec<TraktMatchResult> {
        let mut matches = Vec::new();
        let playlist_uuid = format!("{:?}", channel.header.uuid);

        // Try TMDB exact matching first
        if let Some(playlist_tmdb_id) = extract_tmdb_id_from_playlist_item(channel) {
            for trakt_item in trakt_items {
                if let Some(trakt_tmdb_id) = trakt_item.tmdb_id {
                    if playlist_tmdb_id == trakt_tmdb_id {
                        matches.push(TraktMatchResult {
                            playlist_item_uuid: playlist_uuid.clone(),
                            trakt_item: trakt_item.clone(),
                            match_score: 1.0,
                            match_type: MatchType::TmdbExact,
                        });
                        debug!("TMDB exact match: '{}' (TMDB: {})", channel.header.title, playlist_tmdb_id);
                    }
                }
            }
        }

        // Try fuzzy matching if no exact match found
        if matches.is_empty() {
            let normalized_playlist_title = normalize_title_for_matching(&channel.header.title);
            let playlist_year = extract_year_from_title(&channel.header.title);
            let threshold = (list_config.fuzzy_match_threshold as f64) / 100.0;
            
            let mut best_match: Option<(TraktMatchItem, f64)> = None;
            
            for trakt_item in trakt_items {
                let normalized_trakt_title = normalize_title_for_matching(&trakt_item.title);
                let title_score = jaro_winkler(&normalized_playlist_title, &normalized_trakt_title);
                
                // Calculate year bonus
                let year_bonus = self.calculate_year_bonus(playlist_year, trakt_item.year);
                let mut combined_score = title_score + year_bonus;
                
                // Clamp score to [0.0, 1.0]
                if combined_score > 1.0 { combined_score = 1.0; }
                if combined_score < 0.0 { combined_score = 0.0; }

                // Check if this is the best match so far and meets threshold
                if combined_score >= threshold {
                    if let Some((_, current_best_score)) = &best_match {
                        if combined_score > *current_best_score {
                            best_match = Some((trakt_item.clone(), combined_score));
                        }
                    } else {
                        best_match = Some((trakt_item.clone(), combined_score));
                    }
                }
            }
            
            if let Some((trakt_item, combined_score)) = best_match {
                let match_type = if playlist_year.is_some() && trakt_item.year.is_some() {
                    MatchType::FuzzyTitleYear
                } else {
                    MatchType::FuzzyTitle
                };

                matches.push(TraktMatchResult {
                    playlist_item_uuid: playlist_uuid.clone(),
                    trakt_item: trakt_item.clone(),
                    match_score: combined_score,
                    match_type: match_type.clone(),
                });

                debug!("Fuzzy match: '{}' -> '{}' (final: {:.3}, type: {:?})",
                      channel.header.title,
                      trakt_item.title,
                      combined_score,
                      match_type);
            }
        }

        matches
    }

    fn calculate_year_bonus(&self, playlist_year: Option<u32>, trakt_year: Option<u32>) -> f64 {
        match (playlist_year, trakt_year) {
            (Some(p_year), Some(t_year)) => {
                if p_year == t_year {
                    // Perfect year match gets substantial bonus
                    0.15
                } else {
                    let year_diff = (p_year as i32 - t_year as i32).abs();
                    if year_diff <= 1 {
                        // 1 year difference gets small bonus (could be release date differences)
                        0.05
                    } else if year_diff <= 3 {
                        // 2-3 years difference gets small penalty
                        -0.05
                    } else {
                        // More than 3 years difference gets larger penalty
                        -0.15
                    }
                }
            }
            (Some(_), None) | (None, Some(_)) => {
                // One has year, other doesn't - small penalty
                -0.05
            }
            (None, None) => {
                // Neither has year - no bonus/penalty
                0.0
            }
        }
    }

    fn create_category_from_matches(
        &self,
        matches: Vec<TraktMatchResult>,
        playlist: &[PlaylistGroup],
        list_config: &TraktListConfig,
    ) -> PlaylistGroup {
        let mut matched_items = Vec::new();

        // Create playlist index for faster lookups using centralized function
        let playlist_index = create_playlist_index(playlist);

        let mut sorted_matches = matches;
        // Simple sort by rank only
        sorted_matches.sort_by(|a, b| {
            a.trakt_item.rank.unwrap_or(9999).cmp(&b.trakt_item.rank.unwrap_or(9999))
        });

        for match_result in sorted_matches {
            if let Some(playlist_item) = playlist_index.get(&match_result.playlist_item_uuid) {
                let mut modified_item = (*playlist_item).clone(); // Clone the referenced item
                // Use the (possibly numbered) title from the match result (which now contains the original playlist title)
                modified_item.header.title = match_result.trakt_item.title.clone();
                // Synchronize name with title so both fields show the same value
                modified_item.header.name = match_result.trakt_item.title.clone();
                matched_items.push(modified_item);
            }
        }

        let cluster = match list_config.content_type {
            TraktContentType::Vod => XtreamCluster::Video,
            TraktContentType::Series => XtreamCluster::Series,
            TraktContentType::Both => {
                matched_items.first()
                    .map(|item| item.header.xtream_cluster)
                    .unwrap_or(XtreamCluster::Video)
            }
        };

        PlaylistGroup {
            id: 0,
            title: list_config.category_name.clone(),
            channels: matched_items,
            xtream_cluster: cluster,
        }
    }

    fn select_best_matches(&self, all_matches: Vec<TraktMatchResult>, playlist: &[PlaylistGroup]) -> Vec<TraktMatchResult> {
        use std::collections::HashMap;
        
        // Create playlist index for faster lookups
        let playlist_index = create_playlist_index(playlist);
        
        // Group matches by Trakt title to handle multiple copies of the same content with different qualities
        let mut title_groups: HashMap<String, Vec<TraktMatchResult>> = HashMap::new();
        
        // Group all matches by Trakt title (normalized)
        for match_result in all_matches {
            let normalized_title = normalize_title_for_matching(&match_result.trakt_item.title);
            title_groups.entry(normalized_title).or_default().push(match_result);
        }
        
        let mut final_matches = Vec::new();
        let mut used_playlist_items: std::collections::HashSet<String> = std::collections::HashSet::new();
        
        // For each Trakt title, select the best matches and number duplicates
        for (_title, mut matches) in title_groups {
            // Sort matches by score descending
            matches.sort_by(|a, b| b.match_score.partial_cmp(&a.match_score).unwrap_or(std::cmp::Ordering::Equal));
            
            let mut selected_matches = Vec::new();
            
            for match_result in matches {
                let playlist_uuid = match_result.playlist_item_uuid.clone();
                
                // Skip if this playlist item is already used
                if used_playlist_items.contains(&playlist_uuid) {
                    continue;
                }
                
                // Add this match to our selected list
                selected_matches.push(match_result);
                used_playlist_items.insert(playlist_uuid);
            }
            
            // If we have multiple matches for the same Trakt content title, number them
            if selected_matches.len() > 1 {
                for (index, mut match_result) in selected_matches.into_iter().enumerate() {
                    // Get the original playlist item title for numbering
                    if let Some(playlist_item) = playlist_index.get(&match_result.playlist_item_uuid) {
                        // Use the original playlist title with numbering instead of Trakt title
                        match_result.trakt_item.title = format!("{} #{}", playlist_item.header.title, index + 1);
                    } else {
                        // Fallback to Trakt title with numbering if playlist item not found
                        match_result.trakt_item.title = format!("{} #{}", match_result.trakt_item.title, index + 1);
                    }
                    
                    final_matches.push(match_result);
                }
            } else if let Some(mut match_result) = selected_matches.into_iter().next() {
                // Single match, use original playlist title without numbering
                if let Some(playlist_item) = playlist_index.get(&match_result.playlist_item_uuid) {
                    match_result.trakt_item.title = playlist_item.header.title.clone();
                }
                final_matches.push(match_result);
            }
        }
        
        final_matches
    }
}

pub async fn process_trakt_categories_for_target(
    http_client: Arc<reqwest::Client>,
    playlist: &mut [PlaylistGroup],
    target: &ConfigTarget,
) -> Result<Vec<PlaylistGroup>, Vec<TuliproxError>> {
    let trakt_config = match target.get_xtream_output().and_then(|output| output.trakt_lists.as_ref()) {
        Some(config) => config,
        None => {
            debug!("No Trakt configuration found for target {}", target.name);
            return Ok(vec![]);
        }
    };

    let processor = match TraktCategoriesProcessor::new(http_client, trakt_config) {
        Ok(p) => p,
        Err(e) => return Err(vec![e]),
    };
    processor.process_trakt_categories(playlist, target, trakt_config).await
} 