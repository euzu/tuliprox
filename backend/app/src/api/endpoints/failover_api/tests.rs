use super::*;
use crate::{
    api::model::create_test_app_state,
    auth::create_jwt_web_user,
    model::{Config, SourcesConfig},
};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde::de::DeserializeOwned;
use shared::model::{
    permission::Permission, CandidatePageDto, ChannelMemberRefDto, ConfigInputDto, ConfigSourceDto, ConfigTargetDto,
    ConfigTargetOptions, FailoverGroupHealthDto, FailoverGroupListDto, FailoverGroupOrigin, FailoverGroupPolicy,
    FailoverGroupTargetsDto, FailoverGroupsTargetConfig, FailoverTargetState, ManualFailoverGroupDto,
    OverrideValidationError, SourcesConfigDto, TargetFailoverOverridesDto, TargetOverridesResponseDto, UserId,
    VirtualId, WebAuthConfigDto, WebUiConfigDto,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tempfile::TempDir;
use tower::ServiceExt;
use tuliprox_core::model::{FailoverOverrides, TargetOverridesEntry};
use tuliprox_repository::{
    get_target_storage_path, CatalogRecord, FailoverGroupMeta, FailoverGroupRecord, FailoverGroupSideFiles,
    FailoverGroupStore, GroupMember,
};
use tuliprox_session::{CircuitPolicy, QosKey, QosPart};

const TARGET: &str = "Main";

struct Env {
    _dir: TempDir,
    state: Arc<AppState>,
    storage: PathBuf,
}

fn member_ref(input: &str, stream_id: &str) -> ChannelMemberRefDto {
    ChannelMemberRefDto { input: input.to_string(), stream_id: stream_id.to_string() }
}

fn input(name: &str, max_connections: u16) -> ConfigInputDto {
    ConfigInputDto {
        name: name.to_string().into(),
        url: format!("http://{name}.invalid/playlist.m3u"),
        max_connections,
        ..Default::default()
    }
}

fn target_dto(name: &str, failover: bool) -> ConfigTargetDto {
    ConfigTargetDto {
        name: name.to_string(),
        options: failover.then(|| ConfigTargetOptions {
            failover_groups: Some(FailoverGroupsTargetConfig { enabled: true, ..Default::default() }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn state_with_config(config: Config) -> Arc<AppState> { with_sources(create_test_app_state(config)) }

fn with_sources(state: Arc<AppState>) -> Arc<AppState> {
    let dto = SourcesConfigDto {
        inputs: vec![input("p1", 2), input("p2", 3)],
        sources: vec![ConfigSourceDto {
            inputs: vec!["p1".to_string().into(), "p2".to_string().into()],
            targets: vec![target_dto(TARGET, true), target_dto("Plain", false)],
        }],
        ..Default::default()
    };
    state.app_config.sources.store(Arc::new(SourcesConfig::try_from(&dto).unwrap()));
    state.active_provider.update_config(&state.app_config);
    state
}

fn env() -> Env {
    let dir = TempDir::new().unwrap();
    let config = Config { storage_dir: dir.path().to_string_lossy().to_string(), ..Config::default() };
    let state = state_with_config(config);
    let storage = get_target_storage_path(&state.app_config.config.load(), TARGET).unwrap();
    std::fs::create_dir_all(&storage).unwrap();
    Env { _dir: dir, state, storage }
}

fn catalog_record(input: &str, stream_id: &str, title: &str, key: &str, provider_id: u32) -> CatalogRecord {
    CatalogRecord {
        member_ref: member_ref(input, stream_id),
        natural_key: Some(key.into()),
        dsl_forced: false,
        epg_channel_id: None,
        name: title.into(),
        title: title.into(),
        logo: format!("http://logo.invalid/{stream_id}.png").into(),
        playlist_group: "News".into(),
        provider_id,
        has_archive: stream_id == "1",
    }
}

fn catalog() -> Vec<CatalogRecord> {
    vec![
        catalog_record("p1", "1", "ZDF HD", "zdf", 101),
        catalog_record("p2", "1", "ZDF", "zdf", 201),
        catalog_record("p1", "2", "RTL", "rtl", 102),
        catalog_record("p2", "2", "RTL HD", "rtl", 202),
        catalog_record("p1", "3", "Arte", "arte", 103),
    ]
}

fn group_member(input: &str, stream_id: &str, title: &str, provider_id: u32, vid: u32) -> GroupMember {
    GroupMember {
        member_vid: VirtualId::new(vid),
        input_name: input.into(),
        provider_id,
        input_stream_id: stream_id.into(),
        url: format!("http://{input}.invalid/{stream_id}.ts").into(),
        item_type: shared::model::PlaylistItemType::Live,
        cluster: shared::model::XtreamCluster::Live,
        title: title.into(),
        archive: None,
        epg_channel_id: None,
        additional_properties: None,
    }
}

fn group_record(vid: u32, key: &str, members: Vec<GroupMember>) -> FailoverGroupRecord {
    FailoverGroupRecord {
        group_vid: VirtualId::new(vid),
        key: key.into(),
        origin: FailoverGroupOrigin::Auto,
        policy: FailoverGroupPolicy::Auto,
        members,
        orphaned: Vec::new(),
    }
}

fn persisted_groups() -> Vec<FailoverGroupRecord> {
    vec![
        group_record(
            1000,
            "zdf",
            vec![group_member("p1", "1", "ZDF HD", 101, 1001), group_member("p2", "1", "ZDF", 201, 1002)],
        ),
        group_record(
            2000,
            "rtl",
            vec![group_member("p1", "2", "RTL", 102, 2001), group_member("p2", "2", "RTL HD", 202, 2002)],
        ),
    ]
}

fn publish(env: &Env, records: &[FailoverGroupRecord], meta: &FailoverGroupMeta) {
    let catalog = catalog();
    FailoverGroupStore::write(&env.storage, records, &[], Some(FailoverGroupSideFiles { catalog: &catalog, meta }))
        .unwrap();
    env.state.playlists.bump_failover_group_generation(TARGET);
}

fn seeded_env() -> Env {
    let env = env();
    publish(&env, &persisted_groups(), &FailoverGroupMeta::default());
    env
}

fn section_entry(
    section: TargetFailoverOverridesDto,
    file: &Path,
    findings: Vec<OverrideValidationError>,
) -> Arc<TargetOverridesEntry> {
    let etag: Arc<str> = Arc::from(section.etag());
    Arc::new(TargetOverridesEntry { overrides: section, file: file.to_path_buf(), etag, findings })
}

fn store_snapshot(env: &Env, entries: Vec<Arc<TargetOverridesEntry>>) {
    let mut overrides = FailoverOverrides::default();
    for entry in entries {
        overrides.sections.insert(Arc::from(entry.overrides.target.as_str()), entry);
    }
    env.state.app_config.failover.store(Arc::new(overrides));
}

async fn call(router: &Router, method: &str, uri: &str, body: Option<String>) -> (StatusCode, Vec<u8>) {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header(axum::http::header::CONTENT_TYPE, "application/json");
    }
    let request = builder.body(body.map_or_else(Body::empty, Body::from)).unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    (status, response.into_body().collect().await.unwrap().to_bytes().to_vec())
}

async fn get_json<T: DeserializeOwned>(router: &Router, uri: &str) -> T {
    let (status, body) = call(router, "GET", uri, None).await;
    assert_eq!(status, StatusCode::OK, "GET {uri}: {}", String::from_utf8_lossy(&body));
    serde_json::from_slice(&body).unwrap()
}

fn router(env: &Env) -> Router { failover_api_register_unprotected().with_state(Arc::clone(&env.state)) }

#[tokio::test]
async fn targets_lists_enabled_targets_files_and_orphan_sections() {
    let env = seeded_env();
    let meta = FailoverGroupMeta { overrides_etag: Some("applied-1".into()), ..FailoverGroupMeta::default() };
    publish(&env, &persisted_groups(), &meta);
    let file = PathBuf::from("/cfg/failover.yml");
    let main =
        section_entry(TargetFailoverOverridesDto::new(TARGET), &file, vec![OverrideValidationError::EmptyManualId]);
    let gone = section_entry(TargetFailoverOverridesDto::new("Gone"), &file, Vec::new());
    let plain = section_entry(TargetFailoverOverridesDto::new("Plain"), &file, Vec::new());
    let main_etag = main.etag.to_string();
    let mut overrides = FailoverOverrides::default();
    overrides.sections.insert(Arc::from(TARGET), main);
    overrides.sections.insert(Arc::from("Gone"), gone);
    overrides.sections.insert(Arc::from("Plain"), plain);
    overrides.files_used = vec![file.clone()];
    overrides.source = Some(tuliprox_core::utils::FailoverSource::File(file.clone()));
    env.state.app_config.failover.store(Arc::new(overrides));

    let listing: FailoverGroupTargetsDto = get_json(&router(&env), "/failover/targets").await;

    assert_eq!(listing.source.as_deref(), Some("/cfg/failover.yml"));
    assert_eq!(listing.files_used, vec!["/cfg/failover.yml".to_string()]);
    assert_eq!(listing.load_error, None);
    let names: Vec<&str> = listing.targets.iter().map(|target| target.name.as_str()).collect();
    assert_eq!(names, vec![TARGET, "Gone", "Plain"]);
    let main = &listing.targets[0];
    assert_eq!(main.state, FailoverTargetState::Enabled);
    assert_eq!(main.etag.as_deref(), Some(main_etag.as_str()));
    assert_eq!(main.applied_etag.as_deref(), Some("applied-1"));
    assert_eq!(main.file.as_deref(), Some("/cfg/failover.yml"));
    assert_eq!(main.group_count, 2);
    assert_eq!(main.findings, vec![OverrideValidationError::EmptyManualId]);
    assert_eq!(main.problem_count, 1);
    let gone = &listing.targets[1];
    assert_eq!(gone.state, FailoverTargetState::Orphan);
    assert_eq!(gone.group_count, 0);
    assert_eq!(gone.problem_count, 1);
    // A configured target without failover groups keeps its section visible, with the reason.
    let plain = &listing.targets[2];
    assert_eq!(plain.state, FailoverTargetState::Disabled);
    assert_eq!(plain.file.as_deref(), Some("/cfg/failover.yml"));
    assert_eq!(plain.problem_count, 1);
}

#[tokio::test]
async fn targets_reports_load_error_and_lists_enabled_target_without_section() {
    let env = env();
    let overrides = FailoverOverrides { error: Some("duplicate target".to_string()), ..FailoverOverrides::default() };
    env.state.app_config.failover.store(Arc::new(overrides));

    let listing: FailoverGroupTargetsDto = get_json(&router(&env), "/failover/targets").await;

    assert_eq!(listing.load_error.as_deref(), Some("duplicate target"));
    assert_eq!(listing.targets.len(), 1);
    let main = &listing.targets[0];
    assert_eq!((main.name.as_str(), main.etag.as_ref(), main.file.as_ref(), main.group_count), (TARGET, None, None, 0));
}

#[tokio::test]
async fn groups_and_preview_expose_display_stale_groups() {
    let env = env();
    let meta = FailoverGroupMeta { display_stale_groups: vec!["zdf".to_string()], ..FailoverGroupMeta::default() };
    publish(&env, &persisted_groups(), &meta);
    let router = router(&env);

    let list: FailoverGroupListDto = get_json(&router, "/failover/groups?target=Main").await;
    assert_eq!(list.display_stale, ["zdf"]);

    let body = serde_json::to_string(&TargetFailoverOverridesDto::new(TARGET)).unwrap();
    let (status, bytes) = call(&router, "POST", "/failover/preview?target=Main", Some(body)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_slice::<FailoverGroupListDto>(&bytes).unwrap().display_stale, ["zdf"]);
}

#[tokio::test]
async fn list_returns_groups_diagnostics_and_etags() {
    let env = seeded_env();
    let router = router(&env);
    let section = TargetFailoverOverridesDto::new(TARGET);
    let entry = section_entry(section, Path::new("failover.yml"), Vec::new());
    let current_etag = entry.etag.to_string();
    store_snapshot(&env, vec![entry]);

    let list: FailoverGroupListDto = get_json(&router, "/failover/groups?target=Main").await;

    assert_eq!(list.etag.as_deref(), Some(current_etag.as_str()));
    assert_eq!(list.applied_etag, None);
    assert!(list.diagnostics.is_empty());
    assert_eq!(list.display_stale.len(), 0);
    assert_eq!(list.groups.len(), 2);
    let zdf = list.groups.iter().find(|group| group.key == "zdf").unwrap();
    assert_eq!(zdf.group_vid, Some(VirtualId::new(1000)));
    assert_eq!(zdf.origin, FailoverGroupOrigin::Auto);
    assert_eq!(zdf.name, "ZDF HD");
    assert_eq!(zdf.logo.as_deref(), Some("http://logo.invalid/1.png"));
    assert_eq!(zdf.playlist_group.as_deref(), Some("News"));
    let members: Vec<_> = zdf.members.iter().map(|m| (m.member_vid, m.member_ref.clone(), m.title.as_str())).collect();
    assert_eq!(
        members,
        vec![
            (Some(VirtualId::new(1001)), member_ref("p1", "1"), "ZDF HD"),
            (Some(VirtualId::new(1002)), member_ref("p2", "1"), "ZDF"),
        ]
    );
    assert!(zdf.members[0].has_archive);
    assert_eq!(zdf.members[0].provider_id, 101);
    assert_eq!(zdf.members[0].input_name, "p1");

    // The applied section drives the resolution, so its diagnostics surface here.
    let mut applied = TargetFailoverOverridesDto::new(TARGET);
    applied
        .auto_overrides
        .push(shared::model::AutoGroupOverrideDto { auto_key: "vanished".to_string(), ..Default::default() });
    let meta = FailoverGroupMeta {
        overrides_etag: Some("applied-2".into()),
        applied_overrides: Some(applied),
        ..FailoverGroupMeta::default()
    };
    publish(&env, &persisted_groups(), &meta);

    let list: FailoverGroupListDto = get_json(&router, "/failover/groups?target=Main").await;
    assert_eq!(list.applied_etag.as_deref(), Some("applied-2"));
    assert_eq!(list.diagnostics.stale_auto_overrides, vec!["vanished".to_string()]);
    assert_eq!(list.groups.len(), 2);
}

#[tokio::test]
async fn overrides_get_returns_section_etag_and_file() {
    let env = seeded_env();
    let router = router(&env);
    let mut section = TargetFailoverOverridesDto::new(TARGET);
    section.excluded.push(member_ref("p1", "3"));
    let entry = section_entry(section.clone(), Path::new("/cfg/failover.d/main.yml"), Vec::new());
    let etag = entry.etag.to_string();
    store_snapshot(&env, vec![entry]);

    let response: TargetOverridesResponseDto = get_json(&router, "/failover/overrides?target=Main").await;
    assert_eq!(response.etag.as_deref(), Some(etag.as_str()));
    assert_eq!(response.file.as_deref(), Some("/cfg/failover.d/main.yml"));
    assert_eq!(response.overrides, section);

    store_snapshot(&env, Vec::new());
    let response: TargetOverridesResponseDto = get_json(&router, "/failover/overrides?target=Main").await;
    assert_eq!((response.etag, response.file), (None, None));
    assert_eq!(response.overrides, TargetFailoverOverridesDto::new(TARGET));
}

#[tokio::test]
async fn preview_reflects_draft_and_writes_nothing() {
    let env = seeded_env();
    let router = router(&env);
    let before_groups = FailoverGroupStore::read_all_groups(&env.storage).unwrap();
    let before_meta = FailoverGroupStore::read_meta(&env.storage).unwrap();
    let current = section_entry(TargetFailoverOverridesDto::new(TARGET), Path::new("failover.yml"), Vec::new());
    let current_etag = current.etag.to_string();
    store_snapshot(&env, vec![current]);
    let before_snapshot = env.state.app_config.failover.load_full();
    let generation = env.state.playlists.failover_group_generation(TARGET);

    let mut draft = TargetFailoverOverridesDto::new(TARGET);
    draft.groups.push(ManualFailoverGroupDto {
        id: "mine".to_string(),
        name: "Mine".to_string(),
        members: vec![member_ref("p1", "3"), member_ref("p2", "2")],
        ..Default::default()
    });
    let body = serde_json::to_string(&draft).unwrap();
    let (status, bytes) = call(&router, "POST", "/failover/preview?target=Main", Some(body)).await;

    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&bytes));
    let list: FailoverGroupListDto = serde_json::from_slice(&bytes).unwrap();
    let keys: Vec<(&str, FailoverGroupOrigin)> = list.groups.iter().map(|g| (g.key.as_str(), g.origin)).collect();
    assert_eq!(keys, vec![("mine", FailoverGroupOrigin::Manual), ("zdf", FailoverGroupOrigin::Auto)]);
    let mine = &list.groups[0];
    assert_eq!(mine.group_vid, None);
    assert_eq!(mine.name, "Mine");
    assert_eq!(
        mine.members.iter().map(|m| m.member_ref.clone()).collect::<Vec<_>>(),
        vec![member_ref("p1", "3"), member_ref("p2", "2")]
    );
    assert!(mine.members.iter().all(|m| m.member_vid.is_none()));
    assert_eq!(list.groups[1].group_vid, Some(VirtualId::new(1000)));
    assert_eq!(list.etag.as_deref(), Some(current_etag.as_str()));
    assert_ne!(list.etag, Some(draft.etag()));

    assert_eq!(FailoverGroupStore::read_all_groups(&env.storage).unwrap(), before_groups);
    assert_eq!(FailoverGroupStore::read_meta(&env.storage).unwrap(), before_meta);
    assert!(Arc::ptr_eq(&env.state.app_config.failover.load_full(), &before_snapshot));
    assert_eq!(env.state.playlists.failover_group_generation(TARGET), generation);
}

#[tokio::test]
async fn preview_reports_diagnostics_of_the_draft() {
    let env = seeded_env();
    let mut draft = TargetFailoverOverridesDto::new(TARGET);
    draft.groups.push(ManualFailoverGroupDto {
        id: "lonely".to_string(),
        name: "Lonely".to_string(),
        members: vec![member_ref("p1", "3"), member_ref("p9", "9")],
        ..Default::default()
    });
    let body = serde_json::to_string(&draft).unwrap();

    let (status, bytes) = call(&router(&env), "POST", "/failover/preview?target=Main", Some(body)).await;

    assert_eq!(status, StatusCode::OK);
    let list: FailoverGroupListDto = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(list.diagnostics.inactive_manual_groups.len(), 1);
    assert_eq!(list.diagnostics.unresolved_refs.len(), 1);
    assert_eq!(list.diagnostics.unresolved_refs[0].member_ref, member_ref("p9", "9"));
}

#[tokio::test]
async fn preview_of_empty_draft_matches_the_persisted_listing() {
    let env = seeded_env();
    let router = router(&env);
    let listing: FailoverGroupListDto = get_json(&router, "/failover/groups?target=Main").await;
    let body = serde_json::to_string(&TargetFailoverOverridesDto::new(TARGET)).unwrap();

    let (status, bytes) = call(&router, "POST", "/failover/preview?target=Main", Some(body)).await;

    assert_eq!(status, StatusCode::OK);
    let preview: FailoverGroupListDto = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(preview.groups, listing.groups);
    assert_eq!(preview.diagnostics, listing.diagnostics);
}

#[tokio::test]
async fn preview_group_name_matches_processing() {
    let env = env();
    let mut catalog = catalog();
    catalog[0].name = "ZDF Raw Name".into();
    catalog[0].title = "ZDF Display Title".into();
    let meta = FailoverGroupMeta::default();
    FailoverGroupStore::write(
        &env.storage,
        &persisted_groups(),
        &[],
        Some(FailoverGroupSideFiles { catalog: &catalog, meta: &meta }),
    )
    .unwrap();
    env.state.playlists.bump_failover_group_generation(TARGET);
    let router = router(&env);
    let body = serde_json::to_string(&TargetFailoverOverridesDto::new(TARGET)).unwrap();

    let (status, bytes) = call(&router, "POST", "/failover/preview?target=Main", Some(body)).await;

    assert_eq!(status, StatusCode::OK);
    let preview: FailoverGroupListDto = serde_json::from_slice(&bytes).unwrap();
    let zdf = preview.groups.iter().find(|group| group.key == "zdf").expect("zdf group");
    assert_eq!(zdf.name, "ZDF Raw Name", "name comes from header.name like the published group, not the title");
    assert_eq!(zdf.members[0].title, "ZDF Display Title");
}

#[test]
fn query_match_ignores_case_for_ascii_and_unicode() {
    assert!(contains_ignore_case("ZDF HD", "zdf"));
    assert!(contains_ignore_case("ZDF HD", ""));
    assert!(!contains_ignore_case("ZDF", "zdf hd"));
    assert!(contains_ignore_case("Das Erste Ärzte", "ärzte"));
    assert!(!contains_ignore_case("Arte", "ärzte"));
}

#[tokio::test]
async fn candidates_paged_and_filtered() {
    let env = seeded_env();
    let router = router(&env);
    let mut section = TargetFailoverOverridesDto::new(TARGET);
    section.excluded.push(member_ref("p1", "3"));
    store_snapshot(&env, vec![section_entry(section, Path::new("failover.yml"), Vec::new())]);

    let all: CandidatePageDto = get_json(&router, "/failover/candidates?target=Main").await;
    assert_eq!(all.total, 5);
    assert_eq!(all.items.len(), 5);
    let zdf = &all.items[0];
    assert_eq!(zdf.member_ref, member_ref("p1", "1"));
    assert_eq!(zdf.group_key.as_deref(), Some("zdf"));
    assert!(!zdf.excluded);
    assert_eq!((zdf.title.as_str(), zdf.provider_id, zdf.has_archive), ("ZDF HD", 101, true));

    let ungrouped: CandidatePageDto = get_json(&router, "/failover/candidates?target=Main&filter=ungrouped").await;
    assert_eq!(ungrouped.total, 1);
    assert_eq!(ungrouped.items[0].member_ref, member_ref("p1", "3"));
    assert_eq!(ungrouped.items[0].group_key, None);

    let grouped: CandidatePageDto = get_json(&router, "/failover/candidates?target=Main&filter=grouped").await;
    assert_eq!(grouped.total, 4);
    assert!(grouped.items.iter().all(|item| item.group_key.is_some()));

    let excluded: CandidatePageDto = get_json(&router, "/failover/candidates?target=Main&filter=excluded").await;
    assert_eq!(excluded.total, 1);
    assert!(excluded.items[0].excluded);
    assert_eq!(excluded.items[0].title, "Arte");

    let query: CandidatePageDto = get_json(&router, "/failover/candidates?target=Main&q=zDf").await;
    assert_eq!(query.total, 2);
    assert!(query.items.iter().all(|item| item.title.starts_with("ZDF")));

    let regex: CandidatePageDto =
        get_json(&router, "/failover/candidates?target=Main&regex=true&q=%5EZDF%20.*D%24").await;
    assert_eq!(regex.total, 1);
    assert_eq!(regex.items[0].title, "ZDF HD");
    // a regular expression is case sensitive, a plain query is not
    let regex_case: CandidatePageDto = get_json(&router, "/failover/candidates?target=Main&regex=true&q=zdf").await;
    assert_eq!(regex_case.total, 0);
    let (status, _) = call(&router, "GET", "/failover/candidates?target=Main&regex=true&q=%28", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let page: CandidatePageDto = get_json(&router, "/failover/candidates?target=Main&offset=1&limit=2").await;
    assert_eq!(page.total, 5);
    assert_eq!(
        page.items.iter().map(|item| item.member_ref.clone()).collect::<Vec<_>>(),
        vec![member_ref("p2", "1"), member_ref("p1", "2")]
    );

    let past_end: CandidatePageDto = get_json(&router, "/failover/candidates?target=Main&offset=50").await;
    assert_eq!((past_end.total, past_end.items.len()), (5, 0));
}

#[tokio::test]
async fn health_reports_score_breakdown_and_capacity() {
    let env = seeded_env();
    let health = &env.state.member_health;
    health.set_policy(CircuitPolicy::new(1, Duration::from_secs(60), Duration::from_secs(60)));
    health.replace_qos([
        (
            QosKey::Provider { input: "p1".into(), provider_id: 101 },
            QosPart { score_24h: 90, score_7d: 80, confidence: 100, failure_streak: 0 },
        ),
        (
            QosKey::Provider { input: "p2".into(), provider_id: 201 },
            QosPart { score_24h: 20, score_7d: 20, confidence: 100, failure_streak: 0 },
        ),
    ]);

    let report: FailoverGroupHealthDto = get_json(&router(&env), "/failover/health?target=Main&group=1000").await;

    assert_eq!(report.members.len(), 2);
    let first = &report.members[0];
    assert_eq!((first.member_vid, first.rank), (VirtualId::new(1001), 1));
    assert_eq!(first.member_ref, member_ref("p1", "1"));
    assert_eq!((first.qos_24h, first.qos_7d, first.confidence), (Some(90), Some(80), Some(100)));
    assert_eq!(first.score.qos, 90);
    assert!(!first.score.demoted);
    assert_eq!(first.circuit, "closed");
    assert_eq!((first.slots_used, first.slots_max), (0, 2));
    let second = &report.members[1];
    assert_eq!((second.member_vid, second.rank), (VirtualId::new(1002), 2));
    assert_eq!(second.score.qos, 20);
    assert!(first.score.total > second.score.total);
    assert_eq!((second.slots_used, second.slots_max), (0, 3));
    assert_eq!(report.capacity.accounts, 2);
    assert_eq!(report.capacity.max_connections, 5);
    assert_eq!(report.capacity.in_use, 0);

    // An open member circuit demotes the member behind a healthy one, whatever its score.
    health.record_failure(&(Arc::from("p1"), Arc::from("1")), Instant::now());
    let report: FailoverGroupHealthDto = get_json(&router(&env), "/failover/health?target=Main&group=1000").await;
    assert_eq!(report.members[0].member_vid, VirtualId::new(1002));
    let demoted = &report.members[1];
    assert_eq!(demoted.circuit, "open");
    assert!(demoted.circuit_remaining_secs.is_some());
    assert!(demoted.score.demoted);
}

#[tokio::test]
async fn health_counts_shared_accounts_once() {
    let env = env();
    let records = vec![group_record(
        3000,
        "dup",
        vec![group_member("p1", "1", "A", 101, 3001), group_member("p1", "3", "B", 103, 3002)],
    )];
    publish(&env, &records, &FailoverGroupMeta::default());

    let report: FailoverGroupHealthDto = get_json(&router(&env), "/failover/health?target=Main&group=3000").await;

    assert_eq!(report.members.len(), 2);
    assert_eq!((report.capacity.accounts, report.capacity.max_connections), (1, 2));
    assert!(report.members.iter().all(|member| member.slots_max == 2));
}

#[tokio::test]
async fn health_of_unknown_group_is_not_found() {
    let env = seeded_env();
    let (status, _) = call(&router(&env), "GET", "/failover/health?target=Main&group=4242", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unknown_and_disabled_targets_are_not_found() {
    let env = seeded_env();
    let router = router(&env);
    for target in ["Nope", "Plain"] {
        for (method, route) in
            [("GET", "groups"), ("GET", "overrides"), ("POST", "preview"), ("GET", "candidates"), ("GET", "health")]
        {
            let uri = format!("/failover/{route}?target={target}&group=1");
            let body =
                (method == "POST").then(|| serde_json::to_string(&TargetFailoverOverridesDto::new(target)).unwrap());
            let (status, _) = call(&router, method, &uri, body).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
        }
    }
}

#[tokio::test]
async fn no_catalog_returns_409_run_update_first() {
    let env = env();
    let router = router(&env);
    let body = serde_json::to_string(&TargetFailoverOverridesDto::new(TARGET)).unwrap();
    for (method, uri) in [
        ("GET", "/failover/groups?target=Main"),
        ("POST", "/failover/preview?target=Main"),
        ("GET", "/failover/candidates?target=Main"),
    ] {
        let (status, bytes) = call(&router, method, uri, (method == "POST").then(|| body.clone())).await;
        assert_eq!(status, StatusCode::CONFLICT, "{method} {uri}");
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"], "run a playlist update first");
    }

    // Reading the stored section does not need a catalog.
    let (status, _) = call(&router, "GET", "/failover/overrides?target=Main", None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn catalog_is_read_once_for_repeated_requests() {
    let env = seeded_env();
    let router = router(&env);
    let _: CandidatePageDto = get_json(&router, "/failover/candidates?target=Main").await;
    let loads = env.state.playlists.failover_groups.catalog_load_count();
    let _: CandidatePageDto = get_json(&router, "/failover/candidates?target=Main&q=zdf").await;
    let _: FailoverGroupListDto = get_json(&router, "/failover/groups?target=Main").await;
    assert_eq!(env.state.playlists.failover_groups.catalog_load_count(), loads);
}

#[test]
fn page_window_defaults_and_clamps_limit() {
    assert_eq!(page_window(None, None), (0, 100));
    assert_eq!(page_window(Some(7), Some(10)), (7, 10));
    assert_eq!(page_window(None, Some(10_000)), (0, 500));
    assert_eq!(page_window(None, Some(0)), (0, 0));
}

/// A config with web auth enabled, so the permission layers of the router are active.
fn secured_config(storage_dir: &Path) -> Config {
    let web_ui = WebUiConfigDto {
        auth: Some(WebAuthConfigDto {
            enabled: true,
            issuer: "failover-test".to_owned(),
            secret: "failover-test-secret".to_owned(),
            ..Default::default()
        }),
        ..Default::default()
    };
    Config {
        web_ui: Some((&web_ui).into()),
        storage_dir: storage_dir.to_string_lossy().to_string(),
        ..Default::default()
    }
}

fn token_with(config: &Config, name: &str, permission: Permission) -> Result<String, Box<dyn std::error::Error>> {
    let Some(web_auth) = config.web_ui.as_ref().and_then(|web_ui| web_ui.auth.as_ref()) else {
        return Err("missing web auth".into());
    };
    Ok(create_jwt_web_user(web_auth, name, permission.into(), 0, UserId::web(name))?)
}

fn json_request(method: &str, uri: &str, token: &str, body: Vec<u8>) -> Result<Request<Body>, axum::http::Error> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("Authorization", format!("Bearer {token}"))
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
}

#[tokio::test]
async fn read_routes_require_playlist_read() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let config = secured_config(dir.path());
    let reader = token_with(&config, "reader", Permission::PlaylistRead)?;
    let system = token_with(&config, "system", Permission::SystemRead)?;
    let state = state_with_config(config);
    let router = failover_api_register_with_permissions(&state).with_state(Arc::clone(&state));
    let body = serde_json::to_vec(&TargetFailoverOverridesDto::new(TARGET))?;

    for (method, uri) in [
        ("GET", "/failover/targets"),
        ("GET", "/failover/groups?target=Main"),
        ("GET", "/failover/overrides?target=Main"),
        ("POST", "/failover/preview?target=Main"),
        ("GET", "/failover/candidates?target=Main"),
        ("GET", "/failover/health?target=Main&group=1"),
    ] {
        let denied = router.clone().oneshot(json_request(method, uri, &system, body.clone())?).await?;
        assert_eq!(denied.status(), StatusCode::FORBIDDEN, "{method} {uri}");
        let allowed = router.clone().oneshot(json_request(method, uri, &reader, body.clone())?).await?;
        assert_ne!(allowed.status(), StatusCode::FORBIDDEN, "{method} {uri}");
        assert_ne!(allowed.status(), StatusCode::UNAUTHORIZED, "{method} {uri}");
    }
    Ok(())
}

mod write;
