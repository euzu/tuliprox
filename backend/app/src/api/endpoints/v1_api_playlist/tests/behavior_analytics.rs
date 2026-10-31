use crate::{
    api::{
        endpoints::v1_api_playlist::{
            analytics::{compute_target_next_scheduled, parse_playlist_schedule_next_runs},
            v1_api_playlist_register_with_permissions,
        },
        model::{create_test_app_state, AppState},
    },
    auth::create_jwt_web_user,
    model::{
        Config, ConfigSource, ConfigTarget, M3uTargetOutput, ScheduleConfig, SourcesConfig, TargetOutput,
        XtreamTargetFlagsSet, XtreamTargetOutput,
    },
    repository::{ensure_target_storage_path, save_target_analytics, save_target_analytics_unmapped_csv},
};
use arc_swap::ArcSwapOption;
use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
    response::Response,
    Router,
};
use chrono::{DateTime, FixedOffset};
use shared::{
    foundation::Filter,
    model::{
        ClusterAnalytics, Permission, ProcessingOrder, ScheduleTaskType, TargetAnalyticsDto,
        TargetAnalyticsResponseDto, TargetType, UserId, WebAuthConfigDto, WebUiConfigDto, TARGET_ANALYTICS_VERSION,
    },
};
use std::sync::Arc;
use tower::ServiceExt;

struct Fixture {
    _dir: tempfile::TempDir,
    _app_state: Arc<AppState>,
    router: Router,
    reader: String,
    epg_only: String,
}

fn target(id: u16, name: &str, output: TargetOutput) -> Arc<ConfigTarget> {
    Arc::new(ConfigTarget {
        curation: None,
        id,
        enabled: true,
        name: name.to_string(),
        options: None,
        sort: None,
        filter: Filter::default().into(),
        output: vec![output],
        rename: None,
        mapping_ids: None,
        mapping: Arc::new(ArcSwapOption::new(None)),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
    })
}

fn m3u_output() -> TargetOutput {
    TargetOutput::M3u(M3uTargetOutput {
        filename: None,
        include_type_in_url: false,
        mask_redirect_url: false,
        filter: None,
    })
}

fn xtream_output() -> TargetOutput {
    TargetOutput::Xtream(XtreamTargetOutput { flags: XtreamTargetFlagsSet::new(), trakt: None, filter: None })
}

