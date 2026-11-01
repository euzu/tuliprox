use crate::{
    error::Error,
    services::{
        get_base_href, request_get, request_get_binary, request_post, request_post_with_error_body, Encoding,
        WriteFailure,
    },
};
use futures::join;
use indexmap::IndexMap;
use log::error;
use shared::{
    model::{
        EpgChannel, EpgChannelFilter, EpgGridRequest, EpgGridRow, EpgGroupInfo, EpgGroupsRequest, EpgSearchResultDto,
        EpgTv, FieldError, InputRefreshOverride, OperationRunAccepted, OverrideConflictDto, OverrideDiscardRequest,
        OverrideFieldErrorsDto, OverrideFileDto, OverrideItemKind, OverrideOrphansDto, OverrideProvenanceDto,
        OverrideRevisionDto, OverrideSaveRequest, OverrideScopeDto, PlaylistEpgRequest, PlaylistRequest,
        PlaylistUpdateRequestDto, PlaylistUpdateStatusDto, PlaylistUrlResolveRequest, SeriesStreamProperties,
        StreamEpgItemRequest, StreamEpgRequest, StreamEpgResponse, TargetAnalyticsResponseDto, UiPlaylistCategories,
        UiPlaylistGroup, UiPlaylistItem, XtreamCluster, XtreamSeriesInfoDoc,
    },
    utils::concat_path_leading_slash,
};
use std::rc::Rc;

#[derive(Debug, Clone, PartialEq)]
pub enum OverrideSaveResult {
    Ok(OverrideRevisionDto),
    Conflict(OverrideConflictDto),
    FieldErrors(Vec<FieldError>),
    Err(String),
}

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
    epg_groups_path: String,
    epg_grid_path: String,
    playlist_analytics_path: String,
    overrides_path: String,
    overrides_item_path: String,
    overrides_discard_path: String,
    epg_search_path: String,
    overrides_orphans_path: String,
}
impl Default for PlaylistService {
    fn default() -> Self { Self::new() }
}

impl PlaylistService {
    pub fn new() -> Self { Self::from_base(&get_base_href()) }

