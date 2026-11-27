use crate::{
    model::{
        CommonPlaylistItem, ImdbRatingSource, M3uPlaylistItem, PlaylistItem, PlaylistItemType, StreamProperties,
        XtreamCluster, XtreamPlaylistItem,
    },
    utils::{is_valid_provider_rating, Internable},
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
    /// Internal source id for Xtream items (e.g. `input_stream_id` for M3U origin) used to compute the override key.
    #[serde(skip)]
    pub override_source_id: Option<Arc<str>>,
    /// Internal source URL captured before any target transformation used to compute the override key.
    #[serde(skip)]
    pub override_source_url: Option<Arc<str>>,
}

/// Origin of the effective rating displayed in the UI explorer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiRatingOrigin {
    Imdb,
    Provider,
}

/// Lightweight playlist item for UI streaming.
#[derive(Debug, Clone, PartialEq, Default)]
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
    /// Stable provider/origin id of the stream; empty when the item does not carry one.
    pub input_stream_id: Arc<str>,
    pub epg_channel_id: Option<Arc<str>>,
    pub explorer_meta: Option<UiItemExplorerMeta>,
    pub effective_rating: Option<f32>,
    pub imdb_rating: Option<f32>,
    pub imdb_votes: Option<u32>,
    pub rating_origin: Option<UiRatingOrigin>,
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
        let has_ratings = self.effective_rating.is_some()
            || self.imdb_rating.is_some()
            || self.imdb_votes.is_some()
            || self.rating_origin.is_some();
        let len = if has_ratings {
            19
        } else if self.explorer_meta.is_some() {
            15
        } else if self.epg_channel_id.is_some() {
            14
        } else {
            13
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
        s.serialize_field(self.input_stream_id.as_ref())?;
        if len >= 14 {
            match &self.epg_channel_id {
                Some(id) => s.serialize_field(id.as_ref())?,
                None => s.serialize_field(&Option::<&str>::None)?,
            }
        }
        if len >= 15 {
            match &self.explorer_meta {
                Some(meta) => s.serialize_field(meta)?,
                None => s.serialize_field(&Option::<&UiItemExplorerMeta>::None)?,
            }
        }
        if len == 19 {
            s.serialize_field(&self.effective_rating)?;
            s.serialize_field(&self.imdb_rating)?;
            s.serialize_field(&self.imdb_votes)?;
            s.serialize_field(&self.rating_origin)?;
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
                formatter.write_str("a tuple struct UiPlaylistItem with 13 to 19 elements")
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
                let input_stream_id =
                    seq.next_element::<ArcStrWrap>()?.ok_or_else(|| serde::de::Error::invalid_length(12, &self))?.0;

                let epg_channel_id = seq.next_element::<OptionArcStrWrap>()?.and_then(|w| w.0);
                let explorer_meta = seq.next_element::<Option<UiItemExplorerMeta>>()?.flatten();
                let effective_rating = seq.next_element::<Option<f32>>()?.flatten();
                let imdb_rating = seq.next_element::<Option<f32>>()?.flatten();
                let imdb_votes = seq.next_element::<Option<u32>>()?.flatten();
                let rating_origin = seq.next_element::<Option<UiRatingOrigin>>()?.flatten();

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
                    input_stream_id,
                    epg_channel_id,
                    explorer_meta,
                    effective_rating,
                    imdb_rating,
                    imdb_votes,
                    rating_origin,
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
/// Provider rating of the stream properties; `0.0` when absent or for live and episode items.
#[inline]
pub fn provider_rating(props: Option<&StreamProperties>) -> f64 {
    if let Some(p) = props {
        return match p {
            StreamProperties::Video(v) => v.rating.unwrap_or_default(),
            StreamProperties::Series(s) => s.rating,
            StreamProperties::Live(_) | StreamProperties::Episode(_) => 0.0,
        };
    }
    0.0
}

impl UiPlaylistItem {
    /// Applies a provider-rating override to an item the explorer already shows.
    ///
    /// Semantics:
    /// - `Provider`: provider value is effective, origin `Provider` (also when it equals the `IMDb` value);
    /// - `ImdbThenProvider`: keep an `Imdb` origin, otherwise provider value is effective with origin `Provider`;
    /// - `Imdb`: store `rating` only.
    pub fn set_provider_rating(&mut self, rating: Option<f64>, source: ImdbRatingSource) {
        // A value outside the 0-10 scale is ignored and keeps the stored rating; `None` clears it.
        if rating.is_some_and(|value| !is_valid_provider_rating(value)) {
            return;
        }
        self.rating = rating.unwrap_or(0.0);
        let keeps_effective = match source {
            ImdbRatingSource::Imdb => true,
            ImdbRatingSource::ImdbThenProvider => self.rating_origin == Some(UiRatingOrigin::Imdb),
            ImdbRatingSource::Provider => false,
        };
        if keeps_effective {
            return;
        }
        #[allow(clippy::cast_possible_truncation)]
        let effective = rating.map(|value| value as f32);
        self.effective_rating = effective;
        self.rating_origin = effective.map(|_| UiRatingOrigin::Provider);
    }
}

/// Fallback origin without a rating policy: `Imdb` when `effective_rating` equals `imdb_rating`.
#[inline]
pub fn compute_rating_origin(effective_rating: Option<f32>, imdb_rating: Option<f32>) -> Option<UiRatingOrigin> {
    effective_rating.map(|eff| if imdb_rating == Some(eff) { UiRatingOrigin::Imdb } else { UiRatingOrigin::Provider })
}

/// Origin from the configured rating policy, the same rule `apply_imdb_ratings` uses at update time.
/// `None` source means `IMDb` is disabled.
#[inline]
pub fn compute_policy_rating_origin(
    effective_rating: Option<f32>,
    imdb_rating: Option<f32>,
    imdb_votes: Option<u32>,
    source: Option<ImdbRatingSource>,
    min_votes: u32,
) -> Option<UiRatingOrigin> {
    effective_rating?;
    let imdb_selected = match source {
        None | Some(ImdbRatingSource::Provider) => false,
        Some(ImdbRatingSource::Imdb | ImdbRatingSource::ImdbThenProvider) => {
            imdb_rating.is_some() && imdb_votes.is_some_and(|votes| votes >= min_votes)
        }
    };
    Some(if imdb_selected { UiRatingOrigin::Imdb } else { UiRatingOrigin::Provider })
}

#[inline]
fn extract_ratings(
    props: Option<&StreamProperties>,
) -> (Option<f32>, Option<f32>, Option<u32>, Option<UiRatingOrigin>) {
    let (effective_rating, imdb_rating, imdb_votes) = match props {
        Some(StreamProperties::Video(v)) => (v.effective_rating, v.imdb_rating, v.imdb_votes),
        Some(StreamProperties::Series(s)) => (s.effective_rating, s.imdb_rating, s.imdb_votes),
        _ => (None, None, None),
    };
    let rating_origin = compute_rating_origin(effective_rating, imdb_rating);
    (effective_rating, imdb_rating, imdb_votes, rating_origin)
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
        let (effective_rating, imdb_rating, imdb_votes, rating_origin) =
            extract_ratings(item.additional_properties.as_ref());
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
            rating: provider_rating(item.additional_properties.as_ref()),
            input_name: Arc::clone(&item.input_name),
            input_stream_id: "".intern(),
            epg_channel_id: item.epg_channel_id.clone(),
            explorer_meta,
            effective_rating,
            imdb_rating,
            imdb_votes,
            rating_origin,
        }
    }

    pub fn from_xtream(item: XtreamPlaylistItem, explorer: bool) -> Self {
        let explorer_meta = explorer.then(|| {
            let mut meta = explorer_meta_for(item.additional_properties.as_ref(), item.channel_no);
            meta.override_source_id = Some(item.override_source_id());
            meta.override_source_url = (!item.input_url.is_empty()).then(|| Arc::clone(&item.input_url));
            meta
        });
        let (effective_rating, imdb_rating, imdb_votes, rating_origin) =
            extract_ratings(item.additional_properties.as_ref());
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
            rating: provider_rating(item.additional_properties.as_ref()),
            input_name: Arc::clone(&item.input_name),
            input_stream_id: Arc::clone(&item.input_stream_id),
            epg_channel_id: item.epg_channel_id,
            explorer_meta,
            effective_rating,
            imdb_rating,
            imdb_votes,
            rating_origin,
        }
    }

    pub fn from_m3u(item: M3uPlaylistItem, explorer: bool) -> Self {
        let explorer_meta = explorer.then(|| {
            let mut meta = explorer_meta_for(None, item.chno);
            meta.override_source_id = Some(item.override_source_id());
            meta.override_source_url = (!item.input_url.is_empty()).then(|| Arc::clone(&item.input_url));
            meta
        });
        let (effective_rating, imdb_rating, imdb_votes, rating_origin) =
            extract_ratings(item.additional_properties.as_ref());
        let rating = provider_rating(item.additional_properties.as_ref());
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
            rating,
            input_name: Arc::clone(&item.input_name),
            input_stream_id: Arc::clone(&item.input_stream_id),
            epg_channel_id: item.epg_channel_id,
            explorer_meta,
            effective_rating,
            imdb_rating,
            imdb_votes,
            rating_origin,
        }
    }

    pub fn from_playlist_item(item: &PlaylistItem, explorer: bool) -> Self {
        let header = &item.header;
        let explorer_meta = explorer.then(|| explorer_meta_for(header.additional_properties.as_ref(), header.chno));
        let (effective_rating, imdb_rating, imdb_votes, rating_origin) =
            extract_ratings(header.additional_properties.as_ref());
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
            rating: provider_rating(header.additional_properties.as_ref()),
            input_name: Arc::clone(&header.input_name),
            input_stream_id: Arc::clone(&header.input_stream_id),
            epg_channel_id: header.epg_channel_id.clone(),
            explorer_meta,
            effective_rating,
            imdb_rating,
            imdb_votes,
            rating_origin,
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
            input_stream_id: "".intern(),
            epg_channel_id: None,
            explorer_meta: None,
            effective_rating: None,
            imdb_rating: None,
            imdb_votes: None,
            rating_origin: None,
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
            ..Default::default()
        };

        // Case 1: epg_channel_id None + explorer_meta None -> 13 elements
        let item1 = base_item();
        let json1 = serde_json::to_string(&item1).expect("serialize item1 to JSON");
        let val1: serde_json::Value = serde_json::from_str(&json1).expect("parse JSON value");
        assert_eq!(val1.as_array().expect("array").len(), 13);
        let back_json1: UiPlaylistItem = serde_json::from_str(&json1).expect("deserialize JSON");
        assert_eq!(back_json1, item1);
        let cbor1 = bin_serialize(&item1).expect("serialize item1 to CBOR");
        let back_cbor1: UiPlaylistItem = bin_deserialize(&cbor1).expect("deserialize CBOR");
        assert_eq!(back_cbor1, item1);

        // Case 2: epg_channel_id Some + explorer_meta None -> 14 elements
        let item2 = UiPlaylistItem { epg_channel_id: Some("epg1".intern()), ..base_item() };
        let json2 = serde_json::to_string(&item2).expect("serialize item2 to JSON");
        let val2: serde_json::Value = serde_json::from_str(&json2).expect("parse JSON value");
        assert_eq!(val2.as_array().expect("array").len(), 14);
        let back_json2: UiPlaylistItem = serde_json::from_str(&json2).expect("deserialize JSON");
        assert_eq!(back_json2, item2);
        let cbor2 = bin_serialize(&item2).expect("serialize item2 to CBOR");
        let back_cbor2: UiPlaylistItem = bin_deserialize(&cbor2).expect("deserialize CBOR");
        assert_eq!(back_cbor2, item2);

        // Case 3: epg_channel_id None + explorer_meta Some -> 15 elements (epg_channel_id is null on wire)
        let item3 = UiPlaylistItem { epg_channel_id: None, explorer_meta: Some(sample_meta.clone()), ..base_item() };
        let json3 = serde_json::to_string(&item3).expect("serialize item3 to JSON");
        let val3: serde_json::Value = serde_json::from_str(&json3).expect("parse JSON value");
        let arr3 = val3.as_array().expect("array");
        assert_eq!(arr3.len(), 15);
        assert!(arr3[13].is_null(), "element 13 must be null placeholder when epg_channel_id is None");
        assert!(arr3[14].is_object(), "element 14 must be explorer_meta object");
        let back_json3: UiPlaylistItem = serde_json::from_str(&json3).expect("deserialize JSON");
        assert_eq!(back_json3, item3);
        let cbor3 = bin_serialize(&item3).expect("serialize item3 to CBOR");
        let back_cbor3: UiPlaylistItem = bin_deserialize(&cbor3).expect("deserialize CBOR");
        assert_eq!(back_cbor3, item3);

        // Case 4: epg_channel_id Some + explorer_meta Some -> 15 elements
        let item4 =
            UiPlaylistItem { epg_channel_id: Some("epg1".intern()), explorer_meta: Some(sample_meta), ..base_item() };
        let json4 = serde_json::to_string(&item4).expect("serialize item4 to JSON");
        let val4: serde_json::Value = serde_json::from_str(&json4).expect("parse JSON value");
        let arr4 = val4.as_array().expect("array");
        assert_eq!(arr4.len(), 15);
        assert_eq!(arr4[13].as_str(), Some("epg1"));
        assert!(arr4[14].is_object());
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

    #[test]
    fn serialization_of_ui_item_explorer_meta_skips_override_source_id_and_url() {
        let meta = UiItemExplorerMeta {
            override_source_id: Some(Arc::from("hash123")),
            override_source_url: Some(Arc::from("http://example.test/source.m3u8")),
            ..Default::default()
        };
        let json = serde_json::to_string(&meta).expect("serialize meta");
        assert!(!json.contains("override_source_id"), "wire JSON must not contain override_source_id: {json}");
        assert!(!json.contains("override_source_url"), "wire JSON must not contain override_source_url: {json}");
        let deserialized: UiItemExplorerMeta = serde_json::from_str(&json).expect("deserialize meta");
        assert_eq!(deserialized.override_source_id, None);
        assert_eq!(deserialized.override_source_url, None);
    }

    #[test]
    fn conversion_sets_ratings_and_origin_all_four_cases() {
        use crate::model::{
            stream_properties::VideoStreamProperties, CommonPlaylistItem, PlaylistItem, PlaylistItemHeader, VirtualId,
            XtreamPlaylistItem,
        };

        // 1. IMDb effective: effective_rating == imdb_rating -> Imdb
        let video_imdb = StreamProperties::Video(Box::new(VideoStreamProperties {
            rating: Some(6.8),
            imdb: Some(12345),
            imdb_rating: Some(4.2),
            imdb_votes: Some(12345),
            effective_rating: Some(4.2),
            ..Default::default()
        }));
        let common_imdb = CommonPlaylistItem {
            virtual_id: VirtualId::new(1),
            provider_id: "p1".into(),
            name: "Movie".into(),
            chno: 1,
            logo: "".into(),
            logo_small: "".into(),
            group: "Movies".into(),
            title: "Movie".into(),
            parent_code: "".into(),
            audio_track: "".into(),
            time_shift: "".into(),
            rec: "".into(),
            url: "http://example.com/1.mp4".into(),
            item_type: PlaylistItemType::Video,
            xtream_cluster: Some(XtreamCluster::Video),
            category_id: Some(1),
            input_name: "test".into(),
            epg_channel_id: None,
            additional_properties: Some(video_imdb.clone()),
        };
        let item_imdb = UiPlaylistItem::from_common(&common_imdb, false);
        assert_eq!(item_imdb.rating, 6.8);
        assert_eq!(item_imdb.effective_rating, Some(4.2));
        assert_eq!(item_imdb.imdb_rating, Some(4.2));
        assert_eq!(item_imdb.imdb_votes, Some(12345));
        assert_eq!(item_imdb.rating_origin, Some(UiRatingOrigin::Imdb));

        // 2. Provider effective: effective_rating != imdb_rating -> Provider
        let video_provider = StreamProperties::Video(Box::new(VideoStreamProperties {
            rating: Some(6.8),
            imdb: Some(12345),
            imdb_rating: Some(4.2),
            imdb_votes: Some(10),
            effective_rating: Some(6.8),
            ..Default::default()
        }));
        let xtream_provider = XtreamPlaylistItem::from(&PlaylistItem {
            header: PlaylistItemHeader {
                virtual_id: VirtualId::new(2),
                id: "2".into(),
                name: "Movie 2".into(),
                title: "Movie 2".into(),
                group: "Movies".into(),
                url: "http://example.com/2.mp4".into(),
                item_type: PlaylistItemType::Video,
                xtream_cluster: XtreamCluster::Video,
                category_id: 1,
                chno: 2,
                input_name: "test".into(),
                input_stream_id: "2".into(),
                additional_properties: Some(video_provider),
                ..Default::default()
            },
        });
        let item_provider = UiPlaylistItem::from_xtream(xtream_provider, false);
        assert_eq!(item_provider.rating, 6.8);
        assert_eq!(item_provider.effective_rating, Some(6.8));
        assert_eq!(item_provider.imdb_rating, Some(4.2));
        assert_eq!(item_provider.imdb_votes, Some(10));
        assert_eq!(item_provider.rating_origin, Some(UiRatingOrigin::Provider));

        // 3. No effective rating
        let video_none = StreamProperties::Video(Box::new(VideoStreamProperties {
            rating: None,
            imdb: None,
            imdb_rating: None,
            imdb_votes: None,
            effective_rating: None,
            ..Default::default()
        }));
        let playlist_item_none = PlaylistItem {
            header: PlaylistItemHeader {
                virtual_id: VirtualId::new(3),
                id: "p3".into(),
                name: "Unrated".into(),
                title: "Unrated".into(),
                group: "Movies".into(),
                logo: "".into(),
                logo_small: "".into(),
                url: "http://example.com/3.mp4".into(),
                item_type: PlaylistItemType::Video,
                xtream_cluster: XtreamCluster::Video,
                category_id: 1,
                chno: 3,
                input_name: "test".into(),
                input_stream_id: "".into(),
                epg_channel_id: None,
                additional_properties: Some(video_none),
                ..Default::default()
            },
        };
        let item_none = UiPlaylistItem::from_playlist_item(&playlist_item_none, false);
        assert_eq!(item_none.rating, 0.0);
        assert_eq!(item_none.effective_rating, None);
        assert_eq!(item_none.imdb_rating, None);
        assert_eq!(item_none.imdb_votes, None);
        assert_eq!(item_none.rating_origin, None);

        // 4. Live item (all None)
        let live_props = StreamProperties::Live(Box::default());
        let common_live = CommonPlaylistItem {
            virtual_id: VirtualId::new(4),
            provider_id: "live4".into(),
            name: "Live Channel".into(),
            chno: 4,
            logo: "".into(),
            logo_small: "".into(),
            group: "Live".into(),
            title: "Live Channel".into(),
            parent_code: "".into(),
            audio_track: "".into(),
            time_shift: "".into(),
            rec: "".into(),
            url: "http://example.com/live.m3u8".into(),
            item_type: PlaylistItemType::Live,
            xtream_cluster: Some(XtreamCluster::Live),
            category_id: Some(1),
            input_name: "test".into(),
            epg_channel_id: None,
            additional_properties: Some(live_props),
        };
        let item_live = UiPlaylistItem::from_common(&common_live, false);
        assert_eq!(item_live.rating, 0.0);
        assert_eq!(item_live.effective_rating, None);
        assert_eq!(item_live.imdb_rating, None);
        assert_eq!(item_live.imdb_votes, None);
        assert_eq!(item_live.rating_origin, None);

        // 5. M3U item (always None)
        let m3u = M3uPlaylistItem::from(&PlaylistItem {
            header: PlaylistItemHeader {
                virtual_id: VirtualId::new(5),
                id: "m3u5".into(),
                name: "M3U Item".into(),
                title: "M3U Item".into(),
                group: "M3U".into(),
                url: "http://example.com/m3u.m3u8".into(),
                item_type: PlaylistItemType::Live,
                chno: 5,
                input_name: "test".into(),
                ..Default::default()
            },
        });
        let item_m3u = UiPlaylistItem::from_m3u(m3u, false);
        assert_eq!(item_m3u.rating, 0.0);
        assert_eq!(item_m3u.effective_rating, None);
        assert_eq!(item_m3u.imdb_rating, None);
        assert_eq!(item_m3u.imdb_votes, None);
        assert_eq!(item_m3u.rating_origin, None);

        // 6. M3U item with Video properties (preserves ratings)
        let m3u_video = M3uPlaylistItem::from(&PlaylistItem {
            header: PlaylistItemHeader {
                virtual_id: VirtualId::new(6),
                id: "m3u6".into(),
                name: "M3U Movie".into(),
                title: "M3U Movie".into(),
                group: "Movies".into(),
                url: "http://example.com/movie.mp4".into(),
                item_type: PlaylistItemType::Video,
                chno: 6,
                input_name: "test".into(),
                additional_properties: Some(video_imdb),
                ..Default::default()
            },
        });
        let item_m3u_video = UiPlaylistItem::from_m3u(m3u_video, false);
        assert_eq!(item_m3u_video.effective_rating, Some(4.2));
        assert_eq!(item_m3u_video.imdb_rating, Some(4.2));
        assert_eq!(item_m3u_video.imdb_votes, Some(12345));
        assert_eq!(item_m3u_video.rating_origin, Some(UiRatingOrigin::Imdb));
    }

    #[test]
    fn roundtrip_json_and_cbor_with_ratings() {
        let item = UiPlaylistItem {
            effective_rating: Some(4.2),
            imdb_rating: Some(4.2),
            imdb_votes: Some(12345),
            rating_origin: Some(UiRatingOrigin::Imdb),
            ..base_item()
        };

        // JSON roundtrip
        let json = serde_json::to_string(&item).expect("serialize item with ratings to JSON");
        let val: serde_json::Value = serde_json::from_str(&json).expect("parse JSON value");
        let arr = val.as_array().expect("array");
        assert_eq!(arr.len(), 19);
        assert_eq!(arr[15].as_f64(), Some(4.2));
        assert_eq!(arr[16].as_f64(), Some(4.2));
        assert_eq!(arr[17].as_u64(), Some(12345));
        assert_eq!(arr[18].as_str(), Some("imdb"));
        let back_json: UiPlaylistItem = serde_json::from_str(&json).expect("deserialize JSON");
        assert_eq!(back_json, item);

        // CBOR roundtrip
        let cbor = bin_serialize(&item).expect("serialize item with ratings to CBOR");
        let back_cbor: UiPlaylistItem = bin_deserialize(&cbor).expect("deserialize CBOR");
        assert_eq!(back_cbor, item);

        // Provider rating case
        let item_prov = UiPlaylistItem {
            effective_rating: Some(6.8),
            imdb_rating: None,
            imdb_votes: None,
            rating_origin: Some(UiRatingOrigin::Provider),
            ..base_item()
        };
        let json_prov = serde_json::to_string(&item_prov).expect("serialize item_prov to JSON");
        let val_prov: serde_json::Value = serde_json::from_str(&json_prov).expect("parse JSON value");
        let arr_prov = val_prov.as_array().expect("array");
        assert_eq!(arr_prov.len(), 19);
        assert_eq!(arr_prov[18].as_str(), Some("provider"));
        let back_json_prov: UiPlaylistItem = serde_json::from_str(&json_prov).expect("deserialize JSON");
        assert_eq!(back_json_prov, item_prov);
        let cbor_prov = bin_serialize(&item_prov).expect("serialize item_prov to CBOR");
        let back_cbor_prov: UiPlaylistItem = bin_deserialize(&cbor_prov).expect("deserialize CBOR");
        assert_eq!(back_cbor_prov, item_prov);
    }

    #[test]
    fn set_provider_rating_semantics_across_sources() {
        // Provider source: overwrites effective even when origin was Imdb
        let mut item = UiPlaylistItem {
            effective_rating: Some(8.0),
            imdb_rating: Some(8.0),
            rating_origin: Some(UiRatingOrigin::Imdb),
            ..base_item()
        };
        item.set_provider_rating(Some(8.0), ImdbRatingSource::Provider);
        assert_eq!(item.rating, 8.0);
        assert_eq!(item.effective_rating, Some(8.0));
        assert_eq!(item.rating_origin, Some(UiRatingOrigin::Provider));

        // ImdbThenProvider source: preserves Imdb origin
        let mut item_then = UiPlaylistItem {
            effective_rating: Some(8.0),
            imdb_rating: Some(8.0),
            rating_origin: Some(UiRatingOrigin::Imdb),
            ..base_item()
        };
        item_then.set_provider_rating(Some(9.0), ImdbRatingSource::ImdbThenProvider);
        assert_eq!(item_then.rating, 9.0);
        assert_eq!(item_then.effective_rating, Some(8.0));
        assert_eq!(item_then.rating_origin, Some(UiRatingOrigin::Imdb));

        // ImdbThenProvider source: updates when origin is not Imdb
        let mut item_prov = UiPlaylistItem {
            effective_rating: Some(6.0),
            rating_origin: Some(UiRatingOrigin::Provider),
            ..base_item()
        };
        item_prov.set_provider_rating(Some(9.0), ImdbRatingSource::ImdbThenProvider);
        assert_eq!(item_prov.rating, 9.0);
        assert_eq!(item_prov.effective_rating, Some(9.0));
        assert_eq!(item_prov.rating_origin, Some(UiRatingOrigin::Provider));

        // Imdb source: stores rating only, effective and origin stay unchanged
        let mut item_imdb_strict = UiPlaylistItem { effective_rating: None, rating_origin: None, ..base_item() };
        item_imdb_strict.set_provider_rating(Some(7.5), ImdbRatingSource::Imdb);
        assert_eq!(item_imdb_strict.rating, 7.5);
        assert_eq!(item_imdb_strict.effective_rating, None);
        assert_eq!(item_imdb_strict.rating_origin, None);

        // Clearing rating with 0.0 or None or NaN
        item_prov.set_provider_rating(None, ImdbRatingSource::Provider);
        assert_eq!(item_prov.rating, 0.0);
        assert_eq!(item_prov.effective_rating, None);
        assert_eq!(item_prov.rating_origin, None);
    }

    #[test]
    fn compute_policy_rating_origin_cases() {
        // When effective is None, origin is always None
        assert_eq!(compute_policy_rating_origin(None, Some(8.0), Some(500), None, 100), None);

        // Disabled or None config -> Provider when effective is Some
        assert_eq!(
            compute_policy_rating_origin(Some(7.0), Some(7.0), Some(500), None, 100),
            Some(UiRatingOrigin::Provider)
        );

        // Provider source -> Provider even if equals imdb
        assert_eq!(
            compute_policy_rating_origin(Some(7.0), Some(7.0), Some(500), Some(ImdbRatingSource::Provider), 100),
            Some(UiRatingOrigin::Provider)
        );

        // Imdb source -> Imdb when a qualifying IMDb rating exists
        assert_eq!(
            compute_policy_rating_origin(Some(7.0), Some(7.0), Some(100), Some(ImdbRatingSource::Imdb), 100),
            Some(UiRatingOrigin::Imdb)
        );
        // Imdb source with stale provider data from an older config -> Provider
        assert_eq!(
            compute_policy_rating_origin(Some(7.0), None, None, Some(ImdbRatingSource::Imdb), 100),
            Some(UiRatingOrigin::Provider)
        );

        // ImdbThenProvider -> Imdb iff imdb_rating is Some and imdb_votes >= min_votes
        assert_eq!(
            compute_policy_rating_origin(
                Some(7.0),
                Some(7.0),
                Some(100),
                Some(ImdbRatingSource::ImdbThenProvider),
                100
            ),
            Some(UiRatingOrigin::Imdb)
        );
        assert_eq!(
            compute_policy_rating_origin(Some(7.0), Some(7.0), Some(0), Some(ImdbRatingSource::ImdbThenProvider), 0),
            Some(UiRatingOrigin::Imdb)
        );
        assert_eq!(
            compute_policy_rating_origin(Some(7.0), Some(7.0), Some(99), Some(ImdbRatingSource::ImdbThenProvider), 100),
            Some(UiRatingOrigin::Provider)
        );
        assert_eq!(
            compute_policy_rating_origin(Some(7.0), None, None, Some(ImdbRatingSource::ImdbThenProvider), 100),
            Some(UiRatingOrigin::Provider)
        );
    }

    #[test]
    fn set_provider_rating_outside_zero_to_ten_is_ignored() {
        let mut item = base_item();
        item.set_provider_rating(Some(7.0), ImdbRatingSource::Provider);
        item.set_provider_rating(Some(10.5), ImdbRatingSource::Provider);
        assert_eq!(item.rating, 7.0);
        assert_eq!(item.effective_rating, Some(7.0));

        item.set_provider_rating(Some(10.0), ImdbRatingSource::Provider);
        assert_eq!(item.effective_rating, Some(10.0));
    }
}
