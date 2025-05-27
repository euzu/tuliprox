use crate::model::{TraktConfig, TraktListConfig, TraktListItem, TraktMatchItem, TraktMatchResult, TraktContentType, MatchType};
use crate::model::{PlaylistGroup, PlaylistItem, XtreamCluster, ConfigTarget};
use crate::tuliprox_error::TuliproxError;
use crate::utils::trakt::{TraktClient, normalize_title_for_matching, extract_year_from_title, DEFAULT_TRAKT_NORMALIZE_CONFIG};
use crate::utils::{get_u32_from_serde_value};
use std::collections::HashMap;
use std::sync::Arc;
use log::{debug, info, warn};
use strsim::jaro_winkler;
use rphonetic::{DoubleMetaphone, Encoder};


const YEAR_MATCH_BONUS: f64 = 0.1;

pub struct TraktCategoriesProcessor {
    client: TraktClient,
}

impl TraktCategoriesProcessor {
    pub fn new(http_client: Arc<reqwest::Client>, trakt_config: &TraktConfig) -> Self {
        Self {
            client: TraktClient::new(http_client, trakt_config.api.clone()),
        }
    }

    pub async fn process_trakt_categories(
        &mut self,
        playlist: &mut [PlaylistGroup],
        target: &ConfigTarget,
        trakt_config: &TraktConfig,
    ) -> Result<Vec<PlaylistGroup>, Vec<TuliproxError>> {
        if trakt_config.lists.is_empty() {
            debug!("No Trakt lists configured for target {}", target.name);
            return Ok(vec![]);
        }

        info!("Processing {} Trakt lists for target {}", trakt_config.lists.len(), target.name);

        // Retrieve all Trakt lists
        let trakt_lists = match self.client.get_all_lists(&trakt_config.lists).await {
            Ok(lists) => lists,
            Err(errors) => {
                warn!("Failed to fetch some Trakt lists: {} errors", errors.len());
                return Err(errors);
            }
        };

        let mut new_categories = Vec::new();
        let mut total_matches = 0;

        // Processing each list
        for list_config in &trakt_config.lists {
            let cache_key = format!("{}:{}", list_config.user, list_config.list_slug);
            
            if let Some(trakt_items) = trakt_lists.get(&cache_key) {
                info!("Processing Trakt list {}:{} with {} items", 
                     list_config.user, list_config.list_slug, trakt_items.len());

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
                warn!("No items found for Trakt list {}:{}", list_config.user, list_config.list_slug);
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
        let mut matches = Vec::new();

        // Convert Trakt items into a matching structure
        let trakt_match_items: Vec<TraktMatchItem> = trakt_items
            .iter()
            .map(TraktMatchItem::from)
            .filter(|item| self.should_include_item(item, list_config))
            .collect();

        info!("Matching {} Trakt items against playlist for content type {:?}", 
             trakt_match_items.len(), list_config.content_type);

        for group in playlist {
            for channel in &group.channels {
                if !self.is_compatible_content_type(&channel.header.xtream_cluster, list_config) {
                    continue;
                }

                // Try exact matching by TMDB ID first
                if let Some(playlist_tmdb_id) = self.extract_tmdb_id_from_playlist_item(channel) {
                    for trakt_item in &trakt_match_items {
                        if let Some(trakt_tmdb_id) = trakt_item.tmdb_id {
                            if playlist_tmdb_id == trakt_tmdb_id {
                                matches.push(TraktMatchResult {
                                    playlist_item_uuid: format!("{:?}", channel.header.uuid),
                                    trakt_item: trakt_item.clone(),
                                    match_score: 1.0,
                                    match_type: MatchType::TmdbExact,
                                });
                                debug!("TMDB exact match: '{}' (TMDB: {})", channel.header.title, playlist_tmdb_id);
                                continue;
                            }
                        }
                    }
                }

                // Fuzzy matching by title if no TMDB match
                let best_fuzzy_match = self.find_best_fuzzy_match(channel, &trakt_match_items, list_config);
                if let Some(fuzzy_match) = best_fuzzy_match {
                    matches.push(fuzzy_match);
                }
            }
        }

        info!("Found {} matches for Trakt list {}:{}", matches.len(), list_config.user, list_config.list_slug);
        matches
    }

    fn should_include_item(&self, item: &TraktMatchItem, list_config: &TraktListConfig) -> bool {
        match list_config.content_type {
            TraktContentType::Vod => item.content_type == TraktContentType::Vod,
            TraktContentType::Series => item.content_type == TraktContentType::Series,
            TraktContentType::Both => true,
        }
    }

    fn is_compatible_content_type(&self, cluster: &XtreamCluster, list_config: &TraktListConfig) -> bool {
        match list_config.content_type {
            TraktContentType::Vod => *cluster == XtreamCluster::Video,
            TraktContentType::Series => *cluster == XtreamCluster::Series,
            TraktContentType::Both => matches!(cluster, XtreamCluster::Video | XtreamCluster::Series),
        }
    }

    fn extract_tmdb_id_from_playlist_item(&self, item: &PlaylistItem) -> Option<u32> {
        // Search in additional properties
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

    fn find_best_fuzzy_match(
        &self,
        playlist_item: &PlaylistItem,
        trakt_items: &[TraktMatchItem],
        list_config: &TraktListConfig,
    ) -> Option<TraktMatchResult> {
        let (playlist_title_clean, playlist_year) = extract_year_from_title(&playlist_item.header.title);
        let normalized_playlist_title = normalize_title_for_matching(&playlist_title_clean, &DEFAULT_TRAKT_NORMALIZE_CONFIG);

        let metaphone_encoder = DoubleMetaphone::default();
        let phonetic_playlist_title = metaphone_encoder.encode(&normalized_playlist_title);

        let mut best_match: Option<TraktMatchResult> = None;
        let mut best_score = list_config.fuzzy_match_threshold as f64 / 100.0;

        for trakt_item in trakt_items {
            let (trakt_title_clean, trakt_year) = extract_year_from_title(&trakt_item.title);
            let normalized_trakt_title = normalize_title_for_matching(&trakt_title_clean, &DEFAULT_TRAKT_NORMALIZE_CONFIG);
            let phonetic_trakt_title = metaphone_encoder.encode(&normalized_trakt_title);

            let title_score = jaro_winkler(&normalized_playlist_title, &normalized_trakt_title);
            let phonetic_score = jaro_winkler(&phonetic_playlist_title, &phonetic_trakt_title);

            let mut combined_score = 0.8 * title_score + 0.2 * phonetic_score;

            let match_type = if let (Some(p_year), Some(t_year)) = (playlist_year, trakt_year) {
                if p_year == t_year {
                    combined_score += YEAR_MATCH_BONUS;
                    if combined_score > 1.0 { combined_score = 1.0; }
                    MatchType::FuzzyTitleYear
                } else {
                    combined_score *= 0.8;
                    MatchType::FuzzyTitle
                }
            } else {
                MatchType::FuzzyTitle
            };

            if combined_score > best_score {
                best_score = combined_score;
                best_match = Some(TraktMatchResult {
                    playlist_item_uuid: format!("{:?}", playlist_item.header.uuid),
                    trakt_item: trakt_item.clone(),
                    match_score: combined_score,
                    match_type,
                });
            }
        }

        if let Some(ref match_result) = best_match {
            debug!("Fuzzy match: '{}' -> '{}' (score: {:.3}, type: {:?})",
                  playlist_item.header.title,
                  match_result.trakt_item.title,
                  match_result.match_score,
                  match_result.match_type);
        }

        best_match
    }

    fn create_category_from_matches(
        &self,
        matches: Vec<TraktMatchResult>,
        playlist: &[PlaylistGroup],
        list_config: &TraktListConfig,
    ) -> PlaylistGroup {
        let mut matched_items = Vec::new();

        let mut playlist_index = HashMap::new();
        for group in playlist {
            for item in &group.channels {
                playlist_index.insert(format!("{:?}", item.header.uuid), item.clone());
            }
        }

        let mut sorted_matches = matches;
        sorted_matches.sort_by(|a, b| {
            a.trakt_item.rank.unwrap_or(9999).cmp(&b.trakt_item.rank.unwrap_or(9999))
        });

        for match_result in sorted_matches {
            if let Some(playlist_item) = playlist_index.get(&match_result.playlist_item_uuid) {
                matched_items.push(playlist_item.clone());
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
            id: 0, // Will be assigned later
            title: list_config.category_name.clone(),
            channels: matched_items,
            xtream_cluster: cluster,
        }
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

    let mut processor = TraktCategoriesProcessor::new(http_client, trakt_config);
    processor.process_trakt_categories(playlist, target, trakt_config).await
} 