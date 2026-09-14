use crate::{
    error::Error,
    services::{get_base_href, request_get, request_post, Encoding},
};
use futures::join;
use indexmap::IndexMap;
use log::error;
use shared::{
    model::{
        EpgChannel, EpgTv, InputRefreshOverride, OperationRunAccepted, PlaylistEpgRequest, PlaylistRequest,
        PlaylistUpdateRequestDto, PlaylistUpdateStatusDto, PlaylistUrlResolveRequest, SeriesStreamProperties,
        StreamEpgItemRequest, StreamEpgRequest, StreamEpgResponse, UiPlaylistCategories, UiPlaylistGroup,
        UiPlaylistItem, XtreamCluster, XtreamSeriesInfoDoc,
    },
    utils::concat_path_leading_slash,
};
use std::rc::Rc;

pub struct PlaylistService {
    target_update_api_path: String,
    playlist_update_status_path: String,
    playlist_api_live_path: String,
    playlist_api_vod_path: String,
    playlist_api_series_path: String,
    playlist_api_resolve_url_path: String,
    playlist_api_epg_path: String,
    playlist_api_series_info_path: String,
    playlist_api_episode_info_path: String,
    stream_epg_path: String,
}
impl Default for PlaylistService {
    fn default() -> Self { Self::new() }
}

impl PlaylistService {
    pub fn new() -> Self {
        let base_href = get_base_href();
        let api = |endpoint: &str| concat_path_leading_slash(&base_href, &format!("api/v1/playlist/{endpoint}"));

        Self {
            target_update_api_path: api("update"),
            playlist_update_status_path: api("update/status"),
            playlist_api_live_path: api("live"),
            playlist_api_vod_path: api("vod"),
            playlist_api_series_path: api("series"),
            playlist_api_resolve_url_path: api("resolve_url"),
            playlist_api_epg_path: api("epg"),
            playlist_api_series_info_path: api("series_info"),
            playlist_api_episode_info_path: api("series/episode"),
            stream_epg_path: api("epg/stream"),
        }
    }
    pub async fn update_targets(&self, targets: &[&str]) -> bool {
        let request = build_playlist_update_request(targets);
        self.submit_update_request(&request).await.is_ok()
    }

    pub async fn update_all_inputs(&self) -> Result<OperationRunAccepted, Error> {
        self.submit_update_request(&build_playlist_update_bulk_request()).await
    }

    pub async fn update_input(
        &self,
        target_ids: &[u16],
        input_id: u16,
        action: shared::model::InputUpdateAction,
    ) -> Result<OperationRunAccepted, Error> {
        let request =
            build_manual_input_update_request(target_ids, shared::model::InputUpdateRequest { input_id, action });
        self.submit_update_request(&request).await
    }

    pub async fn get_update_status(&self) -> Result<Option<PlaylistUpdateStatusDto>, crate::error::Error> {
        request_get(&self.playlist_update_status_path, None, None).await
    }

    async fn submit_update_request(&self, request: &PlaylistUpdateRequestDto) -> Result<OperationRunAccepted, Error> {
        request_post::<&PlaylistUpdateRequestDto, OperationRunAccepted>(
            &self.target_update_api_path,
            request,
            None,
            None,
        )
        .await
        .and_then(|response| response.ok_or(Error::RequestError))
    }

    pub async fn get_playlist_categories(
        &self,
        playlist_request: &PlaylistRequest,
    ) -> Option<Rc<UiPlaylistCategories>> {
        let (live_res, vod_res, series_res) = join!(
            request_post::<&PlaylistRequest, Vec<UiPlaylistItem>>(
                &self.playlist_api_live_path,
                playlist_request,
                None,
                Some(Encoding::Cbor),
            ),
            request_post::<&PlaylistRequest, Vec<UiPlaylistItem>>(
                &self.playlist_api_vod_path,
                playlist_request,
                None,
                Some(Encoding::Cbor),
            ),
            request_post::<&PlaylistRequest, Vec<UiPlaylistItem>>(
                &self.playlist_api_series_path,
                playlist_request,
                None,
                Some(Encoding::Cbor),
            ),
        );

        let live = live_res.map_or_else(
            |err| {
                error!("Failed to fetch live playlist: {err}");
                None
            },
            |r| r.map(|resp| to_ui_playlist_groups(resp, XtreamCluster::Live)),
        );
        let vod = vod_res.map_or_else(
            |err| {
                error!("Failed to fetch vod playlist: {err}");
                None
            },
            |r| r.map(|resp| to_ui_playlist_groups(resp, XtreamCluster::Video)),
        );
        let series = series_res.map_or_else(
            |err| {
                error!("Failed to fetch series playlist: {err}");
                None
            },
            |r| r.map(|resp| to_ui_playlist_groups(resp, XtreamCluster::Series)),
        );

        if live.is_some() || vod.is_some() || series.is_some() {
            return Some(Rc::new(UiPlaylistCategories { live, vod, series }));
        }
        None
    }

