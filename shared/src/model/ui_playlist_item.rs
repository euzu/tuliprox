use crate::{
    model::{
        CommonPlaylistItem, M3uPlaylistItem, PlaylistItem, PlaylistItemType, StreamProperties, XtreamCluster,
        XtreamPlaylistItem,
    },
    utils::Internable,
};
use serde::{de::Visitor, ser::SerializeTupleStruct, Deserialize, Deserializer, Serialize, Serializer};
use std::sync::Arc;

/// Current override state of an item in the Explorer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct UiOverrideState {
    pub edited: bool,
    pub pending: bool,
    pub ambiguous: bool,
}

/// EPG state of a live channel computed from the loaded EPG data in the explorer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExplorerEpgState {
    Ok,
    MissingId,
    NotInSources,
    NoProgramme,
}

/// Explorer metadata for a playlist item.
///
/// Only sent when the Explorer asks for it (`explorer=true`), so other playlist consumers keep the
/// compact 12/13 element tuple.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct UiItemExplorerMeta {
    pub override_state: UiOverrideState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epg_state: Option<ExplorerEpgState>,
    /// Key of the item in the override files (`provider_id` for most items, `urlhash-v1:…` for M3U
    /// items without a provider id). `None` when no key can be computed.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::utils::arc_str_option_serde")]
    pub override_id: Option<Arc<str>>,
    /// Unix timestamp (seconds) the provider reports as "added" (VOD, series, live when present).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<u64>,
    /// Unix timestamp (seconds) of the last provider update (series).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<u64>,
    /// Channel number of the item as the playlist stores it (`None` when unset/0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chno: Option<u32>,
}

/// Lightweight playlist item for UI streaming.
#[derive(Debug, Clone, PartialEq)]
pub struct UiPlaylistItem {
    pub virtual_id: u32,
    pub provider_id: Arc<str>,
    pub name: Arc<str>,
    pub title: Arc<str>,
    pub group: Arc<str>,
    pub logo: Arc<str>,
    pub url: Arc<str>,
    pub item_type: PlaylistItemType,
    pub xtream_cluster: XtreamCluster,
    pub category_id: u32,
    pub rating: f64,
    pub input_name: Arc<str>,
    pub epg_channel_id: Option<Arc<str>>,
    pub explorer_meta: Option<UiItemExplorerMeta>,
}

impl UiPlaylistItem {
    #[must_use]
    pub fn override_state(&self) -> Option<UiOverrideState> { self.explorer_meta.as_ref().map(|m| m.override_state) }

    #[must_use]
    pub fn epg_state(&self) -> Option<ExplorerEpgState> { self.explorer_meta.as_ref().and_then(|m| m.epg_state) }
}

impl Serialize for UiPlaylistItem {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let len = if self.explorer_meta.is_some() {
            14
        } else if self.epg_channel_id.is_some() {
            13
        } else {
            12
        };
        let mut s = serializer.serialize_tuple_struct("UiPlaylistItem", len)?;
        s.serialize_field(&self.virtual_id)?;
        s.serialize_field(self.provider_id.as_ref())?;
        s.serialize_field(self.name.as_ref())?;
        s.serialize_field(self.title.as_ref())?;
        s.serialize_field(self.group.as_ref())?;
        s.serialize_field(self.logo.as_ref())?;
        s.serialize_field(self.url.as_ref())?;
        s.serialize_field(&self.item_type)?;
        s.serialize_field(&self.xtream_cluster)?;
        s.serialize_field(&self.category_id)?;
        s.serialize_field(&self.rating)?;
        s.serialize_field(self.input_name.as_ref())?;
        if len >= 13 {
            match &self.epg_channel_id {
                Some(id) => s.serialize_field(id.as_ref())?,
                None => s.serialize_field(&Option::<&str>::None)?,
            }
        }
        if len == 14 {
            if let Some(ref meta) = self.explorer_meta {
                s.serialize_field(meta)?;
            }
        }
        s.end()
    }
}