async fn setup_fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("temp dir");
    let web_ui = WebUiConfigDto {
        auth: Some(WebAuthConfigDto {
            enabled: true,
            issuer: "analytics-test".to_owned(),
            secret: "analytics-test-secret".to_owned(),
            ..Default::default()
        }),
        ..Default::default()
    };
    let schedules = vec![
        ScheduleConfig {
            schedule: "0 0 12 * * * *".to_string(),
            task_type: ScheduleTaskType::PlaylistUpdate,
            targets: Some(vec!["target-alpha".to_string()]),
        },
        ScheduleConfig {
            schedule: "0 0 18 * * * *".to_string(),
            task_type: ScheduleTaskType::PlaylistUpdate,
            targets: None,
        },
        ScheduleConfig {
            schedule: "0 0 6 * * * *".to_string(),
            task_type: ScheduleTaskType::LibraryScan,
            targets: None,
        },
    ];
    let config = Config {
        web_ui: Some((&web_ui).into()),
        storage_dir: dir.path().to_string_lossy().into_owned(),
        schedules: Some(schedules),
        ..Config::default()
    };
    let web_auth = config.web_ui.as_ref().and_then(|ui| ui.auth.as_ref()).expect("web auth");
    let token = |name: &str, permission: Permission| {
        create_jwt_web_user(web_auth, name, permission.into(), 0, UserId::from(format!("web:{name}").as_str()))
            .expect("jwt")
    };
    let reader = token("reader", Permission::PlaylistRead);
    let epg_only = token("epg_only", Permission::EpgRead);

    let app_state = create_test_app_state(config);
    let sources = SourcesConfig {
        sources: vec![ConfigSource {
            inputs: vec![],
            targets: vec![
                target(1, "target-alpha", m3u_output()),
                target(2, "target-beta", xtream_output()),
                target(3, "target-gamma", m3u_output()),
            ],
        }],
        ..SourcesConfig::default()
    };
    app_state.app_config.sources.store(Arc::new(sources));

    // Target 1: valid analytics and unmapped CSV
    let cfg = app_state.app_config.config.load_full();
    let alpha_path = ensure_target_storage_path(&cfg, "target-alpha").await.expect("alpha storage");
    let alpha_dto = TargetAnalyticsDto {
        version: TARGET_ANALYTICS_VERSION,
        success: true,
        updated_at: 1_700_000_000,
        took_secs: 3,
        inputs: vec!["provider1".to_string()],
        live: ClusterAnalytics { count: 42, groups: 4, input_count: 50 },
        vod: ClusterAnalytics::default(),
        series: ClusterAnalytics::default(),
        episodes: None,
        episodes_complete: false,
        catchup_channels: 5,
        catchup_max_days: Some(3),
        top_live_groups: vec![],
        epg: None,
    };
    save_target_analytics(&alpha_path, &alpha_dto).await.expect("save alpha analytics");
    save_target_analytics_unmapped_csv(&alpha_path, "group,name,epg_channel_id,input\nNews,BBC One,bbc.one,p1\n")
        .await
        .expect("save alpha unmapped csv");

    // Target 2: left empty (missing analytics and missing csv)
    let _ = ensure_target_storage_path(&cfg, "target-beta").await.expect("beta storage");

    // Target 3: corrupt analytics.json
    let gamma_path = ensure_target_storage_path(&cfg, "target-gamma").await.expect("gamma storage");
    std::fs::write(gamma_path.join("analytics.json"), b"invalid json content").expect("write corrupt json");

    let router =
        v1_api_playlist_register_with_permissions(Router::new(), &app_state).with_state(Arc::clone(&app_state));
    Fixture { _dir: dir, _app_state: app_state, router, reader, epg_only }
}

impl Fixture {
    async fn call(&self, uri: &str, token: Option<&str>) -> Response {
        let mut request = Request::builder().method("GET").uri(uri);
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        self.router.clone().oneshot(request.body(Body::empty()).expect("request")).await.expect("response")
    }

    async fn get(&self, uri: &str) -> Response { self.call(uri, Some(&self.reader)).await }
}

#[tokio::test]
async fn playlist_analytics_lists_all_targets_with_analytics_and_schedules() {
    let fixture = setup_fixture().await;
    let response = fixture.get("/playlist/analytics").await;
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.expect("body");
    let dto: TargetAnalyticsResponseDto = serde_json::from_slice(&bytes).expect("parse json");

    assert_eq!(dto.targets.len(), 3, "preserves 3 configured targets in order");

    // Target 1: target-alpha
    let alpha = &dto.targets[0];
    assert_eq!(alpha.target_id, 1);
    assert_eq!(alpha.name, "target-alpha");
    assert_eq!(alpha.outputs, vec![TargetType::M3u]);
    assert!(alpha.next_scheduled.is_some(), "alpha has matching schedule");
    let alpha_analytics = alpha.analytics.as_ref().expect("alpha has analytics");
    assert_eq!(alpha_analytics.version, TARGET_ANALYTICS_VERSION);
    assert!(alpha_analytics.success);
    assert_eq!(alpha_analytics.live.count, 42);

    // Target 2: target-beta (missing file)
    let beta = &dto.targets[1];
    assert_eq!(beta.target_id, 2);
    assert_eq!(beta.name, "target-beta");
    assert_eq!(beta.outputs, vec![TargetType::Xtream]);
    assert!(beta.analytics.is_none(), "beta has no analytics file");
    assert!(beta.next_scheduled.is_some(), "beta is covered by global schedule");

    // Target 3: target-gamma (corrupted file)
    let gamma = &dto.targets[2];
    assert_eq!(gamma.target_id, 3);
    assert_eq!(gamma.name, "target-gamma");
    assert_eq!(gamma.outputs, vec![TargetType::M3u]);
    assert!(gamma.analytics.is_none(), "gamma parse failure falls back to None");
    assert!(gamma.next_scheduled.is_some(), "gamma is covered by global schedule");
}

