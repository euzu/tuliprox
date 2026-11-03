use crate::{
    model::{PlaylistItemType, SearchRequest, StreamProperties, UiPlaylistItem, VirtualId, XtreamCluster},
    utils::{arc_str_option_serde, arc_str_serde},
};
use serde::{Deserialize, Serialize};
use std::{rc::Rc, sync::Arc};

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct PlaylistRequestXtream {
    pub username: String,
    pub password: String,
    pub url: String,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct PlaylistRequestM3u {
    pub url: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct PlaylistContentQuery {
    #[serde(default)]
    pub explorer: bool,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub enum PlaylistRequest {
    Target(u16),
    Input(String),
    CustomXtream(PlaylistRequestXtream),
    CustomM3u(PlaylistRequestM3u),
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub enum PlaylistUrlResolveRequest {
    Webplayer { target_id: u16, virtual_id: u32, cluster: XtreamCluster },
    Provider { playlist_request: PlaylistRequest, url: String },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct CommonPlaylistItem {
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
    #[serde(with = "arc_str_serde")]
    pub input_name: Arc<str>,
    pub item_type: PlaylistItemType,
    #[serde(default, with = "arc_str_option_serde")]
    pub epg_channel_id: Option<Arc<str>>,
    #[serde(default)]
    pub xtream_cluster: Option<XtreamCluster>,
    #[serde(default)]
    pub additional_properties: Option<StreamProperties>,
    #[serde(default)]
    pub category_id: Option<u32>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct UiPlaylistGroup {
    pub id: u32,
    #[serde(with = "arc_str_serde")]
    pub title: Arc<str>,
    pub channels: Vec<Rc<UiPlaylistItem>>,
    pub xtream_cluster: XtreamCluster,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct UiPlaylistCategories {
    #[serde(default)]
    pub live: Option<Vec<Rc<UiPlaylistGroup>>>,
    #[serde(default)]
    pub vod: Option<Vec<Rc<UiPlaylistGroup>>>,
    #[serde(default)]
    pub series: Option<Vec<Rc<UiPlaylistGroup>>>,
}

pub const SEARCH_FIELD_GROUP: &str = "group";
pub const SEARCH_FIELD_TITLE: &str = "title";
pub const SEARCH_FIELD_NAME: &str = "name";
pub const SEARCH_FIELD_URL: &str = "url";
pub const SEARCH_FIELD_PROVIDER_ID: &str = "provider_id";

#[derive(Debug, Clone, Copy)]
struct SearchFieldMask {
    group: bool,
    title: bool,
    name: bool,
    url: bool,
    provider_id: bool,
}

impl SearchFieldMask {
    // Legacy scope used when no fields are selected: group title + channel title/name.
    const DEFAULT: Self = Self { group: true, title: true, name: true, url: false, provider_id: false };

    fn from_search_fields(fields: Option<&Vec<String>>) -> Self {
        let Some(fields) = fields.filter(|f| !f.is_empty()) else {
            return Self::DEFAULT;
        };
        let mut mask = Self { group: false, title: false, name: false, url: false, provider_id: false };
        for field in fields {
            match field.as_str() {
                SEARCH_FIELD_GROUP => mask.group = true,
                SEARCH_FIELD_TITLE => mask.title = true,
                SEARCH_FIELD_NAME => mask.name = true,
                SEARCH_FIELD_URL => mask.url = true,
                SEARCH_FIELD_PROVIDER_ID => mask.provider_id = true,
                _ => {}
            }
        }
        if mask.group || mask.title || mask.name || mask.url || mask.provider_id {
            mask
        } else {
            Self::DEFAULT
        }
    }
}

fn filter_channels(
    groups: Option<&Vec<Rc<UiPlaylistGroup>>>,
    mask: SearchFieldMask,
    matches: &dyn Fn(&str) -> bool,
    matches_provider_id: &dyn Fn(&str) -> bool,
) -> Option<Vec<Rc<UiPlaylistGroup>>> {
    groups.map(|gs| {
        gs.iter()
            .filter_map(|group| {
                if mask.group && matches(&group.title) {
                    return Some(Rc::clone(group));
                }

                let filtered_channels: Vec<Rc<UiPlaylistItem>> = group
                    .channels
                    .iter()
                    .filter(|c| {
                        (mask.title && matches(&c.title))
                            || (mask.name && matches(&c.name))
                            || (mask.url && matches(&c.url))
                            || (mask.provider_id
                                && (matches_provider_id(&c.provider_id)
                                    || c.explorer_meta
                                        .as_ref()
                                        .and_then(|m| m.override_id.as_deref())
                                        .is_some_and(matches_provider_id)))
                    })
                    .cloned()
                    .collect();

                if filtered_channels.is_empty() {
                    None
                } else {
                    Some(Rc::new(UiPlaylistGroup {
                        id: group.id,
                        title: group.title.clone(),
                        channels: filtered_channels,
                        xtream_cluster: group.xtream_cluster,
                    }))
                }
            })
            .collect::<Vec<_>>()
    })
}

fn build_result(
    live: Option<Vec<Rc<UiPlaylistGroup>>>,
    vod: Option<Vec<Rc<UiPlaylistGroup>>>,
    series: Option<Vec<Rc<UiPlaylistGroup>>>,
) -> Option<UiPlaylistCategories> {
    if live.is_none() && vod.is_none() && series.is_none() {
        None
    } else {
        Some(UiPlaylistCategories { live, vod, series })
    }
}

impl UiPlaylistCategories {
    pub fn filter(&self, search_req: &SearchRequest) -> Option<Self> {
        match search_req {
            SearchRequest::Clear => None,
            SearchRequest::Text(text, search_fields) => {
                let mask = SearchFieldMask::from_search_fields(search_fields.as_deref());
                let text_lc = text.to_lowercase();
                let matches = |value: &str| value.to_lowercase().contains(&text_lc);
                let text_trim = text.trim();
                let matches_provider_id = |value: &str| value == text_trim;
                let live = filter_channels(self.live.as_ref(), mask, &matches, &matches_provider_id);
                let video = filter_channels(self.vod.as_ref(), mask, &matches, &matches_provider_id);
                let series = filter_channels(self.series.as_ref(), mask, &matches, &matches_provider_id);
                build_result(live, video, series)
            }
            SearchRequest::Regexp(text, search_fields) => {
                if let Ok(regex) = crate::model::REGEX_CACHE.get_or_compile(text) {
                    let mask = SearchFieldMask::from_search_fields(search_fields.as_deref());
                    let matches = |value: &str| regex.is_match(value);
                    let matches_provider_id = |value: &str| regex.is_match(value);
                    let live = filter_channels(self.live.as_ref(), mask, &matches, &matches_provider_id);
                    let video = filter_channels(self.vod.as_ref(), mask, &matches, &matches_provider_id);
                    let series = filter_channels(self.series.as_ref(), mask, &matches, &matches_provider_id);
                    build_result(live, video, series)
                } else {
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::Internable;

    fn sample_channel(id: u32, provider_id: &str, title: &str) -> Rc<UiPlaylistItem> {
        Rc::new(UiPlaylistItem {
            virtual_id: id,
            provider_id: provider_id.intern(),
            name: title.intern(),
            title: title.intern(),
            group: "Group 1".intern(),
            logo: "".intern(),
            url: "http://example.com/stream.m3u8".intern(),
            item_type: PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            category_id: 1,
            rating: 0.0,
            input_name: "test_input".intern(),
            epg_channel_id: None,
            explorer_meta: None,
        })
    }

    #[test]
    fn search_by_provider_id_exact() {
        let ch1 = sample_channel(1, "12345", "Channel One");
        let ch2 = sample_channel(2, "urlhash-v1:abcdef1234567890", "Channel Two");
        let group = Rc::new(UiPlaylistGroup {
            id: 1,
            title: "Group 1".intern(),
            channels: vec![ch1, ch2],
            xtream_cluster: XtreamCluster::Live,
        });
        let categories = UiPlaylistCategories { live: Some(vec![group]), vod: None, series: None };

        // Exact match for numeric provider id
        let req_exact =
            SearchRequest::Text("12345".to_string(), Some(Rc::new(vec![SEARCH_FIELD_PROVIDER_ID.to_string()])));
        let res = categories.filter(&req_exact).expect("match found");
        let live_groups = res.live.expect("live groups");
        assert_eq!(live_groups[0].channels.len(), 1);
        assert_eq!(live_groups[0].channels[0].provider_id.as_ref(), "12345");

        // Substring / prefix of provider id must NOT match
        let req_prefix =
            SearchRequest::Text("123".to_string(), Some(Rc::new(vec![SEARCH_FIELD_PROVIDER_ID.to_string()])));
        assert_eq!(categories.filter(&req_prefix).and_then(|r| r.live).map_or(0, |g| g.len()), 0);

        // Exact match for urlhash-v1 key
        let req_hash = SearchRequest::Text(
            "urlhash-v1:abcdef1234567890".to_string(),
            Some(Rc::new(vec![SEARCH_FIELD_PROVIDER_ID.to_string()])),
        );
        let res_hash = categories.filter(&req_hash).expect("match found");
        let live_groups_hash = res_hash.live.expect("live groups");
        assert_eq!(live_groups_hash[0].channels.len(), 1);
        assert_eq!(live_groups_hash[0].channels[0].provider_id.as_ref(), "urlhash-v1:abcdef1234567890");

        // Substring of urlhash must NOT match
        let req_hash_sub =
            SearchRequest::Text("urlhash".to_string(), Some(Rc::new(vec![SEARCH_FIELD_PROVIDER_ID.to_string()])));
        assert_eq!(categories.filter(&req_hash_sub).and_then(|r| r.live).map_or(0, |g| g.len()), 0);
    }
}
