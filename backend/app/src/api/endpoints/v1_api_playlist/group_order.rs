//! Playlist Explorer group order of one target cluster.
//!
//! The order is stored in `source.yml` as a managed `sort.rules` group rule of the target, so it
//! survives restarts and takes effect on the next playlist update. A save runs through the same
//! patch transaction as the expiry and alias tasks (lock, backup, in-place edit, one reload), and
//! the prepared configuration is updated as well because the config watcher ignores the server's
//! own writes.
//!
//! Only the rule this API writes is ever touched: the managed shape is the target `group`, the
//! field `group`, the order `none`, the cluster's own type filter and one anchored escaped literal
//! per group name. Every other rule of the target belongs to the user.

use super::recording::find_target_by_name;
use crate::{
    api::{
        model::AppState,
        source_yml_patch::{
            execute_target_group_order_patch_locked, stored_group_order_from_sort_dto, SourcesYmlPatch,
            TargetGroupOrderPatchError,
        },
    },
    model::ConfigTarget,
};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use log::error;
use serde::Deserialize;
use shared::model::{
    FieldError, OverrideFieldErrorsDto, TargetGroupCluster, TargetGroupOrderDto, TargetGroupOrderSaveRequest,
    TargetGroupOrderSaveResponse,
};
use std::{collections::HashSet, path::PathBuf, sync::Arc};

/// Upper bound of group names one stored order holds.
const MAX_GROUP_ORDER_ENTRIES: usize = 10_000;

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

/// Everything the group-order endpoints answer with.
#[derive(Debug)]
enum GroupOrderApiError {
    NotFound(String),
    /// `409`: the request cannot be applied to the file as it is now.
    Conflict(String),
    /// `422`: the request is well-formed but its names are not storable.
    FieldErrors(Vec<FieldError>),
    Internal(String),
}

impl GroupOrderApiError {
    fn conflict(message: impl Into<String>) -> Self { Self::Conflict(message.into()) }

    fn internal(message: impl Into<String>) -> Self { Self::Internal(message.into()) }
}

impl IntoResponse for GroupOrderApiError {
    fn into_response(self) -> Response {
        match self {
            Self::NotFound(message) => {
                (StatusCode::NOT_FOUND, axum::Json(serde_json::json!({ "error": message }))).into_response()
            }
            Self::Conflict(message) => {
                (StatusCode::CONFLICT, axum::Json(serde_json::json!({ "error": message }))).into_response()
            }
            // The Explorer parses field errors the same way for overrides and for the group order.
            Self::FieldErrors(field_errors) => {
                (StatusCode::UNPROCESSABLE_ENTITY, axum::Json(OverrideFieldErrorsDto { field_errors })).into_response()
            }
            Self::Internal(message) => {
                error!("{message}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(serde_json::json!({ "error": "Internal error; see the server log" })),
                )
                    .into_response()
            }
        }
    }
}

fn ok_json<T: serde::Serialize>(value: T) -> Response { axum::Json(value).into_response() }

// ---------------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------------

/// The configured target `name`; an unknown name answers `404` like every other playlist endpoint.
fn resolve_target(app_config: &crate::model::AppConfig, name: &str) -> Result<Arc<ConfigTarget>, GroupOrderApiError> {
    find_target_by_name(app_config, name)
        .ok_or_else(|| GroupOrderApiError::NotFound(format!("Unknown target '{name}'")))
}

