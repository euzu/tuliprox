use crate::library::metadata::{Actor, MetadataSource, MovieMetadata, SeriesMetadata};
use crate::library::{EpisodeMetadata, SeasonMetadata, VideoClipMetadata};
use serde::{Deserialize, Serialize};

const TMDB_IMAGE_BASE_URL: &str = "https://image.tmdb.org/t/p/w500";

// Helper function: Vec<T> -> Option<Vec<T>>
fn some_if_nonempty<T>(v: Vec<T>) -> Option<Vec<T>> {
    if v.is_empty() { None } else { Some(v) }
}

fn resolve_tmdb_image_url(path: &str) -> String {
    let normalized = path.trim();
    if normalized.starts_with("http://") || normalized.starts_with("https://") {
        return normalized.to_string();
    }
    if normalized.starts_with("//") {
        return format!("https:{normalized}");
    }
    if normalized.starts_with('/') {
        return format!("{TMDB_IMAGE_BASE_URL}{normalized}");
    }
    format!("{TMDB_IMAGE_BASE_URL}/{normalized}")
}

// helper function: Crew-Filter
fn crew_names(credits: Option<&TmdbCredits>, jobs: &[&str]) -> Option<Vec<String>> {
    credits
        .and_then(|c| c.crew.as_ref())
        .map(|crew| {
            crew.iter()
                .filter(|crew| {
                    crew.job
                        .as_ref()
                        .is_some_and(|j| jobs.contains(&j.as_str()))
                })
                .map(|crew| crew.name.clone())
                .collect::<Vec<_>>()
        })
        .and_then(some_if_nonempty)
}

// TMDB API response structures
#[derive(Debug, Deserialize)]
pub struct TmdbSearchResponse {
    pub(crate) results: Vec<TmdbMovieSearchResult>,
}

#[derive(Debug, Deserialize)]
pub struct TmdbMovieSearchResult {
    pub(crate) id: u32,
    pub(crate) title: String,
    pub(crate) original_title: String,
}

#[derive(Debug, Deserialize)]
pub struct TmdbTvSearchResponse {
    pub(crate) results: Vec<TmdbTvSearchResult>,
}

#[derive(Debug, Deserialize)]
pub struct TmdbTvSearchResult {
    pub(crate) id: u32,
    pub(crate) name: String,
    pub(crate) original_name: String,
}


#[derive(Debug, Deserialize)]
pub struct TmdbExternalIds {
    imdb_id: Option<String>,
    tvdb_id: Option<u32>,
    // wikidata_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TmdbVideo {
    name: String, //"Official Trailer",
    key: String,
    site: String, // "YouTube",
    // size: u32,
    #[serde(rename = "type")]
    video_type: String, // "Trailer", "Teaser"
    // official: bool,

    // "iso_639_1": "en",
    // "iso_3166_1": "US",
    // "published_at": "2019-05-31T02:00:01.000Z",
    // "id": "5cf20e7dc3a368697a2032d1"
}

