pub const XC_LIVE_ID: &str = "live_id";
pub const XC_VOO_ID: &str = "vod_id";
pub const XC_SERIES_ID: &str = "series_id";

pub const XC_ACTION_GET_SERIES_INFO: &str = "get_series_info";
pub const XC_ACTION_GET_VOD_INFO: &str = "get_vod_info";
pub const XC_ACTION_GET_LIVE_INFO: &str = "get_live_info";
pub const XC_ACTION_GET_SERIES: &str = "get_series";
pub const XC_ACTION_GET_LIVE_CATEGORIES: &str = "get_live_categories";
pub const XC_ACTION_GET_VOD_CATEGORIES: &str = "get_vod_categories";
pub const XC_ACTION_GET_SERIES_CATEGORIES: &str = "get_series_categories";
pub const XC_ACTION_GET_LIVE_STREAMS: &str = "get_live_streams";
pub const XC_ACTION_GET_VOD_STREAMS: &str = "get_vod_streams";
pub const XC_ACTION_GET_ACCOUNT_INFO: &str = "get_account_info";
pub const XC_ACTION_GET_EPG: &str = "get_epg";
pub const XC_ACTION_GET_SHORT_EPG: &str = "get_short_epg";
pub const XC_ACTION_GET_CATCHUP_TABLE: &str = "get_simple_data_table";
pub const XC_TAG_ID: &str = "id";
pub const XC_TAG_CATEGORY_ID: &str = "category_id";
pub const XC_TAG_STREAM_ID: &str = "stream_id";
pub const XC_TAG_EPG_LISTINGS: &str = "epg_listings";
pub const XC_INFO_RESOURCE_PREFIX: &str = "nfo_";
pub const XC_INFO_RESOURCE_PREFIX_EPISODE: &str = "nfo_ep_";
pub const XC_SEASON_RESOURCE_PREFIX: &str = "ssn_";
pub const XC_PROP_BACKDROP_PATH: &str = "backdrop_path";
pub const XC_PROP_COVER: &str = "cover";
pub const XC_TAG_CATEGORY_IDS: &str = "category_ids";
pub const XC_TAG_CATEGORY_NAME: &str = "category_name";
pub const XC_TAG_DIRECT_SOURCE: &str = "direct_source";
pub const XC_TAG_PARENT_ID: &str = "parent_id";
pub const XC_TAG_MOVIE_DATA: &str = "movie_data";
pub const XC_TAG_INFO_DATA: &str = "info";
pub const XC_TAG_SEASONS_DATA: &str = "seasons";
pub const XC_TAG_EPISODES: &str = "episodes";
pub const XC_TAG_VOD_INFO_INFO: &str = "info";
pub const XC_TAG_VOD_INFO_MOVIE_DATA: &str = "movie_data";
pub const XC_TAG_VOD_INFO_TMDB_ID: &str = "tmdb_id";
pub const XC_TAG_VOD_INFO_STREAM_ID: &str = "stream_id";
pub const XC_TAG_VOD_INFO_ADDED: &str = "added";
pub const XC_TAG_VOD_INFO_RELEASEDATE: &str = "release_date";

pub const XC_FILE_SERIES_INFO: &str = "xtream_series_info";
pub const XC_FILE_VOD_INFO: &str = "xtream_vod_info";
pub const XC_FILE_SERIES_EPISODE_RECORD: &str = "series_episode_record";
pub const XC_TAG_SERIES_INFO_LAST_MODIFIED: &str = "last_modified";


pub const LIVE_STREAM_FIELDS: &[&str] = &[];

pub const VIDEO_STREAM_FIELDS: &[&str] = &[
    "release_date", "cast",
    "director", "episode_run_time", "genre",
    "stream_type", "title", "year", "youtube_trailer", "trailer",
    "plot", "rating_5based", "stream_icon", "container_extension"
];

pub const SERIES_STREAM_FIELDS: &[&str] = &[
    XC_PROP_BACKDROP_PATH, "cast", XC_PROP_COVER, "director", "episode_run_time", "genre",
    "last_modified", "name", "plot", "rating_5based",
    "stream_type", "title", "year", "youtube_trailer", "trailer"
];

pub const XTREAM_VOD_REWRITE_URL_PROPS: &[&str] = &[XC_PROP_COVER];

