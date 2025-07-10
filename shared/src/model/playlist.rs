use std::borrow::Cow;
use std::fmt::{Display, Formatter};
use std::str::FromStr;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use crate::model::{ConfigTargetOptions, xtream_const};
use crate::utils::{extract_extension_from_url, generate_playlist_uuid, get_provider_id, get_string_from_serde_value, get_u64_from_serde_value};

// https://de.wikipedia.org/wiki/M3U
// https://siptv.eu/howto/playlist.html

pub type UUIDType = [u8; 32];

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
            PlaylistItemType::Catchup | PlaylistItemType::Video => Ok(Self::Video),
            PlaylistItemType::Series | PlaylistItemType::SeriesInfo => Ok(Self::Series),
        }
    }
}

#[derive(Debug, Copy, Clone, Eq, Hash, PartialEq, Serialize, Deserialize, Default)]
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
            "Series" => Ok(PlaylistItemType::Series),
            "SeriesInfo" => Ok(PlaylistItemType::SeriesInfo),
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
}

impl Display for PlaylistItemType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", match self {
            Self::Live | Self::LiveHls | Self::LiveDash | Self::LiveUnknown => Self::LIVE,
            Self::Video => Self::VIDEO,
            Self::Series => Self::SERIES,
            Self::SeriesInfo => Self::SERIES_INFO,
            Self::Catchup => Self::CATCHUP,
        })
    }
}

pub trait FieldGetAccessor {
    fn get_field(&self, field: &str) -> Option<Cow<str>>;
}
pub trait FieldSetAccessor {
    fn set_field(&mut self, field: &str, value: &str) -> bool;
}

pub trait PlaylistEntry: Send + Sync {
    fn get_virtual_id(&self) -> u32;
    fn get_provider_id(&self) -> Option<u32>;
    fn get_category_id(&self) -> Option<u32>;
    fn get_provider_url(&self) -> String;
    fn get_uuid(&self) -> UUIDType;
    fn get_item_type(&self) -> PlaylistItemType;
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlaylistItemHeader {
    pub uuid: UUIDType, // calculated
    pub id: String, // provider id
    pub virtual_id: u32, // virtual id
    pub name: String,
    pub chno: String,
    pub logo: String,
    pub logo_small: String,
    pub group: String,
    pub title: String,
    pub parent_code: String,
    pub audio_track: String,
    pub time_shift: String,
    pub rec: String,
    pub url: String,
    pub epg_channel_id: Option<String>,
    pub xtream_cluster: XtreamCluster,
    pub additional_properties: Option<Value>,
    #[serde(default, skip_serializing, skip_deserializing)]
    pub item_type: PlaylistItemType,
    #[serde(default)]
    pub category_id: u32,
    pub input_name: String,
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
                self.id = newid.to_string();
                Some(newid)
            }
        }
    }

    pub fn get_additional_property(&self, field: &str) -> Option<&Value> {
        self.additional_properties.as_ref().and_then(|v| match v {
            Value::Object(map) => {
                map.get(field)
            }
            _ => None,
        })
    }
    // pub fn get_additional_property_as_u32(&self, field: &str) -> Option<u32> {
    //     match self.get_additional_property(field) {
    //         Some(value) => get_u32_from_serde_value(value),
    //         None => None
    //     }
    // }
    pub fn get_additional_property_as_u64(&self, field: &str) -> Option<u64> {
        match self.get_additional_property(field) {
            Some(value) => get_u64_from_serde_value(value),
            None => None
        }
    }

    pub fn get_additional_property_as_str(&self, field: &str) -> Option<String> {
        match self.get_additional_property(field) {
            Some(value) => get_string_from_serde_value(value),
            None => None
        }
    }
}

macro_rules! to_m3u_non_empty_fields {
    ($header:expr, $line:expr, $(($prop:ident, $field:expr)),*;) => {
        $(
           if !$header.$prop.is_empty() {
                $line = format!("{} {}=\"{}\"", $line, $field, $header.$prop);
            }
         )*
    };
}

macro_rules! to_m3u_resource_non_empty_fields {
    ($header:expr, $url:expr, $line:expr, $(($prop:ident, $field:expr)),*;) => {
        $(
           if !$header.$prop.is_empty() {
                $line = format!("{} {}=\"{}/{}\"", $line, $field, $url, stringify!($prop));
            }
         )*
    };
}