    pub fn from_base(base_href: &str) -> Self {
        let api = |endpoint: &str| concat_path_leading_slash(base_href, &format!("api/v1/playlist/{endpoint}"));

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
            epg_groups_path: api("epg/groups"),
            epg_grid_path: api("epg/grid"),
            playlist_analytics_path: api("analytics"),
            overrides_path: api("overrides"),
            overrides_item_path: api("overrides/item"),
            overrides_discard_path: api("overrides/discard"),
            epg_search_path: api("epg/search"),
            overrides_orphans_path: api("overrides/orphans"),
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

    pub async fn get_target_analytics(&self) -> Result<Option<TargetAnalyticsResponseDto>, Error> {
        request_get(&self.playlist_analytics_path, None, None).await
    }

    pub fn unmapped_csv_url_from_base(base_href: &str, target_id: u16) -> String {
        concat_path_leading_slash(base_href, &format!("api/v1/playlist/analytics/{target_id}/unmapped.csv"))
    }

    pub fn unmapped_csv_url(&self, target_id: u16) -> String {
        Self::unmapped_csv_url_from_base(&get_base_href(), target_id)
    }

    pub async fn download_unmapped_csv(&self, target_id: u16, target_name: &str) -> Result<(), Error> {
        let url = self.unmapped_csv_url(target_id);
        let bytes = request_get_binary(&url).await?;
        let mut sanitized = shared::utils::sanitize_filename_chars(target_name, true);
        if sanitized.is_empty() {
            sanitized = format!("target-{target_id}");
        }
        let filename = format!("{sanitized}-unmapped.csv");
        crate::utils::trigger_browser_download(&bytes, &filename, "text/csv;charset=utf-8")
    }

    pub async fn update_target_by_id(&self, target_id: u16) -> Result<OperationRunAccepted, Error> {
        let request =
            PlaylistUpdateRequestDto { target_ids: Some(vec![target_id]), ..PlaylistUpdateRequestDto::default() };
        self.submit_update_request(&request).await
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

    /// Playlist groups of a target that have channels with EPG data, in playlist order.
    /// `Ok(None)`: the target has no group index yet (it was not updated since the index exists).
    /// With a `filter`, only groups with matching channels, counting the matches.
    pub async fn get_epg_groups(
        &self,
        target_id: u16,
        filter: Option<EpgChannelFilter>,
    ) -> Result<Option<Vec<EpgGroupInfo>>, Error> {
        request_post(&self.epg_groups_path, &EpgGroupsRequest { target_id, filter }, None, Some(Encoding::Cbor)).await
    }

    /// Channels of one playlist group with their programmes (time and title only).
    /// `Ok(None)`: the server has no content for the group.
    pub async fn get_epg_grid(
        &self,
        target_id: u16,
        group: String,
        filter: Option<EpgChannelFilter>,
    ) -> Result<Option<Vec<EpgGridRow>>, Error> {
        request_post(&self.epg_grid_path, &EpgGridRequest { target_id, group, filter }, None, Some(Encoding::Cbor))
            .await
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
        pli: &UiPlaylistItem,
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

    pub async fn get_overrides(&self, scope: OverrideScopeDto, name: &str) -> Result<Option<OverrideFileDto>, Error> {
        let url = format!("{}?scope={}&name={}", self.overrides_path, scope, js_sys::encode_uri_component(name));
        request_get(&url, None, None).await
    }

    pub async fn get_override_item(
        &self,
        input: &str,
        kind: OverrideItemKind,
        id: &str,
        target_id: Option<u16>,
    ) -> Result<Option<OverrideProvenanceDto>, Error> {
        let kind_str = match kind {
            OverrideItemKind::Live => "live",
            OverrideItemKind::Vod => "vod",
            OverrideItemKind::Series => "series",
        };
        let mut url = format!(
            "{}?input={}&kind={}&id={}",
            self.overrides_item_path,
            js_sys::encode_uri_component(input),
            kind_str,
            js_sys::encode_uri_component(id),
        );
        if let Some(target_id) = target_id {
            url.push_str(&format!("&target_id={target_id}"));
        }
        request_get(&url, None, None).await
    }

    pub async fn save_overrides(&self, request: &OverrideSaveRequest) -> OverrideSaveResult {
        let outcome =
            request_post_with_error_body::<_, OverrideRevisionDto, serde_json::Value>(&self.overrides_path, request)
                .await;

        match outcome {
            Ok(Some(revision)) => OverrideSaveResult::Ok(revision),
            Ok(None) => {
                OverrideSaveResult::Err(Error::HttpResponse("Empty override save response".to_string()).to_string())
            }
            Err(failure) => {
                let WriteFailure { status, error, body, message } = failure;
                let message = error.map_or(message, |error| error.to_string());
                match status {
                    409 => body
                        .and_then(|body| serde_json::from_value::<OverrideConflictDto>(body).ok())
                        .map_or_else(|| OverrideSaveResult::Err(message.clone()), OverrideSaveResult::Conflict),
                    422 => {
                        body.and_then(|body| serde_json::from_value::<OverrideFieldErrorsDto>(body).ok()).map_or_else(
                            || OverrideSaveResult::Err(message),
                            |errors| OverrideSaveResult::FieldErrors(errors.field_errors),
                        )
                    }
                    _ => OverrideSaveResult::Err(message),
                }
            }
        }
    }

    pub async fn discard_overrides(
        &self,
        request: &OverrideDiscardRequest,
    ) -> Result<Option<OverrideRevisionDto>, Error> {
        request_post::<&OverrideDiscardRequest, OverrideRevisionDto>(&self.overrides_discard_path, request, None, None)
            .await
    }

    pub async fn search_epg(
        &self,
        query: &str,
        target_id: Option<u16>,
        input: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<EpgSearchResultDto>, Error> {
        let mut params = Vec::new();
        params.push(format!("q={}", js_sys::encode_uri_component(query)));
        if let Some(target_id) = target_id {
            params.push(format!("target_id={target_id}"));
        } else if let Some(input) = input {
            params.push(format!("input={}", js_sys::encode_uri_component(input)));
        }
        if let Some(limit) = limit {
            params.push(format!("limit={limit}"));
        }
        let url = format!("{}?{}", self.epg_search_path, params.join("&"));
        let res: Result<Option<Vec<EpgSearchResultDto>>, Error> = request_get(&url, None, None).await;
        res.map(Option::unwrap_or_default)
    }

    pub async fn get_orphans(&self, input: Option<&str>) -> Result<Option<OverrideOrphansDto>, Error> {
        let mut url = self.overrides_orphans_path.clone();
        if let Some(input) = input {
            url.push_str(&format!("?input={}", js_sys::encode_uri_component(input)));
        }
        request_get(&url, None, None).await
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
        assert_eq!(request.targets.len(), 0);
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
                assert_eq!(request.targets.len(), 0);
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

        assert_eq!(request.targets.len(), 0);
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

            assert_eq!(request.targets.len(), 0);
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

        assert_eq!(request.targets.len(), 0);
        assert_eq!(request.target_ids, None);
        assert_eq!(request.input_refresh, None);
    }

    #[test]
    fn unmapped_csv_url_constructs_expected_endpoint_path() {
        let url = super::PlaylistService::unmapped_csv_url_from_base("/", 42);
        assert_eq!(url, "/api/v1/playlist/analytics/42/unmapped.csv");
        let url_subpath = super::PlaylistService::unmapped_csv_url_from_base("/prefix/", 99);
        assert_eq!(url_subpath, "/prefix/api/v1/playlist/analytics/99/unmapped.csv");
    }

    #[test]
    fn override_urls_construct_expected_endpoint_paths() {
        let service = super::PlaylistService::from_base("/");
        assert_eq!(service.overrides_path, "/api/v1/playlist/overrides");
        assert_eq!(service.overrides_item_path, "/api/v1/playlist/overrides/item");
        assert_eq!(service.overrides_discard_path, "/api/v1/playlist/overrides/discard");
        assert_eq!(service.epg_search_path, "/api/v1/playlist/epg/search");
        assert_eq!(service.overrides_orphans_path, "/api/v1/playlist/overrides/orphans");
    }
}