/// Validates the requested group names while preserving each name exactly as received.
///
/// Trimming is used only to reject all-whitespace names; `" News "` and `"News"` remain distinct
/// groups because group matching is exact.
fn validate_group_order(groups: &[String]) -> Result<(), Vec<FieldError>> {
    if groups.len() > MAX_GROUP_ORDER_ENTRIES {
        return Err(vec![FieldError::new("groups", "error.too_many_groups")]);
    }
    let mut errors = Vec::new();
    let mut seen: HashSet<&str> = HashSet::with_capacity(groups.len());
    for group in groups {
        if group.trim().is_empty() {
            errors.push(FieldError::new("groups", "error.empty_group_name"));
        } else if !seen.insert(group.as_str()) {
            errors.push(FieldError::new("groups", "error.duplicate_group_name").with_entry(group));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Maps a failed patch transaction to the answer of the endpoint by error variant.
fn patch_error(error: TargetGroupOrderPatchError) -> GroupOrderApiError {
    match error {
        TargetGroupOrderPatchError::TargetNotFound(message) => GroupOrderApiError::NotFound(message),
        TargetGroupOrderPatchError::UnsupportedLayout(message)
        | TargetGroupOrderPatchError::ValidationFailed(message) => GroupOrderApiError::conflict(message),
        TargetGroupOrderPatchError::RevisionChanged => {
            GroupOrderApiError::conflict("source.yml changed while the order was saved; reload and repeat the change")
        }
        TargetGroupOrderPatchError::Other(err) => {
            GroupOrderApiError::internal(format!("Failed to store the group order in source.yml: {err}"))
        }
    }
}

/// The `source.yml` path of this deployment.
fn sources_path(app_state: &Arc<AppState>) -> Result<PathBuf, GroupOrderApiError> {
    let path = app_state.app_config.paths.load().sources_file_path.clone();
    if path.trim().is_empty() {
        return Err(GroupOrderApiError::internal("source.yml path is not configured"));
    }
    Ok(PathBuf::from(path))
}

// ---------------------------------------------------------------------------------------------
// GET /playlist/group_order
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(super) struct GroupOrderQuery {
    target: String,
    cluster: TargetGroupCluster,
}

async fn read_group_order(
    app_state: &Arc<AppState>,
    query: &GroupOrderQuery,
) -> Result<TargetGroupOrderDto, GroupOrderApiError> {
    resolve_target(&app_state.app_config, &query.target)?;
    let path = sources_path(app_state)?;
    let text = {
        let _guard = app_state.app_config.file_locks.read_lock(&path).await;
        tokio::fs::read_to_string(&path).await
    }
    .map_err(|err| GroupOrderApiError::internal(format!("Failed to read source.yml: {err}")))?;
    let doc: shared::model::SourcesConfigDto = shared::utils::yaml_from_str(&text)
        .map_err(|err| GroupOrderApiError::internal(format!("Failed to parse source.yml: {err}")))?;
    let target = doc
        .sources
        .iter()
        .flat_map(|source| source.targets.iter())
        .find(|target| target.name == query.target)
        .ok_or_else(|| GroupOrderApiError::NotFound(format!("Unknown target '{}'", query.target)))?;
    Ok(TargetGroupOrderDto { groups: stored_group_order_from_sort_dto(target.sort.as_ref(), query.cluster) })
}

pub(super) async fn playlist_group_order(
    State(app_state): State<Arc<AppState>>,
    Query(query): Query<GroupOrderQuery>,
) -> Response {
    match read_group_order(&app_state, &query).await {
        Ok(dto) => ok_json(dto),
        Err(err) => err.into_response(),
    }
}

// ---------------------------------------------------------------------------------------------
// PUT /playlist/group_order
// ---------------------------------------------------------------------------------------------

async fn save_group_order(
    app_state: &Arc<AppState>,
    request: TargetGroupOrderSaveRequest,
) -> Result<TargetGroupOrderSaveResponse, GroupOrderApiError> {
    validate_group_order(&request.groups).map_err(GroupOrderApiError::FieldErrors)?;
    resolve_target(&app_state.app_config, &request.target)?;
    let path = sources_path(app_state)?;
    let command = SourcesYmlPatch::SetTargetGroupOrder {
        target_name: Arc::from(request.target.as_str()),
        cluster: request.cluster,
        groups: request.groups.clone(),
    };

    let guard = app_state.app_config.file_locks.write_lock(&path).await;
    let written = execute_target_group_order_patch_locked(&app_state.app_config, &path, command, &guard)
        .await
        .map_err(patch_error)?;
    drop(guard);

    Ok(TargetGroupOrderSaveResponse { groups: request.groups, pending: written })
}

pub(super) async fn playlist_group_order_save(
    State(app_state): State<Arc<AppState>>,
    axum::extract::Json(request): axum::extract::Json<TargetGroupOrderSaveRequest>,
) -> Response {
    match save_group_order(&app_state, request).await {
        Ok(dto) => ok_json(dto),
        Err(err) => err.into_response(),
    }
}

#[cfg(test)]
mod tests {
    //! Tests of the group-order API.
    //!
    //! The tests mount the production router registrations from `routes.rs`, so the handlers, the
    //! permission split and the route table are exercised over `oneshot` requests, and they patch a
    //! real `source.yml` on disk.

    use super::{
        super::routes::{v1_api_playlist_register_protected, v1_api_playlist_register_with_permissions},
        MAX_GROUP_ORDER_ENTRIES,
    };
    use crate::{
        api::model::{create_test_app_state, AppState},
        auth::create_jwt_web_user,
        model::{Config, SourcesConfig},
    };
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
        Router,
    };
    use serde_json::{json, Value};
    use shared::model::{ConfigPaths, Permission, UserId, WebAuthConfigDto, WebUiConfigDto};
    use std::{path::PathBuf, sync::Arc};
    use tower::ServiceExt;

    const SOURCE_YML: &str = "\
inputs:
  - name: provider
    type: m3u
    url: http://provider.example/playlist.m3u
sources:
  - inputs: [provider]
    targets:
      - name: main
        output:
          - type: m3u
            filename: main.m3u
      - name: kids
        output:
          - type: m3u
            filename: kids.m3u
";

    struct Fixture {
        _dir: tempfile::TempDir,
        app_state: Arc<AppState>,
        router: Router,
        public_router: Router,
        source_path: PathBuf,
        reader: String,
        playlist_writer: String,
        source_writer: String,
    }

    fn config_paths(
        config_dir: &std::path::Path,
        storage_dir: &std::path::Path,
        source_path: &std::path::Path,
    ) -> ConfigPaths {
        ConfigPaths {
            home_path: String::new(),
            config_path: config_dir.to_string_lossy().into_owned(),
            storage_path: storage_dir.to_string_lossy().into_owned(),
            config_file_path: String::new(),
            sources_file_path: source_path.to_string_lossy().into_owned(),
            mapping_file_path: None,
            mapping_files_used: None,
            template_file_path: None,
            template_files_used: None,
            failover_file_path: None,
            api_proxy_file_path: String::new(),
            custom_stream_response_path: None,
        }
    }

    /// A target that stores its own channel rule next to the Explorer rule.
    const SOURCE_YML_WITH_RULES: &str = "\
inputs:
  - name: provider
    type: m3u
    url: http://provider.example/playlist.m3u
sources:
  - inputs: [provider]
    targets:
      - name: kids
        output:
          - type: m3u
            filename: kids.m3u
        sort:
          rules:
            - target: group
              field: group
              order: none
              filter: 'Type = live'
              sequence:
                - '^Kids$'
            - target: channel
              field: title
              order: asc
              filter: 'Type = live'
";

    async fn fixture() -> Fixture { fixture_with(SOURCE_YML).await }

    async fn fixture_with(source_yml: &str) -> Fixture {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().to_path_buf();
        let config_dir = root.join("config");
        let storage_dir = root.join("storage");
        let backup_dir = root.join("backup");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::create_dir_all(&storage_dir).expect("storage dir");
        let source_path = config_dir.join("source.yml");
        std::fs::write(&source_path, source_yml).expect("source.yml written");

        let web_ui = WebUiConfigDto {
            auth: Some(WebAuthConfigDto {
                enabled: true,
                issuer: "group-order-test".to_owned(),
                secret: "group-order-test-secret".to_owned(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let config = Config {
            web_ui: Some((&web_ui).into()),
            storage_dir: storage_dir.to_string_lossy().into_owned(),
            backup_dir: Some(backup_dir.to_string_lossy().into_owned()),
            ..Config::default()
        };
        let web_auth = config.web_ui.as_ref().and_then(|ui| ui.auth.as_ref()).expect("web auth");
        let token = |name: &str, permission: Permission| {
            create_jwt_web_user(web_auth, name, permission.into(), 0, UserId::from(format!("web:{name}").as_str()))
                .expect("jwt")
        };
        let reader = token("reader", Permission::PlaylistRead);
        let playlist_writer = token("playlist-writer", Permission::PlaylistWrite);
        let source_writer = token("source-writer", Permission::SourceWrite);

        let app_state = create_test_app_state(config);
        app_state.app_config.paths.store(Arc::new(config_paths(&config_dir, &storage_dir, &source_path)));

        // The runtime configuration is loaded the way the server loads it, so the prepared sort
        // rules of the target exist before the first request.
        let dto = crate::config_loader::read_sources_file_from_path_with_options(
            &source_path,
            crate::config_loader::ReadConfigOptions { resolve_env: false, include_computed: false },
            None,
            None,
        )
        .await
        .expect("prepared sources");
        app_state.app_config.sources.store(Arc::new(SourcesConfig::try_from(&dto).expect("runtime sources")));

        let router =
            v1_api_playlist_register_with_permissions(Router::new(), &app_state).with_state(Arc::clone(&app_state));
        let public_router = v1_api_playlist_register_protected(Router::new()).with_state(Arc::clone(&app_state));
        Fixture { _dir: dir, app_state, router, public_router, source_path, reader, playlist_writer, source_writer }
    }

    impl Fixture {
        async fn call(&self, method: &str, uri: &str, token: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
            self.call_on(&self.router, method, uri, token, body).await
        }

        async fn call_on(
            &self,
            router: &Router,
            method: &str,
            uri: &str,
            token: Option<&str>,
            body: Option<Value>,
        ) -> (StatusCode, Value) {
            let mut request = Request::builder().method(method).uri(uri);
            if let Some(token) = token {
                request = request.header("Authorization", format!("Bearer {token}"));
            }
            let body = match body {
                Some(body) => {
                    request = request.header("content-type", "application/json");
                    Body::from(serde_json::to_vec(&body).expect("json body"))
                }
                None => Body::empty(),
            };
            let response = router.clone().oneshot(request.body(body).expect("request")).await.expect("response");
            let status = response.status();
            let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024).await.expect("body");
            (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
        }

        async fn get(&self, uri: &str) -> (StatusCode, Value) { self.call("GET", uri, Some(&self.reader), None).await }

        async fn put(&self, body: Value) -> (StatusCode, Value) {
            self.call("PUT", "/playlist/group_order", Some(&self.source_writer), Some(body)).await
        }

        fn source_text(&self) -> String { std::fs::read_to_string(&self.source_path).expect("source.yml") }

        /// The order the prepared runtime configuration holds for `cluster`.
        fn runtime_order(&self, target_name: &str, cluster: shared::model::TargetGroupCluster) -> Vec<String> {
            use crate::api::source_yml_patch::decode_group_sequence_entry;
            use shared::model::{ItemField, SortOrder, SortTarget, TargetGroupCluster};
            let filter_str = match cluster {
                TargetGroupCluster::Live => "Type = live",
                TargetGroupCluster::Vod => "Type = movie",
                TargetGroupCluster::Series => "Type = series",
            };
            let sources = self.app_state.app_config.sources.load();
            let target = sources.get_target_by_name(target_name).expect("target");
            let Some(sort) = target.sort.as_ref() else {
                return Vec::new();
            };
            let rule = sort
                .rules
                .iter()
                .find(|rule| {
                    rule.target == SortTarget::Group
                        && rule.field == ItemField::Group
                        && rule.order == SortOrder::None
                        && !rule.natural
                        && rule.filter.to_string().eq_ignore_ascii_case(filter_str)
                })
                .expect("managed rule");
            rule.sequence
                .as_ref()
                .expect("sequence")
                .iter()
                .filter_map(|regex| decode_group_sequence_entry(regex.as_str()))
                .collect()
        }
    }

    #[tokio::test]
    async fn put_stores_the_order_and_get_reads_it_back() {
        let fixture = fixture().await;
        let (status, body) =
            fixture.put(json!({"target": "kids", "cluster": "live", "groups": ["Kids", "Cartoons", "Movies"]})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["groups"], json!(["Kids", "Cartoons", "Movies"]));
        assert_eq!(body["pending"], json!(true));

        let text = fixture.source_text();
        assert!(text.contains("        sort:\n          rules:\n            - target: group\n"), "{text}");
        assert!(text.contains("              filter: Type = live\n"), "{text}");
        assert!(
            text.contains("                - ^Kids$\n                - ^Cartoons$\n                - ^Movies$\n"),
            "{text}"
        );

        // The prepared configuration the next playlist update reads carries the new order.
        assert_eq!(
            fixture.runtime_order("kids", shared::model::TargetGroupCluster::Live),
            ["Kids", "Cartoons", "Movies"]
        );

        let (status, body) = fixture.get("/playlist/group_order?target=kids&cluster=live").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["groups"], json!(["Kids", "Cartoons", "Movies"]));
    }

    #[tokio::test]
    async fn put_is_forbidden_without_source_write() {
        let fixture = fixture().await;
        let body = json!({"target": "kids", "cluster": "live", "groups": ["Kids"]});
        let (status, _) = fixture.call("PUT", "/playlist/group_order", Some(&fixture.reader), Some(body.clone())).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) =
            fixture.call("PUT", "/playlist/group_order", Some(&fixture.playlist_writer), Some(body)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "playlist write does not include source.yml");
        assert_eq!(fixture.source_text(), SOURCE_YML, "a rejected save must not touch the file");
    }

    #[tokio::test]
    async fn get_requires_playlist_read() {
        let fixture = fixture().await;
        let (status, _) = fixture
            .call("GET", "/playlist/group_order?target=kids&cluster=live", Some(&fixture.source_writer), None)
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn unknown_target_answers_not_found() {
        let fixture = fixture().await;
        let (status, _) = fixture.get("/playlist/group_order?target=missing&cluster=live").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = fixture.put(json!({"target": "missing", "cluster": "live", "groups": ["Kids"]})).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn duplicate_and_empty_names_answer_field_errors() {
        let fixture = fixture().await;
        let (status, body) =
            fixture.put(json!({"target": "kids", "cluster": "live", "groups": ["Kids", "Kids", "News"]})).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["field_errors"][0]["field"], json!("groups"));
        assert_eq!(body["field_errors"][0]["message_key"], json!("error.duplicate_group_name"));
        assert_eq!(body["field_errors"][0]["entry"], json!("Kids"));

        let (status, body) = fixture.put(json!({"target": "kids", "cluster": "live", "groups": ["Kids", "  "]})).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["field_errors"][0]["message_key"], json!("error.empty_group_name"));
        assert_eq!(fixture.source_text(), SOURCE_YML, "a rejected save must not touch the file");
    }

    #[tokio::test]
    async fn too_many_names_answer_field_errors() {
        let fixture = fixture().await;
        let mut groups = (0..=MAX_GROUP_ORDER_ENTRIES).map(|index| format!("Group {index}")).collect::<Vec<_>>();
        groups[0] = String::new();
        groups[1] = "Group 2".to_string();
        let (status, body) = fixture.put(json!({"target": "kids", "cluster": "live", "groups": groups})).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["field_errors"].as_array().map(Vec::len), Some(1), "{body}");
        assert_eq!(body["field_errors"][0]["message_key"], json!("error.too_many_groups"));
    }

    #[tokio::test]
    async fn group_names_with_template_delimiters_store_reload_and_match_literal_title() {
        let yaml = format!(
            "\
templates:
  - name: HD
    value: '1080p'
{SOURCE_YML}"
        );
        let fixture = fixture_with(&yaml).await;
        let groups = ["Sports !HD!", "!custom!", "Sports!", "!News"];
        let (status, body) = fixture.put(json!({"target": "kids", "cluster": "live", "groups": groups})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["groups"], json!(groups));
        assert_eq!(body["pending"], json!(true));

        let text = fixture.source_text();
        assert!(!text.contains("^Sports !HD!$"), "stored pattern must not contain raw !HD!: {text}");
        assert!(text.contains(r"\x21HD\x21"), "stored pattern must encode ! as \\x21: {text}");

        // Reload from disk with templates prepared (same path as server reload) and verify literal match.
        let mut paths = (*fixture.app_state.app_config.paths.load_full()).clone();
        paths.sources_file_path = fixture.source_path.to_string_lossy().into_owned();
        let config = fixture.app_state.app_config.config.load();
        let templates = crate::config_loader::load_prepared_global_templates(&paths, &config).await.expect("templates");
        let reloaded_dto = crate::config_loader::read_sources_file_from_path_with_options(
            &fixture.source_path,
            crate::config_loader::ReadConfigOptions { resolve_env: true, include_computed: false },
            None,
            templates.as_deref(),
        )
        .await
        .expect("reloaded sources");
        let reloaded = SourcesConfig::try_from(&reloaded_dto).expect("runtime sources");
        let sort = reloaded.get_target_by_name("kids").expect("target").sort.as_ref().expect("sort");
        let seq = sort.rules[0].sequence.as_ref().expect("sequence");
        assert!(seq[0].is_match("Sports !HD!"), "must match literal group title");
        assert!(!seq[0].is_match("Sports 1080p"), "must not expand !HD! template");
        assert!(seq[1].is_match("!custom!"));
        assert!(seq[2].is_match("Sports!"));
        assert!(seq[3].is_match("!News"));

        assert_eq!(fixture.runtime_order("kids", shared::model::TargetGroupCluster::Live), groups);
        let (status, body) = fixture.get("/playlist/group_order?target=kids&cluster=live").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["groups"], json!(groups));
    }

    #[tokio::test]
    async fn patch_error_maps_variants_to_expected_http_statuses() {
        use axum::response::IntoResponse;
        let cases = [
            (
                super::TargetGroupOrderPatchError::TargetNotFound("missing target".to_string()),
                StatusCode::NOT_FOUND,
                Some("missing target"),
            ),
            (
                super::TargetGroupOrderPatchError::UnsupportedLayout("bad layout".to_string()),
                StatusCode::CONFLICT,
                Some("bad layout"),
            ),
            (
                super::TargetGroupOrderPatchError::ValidationFailed("validation failed".to_string()),
                StatusCode::CONFLICT,
                Some("validation failed"),
            ),
            (
                super::TargetGroupOrderPatchError::RevisionChanged,
                StatusCode::CONFLICT,
                Some("source.yml changed while the order was saved; reload and repeat the change"),
            ),
            (
                super::TargetGroupOrderPatchError::Other(shared::error::TuliproxError::Io("disk full")),
                StatusCode::INTERNAL_SERVER_ERROR,
                None,
            ),
        ];
        for (err, expected_status, expected_msg) in cases {
            let response = super::patch_error(err).into_response();
            assert_eq!(response.status(), expected_status);
            let bytes = to_bytes(response.into_body(), 64 * 1024).await.expect("body");
            let json: Value = serde_json::from_slice(&bytes).expect("json");
            if let Some(msg) = expected_msg {
                assert_eq!(json["error"], json!(msg));
            }
        }
    }

    #[tokio::test]
    async fn user_rule_with_field_title_is_never_returned_or_changed_and_runtime_matches_reload() {
        let yaml = "\
inputs:
  - name: provider
    type: m3u
    url: http://provider.example/playlist.m3u
sources:
  - inputs: [provider]
    targets:
      - name: kids
        output:
          - type: m3u
            filename: kids.m3u
        sort:
          rules:
            - target: group
              field: title
              order: none
              filter: 'Type = live'
              sequence:
                - '^UserGroup$'
";
        let fixture = fixture_with(yaml).await;
        // Prepare runtime like a real server reload so `prepare()` normalizes `field: title` -> `Group`.
        let reloaded_dto = crate::config_loader::read_sources_file_from_path_with_options(
            &fixture.source_path,
            crate::config_loader::ReadConfigOptions { resolve_env: true, include_computed: false },
            None,
            None,
        )
        .await
        .expect("prepared sources");
        fixture.app_state.app_config.sources.store(Arc::new(SourcesConfig::try_from(&reloaded_dto).expect("runtime")));

        let (status, body) = fixture.get("/playlist/group_order?target=kids&cluster=live").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["groups"], json!([]), "a rule with field: title is never managed");

        let (status, body) = fixture.put(json!({"target": "kids", "cluster": "live", "groups": ["Kids"]})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["pending"], json!(true));

        let (status, body) = fixture.get("/playlist/group_order?target=kids&cluster=live").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["groups"], json!(["Kids"]));

        let text = fixture.source_text();
        assert!(text.contains("field: title"), "user rule keeps field: title in YAML: {text}");
        assert!(text.contains("'^UserGroup$'") || text.contains("- ^UserGroup$"), "{text}");

        // Runtime target sort must match a fresh reload.
        let fresh_dto = crate::config_loader::read_sources_file_from_path_with_options(
            &fixture.source_path,
            crate::config_loader::ReadConfigOptions { resolve_env: true, include_computed: false },
            None,
            None,
        )
        .await
        .expect("fresh reload");
        let fresh_sources = SourcesConfig::try_from(&fresh_dto).expect("fresh runtime");
        let runtime_sources = fixture.app_state.app_config.sources.load();
        let runtime_sort = runtime_sources.get_target_by_name("kids").expect("target").sort.as_ref().expect("sort");
        let fresh_sort = fresh_sources.get_target_by_name("kids").expect("target").sort.as_ref().expect("sort");
        assert_eq!(runtime_sort.rules.len(), fresh_sort.rules.len());
        assert_eq!(runtime_sort.rules.len(), 2);
        assert_eq!(
            runtime_sort.rules[1].sequence.as_ref().map(|s| s.iter().map(|r| r.as_str()).collect::<Vec<_>>()),
            fresh_sort.rules[1].sequence.as_ref().map(|s| s.iter().map(|r| r.as_str()).collect::<Vec<_>>())
        );
    }

    #[tokio::test]
    async fn unsupported_layouts_return_conflict_and_leave_file_unchanged() {
        let base = "\
inputs:
  - name: provider
    type: m3u
    url: http://provider.example/playlist.m3u
sources:
  - inputs: [provider]
    targets:
      - name: kids
        output:
          - type: m3u
            filename: kids.m3u
";
        let cases = [
            ("flow-style sort", format!("{base}        sort: {{rules: [{{target: channel, field: title, order: asc, filter: 'Type = live'}}]}}\n")),
            ("flow-style rules", format!("{base}        sort:\n          rules: [{{target: channel, field: title, order: asc, filter: 'Type = live'}}]\n")),
            ("inline comment on sort", format!("{base}        sort: # keep comment\n          rules:\n            - target: channel\n              field: title\n              order: asc\n              filter: 'Type = live'\n")),
            ("null sort", format!("{base}        sort: null\n")),
            ("inline flow target", "\
inputs:
  - name: provider
    type: m3u
    url: http://provider.example/playlist.m3u
sources:
  - inputs: [provider]
    targets:
      - {name: kids, output: [{type: m3u, filename: kids.m3u}]}
".to_string()),
        ];

        for (label, yaml) in cases {
            let fixture = fixture_with(&yaml).await;
            let before = std::fs::read(&fixture.source_path).expect("before bytes");
            let (status, body) = fixture.put(json!({"target": "kids", "cluster": "live", "groups": ["Kids"]})).await;
            assert_eq!(status, StatusCode::CONFLICT, "{label}: {body}");
            assert_eq!(
                body["error"],
                json!("the target's sort block uses a layout that cannot be edited automatically; edit sort.rules in source.yml by hand"),
                "{label}: {body}"
            );
            let after = std::fs::read(&fixture.source_path).expect("after bytes");
            assert_eq!(after, before, "{label}: file must remain unchanged byte for byte");
        }
    }

    #[tokio::test]
    async fn noop_on_unsupported_layout_returns_conflict_and_leaves_file_unchanged() {
        let base = "\
inputs:
  - name: provider
    type: m3u
    url: http://provider.example/playlist.m3u
sources:
  - inputs: [provider]
    targets:
      - name: kids
        output:
          - type: m3u
            filename: kids.m3u
";
        let cases = [
            (
                "empty order on flow-style sort",
                format!("{base}        sort: {{rules: [{{target: channel, field: title, order: asc, filter: 'Type = live'}}]}}\n"),
                json!([]),
            ),
            (
                "unchanged order on flow-style rules",
                format!("{base}        sort:\n          rules: [{{target: group, field: group, order: none, filter: 'Type = live', sequence: ['^Kids$']}}]\n"),
                json!(["Kids"]),
            ),
            ("empty order on null sort", format!("{base}        sort: null\n"), json!([])),
            (
                "empty order on inline flow target",
                "\
inputs:
  - name: provider
    type: m3u
    url: http://provider.example/playlist.m3u
sources:
  - inputs: [provider]
    targets:
      - {name: kids, output: [{type: m3u, filename: kids.m3u}]}
"
                .to_string(),
                json!([]),
            ),
        ];

        for (label, yaml, groups) in cases {
            let fixture = fixture_with(&yaml).await;
            let before = std::fs::read(&fixture.source_path).expect("before bytes");
            let (status, body) = fixture.put(json!({"target": "kids", "cluster": "live", "groups": groups})).await;
            assert_eq!(status, StatusCode::CONFLICT, "{label}: {body}");
            assert_eq!(
                body["error"],
                json!("the target's sort block uses a layout that cannot be edited automatically; edit sort.rules in source.yml by hand"),
                "{label}: {body}"
            );
            let after = std::fs::read(&fixture.source_path).expect("after bytes");
            assert_eq!(after, before, "{label}: file must remain unchanged byte for byte");
        }
    }

    #[tokio::test]
    async fn an_empty_order_removes_the_rule_again() {
        let fixture = fixture().await;
        let (status, _) = fixture.put(json!({"target": "kids", "cluster": "live", "groups": ["Kids"]})).await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = fixture.put(json!({"target": "kids", "cluster": "live", "groups": []})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["pending"], json!(true));
        assert!(!fixture.source_text().contains("sort:"), "{}", fixture.source_text());
        assert_eq!(fixture.runtime_order("kids", shared::model::TargetGroupCluster::Live), Vec::<String>::new());

        let (status, body) = fixture.get("/playlist/group_order?target=kids&cluster=live").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["groups"], json!([]));
    }

    #[tokio::test]
    async fn an_empty_order_keeps_the_rule_of_the_user() {
        let fixture = fixture_with(SOURCE_YML_WITH_RULES).await;
        let (status, body) = fixture.get("/playlist/group_order?target=kids&cluster=live").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["groups"], json!(["Kids"]));

        let (status, body) = fixture.put(json!({"target": "kids", "cluster": "live", "groups": []})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["pending"], json!(true));
        let text = fixture.source_text();
        assert!(
            text.contains("            - target: channel\n              field: title\n              order: asc\n"),
            "{text}"
        );
        assert!(!text.contains("sequence"), "{text}");

        let (status, body) = fixture.get("/playlist/group_order?target=kids&cluster=live").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["groups"], json!([]));
    }

    #[tokio::test]
    async fn a_second_identical_save_is_not_pending() {
        let fixture = fixture().await;
        let body = json!({"target": "kids", "cluster": "vod", "groups": ["Movies"]});
        let (status, _) = fixture.put(body.clone()).await;
        assert_eq!(status, StatusCode::OK);
        let before = std::fs::read(&fixture.source_path).expect("before bytes");
        let (status, body) = fixture.put(body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["pending"], json!(false), "an unchanged order needs no playlist update");
        assert_eq!(std::fs::read(&fixture.source_path).expect("after bytes"), before);
        assert!(fixture.source_text().contains("filter: Type = vod"), "{}", fixture.source_text());
    }

    #[tokio::test]
    async fn the_auth_disabled_registration_serves_both_methods() {
        let fixture = fixture().await;
        let (status, body) = fixture
            .call_on(
                &fixture.public_router,
                "PUT",
                "/playlist/group_order",
                None,
                Some(json!({"target": "kids", "cluster": "series", "groups": ["Shows"]})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = fixture
            .call_on(&fixture.public_router, "GET", "/playlist/group_order?target=kids&cluster=series", None, None)
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["groups"], json!(["Shows"]));
    }

    #[tokio::test]
    async fn invalid_user_regex_in_target_aborts_before_write_and_returns_conflict() {
        let yaml = "\
inputs:
  - name: provider
    type: m3u
    url: http://provider.example/playlist.m3u
sources:
  - inputs: [provider]
    targets:
      - name: kids
        output:
          - type: m3u
            filename: kids.m3u
        sort:
          rules:
            - target: channel
              field: title
              order: asc
              filter: 'Group ~ \"[unclosed\"'
";
        let fixture = fixture_with(yaml).await;
        let before = std::fs::read(&fixture.source_path).expect("before bytes");
        let (status, body) = fixture.put(json!({"target": "kids", "cluster": "live", "groups": ["Kids"]})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body["error"].as_str().is_some_and(|msg| msg.contains("unclosed")), "{body}");
        let after = std::fs::read(&fixture.source_path).expect("after bytes");
        assert_eq!(after, before, "a prepare failure must leave source.yml byte-identical");
    }

    #[tokio::test]
    async fn env_var_in_user_rule_filter_resolves_for_prepare_and_runtime_matches_reload() {
        const ENV_VAR: &str = "TULIPROX_GROUP_ORDER_ENV_FILTER_TEST";
        std::env::set_var(ENV_VAR, "News");
        let yaml = format!(
            "\
inputs:
  - name: provider
    type: m3u
    url: http://provider.example/playlist.m3u
sources:
  - inputs: [provider]
    targets:
      - name: kids
        output:
          - type: m3u
            filename: kids.m3u
        sort:
          rules:
            - target: channel
              field: title
              order: asc
              filter: 'Group ~ \"${{env:{ENV_VAR}}}\"'
"
        );
        let fixture = fixture_with(&yaml).await;
        let (status, body) = fixture.put(json!({"target": "kids", "cluster": "live", "groups": ["Kids"]})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(fixture.source_text().contains("${env:TULIPROX_GROUP_ORDER_ENV_FILTER_TEST}"));

        let fresh_dto = crate::config_loader::read_sources_file_from_path_with_options(
            &fixture.source_path,
            crate::config_loader::ReadConfigOptions { resolve_env: true, include_computed: false },
            None,
            None,
        )
        .await
        .expect("fresh reload");
        let fresh_sources = SourcesConfig::try_from(&fresh_dto).expect("fresh runtime");
        let runtime_sources = fixture.app_state.app_config.sources.load();
        let runtime_sort = runtime_sources.get_target_by_name("kids").expect("target").sort.as_ref().expect("sort");
        let fresh_sort = fresh_sources.get_target_by_name("kids").expect("target").sort.as_ref().expect("sort");
        assert_eq!(shared::model::ConfigSortDto::from(runtime_sort), shared::model::ConfigSortDto::from(fresh_sort));
    }

    #[tokio::test]
    async fn accepts_2500_group_names() {
        let fixture = fixture().await;
        let groups: Vec<String> = (0..2500).map(|index| format!("Group {index}")).collect();
        let (status, body) = fixture.put(json!({"target": "kids", "cluster": "vod", "groups": groups})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["groups"], json!(groups));

        let (status, body) = fixture.get("/playlist/group_order?target=kids&cluster=vod").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["groups"], json!(groups));
    }

    #[tokio::test]
    async fn preserves_leading_and_trailing_spaces_as_distinct_groups() {
        let fixture = fixture().await;
        let groups = [" News ", "News"];
        let (status, body) = fixture.put(json!({"target": "kids", "cluster": "live", "groups": groups})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["groups"], json!(groups));
        assert_eq!(fixture.runtime_order("kids", shared::model::TargetGroupCluster::Live), groups);

        let (status, body) = fixture.get("/playlist/group_order?target=kids&cluster=live").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["groups"], json!(groups));
    }

    #[tokio::test]
    async fn broken_source_yml_elsewhere_returns_parse_error_instead_of_masking() {
        let broken_with_valid_target = "\
inputs:
  - name: provider
    type: m3u
    url: http://provider.example/playlist.m3u
    unknown_field_that_breaks_parse: true
sources:
  - inputs: [provider]
    targets:
      - name: kids
        output:
          - type: m3u
            filename: kids.m3u
";
        let broken_with_unsupported_sort = format!("{broken_with_valid_target}        sort: null\n");

        for (label, broken_yaml) in [
            ("valid target", broken_with_valid_target),
            ("unsupported target sort", broken_with_unsupported_sort.as_str()),
        ] {
            let fixture = fixture().await;
            std::fs::write(&fixture.source_path, broken_yaml).expect("write broken source.yml");
            let before = std::fs::read(&fixture.source_path).expect("before bytes");
            let (status, body) = fixture.put(json!({"target": "kids", "cluster": "live", "groups": ["Kids"]})).await;
            assert_eq!(status, StatusCode::CONFLICT, "{label}: {body}");
            assert!(
                body["error"].as_str().is_some_and(|msg| msg.contains("failed to parse source.yml:")),
                "{label}: {body}"
            );
            assert_eq!(std::fs::read(&fixture.source_path).expect("after bytes"), before, "{label}");
        }
    }
}