macro_rules! generate_field_accessor_impl_for_playlist_item_header {
    ($($prop:ident),*;) => {
        impl crate::model::FieldGetAccessor for crate::model::PlaylistItemHeader {
            fn get_field(&self, field: &str) -> Option<Cow<str>> {
                let field = field.to_lowercase();
                match field.as_str() {
                    $(
                        stringify!($prop) => Some(Cow::Borrowed(&self.$prop)),
                    )*
                    "input" =>  Some(Cow::Borrowed(self.input_name.as_str())),
                    "type" => Some(Cow::Owned(self.item_type.to_string())),
                    "caption" =>  Some(if self.title.is_empty() { Cow::Borrowed(&self.name) } else { Cow::Borrowed(&self.title) }),
                    "epg_channel_id" | "epg_id" => self.epg_channel_id.as_ref().map(|s| Cow::Borrowed(s.as_str())),
                    _ => None,
                }
            }
         }
         impl crate::model::FieldSetAccessor for crate::model::PlaylistItemHeader {
            fn set_field(&mut self, field: &str, value: &str) -> bool {
                let field = field.to_lowercase();
                let val = String::from(value);
                match field.as_str() {
                    $(
                        stringify!($prop) => {
                            self.$prop = val;
                            true
                        }
                    )*
                    "caption" => {
                        self.title = val.clone();
                        self.name = val;
                        true
                    }
                    "epg_channel_id" | "epg_id" => {
                        self.epg_channel_id = Some(value.to_owned());
                        true
                    }
                    _ => false,
                }
            }
        }
    }
}

generate_field_accessor_impl_for_playlist_item_header!(id, /*virtual_id,*/ name, chno, logo, logo_small, group, title, parent_code, audio_track, time_shift, rec, url;);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct M3uPlaylistItem {
    pub virtual_id: u32,
    pub provider_id: String,
    pub name: String,
    pub chno: String,
    pub logo: String,
    pub logo_small: String,
    pub group: String,
    pub title: String,
    pub parent_code: String,
    pub audio_track: String,
    pub time_shift: String,
    pub rec: String,
    pub url: String,
    pub epg_channel_id: Option<String>,
    pub input_name: String,
    pub item_type: PlaylistItemType,
    #[serde(skip)]
    pub t_stream_url: String,
    #[serde(skip)]
    pub t_resource_url: Option<String>,
}

impl M3uPlaylistItem {
    #[allow(clippy::missing_panics_doc)]
    pub fn to_m3u(&self, target_options: Option<&ConfigTargetOptions>, rewrite_urls: bool) -> String {
        let options = target_options.as_ref();
        let ignore_logo = options.is_some_and(|o| o.ignore_logo);
        let mut line = format!("#EXTINF:-1 tvg-id=\"{}\" tvg-name=\"{}\" group-title=\"{}\"",
                               self.epg_channel_id.as_ref().map_or("", |o| o.as_ref()),
                               self.name, self.group);

        if !ignore_logo {
            if rewrite_urls && self.t_resource_url.is_some() {
                to_m3u_resource_non_empty_fields!(self, self.t_resource_url.as_ref().unwrap(), line, (logo, "tvg-logo"), (logo_small, "tvg-logo-small"););
            } else {
                to_m3u_non_empty_fields!(self, line, (logo, "tvg-logo"), (logo_small, "tvg-logo-small"););
            }
        }

        to_m3u_non_empty_fields!(self, line,
            (chno, "tvg-chno"),
            (parent_code, "parent-code"),
            (audio_track, "audio-track"),
            (time_shift, "timeshift"),
            (rec, "tvg-rec"););

        let url = if self.t_stream_url.is_empty() { &self.url } else { &self.t_stream_url };
        format!("{line},{}\n{url}", self.title, )
    }
}

