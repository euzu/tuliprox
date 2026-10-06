//! Web UI EPG grid: the playlist groups of a target and one group's channels with their
//! programmes. Both endpoints read the group index built next to the target EPG db and stream
//! one record at a time (JSON, or CBOR on `Accept: application/cbor`).

use crate::{
    api::{
        api_utils::stream_json_or_bin_response_try_stream,
        endpoints::{
            extract_accept_header::ExtractAcceptHeader,
            xmltv_api::{get_epg_path_for_target_by_type, rewrite_epg_resource_icon},
        },
        model::AppState,
    },
    model::ConfigTarget,
    repository::{
        epg_group_channels_path, epg_groups_path, BPlusTreeQuery, EpgGroupChannel, EpgGroupChannelKey, EpgGroupEntry,
        LockedReceiverStream,
    },
    utils::file_exists_async,
};
use axum::response::IntoResponse;
use log::error;
use serde::Serialize;
use shared::{
    model::{
        EpgChannel, EpgChannelFilter, EpgGridProgrammeDto, EpgGridRequest, EpgGridRow, EpgGroupInfo, EpgGroupsRequest,
        TargetType, MAX_EPG_GRID_ROWS,
    },
    utils::{concat_path_leading_slash, sanitize_sensitive_info},
};
use std::{ops::Bound, path::PathBuf, sync::Arc};
use tokio::{sync::mpsc, task};

/// Output whose playlist and EPG back the grid: Xtream if present, else M3U, matching the
/// Web UI playlist and EPG views.
pub(in crate::api) fn epg_grid_output_type(target: &ConfigTarget) -> Option<TargetType> {
    [TargetType::Xtream, TargetType::M3u].into_iter().find(|output| target.has_output(*output))
}

/// Keeps only what the grid shows: time and title.
pub(in crate::api) fn to_grid_programmes(channel: EpgChannel) -> Vec<EpgGridProgrammeDto> {
    channel
        .programmes
        .into_iter()
        .map(|programme| EpgGridProgrammeDto { start: programme.start, stop: programme.stop, title: programme.title })
        .collect()
}

/// Compiled size limit of a grid search regular expression.
const MAX_SEARCH_REGEX_SIZE: usize = 1 << 20;

/// Channel name matcher of the grid search.
pub(in crate::api) enum ChannelMatcher {
    All,
    /// Lowercased pattern, matched as substring of the lowercased name.
    Text(String),
    Regexp(Arc<regex::Regex>),
}

impl ChannelMatcher {
    /// `Err` with a message when the regular expression is invalid.
    pub(in crate::api) fn new(filter: Option<&EpgChannelFilter>) -> Result<Self, String> {
        match filter {
            None => Ok(Self::All),
            Some(EpgChannelFilter::Text(pattern)) if pattern.trim().is_empty() => Ok(Self::All),
            Some(EpgChannelFilter::Text(pattern)) => Ok(Self::Text(pattern.to_lowercase())),
            // Request patterns stay out of the shared regex cache, which only a reload sweeps.
            Some(EpgChannelFilter::Regexp(pattern)) => regex::RegexBuilder::new(pattern)
                .size_limit(MAX_SEARCH_REGEX_SIZE)
                .build()
                .map(|regex| Self::Regexp(Arc::new(regex)))
                .map_err(|err| format!("Invalid search pattern: {err}")),
        }
    }

    pub(in crate::api) fn is_all(&self) -> bool { matches!(self, Self::All) }

    pub(in crate::api) fn matches(&self, name: &str) -> bool {
        match self {
            Self::All => true,
            Self::Text(pattern) => name.to_lowercase().contains(pattern.as_str()),
            Self::Regexp(regex) => regex.is_match(name),
        }
    }
}

/// Key range of all channels of one group in the group channel index.
fn group_channel_range(group: &str) -> (EpgGroupChannelKey, EpgGroupChannelKey) {
    let name: Arc<str> = Arc::from(group);
    ((Arc::clone(&name), 0), (name, u32::MAX))
}