    pub async fn resolve_url(&self, request: PlaylistUrlResolveRequest) -> Option<String> {
        if let PlaylistUrlResolveRequest::Provider { url, .. } = &request {
            if !url.starts_with(shared::utils::PROVIDER_SCHEME_PREFIX) {
                return Some(url.clone());
            }
        }

        request_post::<&PlaylistUrlResolveRequest, String>(
            &self.playlist_api_resolve_url_path,
            &request,
            None,
            Some(Encoding::Text),
        )
        .await
        .unwrap_or_else(|err| {
            error!("{err}");
            None
        })
    }

    pub async fn get_playlist_epg(&self, request: PlaylistEpgRequest) -> Option<EpgTv> {
        match request_post::<&PlaylistEpgRequest, Vec<EpgChannel>>(
            &self.playlist_api_epg_path,
            &request,
            None,
            Some(Encoding::Cbor),
        )
        .await
        {
            Ok(channels) => channels.map(EpgTv::new),
            Err(err) => {
                error!("{err}");
                None
            }
        }
    }

    /// Fetches per-stream EPG data for the UI "now playing" / "up next" display.
    /// Accepts a batch of `epg_channel_ids` and returns programme data for each,
    /// filtered to an 8h window with user timeshift applied server-side.
    pub async fn get_stream_epg(&self, items: Vec<StreamEpgItemRequest>) -> Option<StreamEpgResponse> {
        let request = StreamEpgRequest { items };
        request_post(&self.stream_epg_path, &request, None, Some(Encoding::Cbor)).await.unwrap_or_else(|err| {
            error!("{err}");
            None
        })
    }

    pub async fn get_series_info(
        &self,
        pli: &Rc<UiPlaylistItem>,
        playlist_request: &PlaylistRequest,
    ) -> Option<SeriesStreamProperties> {
        let path = format!("{}/{}/{}", self.playlist_api_series_info_path, pli.virtual_id, pli.provider_id);
        request_post::<&PlaylistRequest, XtreamSeriesInfoDoc>(&path, playlist_request, None, Some(Encoding::Cbor))
            .await
            .map_or_else(
                |err| {
                    error!("{err}");
                    None
                },
                |response| response.as_ref().map(|doc| SeriesStreamProperties::from_info_doc(doc, pli.virtual_id)),
            )
    }

    pub async fn get_episode(&self, virtual_id: u32, playlist_request: &PlaylistRequest) -> Option<UiPlaylistItem> {
        let path = format!("{}/{virtual_id}", self.playlist_api_episode_info_path);
        request_post::<&PlaylistRequest, UiPlaylistItem>(&path, playlist_request, None, None).await.unwrap_or_else(
            |err| {
                error!("{err}");
                None
            },
        )
    }
}

fn build_playlist_update_request(targets: &[&str]) -> PlaylistUpdateRequestDto {
    PlaylistUpdateRequestDto {
        targets: targets.iter().map(|target| (*target).to_string()).collect(),
        target_ids: None,
        input_refresh: None,
        input_action: None,
    }
}

fn build_playlist_update_bulk_request() -> PlaylistUpdateRequestDto { PlaylistUpdateRequestDto::default() }

fn build_manual_input_update_request(
    target_ids: &[u16],
    request: shared::model::InputUpdateRequest,
) -> PlaylistUpdateRequestDto {
    match request.action {
        shared::model::InputUpdateAction::Provider(policy) => {
            build_input_playlist_update_request(target_ids, InputRefreshOverride { input_id: request.input_id, policy })
        }
        shared::model::InputUpdateAction::Rescan => PlaylistUpdateRequestDto {
            target_ids: Some(target_ids.to_vec()),
            input_action: Some(request),
            ..PlaylistUpdateRequestDto::default()
        },
    }
}

