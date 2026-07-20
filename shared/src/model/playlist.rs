use crate::{
    create_bitset,
    error::TuliproxError,
    model::{
        stalker::StalkerStreamKind, stalker_item::StalkerPlaylistItem, xtream_const, CatchupAttribute,
        CatchupProperties, ClusterFlags, CommonPlaylistItem, ConfigTargetOptions, EpisodeStreamProperties,
        SeriesStreamProperties, StreamProperties, UUIDType, VideoStreamProperties, XtreamInfoDocument,
    },
    utils::{
        arc_str_option_serde, arc_str_serde, concat_path, extract_extension_from_url, generate_provider_playlist_uuid,
        generate_runtime_playlist_uuid, get_provider_id, obfuscate_text, Internable,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    fmt::{Display, Formatter, Write},
    str::FromStr,
    sync::Arc,
};
use strum_macros::{AsRefStr, Display, EnumIter, EnumString};
// https://de.wikipedia.org/wiki/M3U
// https://siptv.eu/howto/playlist.html

pub type VirtualId = u32;

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

impl TryFrom<PlaylistItemType> for XtreamCluster {
    type Error = String;
    fn try_from(item_type: PlaylistItemType) -> Result<Self, Self::Error> {
        match item_type {
            PlaylistItemType::Live
            | PlaylistItemType::LiveHls
            | PlaylistItemType::LiveDash
            | PlaylistItemType::LiveUnknown => Ok(Self::Live),
            PlaylistItemType::Catchup | PlaylistItemType::Video | PlaylistItemType::LocalVideo => Ok(Self::Video),
            PlaylistItemType::Series
            | PlaylistItemType::SeriesInfo
            | PlaylistItemType::LocalSeries
            | PlaylistItemType::LocalSeriesInfo => Ok(Self::Series),
        }
    }
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
    const LIVE: &'static str = "live";
    const VIDEO: &'static str = "video";
    const SERIES: &'static str = "series";
    const SERIES_INFO: &'static str = "series-info";
    const CATCHUP: &'static str = "catchup";

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

    pub fn is_cluster(&self, cluster: XtreamCluster) -> bool {
        match self {
            Self::Live | Self::LiveHls | Self::LiveDash | Self::LiveUnknown => cluster == XtreamCluster::Live,
            Self::Catchup | Self::Video | Self::LocalVideo => cluster == XtreamCluster::Video,
            Self::Series | Self::LocalSeries | Self::SeriesInfo | Self::LocalSeriesInfo => {
                cluster == XtreamCluster::Series
            }
        }
    }
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

#[derive(Copy, Clone, Default, Debug)]
pub struct PlaylistItemTypeSet(u16);
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

pub trait FieldGetAccessor {
    fn get_field(&self, field: &str) -> Option<Arc<str>>;
}
pub trait FieldSetAccessor {
    fn set_field(&mut self, field: &str, value: &str) -> bool;
}

pub trait PlaylistEntry: Send + Sync {
    fn get_virtual_id(&self) -> VirtualId;
    /// Returns the immutable stream identifier captured from the input playlist before target mappings.
    fn get_input_stream_id(&self) -> Option<Arc<str>>;
    fn get_provider_id(&self) -> Option<u32>;
    fn get_category_id(&self) -> Option<u32>;
    fn get_provider_url(&self) -> Arc<str>;
    fn get_uuid(&self) -> UUIDType;
    fn get_item_type(&self) -> PlaylistItemType;
    fn get_group(&self) -> Arc<str>;
    fn get_name(&self) -> Arc<str>;
    fn get_resolved_info_document(&self, options: &XtreamMappingOptions) -> Option<XtreamInfoDocument>;
    fn get_additional_properties(&self) -> Option<&StreamProperties>;
    fn get_additional_properties_mut(&mut self) -> Option<&mut StreamProperties>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistItemHeader {
    #[serde(skip)]
    pub uuid: UUIDType, // calculated
    #[serde(with = "arc_str_serde")]
    pub id: Arc<str>, // provider id
    #[serde(with = "arc_str_serde")]
    pub name: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub logo: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub logo_small: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub group: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub title: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub parent_code: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub audio_track: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub time_shift: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub rec: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub url: Arc<str>,
    #[serde(default, with = "arc_str_option_serde")]
    pub epg_channel_id: Option<Arc<str>>,
    #[serde(with = "arc_str_serde")]
    pub input_name: Arc<str>,
    #[serde(default)]
    pub additional_properties: Option<StreamProperties>,

    // 4-byte aligned
    pub virtual_id: VirtualId, // virtual id
    pub chno: u32,
    #[serde(default)]
    pub category_id: u32,
    #[serde(default)]
    pub source_ordinal: u32,

    // 1-byte aligned
    pub xtream_cluster: XtreamCluster,
    #[serde(default)]
    pub item_type: PlaylistItemType,
    /// Stable provider/origin ID captured before any target transformation.
    #[serde(default, with = "arc_str_serde")]
    pub input_stream_id: Arc<str>,
}

impl Default for PlaylistItemHeader {
    fn default() -> Self {
        Self {
            uuid: UUIDType::default(),
            id: "".intern(),
            virtual_id: 0,
            name: "".intern(),
            chno: 0,
            logo: "".intern(),
            logo_small: "".intern(),
            group: "".intern(),
            title: "".intern(),
            parent_code: "".intern(),
            audio_track: "".intern(),
            time_shift: "".intern(),
            rec: "".intern(),
            url: "".intern(),
            epg_channel_id: None,
            xtream_cluster: XtreamCluster::default(),
            additional_properties: None,
            item_type: PlaylistItemType::default(),
            category_id: 0,
            input_name: "".intern(),
            source_ordinal: 0,
            input_stream_id: "".intern(),
        }
    }
}

impl PlaylistItemHeader {
    /// Captures the input playlist item ID at the input-processing boundary.
    ///
    /// This must run before target transformations and must not be used to recover an identity
    /// from a target-mutated `id`.
    pub fn freeze_input_stream_id(&mut self) {
        if self.input_stream_id.is_empty() && !self.id.is_empty() {
            self.input_stream_id = Arc::clone(&self.id);
        }
    }

    /// Returns the input stream ID captured at the input-processing boundary.
    ///
    /// Legacy fallback must be resolved before a target transformation starts. Once an item is
    /// represented by this header, the mutable target-facing `id` is never a valid fallback.
    pub fn get_input_stream_id(&self) -> Option<Arc<str>> {
        (!self.input_stream_id.is_empty()).then(|| Arc::clone(&self.input_stream_id))
    }

    #[inline]
    pub fn gen_uuid(&mut self) {
        self.uuid = generate_runtime_playlist_uuid(&self.input_name, &self.id, self.item_type, &self.url);
    }

    #[inline]
    pub const fn get_uuid(&self) -> &UUIDType { &self.uuid }

    pub fn get_provider_id(&mut self) -> Option<u32> {
        match get_provider_id(&self.id, &self.url) {
            None => None,
            Some(newid) => {
                self.id = newid.to_string().intern();
                Some(newid)
            }
        }
    }

    #[inline]
    pub fn get_name(&self) -> Arc<str> {
        if self.title.is_empty() {
            Arc::clone(&self.name)
        } else {
            Arc::clone(&self.title)
        }
    }

    #[inline]
    pub fn get_container_extension(&self) -> Option<Arc<str>> {
        self.additional_properties.as_ref().and_then(|a| a.get_container_extension())
    }
}

macro_rules! to_m3u_non_empty_fields {
    ($header:expr, $line:expr, $(($prop:ident, $field:expr)),*;) => {
        $(
            if !$header.$prop.is_empty() {
                let _ = write!($line," {}=\"{}\"", $field, $header.$prop );
            }
         )*
    };
}

macro_rules! to_m3u_resource_non_empty_fields {
    ($header:expr, $url:expr, $line:expr, $(($prop:ident, $field:expr)),*;) => {
        $(
           if !$header.$prop.is_empty() {
               let _ = write!($line, " {}=\"{}/{}\"", $field, $url, stringify!($prop));
            }
         )*
    };
}

macro_rules! generate_field_accessor_impl_for_playlist_item_header {
    ($($prop:ident),*;) => {
        impl crate::model::FieldGetAccessor for crate::model::PlaylistItemHeader {
            fn get_field(&self, field: &str) -> Option<Arc<str>> {
                let bytes = field.as_bytes();

                $(
                    {
                        let target = stringify!($prop).as_bytes();
                        if bytes.eq_ignore_ascii_case(target) {
                            return Some(Arc::clone(&self.$prop));
                        }
                    }
                )*

                if bytes.eq_ignore_ascii_case(b"group") {
                        Some(Arc::clone(&self.group))
                } else if bytes.eq_ignore_ascii_case(b"caption") {
                    Some(if self.title.is_empty() {
                        Arc::clone(&self.name)
                    } else {
                        Arc::clone(&self.title)
                    })
                } else if bytes.eq_ignore_ascii_case(b"input") {
                    Some(Arc::clone(&self.input_name))
                } else if bytes.eq_ignore_ascii_case(b"type") {
                    Some(self.item_type.as_str().intern())
                } else if bytes.eq_ignore_ascii_case(b"epg_channel_id") || bytes.eq_ignore_ascii_case(b"epg_id") {
                    self.epg_channel_id.as_ref().map(Arc::clone)
                } else if bytes.eq_ignore_ascii_case(b"chno") {
                    Some(self.chno.to_string().intern())
                } else if bytes.eq_ignore_ascii_case(b"genre") {
                    crate::get_genre!(self)
                } else {
                    None
                }
            }
         }

         impl crate::model::FieldSetAccessor for crate::model::PlaylistItemHeader {
            fn set_field(&mut self, field: &str, value: &str) -> bool {
                let bytes = field.as_bytes();
                $(
                    {
                        let target = stringify!($prop).as_bytes();
                        if bytes.eq_ignore_ascii_case(target) {
                            self.$prop = value.intern();
                            return true;
                        }
                    }
                )*

                if bytes.eq_ignore_ascii_case(b"group") {
                    self.group = value.intern();
                    true
                } else if bytes.eq_ignore_ascii_case(b"caption") {
                    let interned = value.intern();
                    self.title = Arc::clone(&interned);
                    self.name = interned;
                    true
                } else if bytes.eq_ignore_ascii_case(b"epg_channel_id") || bytes.eq_ignore_ascii_case(b"epg_id") {
                    self.epg_channel_id = Some(value.intern());
                    true
                } else if bytes.eq_ignore_ascii_case(b"chno") {
                    if let Ok(parsed) = value.parse::<u32>() {
                        self.chno = parsed;
                        true
                    } else {
                        false
                    }
                } else if bytes.eq_ignore_ascii_case(b"genre") {
                    return crate::set_genre!(self, value);
                } else {
                    false
                }
            }
        }
    }
}

generate_field_accessor_impl_for_playlist_item_header!(id, /*virtual_id,*/ title, name, logo, logo_small, parent_code, audio_track, time_shift, rec, url;);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct M3uPlaylistItem {
    pub virtual_id: VirtualId,
    #[serde(with = "arc_str_serde")]
    pub provider_id: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub name: Arc<str>,
    pub chno: u32,
    #[serde(with = "arc_str_serde")]
    pub logo: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub logo_small: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub group: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub title: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub parent_code: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub audio_track: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub time_shift: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub rec: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub url: Arc<str>,
    #[serde(default, with = "arc_str_option_serde")]
    pub epg_channel_id: Option<Arc<str>>,
    #[serde(with = "arc_str_serde")]
    pub input_name: Arc<str>,
    pub item_type: PlaylistItemType,
    #[serde(skip)]
    pub t_stream_url: Arc<str>,
    #[serde(skip)]
    pub t_resource_url: Option<String>,
    #[serde(skip)]
    pub t_catchup_source: Option<Arc<str>>,
    #[serde(skip)]
    pub t_catchup_mode: Option<Arc<str>>,
    #[serde(default)]
    pub source_ordinal: u32,
    #[serde(default)]
    pub additional_properties: Option<StreamProperties>,
    /// Stable provider/origin ID captured before any target transformation.
    #[serde(default, with = "arc_str_serde")]
    pub input_stream_id: Arc<str>,
}

fn write_m3u_attr(line: &mut String, name: &str, value: &str) { let _ = write!(line, " {name}=\"{value}\""); }

fn append_catchup_attribute(line: &mut String, name: &str, value: Option<&Arc<str>>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        write_m3u_attr(line, name, value);
    }
}

fn append_extra_catchup_attributes(line: &mut String, attributes: &[CatchupAttribute]) {
    for attribute in attributes {
        if !attribute.name.is_empty() && !attribute.value.is_empty() {
            write_m3u_attr(line, attribute.name.as_ref(), attribute.value.as_ref());
        }
    }
}

fn append_m3u_catchup_attributes(
    line: &mut String,
    catchup: &CatchupProperties,
    rewritten_mode: Option<&Arc<str>>,
    rewritten_source: Option<&Arc<str>>,
) {
    if let Some(mode) = rewritten_mode.filter(|mode| !mode.is_empty()) {
        write_m3u_attr(line, "catchup", mode.as_ref());
    } else {
        append_catchup_attribute(line, "catchup", catchup.mode.as_ref());
    }
    append_catchup_attribute(line, "catchup-days", catchup.days.as_ref());
    if let Some(source) = rewritten_source.filter(|source| !source.is_empty()) {
        write_m3u_attr(line, "catchup-source", source.as_ref());
    } else {
        append_catchup_attribute(line, "catchup-source", catchup.source.as_ref());
    }
    append_catchup_attribute(line, "catchup-time", catchup.time.as_ref());
    append_catchup_attribute(line, "catchup-correction", catchup.correction.as_ref());
    append_catchup_attribute(line, "catchup-type", catchup.catchup_type.as_ref());
    append_extra_catchup_attributes(line, &catchup.extra_attributes);
}

impl M3uPlaylistItem {
    #[allow(clippy::missing_panics_doc)]
    pub fn to_m3u(&self, target_options: Option<&ConfigTargetOptions>, rewrite_urls: bool) -> String {
        let options = target_options.as_ref();
        let ignore_logo = options.is_some_and(|o| o.ignore_logo);
        let mut line = String::with_capacity(256);
        let _ = write!(
            &mut line,
            "#EXTINF:-1 tvg-id=\"{}\" tvg-name=\"{}\" group-title=\"{}\"",
            self.epg_channel_id.as_ref().map_or("", |o| o.as_ref()),
            self.name,
            self.group
        );

        if !ignore_logo {
            if let (true, Some(resource_url)) = (rewrite_urls, self.t_resource_url.as_ref()) {
                to_m3u_resource_non_empty_fields!(self, resource_url, line, (logo, "tvg-logo"), (logo_small, "tvg-logo-small"););
            } else {
                to_m3u_non_empty_fields!(self, line, (logo, "tvg-logo"), (logo_small, "tvg-logo-small"););
            }
        }

        if self.chno != 0 {
            let _ = write!(line, " tvg-chno=\"{}\"", self.chno);
        }
        to_m3u_non_empty_fields!(self, line,
            (parent_code, "parent-code"),
            (audio_track, "audio-track"),
            (time_shift, "timeshift"),
            (rec, "tvg-rec"););
        if let Some(StreamProperties::Live(live)) = self.additional_properties.as_ref() {
            if let Some(catchup) = live.catchup.as_ref() {
                append_m3u_catchup_attributes(
                    &mut line,
                    catchup,
                    self.t_catchup_mode.as_ref(),
                    self.t_catchup_source.as_ref(),
                );
            }
        }

        let url = if self.t_stream_url.is_empty() { &self.url } else { &self.t_stream_url };
        let _ = write!(&mut line, ",{}\n{}", self.title, url);
        line
    }

    pub fn to_common(&self) -> CommonPlaylistItem {
        CommonPlaylistItem {
            virtual_id: self.virtual_id,
            provider_id: Arc::clone(&self.provider_id),
            name: Arc::clone(&self.name),
            chno: self.chno,
            logo: Arc::clone(&self.logo),
            logo_small: Arc::clone(&self.logo_small),
            group: Arc::clone(&self.group),
            title: Arc::clone(&self.title),
            parent_code: Arc::clone(&self.parent_code),
            audio_track: Arc::clone(&self.audio_track),
            time_shift: Arc::clone(&self.time_shift),
            rec: Arc::clone(&self.rec),
            url: Arc::clone(&self.url),
            input_name: Arc::clone(&self.input_name),
            item_type: self.item_type,
            epg_channel_id: self.epg_channel_id.clone(),
            xtream_cluster: XtreamCluster::try_from(self.item_type).ok(),
            additional_properties: self.additional_properties.clone(),
            category_id: None,
        }
    }
}

impl PlaylistEntry for M3uPlaylistItem {
    #[inline]
    fn get_virtual_id(&self) -> VirtualId { self.virtual_id }

    fn get_input_stream_id(&self) -> Option<Arc<str>> {
        let input_stream_id = if self.input_stream_id.is_empty() { &self.provider_id } else { &self.input_stream_id };
        (!input_stream_id.is_empty()).then(|| Arc::clone(input_stream_id))
    }

    fn get_provider_id(&self) -> Option<u32> { get_provider_id(&self.provider_id, &self.url) }
    #[inline]
    fn get_category_id(&self) -> Option<u32> { None }
    #[inline]
    fn get_provider_url(&self) -> Arc<str> { Arc::clone(&self.url) }

    fn get_uuid(&self) -> UUIDType {
        generate_runtime_playlist_uuid(&self.input_name, &self.provider_id, self.item_type, &self.url)
    }

    #[inline]
    fn get_item_type(&self) -> PlaylistItemType { self.item_type }

    #[inline]
    fn get_group(&self) -> Arc<str> { Arc::clone(&self.group) }

    #[inline]
    fn get_name(&self) -> Arc<str> {
        if self.title.is_empty() {
            Arc::clone(&self.name)
        } else {
            Arc::clone(&self.title)
        }
    }

    #[inline]
    fn get_resolved_info_document(&self, _options: &XtreamMappingOptions) -> Option<XtreamInfoDocument> { None }
    #[inline]
    fn get_additional_properties(&self) -> Option<&StreamProperties> { self.additional_properties.as_ref() }
    #[inline]
    fn get_additional_properties_mut(&mut self) -> Option<&mut StreamProperties> { self.additional_properties.as_mut() }
}

macro_rules! generate_field_accessor_impl_for_m3u_playlist_item {
    ($($prop:ident),*;) => {
        impl crate::model::FieldGetAccessor for M3uPlaylistItem {
            fn get_field(&self, field: &str) -> Option<Arc<str>> {
                let bytes = field.as_bytes();
                $(
                    {
                        let target = stringify!($prop).as_bytes();
                        if bytes.len() == target.len() &&
                           bytes.iter().zip(target).all(|(a, b)| a.to_ascii_lowercase() == *b)
                        {
                            return Some(Arc::clone(&self.$prop));
                        }
                    }
                )*
                if bytes.eq_ignore_ascii_case(b"group") {
                    Some(Arc::clone(&self.group))
                } else if bytes.eq_ignore_ascii_case(b"caption") {
                    Some(if self.title.is_empty() {
                        Arc::clone(&self.name)
                    } else {
                        Arc::clone(&self.title)
                    })
                } else if bytes.eq_ignore_ascii_case(b"epg_channel_id") || bytes.eq_ignore_ascii_case(b"epg_id") {
                    self.epg_channel_id.as_ref().map(Arc::clone)
                } else if bytes.eq_ignore_ascii_case(b"chno") {
                    Some(self.chno.to_string().intern())
                } else  {
                    None
                }
            }
        }
    }
}

generate_field_accessor_impl_for_m3u_playlist_item!(title, name, provider_id, logo, logo_small, parent_code, audio_track, time_shift, rec, url;);

impl From<M3uPlaylistItem> for CommonPlaylistItem {
    fn from(item: M3uPlaylistItem) -> Self { item.to_common() }
}

create_bitset!(
    u8,
    XtreamMappingFlags,
    SkipLiveDirectSource,
    SkipVideoDirectSource,
    SkipSeriesDirectSource,
    RewriteResourceUrl
);

pub struct XtreamMappingOptions {
    pub flags: XtreamMappingFlagsSet,
    pub force_redirect: Option<ClusterFlags>,
    pub reverse_item_types: PlaylistItemTypeSet,
    pub resource_proxy_item_types: PlaylistItemTypeSet,
    pub username: String,
    pub password: String,
    pub base_url: String,
    pub web_ui_request: bool,
    pub encrypt_secret: [u8; 16],
}

impl XtreamMappingOptions {
    #[inline]
    pub fn is_reverse(&self, item_type: PlaylistItemType) -> bool { self.reverse_item_types.is_set(item_type) }

    fn is_trusted_web_ui_resource_path(resource_url: &str) -> bool {
        const TRUSTED_WEB_UI_RESOURCE_PREFIXES: [&str; 1] = ["/api/v1/library/thumbnail/"];
        TRUSTED_WEB_UI_RESOURCE_PREFIXES.iter().any(|prefix| resource_url.starts_with(prefix))
    }

    fn build_reverse_proxy_base_url(
        &self,
        xtream_cluster: XtreamCluster,
        item_type: PlaylistItemType,
        virtual_id: VirtualId,
    ) -> Option<String> {
        let proxy_resource = self.resource_proxy_item_types.is_set(item_type);
        if proxy_resource && self.flags.contains(XtreamMappingFlags::RewriteResourceUrl) {
            Some(format!(
                "{}/resource/{}/{}/{}/{}",
                self.base_url,
                xtream_cluster.as_stream_type(),
                self.username,
                self.password,
                virtual_id
            ))
        } else {
            None
        }
    }

    pub fn get_resource_url(
        &self,
        xtream_cluster: XtreamCluster,
        item_type: PlaylistItemType,
        virtual_id: VirtualId,
        resource_url: &str,
        resource_field: &str,
    ) -> String {
        if !self.web_ui_request && Self::is_trusted_web_ui_resource_path(resource_url) {
            return concat_path(&self.base_url, resource_url);
        }

        if self.web_ui_request {
            if resource_url.is_empty() {
                return resource_url.to_string();
            }
            if Self::is_trusted_web_ui_resource_path(resource_url) {
                return resource_url.to_string();
            }
            let rewrite_url = concat_path(&self.base_url, &obfuscate_text(&self.encrypt_secret, resource_url));
            return rewrite_url;
        }

        let rewrite_url = self.build_reverse_proxy_base_url(xtream_cluster, item_type, virtual_id);

        if let Some(url) = rewrite_url {
            if resource_url.starts_with("http://") || resource_url.starts_with("https://") {
                return format!("{url}/{resource_field}");
            }
        }
        resource_url.to_string()
    }
    pub fn get_bd_path_resource_url(
        &self,
        xtream_cluster: XtreamCluster,
        item_type: PlaylistItemType,
        virtual_id: VirtualId,
        resource_url: &str,
        resource_field: &str,
        index: usize,
    ) -> String {
        if !self.web_ui_request && Self::is_trusted_web_ui_resource_path(resource_url) {
            return concat_path(&self.base_url, resource_url);
        }

        if self.web_ui_request {
            if resource_url.is_empty() {
                return resource_url.to_string();
            }
            if Self::is_trusted_web_ui_resource_path(resource_url) {
                return resource_url.to_string();
            }
            let rewrite_url = concat_path(&self.base_url, &obfuscate_text(&self.encrypt_secret, resource_url));
            return rewrite_url;
        }

        let rewrite_url = self.build_reverse_proxy_base_url(xtream_cluster, item_type, virtual_id);

        if let Some(url) = rewrite_url {
            if resource_url.starts_with("http://") || resource_url.starts_with("https://") {
                return format!("{url}/{resource_field}{}_{index}", xtream_const::XC_PROP_BACKDROP_PATH);
            }
        }
        resource_url.to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XtreamPlaylistItem {
    pub virtual_id: VirtualId,
    pub provider_id: u32,
    #[serde(with = "arc_str_serde")]
    pub name: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub logo: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub logo_small: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub group: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub title: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub parent_code: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub rec: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub url: Arc<str>,
    #[serde(default, with = "arc_str_option_serde")]
    pub epg_channel_id: Option<Arc<str>>,
    pub xtream_cluster: XtreamCluster,
    #[serde(default)]
    pub additional_properties: Option<StreamProperties>,
    pub item_type: PlaylistItemType,
    pub category_id: u32,
    #[serde(with = "arc_str_serde")]
    pub input_name: Arc<str>,
    pub channel_no: u32,
    #[serde(default)]
    pub source_ordinal: u32,
    /// Stable provider/origin ID captured before any target transformation.
    #[serde(default, with = "arc_str_serde")]
    pub input_stream_id: Arc<str>,
}

impl XtreamPlaylistItem {
    pub fn to_common(&self) -> CommonPlaylistItem {
        CommonPlaylistItem {
            virtual_id: self.virtual_id,
            provider_id: self.provider_id.intern(),
            name: self.name.clone(),
            chno: self.channel_no,
            logo: self.logo.clone(),
            logo_small: self.logo_small.clone(),
            group: self.group.clone(),
            title: self.title.clone(),
            parent_code: self.parent_code.clone(),
            audio_track: "".intern(),
            time_shift: "".intern(),
            rec: self.rec.clone(),
            url: self.url.clone(),
            input_name: self.input_name.clone(),
            item_type: self.item_type,
            epg_channel_id: self.epg_channel_id.clone(),
            xtream_cluster: Some(self.xtream_cluster),
            additional_properties: self.additional_properties.clone(),
            category_id: Some(self.category_id),
        }
    }

    pub fn get_container_extension(&self) -> Option<Arc<str>> {
        match self.additional_properties {
            None => None,
            Some(ref props) => match props {
                StreamProperties::Live(_) => Some("ts".intern()),
                StreamProperties::Video(video) => Some(Arc::clone(&video.container_extension)),
                StreamProperties::Series(_) => None,
                StreamProperties::Episode(episode) => Some(Arc::clone(&episode.container_extension)),
            },
        }
    }

    #[inline]
    pub fn has_details(&self) -> bool { self.additional_properties.as_ref().is_some_and(|p| p.has_details()) }

    pub fn resolve_resource_url(&self, field: &str) -> Option<Arc<str>> {
        let bytes = field.as_bytes();
        if bytes.eq_ignore_ascii_case(b"logo") && !self.logo.is_empty() {
            return Some(Arc::clone(&self.logo));
        } else if bytes.eq_ignore_ascii_case(b"logo_small") && !self.logo_small.is_empty() {
            return Some(Arc::clone(&self.logo_small));
        }
        self.additional_properties.as_ref().and_then(|a| a.resolve_resource_url(field))
    }
}

impl PlaylistEntry for XtreamPlaylistItem {
    #[inline]
    fn get_virtual_id(&self) -> VirtualId { self.virtual_id }
    fn get_input_stream_id(&self) -> Option<Arc<str>> {
        if self.input_stream_id.is_empty() {
            (self.provider_id > 0).then(|| self.provider_id.to_string().intern())
        } else {
            Some(Arc::clone(&self.input_stream_id))
        }
    }
    #[inline]
    fn get_provider_id(&self) -> Option<u32> { Some(self.provider_id) }
    #[inline]
    fn get_category_id(&self) -> Option<u32> { Some(self.category_id) }
    #[inline]
    fn get_provider_url(&self) -> Arc<str> { Arc::clone(&self.url) }

    #[inline]
    fn get_uuid(&self) -> UUIDType {
        generate_runtime_playlist_uuid(&self.input_name, &self.provider_id.to_string(), self.item_type, &self.url)
    }
    #[inline]
    fn get_item_type(&self) -> PlaylistItemType { self.item_type }
    #[inline]
    fn get_group(&self) -> Arc<str> { Arc::clone(&self.group) }
    #[inline]
    fn get_name(&self) -> Arc<str> {
        if self.title.is_empty() {
            Arc::clone(&self.name)
        } else {
            Arc::clone(&self.title)
        }
    }

    fn get_resolved_info_document(&self, options: &XtreamMappingOptions) -> Option<XtreamInfoDocument> {
        if self.has_details() {
            self.additional_properties.as_ref().map(|p| {
                p.to_info_document(
                    options,
                    self.get_item_type(),
                    self.get_virtual_id(),
                    self.get_category_id().unwrap_or(0),
                )
            })
        } else {
            None
        }
    }

    #[inline]
    fn get_additional_properties(&self) -> Option<&StreamProperties> { self.additional_properties.as_ref() }
    #[inline]
    fn get_additional_properties_mut(&mut self) -> Option<&mut StreamProperties> { self.additional_properties.as_mut() }
}

macro_rules! generate_field_accessor_impl_for_xtream_playlist_item {
    ($($prop:ident),*;) => {
        impl crate::model::FieldGetAccessor for crate::model::XtreamPlaylistItem {
            fn get_field(&self, field: &str) -> Option<Arc<str>> {
                let bytes = field.as_bytes();

                $(
                    {
                        let target = stringify!($prop).as_bytes();
                        if bytes.len() == target.len() &&
                           bytes.iter().zip(target).all(|(a, b)| a.to_ascii_lowercase() == *b)
                        {
                            return Some(Arc::clone(&self.$prop));
                        }
                    }
                )*

                // Caption
                if bytes.eq_ignore_ascii_case(b"caption") {
                    Some(if self.title.is_empty() {
                        Arc::clone(&self.name)
                    } else {
                        Arc::clone(&self.title)
                    })
                }
                // epg_channel_id / epg_id
                else if bytes.eq_ignore_ascii_case(b"epg_channel_id") || bytes.eq_ignore_ascii_case(b"epg_id") {
                    self.epg_channel_id.as_ref().map(Arc::clone)
                }
                // Additional Properties
                else if field.starts_with(xtream_const::XC_PROP_BACKDROP_PATH)
                     || bytes.eq_ignore_ascii_case(xtream_const::XC_PROP_COVER.as_bytes())
                {
                    match self.additional_properties.as_ref() {
                        Some(additional_properties) => match additional_properties {
                            StreamProperties::Live(_) => None,
                            StreamProperties::Video(video) => {
                                if bytes.eq_ignore_ascii_case(xtream_const::XC_PROP_COVER.as_bytes()) {
                                    video.details.as_ref().and_then(|details| {
                                        details.cover_big.as_ref()
                                            .or(details.movie_image.as_ref())
                                            .or_else(|| details.backdrop_path.as_ref().and_then(|p| p.first()))
                                            .map(Arc::clone)
                                    })
                                } else {
                                    video.details.as_ref().and_then(|details| {
                                        details.backdrop_path.as_ref().and_then(|p| p.first())
                                        .or(details.movie_image.as_ref())
                                        .or(details.cover_big.as_ref())
                                        .map(Arc::clone)
                                    })
                                }
                            }
                            StreamProperties::Series(series) => {
                                if bytes.eq_ignore_ascii_case(xtream_const::XC_PROP_COVER.as_bytes()) {
                                    if series.cover.is_empty() {
                                        series.backdrop_path.as_ref().and_then(|p| p.first()).map(Arc::clone)
                                    } else {
                                        Some(Arc::clone(&series.cover))
                                    }
                                } else {
                                    match series.backdrop_path.as_ref() {
                                        None => if series.cover.is_empty() { None } else { Some(Arc::clone(&series.cover)) },
                                        Some(p) => p.first().map(Arc::clone),
                                    }
                                }
                            }
                            StreamProperties::Episode(episode) => Some(Arc::clone(&episode.movie_image)),
                        },
                        None => None,
                    }
                }
                // Default fallback
                else {
                    None
                }
            }
        }
    }
}

impl From<XtreamPlaylistItem> for CommonPlaylistItem {
    fn from(item: XtreamPlaylistItem) -> Self { item.to_common() }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistItem {
    #[serde(flatten)]
    pub header: PlaylistItemHeader,
}

generate_field_accessor_impl_for_xtream_playlist_item!(group, title, name, logo, logo_small, parent_code, rec, url;);

impl PlaylistItem {
    fn get_additional_properties(header: &PlaylistItemHeader) -> Option<StreamProperties> {
        match &header.additional_properties {
            Some(props) => Some(props.clone()),
            None => {
                match header.xtream_cluster {
                    XtreamCluster::Live => None,
                    XtreamCluster::Video => {
                        let container_extension = extract_extension_from_url(&header.url)
                            .map(|e| e.strip_prefix('.').unwrap_or(e).to_string())
                            .unwrap_or_default();
                        Some(StreamProperties::Video(Box::new(VideoStreamProperties {
                            name: header.name.clone(),
                            category_id: header.category_id,
                            stream_id: header.virtual_id,
                            stream_icon: "".intern(),
                            direct_source: "".intern(),
                            custom_sid: None,
                            added: "".intern(),
                            container_extension: container_extension.intern(),
                            rating: None,
                            rating_5based: None,
                            stream_type: Some("movie".intern()),
                            trailer: None,
                            tmdb: None,
                            is_adult: 0,
                            details: None,
                        })))
                    }
                    XtreamCluster::Series => {
                        if header.item_type == PlaylistItemType::Series {
                            let container_extension = extract_extension_from_url(&header.url)
                                .map(|e| e.strip_prefix('.').unwrap_or(e).to_string())
                                .unwrap_or_default();
                            // TODO maybe from link ? like s01e02 or something like this
                            Some(StreamProperties::Episode(Box::new(EpisodeStreamProperties {
                                episode_id: 0,
                                episode: 0,
                                season: 0,
                                added: None,
                                release_date: None,
                                series_release_date: None,
                                plot: None,
                                tmdb: None,
                                movie_image: "".intern(),
                                container_extension: container_extension.intern(),
                                audio: None,
                                video: None,
                            })))
                        } else if header.item_type == PlaylistItemType::SeriesInfo {
                            Some(StreamProperties::Series(Box::new(SeriesStreamProperties {
                                name: header.name.clone(),
                                category_id: header.category_id,
                                tmdb: None,
                                series_id: 0,
                                backdrop_path: None,
                                cast: "".intern(),
                                cover: "".intern(),
                                director: "".intern(),
                                episode_run_time: None,
                                genre: None,
                                last_modified: None,
                                plot: None,
                                rating: 0.0,
                                rating_5based: 0.0,
                                release_date: None,
                                youtube_trailer: "".intern(),
                                details: None,
                            })))
                        } else {
                            None
                        }
                    }
                }
            }
        }
    }

    pub fn has_details(&self) -> bool { self.header.additional_properties.as_ref().is_some_and(|p| p.has_details()) }

    pub fn get_tmdb_id(&self) -> Option<u32> {
        self.header.additional_properties.as_ref().and_then(|p| p.get_tmdb_id())
    }
}

impl From<&PlaylistItem> for XtreamPlaylistItem {
    fn from(item: &PlaylistItem) -> Self {
        let header = &item.header;
        let input_stream_id = Arc::clone(&header.input_stream_id);
        let missing_live_input_identity =
            input_stream_id.is_empty() && (header.item_type.is_live() || header.item_type == PlaylistItemType::Catchup);
        let provider_id =
            if missing_live_input_identity { 0 } else { get_provider_id(&header.id, &header.url).unwrap_or_default() };
        let additional_properties = PlaylistItem::get_additional_properties(header);

        XtreamPlaylistItem {
            virtual_id: header.virtual_id,
            provider_id,
            name: if header.item_type == PlaylistItemType::Series {
                Arc::clone(&header.title)
            } else {
                Arc::clone(&header.name)
            },
            logo: Arc::clone(&header.logo),
            logo_small: Arc::clone(&header.logo_small),
            group: Arc::clone(&header.group),
            title: Arc::clone(&header.title),
            parent_code: Arc::clone(&header.parent_code),
            rec: Arc::clone(&header.rec),
            url: Arc::clone(&header.url),
            epg_channel_id: header.epg_channel_id.clone(),
            xtream_cluster: header.xtream_cluster,
            additional_properties,
            item_type: header.item_type,
            category_id: header.category_id,
            input_name: Arc::clone(&header.input_name),
            channel_no: header.chno,
            source_ordinal: header.source_ordinal,
            input_stream_id,
        }
    }
}

impl From<&PlaylistItem> for M3uPlaylistItem {
    fn from(item: &PlaylistItem) -> Self {
        let header = &item.header;
        let input_stream_id = Arc::clone(&header.input_stream_id);
        let missing_live_input_identity =
            input_stream_id.is_empty() && (header.item_type.is_live() || header.item_type == PlaylistItemType::Catchup);
        M3uPlaylistItem {
            virtual_id: header.virtual_id,
            provider_id: if missing_live_input_identity { "".intern() } else { Arc::clone(&header.id) },
            name: if header.item_type == PlaylistItemType::Series {
                Arc::clone(&header.title)
            } else {
                Arc::clone(&header.name)
            },
            chno: header.chno,
            logo: Arc::clone(&header.logo),
            logo_small: Arc::clone(&header.logo_small),
            group: Arc::clone(&header.group),
            title: Arc::clone(&header.title),
            parent_code: Arc::clone(&header.parent_code),
            audio_track: Arc::clone(&header.audio_track),
            time_shift: Arc::clone(&header.time_shift),
            rec: Arc::clone(&header.rec),
            url: Arc::clone(&header.url),
            epg_channel_id: header.epg_channel_id.clone(),
            input_name: Arc::clone(&header.input_name),
            item_type: header.item_type,
            t_stream_url: Arc::clone(&header.url),
            t_resource_url: None,
            t_catchup_source: None,
            t_catchup_mode: None,
            source_ordinal: header.source_ordinal,
            additional_properties: header.additional_properties.clone(),
            input_stream_id,
        }
    }
}

impl From<&PlaylistItem> for CommonPlaylistItem {
    fn from(item: &PlaylistItem) -> Self {
        let header = &item.header;

        let additional_properties = PlaylistItem::get_additional_properties(header);

        CommonPlaylistItem {
            virtual_id: header.virtual_id,
            provider_id: Arc::clone(&header.id),
            name: if header.item_type == PlaylistItemType::Series {
                Arc::clone(&header.title)
            } else {
                Arc::clone(&header.name)
            },
            logo: header.logo.clone(),
            logo_small: header.logo_small.clone(),
            group: Arc::clone(&header.group),
            title: header.title.clone(),
            parent_code: header.parent_code.clone(),
            audio_track: header.audio_track.clone(),
            time_shift: header.time_shift.clone(),
            rec: header.rec.clone(),
            url: header.url.clone(),
            epg_channel_id: header.epg_channel_id.clone(),
            xtream_cluster: Some(header.xtream_cluster),
            additional_properties,
            item_type: header.item_type,
            category_id: Some(header.category_id),
            input_name: Arc::clone(&header.input_name),
            chno: header.chno,
        }
    }
}

impl From<&XtreamPlaylistItem> for PlaylistItem {
    fn from(item: &XtreamPlaylistItem) -> Self {
        let input_stream_id = item.get_input_stream_id();
        let header = PlaylistItemHeader {
            uuid: item.get_uuid(),
            virtual_id: item.virtual_id,
            id: if item.provider_id == 0 && input_stream_id.is_none() {
                "".intern()
            } else {
                item.provider_id.to_string().intern()
            },
            name: item.name.clone(),
            title: item.title.clone(),
            logo: item.logo.clone(),
            logo_small: item.logo_small.clone(),
            group: item.group.clone(),
            parent_code: item.parent_code.clone(),
            rec: item.rec.clone(),
            url: item.url.clone(),
            epg_channel_id: item.epg_channel_id.clone(),
            xtream_cluster: item.xtream_cluster,
            item_type: item.item_type,
            category_id: item.category_id,
            input_name: item.input_name.clone(),
            chno: item.channel_no,
            audio_track: "".intern(),
            time_shift: "".intern(),
            additional_properties: item.additional_properties.clone(),
            source_ordinal: item.source_ordinal,
            input_stream_id: input_stream_id.unwrap_or_else(|| "".intern()),
        };

        PlaylistItem { header }
    }
}

impl From<&M3uPlaylistItem> for PlaylistItem {
    fn from(item: &M3uPlaylistItem) -> Self {
        let header = PlaylistItemHeader {
            uuid: item.get_uuid(),
            virtual_id: item.virtual_id,
            id: item.provider_id.clone(),
            name: item.name.clone(),
            title: item.title.clone(),
            logo: item.logo.clone(),
            logo_small: item.logo_small.clone(),
            group: item.group.clone(),
            parent_code: item.parent_code.clone(),
            rec: item.rec.clone(),
            url: item.url.clone(),
            epg_channel_id: item.epg_channel_id.clone(),
            xtream_cluster: XtreamCluster::try_from(item.item_type).unwrap_or(XtreamCluster::Live),
            item_type: item.item_type,
            category_id: 0,
            input_name: item.input_name.clone(),
            chno: item.chno,
            audio_track: item.audio_track.clone(),
            time_shift: item.time_shift.clone(),
            additional_properties: item.additional_properties.clone(),
            source_ordinal: item.source_ordinal,
            input_stream_id: item.get_input_stream_id().unwrap_or_else(|| "".intern()),
        };

        PlaylistItem { header }
    }
}

impl PlaylistItem {
    /// Canonical `StalkerPlaylistItem` → `PlaylistItem` conversion.
    ///
    /// This is the single conversion used by both the download path
    /// (processor) and the disk-load path so the generated identity is stable
    /// across processing modes: the UUID is always seeded with the owning
    /// input name and the `input_name` header field is always populated.
    /// The group falls back to a cluster default only when the portal did
    /// not supply a category name.
    pub fn from_stalker(item: &StalkerPlaylistItem, input_name: &str) -> Self {
        let item_type = match item.stream_kind {
            StalkerStreamKind::Live | StalkerStreamKind::Archive => PlaylistItemType::Live,
            StalkerStreamKind::Movie => PlaylistItemType::Video,
            StalkerStreamKind::Episode => {
                if item.is_series_root() {
                    PlaylistItemType::SeriesInfo
                } else {
                    PlaylistItemType::Series
                }
            }
        };
        let xtream_cluster = match item.stream_kind {
            StalkerStreamKind::Live | StalkerStreamKind::Archive => XtreamCluster::Live,
            StalkerStreamKind::Movie => XtreamCluster::Video,
            StalkerStreamKind::Episode => XtreamCluster::Series,
        };
        let stream_id_str: Arc<str> = Internable::intern(item.stream_id.to_string());
        let logo: Arc<str> = item.logo_url.clone().unwrap_or_else(|| Internable::intern(String::new()));
        // Keep unresolved commands private; playback resolves them from the persisted Stalker metadata.
        let url = Arc::clone(&item.stream_url);
        let group: Arc<str> = if item.category_name.is_empty() {
            let fallback = match item.stream_kind {
                StalkerStreamKind::Live | StalkerStreamKind::Archive => "Live",
                StalkerStreamKind::Movie => "Movies",
                StalkerStreamKind::Episode => "Series",
            };
            Internable::intern(fallback.to_string())
        } else {
            Arc::clone(&item.category_name)
        };
        let header = PlaylistItemHeader {
            uuid: generate_provider_playlist_uuid(input_name, &stream_id_str, item_type),
            virtual_id: item.stream_id,
            id: Arc::clone(&stream_id_str),
            name: Arc::clone(&item.name),
            title: Arc::clone(&item.name),
            logo: Arc::clone(&logo),
            logo_small: Internable::intern(String::new()),
            group,
            parent_code: Internable::intern(String::new()),
            audio_track: Internable::intern(String::new()),
            time_shift: Internable::intern(String::new()),
            rec: Internable::intern(String::new()),
            url,
            epg_channel_id: item.epg_channel_id.clone(),
            item_type,
            xtream_cluster,
            additional_properties: None,
            input_name: Internable::intern(input_name.to_string()),
            chno: item.number,
            category_id: item.category_id,
            source_ordinal: 0,
            input_stream_id: stream_id_str,
        };

        PlaylistItem { header }
    }
}

impl PlaylistEntry for PlaylistItem {
    #[inline]
    fn get_virtual_id(&self) -> VirtualId { self.header.virtual_id }

    #[inline]
    fn get_input_stream_id(&self) -> Option<Arc<str>> { self.header.get_input_stream_id() }

    fn get_provider_id(&self) -> Option<u32> {
        let header = &self.header;
        get_provider_id(&header.id, &header.url)
    }

    #[inline]
    fn get_category_id(&self) -> Option<u32> { Some(self.header.category_id) }

    #[inline]
    fn get_provider_url(&self) -> Arc<str> { Arc::clone(&self.header.url) }

    #[inline]
    fn get_uuid(&self) -> UUIDType {
        let header = &self.header;
        generate_runtime_playlist_uuid(&header.input_name, &header.id, header.item_type, &header.url)
    }

    #[inline]
    fn get_item_type(&self) -> PlaylistItemType { self.header.item_type }

    #[inline]
    fn get_group(&self) -> Arc<str> { Arc::clone(&self.header.group) }

    #[inline]
    fn get_name(&self) -> Arc<str> { self.header.get_name() }

    fn get_resolved_info_document(&self, options: &XtreamMappingOptions) -> Option<XtreamInfoDocument> {
        if self.has_details() {
            self.header.additional_properties.as_ref().map(|p| {
                p.to_info_document(
                    options,
                    self.get_item_type(),
                    self.get_virtual_id(),
                    self.get_category_id().unwrap_or(0),
                )
            })
        } else {
            None
        }
    }

    fn get_additional_properties(&self) -> Option<&StreamProperties> { self.header.additional_properties.as_ref() }
    #[inline]
    fn get_additional_properties_mut(&mut self) -> Option<&mut StreamProperties> {
        self.header.additional_properties.as_mut()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistGroup {
    pub id: u32,
    #[serde(with = "arc_str_serde")]
    pub title: Arc<str>,
    pub channels: Vec<PlaylistItem>,
    pub xtream_cluster: XtreamCluster,
}

impl PlaylistGroup {
    #[inline]
    pub fn on_load(&mut self) {
        for pl in &mut self.channels {
            pl.header.freeze_input_stream_id();
            pl.header.gen_uuid();
            pl.header.category_id = self.id;
        }
    }

    #[inline]
    pub fn filter_count<F>(&self, filter: F) -> usize
    where
        F: Fn(&PlaylistItem) -> bool,
    {
        self.channels.iter().filter(|&c| filter(c)).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        CatchupAttribute, CatchupProperties, LiveStreamProperties, PlaylistItemType, StreamProperties, XtreamCluster,
        XtreamMappingFlags,
    };

    #[test]
    fn from_stalker_uuid_is_seeded_with_input_name_not_category() {
        let mut item = StalkerPlaylistItem {
            stream_id: 42,
            category_name: Internable::intern("News".to_string()),
            ..StalkerPlaylistItem::default()
        };
        let converted_a = PlaylistItem::from_stalker(&item, "input_a");
        // A category rename must not change the identity of the channel.
        item.category_name = Internable::intern("World News".to_string());
        let converted_b = PlaylistItem::from_stalker(&item, "input_a");
        assert_eq!(converted_a.header.uuid, converted_b.header.uuid);
        // A different input owning the same stream id must produce a distinct identity.
        let converted_c = PlaylistItem::from_stalker(&item, "input_b");
        assert_ne!(converted_a.header.uuid, converted_c.header.uuid);
    }

    #[test]
    fn from_stalker_populates_input_name_and_group() {
        let item = StalkerPlaylistItem {
            stream_id: 7,
            stream_kind: StalkerStreamKind::Movie,
            category_name: Internable::intern("Action".to_string()),
            cmd: Internable::intern("ffmpeg http://streams.example/movie/7".to_string()),
            ..StalkerPlaylistItem::default()
        };
        let converted = PlaylistItem::from_stalker(&item, "my_input");
        assert_eq!(&*converted.header.input_name, "my_input");
        assert_eq!(&*converted.header.input_stream_id, "7");
        assert!(converted.header.url.is_empty());
        assert_eq!(&*converted.header.group, "Action");
        assert_eq!(converted.header.item_type, PlaylistItemType::Video);
    }

    #[test]
    fn from_stalker_group_falls_back_to_cluster_default() {
        let item = StalkerPlaylistItem {
            stream_id: 7,
            stream_kind: StalkerStreamKind::Movie,
            ..StalkerPlaylistItem::default()
        };
        let converted = PlaylistItem::from_stalker(&item, "my_input");
        assert_eq!(&*converted.header.group, "Movies");
    }

    fn sample_options() -> XtreamMappingOptions {
        XtreamMappingOptions {
            base_url: "/api/v1/playlist/resource".to_string(),
            username: "user".to_string(),
            password: "pass".to_string(),
            force_redirect: None,
            reverse_item_types: PlaylistItemTypeSet::from_item(PlaylistItemType::Live),
            resource_proxy_item_types: PlaylistItemTypeSet::from_item(PlaylistItemType::Live),
            web_ui_request: true,
            flags: XtreamMappingFlags::RewriteResourceUrl.into(),
            encrypt_secret: [3u8; 16],
        }
    }

    #[test]
    fn get_resource_url_keeps_internal_paths_for_web_ui_requests() {
        let options = sample_options();

        assert_eq!(
            options.get_resource_url(
                XtreamCluster::Series,
                PlaylistItemType::Series,
                1,
                "/api/v1/library/thumbnail/abc",
                "logo",
            ),
            "/api/v1/library/thumbnail/abc",
        );
    }

    #[test]
    fn get_bd_path_resource_url_keeps_internal_thumbnail_paths_for_web_ui_requests() {
        let options = sample_options();

        assert_eq!(
            options.get_bd_path_resource_url(
                XtreamCluster::Series,
                PlaylistItemType::Series,
                1,
                "/api/v1/library/thumbnail/backdrop",
                "backdrop_",
                0,
            ),
            "/api/v1/library/thumbnail/backdrop",
        );
    }

    #[test]
    fn get_resource_url_absolutizes_internal_thumbnail_paths_for_xtream_clients() {
        let mut options = sample_options();
        options.web_ui_request = false;
        options.base_url = "http://proxy.example/base".to_string();
        options.reverse_item_types = PlaylistItemTypeSet::empty();
        options.resource_proxy_item_types = PlaylistItemTypeSet::empty();

        assert_eq!(
            options.get_resource_url(
                XtreamCluster::Series,
                PlaylistItemType::LocalSeries,
                1,
                "/api/v1/library/thumbnail/abc",
                "logo",
            ),
            "http://proxy.example/base/api/v1/library/thumbnail/abc",
        );
    }

    #[test]
    fn get_bd_path_resource_url_absolutizes_internal_thumbnail_paths_for_xtream_clients() {
        let mut options = sample_options();
        options.web_ui_request = false;
        options.base_url = "http://proxy.example/base".to_string();
        options.reverse_item_types = PlaylistItemTypeSet::empty();
        options.resource_proxy_item_types = PlaylistItemTypeSet::empty();

        assert_eq!(
            options.get_bd_path_resource_url(
                XtreamCluster::Series,
                PlaylistItemType::LocalSeries,
                1,
                "/api/v1/library/thumbnail/backdrop",
                "backdrop_",
                0,
            ),
            "http://proxy.example/base/api/v1/library/thumbnail/backdrop",
        );
    }

    #[test]
    fn get_resource_url_obfuscates_protocol_relative_urls_for_web_ui_requests() {
        let options = sample_options();
        let resource_url = "//cdn.example.com/poster.jpg";

        assert_eq!(
            options.get_resource_url(XtreamCluster::Series, PlaylistItemType::Series, 1, resource_url, "logo",),
            concat_path(&options.base_url, &obfuscate_text(&options.encrypt_secret, resource_url)),
        );
    }

    #[test]
    fn get_resource_url_rewrites_redirect_resources_for_xtream_clients() {
        let mut options = sample_options();
        options.web_ui_request = false;
        options.base_url = "http://proxy.example/iptv".to_string();
        options.reverse_item_types = PlaylistItemTypeSet::empty();
        options.resource_proxy_item_types = PlaylistItemTypeSet::from_item(PlaylistItemType::Live);

        assert_eq!(
            options.get_resource_url(
                XtreamCluster::Live,
                PlaylistItemType::Live,
                2017,
                "https://provider.example/logo.png",
                "logo",
            ),
            "http://proxy.example/iptv/resource/live/user/pass/2017/logo",
        );
    }

    #[test]
    fn get_resource_url_does_not_bypass_untrusted_root_relative_paths_for_web_ui_requests() {
        let options = sample_options();
        let resource_url = "/provider-controlled/poster.jpg";

        assert_eq!(
            options.get_resource_url(XtreamCluster::Series, PlaylistItemType::Series, 1, resource_url, "logo",),
            concat_path(&options.base_url, &obfuscate_text(&options.encrypt_secret, resource_url)),
        );
    }

    #[test]
    fn get_resource_url_does_not_trust_absolute_urls_containing_internal_thumbnail_path() {
        let options = sample_options();
        let resource_url = "https://provider.example/api/v1/library/thumbnail/abc";

        assert_eq!(
            options.get_resource_url(XtreamCluster::Series, PlaylistItemType::Series, 1, resource_url, "logo",),
            concat_path(&options.base_url, &obfuscate_text(&options.encrypt_secret, resource_url)),
        );
    }

    #[test]
    fn xtream_playlist_item_conversion_falls_back_to_numeric_id_in_url() {
        let mut item = PlaylistItem {
            header: PlaylistItemHeader {
                id: "channel-alpha".intern(),
                url: "http://provider.example/live/user/pass/12345.ts".intern(),
                input_name: "input".intern(),
                item_type: PlaylistItemType::Live,
                xtream_cluster: XtreamCluster::Live,
                ..PlaylistItemHeader::default()
            },
        };
        item.header.freeze_input_stream_id();

        let xtream_item = XtreamPlaylistItem::from(&item);
        assert_eq!(xtream_item.provider_id, 12345);
    }

    #[test]
    fn xtream_playlist_item_preserves_numeric_input_stream_id_as_string() {
        let mut item = PlaylistItem {
            header: PlaylistItemHeader {
                id: "80510".intern(),
                virtual_id: 1001,
                url: "http://provider.example/live/user/pass/80510.ts".intern(),
                input_name: "input".intern(),
                item_type: PlaylistItemType::Live,
                xtream_cluster: XtreamCluster::Live,
                ..PlaylistItemHeader::default()
            },
        };
        item.header.freeze_input_stream_id();

        let xtream_item = XtreamPlaylistItem::from(&item);

        assert_eq!(xtream_item.provider_id, 80510);
        assert_eq!(xtream_item.input_stream_id.as_ref(), "80510");
        assert_eq!(xtream_item.get_input_stream_id().as_deref(), Some("80510"));
    }

    #[test]
    fn alphanumeric_m3u_input_stream_id_survives_xtream_materialization() {
        let mut source = PlaylistItem {
            header: PlaylistItemHeader {
                id: "channel-alpha".intern(),
                url: "http://provider.example/live/user/pass/12345.ts".intern(),
                input_name: "input".intern(),
                item_type: PlaylistItemType::Live,
                xtream_cluster: XtreamCluster::Live,
                ..PlaylistItemHeader::default()
            },
        };
        source.header.freeze_input_stream_id();

        let m3u_item = M3uPlaylistItem::from(&source);
        let common_item = PlaylistItem::from(&m3u_item);
        let xtream_item = XtreamPlaylistItem::from(&common_item);

        assert_eq!(m3u_item.input_stream_id.as_ref(), "channel-alpha");
        assert_eq!(xtream_item.provider_id, 12345);
        assert_eq!(xtream_item.input_stream_id.as_ref(), "channel-alpha");
    }

    #[test]
    fn url_hash_input_stream_id_is_preserved_verbatim() {
        let mut source = PlaylistItem {
            header: PlaylistItemHeader {
                id: "d34db33f".intern(),
                url: "http://provider.example/live/channel.m3u8".intern(),
                ..PlaylistItemHeader::default()
            },
        };
        source.header.freeze_input_stream_id();

        let m3u_item = M3uPlaylistItem::from(&source);

        assert_eq!(m3u_item.input_stream_id.as_ref(), "d34db33f");
    }

    #[test]
    fn target_field_mapping_cannot_change_frozen_input_stream_id() {
        let mut header = PlaylistItemHeader { id: "origin-alpha".intern(), ..PlaylistItemHeader::default() };
        header.freeze_input_stream_id();

        assert!(header.set_field("id", "target-id"));

        assert_eq!(header.id.as_ref(), "target-id");
        assert_eq!(header.input_stream_id.as_ref(), "origin-alpha");
        assert!(!header.set_field("input_stream_id", "unexpected"));
        assert_eq!(header.input_stream_id.as_ref(), "origin-alpha");
    }

    #[test]
    fn legacy_playlist_items_fall_back_to_provider_id_without_virtual_id() {
        let mut source = PlaylistItem {
            header: PlaylistItemHeader {
                id: "legacy-alpha".intern(),
                virtual_id: 7001,
                url: "http://provider.example/live/user/pass/80510.ts".intern(),
                ..PlaylistItemHeader::default()
            },
        };
        source.header.freeze_input_stream_id();
        let mut m3u_item = M3uPlaylistItem::from(&source);
        m3u_item.input_stream_id = "".intern();
        let mut xtream_item = XtreamPlaylistItem::from(&source);
        xtream_item.input_stream_id = "".intern();

        assert_eq!(m3u_item.get_input_stream_id().as_deref(), Some("legacy-alpha"));
        assert_eq!(xtream_item.get_input_stream_id().as_deref(), Some("80510"));
        xtream_item.provider_id = 0;
        xtream_item.url = "http://provider.example/live/channel.ts".intern();
        assert_eq!(xtream_item.get_input_stream_id(), None);

        let mut legacy_common_item = PlaylistItem::from(&xtream_item);
        assert!(legacy_common_item.header.id.is_empty());
        assert_eq!(legacy_common_item.get_input_stream_id(), None);
        legacy_common_item.header.id = "target-mapped-id".intern();
        let rematerialized_m3u_item = M3uPlaylistItem::from(&legacy_common_item);
        let rematerialized_xtream_item = XtreamPlaylistItem::from(&legacy_common_item);
        assert_eq!(legacy_common_item.header.id.as_ref(), "target-mapped-id");
        assert_eq!(legacy_common_item.get_input_stream_id(), None);
        assert!(rematerialized_m3u_item.provider_id.is_empty());
        assert_eq!(rematerialized_m3u_item.get_input_stream_id(), None);
        assert_eq!(rematerialized_xtream_item.provider_id, 0);
        assert_eq!(rematerialized_xtream_item.get_input_stream_id(), None);

        xtream_item.input_stream_id = "explicit-origin-alpha".intern();
        assert_eq!(xtream_item.get_input_stream_id().as_deref(), Some("explicit-origin-alpha"));
        let identified_common_item = PlaylistItem::from(&xtream_item);
        assert_eq!(identified_common_item.header.input_stream_id.as_ref(), "explicit-origin-alpha");
        assert_eq!(identified_common_item.get_input_stream_id().as_deref(), Some("explicit-origin-alpha"));

        let identified_m3u_item = M3uPlaylistItem::from(&identified_common_item);
        let identified_xtream_item = XtreamPlaylistItem::from(&identified_common_item);
        assert_eq!(identified_m3u_item.input_stream_id.as_ref(), "explicit-origin-alpha");
        assert_eq!(identified_m3u_item.get_input_stream_id().as_deref(), Some("explicit-origin-alpha"));
        assert_eq!(identified_xtream_item.provider_id, 0);
        assert_eq!(identified_xtream_item.input_stream_id.as_ref(), "explicit-origin-alpha");
        assert_eq!(identified_xtream_item.get_input_stream_id().as_deref(), Some("explicit-origin-alpha"));
    }

    #[test]
    fn m3u_to_m3u_emits_tvg_id_from_epg_channel_id() {
        let item = M3uPlaylistItem {
            virtual_id: 0,
            provider_id: "prov1".intern(),
            input_stream_id: "prov1".intern(),
            name: "Test Channel".intern(),
            chno: 0,
            logo: "".intern(),
            logo_small: "".intern(),
            group: "Test Group".intern(),
            title: "Test Title".intern(),
            parent_code: "".intern(),
            audio_track: "".intern(),
            time_shift: "".intern(),
            rec: "".intern(),
            url: "http://example.com/stream".intern(),
            epg_channel_id: Some("epg_channel_123".intern()),
            input_name: "test".intern(),
            item_type: PlaylistItemType::Live,
            t_stream_url: "".intern(),
            t_resource_url: None,
            t_catchup_source: None,
            t_catchup_mode: None,
            source_ordinal: 0,
            additional_properties: None,
        };

        let output = item.to_m3u(None, false);
        assert!(output.contains(r#"tvg-id="epg_channel_123""#), "M3U output must contain tvg-id from epg_channel_id");
    }

    #[test]
    fn m3u_to_m3u_preserves_mixed_case_tvg_id() {
        // EPG matching is case-insensitive, so ids are no longer lowercased when parsed.
        // The M3U tvg-id output must preserve the channel's original source case.
        let item = M3uPlaylistItem {
            virtual_id: 0,
            provider_id: "prov1".intern(),
            input_stream_id: "prov1".intern(),
            name: "Test Channel".intern(),
            chno: 0,
            logo: "".intern(),
            logo_small: "".intern(),
            group: "Test Group".intern(),
            title: "Test Title".intern(),
            parent_code: "".intern(),
            audio_track: "".intern(),
            time_shift: "".intern(),
            rec: "".intern(),
            url: "http://example.com/stream".intern(),
            epg_channel_id: Some("CNN.us".intern()),
            input_name: "test".intern(),
            item_type: PlaylistItemType::Live,
            t_stream_url: "".intern(),
            t_resource_url: None,
            t_catchup_source: None,
            t_catchup_mode: None,
            source_ordinal: 0,
            additional_properties: None,
        };

        let output = item.to_m3u(None, false);
        assert!(output.contains(r#"tvg-id="CNN.us""#), "M3U tvg-id must preserve original case, got: {output}");
    }

    #[test]
    fn m3u_to_m3u_emits_tvg_chno_from_chno() {
        let item = M3uPlaylistItem {
            virtual_id: 0,
            provider_id: "prov1".intern(),
            input_stream_id: "prov1".intern(),
            name: "Test Channel".intern(),
            chno: 42,
            logo: "".intern(),
            logo_small: "".intern(),
            group: "Test Group".intern(),
            title: "Test Title".intern(),
            parent_code: "".intern(),
            audio_track: "".intern(),
            time_shift: "".intern(),
            rec: "".intern(),
            url: "http://example.com/stream".intern(),
            epg_channel_id: None,
            input_name: "test".intern(),
            item_type: PlaylistItemType::Live,
            t_stream_url: "".intern(),
            t_resource_url: None,
            t_catchup_source: None,
            t_catchup_mode: None,
            source_ordinal: 0,
            additional_properties: None,
        };

        let output = item.to_m3u(None, false);
        assert!(output.contains(r#"tvg-chno="42""#), "M3U output must contain tvg-chno from chno");
    }

    #[test]
    fn m3u_to_m3u_omits_tvg_chno_when_chno_is_zero() {
        let item = M3uPlaylistItem {
            virtual_id: 0,
            provider_id: "prov1".intern(),
            input_stream_id: "prov1".intern(),
            name: "Test Channel".intern(),
            chno: 0,
            logo: "".intern(),
            logo_small: "".intern(),
            group: "Test Group".intern(),
            title: "Test Title".intern(),
            parent_code: "".intern(),
            audio_track: "".intern(),
            time_shift: "".intern(),
            rec: "".intern(),
            url: "http://example.com/stream".intern(),
            epg_channel_id: None,
            input_name: "test".intern(),
            item_type: PlaylistItemType::Live,
            t_stream_url: "".intern(),
            t_resource_url: None,
            t_catchup_source: None,
            t_catchup_mode: None,
            source_ordinal: 0,
            additional_properties: None,
        };

        let output = item.to_m3u(None, false);
        assert!(!output.contains("tvg-chno"), "M3U output must not contain tvg-chno when chno is 0");
    }

    #[test]
    fn m3u_to_m3u_preserves_catchup_attributes() {
        let item = M3uPlaylistItem {
            virtual_id: 0,
            provider_id: "prov1".intern(),
            input_stream_id: "prov1".intern(),
            name: "Test Channel".intern(),
            chno: 0,
            logo: "".intern(),
            logo_small: "".intern(),
            group: "Test Group".intern(),
            title: "Test Title".intern(),
            parent_code: "".intern(),
            audio_track: "".intern(),
            time_shift: "".intern(),
            rec: "".intern(),
            url: "http://example.com/stream".intern(),
            epg_channel_id: Some("channel1".intern()),
            input_name: "test".intern(),
            item_type: PlaylistItemType::Live,
            t_stream_url: "".intern(),
            t_resource_url: None,
            t_catchup_source: None,
            t_catchup_mode: None,
            source_ordinal: 0,
            additional_properties: Some(StreamProperties::Live(Box::new(LiveStreamProperties {
                catchup: Some(CatchupProperties {
                    mode: Some("append".intern()),
                    days: Some("7".intern()),
                    source: Some("?offset=-${offset}&utcstart=${timestamp}".intern()),
                    correction: Some("-2.0".intern()),
                    catchup_type: Some("xc".intern()),
                    extra_attributes: vec![CatchupAttribute { name: "catchup-extra".intern(), value: "keep".intern() }],
                    ..CatchupProperties::default()
                }),
                ..LiveStreamProperties::default()
            }))),
        };

        let output = item.to_m3u(None, false);
        assert!(output.contains(r#"catchup="append""#));
        assert!(output.contains(r#"catchup-days="7""#));
        assert!(output.contains(r#"catchup-source="?offset=-${offset}&utcstart=${timestamp}""#));
        assert!(output.contains(r#"catchup-correction="-2.0""#));
        assert!(output.contains(r#"catchup-type="xc""#));
        assert!(output.contains(r#"catchup-extra="keep""#));
    }

    #[test]
    fn m3u_to_m3u_uses_rewritten_catchup_mode_and_source() {
        let item = M3uPlaylistItem {
            virtual_id: 0,
            provider_id: "prov1".intern(),
            input_stream_id: "prov1".intern(),
            name: "Test Channel".intern(),
            chno: 0,
            logo: "".intern(),
            logo_small: "".intern(),
            group: "Test Group".intern(),
            title: "Test Title".intern(),
            parent_code: "".intern(),
            audio_track: "".intern(),
            time_shift: "".intern(),
            rec: "".intern(),
            url: "http://example.com/stream".intern(),
            epg_channel_id: Some("channel1".intern()),
            input_name: "test".intern(),
            item_type: PlaylistItemType::Live,
            t_stream_url: "".intern(),
            t_resource_url: None,
            t_catchup_source: Some("http://proxy.example/m3u-catchup/token?v0={utc}".intern()),
            t_catchup_mode: Some("default".intern()),
            source_ordinal: 0,
            additional_properties: Some(StreamProperties::Live(Box::new(LiveStreamProperties {
                catchup: Some(CatchupProperties {
                    mode: Some("append".intern()),
                    source: Some("?offset=-${offset}".intern()),
                    ..CatchupProperties::default()
                }),
                ..LiveStreamProperties::default()
            }))),
        };

        let output = item.to_m3u(None, false);
        assert!(output.contains(r#"catchup="default""#));
        assert!(output.contains(r#"catchup-source="http://proxy.example/m3u-catchup/token?v0={utc}""#));
        assert!(!output.contains(r#"catchup-source="?offset=-${offset}""#));
    }
}