/// Runs `produce` on the blocking pool and streams what it sends. `produce` returns when done
/// or when sending fails because the client is gone.
fn stream_from_blocking<T, F>(accept: Option<&str>, label: &'static str, produce: F) -> axum::response::Response
where
    T: Serialize + Send + 'static,
    F: FnOnce(&dyn Fn(Result<T, String>) -> bool) + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<Result<T, String>>(64);
    let join_tx = tx.clone();
    let handle = task::spawn_blocking(move || produce(&|item| tx.blocking_send(item).is_ok()));
    tokio::spawn(async move {
        if let Err(err) = handle.await {
            let message = format!("{label} producer task failed: {err}");
            error!("{message}");
            let _ = join_tx.send(Err(message)).await;
        }
    });
    stream_json_or_bin_response_try_stream(accept, LockedReceiverStream::new_empty(rx))
}

/// Logs a producer error and forwards it to the stream, which ends the response.
fn send_error<T>(send: &dyn Fn(Result<T, String>) -> bool, message: String) {
    error!("{}", sanitize_sensitive_info(&message));
    let _ = send(Err(message));
}

/// EPG db path of the grid output, or the status to answer with instead.
fn resolve_epg_path(app_state: &AppState, target_id: u16) -> Result<PathBuf, axum::http::StatusCode> {
    let target = app_state.app_config.get_target_by_id(target_id).ok_or(axum::http::StatusCode::NOT_FOUND)?;
    let output = epg_grid_output_type(&target).ok_or(axum::http::StatusCode::NOT_FOUND)?;
    let config = app_state.app_config.config.load();
    get_epg_path_for_target_by_type(&config, &target, output).ok_or(axum::http::StatusCode::NO_CONTENT)
}

fn bad_search_request(message: &str) -> axum::response::Response {
    (axum::http::StatusCode::BAD_REQUEST, axum::Json(serde_json::json!({ "error": message }))).into_response()
}

/// Number of channels of `group` matching `matcher`, read from the group channel index.
fn count_group_matches(
    index: &mut BPlusTreeQuery<EpgGroupChannelKey, EpgGroupChannel>,
    group: &str,
    matcher: &ChannelMatcher,
) -> std::io::Result<u32> {
    let (start, end) = group_channel_range(group);
    let mut count = 0u32;
    for entry in index.range_iter(Bound::Included(&start), Bound::Included(&end)) {
        let (_, channel) = entry?;
        if matcher.matches(&channel.name) {
            count += 1;
        }
    }
    Ok(count)
}

pub(in crate::api) async fn playlist_epg_groups(
    ExtractAcceptHeader(accept): ExtractAcceptHeader,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(request): axum::extract::Json<EpgGroupsRequest>,
) -> axum::response::Response {
    let matcher = match ChannelMatcher::new(request.filter.as_ref()) {
        Ok(matcher) => matcher,
        Err(message) => return bad_search_request(&message),
    };
    let epg_path = match resolve_epg_path(&app_state, request.target_id) {
        Ok(epg_path) => epg_path,
        Err(status) => return status.into_response(),
    };
    let groups_path = epg_groups_path(&epg_path);
    let channels_path = epg_group_channels_path(&epg_path);
    // Targets not updated since the group index exists have no index yet.
    if !file_exists_async(&groups_path).await || !file_exists_async(&channels_path).await {
        return axum::http::StatusCode::NO_CONTENT.into_response();
    }
    let groups_lock = app_state.app_config.file_locks.read_lock(&groups_path).await;
    let channels_lock = app_state.app_config.file_locks.read_lock(&channels_path).await;
    stream_from_blocking(accept.as_deref(), "EPG groups", move |send| {
        let _guards = (groups_lock, channels_lock);
        let mut groups = match BPlusTreeQuery::<u32, EpgGroupEntry>::try_new(&groups_path) {
            Ok(groups) => groups,
            Err(err) => return send_error(send, format!("Failed to open {}: {err}", groups_path.display())),
        };
        // Only opened for a search: without one the stored counts are the answer.
        let mut index = if matcher.is_all() {
            None
        } else {
            match BPlusTreeQuery::<EpgGroupChannelKey, EpgGroupChannel>::try_new(&channels_path) {
                Ok(index) => Some(index),
                Err(err) => return send_error(send, format!("Failed to open {}: {err}", channels_path.display())),
            }
        };
        for entry in groups.iter() {
            let group = match entry {
                Ok((_, group)) => group,
                Err(err) => return send_error(send, format!("Failed to read {}: {err}", groups_path.display())),
            };
            let channel_count = match index.as_mut() {
                None => group.channel_count,
                Some(index) => match count_group_matches(index, &group.name, &matcher) {
                    Ok(0) => continue,
                    Ok(count) => count,
                    Err(err) => return send_error(send, format!("Failed to read {}: {err}", channels_path.display())),
                },
            };
            if !send(Ok(EpgGroupInfo { name: group.name, channel_count })) {
                return;
            }
        }
    })
}

