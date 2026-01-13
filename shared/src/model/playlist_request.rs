use crate::model::{PlaylistItemType, SearchRequest, StreamProperties, XtreamCluster};
use crate::utils::arc_str_serde;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::rc::Rc;
use std::sync::Arc;

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

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub enum PlaylistRequest {
    Target(u16),
    Input(u16),
    CustomXtream(PlaylistRequestXtream),
    CustomM3u(PlaylistRequestM3u)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct CommonPlaylistItem {
    pub virtual_id: u32,
    pub provider_id: String,
    pub name: String,
    pub chno: u32,
    pub logo: String,
    pub logo_small: String,
    #[serde(with = "arc_str_serde")]
    pub group: Arc<str>,
    pub title: String,
    pub parent_code: String,
    pub audio_track: String,
    pub time_shift: String,
    pub rec: String,
    pub url: String,
    #[serde(with = "arc_str_serde")]
    pub input_name: Arc<str>,
    pub item_type: PlaylistItemType,
    #[serde(default)]
    pub epg_channel_id: Option<String>,
    #[serde(default)]
    pub xtream_cluster: Option<XtreamCluster>,
    #[serde(default)]
    pub additional_properties: Option<StreamProperties>,
    #[serde(default)]
    pub category_id: Option<u32>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct PlaylistResponseGroup {
    pub id: u32,
    #[serde(with = "arc_str_serde")]
    pub title: Arc<str>,
    pub channels: Vec<CommonPlaylistItem>,
    pub xtream_cluster: XtreamCluster,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct UiPlaylistGroup {
    pub id: u32,
    #[serde(with = "arc_str_serde")]
    pub title: Arc<str>,
    pub channels: Vec<Rc<CommonPlaylistItem>>,
    pub xtream_cluster: XtreamCluster,
}

impl From<PlaylistResponseGroup> for UiPlaylistGroup {
    fn from(response: PlaylistResponseGroup) -> Self {
        Self {
            id: response.id,
            title: response.title,
            channels: response.channels.into_iter().map(Rc::new).collect(),
            xtream_cluster: response.xtream_cluster,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct PlaylistCategoriesResponse {
    #[serde(default)]
    pub live: Option<Vec<PlaylistResponseGroup>>,
    #[serde(default)]
    pub vod: Option<Vec<PlaylistResponseGroup>>,
    #[serde(default)]
    pub series: Option<Vec<PlaylistResponseGroup>>,
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

impl From<PlaylistCategoriesResponse> for UiPlaylistCategories {
    fn from(response: PlaylistCategoriesResponse) -> Self {
        Self {
            live: response.live.map(|groups| groups.into_iter().map(Into::into).map(Rc::new).collect()),
            vod: response.vod.map(|groups| groups.into_iter().map(Into::into).map(Rc::new).collect()),
            series: response.series.map(|groups| groups.into_iter().map(Into::into).map(Rc::new).collect()),
        }
    }
}

fn filter_channels(
    groups: Option<&Vec<Rc<UiPlaylistGroup>>>,
    text: &str,
) -> Option<Vec<Rc<UiPlaylistGroup>>> {
    // normalize search text (lowercase)
    let text = text.to_lowercase();

    groups.as_ref().map(|gs| {
        gs.iter()
            .filter_map(|group| {
                let title_lower = group.title.to_lowercase();

                if title_lower.contains(&text) {
                    return Some(Rc::clone(group));
                }

                let filtered_channels: Vec<Rc<CommonPlaylistItem>> = group
                    .channels
                    .iter()
                    .filter(|c| {
                        c.title.to_lowercase().contains(&text)
                            || c.name.to_lowercase().contains(&text)
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

fn filter_channels_re(groups: Option<&Vec<Rc<UiPlaylistGroup>>>, regex: &Regex) -> Option<Vec<Rc<UiPlaylistGroup>>> {
    groups.as_ref().map(|gs| {
        gs.iter()
            .filter_map(|group| {
                if regex.is_match(&group.title) {
                    return Some(Rc::clone(group));
                }

                let filtered_channels: Vec<Rc<CommonPlaylistItem>> = group
                    .channels
                    .iter()
                    .filter(|c| regex.is_match(&c.title) || regex.is_match(&c.name))
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

fn build_result(live: Option<Vec<Rc<UiPlaylistGroup>>>,
                vod: Option<Vec<Rc<UiPlaylistGroup>>>,
                series: Option<Vec<Rc<UiPlaylistGroup>>>) -> Option<UiPlaylistCategories> {
    if live.is_none() && vod.is_none() && series.is_none() {
        None
    } else {
        Some(UiPlaylistCategories {
            live,
            vod,
            series,
        })
    }
}

impl UiPlaylistCategories {
    pub fn filter(&self, search_req: &SearchRequest) -> Option<Self> {
        match search_req {
            SearchRequest::Clear => None,
            SearchRequest::Text(text, _search_fields) => {
                let text_lc = text.to_lowercase();
                let live = filter_channels(self.live.as_ref(), &text_lc);
                let video = filter_channels(self.vod.as_ref(), &text_lc);
                let series = filter_channels(self.series.as_ref(), &text_lc);
                build_result(live, video, series)
            }
            SearchRequest::Regexp(text, _search_fields) => {
                if let Ok(regex) = crate::model::REGEX_CACHE.get_or_compile(text) {
                    let live = filter_channels_re(self.live.as_ref(), &regex);
                    let video = filter_channels_re(self.vod.as_ref(), &regex);
                    let series = filter_channels_re(self.series.as_ref(), &regex);
                    build_result(live, video, series)
                } else {
                    None
                }
            }
        }
    }
}