#[derive(Deserialize)]
#[serde(transparent)]
struct ArcStrWrap(#[serde(with = "crate::utils::arc_str_serde")] Arc<str>);

#[derive(Deserialize)]
#[serde(transparent)]
struct OptionArcStrWrap(#[serde(with = "crate::utils::arc_str_option_serde")] Option<Arc<str>>);

impl<'de> Deserialize<'de> for UiPlaylistItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct UiPlaylistItemVisitor;

        impl<'de> Visitor<'de> for UiPlaylistItemVisitor {
            type Value = UiPlaylistItem;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a tuple struct UiPlaylistItem with 12 to 14 elements")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let virtual_id: u32 = seq.next_element()?.ok_or_else(|| serde::de::Error::invalid_length(0, &self))?;
                let provider_id =
                    seq.next_element::<ArcStrWrap>()?.ok_or_else(|| serde::de::Error::invalid_length(1, &self))?.0;
                let name =
                    seq.next_element::<ArcStrWrap>()?.ok_or_else(|| serde::de::Error::invalid_length(2, &self))?.0;
                let title =
                    seq.next_element::<ArcStrWrap>()?.ok_or_else(|| serde::de::Error::invalid_length(3, &self))?.0;
                let group =
                    seq.next_element::<ArcStrWrap>()?.ok_or_else(|| serde::de::Error::invalid_length(4, &self))?.0;
                let logo =
                    seq.next_element::<ArcStrWrap>()?.ok_or_else(|| serde::de::Error::invalid_length(5, &self))?.0;
                let url =
                    seq.next_element::<ArcStrWrap>()?.ok_or_else(|| serde::de::Error::invalid_length(6, &self))?.0;
                let item_type = seq.next_element()?.ok_or_else(|| serde::de::Error::invalid_length(7, &self))?;
                let xtream_cluster = seq.next_element()?.ok_or_else(|| serde::de::Error::invalid_length(8, &self))?;
                let category_id = seq.next_element()?.ok_or_else(|| serde::de::Error::invalid_length(9, &self))?;
                let rating = seq.next_element()?.ok_or_else(|| serde::de::Error::invalid_length(10, &self))?;
                let input_name =
                    seq.next_element::<ArcStrWrap>()?.ok_or_else(|| serde::de::Error::invalid_length(11, &self))?.0;

                let epg_channel_id = seq.next_element::<OptionArcStrWrap>()?.and_then(|w| w.0);
                let explorer_meta = seq.next_element::<Option<UiItemExplorerMeta>>()?.flatten();

                while let Some(serde::de::IgnoredAny) = seq.next_element()? {}

                Ok(UiPlaylistItem {
                    virtual_id,
                    provider_id,
                    name,
                    title,
                    group,
                    logo,
                    url,
                    item_type,
                    xtream_cluster,
                    category_id,
                    rating,
                    input_name,
                    epg_channel_id,
                    explorer_meta,
                })
            }
        }

        deserializer.deserialize_seq(UiPlaylistItemVisitor)
    }
}

/// Helper to pick the best logo: prefer `logo` if non-empty, else `logo_small`
fn pick_logo(logo: &Arc<str>, logo_small: &Arc<str>, props: Option<&StreamProperties>) -> Arc<str> {
    if !logo.is_empty() {
        return Arc::clone(logo);
    }
    if !logo_small.is_empty() {
        return Arc::clone(logo_small);
    }

    props
        .and_then(|p| match p {
            StreamProperties::Video(v) => non_empty(&v.stream_icon).or_else(|| {
                v.details.as_ref().and_then(|d| {
                    non_empty_opt(d.movie_image.as_ref())
                        .or_else(|| non_empty_opt(d.cover_big.as_ref()))
                        .or_else(|| d.backdrop_path.as_ref().and_then(|b| non_empty(b.first()?)))
                })
            }),
            StreamProperties::Series(s) => {
                non_empty(&s.cover).or_else(|| s.backdrop_path.as_ref().and_then(|b| non_empty(b.first()?)))
            }
            _ => None,
        })
        .unwrap_or_else(|| "".intern())
}

fn non_empty(s: &Arc<str>) -> Option<Arc<str>> { (!s.is_empty()).then(|| Arc::clone(s)) }

fn non_empty_opt(s: Option<&Arc<str>>) -> Option<Arc<str>> { s.and_then(non_empty) }

/// Helper to get rating
#[inline]
fn get_rating(props: Option<&StreamProperties>) -> f64 {
    if let Some(p) = props {
        return match p {
            StreamProperties::Video(v) => v.rating.unwrap_or_default(),
            StreamProperties::Series(s) => s.rating,
            StreamProperties::Live(_) | StreamProperties::Episode(_) => 0.0,
        };
    }
    0.0
}