impl TmdbVideo {
    pub fn to_meta_data(&self) -> VideoClipMetadata {
        VideoClipMetadata {
            name: self.name.clone(),
            key: self.key.clone(),
            site: self.site.clone(),
            video_type: self.video_type.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct TmdbVideos {
    results: Option<Vec<TmdbVideo>>,
}

impl TmdbVideos {
    pub fn get_youtube(&self) -> Option<Vec<TmdbVideo>> {
        self.results.as_ref().map(|r| {
            r.iter()
                .filter(|v| v.site.eq_ignore_ascii_case("youtube"))
                .cloned()
                .collect()
        })
    }
}

#[derive(Debug, Deserialize)]
pub struct TmdbMovieDetails {
    id: u32,
    title: String,
    original_title: String,
    #[serde(default)]
    overview: String,
    tagline: Option<String>,
    #[serde(default)]
    release_date: String,
    #[serde(default)]
    runtime: u32,
    #[serde(default)]
    vote_average: f64,
    imdb_id: Option<String>,
    poster_path: Option<String>,
    backdrop_path: Option<String>,
    genres: Option<Vec<TmdbGenre>>,
    production_companies: Option<Vec<TmdbCompany>>,
    credits: Option<TmdbCredits>,
    external_ids: Option<TmdbExternalIds>,
    videos: Option<TmdbVideos>,
}

impl TmdbMovieDetails {
    pub(crate) fn to_meta_data(&self) -> MovieMetadata {
        MovieMetadata {
            title: self.title.clone(),
            original_title: Some(self.original_title.clone()),
            year: self.release_date.split('-').next().and_then(|y| y.parse().ok()),
            plot: Some(self.overview.clone()),
            tagline: self.tagline.clone(),
            runtime: Some(self.runtime),
            mpaa: None,
            tmdb_id: Some(self.id),
            imdb_id: self.imdb_id.clone().or_else(|| self.external_ids.as_ref().and_then(|ei| ei.imdb_id.clone())),
            tvdb_id: self.external_ids.as_ref().and_then(|ei| ei.tvdb_id),
            rating: Some(self.vote_average),
            genres: self.genres.as_ref().map(|list| list.iter().map(|g| g.name.clone()).collect()).and_then(some_if_nonempty),
            directors: crew_names(self.credits.as_ref(), &["Director"]),
            writers: crew_names(self.credits.as_ref(), &["Writer", "Screenplay"]),
            actors: self.credits
                .as_ref()
                .and_then(|c| c.cast.as_ref())
                .map(|cast| {
                    cast.iter()
                        .take(10)
                        .map(|actor| Actor {
                            name: actor.name.clone(),
                            role: Some(actor.character.clone()),
                            thumb: actor.profile_path
                                .as_ref()
                                .map(|p| resolve_tmdb_image_url(p)),
                        })
                        .collect::<Vec<_>>()
                })
                .and_then(some_if_nonempty),
            studios: self.production_companies.as_ref().map(|list| list.iter().map(|n| n.name.clone()).collect()).and_then(some_if_nonempty),
            poster: self.poster_path.as_ref().map(|p| resolve_tmdb_image_url(p)),
            fanart: self.backdrop_path.as_ref().map(|p| resolve_tmdb_image_url(p)),
            source: MetadataSource::Tmdb,
            last_updated: chrono::Utc::now().timestamp(),
            videos: self.videos
                .as_ref()
                .and_then(TmdbVideos::get_youtube)
                .map(|list| {
                    list.iter()
                        .map(TmdbVideo::to_meta_data)
                        .collect::<Vec<_>>()
                }).filter(|v| !v.is_empty()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TmdbSeriesInfoEpisodeDetails {
    pub id: u32,
    pub show_id: u32,

    pub air_date: Option<String>,
    pub episode_number: u32,
    // pub episode_type: String,

    pub name: String,
    pub overview: String,

    // pub production_code: String,
    pub runtime: Option<u32>,

    pub season_number: u32,

    pub still_path: Option<String>,

    pub vote_average: f64,
    // pub vote_count: u32,
    // pub crew: Vec<TmdbCrew>,
    // pub guest_stars: Vec<TmdbCrew>,
}

impl TmdbSeriesInfoEpisodeDetails {
    pub fn to_meta_data(&self) -> EpisodeMetadata {
        EpisodeMetadata {
            id: self.id,
            tmdb_id: self.show_id,
            title: self.name.clone(),
            season: self.season_number,
            episode: self.episode_number,
            aired: self.air_date.clone(),
            plot: if self.overview.is_empty() {None} else { Some(self.overview.clone())},
            runtime: self.runtime,
            rating: Some(self.vote_average),
            thumb: self.still_path.as_ref().map(|path| resolve_tmdb_image_url(path)),
            file_path: String::new(),
            file_size: 0,
            file_modified: 0,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct  TmdbSeriesInfoSeasonDetails {
    // #[serde(rename = "_id")]
    // pub internal_id: String,
    // pub id: u32,
    //
    // pub air_date: Option<String>,
    // pub name: String,
    // pub overview: String,
    // pub vote_average: f64,
    // pub poster_path: Option<String>,

    pub season_number: u32,
    pub episodes: Vec<TmdbSeriesInfoEpisodeDetails>,
    pub networks: Vec<TmdbNetwork>,
    pub credits: Option<TmdbCredits>,
}

#[derive(Debug, Deserialize)]
pub struct TmdbSeriesInfoDetails {
    id: u32,
    name: String,
    original_name: String,
    #[serde(default)]
    overview: String,
    #[serde(default)]
    first_air_date: String,
    // last_air_date: Option<String>,
    #[serde(default)]
    pub number_of_episodes: u32,
    #[serde(default)]
    pub(crate) number_of_seasons: u32,
    #[serde(default)]
    vote_average: f64,
    // #[serde(default)]
    // popularity: f64,
    poster_path: Option<String>,
    backdrop_path: Option<String>,
    #[serde(default)]
    status: String,
    genres: Option<Vec<TmdbGenre>>,
    networks: Option<Vec<TmdbNetwork>>,
    credits: Option<TmdbCredits>,
    // #[serde(default)]
    // adult: bool,
    // homepage: Option<String>,
    // #[serde(default)]
    // in_production: bool,
    // languages: Option<Vec<String>>,
    pub(crate) seasons: Option<Vec<TmdbSeason>>,
    // external_ids: Option<TmdbExternalIds>,
    videos: Option<TmdbVideos>,
}

impl TmdbSeriesInfoDetails {
    pub(crate) fn to_meta_data(&self) -> SeriesMetadata {
        SeriesMetadata {
            title: self.name.clone(),
            original_title: Some(self.original_name.clone()),
            year: self
                .first_air_date
                .split('-')
                .next()
                .and_then(|y| y.parse().ok()),
            plot: Some(self.overview.clone()),
            mpaa: None,
            imdb_id: None, // TMDB TV doesn't always provide IMDB ID
            tmdb_id: Some(self.id),
            tvdb_id: None, // TMDB doesn't provide TVDB ID directly
            rating: Some(self.vote_average),
            genres: self.genres.as_ref().and_then(|list| {
                let result: Vec<String> = list.iter().map(|g| g.name.clone()).collect();
                if result.is_empty() {
                    None
                } else {
                    Some(result)
                }
            }),
            directors: crew_names(self.credits.as_ref(), &["Director"]),
            writers: crew_names(self.credits.as_ref(), &["Writer", "Screenplay"]),
            actors: self.credits
                .as_ref()
                .and_then(|c| c.cast.as_ref())
                .map(|cast| {
                    cast.iter()
                        .take(10)
                        .map(|actor| Actor {
                            name: actor.name.clone(),
                            role: Some(actor.character.clone()),
                            thumb: actor.profile_path
                                .as_ref()
                                .map(|p| resolve_tmdb_image_url(p)),
                        })
                        .collect::<Vec<_>>()
                })
                .filter(|v| !v.is_empty()),
            studios: self.networks.as_ref().and_then(|list| {
                let result: Vec<String> = list.iter().map(|n| n.name.clone()).collect();
                if result.is_empty() {
                    None
                } else {
                    Some(result)
                }
            }),
            poster: self.poster_path.as_ref().map(|p| resolve_tmdb_image_url(p)),
            fanart: self.backdrop_path.as_ref().map(|p| resolve_tmdb_image_url(p)),
            status: Some(self.status.clone()),
            seasons: self.seasons.as_ref().map(|seasons| seasons.iter().map(TmdbSeason::to_meta_data).collect()),
            episodes: self.seasons
                .as_ref()
                .map(|season_list| {
                    season_list.iter()
                        .flat_map(|season| {
                            season.episodes
                                .as_ref()
                                .into_iter() // Option<&Vec<_>> -> Iterator<&Vec<_>>
                                .flat_map(|episode_list| episode_list.iter())
                                .map(TmdbSeriesInfoEpisodeDetails::to_meta_data)
                        })
                        .collect::<Vec<_>>()
                }),
            source: MetadataSource::Tmdb,
            number_of_episodes: self.number_of_episodes,
            number_of_seasons: self.number_of_seasons,
            last_updated: chrono::Utc::now().timestamp(),
            videos: self.videos
                .as_ref()
                .and_then(TmdbVideos::get_youtube)
                .map(|list| {
                    list.iter()
                        .map(TmdbVideo::to_meta_data)
                        .collect::<Vec<_>>()
                }).filter(|v| !v.is_empty()),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct TmdbGenre {
    name: String,
}

#[derive(Debug, Deserialize)]
pub struct TmdbCompany {
    name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TmdbNetwork {
    name: String,
    // id: u32,
    // logo_path: Option<String>,
    // origin_country: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TmdbCredits {
    cast: Option<Vec<TmdbCast>>,
    crew: Option<Vec<TmdbCrew>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TmdbCast {
    name: String,
    #[serde(default)]
    character: String,
    profile_path: Option<String>,

    // adult: bool,
    // gender: Option<u8>,
    // id: u32,
    // original_name: String,
    // known_for_department: String,
    // popularity: f32,
    // credit_id: String,
    // order: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TmdbCrew {
    pub name: String,
    pub job: Option<String>,

    // adult: Option<bool>,
    // gender: Option<u8>,
    // id: u32,
    // original_name: String,
    // known_for_department: String,
    // popularity: f32,
    // profile_path: Option<String>,
    // credit_id: String,
    // department: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TmdbSeason {
    id: u32,
    air_date: Option<String>,
    #[serde(default)]
    episode_count: u32,
    #[serde(default)]
    name: String,
    overview: Option<String>,
    poster_path: Option<String>,
    #[serde(default)]
    pub(crate) season_number: u32,
    #[serde(default)]
    vote_average: f64,

    pub episodes: Option<Vec<TmdbSeriesInfoEpisodeDetails>>,
    pub networks: Option<Vec<TmdbNetwork>>,
    pub credits: Option<TmdbCredits>,
}

impl TmdbSeason {

    // TODO maybe use Arc for episodes, networks, credits to avoid memory usage
    pub fn to_meta_data(&self) -> SeasonMetadata {
        SeasonMetadata {
            id: self.id,
            air_date: self.air_date.clone(),
            episode_count: self.episode_count,
            name: self.name.clone(),
            overview: self.overview.clone(),
            poster_path: self.poster_path.clone(),
            season_number: self.season_number,
            vote_average: self.vote_average,
            episodes: self.episodes.clone(),
            networks: self.networks.clone(),
            credits: self.credits.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{resolve_tmdb_image_url, TMDB_IMAGE_BASE_URL};

    #[test]
    fn test_resolve_tmdb_url_with_absolute_http() {
        let input = "http://cdn.example.org/image.jpg";
        assert_eq!(resolve_tmdb_image_url(input), input);
    }

    #[test]
    fn test_resolve_tmdb_url_with_absolute_https() {
        let input = "https://cdn.example.org/image.jpg";
        assert_eq!(resolve_tmdb_image_url(input), input);
    }

    #[test]
    fn test_resolve_tmdb_url_with_protocol_relative() {
        let input = "//cdn.example.org/image.jpg";
        assert_eq!(resolve_tmdb_image_url(input), String::from("https://cdn.example.org/image.jpg"));
    }

    #[test]
    fn test_resolve_tmdb_url_with_leading_slash() {
        let input = "/abc/def.jpg";
        assert_eq!(resolve_tmdb_image_url(input), format!("{TMDB_IMAGE_BASE_URL}{input}"));
    }

    #[test]
    fn test_resolve_tmdb_url_with_relative_path() {
        let input = "abc/def.jpg";
        assert_eq!(resolve_tmdb_image_url(input), format!("{TMDB_IMAGE_BASE_URL}/{input}"));
    }
}
