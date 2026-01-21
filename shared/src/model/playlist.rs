use crate::utils::{arc_str_option_serde, arc_str_serde, extract_extension_from_url, generate_playlist_uuid,
                   get_provider_id, Internable};
use crate::model::{xtream_const, ClusterFlags, CommonPlaylistItem, ConfigTargetOptions, EpisodeStreamProperties,
                   SeriesStreamProperties, StreamProperties, VideoStreamProperties, XtreamInfoDocument};
use enum_iterator::Sequence;
use serde::{Deserialize, Serialize};
use std::fmt::Write;
use std::fmt::{Display, Formatter};
use std::str::FromStr;
use std::sync::Arc;
use crate::model::UUIDType;
// https://de.wikipedia.org/wiki/M3U
// https://siptv.eu/howto/playlist.html

pub type VirtualId = u32;

#[derive(Debug, Copy, Clone, Eq, Hash, PartialEq, Serialize, Deserialize, Default)]
#[repr(u8)]
pub enum XtreamCluster {
    #[default]
    Live = 1,
    Video = 2,
    Series = 3,
}

impl XtreamCluster {
    pub const fn as_str(&self) -> &str {
        match self {
            Self::Live => "Live",
            Self::Video => "Video",
            Self::Series => "Series",
        }
    }
    pub const fn as_stream_type(&self) -> &str {
        match self {
            Self::Live => "live",
            Self::Video => "movie",
            Self::Series => "series",
        }
    }
}

impl FromStr for XtreamCluster {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "live" => Ok(XtreamCluster::Live),
            "video" | "vod" | "movie" => Ok(XtreamCluster::Video),
            "series" => Ok(XtreamCluster::Series),
            _ => Err(format!("Invalid XtreamCluster: {s}")),
        }
    }
}

impl Display for XtreamCluster {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl TryFrom<PlaylistItemType> for XtreamCluster {
    type Error = String;
    fn try_from(item_type: PlaylistItemType) -> Result<Self, Self::Error> {
        match item_type {
            PlaylistItemType::Live | PlaylistItemType::LiveHls | PlaylistItemType::LiveDash | PlaylistItemType::LiveUnknown => Ok(Self::Live),
            PlaylistItemType::Catchup | PlaylistItemType::Video | PlaylistItemType::LocalVideo => Ok(Self::Video),
            PlaylistItemType::Series | PlaylistItemType::SeriesInfo | PlaylistItemType::LocalSeries | PlaylistItemType::LocalSeriesInfo => Ok(Self::Series),
        }
    }
}

#[derive(Debug, Copy, Clone, Eq, Hash, PartialEq, Serialize, Deserialize, Default, Sequence)]
#[repr(u8)]
pub enum PlaylistItemType {
    #[default]
    Live = 1,
    Video = 2,
    Series = 3, //  xtream series description
    SeriesInfo = 4, //  xtream series info fetched for series description
    Catchup = 5,
    LiveUnknown = 6, // No Provider id
    LiveHls = 7, // m3u8 entry
    LiveDash = 8, // mpd
    LocalVideo = 9,
    LocalSeries = 10,
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
    type Err = String;

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
            _ => Err(format!("Invalid PlaylistItemType: {s}")),
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
        matches!(self, PlaylistItemType::Live | PlaylistItemType::LiveDash | PlaylistItemType::LiveHls | PlaylistItemType::LiveUnknown)
    }

    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Live | Self::LiveHls | Self::LiveDash | Self::LiveUnknown => Self::LIVE,
            Self::Video | Self::LocalVideo => Self::VIDEO,
            Self::Series | Self::LocalSeries => Self::SERIES,
            Self::SeriesInfo | Self::LocalSeriesInfo => Self::SERIES_INFO,
            Self::Catchup => Self::CATCHUP,
        }
    }

    pub fn is_cluster(&self, cluster: XtreamCluster) -> bool {
        match self {
            Self::Live | Self::LiveHls | Self::LiveDash | Self::LiveUnknown => cluster == XtreamCluster::Live,
            Self::Catchup | Self::Video | Self::LocalVideo => cluster == XtreamCluster::Video,
            Self::Series | Self::LocalSeries
            | Self::SeriesInfo | Self::LocalSeriesInfo => cluster == XtreamCluster::Series,
        }
    }
}

