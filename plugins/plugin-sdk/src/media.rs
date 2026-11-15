//! Typed models for media catalog provider plugins (`media-provider-v1-1`).

use crate::bindings::{
    exports::tuliprox::plugin::media_provider_v1_1 as wit_provider, tuliprox::plugin::media_types_v1_1 as wit_types,
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProviderId {
    pub namespace: String,
    pub value: String,
}

impl From<wit_types::ProviderId> for ProviderId {
    fn from(w: wit_types::ProviderId) -> Self { Self { namespace: w.namespace, value: w.value } }
}

impl From<ProviderId> for wit_types::ProviderId {
    fn from(s: ProviderId) -> Self { Self { namespace: s.namespace, value: s.value } }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VideoTechnicalFacts {
    pub codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl From<wit_types::VideoTechnicalFacts> for VideoTechnicalFacts {
    fn from(w: wit_types::VideoTechnicalFacts) -> Self { Self { codec: w.codec, width: w.width, height: w.height } }
}

impl From<VideoTechnicalFacts> for wit_types::VideoTechnicalFacts {
    fn from(s: VideoTechnicalFacts) -> Self { Self { codec: s.codec, width: s.width, height: s.height } }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AudioTechnicalFacts {
    pub codec: Option<String>,
    pub channels: Option<u32>,
}

impl From<wit_types::AudioTechnicalFacts> for AudioTechnicalFacts {
    fn from(w: wit_types::AudioTechnicalFacts) -> Self { Self { codec: w.codec, channels: w.channels } }
}

impl From<AudioTechnicalFacts> for wit_types::AudioTechnicalFacts {
    fn from(s: AudioTechnicalFacts) -> Self { Self { codec: s.codec, channels: s.channels } }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TechnicalFacts {
    pub container: Option<String>,
    pub duration_secs: Option<u32>,
    pub bitrate: Option<u32>,
    pub video: Option<VideoTechnicalFacts>,
    pub audio: Option<AudioTechnicalFacts>,
}

impl From<wit_types::TechnicalFacts> for TechnicalFacts {
    fn from(w: wit_types::TechnicalFacts) -> Self {
        Self {
            container: w.container,
            duration_secs: w.duration_secs,
            bitrate: w.bitrate,
            video: w.video.map(Into::into),
            audio: w.audio.map(Into::into),
        }
    }
}

impl From<TechnicalFacts> for wit_types::TechnicalFacts {
    fn from(s: TechnicalFacts) -> Self {
        Self {
            container: s.container,
            duration_secs: s.duration_secs,
            bitrate: s.bitrate,
            video: s.video.map(Into::into),
            audio: s.audio.map(Into::into),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DescriptiveFacts {
    pub original_title: Option<String>,
    pub sort_title: Option<String>,
    pub summary: Option<String>,
    pub tagline: Option<String>,
    pub studio: Option<String>,
    pub network: Option<String>,
    pub content_rating: Option<String>,
    pub parental_age: Option<u32>,
    pub audience_rating: Option<String>,
    pub genres: Vec<String>,
    pub countries: Vec<String>,
    pub directors: Vec<String>,
    pub writers: Vec<String>,
    pub cast: Vec<String>,
    pub crew: Vec<String>,
}

impl From<wit_types::DescriptiveFacts> for DescriptiveFacts {
    fn from(w: wit_types::DescriptiveFacts) -> Self {
        Self {
            original_title: w.original_title,
            sort_title: w.sort_title,
            summary: w.summary,
            tagline: w.tagline,
            studio: w.studio,
            network: w.network,
            content_rating: w.content_rating,
            parental_age: w.parental_age,
            audience_rating: w.audience_rating,
            genres: w.genres,
            countries: w.countries,
            directors: w.directors,
            writers: w.writers,
            cast: w.cast,
            crew: w.crew,
        }
    }
}

impl From<DescriptiveFacts> for wit_types::DescriptiveFacts {
    fn from(s: DescriptiveFacts) -> Self {
        Self {
            original_title: s.original_title,
            sort_title: s.sort_title,
            summary: s.summary,
            tagline: s.tagline,
            studio: s.studio,
            network: s.network,
            content_rating: s.content_rating,
            parental_age: s.parental_age,
            audience_rating: s.audience_rating,
            genres: s.genres,
            countries: s.countries,
            directors: s.directors,
            writers: s.writers,
            cast: s.cast,
            crew: s.crew,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryKind {
    Movies,
    TvShows,
}

impl From<wit_types::LibraryKind> for LibraryKind {
    fn from(w: wit_types::LibraryKind) -> Self {
        match w {
            wit_types::LibraryKind::Movies => Self::Movies,
            wit_types::LibraryKind::TvShows => Self::TvShows,
        }
    }
}

impl From<LibraryKind> for wit_types::LibraryKind {
    fn from(s: LibraryKind) -> Self {
        match s {
            LibraryKind::Movies => Self::Movies,
            LibraryKind::TvShows => Self::TvShows,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaLibrary {
    pub id: String,
    pub name: String,
    pub kind: LibraryKind,
}

impl From<wit_types::MediaLibrary> for MediaLibrary {
    fn from(w: wit_types::MediaLibrary) -> Self { Self { id: w.id, name: w.name, kind: w.kind.into() } }
}

impl From<MediaLibrary> for wit_types::MediaLibrary {
    fn from(s: MediaLibrary) -> Self { Self { id: s.id, name: s.name, kind: s.kind.into() } }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaMovie {
    pub library_id: String,
    pub item_id: String,
    pub title: String,
    pub year: Option<u32>,
    pub release_date: Option<String>,
    pub source_version_hint: Option<String>,
    pub provider_ids: Vec<ProviderId>,
    pub descriptive: Option<DescriptiveFacts>,
    pub technical: Option<TechnicalFacts>,
    pub stream_key: Option<String>,
    pub image_key: Option<String>,
}

impl From<wit_types::MediaMovie> for MediaMovie {
    fn from(w: wit_types::MediaMovie) -> Self {
        Self {
            library_id: w.library_id,
            item_id: w.item_id,
            title: w.title,
            year: w.year,
            release_date: w.release_date,
            source_version_hint: w.source_version_hint,
            provider_ids: w.provider_ids.into_iter().map(Into::into).collect(),
            descriptive: w.descriptive.map(Into::into),
            technical: w.technical.map(Into::into),
            stream_key: w.stream_key,
            image_key: w.image_key,
        }
    }
}

impl From<MediaMovie> for wit_types::MediaMovie {
    fn from(s: MediaMovie) -> Self {
        Self {
            library_id: s.library_id,
            item_id: s.item_id,
            title: s.title,
            year: s.year,
            release_date: s.release_date,
            source_version_hint: s.source_version_hint,
            provider_ids: s.provider_ids.into_iter().map(Into::into).collect(),
            descriptive: s.descriptive.map(Into::into),
            technical: s.technical.map(Into::into),
            stream_key: s.stream_key,
            image_key: s.image_key,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaSeries {
    pub library_id: String,
    pub item_id: String,
    pub title: String,
    pub year: Option<u32>,
    pub release_date: Option<String>,
    pub source_version_hint: Option<String>,
    pub provider_ids: Vec<ProviderId>,
    pub descriptive: Option<DescriptiveFacts>,
    pub child_count: Option<u32>,
    pub episode_count: Option<u32>,
    pub image_key: Option<String>,
}

impl From<wit_types::MediaSeries> for MediaSeries {
    fn from(w: wit_types::MediaSeries) -> Self {
        Self {
            library_id: w.library_id,
            item_id: w.item_id,
            title: w.title,
            year: w.year,
            release_date: w.release_date,
            source_version_hint: w.source_version_hint,
            provider_ids: w.provider_ids.into_iter().map(Into::into).collect(),
            descriptive: w.descriptive.map(Into::into),
            child_count: w.child_count,
            episode_count: w.episode_count,
            image_key: w.image_key,
        }
    }
}

impl From<MediaSeries> for wit_types::MediaSeries {
    fn from(s: MediaSeries) -> Self {
        Self {
            library_id: s.library_id,
            item_id: s.item_id,
            title: s.title,
            year: s.year,
            release_date: s.release_date,
            source_version_hint: s.source_version_hint,
            provider_ids: s.provider_ids.into_iter().map(Into::into).collect(),
            descriptive: s.descriptive.map(Into::into),
            child_count: s.child_count,
            episode_count: s.episode_count,
            image_key: s.image_key,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaSeason {
    pub library_id: String,
    pub item_id: String,
    pub series_id: Option<String>,
    pub series_title: Option<String>,
    pub title: String,
    pub season: Option<u32>,
    pub year: Option<u32>,
    pub release_date: Option<String>,
    pub source_version_hint: Option<String>,
    pub provider_ids: Vec<ProviderId>,
    pub descriptive: Option<DescriptiveFacts>,
    pub episode_count: Option<u32>,
    pub image_key: Option<String>,
}

impl From<wit_types::MediaSeason> for MediaSeason {
    fn from(w: wit_types::MediaSeason) -> Self {
        Self {
            library_id: w.library_id,
            item_id: w.item_id,
            series_id: w.series_id,
            series_title: w.series_title,
            title: w.title,
            season: w.season,
            year: w.year,
            release_date: w.release_date,
            source_version_hint: w.source_version_hint,
            provider_ids: w.provider_ids.into_iter().map(Into::into).collect(),
            descriptive: w.descriptive.map(Into::into),
            episode_count: w.episode_count,
            image_key: w.image_key,
        }
    }
}

impl From<MediaSeason> for wit_types::MediaSeason {
    fn from(s: MediaSeason) -> Self {
        Self {
            library_id: s.library_id,
            item_id: s.item_id,
            series_id: s.series_id,
            series_title: s.series_title,
            title: s.title,
            season: s.season,
            year: s.year,
            release_date: s.release_date,
            source_version_hint: s.source_version_hint,
            provider_ids: s.provider_ids.into_iter().map(Into::into).collect(),
            descriptive: s.descriptive.map(Into::into),
            episode_count: s.episode_count,
            image_key: s.image_key,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaEpisode {
    pub library_id: String,
    pub item_id: String,
    pub season_id: Option<String>,
    pub series_id: Option<String>,
    pub series_title: Option<String>,
    pub title: String,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub release_date: Option<String>,
    pub source_version_hint: Option<String>,
    pub provider_ids: Vec<ProviderId>,
    pub descriptive: Option<DescriptiveFacts>,
    pub technical: Option<TechnicalFacts>,
    pub stream_key: Option<String>,
    pub image_key: Option<String>,
}

impl From<wit_types::MediaEpisode> for MediaEpisode {
    fn from(w: wit_types::MediaEpisode) -> Self {
        Self {
            library_id: w.library_id,
            item_id: w.item_id,
            season_id: w.season_id,
            series_id: w.series_id,
            series_title: w.series_title,
            title: w.title,
            season: w.season,
            episode: w.episode,
            release_date: w.release_date,
            source_version_hint: w.source_version_hint,
            provider_ids: w.provider_ids.into_iter().map(Into::into).collect(),
            descriptive: w.descriptive.map(Into::into),
            technical: w.technical.map(Into::into),
            stream_key: w.stream_key,
            image_key: w.image_key,
        }
    }
}

impl From<MediaEpisode> for wit_types::MediaEpisode {
    fn from(s: MediaEpisode) -> Self {
        Self {
            library_id: s.library_id,
            item_id: s.item_id,
            season_id: s.season_id,
            series_id: s.series_id,
            series_title: s.series_title,
            title: s.title,
            season: s.season,
            episode: s.episode,
            release_date: s.release_date,
            source_version_hint: s.source_version_hint,
            provider_ids: s.provider_ids.into_iter().map(Into::into).collect(),
            descriptive: s.descriptive.map(Into::into),
            technical: s.technical.map(Into::into),
            stream_key: s.stream_key,
            image_key: s.image_key,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaCatalog {
    pub libraries: Vec<MediaLibrary>,
    pub movies: Vec<MediaMovie>,
    pub series: Vec<MediaSeries>,
    pub seasons: Vec<MediaSeason>,
    pub episodes: Vec<MediaEpisode>,
}

impl From<wit_types::MediaCatalog> for MediaCatalog {
    fn from(w: wit_types::MediaCatalog) -> Self {
        Self {
            libraries: w.libraries.into_iter().map(Into::into).collect(),
            movies: w.movies.into_iter().map(Into::into).collect(),
            series: w.series.into_iter().map(Into::into).collect(),
            seasons: w.seasons.into_iter().map(Into::into).collect(),
            episodes: w.episodes.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<MediaCatalog> for wit_types::MediaCatalog {
    fn from(s: MediaCatalog) -> Self {
        Self {
            libraries: s.libraries.into_iter().map(Into::into).collect(),
            movies: s.movies.into_iter().map(Into::into).collect(),
            series: s.series.into_iter().map(Into::into).collect(),
            seasons: s.seasons.into_iter().map(Into::into).collect(),
            episodes: s.episodes.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaRequest {
    pub input_name: String,
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub options: Vec<(String, String)>,
    pub refresh: bool,
    pub last_success_at: Option<u64>,
    pub cursor: Option<String>,
}

impl From<wit_provider::MediaRequest> for MediaRequest {
    fn from(w: wit_provider::MediaRequest) -> Self {
        Self {
            input_name: w.input_name,
            url: w.url,
            username: w.username,
            password: w.password,
            options: w.options,
            refresh: w.refresh,
            last_success_at: w.last_success_at,
            cursor: w.cursor,
        }
    }
}

impl From<MediaRequest> for wit_provider::MediaRequest {
    fn from(s: MediaRequest) -> Self {
        Self {
            input_name: s.input_name,
            url: s.url,
            username: s.username,
            password: s.password,
            options: s.options,
            refresh: s.refresh,
            last_success_at: s.last_success_at,
            cursor: s.cursor,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaCatalogPage {
    pub catalog: MediaCatalog,
    pub next_cursor: Option<String>,
}

impl From<wit_provider::MediaCatalogPage> for MediaCatalogPage {
    fn from(w: wit_provider::MediaCatalogPage) -> Self {
        Self { catalog: w.catalog.into(), next_cursor: w.next_cursor }
    }
}

impl From<MediaCatalogPage> for wit_provider::MediaCatalogPage {
    fn from(s: MediaCatalogPage) -> Self { Self { catalog: s.catalog.into(), next_cursor: s.next_cursor } }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamRequest {
    pub input_name: String,
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub options: Vec<(String, String)>,
    pub item_id: String,
    pub stream_key: Option<String>,
    pub range: Option<String>,
}

impl From<wit_provider::StreamRequest> for StreamRequest {
    fn from(w: wit_provider::StreamRequest) -> Self {
        Self {
            input_name: w.input_name,
            url: w.url,
            username: w.username,
            password: w.password,
            options: w.options,
            item_id: w.item_id,
            stream_key: w.stream_key,
            range: w.range,
        }
    }
}

impl From<StreamRequest> for wit_provider::StreamRequest {
    fn from(s: StreamRequest) -> Self {
        Self {
            input_name: s.input_name,
            url: s.url,
            username: s.username,
            password: s.password,
            options: s.options,
            item_id: s.item_id,
            stream_key: s.stream_key,
            range: s.range,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StreamTarget {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub ttl_secs: Option<u32>,
}

impl From<wit_provider::StreamTarget> for StreamTarget {
    fn from(w: wit_provider::StreamTarget) -> Self { Self { url: w.url, headers: w.headers, ttl_secs: w.ttl_secs } }
}

impl From<StreamTarget> for wit_provider::StreamTarget {
    fn from(s: StreamTarget) -> Self { Self { url: s.url, headers: s.headers, ttl_secs: s.ttl_secs } }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRequest {
    pub input_name: String,
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub options: Vec<(String, String)>,
    pub item_id: String,
    pub image_key: String,
}

impl From<wit_provider::ImageRequest> for ImageRequest {
    fn from(w: wit_provider::ImageRequest) -> Self {
        Self {
            input_name: w.input_name,
            url: w.url,
            username: w.username,
            password: w.password,
            options: w.options,
            item_id: w.item_id,
            image_key: w.image_key,
        }
    }
}

impl From<ImageRequest> for wit_provider::ImageRequest {
    fn from(s: ImageRequest) -> Self {
        Self {
            input_name: s.input_name,
            url: s.url,
            username: s.username,
            password: s.password,
            options: s.options,
            item_id: s.item_id,
            image_key: s.image_key,
        }
    }
}
