use crate::utils::Internable;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum StreamProperties {
    Live(Box<LiveStreamProperties>),
    Video(Box<VideoStreamProperties>),
    Series(Box<SeriesStreamProperties>),
    Episode(Box<EpisodeStreamProperties>),
}

impl StreamProperties {
    fn episode_value<T, F>(&self, selector: F) -> Option<T>
    where
        F: FnOnce(&EpisodeStreamProperties) -> T,
    {
        match self {
            StreamProperties::Episode(episode) => Some(selector(episode)),
            StreamProperties::Live(_) | StreamProperties::Video(_) | StreamProperties::Series(_) => None,
        }
    }

    /// The genre, wherever this variant happens to keep it.
    ///
    /// Video keeps it under `details`, Series keeps it inline, and Live and
    /// Episode do not have one. That four-arm match was written out at every
    /// read site and in three macros; this is the single copy.
    #[must_use]
    pub fn genre(&self) -> Option<&Arc<str>> {
        match self {
            Self::Video(video) => video.details.as_ref().and_then(|details| details.genre.as_ref()),
            Self::Series(series) => series.genre.as_ref(),
            Self::Live(_) | Self::Episode(_) => None,
        }
    }

    /// Sets the genre, creating Video `details` if absent.
    ///
    /// Returns `false` for the variants that have no genre. Does **not** create
    /// the `StreamProperties` itself -- that needs the header to build from, so
    /// it stays with the caller.
    pub fn set_genre(&mut self, value: &str) -> bool {
        match self {
            Self::Video(video) => {
                match &mut video.details {
                    Some(details) => details.genre = Some(value.intern()),
                    None => {
                        video.details = Some(VideoStreamDetailProperties {
                            genre: Some(value.intern()),
                            ..VideoStreamDetailProperties::default()
                        });
                    }
                }
                true
            }
            Self::Series(series) => {
                series.genre = Some(value.intern());
                true
            }
            Self::Live(_) | Self::Episode(_) => false,
        }
    }

    pub fn has_details(&self) -> bool {
        match self {
            StreamProperties::Video(video) => video.details.is_some(),
            StreamProperties::Series(series) => series.details.is_some(),
            StreamProperties::Live(live) => {
                live.video.is_some()
                    || live.audio.is_some()
                    || live.last_probed_timestamp.is_some()
                    || live.last_success_timestamp.is_some()
                    || live.catchup.is_some()
                    || live.bitrate > 0
            }
            StreamProperties::Episode(_) => false,
        }
    }

    pub fn prepare(&mut self) {
        match self {
            StreamProperties::Live(live) => {
                live.epg_channel_id = live
                    .epg_channel_id
                    .as_ref()
                    .filter(|epg_id| !epg_id.trim().is_empty())
                    .map(|epg_id| epg_id.to_lowercase().intern())
                    .or(live.epg_channel_id.clone());
                if live.catchup.as_ref().is_some_and(CatchupProperties::is_empty) {
                    live.catchup = None;
                }
            }
            StreamProperties::Video(_) => {}
            StreamProperties::Series(_) => {}
            StreamProperties::Episode(_) => {}
        }
    }

    pub fn get_category_id(&self) -> u32 {
        match self {
            StreamProperties::Live(live) => live.category_id,
            StreamProperties::Video(video) => video.category_id,
            StreamProperties::Series(series) => series.category_id,
            StreamProperties::Episode(_episode) => 0,
        }
    }

    pub fn get_stream_id(&self) -> u32 {
        match self {
            StreamProperties::Live(live) => live.stream_id,
            StreamProperties::Video(video) => video.stream_id,
            StreamProperties::Series(series) => series.series_id,
            StreamProperties::Episode(episode) => episode.episode_id,
        }
    }

    pub fn get_stream_icon(&self) -> Arc<str> {
        match self {
            StreamProperties::Live(live) => Arc::clone(&live.stream_icon),
            StreamProperties::Video(video) => Arc::clone(&video.stream_icon),
            StreamProperties::Series(series) => Arc::clone(&series.cover),
            StreamProperties::Episode(episode) => Arc::clone(&episode.movie_image),
        }
    }

    pub fn get_name(&self) -> Arc<str> {
        match self {
            StreamProperties::Live(live) => Arc::clone(&live.name),
            StreamProperties::Video(video) => Arc::clone(&video.name),
            StreamProperties::Series(series) => Arc::clone(&series.name),
            StreamProperties::Episode(_episode) => "".intern(),
        }
    }

    pub fn get_epg_channel_id(&self) -> Option<Arc<str>> {
        match self {
            StreamProperties::Live(live) => live.epg_channel_id.clone(),
            StreamProperties::Video(_) => None,
            StreamProperties::Series(_) => None,
            StreamProperties::Episode(_) => None,
        }
    }

    pub fn get_tmdb_id(&self) -> Option<u32> {
        match self {
            StreamProperties::Live(_) => None,
            StreamProperties::Video(video) => video.tmdb,
            StreamProperties::Series(series) => series.tmdb,
            StreamProperties::Episode(episode) => episode.tmdb,
        }
    }

    pub fn get_release_date(&self) -> Option<Arc<str>> {
        match self {
            StreamProperties::Live(_) => None,
            StreamProperties::Video(video) => {
                video.details.as_ref().and_then(|d| d.release_date.as_ref().map(Arc::clone))
            }
            StreamProperties::Series(series) => series.release_date.as_ref().map(Arc::clone),
            StreamProperties::Episode(episode) => episode.release_date.as_ref().map(Arc::clone),
        }
    }

    pub fn get_added(&self) -> Option<Arc<str>> {
        match self {
            StreamProperties::Live(_) => None,
            StreamProperties::Video(video) => non_empty_arc(&video.added),
            StreamProperties::Series(series) => {
                first_series_episode_value(series, |episode| non_empty_arc(&episode.added))
            }
            StreamProperties::Episode(episode) => episode.added.as_ref().map(Arc::clone),
        }
    }

