use crate::{
    model::{
        CommonPlaylistItem, M3uPlaylistItem, PlaylistItem, PlaylistItemType, StreamProperties, XtreamCluster,
        XtreamPlaylistItem,
    },
    utils::{arc_str_serde, Internable},
};
use serde_tuple::{Deserialize_tuple, Serialize_tuple};
use std::sync::Arc;

/// Lightweight playlist item for UI streaming.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple, PartialEq)]
pub struct UiPlaylistItem {
    #[serde(rename = "v")]
    pub virtual_id: u32,
    #[serde(rename = "p", with = "arc_str_serde")]
    pub provider_id: Arc<str>,
    #[serde(rename = "n", with = "arc_str_serde")]
    pub name: Arc<str>,
    #[serde(rename = "t", with = "arc_str_serde")]
    pub title: Arc<str>,
    #[serde(rename = "g", with = "arc_str_serde")]
    pub group: Arc<str>,
    #[serde(rename = "l", with = "arc_str_serde")]
    pub logo: Arc<str>,
    #[serde(rename = "u", with = "arc_str_serde")]
    pub url: Arc<str>,
    #[serde(rename = "t")]
    pub item_type: PlaylistItemType,
    #[serde(rename = "x")]
    pub xtream_cluster: XtreamCluster,
    #[serde(rename = "c")]
    pub category_id: u32,
    #[serde(rename = "s")]
    pub rating: f64,
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

impl From<&CommonPlaylistItem> for UiPlaylistItem {
    fn from(item: &CommonPlaylistItem) -> Self {
        Self {
            virtual_id: item.virtual_id,
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
        }
    }
}

impl From<XtreamPlaylistItem> for UiPlaylistItem {
    fn from(item: XtreamPlaylistItem) -> Self {
        Self {
            virtual_id: item.virtual_id,
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
        }
    }
}

impl From<M3uPlaylistItem> for UiPlaylistItem {
    fn from(item: M3uPlaylistItem) -> Self {
        Self {
            virtual_id: item.virtual_id,
            provider_id: Arc::clone(&item.provider_id),
            name: Arc::clone(&item.name),
            title: Arc::clone(&item.title),
            group: Arc::clone(&item.group),
            logo: pick_logo(&item.logo, &item.logo_small, None),
            url: Arc::clone(&item.url),
            item_type: item.item_type,
            xtream_cluster: XtreamCluster::try_from(item.item_type).unwrap_or_default(),
            category_id: 0,
            rating: 0.0,
        }
    }
}

impl From<&PlaylistItem> for UiPlaylistItem {
    fn from(item: &PlaylistItem) -> Self {
        let header = &item.header;
        Self {
            virtual_id: header.virtual_id,
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
        }
    }
}