impl PlaylistEntry for M3uPlaylistItem {
    #[inline]
    fn get_virtual_id(&self) -> u32 {
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
    fn get_provider_url(&self) -> String {
        self.url.to_string()
    }

    fn get_uuid(&self) -> UUIDType {
        generate_playlist_uuid(&self.input_name, &self.provider_id, self.item_type, &self.url)
    }

    #[inline]
    fn get_item_type(&self) -> PlaylistItemType {
        self.item_type
    }
}

macro_rules! generate_field_accessor_impl_for_m3u_playlist_item {
    ($($prop:ident),*;) => {
        impl crate::model::FieldGetAccessor for M3uPlaylistItem {
            fn get_field(&self, field: &str) -> Option<Cow<str>> {
                let field = field.to_lowercase();
                match field.as_str() {
                    $(
                        stringify!($prop) => Some(Cow::Borrowed(&self.$prop)),
                    )*
                    "caption" =>  Some(if self.title.is_empty() { Cow::Borrowed(&self.name) } else { Cow::Borrowed(&self.title) }),
                    "epg_channel_id" | "epg_id" => self.epg_channel_id.as_ref().map(|s| Cow::Borrowed(s.as_str())),
                    _ => None,
                }
            }
        }
    }
}

generate_field_accessor_impl_for_m3u_playlist_item!(provider_id, name, chno, logo, logo_small, group, title, parent_code, audio_track, time_shift, rec, url;);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XtreamPlaylistItem {
    pub virtual_id: u32,
    pub provider_id: u32,
    pub name: String,
    pub logo: String,
    pub logo_small: String,
    pub group: String,
    pub title: String,
    pub parent_code: String,
    pub rec: String,
    pub url: String,
    pub epg_channel_id: Option<String>,
    pub xtream_cluster: XtreamCluster,
    pub additional_properties: Option<String>,
    pub item_type: PlaylistItemType,
    pub category_id: u32,
    pub input_name: String,
    pub channel_no: u32,
}

impl XtreamPlaylistItem {

    pub fn get_additional_property(&self, field: &str) -> Option<Value> {
        if let Some(json) = self.additional_properties.as_ref() {
            if let Ok(Value::Object(props)) = serde_json::from_str(json) {
                return props.get(field).cloned();
            }
        }
        None
    }
}

impl PlaylistEntry for XtreamPlaylistItem {
    #[inline]
    fn get_virtual_id(&self) -> u32 {
        self.virtual_id
    }
    #[inline]
    fn get_provider_id(&self) -> Option<u32> {
        Some(self.provider_id)
    }
    #[inline]
    fn get_category_id(&self) -> Option<u32> {
        None
    }
    #[inline]
    fn get_provider_url(&self) -> String {
        self.url.to_string()
    }

