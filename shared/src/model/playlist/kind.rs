use super::PlaylistItemTypeSet;
use crate::{error::TuliproxError, model::xtream_const, utils::Internable};
use serde::{Deserialize, Serialize};
use std::{
    fmt::{Display, Formatter},
    str::FromStr,
    sync::Arc,
};
use strum_macros::{AsRefStr, Display, EnumIter, EnumString};

// https://de.wikipedia.org/wiki/M3U
// https://siptv.eu/howto/playlist.html

#[derive(Debug, Copy, Clone, Eq, Hash, PartialEq, Serialize, Deserialize, Default, Display, EnumString, AsRefStr)]
#[repr(u8)]
pub enum XtreamCluster {
    #[default]
    #[strum(serialize = "Live", serialize = "live")]
    Live = 1,

    #[strum(serialize = "Video", serialize = "movie", serialize = "vod", serialize = "video")]
    Video = 2,

    #[strum(serialize = "Series", serialize = "series")]
    Series = 3,
}

impl XtreamCluster {
    pub fn as_str(&self) -> &str { self.as_ref() }

    /// True when this cluster is the Xtream `Series` cluster. Used in
    /// bucket-key computations and dispatch sites that previously
    /// spelled out `== XtreamCluster::Series` inline.
    pub fn is_series(self) -> bool { matches!(self, Self::Series) }

    pub fn as_stream_type(&self) -> &str {
        match self {
            Self::Live => "live",
            Self::Video => "movie",
            Self::Series => "series",
        }
    }

    /// Returns the xtream `player_api` info action and the stream-id query field for this cluster.
    ///
    /// Keeps the per-cluster `(action, id_field)` mapping attached to the enum so call sites read a
    /// single property instead of re-deriving it with a local `match`.
    pub fn info_action_and_id_field(&self) -> (&'static str, &'static str) {
        match self {
            Self::Live => (xtream_const::XC_ACTION_GET_LIVE_INFO, xtream_const::XC_LIVE_ID),
            Self::Video => (xtream_const::XC_ACTION_GET_VOD_INFO, xtream_const::XC_VOD_ID),
            Self::Series => (xtream_const::XC_ACTION_GET_SERIES_INFO, xtream_const::XC_SERIES_ID),
        }
    }
}

/// Every item type belongs to exactly one cluster, so this cannot fail.
///
/// This used to be a `TryFrom` whose every arm returned `Ok`, and the phantom
/// error spread `.unwrap_or(Live)`, `.unwrap_or_default()` and `.ok()` across 17
/// call sites in four crates. See [`PlaylistItemType::cluster`].
impl From<PlaylistItemType> for XtreamCluster {
    #[inline]
    fn from(item_type: PlaylistItemType) -> Self { item_type.cluster() }
}

#[derive(Debug, Copy, Clone, Eq, Hash, PartialEq, Serialize, Deserialize, Default, EnumIter)]
#[repr(u8)]
pub enum PlaylistItemType {
    #[default]
    #[serde(alias = "live")]
    Live = 1,
    #[serde(alias = "video")]
    Video = 2,
    #[serde(alias = "series")]
    Series = 3, //  xtream series description
    #[serde(alias = "series_info")]
    SeriesInfo = 4, //  xtream series info fetched for series description
    #[serde(alias = "catchup")]
    Catchup = 5,
    #[serde(alias = "live_unknown")]
    LiveUnknown = 6, // No Provider id
    #[serde(alias = "live_hls")]
    LiveHls = 7, // m3u8 entry
    #[serde(alias = "live_dash")]
    LiveDash = 8, // mpd
    #[serde(alias = "local_video")]
    LocalVideo = 9,
    #[serde(alias = "local_series")]
    LocalSeries = 10,
    #[serde(alias = "local_series_info")]
    LocalSeriesInfo = 11,
}

impl From<XtreamCluster> for PlaylistItemType {
    fn from(xtream_cluster: XtreamCluster) -> Self {
        match xtream_cluster {
            XtreamCluster::Live => Self::Live,
            XtreamCluster::Video => Self::Video,
            XtreamCluster::Series => Self::SeriesInfo,
        }
    }
}

impl FromStr for PlaylistItemType {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Live" => Ok(PlaylistItemType::Live),
            "Video" => Ok(PlaylistItemType::Video),
            "LocalVideo" => Ok(PlaylistItemType::LocalVideo),
            "Series" => Ok(PlaylistItemType::Series),
            "SeriesInfo" => Ok(PlaylistItemType::SeriesInfo),
            "LocalSeries" => Ok(PlaylistItemType::LocalSeries),
            "LocalSeriesInfo" => Ok(PlaylistItemType::LocalSeriesInfo),
            "Catchup" => Ok(PlaylistItemType::Catchup),
            "LiveUnknown" => Ok(PlaylistItemType::LiveUnknown),
            "LiveHls" => Ok(PlaylistItemType::LiveHls),
            "LiveDash" => Ok(PlaylistItemType::LiveDash),
            _ => Err(TuliproxError::Config(format!("Invalid PlaylistItemType: {s}"))),
        }
    }
}

impl PlaylistItemType {
    pub(super) const LIVE: &'static str = "live";
    pub(super) const VIDEO: &'static str = "video";
    pub(super) const SERIES: &'static str = "series";
    pub(super) const SERIES_INFO: &'static str = "series-info";
    pub(super) const CATCHUP: &'static str = "catchup";

