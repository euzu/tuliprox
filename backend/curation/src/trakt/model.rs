use crate::kernel::{CuratedMediaReference, CurationMediaKind};
use serde::{Deserialize, Serialize};
use shared::utils::is_blank_optional_string;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct TraktListItem {
    pub(super) id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) rank: Option<u32>,
    pub(super) listed_at: String,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub(super) notes: Option<String>,
    #[serde(rename = "type")]
    pub(super) item_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) movie: Option<TraktMovie>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) show: Option<TraktShow>,
}

impl TraktListItem {
    pub(super) fn from_movie_chart(movie: TraktMovie, rank: u32) -> Self {
        Self {
            id: u64::from(movie.ids.trakt),
            rank: Some(rank),
            listed_at: String::new(),
            notes: None,
            item_type: "movie".to_string(),
            movie: Some(movie),
            show: None,
        }
    }

    pub(super) fn from_show_chart(show: TraktShow, rank: u32) -> Self {
        Self {
            id: u64::from(show.ids.trakt),
            rank: Some(rank),
            listed_at: String::new(),
            notes: None,
            item_type: "show".to_string(),
            movie: None,
            show: Some(show),
        }
    }

    pub(super) fn into_curated_reference(self) -> Option<CuratedMediaReference> {
        match self.item_type.as_str() {
            "movie" => self.movie.map(|movie| {
                CuratedMediaReference::new(CurationMediaKind::Movie, movie.title, movie.year, movie.ids.tmdb, self.rank)
            }),
            "show" => self.show.map(|show| {
                CuratedMediaReference::new(CurationMediaKind::Series, show.title, show.year, show.ids.tmdb, self.rank)
            }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct TraktMovie {
    pub(super) ids: TraktIds,
    pub(super) title: String,
    pub(super) year: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct TraktShow {
    pub(super) ids: TraktIds,
    pub(super) title: String,
    pub(super) year: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct TraktIds {
    pub(super) trakt: u32,
    pub(super) slug: String,
    pub(super) tvdb: Option<u32>,
    pub(super) imdb: Option<String>,
    pub(super) tmdb: Option<u32>,
    pub(super) tvrage: Option<u32>,
}

#[derive(Deserialize)]
pub(super) struct TraktTrendingMovieItem {
    pub(super) movie: TraktMovie,
}

#[derive(Deserialize)]
pub(super) struct TraktTrendingShowItem {
    pub(super) show: TraktShow,
}