impl Display for PlaylistItemType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl Internable for PlaylistItemType {
    fn intern(self) -> Arc<str> {
        self.as_str().intern()
    }
}

#[derive(Copy, Clone, Default, Debug)]
pub struct PlaylistItemTypeSet(u16);
impl PlaylistItemTypeSet {
    #[inline]
    pub fn empty() -> Self {
        Self(0)
    }

    #[inline]
    pub fn from_item(item: PlaylistItemType) -> Self {
        let bit = 1u16 << ((item as u8) - 1);
        Self(bit)
    }

    #[inline]
    pub fn insert(&mut self, item: PlaylistItemType) {
        self.0 |= 1u16 << ((item as u8) - 1);
    }

    #[inline]
    pub fn remove(&mut self, item: PlaylistItemType) {
        self.0 &= !(1u16 << ((item as u8) - 1));
    }

    #[inline]
    pub fn is_set(&self, item: PlaylistItemType) -> bool {
        (self.0 & (1u16 << ((item as u8) - 1))) != 0
    }

    #[inline]
    pub fn bits(self) -> u16 {
        self.0
    }
}


pub trait FieldGetAccessor {
    fn get_field(&self, field: &str) -> Option<Arc<str>>;
}
pub trait FieldSetAccessor {
    fn set_field(&mut self, field: &str, value: &str) -> bool;
}

pub trait PlaylistEntry: Send + Sync {
    fn get_virtual_id(&self) -> VirtualId;
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
    pub virtual_id: VirtualId, // virtual id
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
    pub xtream_cluster: XtreamCluster,
    pub additional_properties: Option<StreamProperties>,
    #[serde(default)]
    pub item_type: PlaylistItemType,
    #[serde(default)]
    pub category_id: u32,
    #[serde(with = "arc_str_serde")]
    pub input_name: Arc<str>,
    #[serde(default)]
    pub source_ordinal: u32,
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
        }
    }
}

impl PlaylistItemHeader {
    pub fn gen_uuid(&mut self) {
        self.uuid = generate_playlist_uuid(&self.input_name, &self.id, self.item_type, &self.url);
    }
    pub const fn get_uuid(&self) -> &UUIDType {
        &self.uuid
    }

    pub fn get_provider_id(&mut self) -> Option<u32> {
        match get_provider_id(&self.id, &self.url) {
            None => None,
            Some(newid) => {
                self.id = newid.to_string().intern();
                Some(newid)
            }
        }
    }

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
    #[serde(with = "arc_str_serde")]
    pub t_stream_url: Arc<str>,
    #[serde(skip)]
    pub t_resource_url: Option<String>,
    #[serde(default)]
    pub source_ordinal: u32,
}

impl M3uPlaylistItem {
    #[allow(clippy::missing_panics_doc)]
    pub fn to_m3u(&self, target_options: Option<&ConfigTargetOptions>, rewrite_urls: bool) -> String {
        let options = target_options.as_ref();
        let ignore_logo = options.is_some_and(|o| o.ignore_logo);
        let mut line = String::with_capacity(256);
        let _ = write!(&mut line, "#EXTINF:-1 tvg-id=\"{}\" tvg-name=\"{}\" group-title=\"{}\"",
                       self.epg_channel_id.as_ref().map_or("", |o| o.as_ref()),
                       self.name, self.group);

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
            additional_properties: None,
            category_id: None,
        }
    }
}

impl PlaylistEntry for M3uPlaylistItem {
    #[inline]
    fn get_virtual_id(&self) -> VirtualId {
        self.virtual_id
    }

    fn get_provider_id(&self) -> Option<u32> {
        get_provider_id(&self.provider_id, &self.url)
    }
    #[inline]
    fn get_category_id(&self) -> Option<u32> {
        None
    }
    #[inline]
    fn get_provider_url(&self) ->  Arc<str> {
        Arc::clone(&self.url)
    }

    fn get_uuid(&self) -> UUIDType {
        generate_playlist_uuid(&self.input_name, &self.provider_id, self.item_type, &self.url)
    }

    #[inline]
    fn get_item_type(&self) -> PlaylistItemType {
        self.item_type
    }

