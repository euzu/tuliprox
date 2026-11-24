use crate::model::XtreamCluster;

pub const XC_LIVE_ID: &str = "live_id";
pub const XC_VOD_ID: &str = "vod_id";
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
pub const XC_PROP_BACKDROP_PATH: &str = "backdrop_path";
pub const XC_PROP_COVER: &str = "cover";
pub const XC_TAG_CATEGORY_NAME: &str = "category_name";

/// First stream id a recording is assigned in the Xtream VOD namespace of an API user.
/// Xtream players parse stream ids as signed 32-bit integers, so the whole range stays
/// below `i32::MAX`; no playlist virtual id reaches it because those count from one.
pub const RECORDING_VOD_ID_BASE: u32 = 1_000_000_000;

/// Category id under which API users find their recordings in the VOD category list.
/// Reserved: when a provider category already carries it, that category wins and the
/// synthetic one is omitted.
pub const RECORDINGS_VOD_CATEGORY_ID: u32 = 2_000_000_000;

/// Display name of the synthetic recording category.
pub const RECORDINGS_VOD_CATEGORY_NAME: &str = "Recordings";

pub const VIDEO_STREAM_FIELDS: [&str; 14] = [
    "release_date",
    "cast",
    "director",
    "episode_run_time",
    "genre",
    "stream_type",
    "title",
    "year",
    "youtube_trailer",
    "trailer",
    "plot",
    "rating_5based",
    "stream_icon",
    "container_extension",
];

pub const SERIES_STREAM_FIELDS: [&str; 15] = [
    XC_PROP_BACKDROP_PATH,
    "cast",
    XC_PROP_COVER,
    "director",
    "episode_run_time",
    "genre",
    "last_modified",
    "name",
    "plot",
    "rating_5based",
    "stream_type",
    "title",
    "year",
    "youtube_trailer",
    "trailer",
];

pub const XTREAM_VOD_REWRITE_URL_PROPS: [&str; 1] = [XC_PROP_COVER];

pub const XTREAM_CLUSTER: [XtreamCluster; 3] = [XtreamCluster::Live, XtreamCluster::Video, XtreamCluster::Series];