    pub fn is_local(&self) -> bool {
        matches!(self, PlaylistItemType::LocalVideo | PlaylistItemType::LocalSeries | PlaylistItemType::LocalSeriesInfo)
    }

    pub fn is_live(&self) -> bool {
        matches!(
            self,
            PlaylistItemType::Live
                | PlaylistItemType::LiveDash
                | PlaylistItemType::LiveHls
                | PlaylistItemType::LiveUnknown
        )
    }

    pub fn is_live_adaptive(&self) -> bool { matches!(self, PlaylistItemType::LiveHls | PlaylistItemType::LiveDash) }

    /// True for VOD item types (`Video` or `LocalVideo`).
    pub fn is_video(&self) -> bool { matches!(self, PlaylistItemType::Video | PlaylistItemType::LocalVideo) }

    /// True for concrete series item types (`Series` or `LocalSeries`); excludes the `SeriesInfo` containers.
    pub fn is_series(&self) -> bool { matches!(self, PlaylistItemType::Series | PlaylistItemType::LocalSeries) }

    /// Controls address tracking only.
    /// Do not use this to decide whether a playback request should use session-based admission
    /// or whether a logical playback must stay on the same provider account.
    pub fn uses_socket_bound_session(&self) -> bool {
        matches!(self, PlaylistItemType::Live | PlaylistItemType::LiveUnknown)
    }

    /// Controls whether follow-up requests for the same logical playback must stay on the
    /// same provider account.
    /// This is separate from both session admission and socket binding.
    pub fn requires_provider_affinity(&self) -> bool {
        matches!(
            self,
            PlaylistItemType::LiveHls
                | PlaylistItemType::LiveDash
                | PlaylistItemType::Video
                | PlaylistItemType::LocalVideo
                | PlaylistItemType::Series
                | PlaylistItemType::SeriesInfo
                | PlaylistItemType::LocalSeries
                | PlaylistItemType::LocalSeriesInfo
                | PlaylistItemType::Catchup
        )
    }

    pub fn as_u8(self) -> u8 { self as u8 }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Live | Self::LiveHls | Self::LiveDash | Self::LiveUnknown => Self::LIVE,
            Self::Video | Self::LocalVideo => Self::VIDEO,
            Self::Series | Self::LocalSeries => Self::SERIES,
            Self::SeriesInfo | Self::LocalSeriesInfo => Self::SERIES_INFO,
            Self::Catchup => Self::CATCHUP,
        }
    }

    /// Returns a cached interned `Arc<str>` of this type's label.
    ///
    /// `intern()` performs an interner hash-map lookup on every call. Because the
    /// label is one of only five fixed values, this caches the interned `Arc` per
    /// label in a `OnceLock` and returns a cheap `Arc::clone`, avoiding repeated
    /// interner lookups on hot sort/filter paths.
    pub fn interned_label(&self) -> Arc<str> {
        static CACHE: [std::sync::OnceLock<Arc<str>>; 5] = [const { std::sync::OnceLock::new() }; 5];
        let idx = match self {
            Self::Live | Self::LiveHls | Self::LiveDash | Self::LiveUnknown => 0,
            Self::Video | Self::LocalVideo => 1,
            Self::Series | Self::LocalSeries => 2,
            Self::SeriesInfo | Self::LocalSeriesInfo => 3,
            Self::Catchup => 4,
        };
        Arc::clone(CACHE[idx].get_or_init(|| self.as_str().intern()))
    }

    /// The cluster this item type belongs to.
    ///
    /// The one place the item-type-to-cluster relation is written down. It used
    /// to be encoded twice -- here and in a `TryFrom` impl -- with nothing
    /// keeping the two in agreement.
    #[inline]
    pub const fn cluster(self) -> XtreamCluster {
        match self {
            Self::Live | Self::LiveHls | Self::LiveDash | Self::LiveUnknown => XtreamCluster::Live,
            Self::Catchup | Self::Video | Self::LocalVideo => XtreamCluster::Video,
            Self::Series | Self::LocalSeries | Self::SeriesInfo | Self::LocalSeriesInfo => XtreamCluster::Series,
        }
    }

    #[inline]
    pub const fn is_cluster(&self, cluster: XtreamCluster) -> bool { self.cluster() as u8 == cluster as u8 }
}

impl Display for PlaylistItemType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result { write!(f, "{}", self.as_str()) }
}

impl Internable for PlaylistItemType {
    fn intern(self) -> Arc<str> { self.as_str().intern() }
}

impl Internable for XtreamCluster {
    fn intern(self) -> Arc<str> { self.as_str().intern() }
}

impl PlaylistItemTypeSet {
    #[inline]
    pub fn empty() -> Self { Self(0) }

    #[inline]
    pub fn from_item(item: PlaylistItemType) -> Self {
        let bit = 1u16 << ((item as u8) - 1);
        Self(bit)
    }

    #[inline]
    pub fn insert(&mut self, item: PlaylistItemType) { self.0 |= 1u16 << ((item as u8) - 1); }

    #[inline]
    pub fn remove(&mut self, item: PlaylistItemType) { self.0 &= !(1u16 << ((item as u8) - 1)); }

    #[inline]
    pub fn is_set(&self, item: PlaylistItemType) -> bool { (self.0 & (1u16 << ((item as u8) - 1))) != 0 }

    #[inline]
    pub fn bits(self) -> u16 { self.0 }
}
