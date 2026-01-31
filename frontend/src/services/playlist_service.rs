use crate::services::{get_base_href, request_post};
use log::error;
use shared::model::{EpgChannel, EpgTv, PlaylistEpgRequest, PlaylistRequest, SeriesStreamProperties, UiPlaylistCategories, UiPlaylistGroup, UiPlaylistItem, WebplayerUrlRequest, XtreamCluster, XtreamSeriesInfoDoc};

use futures::join;
use indexmap::IndexMap;
use shared::utils::{concat_path_leading_slash, ACCEPT_PREFER_CBOR};
use std::rc::Rc;

pub struct PlaylistService {
    target_update_api_path: String,
    playlist_api_live_path: String,
    playlist_api_vod_path: String,
    playlist_api_series_path: String,
    playlist_api_webplayer_url_path: String,
    playlist_api_epg_path: String,
    playlist_api_series_info_path: String,
    playlist_api_episode_info_path: String,
}
impl Default for PlaylistService {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaylistService {
    pub fn new() -> Self {
        let base_href = get_base_href();
        Self {
            target_update_api_path: concat_path_leading_slash(&base_href, "api/v1/playlist/update"),
            playlist_api_live_path: concat_path_leading_slash(&base_href, "api/v1/playlist/live"),
            playlist_api_vod_path: concat_path_leading_slash(&base_href, "api/v1/playlist/vod"),
            playlist_api_series_path: concat_path_leading_slash(&base_href, "api/v1/playlist/series"),
            playlist_api_webplayer_url_path: concat_path_leading_slash(&base_href, "api/v1/playlist/webplayer"),
            playlist_api_epg_path: concat_path_leading_slash(&base_href, "api/v1/playlist/epg"),
            playlist_api_series_info_path: concat_path_leading_slash(&base_href, "api/v1/playlist/series_info"),
            playlist_api_episode_info_path: concat_path_leading_slash(&base_href, "api/v1/playlist/series/episode"),
        }
    }
    pub async fn update_targets(&self, targets: &[&str]) -> bool {
        request_post::<&[&str], ()>(&self.target_update_api_path, targets, None, None).await.map_or_else(|_err| {
            false
        }, |_| true)
    }

    pub async fn get_playlist_categories(&self, playlist_request: &PlaylistRequest) -> Option<Rc<UiPlaylistCategories>> {
        let (live_res, vod_res, series_res) = join!(
            request_post::<&PlaylistRequest, Vec<UiPlaylistItem>>(&self.playlist_api_live_path, playlist_request, None, Some(ACCEPT_PREFER_CBOR.to_string())),
            request_post::<&PlaylistRequest, Vec<UiPlaylistItem>>(&self.playlist_api_vod_path, playlist_request, None, Some(ACCEPT_PREFER_CBOR.to_string())),
            request_post::<&PlaylistRequest, Vec<UiPlaylistItem>>(&self.playlist_api_series_path, playlist_request, None, Some(ACCEPT_PREFER_CBOR.to_string())),
        );

        let live = live_res.map_or_else(|err| {
            error!("Failed to fetch live playlist: {err}");
            None
        }, |r| r.map(|resp| to_ui_playlist_groups(resp, XtreamCluster::Live)));
        let vod = vod_res.map_or_else(|err| {
            error!("Failed to fetch vod playlist: {err}");
            None
        }, |r| r.map(|resp| to_ui_playlist_groups(resp, XtreamCluster::Video)));
        let series = series_res.map_or_else(|err| {
            error!("Failed to fetch series playlist: {err}");
            None
        }, |r| r.map(|resp| to_ui_playlist_groups(resp, XtreamCluster::Series)));

        if live.is_some() || vod.is_some() || series.is_some() {
            return Some(Rc::new(UiPlaylistCategories {
                live,
                vod,
                series,
            }));
        }
        None
    }

    pub async fn get_playlist_webplayer_url(&self, target_id: u16, virtual_id: u32, cluster: XtreamCluster) -> Option<String> {
        let request = WebplayerUrlRequest {
            target_id,
            virtual_id,
            cluster,
        };
        request_post::<&WebplayerUrlRequest, String>(&self.playlist_api_webplayer_url_path, &request, None, Some("text/plain".to_string())).await.unwrap_or_else(|err| {
            error!("{err}");
            None
        })
    }

    pub async fn get_playlist_epg(&self, request: PlaylistEpgRequest) -> Option<EpgTv> {
        match request_post::<&PlaylistEpgRequest, Vec<EpgChannel>>(&self.playlist_api_epg_path, &request, None, Some(ACCEPT_PREFER_CBOR.to_string())).await {
            Ok(channels) => channels.map(EpgTv::new),
            Err(err) => {
                error!("{err}");
                None
            }
        }
    }

    pub async fn get_series_info(&self, pli: &Rc<UiPlaylistItem>, playlist_request: &PlaylistRequest) -> Option<SeriesStreamProperties> {
        let path = format!("{}/{}/{}", self.playlist_api_series_info_path, pli.virtual_id, pli.provider_id);
        request_post::<&PlaylistRequest, XtreamSeriesInfoDoc>(&path, playlist_request, None, Some(ACCEPT_PREFER_CBOR.to_string())).await.map_or_else(|err| {
            error!("{err}");
            None
        }, |response| {
            response.as_ref().map(|doc| SeriesStreamProperties::from_info_doc(doc, pli.virtual_id))
        })
    }

    pub async fn get_episode(&self, virtual_id: u32, playlist_request: &PlaylistRequest) -> Option<UiPlaylistItem> {
        let path = format!("{}/{virtual_id}", self.playlist_api_episode_info_path);
        request_post::<&PlaylistRequest, UiPlaylistItem>(&path, playlist_request, None, None).await.unwrap_or_else(|err| {
            error!("{err}");
            None
        })
    }
}

fn to_ui_playlist_groups(list: Vec<UiPlaylistItem>, xtream_cluster: XtreamCluster) -> Vec<Rc<UiPlaylistGroup>> {
    let mut groups = IndexMap::new();
    list.into_iter().for_each(|item| {
        let group_id = item.group.clone();
        let group = groups.entry(group_id).or_insert_with(|| UiPlaylistGroup {
            id: item.category_id,
            title: item.group.clone(),
            channels: vec![],
            xtream_cluster,
        });
        group.channels.push(Rc::new(item));
    });
    groups.into_iter().map(|(_, v)| Rc::new(v)).collect::<Vec<_>>()
}