/// Helper to extract added and `last_modified` timestamps as unix seconds from `StreamProperties`.
pub fn extract_added_and_last_modified(props: Option<&StreamProperties>) -> (Option<u64>, Option<u64>) {
    let Some(props) = props else {
        return (None, None);
    };
    match props {
        StreamProperties::Live(live) => {
            let added = live.added.as_deref().and_then(|s| s.parse::<u64>().ok());
            (added, None)
        }
        StreamProperties::Video(video) => {
            let added = video.added.parse::<u64>().ok();
            (added, None)
        }
        StreamProperties::Series(series) => {
            let last_modified = series.last_modified.as_deref().and_then(|s| s.parse::<u64>().ok());
            let added = series
                .details
                .as_ref()
                .and_then(|details| details.episodes.as_ref())
                .and_then(|episodes| episodes.iter().filter_map(|ep| ep.added.parse::<u64>().ok()).max());
            (added, last_modified)
        }
        StreamProperties::Episode(episode) => {
            let added = episode.added.as_deref().and_then(|s| s.parse::<u64>().ok());
            (added, None)
        }
    }
}

/// Explorer metadata derivable from the stored record itself; override and EPG state are added later.
fn explorer_meta_for(props: Option<&StreamProperties>, chno: u32) -> UiItemExplorerMeta {
    let (added, last_modified) = extract_added_and_last_modified(props);
    UiItemExplorerMeta { added, last_modified, chno: (chno > 0).then_some(chno), ..Default::default() }
}

impl UiPlaylistItem {
    pub fn from_common(item: &CommonPlaylistItem, explorer: bool) -> Self {
        let explorer_meta = explorer.then(|| explorer_meta_for(item.additional_properties.as_ref(), item.chno));
        Self {
            virtual_id: item.virtual_id.get(),
            provider_id: Arc::clone(&item.provider_id),
            name: Arc::clone(&item.name),
            title: Arc::clone(&item.title),
            group: Arc::clone(&item.group),
            logo: pick_logo(&item.logo, &item.logo_small, item.additional_properties.as_ref()),
            url: Arc::clone(&item.url),
            item_type: item.item_type,
            xtream_cluster: item.xtream_cluster.unwrap_or_default(),
            category_id: item.category_id.unwrap_or(0),
            rating: get_rating(item.additional_properties.as_ref()),
            input_name: Arc::clone(&item.input_name),
            epg_channel_id: item.epg_channel_id.clone(),
            explorer_meta,
        }
    }

    pub fn from_xtream(item: XtreamPlaylistItem, explorer: bool) -> Self {
        let explorer_meta = explorer.then(|| explorer_meta_for(item.additional_properties.as_ref(), item.channel_no));
        Self {
            virtual_id: item.virtual_id.get(),
            provider_id: item.provider_id.to_string().into(),
            name: Arc::clone(&item.name),
            title: Arc::clone(&item.title),
            group: Arc::clone(&item.group),
            logo: pick_logo(&item.logo, &item.logo_small, item.additional_properties.as_ref()),
            url: Arc::clone(&item.url),
            item_type: item.item_type,
            xtream_cluster: item.xtream_cluster,
            category_id: item.category_id,
            rating: get_rating(item.additional_properties.as_ref()),
            input_name: Arc::clone(&item.input_name),
            epg_channel_id: item.epg_channel_id,
            explorer_meta,
        }
    }

    pub fn from_m3u(item: M3uPlaylistItem, explorer: bool) -> Self {
        let explorer_meta = explorer.then(|| explorer_meta_for(None, item.chno));
        Self {
            virtual_id: item.virtual_id.get(),
            provider_id: Arc::clone(&item.provider_id),
            name: Arc::clone(&item.name),
            title: Arc::clone(&item.title),
            group: Arc::clone(&item.group),
            logo: pick_logo(&item.logo, &item.logo_small, None),
            url: Arc::clone(&item.url),
            item_type: item.item_type,
            xtream_cluster: item.item_type.cluster(),
            category_id: 0,
            rating: 0.0,
            input_name: Arc::clone(&item.input_name),
            epg_channel_id: item.epg_channel_id,
            explorer_meta,
        }
    }