#[tokio::test]
async fn unmapped_csv_endpoint_returns_200_with_headers_and_body() {
    let fixture = setup_fixture().await;
    let response = fixture.get("/playlist/analytics/1/unmapped.csv").await;
    assert_eq!(response.status(), StatusCode::OK);

    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()),
        Some("text/csv; charset=utf-8")
    );
    assert_eq!(
        response.headers().get(header::CONTENT_DISPOSITION).and_then(|v| v.to_str().ok()),
        Some("attachment; filename=\"target-alpha-unmapped.csv\"")
    );

    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.expect("body");
    let content = String::from_utf8(bytes.to_vec()).expect("utf-8");
    assert_eq!(content, "group,name,epg_channel_id,input\nNews,BBC One,bbc.one,p1\n");
}

#[tokio::test]
async fn unmapped_csv_endpoint_returns_404_for_unknown_id_and_missing_file() {
    let fixture = setup_fixture().await;

    // Unknown target id
    let response = fixture.get("/playlist/analytics/999/unmapped.csv").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // Target 2 exists but unmapped.csv does not exist
    let response = fixture.get("/playlist/analytics/2/unmapped.csv").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn analytics_routes_require_playlist_read() {
    let fixture = setup_fixture().await;
    for uri in ["/playlist/analytics", "/playlist/analytics/1/unmapped.csv"] {
        let response = fixture.call(uri, None).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri} without token");

        let response = fixture.call(uri, Some(&fixture.epg_only)).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{uri} with EpgRead only");

        let response = fixture.call(uri, Some(&fixture.reader)).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri} with PlaylistRead");
    }
}

#[test]
fn test_schedule_filtering_and_next_run_helper() {
    let offset = FixedOffset::east_opt(0).expect("utc offset");
    let now = DateTime::parse_from_rfc3339("2026-10-31T10:00:00Z").expect("valid rfc3339").with_timezone(&offset);

    let schedules = vec![
        ScheduleConfig {
            schedule: "0 0 12 * * * *".to_string(), // next: 12:00 (timestamp: 1761912000)
            task_type: ScheduleTaskType::PlaylistUpdate,
            targets: Some(vec!["target-alpha".to_string()]),
        },
        ScheduleConfig {
            schedule: "0 0 14 * * * *".to_string(), // next: 14:00 (timestamp: 1761919200)
            task_type: ScheduleTaskType::PlaylistUpdate,
            targets: None, // applies to all
        },
        ScheduleConfig {
            schedule: "0 0 11 * * * *".to_string(),   // next: 11:00
            task_type: ScheduleTaskType::LibraryScan, // must be ignored
            targets: None,
        },
    ];

    let runs = parse_playlist_schedule_next_runs(&schedules, &now);
    assert_eq!(runs.len(), 2, "LibraryScan is omitted from playlist schedule runs");

    // target-alpha matches both schedule 1 (12:00) and schedule 2 (14:00); min is 12:00
    let alpha_next = compute_target_next_scheduled(&runs, "TARGET-ALPHA");
    let expected_alpha = DateTime::parse_from_rfc3339("2026-10-31T12:00:00Z").unwrap().timestamp();
    assert_eq!(alpha_next, Some(expected_alpha));

    // target-beta matches only schedule 2 (14:00)
    let beta_next = compute_target_next_scheduled(&runs, "target-beta");
    let expected_beta = DateTime::parse_from_rfc3339("2026-10-31T14:00:00Z").unwrap().timestamp();
    assert_eq!(beta_next, Some(expected_beta));

    // Empty schedules list yields None
    assert_eq!(compute_target_next_scheduled(&[], "target-alpha"), None);
}