pub(in crate::api) async fn playlist_epg_grid(
    ExtractAcceptHeader(accept): ExtractAcceptHeader,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(request): axum::extract::Json<EpgGridRequest>,
) -> axum::response::Response {
    let matcher = match ChannelMatcher::new(request.filter.as_ref()) {
        Ok(matcher) => matcher,
        Err(message) => return bad_search_request(&message),
    };
    let epg_path = match resolve_epg_path(&app_state, request.target_id) {
        Ok(epg_path) => epg_path,
        Err(status) => return status.into_response(),
    };
    let channels_path = epg_group_channels_path(&epg_path);
    if !file_exists_async(&channels_path).await || !file_exists_async(&epg_path).await {
        return axum::http::StatusCode::NO_CONTENT.into_response();
    }
    let channels_lock = app_state.app_config.file_locks.read_lock(&channels_path).await;
    let epg_lock = app_state.app_config.file_locks.read_lock(&epg_path).await;
    let config = app_state.app_config.config.load();
    let web_ui_path = config.web_ui.as_ref().and_then(|web_ui| web_ui.path.as_ref()).map_or("", String::as_str);
    let resource_url = concat_path_leading_slash(web_ui_path, "api/v1/playlist/resource");
    let encrypt_secret = app_state.get_encrypt_secret();
    let group = request.group;

    stream_from_blocking(accept.as_deref(), "EPG grid", move |send| {
        let _guards = (channels_lock, epg_lock);
        let mut index = match BPlusTreeQuery::<EpgGroupChannelKey, EpgGroupChannel>::try_new(&channels_path) {
            Ok(index) => index,
            Err(err) => return send_error(send, format!("Failed to open {}: {err}", channels_path.display())),
        };
        let mut epg = match BPlusTreeQuery::<Arc<str>, EpgChannel>::try_new(&epg_path) {
            Ok(epg) => epg,
            Err(err) => return send_error(send, format!("Failed to open {}: {err}", epg_path.display())),
        };
        let (start, end) = group_channel_range(&group);
        let mut sent = 0usize;
        for entry in index.range_iter(Bound::Included(&start), Bound::Included(&end)) {
            let channel = match entry {
                Ok((_, channel)) => channel,
                Err(err) => return send_error(send, format!("Failed to read {}: {err}", channels_path.display())),
            };
            // The search filters before the row limit, so matches past the limit are not lost.
            if !matcher.matches(&channel.name) {
                continue;
            }
            if sent == MAX_EPG_GRID_ROWS {
                return;
            }
            // Channels sharing an EPG key query it again; nothing is cached, to keep memory flat.
            let programmes = match epg.query(&channel.epg_key) {
                Ok(found) => found.map(to_grid_programmes).unwrap_or_default(),
                Err(err) => return send_error(send, format!("Failed to read {}: {err}", epg_path.display())),
            };
            let logo =
                rewrite_epg_resource_icon(&encrypt_secret, &resource_url, Some(channel.logo)).unwrap_or_default();
            let row = EpgGridRow {
                virtual_id: channel.virtual_id,
                name: channel.name,
                logo,
                epg_channel_id: channel.epg_key,
                programmes,
            };
            if !send(Ok(row)) {
                return;
            }
            sent += 1;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ConfigTarget, M3uTargetOutput, TargetOutput, XtreamTargetFlagsSet, XtreamTargetOutput};
    use arc_swap::ArcSwapOption;
    use shared::{
        foundation::Filter,
        model::{EpgProgramme, ProcessingOrder},
        utils::Internable,
    };

    fn target(output: Vec<TargetOutput>) -> ConfigTarget {
        ConfigTarget {
            curation: None,
            id: 1,
            enabled: true,
            name: "t".to_string(),
            options: None,
            sort: None,
            filter: Filter::default().into(),
            output,
            rename: None,
            mapping_ids: None,
            mapping: Arc::new(ArcSwapOption::new(None)),
            favourites: None,
            processing_order: ProcessingOrder::default(),
            execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
            watch: None,
            use_memory_cache: false,
        }
    }

    fn xtream() -> TargetOutput {
        TargetOutput::Xtream(XtreamTargetOutput { flags: XtreamTargetFlagsSet::new(), trakt: None, filter: None })
    }

    fn m3u() -> TargetOutput {
        TargetOutput::M3u(M3uTargetOutput {
            filename: None,
            include_type_in_url: false,
            mask_redirect_url: false,
            filter: None,
        })
    }

    #[test]
    fn output_type_prefers_xtream_then_m3u() {
        assert_eq!(epg_grid_output_type(&target(vec![m3u(), xtream()])), Some(TargetType::Xtream));
        assert_eq!(epg_grid_output_type(&target(vec![m3u()])), Some(TargetType::M3u));
        assert_eq!(epg_grid_output_type(&target(vec![])), None);
    }

    #[test]
    fn to_grid_programmes_keeps_only_time_and_title() {
        let mut channel = EpgChannel::new("x".intern());
        let mut programme = EpgProgramme::new(0, 60, "x".intern());
        programme.title = Some("T".intern());
        programme.desc = Some("long description".intern());
        channel.programmes = vec![programme];
        assert_eq!(
            to_grid_programmes(channel),
            vec![EpgGridProgrammeDto { start: 0, stop: 60, title: Some("T".intern()) }]
        );
    }

    #[test]
    fn channel_matcher_text_is_case_insensitive_and_empty_matches_all() {
        let matcher = ChannelMatcher::new(Some(&EpgChannelFilter::Text("ER".to_owned()))).expect("valid");
        assert!(matcher.matches("Das Erste"));
        assert!(!matcher.matches("ZDF"));
        assert!(ChannelMatcher::new(Some(&EpgChannelFilter::Text("  ".to_owned()))).expect("valid").is_all());
        assert!(ChannelMatcher::new(None).expect("valid").is_all());
    }

    #[test]
    fn channel_matcher_regexp_and_invalid_pattern() {
        let matcher = ChannelMatcher::new(Some(&EpgChannelFilter::Regexp("^ZDF".to_owned()))).expect("valid");
        assert!(matcher.matches("ZDF HD"));
        assert!(!matcher.matches("Das ZDF"));
        assert!(ChannelMatcher::new(Some(&EpgChannelFilter::Regexp("(".to_owned()))).is_err());
    }

    #[test]
    fn channel_matcher_rejects_oversized_regexp() {
        assert!(ChannelMatcher::new(Some(&EpgChannelFilter::Regexp("a{1000}{1000}".to_owned()))).is_err());
    }

    #[test]
    fn group_channel_range_covers_exactly_one_group() {
        let (start, end) = group_channel_range("News");
        let contains = |key: &EpgGroupChannelKey| key >= &start && key <= &end;
        assert!(contains(&("News".intern(), 0)));
        assert!(contains(&("News".intern(), 7)));
        assert!(!contains(&("Movies".intern(), u32::MAX)));
        assert!(!contains(&("Newsroom".intern(), 0)));
        assert!(!contains(&("News2".intern(), 0)));
    }

    mod routes {
        use super::{m3u, target, xtream};
        use crate::{
            api::{endpoints::v1_api_playlist, model::create_test_app_state},
            auth::create_jwt_web_user,
            model::{Config, ConfigSource, Epg, SourcesConfig, TargetOutput},
            repository::{
                epg_group_index_write, epg_groups_path, epg_write_for_target, get_target_storage_path,
                m3u_get_epg_file_path_for_target, xtream_get_epg_file_path_for_target, xtream_get_storage_path,
            },
            utils::EpgIdOutputCase,
        };
        use axum::{
            body::{to_bytes, Body},
            http::{Request, StatusCode},
            Router,
        };
        use shared::{
            model::{
                EpgChannel, EpgGridRow, EpgGroupInfo, EpgProgramme, Permission, PlaylistGroup, PlaylistItem,
                PlaylistItemHeader, UserId, VirtualId, WebAuthConfigDto, WebUiConfigDto, XtreamCluster,
                MAX_EPG_GRID_ROWS,
            },
            utils::{bin_deserialize, Internable},
        };
        use std::{collections::HashSet, sync::Arc};
        use tempfile::TempDir;
        use tower::ServiceExt;

        const TARGET_ID: u16 = 1;

        fn epg(channels: &[(&str, &str)]) -> Epg {
            Epg {
                priority: 0,
                logo_override: false,
                attributes: None,
                children: channels
                    .iter()
                    .map(|(id, title)| {
                        let mut programme = EpgProgramme::new(3600, 7200, id.intern());
                        programme.title = Some(title.intern());
                        Arc::new(EpgChannel { id: id.intern(), title: None, icon: None, programmes: vec![programme] })
                    })
                    .collect(),
            }
        }

        fn live_group(title: &str, channels: &[(u32, &str)]) -> PlaylistGroup {
            PlaylistGroup {
                id: 1,
                title: title.intern(),
                channels: channels
                    .iter()
                    .map(|(virtual_id, epg_id)| PlaylistItem {
                        header: PlaylistItemHeader {
                            virtual_id: VirtualId::new(*virtual_id),
                            name: format!("ch{virtual_id}").intern(),
                            epg_channel_id: Some(epg_id.intern()),
                            xtream_cluster: XtreamCluster::Live,
                            ..PlaylistItemHeader::default()
                        },
                    })
                    .collect(),
                xtream_cluster: XtreamCluster::Live,
            }
        }

        fn web_ui_config(tmp: &TempDir) -> Config {
            let web_ui = WebUiConfigDto {
                auth: Some(WebAuthConfigDto {
                    enabled: true,
                    issuer: "epg-grid-test".to_owned(),
                    secret: "epg-grid-test-secret".to_owned(),
                    ..Default::default()
                }),
                ..Default::default()
            };
            Config {
                storage_dir: tmp.path().to_string_lossy().into_owned(),
                web_ui: Some((&web_ui).into()),
                ..Default::default()
            }
        }

        /// Writes the target EPG (with group index when `playlist` is given) for every output.
        /// `epg_for` gives the EPG of each output, so outputs can differ.
        async fn app_with_target(
            tmp: &TempDir,
            outputs: Vec<TargetOutput>,
            epg_for: impl Fn(&TargetOutput) -> Epg,
            playlist: Option<&[PlaylistGroup]>,
        ) -> Arc<crate::api::model::AppState> {
            let config = web_ui_config(tmp);
            let mut target = target(outputs);
            target.id = TARGET_ID;
            let target = Arc::new(target);
            let target_path = get_target_storage_path(&config, &target.name).expect("target storage path");
            std::fs::create_dir_all(m3u_get_epg_file_path_for_target(&target_path).parent().expect("m3u dir"))
                .expect("create m3u dir");
            let xtream_storage = xtream_get_storage_path(&config, &target.name).expect("xtream storage path");
            std::fs::create_dir_all(&xtream_storage).expect("create xtream dir");
            for output in &target.output {
                epg_write_for_target(&config, &target, &target_path, Some(&epg_for(output)), output, playlist)
                    .await
                    .expect("write target epg");
            }
            let app_state = create_test_app_state(config);
            app_state.app_config.sources.store(Arc::new(SourcesConfig {
                sources: vec![ConfigSource { inputs: vec![], targets: vec![target] }],
                ..SourcesConfig::default()
            }));
            app_state
        }

        fn protected_router(app_state: Arc<crate::api::model::AppState>) -> Router {
            v1_api_playlist::v1_api_playlist_register_protected(Router::new()).with_state(app_state)
        }

        async fn post(
            router: Router,
            uri: &str,
            body: &str,
            accept: Option<&str>,
            token: Option<&str>,
        ) -> (StatusCode, Vec<u8>) {
            let mut request = Request::builder().method("POST").uri(uri).header("content-type", "application/json");
            if let Some(accept) = accept {
                request = request.header("accept", accept);
            }
            if let Some(token) = token {
                request = request.header("authorization", format!("Bearer {token}"));
            }
            let response =
                router.oneshot(request.body(Body::from(body.to_owned())).expect("request")).await.expect("response");
            let status = response.status();
            let body = to_bytes(response.into_body(), usize::MAX).await.expect("body").to_vec();
            (status, body)
        }

        fn grid_body(group: &str) -> String { format!(r#"{{"target_id":{TARGET_ID},"group":"{group}"}}"#) }

        fn groups_body() -> String { format!(r#"{{"target_id":{TARGET_ID}}}"#) }

        fn rows(body: &[u8]) -> Vec<(u32, Vec<String>)> {
            let rows: Vec<EpgGridRow> = serde_json::from_slice(body).expect("grid json");
            rows.into_iter()
                .map(|row| {
                    let titles = row.programmes.iter().map(|p| p.title.as_deref().unwrap_or_default().to_owned());
                    (row.virtual_id, titles.collect())
                })
                .collect()
        }

        fn sport_and_news() -> Vec<PlaylistGroup> {
            vec![live_group("Sport", &[(7, "b"), (8, "a")]), live_group("News", &[(3, "a")])]
        }

        #[tokio::test]
        async fn groups_and_grid_stream_in_playlist_order() {
            let tmp = TempDir::new().expect("temp dir");
            let playlist = sport_and_news();
            let app_state =
                app_with_target(&tmp, vec![xtream()], |_| epg(&[("a", "A show"), ("b", "B show")]), Some(&playlist))
                    .await;

            let (status, body) =
                post(protected_router(app_state.clone()), "/playlist/epg/groups", &groups_body(), None, None).await;
            assert_eq!(status, StatusCode::OK);
            let groups: Vec<EpgGroupInfo> = serde_json::from_slice(&body).expect("groups json");
            let groups: Vec<_> = groups.iter().map(|g| (g.name.to_string(), g.channel_count)).collect();
            assert_eq!(groups, vec![("Sport".to_string(), 2), ("News".to_string(), 1)]);

            let (status, body) =
                post(protected_router(app_state), "/playlist/epg/grid", &grid_body("Sport"), None, None).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(rows(&body), vec![(7, vec!["B show".to_string()]), (8, vec!["A show".to_string()])]);
        }

        fn groups_of(body: &[u8]) -> Vec<(String, u32)> {
            let groups: Vec<EpgGroupInfo> = serde_json::from_slice(body).expect("groups json");
            groups.iter().map(|g| (g.name.to_string(), g.channel_count)).collect()
        }

        #[tokio::test]
        async fn search_hides_groups_without_matches_and_counts_matches() {
            let tmp = TempDir::new().expect("temp dir");
            let playlist = sport_and_news();
            let app_state =
                app_with_target(&tmp, vec![xtream()], |_| epg(&[("a", "A show"), ("b", "B show")]), Some(&playlist))
                    .await;
            let search = |filter: &str| format!(r#"{{"target_id":{TARGET_ID},"filter":{filter}}}"#);

            let (status, body) = post(
                protected_router(app_state.clone()),
                "/playlist/epg/groups",
                &search(r#"{"Text":"ch3"}"#),
                None,
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(groups_of(&body), vec![("News".to_string(), 1)]);

            let (_, body) = post(
                protected_router(app_state.clone()),
                "/playlist/epg/groups",
                &search(r#"{"Text":"CH"}"#),
                None,
                None,
            )
            .await;
            assert_eq!(groups_of(&body), vec![("Sport".to_string(), 2), ("News".to_string(), 1)]);

            let (_, body) = post(
                protected_router(app_state),
                "/playlist/epg/groups",
                &search(r#"{"Regexp":"^ch[78]$"}"#),
                None,
                None,
            )
            .await;
            assert_eq!(groups_of(&body), vec![("Sport".to_string(), 2)]);
        }

        #[tokio::test]
        async fn search_filters_grid_rows() {
            let tmp = TempDir::new().expect("temp dir");
            let playlist = sport_and_news();
            let app_state =
                app_with_target(&tmp, vec![xtream()], |_| epg(&[("a", "A show"), ("b", "B show")]), Some(&playlist))
                    .await;
            let body = format!(r#"{{"target_id":{TARGET_ID},"group":"Sport","filter":{{"Text":"ch8"}}}}"#);
            let (status, body) = post(protected_router(app_state), "/playlist/epg/grid", &body, None, None).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(rows(&body), vec![(8, vec!["A show".to_string()])]);
        }

        #[tokio::test]
        async fn search_with_invalid_regexp_is_rejected() {
            let tmp = TempDir::new().expect("temp dir");
            let playlist = sport_and_news();
            let app_state = app_with_target(&tmp, vec![xtream()], |_| epg(&[("a", "A show")]), Some(&playlist)).await;
            for (uri, body) in [
                ("/playlist/epg/groups", format!(r#"{{"target_id":{TARGET_ID},"filter":{{"Regexp":"("}}}}"#)),
                (
                    "/playlist/epg/grid",
                    format!(r#"{{"target_id":{TARGET_ID},"group":"News","filter":{{"Regexp":"("}}}}"#),
                ),
            ] {
                let (status, _) = post(protected_router(app_state.clone()), uri, &body, None, None).await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
            }
        }

        #[tokio::test]
        async fn search_applies_before_row_limit() {
            let tmp = TempDir::new().expect("temp dir");
            let last = u32::try_from(MAX_EPG_GRID_ROWS + 5).expect("fits");
            let channels: Vec<(u32, &str)> = (1..=last).map(|id| (id, "a")).collect();
            let playlist = vec![live_group("Big", &channels)];
            let app_state = app_with_target(&tmp, vec![xtream()], |_| epg(&[("a", "A show")]), Some(&playlist)).await;
            let body = format!(r#"{{"target_id":{TARGET_ID},"group":"Big","filter":{{"Regexp":"^ch{last}$"}}}}"#);
            let (_, body) = post(protected_router(app_state), "/playlist/epg/grid", &body, None, None).await;
            assert_eq!(rows(&body), vec![(last, vec!["A show".to_string()])]);
        }

        #[tokio::test]
        async fn grid_streams_cbor_on_request() {
            let tmp = TempDir::new().expect("temp dir");
            let playlist = sport_and_news();
            let app_state =
                app_with_target(&tmp, vec![xtream()], |_| epg(&[("a", "A show"), ("b", "B show")]), Some(&playlist))
                    .await;
            let (_, json) =
                post(protected_router(app_state.clone()), "/playlist/epg/grid", &grid_body("Sport"), None, None).await;
            let (status, cbor) = post(
                protected_router(app_state),
                "/playlist/epg/grid",
                &grid_body("Sport"),
                Some("application/cbor"),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            let from_cbor: Vec<EpgGridRow> = bin_deserialize(&cbor).expect("grid cbor");
            let from_json: Vec<EpgGridRow> = serde_json::from_slice(&json).expect("grid json");
            assert_eq!(from_cbor, from_json);
        }

        #[tokio::test]
        async fn permission_router_requires_epg_read() {
            let tmp = TempDir::new().expect("temp dir");
            let playlist = sport_and_news();
            let app_state =
                app_with_target(&tmp, vec![xtream()], |_| epg(&[("a", "A show"), ("b", "B show")]), Some(&playlist))
                    .await;
            let config = app_state.app_config.config.load();
            let web_auth = config.web_ui.as_ref().and_then(|web_ui| web_ui.auth.as_ref()).expect("web auth");
            let reader = create_jwt_web_user(web_auth, "reader", Permission::EpgRead.into(), 0, UserId::from("web:r"))
                .expect("reader token");
            let other =
                create_jwt_web_user(web_auth, "other", Permission::PlaylistRead.into(), 0, UserId::from("web:o"))
                    .expect("other token");
            let router = || {
                v1_api_playlist::v1_api_playlist_register_with_permissions(Router::new(), &app_state)
                    .with_state(app_state.clone())
            };

            for uri in ["/playlist/epg/groups", "/playlist/epg/grid"] {
                let (status, _) = post(router(), uri, &grid_body("Sport"), None, Some(&reader)).await;
                assert_eq!(status, StatusCode::OK, "{uri} with EpgRead");
            }
            let (epg_status, _) = post(router(), "/playlist/epg", r#"{"Target":1}"#, None, Some(&other)).await;
            assert_ne!(epg_status, StatusCode::OK);
            for uri in ["/playlist/epg/groups", "/playlist/epg/grid"] {
                let (status, _) = post(router(), uri, &grid_body("Sport"), None, Some(&other)).await;
                assert_eq!(status, epg_status, "{uri} without EpgRead");
            }
        }

        #[tokio::test]
        async fn m3u_only_target_uses_m3u_index_and_epg() {
            let tmp = TempDir::new().expect("temp dir");
            let playlist = sport_and_news();
            let app_state =
                app_with_target(&tmp, vec![m3u()], |_| epg(&[("a", "A show"), ("b", "B show")]), Some(&playlist)).await;
            let (status, body) =
                post(protected_router(app_state), "/playlist/epg/grid", &grid_body("News"), None, None).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(rows(&body), vec![(3, vec!["A show".to_string()])]);
        }

        #[tokio::test]
        async fn dual_output_target_uses_xtream() {
            let tmp = TempDir::new().expect("temp dir");
            let playlist = sport_and_news();
            let app_state = app_with_target(
                &tmp,
                vec![m3u(), xtream()],
                |output| match output {
                    TargetOutput::Xtream(_) => epg(&[("a", "xtream A"), ("b", "xtream B")]),
                    _ => epg(&[("a", "m3u A"), ("b", "m3u B")]),
                },
                Some(&playlist),
            )
            .await;
            let (_, body) =
                post(protected_router(app_state), "/playlist/epg/grid", &grid_body("News"), None, None).await;
            assert_eq!(rows(&body), vec![(3, vec!["xtream A".to_string()])]);
        }

        #[tokio::test]
        async fn target_without_group_index_gives_no_content() {
            let tmp = TempDir::new().expect("temp dir");
            let app_state = app_with_target(&tmp, vec![xtream()], |_| epg(&[("a", "A show")]), None).await;
            for (uri, body) in [("/playlist/epg/groups", groups_body()), ("/playlist/epg/grid", grid_body("News"))] {
                let (status, _) = post(protected_router(app_state.clone()), uri, &body, None, None).await;
                assert_eq!(status, StatusCode::NO_CONTENT, "{uri}");
            }
        }

        #[tokio::test]
        async fn unknown_target_gives_not_found() {
            let tmp = TempDir::new().expect("temp dir");
            let app_state = app_with_target(&tmp, vec![xtream()], |_| epg(&[("a", "A show")]), None).await;
            let (status, _) = post(
                protected_router(app_state),
                "/playlist/epg/grid",
                r#"{"target_id":99,"group":"News"}"#,
                None,
                None,
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        }

        #[tokio::test]
        async fn grid_stops_at_max_rows() {
            let tmp = TempDir::new().expect("temp dir");
            let channels: Vec<(u32, &str)> =
                (1..=u32::try_from(MAX_EPG_GRID_ROWS + 5).expect("fits")).map(|id| (id, "a")).collect();
            let playlist = vec![live_group("Big", &channels)];
            let app_state = app_with_target(&tmp, vec![xtream()], |_| epg(&[("a", "A show")]), Some(&playlist)).await;
            let (_, body) =
                post(protected_router(app_state), "/playlist/epg/grid", &grid_body("Big"), None, None).await;
            assert_eq!(rows(&body).len(), MAX_EPG_GRID_ROWS);
        }

        #[tokio::test]
        async fn indexed_channel_missing_in_epg_gives_row_without_programmes() {
            let tmp = TempDir::new().expect("temp dir");
            let playlist = vec![live_group("News", &[(3, "a"), (4, "gone")])];
            let app_state = app_with_target(&tmp, vec![xtream()], |_| epg(&[("a", "A show")]), Some(&playlist)).await;
            // Rebuild the index as if "gone" had been in the EPG when it was built.
            let config = app_state.app_config.config.load();
            let storage = xtream_get_storage_path(&config, "t").expect("xtream storage");
            let epg_path = xtream_get_epg_file_path_for_target(&storage);
            let keys: HashSet<Arc<str>> = ["a".intern(), "gone".intern()].into_iter().collect();
            epg_group_index_write(&playlist, &keys, EpgIdOutputCase::Preserve, &epg_path).expect("index rewritten");
            assert!(epg_groups_path(&epg_path).exists());

            let (status, body) =
                post(protected_router(app_state), "/playlist/epg/grid", &grid_body("News"), None, None).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(rows(&body), vec![(3, vec!["A show".to_string()]), (4, vec![])]);
        }
    }
}