    pub fn get_container_extension(&self) -> Option<Arc<str>> {
        match self {
            StreamProperties::Live(_) => None,
            StreamProperties::Video(video) => non_empty_arc(&video.container_extension),
            StreamProperties::Series(series) => {
                first_series_episode_value(series, |episode| non_empty_arc(&episode.container_extension))
            }
            StreamProperties::Episode(episode) => non_empty_arc(&episode.container_extension),
        }
    }

    pub fn get_direct_source(&self) -> Option<Arc<str>> {
        match self {
            StreamProperties::Live(_) => None,
            StreamProperties::Video(video) => non_empty_arc(&video.direct_source),
            StreamProperties::Series(series) => {
                first_series_episode_value(series, |episode| non_empty_arc(&episode.direct_source))
            }
            StreamProperties::Episode(_episode) => None,
        }
    }

    pub fn get_season(&self) -> Option<u32> { self.episode_value(|episode| episode.season) }

    pub fn get_episode(&self) -> Option<u32> { self.episode_value(|episode| episode.episode) }

    pub fn get_last_modified(&self) -> Option<u64> {
        match self {
            StreamProperties::Live(_) => None,
            StreamProperties::Video(video) => video.added.parse::<u64>().ok(),
            StreamProperties::Series(series) => series.last_modified.as_ref().and_then(|v| v.parse::<u64>().ok()),
            StreamProperties::Episode(episode) => episode.added.as_ref().and_then(|v| v.parse::<u64>().ok()),
        }
    }

    pub fn resolve_resource_url(&self, field: &str) -> Option<Arc<str>> {
        if field.starts_with("backdrop_path") {
            if let StreamProperties::Series(series) = self {
                if let Some(backdrop) = series.backdrop_path.as_ref() {
                    if let Some(url) = backdrop.first() {
                        return Some(Arc::clone(url));
                    }
                }
            }
            return None;
        } else if field.starts_with("nfo_backdrop_path") {
            if let StreamProperties::Video(video) = self {
                if let Some(details) = video.details.as_ref() {
                    if let Some(backdrop) = details.backdrop_path.as_ref() {
                        if let Some(url) = backdrop.first() {
                            return Some(Arc::clone(url));
                        }
                    }
                }
            }
            return None;
        }

        if field == "cover" {
            if let StreamProperties::Series(series) = self {
                return Some(Arc::clone(&series.cover));
            }
            return None;
        }
        if field == "logo" || field == "logo_small" {
            return match self {
                StreamProperties::Live(live) => Some(Arc::clone(&live.stream_icon)),
                StreamProperties::Video(video) => Some(Arc::clone(&video.stream_icon)),
                StreamProperties::Series(series) => Some(Arc::clone(&series.cover)),
                StreamProperties::Episode(episode) => Some(Arc::clone(&episode.movie_image)),
            };
        }
        if field == "movie_image" {
            if let StreamProperties::Episode(episode) = self {
                return Some(Arc::clone(&episode.movie_image));
            }
            return None;
        }
        if field == "nfo_cover_big" {
            if let StreamProperties::Video(video) = self {
                if let Some(details) = video.details.as_ref() {
                    if let Some(cover_big) = details.cover_big.as_ref() {
                        return Some(Arc::clone(cover_big));
                    }
                }
            }
        }

        if field == "nfo_movie_image" {
            if let StreamProperties::Video(video) = self {
                if let Some(details) = video.details.as_ref() {
                    if let Some(movie_image) = details.movie_image.as_ref() {
                        return Some(Arc::clone(movie_image));
                    }
                }
            }
            return None;
        }

        if field.starts_with("nfo_s_") {
            if let Some((season_num, field)) = parse_season_field(field) {
                if let StreamProperties::Series(series) = self {
                    if let Some(details) = series.details.as_ref() {
                        if let Some(seasons) = details.seasons.as_ref() {
                            for season in seasons {
                                if season.season_number == season_num {
                                    if field == "cover" {
                                        return season.cover.as_ref().map(Arc::clone);
                                    }
                                    if field == "cover_tmdb" {
                                        return season.cover_tmdb.as_ref().map(Arc::clone);
                                    }
                                    if field == "cover_big" {
                                        return season.cover_big.as_ref().map(Arc::clone);
                                    }
                                    if field == "overview" {
                                        return season.overview.as_ref().map(Arc::clone);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if field.starts_with("nfo_ep_") {
            if let Some((season, episode_num, field)) = parse_season_episode_field(field) {
                if let StreamProperties::Series(series) = self {
                    if let Some(details) = series.details.as_ref() {
                        if let Some(episodes) = details.episodes.as_ref() {
                            for episode in episodes {
                                if episode.season == season
                                    && episode_num == episode.episode_num
                                    && field == "movie_image"
                                {
                                    return Some(Arc::clone(&episode.movie_image));
                                }
                            }
                        }
                    }
                }
            }
        }

        None
    }
}

#[cfg(test)]
mod tests;

mod catchup;
mod live;
mod resources;
mod series;
mod video;
pub use catchup::{CatchupAttribute, CatchupProperties};
pub use live::LiveStreamProperties;
pub use resources::is_resource_field_name;
use series::{first_series_episode_value, non_empty_arc, parse_season_episode_field, parse_season_field};
pub use series::{
    normalize_episode_title, EpisodeStreamProperties, SeriesStreamDetailEpisodeProperties,
    SeriesStreamDetailProperties, SeriesStreamDetailSeasonProperties, SeriesStreamProperties,
};
pub use video::{VideoStreamDetailProperties, VideoStreamProperties};
