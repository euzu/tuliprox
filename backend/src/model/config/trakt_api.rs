use serde::{Deserialize, Serialize};
use shared::model::TraktContentType;
use crate::utils::normalize_title_for_matching;

// Trakt API Response structures
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraktListItem {
    pub id: u64,
    pub rank: Option<u32>,
    pub listed_at: String,
    pub notes: Option<String>,
    #[serde(rename = "type")]
    pub item_type: String,
    pub movie: Option<TraktMovie>,
    pub show: Option<TraktShow>,
    #[serde(skip)]
    pub content_type: TraktContentType,
}

impl TraktListItem {
    pub fn prepare(&mut self) {
        self.content_type = match self.item_type.as_str() {
            "movie" => if self.movie.is_some() { TraktContentType::Vod } else { TraktContentType::Both },
            "show" => if self.show.is_some() { TraktContentType::Series } else { TraktContentType::Both },
            _ => TraktContentType::Both,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraktMovie {
    pub ids: TraktIds,
    pub title: String,
    pub year: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraktShow {
    pub ids: TraktIds,
    pub title: String,
    pub year: Option<u32>,
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
pub struct TraktMatchItem<'a> {
    pub title: &'a str,
    pub normalized_title: String,
    pub year: Option<u32>,
    pub tmdb_id: Option<u32>,
    pub trakt_id: u32,
    pub content_type: TraktContentType,
    pub rank: Option<u32>,
}

impl<'a> TraktMatchItem<'a> {
    pub fn from_trakt_list_item(item: &'a TraktListItem) -> Option<Self> {
        match item.item_type.as_str() {
            "movie" => {
                item.movie.as_ref().map(|movie| TraktMatchItem {
                    title: movie.title.as_str(),
                    normalized_title: normalize_title_for_matching(movie.title.as_str()),
                    year: movie.year,
                    tmdb_id: movie.ids.tmdb,
                    trakt_id: movie.ids.trakt,
                    content_type: TraktContentType::Vod,
                    rank: item.rank,
                })
            }
            "show" => {
                item.show.as_ref().map(|show| TraktMatchItem {
                    title: show.title.as_str(),
                    normalized_title: normalize_title_for_matching(show.title.as_str()),
                    year: show.year,
                    tmdb_id: show.ids.tmdb,
                    trakt_id: show.ids.trakt,
                    content_type: TraktContentType::Series,
                    rank: item.rank,
                })
            }
            _ => None,
        }
    }
}