    pub fn from_playlist_item(item: &PlaylistItem, explorer: bool) -> Self {
        let header = &item.header;
        let explorer_meta = explorer.then(|| explorer_meta_for(header.additional_properties.as_ref(), header.chno));
        Self {
            virtual_id: header.virtual_id.get(),
            provider_id: Arc::clone(&header.id),
            name: Arc::clone(&header.name),
            title: Arc::clone(&header.title),
            group: Arc::clone(&header.group),
            logo: pick_logo(&header.logo, &header.logo_small, header.additional_properties.as_ref()),
            url: Arc::clone(&header.url),
            item_type: header.item_type,
            xtream_cluster: header.xtream_cluster,
            category_id: header.category_id,
            rating: get_rating(header.additional_properties.as_ref()),
            input_name: Arc::clone(&header.input_name),
            epg_channel_id: header.epg_channel_id.clone(),
            explorer_meta,
        }
    }
}

impl From<&CommonPlaylistItem> for UiPlaylistItem {
    fn from(item: &CommonPlaylistItem) -> Self { Self::from_common(item, false) }
}

impl From<XtreamPlaylistItem> for UiPlaylistItem {
    fn from(item: XtreamPlaylistItem) -> Self { Self::from_xtream(item, false) }
}

impl From<M3uPlaylistItem> for UiPlaylistItem {
    fn from(item: M3uPlaylistItem) -> Self { Self::from_m3u(item, false) }
}