    #[inline]
    fn get_uuid(&self) -> UUIDType {
        generate_playlist_uuid(&self.input_name, &self.provider_id.to_string(), self.item_type, &self.url)
    }
    #[inline]
    fn get_item_type(&self) -> PlaylistItemType {
        self.item_type
    }
}

pub fn get_backdrop_path_value<'a>(field: &'a str, value: Option<&'a Value>) -> Option<Cow<'a, str>> {
    match value {
        Some(Value::String(url)) => Some(Cow::Borrowed(url)),
        Some(Value::Array(values)) => {
            match values.as_slice() {
                [Value::String(single)] => Some(Cow::Borrowed(single)),
                multiple if !multiple.is_empty() => {
                    if let Some(index) = field.rfind('_') {
                        if let Ok(bd_index) = field[index + 1..].parse::<usize>() {
                            if let Some(Value::String(selected)) = multiple.get(bd_index) {
                                return Some(Cow::Borrowed(selected));
                            }
                        }
                    }
                    if let Value::String(url) = &multiple[0] {
                        Some(Cow::Borrowed(url))
                    } else {
                        None
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

macro_rules! generate_field_accessor_impl_for_xtream_playlist_item {
    ($($prop:ident),*;) => {
        impl crate::model::FieldGetAccessor for crate::model::XtreamPlaylistItem {
            fn get_field(&self, field: &str) -> Option<Cow<str>> {
                let field = field.to_lowercase();
                match field.as_str() {
                    $(
                        stringify!($prop) => Some(Cow::Borrowed(&self.$prop)),
                    )*
                    "caption" =>  Some(if self.title.is_empty() { Cow::Borrowed(&self.name) } else { Cow::Borrowed(&self.title) }),
                    "epg_channel_id" | "epg_id" => self.epg_channel_id.as_ref().map(|s| Cow::Borrowed(s.as_str())),
                    _ => {
                       if field.starts_with(xtream_const::XC_PROP_BACKDROP_PATH) || field == xtream_const::XC_PROP_COVER {
                            let props = self.additional_properties.as_ref().and_then(|add_props| serde_json::from_str::<Map<String, Value>>(add_props).ok());
                            return match props {
                                Some(doc) => {
                                    return if field == xtream_const::XC_PROP_COVER {
                                       doc.get(&field).and_then(|value| value.as_str()).map(|s| Cow::<str>::Owned(s.to_owned()))
                                    } else {
                                      get_backdrop_path_value(&field, doc.get(xtream_const::XC_PROP_BACKDROP_PATH)).map(|s| Cow::<str>::Owned(s.to_string()))
                                    }
                                }
                                _=> None,
                            }
                        }
                        None
                    },
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistItem {
    pub header: PlaylistItemHeader,
}

generate_field_accessor_impl_for_xtream_playlist_item!(name, logo, logo_small, group, title, parent_code, rec, url;);

impl PlaylistItem {
    pub fn to_m3u(&self) -> M3uPlaylistItem {
        let header = &self.header;
        M3uPlaylistItem {
            virtual_id: header.virtual_id,
            provider_id: header.id.to_string(),
            name: if header.item_type == PlaylistItemType::Series { &header.title } else { &header.name }.to_string(),
            chno: header.chno.to_string(),
            logo: header.logo.to_string(),
            logo_small: header.logo_small.to_string(),
            group: header.group.to_string(),
            title: header.title.to_string(),
            parent_code: header.parent_code.to_string(),
            audio_track: header.audio_track.to_string(),
            time_shift: header.time_shift.to_string(),
            rec: header.rec.to_string(),
            url: header.url.to_string(),
            epg_channel_id: header.epg_channel_id.clone(),
            input_name: header.input_name.to_string(),
            item_type: header.item_type,
            t_stream_url: header.url.to_string(),
            t_resource_url: None,
        }
    }

    pub fn to_xtream(&self) -> XtreamPlaylistItem {
        let header = &self.header;
        let provider_id = header.id.parse::<u32>().unwrap_or_default();
        let mut additional_properties = None;
        if header.xtream_cluster != XtreamCluster::Live {
            let add_ext = match header.get_additional_property("container_extension") {
                None => true,
                Some(ext) => ext.as_str().is_none_or(str::is_empty)
            };
            if add_ext {
                if let Some(cont_ext) = extract_extension_from_url(&header.url) {
                    let ext = if let Some(stripped) = cont_ext.strip_prefix('.') { stripped } else { cont_ext };
                    let mut result = match header.additional_properties.as_ref() {
                        None => Map::new(),
                        Some(props) => {
                            if let Value::Object(map) = props {
                                map.clone()
                            } else {
                                Map::new()
                            }
                        }
                    };
                    result.insert("container_extension".to_string(), Value::String(ext.to_string()));
                    additional_properties = serde_json::to_string(&Value::Object(result)).ok();
                }
            }
        }
        if additional_properties.is_none() {
            additional_properties = header.additional_properties.as_ref().and_then(|props| {
                serde_json::to_string(props).ok()
            });
        }
        // let additional_properties = header.additional_properties.as_ref().and_then(|props| {
        //     serde_json::to_string(props).ok()
        // });

        XtreamPlaylistItem {
            virtual_id: header.virtual_id,
            provider_id,
            name: if header.item_type == PlaylistItemType::Series { &header.title } else { &header.name }.to_string(),
            logo: header.logo.to_string(),
            logo_small: header.logo_small.to_string(),
            group: header.group.to_string(),
            title: header.title.to_string(),
            parent_code: header.parent_code.to_string(),
            rec: header.rec.to_string(),
            url: header.url.to_string(),
            epg_channel_id: header.epg_channel_id.clone(),
            xtream_cluster: header.xtream_cluster,
            additional_properties,
            item_type: header.item_type,
            category_id: header.category_id,
            input_name: header.input_name.to_string(),
            channel_no: header.chno.parse::<u32>().unwrap_or(0),
        }
    }
}

impl PlaylistEntry for PlaylistItem {
    #[inline]
    fn get_virtual_id(&self) -> u32 {
        self.header.virtual_id
    }

    fn get_provider_id(&self) -> Option<u32> {
        let header = &self.header;
        get_provider_id(&header.id, &header.url)
    }

    #[inline]
    fn get_category_id(&self) -> Option<u32> {
        None
    }

    #[inline]
    fn get_provider_url(&self) -> String {
        self.header.url.to_string()
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistGroup {
    pub id: u32,
    pub title: String,
    pub channels: Vec<PlaylistItem>,
    #[serde(skip_serializing, skip_deserializing)]
    pub xtream_cluster: XtreamCluster,
}

impl PlaylistGroup {
    #[inline]
    pub fn on_load(&mut self) {
        for pl in &mut self.channels {
            pl.header.gen_uuid();
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
