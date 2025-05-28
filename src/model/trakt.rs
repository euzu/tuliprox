use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraktApiConfig {

}

impl TraktApiConfig {
    pub fn get_api_key(&self) -> &str {
        "0183a05ad97098d87287fe46da4ae286f434f32e8e951caad4cc147c947d79a3"
    }

    pub fn get_api_version(&self) -> &str {
        "2"
    }

    pub fn get_base_url(&self) -> &str {
        "https://api.trakt.tv"
    }
}

impl Default for TraktApiConfig {
    fn default() -> Self {
        Self {}
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraktListConfig {
    pub user: String,
    pub list_slug: String,
    pub category_name: String,
    #[serde(default)]
    pub content_type: TraktContentType,
    #[serde(default = "default_fuzzy_threshold")]
    pub fuzzy_match_threshold: u8, // Percentage (0-100)
}

fn default_fuzzy_threshold() -> u8 {
    80
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TraktContentType {
    Vod,
    Series,
    Both,
}

impl Default for TraktContentType {
    fn default() -> Self {
        Self::Both
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraktConfig {
    pub api: TraktApiConfig,
    pub lists: Vec<TraktListConfig>,
}

// API Response structures
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraktListItem {
    pub rank: Option<u32>,
    pub id: u64,
    pub listed_at: String,
    pub notes: Option<String>,
    #[serde(rename = "type")]
    pub item_type: String,
    pub movie: Option<TraktMovie>,
    pub show: Option<TraktShow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraktMovie {
    pub title: String,
    pub year: Option<u32>,
    pub ids: TraktIds,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraktShow {
    pub title: String,
    pub year: Option<u32>,
    pub ids: TraktIds,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraktIds {
    pub trakt: u32,
    pub slug: String,
    pub tvdb: Option<u32>,
    pub imdb: Option<String>,
    pub tmdb: Option<u32>,
    pub tvrage: Option<u32>,
}

// Internal matching structures
#[derive(Debug, Clone)]
pub struct TraktMatchItem {
    pub title: String,
    pub year: Option<u32>,
    pub tmdb_id: Option<u32>,
    pub trakt_id: u32,
    pub content_type: TraktContentType,
    pub rank: Option<u32>,
}

impl From<&TraktListItem> for TraktMatchItem {
    fn from(item: &TraktListItem) -> Self {
        match item.item_type.as_str() {
            "movie" => {
                if let Some(movie) = &item.movie {
                    TraktMatchItem {
                        title: movie.title.clone(),
                        year: movie.year,
                        tmdb_id: movie.ids.tmdb,
                        trakt_id: movie.ids.trakt,
                        content_type: TraktContentType::Vod,
                        rank: item.rank,
                    }
                } else {
                    TraktMatchItem::default()
                }
            }
            "show" => {
                if let Some(show) = &item.show {
                    TraktMatchItem {
                        title: show.title.clone(),
                        year: show.year,
                        tmdb_id: show.ids.tmdb,
                        trakt_id: show.ids.trakt,
                        content_type: TraktContentType::Series,
                        rank: item.rank,
                    }
                } else {
                    TraktMatchItem::default()
                }
            }
            _ => TraktMatchItem::default(),
        }
    }
}

impl Default for TraktMatchItem {
    fn default() -> Self {
        Self {
            title: String::new(),
            year: None,
            tmdb_id: None,
            trakt_id: 0,
            content_type: TraktContentType::Both,
            rank: None,
        }
    }
}

// Matching results
#[derive(Debug, Clone)]
pub struct TraktMatchResult {
    pub playlist_item_uuid: String,
    pub trakt_item: TraktMatchItem,
    pub match_score: f64,
    pub match_type: MatchType,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MatchType {
    TmdbExact,
    FuzzyTitle,
    FuzzyTitleYear,
} 