fn build_input_playlist_update_request(
    target_ids: &[u16],
    input_refresh: InputRefreshOverride,
) -> PlaylistUpdateRequestDto {
    PlaylistUpdateRequestDto {
        targets: Vec::new(),
        target_ids: Some(target_ids.to_vec()),
        input_refresh: Some(input_refresh),
        input_action: None,
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

#[cfg(test)]
mod tests {
    use super::{
        build_input_playlist_update_request, build_playlist_update_bulk_request, build_playlist_update_request,
    };
    use crate::model::{InputUpdateCapabilities, InputUpdateCapabilitiesExt};
    use shared::model::{ConfigInputDto, InputRefreshOverride, InputRefreshPolicy, InputType};

    #[test]
    fn playlist_update_action_rescan_sends_selected_ids_without_a_provider_policy() {
        let action =
            shared::model::InputUpdateRequest { input_id: 2, action: shared::model::InputUpdateAction::Rescan };
        let request = super::build_manual_input_update_request(&[20, 21], action);
        assert_eq!(request.target_ids, Some(vec![20, 21]));
        assert_eq!(request.input_action, Some(action));
        assert_eq!(request.input_refresh, None);
        assert!(request.targets.is_empty());
        assert_eq!(build_playlist_update_bulk_request().input_action, None);
    }

    #[test]
    fn playlist_update_view_action_capability_policies_reach_the_existing_id_scoped_request() {
        for input_type in [
            InputType::Xtream,
            InputType::Stalker,
            InputType::M3u,
            InputType::M3uBatch,
            InputType::XtreamBatch,
            InputType::StalkerBatch,
            InputType::Staged,
            InputType::Plex,
            InputType::Jellyfin,
            InputType::Emby,
        ] {
            let input = ConfigInputDto { id: 7, input_type, ..ConfigInputDto::default() };
            for &policy in InputUpdateCapabilities::for_input(&input).policies() {
                let request =
                    build_input_playlist_update_request(&[30, 20], InputRefreshOverride { input_id: input.id, policy });
                assert!(request.targets.is_empty());
                assert_eq!(request.target_ids, Some(vec![30, 20]));
                assert_eq!(request.input_refresh, Some(InputRefreshOverride { input_id: 7, policy }));
            }
        }
    }

    #[test]
    fn playlist_update_view_action_m3u_cache_refresh_contract_is_request_local() -> Result<(), serde_json::Error> {
        let input = ConfigInputDto { id: 7, input_type: InputType::M3u, ..ConfigInputDto::default() };
        let capabilities = InputUpdateCapabilities::for_input(&input);
        // Exercise the existing request contract consumed by the default input-cache path.
        // A later normal request (including another input) must not inherit refresh or force overrides.
        for (input_id, policy, cache, quality) in [
            (7, InputRefreshPolicy::NORMAL, "respect", "enforce"),
            (7, InputRefreshPolicy::REFRESH, "bypass", "enforce"),
            (7, InputRefreshPolicy::FORCE, "bypass", "bypass"),
            (8, InputRefreshPolicy::NORMAL, "respect", "enforce"),
            (7, InputRefreshPolicy::NORMAL, "respect", "enforce"),
        ] {
            assert!(capabilities.supports(policy));
            let request = build_input_playlist_update_request(&[30], InputRefreshOverride { input_id, policy });
            assert_eq!(
                serde_json::to_value(request)?,
                serde_json::json!({
                    "targets": [], "target_ids": [30],
                    "input_refresh": { "input_id": input_id, "policy": { "cache": cache, "quality": quality } }
                })
            );
            assert_eq!(policy.bypasses_cache(), cache == "bypass");
            assert_eq!(policy.bypasses_quality(), quality == "bypass");
        }
        Ok(())
    }

    #[test]
    fn playlist_update_bulk_request_is_independent_of_every_card_capability_and_selection() {
        for input_type in [
            InputType::Xtream,
            InputType::Stalker,
            InputType::M3u,
            InputType::M3uBatch,
            InputType::XtreamBatch,
            InputType::StalkerBatch,
            InputType::Staged,
            InputType::Library,
            InputType::Plex,
            InputType::Jellyfin,
            InputType::Emby,
        ] {
            for enabled in [true, false] {
                let input = ConfigInputDto { id: 7, input_type, enabled, ..ConfigInputDto::default() };
                for &policy in InputUpdateCapabilities::for_input(&input).policies() {
                    let card_request =
                        build_input_playlist_update_request(&[30], InputRefreshOverride { input_id: input.id, policy });
                    assert!(card_request.input_refresh.is_some());
                    assert_eq!(build_playlist_update_bulk_request(), Default::default());
                }
                // Bulk remains available as the normal server-selected all-targets request,
                // including when no single-input policy is offered by this capability.
                assert_eq!(build_playlist_update_bulk_request(), Default::default());
            }
        }
    }

    #[test]
    fn playlist_update_action_request_keeps_target_ids_input_id_and_typed_policy() {
        let request = build_input_playlist_update_request(
            &[30, 20],
            InputRefreshOverride { input_id: 7, policy: InputRefreshPolicy::FORCE },
        );

        assert!(request.targets.is_empty());
        assert_eq!(request.target_ids, Some(vec![30, 20]));
        assert_eq!(
            request.input_refresh,
            Some(InputRefreshOverride { input_id: 7, policy: InputRefreshPolicy::FORCE })
        );
    }

    #[test]
    fn playlist_update_action_same_name_targets_create_distinct_id_only_requests() {
        for (selected_ids, expected_ids) in [(&[1][..], vec![1]), (&[4][..], vec![4]), (&[1, 4][..], vec![1, 4])] {
            let request = build_input_playlist_update_request(
                selected_ids,
                InputRefreshOverride { input_id: 7, policy: InputRefreshPolicy::NORMAL },
            );

            assert!(request.targets.is_empty());
            assert_eq!(request.target_ids, Some(expected_ids));
        }
    }

    #[test]
    fn playlist_update_action_legacy_bulk_request_keeps_target_names_without_ids() {
        let request = build_playlist_update_request(&["target-30", "target-20"]);

        assert_eq!(request.targets, vec!["target-30", "target-20"]);
        assert_eq!(request.target_ids, None);
        assert_eq!(request.input_refresh, None);
    }

    #[test]
    fn playlist_update_bulk_request_uses_normal_policy_and_complete_server_target_selection() {
        let request = build_playlist_update_bulk_request();

        assert!(request.targets.is_empty());
        assert_eq!(request.target_ids, None);
        assert_eq!(request.input_refresh, None);
    }
}