impl From<&PlaylistItem> for UiPlaylistItem {
    fn from(item: &PlaylistItem) -> Self { Self::from_playlist_item(item, false) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::{bin_deserialize, bin_serialize};

    fn base_item() -> UiPlaylistItem {
        UiPlaylistItem {
            virtual_id: 1,
            provider_id: "p1".intern(),
            name: "n1".intern(),
            title: "t1".intern(),
            group: "g1".intern(),
            logo: "l1".intern(),
            url: "u1".intern(),
            item_type: PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            category_id: 10,
            rating: 5.0,
            input_name: "in1".intern(),
            epg_channel_id: None,
            explorer_meta: None,
        }
    }

    #[test]
    fn roundtrip_json_and_cbor_four_tail_cases() {
        let sample_meta = UiItemExplorerMeta {
            override_state: UiOverrideState { edited: true, pending: false, ambiguous: false },
            epg_state: Some(ExplorerEpgState::Ok),
            override_id: Some("prov1".into()),
            added: Some(1700000000),
            last_modified: Some(1700000100),
            chno: Some(42),
        };

        // Case 1: epg_channel_id None + explorer_meta None -> 12 elements
        let item1 = base_item();
        let json1 = serde_json::to_string(&item1).expect("serialize item1 to JSON");
        let val1: serde_json::Value = serde_json::from_str(&json1).expect("parse JSON value");
        assert_eq!(val1.as_array().expect("array").len(), 12);
        let back_json1: UiPlaylistItem = serde_json::from_str(&json1).expect("deserialize JSON");
        assert_eq!(back_json1, item1);
        let cbor1 = bin_serialize(&item1).expect("serialize item1 to CBOR");
        let back_cbor1: UiPlaylistItem = bin_deserialize(&cbor1).expect("deserialize CBOR");
        assert_eq!(back_cbor1, item1);

        // Case 2: epg_channel_id Some + explorer_meta None -> 13 elements
        let item2 = UiPlaylistItem { epg_channel_id: Some("epg1".intern()), ..base_item() };
        let json2 = serde_json::to_string(&item2).expect("serialize item2 to JSON");
        let val2: serde_json::Value = serde_json::from_str(&json2).expect("parse JSON value");
        assert_eq!(val2.as_array().expect("array").len(), 13);
        let back_json2: UiPlaylistItem = serde_json::from_str(&json2).expect("deserialize JSON");
        assert_eq!(back_json2, item2);
        let cbor2 = bin_serialize(&item2).expect("serialize item2 to CBOR");
        let back_cbor2: UiPlaylistItem = bin_deserialize(&cbor2).expect("deserialize CBOR");
        assert_eq!(back_cbor2, item2);

        // Case 3: epg_channel_id None + explorer_meta Some -> 14 elements (epg_channel_id is null on wire)
        let item3 = UiPlaylistItem { epg_channel_id: None, explorer_meta: Some(sample_meta.clone()), ..base_item() };
        let json3 = serde_json::to_string(&item3).expect("serialize item3 to JSON");
        let val3: serde_json::Value = serde_json::from_str(&json3).expect("parse JSON value");
        let arr3 = val3.as_array().expect("array");
        assert_eq!(arr3.len(), 14);
        assert!(arr3[12].is_null(), "element 12 must be null placeholder when epg_channel_id is None");
        assert!(arr3[13].is_object(), "element 13 must be explorer_meta object");
        let back_json3: UiPlaylistItem = serde_json::from_str(&json3).expect("deserialize JSON");
        assert_eq!(back_json3, item3);
        let cbor3 = bin_serialize(&item3).expect("serialize item3 to CBOR");
        let back_cbor3: UiPlaylistItem = bin_deserialize(&cbor3).expect("deserialize CBOR");
        assert_eq!(back_cbor3, item3);

        // Case 4: epg_channel_id Some + explorer_meta Some -> 14 elements
        let item4 =
            UiPlaylistItem { epg_channel_id: Some("epg1".intern()), explorer_meta: Some(sample_meta), ..base_item() };
        let json4 = serde_json::to_string(&item4).expect("serialize item4 to JSON");
        let val4: serde_json::Value = serde_json::from_str(&json4).expect("parse JSON value");
        let arr4 = val4.as_array().expect("array");
        assert_eq!(arr4.len(), 14);
        assert_eq!(arr4[12].as_str(), Some("epg1"));
        assert!(arr4[13].is_object());
        let back_json4: UiPlaylistItem = serde_json::from_str(&json4).expect("deserialize JSON");
        assert_eq!(back_json4, item4);
        let cbor4 = bin_serialize(&item4).expect("serialize item4 to CBOR");
        let back_cbor4: UiPlaylistItem = bin_deserialize(&cbor4).expect("deserialize CBOR");
        assert_eq!(back_cbor4, item4);
    }

    #[test]
    fn test_b3_extract_added_and_last_modified_and_constructor_meta() {
        use crate::model::{
            stream_properties::{
                LiveStreamProperties, SeriesStreamDetailEpisodeProperties, SeriesStreamDetailProperties,
                SeriesStreamProperties, VideoStreamProperties,
            },
            CommonPlaylistItem, VirtualId,
        };

        // Live: added parsed, last_modified None
        let live_props = StreamProperties::Live(Box::new(LiveStreamProperties {
            added: Some("1700000010".into()),
            ..Default::default()
        }));
        assert_eq!(extract_added_and_last_modified(Some(&live_props)), (Some(1700000010), None));

        // Live with unparsable added: ignored (None)
        let live_invalid = StreamProperties::Live(Box::new(LiveStreamProperties {
            added: Some("invalid_date".into()),
            ..Default::default()
        }));
        assert_eq!(extract_added_and_last_modified(Some(&live_invalid)), (None, None));

        // Video: added parsed, last_modified None
        let video_props = StreamProperties::Video(Box::new(VideoStreamProperties {
            added: "1700000020".into(),
            ..Default::default()
        }));
        assert_eq!(extract_added_and_last_modified(Some(&video_props)), (Some(1700000020), None));

        // Series: last_modified parsed, and added from first episode parsed
        let series_props = StreamProperties::Series(Box::new(SeriesStreamProperties {
            last_modified: Some("1700000030".into()),
            details: Some(SeriesStreamDetailProperties {
                year: None,
                seasons: None,
                episodes: Some(vec![SeriesStreamDetailEpisodeProperties {
                    added: "1700000025".into(),
                    ..Default::default()
                }]),
            }),
            ..Default::default()
        }));
        assert_eq!(extract_added_and_last_modified(Some(&series_props)), (Some(1700000025), Some(1700000030)));

        // CommonPlaylistItem constructor:
        // explorer=false -> explorer_meta is None
        let common = CommonPlaylistItem {
            virtual_id: VirtualId::new(1),
            provider_id: "p1".into(),
            name: "Item 1".into(),
            chno: 1,
            logo: "".into(),
            logo_small: "".into(),
            group: "Group".into(),
            title: "Title".into(),
            parent_code: "".into(),
            audio_track: "".into(),
            time_shift: "".into(),
            rec: "".into(),
            url: "http://example.com".into(),
            item_type: PlaylistItemType::Video,
            xtream_cluster: Some(XtreamCluster::Video),
            category_id: Some(1),
            input_name: "test".into(),
            epg_channel_id: None,
            additional_properties: Some(video_props),
        };
        let item_stream = UiPlaylistItem::from_common(&common, false);
        assert!(item_stream.explorer_meta.is_none());

        // explorer=true -> explorer_meta is Some with extracted timestamps
        let item_explorer = UiPlaylistItem::from_common(&common, true);
        let meta = item_explorer.explorer_meta.expect("explorer_meta must be Some");
        assert_eq!(meta.added, Some(1700000020));
        assert_eq!(meta.last_modified, None);
    }
}