    #[inline]
    fn get_group(&self) -> Arc<str> {
        Arc::clone(&self.group)
    }

    #[inline]
    fn get_name(&self) -> Arc<str> {
        if self.title.is_empty() {
            Arc::clone(&self.name)
        } else {
            Arc::clone(&self.title)
        }
    }

    #[inline]
    fn get_resolved_info_document(&self, _options: &XtreamMappingOptions) -> Option<XtreamInfoDocument> {
        None
    }
    #[inline]
    fn get_additional_properties(&self) -> Option<&StreamProperties> {
        None
    }
    #[inline]
    fn get_additional_properties_mut(&mut self) -> Option<&mut StreamProperties> {
        None
    }

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
    fn from(item: M3uPlaylistItem) -> Self {
        item.to_common()
    }
}

#[allow(clippy::struct_excessive_bools)]
pub struct XtreamMappingOptions {
    pub skip_live_direct_source: bool,
    pub skip_video_direct_source: bool,
    pub skip_series_direct_source: bool,
    pub rewrite_resource_url: bool,
    pub force_redirect: Option<ClusterFlags>,
    pub reverse_item_types: PlaylistItemTypeSet,
    pub username: String,
    pub password: String,
    pub base_url: Option<String>,
}

impl XtreamMappingOptions {
    #[inline]
    pub fn is_reverse(&self, item_type: PlaylistItemType) -> bool {
        self.reverse_item_types.is_set(item_type)
    }

    pub fn get_resource_url(&self, xtream_cluster: XtreamCluster, item_type: PlaylistItemType, virtual_id: VirtualId) -> Option<String> {
        let is_reverse = self.is_reverse(item_type);
        let resource_url = if is_reverse && self.rewrite_resource_url && self.base_url.is_some() {
            let resource_url = format!("{}/resource/{}/{}/{}/{}", self.base_url.as_ref().map_or_else(String::new, |b| b.clone()),
                                       xtream_cluster.as_stream_type(), self.username, self.password, virtual_id);
            Some(resource_url)
        } else {
            None
        };
        resource_url
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
    pub additional_properties: Option<StreamProperties>,
    pub item_type: PlaylistItemType,
    pub category_id: u32,
    #[serde(with = "arc_str_serde")]
    pub input_name: Arc<str>,
    pub channel_no: u32,
    #[serde(default)]
    pub source_ordinal: u32,
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
            Some(ref props) => {
                match props {
                    StreamProperties::Live(_) => Some("ts".intern()),
                    StreamProperties::Video(video) => Some(Arc::clone(&video.container_extension)),
                    StreamProperties::Series(_) => None,
                    StreamProperties::Episode(episode) => Some(Arc::clone(&episode.container_extension)),
                }
            }
        }
    }

    #[inline]
    pub fn has_details(&self) -> bool {
        self.additional_properties.as_ref().is_some_and(|p| p.has_details())
    }

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
    fn get_virtual_id(&self) -> VirtualId {
        self.virtual_id
    }
    #[inline]
    fn get_provider_id(&self) -> Option<u32> {
        Some(self.provider_id)
    }
    #[inline]
    fn get_category_id(&self) -> Option<u32> {
        Some(self.category_id)
    }
    #[inline]
    fn get_provider_url(&self) ->  Arc<str> {
        Arc::clone(&self.url)
    }

    #[inline]
    fn get_uuid(&self) -> UUIDType {
        generate_playlist_uuid(&self.input_name, &self.provider_id.to_string(), self.item_type, &self.url)
    }
    #[inline]
    fn get_item_type(&self) -> PlaylistItemType {
        self.item_type
    }
    #[inline]
    fn get_group(&self) -> Arc<str> {
        Arc::clone(&self.group)
    }
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
            self.additional_properties.as_ref()
                .map(|p| p.to_info_document(options, self.get_item_type(),
                                            self.get_virtual_id(), self.get_category_id().unwrap_or(0)))
        } else {
            None
        }
    }

    #[inline]
    fn get_additional_properties(&self) -> Option<&StreamProperties> {
        self.additional_properties.as_ref()
    }
    #[inline]
    fn get_additional_properties_mut(&mut self) -> Option<&mut StreamProperties> {
        self.additional_properties.as_mut()
    }
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
    fn from(item: XtreamPlaylistItem) -> Self {
        item.to_common()
    }
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
                        let container_extension = extract_extension_from_url(&header.url).map(|e| e.strip_prefix('.').unwrap_or(e).to_string()).unwrap_or_default();
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
                            let container_extension = extract_extension_from_url(&header.url).map(|e| e.strip_prefix('.').unwrap_or(e).to_string()).unwrap_or_default();
                            // TODO maybe from link ? like s01e02 or something like this
                            Some(StreamProperties::Episode(EpisodeStreamProperties {
                                episode_id: 0,
                                episode: 0,
                                season: 0,
                                added: None,
                                release_date: None,
                                tmdb: None,
                                movie_image: "".intern(),
                                container_extension: container_extension.intern(),
                                audio: None,
                                video: None,
                            }))
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

    pub fn has_details(&self) -> bool {
        self.header.additional_properties.as_ref().is_some_and(|p| p.has_details())
    }

    pub fn get_tmdb_id(&self) -> Option<u32> {
        self.header.additional_properties.as_ref().and_then(|p| p.get_tmdb_id())
    }
}

impl From<&PlaylistItem> for XtreamPlaylistItem {
    fn from(item: &PlaylistItem) -> Self {
        let header = &item.header;
        let provider_id = header.id.parse::<u32>().unwrap_or_default();
        let additional_properties = PlaylistItem::get_additional_properties(header);

        XtreamPlaylistItem {
            virtual_id: header.virtual_id,
            provider_id,
            name: if header.item_type == PlaylistItemType::Series { Arc::clone(&header.title) } else { Arc::clone(&header.name) },
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
        }
    }
}

impl From<&PlaylistItem> for M3uPlaylistItem {
    fn from(item: &PlaylistItem) -> Self {
        let header = &item.header;
        M3uPlaylistItem {
            virtual_id: header.virtual_id,
            provider_id: Arc::clone(&header.id),
            name: if header.item_type == PlaylistItemType::Series { Arc::clone(&header.title) } else { Arc::clone(&header.name) },
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
            source_ordinal: header.source_ordinal,
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
            name: if header.item_type == PlaylistItemType::Series { Arc::clone(&header.title) } else { Arc::clone(&header.name) },
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
        let header = PlaylistItemHeader {
            uuid: item.get_uuid(),
            virtual_id: item.virtual_id,
            id: item.provider_id.to_string().intern(),
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
        };

        PlaylistItem {
            header
        }
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
            additional_properties: None,
            source_ordinal: item.source_ordinal,
        };

        PlaylistItem {
            header
        }
    }
}


impl PlaylistEntry for PlaylistItem {
    #[inline]
    fn get_virtual_id(&self) -> VirtualId {
        self.header.virtual_id
    }

    fn get_provider_id(&self) -> Option<u32> {
        let header = &self.header;
        get_provider_id(&header.id, &header.url)
    }

    #[inline]
    fn get_category_id(&self) -> Option<u32> {
        Some(self.header.category_id)
    }

    #[inline]
    fn get_provider_url(&self) ->  Arc<str> {
        Arc::clone(&self.header.url)
    }

    #[inline]
    fn get_uuid(&self) -> UUIDType {
        let header = &self.header;
        generate_playlist_uuid(&header.input_name, &header.id, header.item_type, &header.url)
    }

    #[inline]
    fn get_item_type(&self) -> PlaylistItemType {
        self.header.item_type
    }

    #[inline]
    fn get_group(&self) -> Arc<str> {
        Arc::clone(&self.header.group)
    }

    #[inline]
    fn get_name(&self) -> Arc<str> {
        if self.header.title.is_empty() {
            Arc::clone(&self.header.name)
        } else {
            Arc::clone(&self.header.title)
        }
    }

    fn get_resolved_info_document(&self, options: &XtreamMappingOptions) -> Option<XtreamInfoDocument> {
        if self.has_details() {
            self.header.additional_properties.as_ref().map(|p|
                p.to_info_document(options, self.get_item_type(), self.get_virtual_id(),
                                   self.get_category_id().unwrap_or(0)))
        } else {
            None
        }
    }

    fn get_additional_properties(&self) -> Option<&StreamProperties> {
        self.header.additional_properties.as_ref()
    }
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
