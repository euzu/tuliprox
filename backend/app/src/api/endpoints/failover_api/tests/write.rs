use super::*;
use crate::api::model::{create_test_app_state_with_manual_update_sender, EventMessage, ManualPlaylistUpdateRequest};
use shared::model::{
    AutoGroupOverrideDto, FailoverFileDto, FailoverGroupConflictDto, FailoverGroupSaveResultDto, OperationRunAccepted,
    ReassignOverridesRequestDto, SaveOverridesRequestDto,
};
use tokio::sync::{broadcast, mpsc};
use tuliprox_repository::load_failover_overrides;

struct WriteEnv {
    env: Env,
    failover_file: PathBuf,
    updates: mpsc::Receiver<ManualPlaylistUpdateRequest>,
}

impl WriteEnv {
    fn router(&self) -> Router { router(&self.env) }

    fn events(&self) -> broadcast::Receiver<EventMessage> { self.env.state.event_manager.get_event_channel() }

    fn snapshot_etag(&self, target: &str) -> Option<String> {
        self.env.state.app_config.failover.load().sections.get(target).map(|entry| entry.etag.to_string())
    }

    /// Writes `targets` to the failover file and loads them into the snapshot like start-up does.
    fn seed(&self, targets: Vec<TargetFailoverOverridesDto>) {
        write_file(&self.failover_file, targets);
        self.reload_snapshot();
    }

    fn reload_snapshot(&self) {
        let source = tuliprox_core::utils::FailoverSource::File(self.failover_file.clone());
        self.env.state.app_config.failover.store(Arc::new(load_failover_overrides(&source).unwrap()));
    }

    fn file_targets(&self) -> Vec<String> {
        let dto: FailoverFileDto =
            serde_saphyr::from_str(&std::fs::read_to_string(&self.failover_file).unwrap()).unwrap();
        dto.targets.into_iter().map(|section| section.target).collect()
    }
}

fn write_env() -> WriteEnv { write_env_with_config(|config| config) }

fn write_env_with_config(adjust: impl FnOnce(Config) -> Config) -> WriteEnv {
    let dir = TempDir::new().unwrap();
    let config_dir = dir.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let config = adjust(Config { storage_dir: dir.path().to_string_lossy().to_string(), ..Config::default() });
    let (sender, updates) = mpsc::channel(1);
    let state = with_sources(create_test_app_state_with_manual_update_sender(config, sender));
    let mut paths = (*state.app_config.paths.load_full()).clone();
    paths.config_path = config_dir.to_string_lossy().into_owned();
    state.app_config.paths.store(Arc::new(paths));
    let storage = get_target_storage_path(&state.app_config.config.load(), TARGET).unwrap();
    std::fs::create_dir_all(&storage).unwrap();
    WriteEnv { env: Env { _dir: dir, state, storage }, failover_file: config_dir.join("failover.yml"), updates }
}

fn write_file(path: &Path, targets: Vec<TargetFailoverOverridesDto>) {
    let dto = FailoverFileDto { version: 1, targets };
    std::fs::write(path, serde_saphyr::to_string(&dto).unwrap()).unwrap();
}

fn excluding(target: &str, stream_ids: &[&str]) -> TargetFailoverOverridesDto {
    let mut section = TargetFailoverOverridesDto::new(target);
    section.excluded = stream_ids.iter().map(|id| member_ref("p1", id)).collect();
    section
}

fn save_body(expected_etag: Option<&str>, overrides: TargetFailoverOverridesDto) -> String {
    serde_json::to_string(&SaveOverridesRequestDto { expected_etag: expected_etag.map(str::to_string), overrides })
        .unwrap()
}

fn parse<T: DeserializeOwned>(body: &[u8]) -> T { serde_json::from_slice(body).unwrap() }

fn drain_changed(events: &mut broadcast::Receiver<EventMessage>) -> Vec<Vec<String>> {
    let mut changed = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let EventMessage::FailoverOverridesChanged { targets } = event {
            changed.push(targets);
        }
    }
    changed
}

fn pinned_zdf(order: &[(&str, &str)]) -> TargetFailoverOverridesDto {
    let mut section = TargetFailoverOverridesDto::new(TARGET);
    section.auto_overrides.push(AutoGroupOverrideDto {
        auto_key: "zdf".to_string(),
        order: order.iter().map(|(input, id)| member_ref(input, id)).collect(),
        name: Some("ZDF".to_string()),
        logo: Some("http://logo.invalid/zdf.png".to_string()),
        epg_channel_id: Some("zdf.de".to_string()),
        playlist_group: Some("News".to_string()),
        ..Default::default()
    });
    section
}

#[tokio::test]
async fn save_writes_yaml_and_returns_new_etag() {
    let env = write_env();
    let router = env.router();

    let (status, body) =
        call(&router, "PUT", "/failover/overrides?target=Main", Some(save_body(None, excluding(TARGET, &["3"])))).await;

    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let saved: FailoverGroupSaveResultDto = parse(&body);
    assert!(!saved.hot_applied);
    assert_eq!(saved.file.as_deref(), Some(env.failover_file.to_string_lossy().as_ref()));
    assert_eq!(env.file_targets(), vec![TARGET.to_string()]);
    assert_eq!(saved.etag, env.snapshot_etag(TARGET));
    assert!(saved.etag.is_some());

    let stored: TargetOverridesResponseDto = get_json(&router, "/failover/overrides?target=Main").await;
    assert_eq!(stored.etag, saved.etag);
    assert_eq!(stored.overrides.excluded, vec![member_ref("p1", "3")]);
}

#[tokio::test]
async fn save_conflict_returns_409_with_current_etag() {
    let env = write_env();
    env.seed(vec![excluding(TARGET, &["1"])]);
    let current = env.snapshot_etag(TARGET);
    let router = env.router();

    let (status, body) =
        call(&router, "PUT", "/failover/overrides?target=Main", Some(save_body(None, excluding(TARGET, &["3"])))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(parse::<FailoverGroupConflictDto>(&body).current_etag, current);

    let (status, _) = call(
        &router,
        "PUT",
        "/failover/overrides?target=Main",
        Some(save_body(Some("stale"), excluding(TARGET, &["3"]))),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(env.snapshot_etag(TARGET), current, "a conflicting save leaves the section untouched");
}

#[tokio::test]
async fn save_invalid_returns_422_findings() {
    let env = write_env();
    let mut invalid = TargetFailoverOverridesDto::new(TARGET);
    invalid.groups.push(ManualFailoverGroupDto {
        id: "Not Valid".to_string(),
        name: "Bad".to_string(),
        ..Default::default()
    });

    let (status, body) =
        call(&env.router(), "PUT", "/failover/overrides?target=Main", Some(save_body(None, invalid))).await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        parse::<Vec<OverrideValidationError>>(&body),
        vec![OverrideValidationError::InvalidManualId("Not Valid".to_string())]
    );
    assert!(!env.failover_file.exists());
}

#[tokio::test]
async fn save_refused_with_423_when_files_invalid() {
    let env = write_env();
    std::fs::write(&env.failover_file, "version: 1\ntargets: [not a mapping").unwrap();
    let source = tuliprox_core::utils::FailoverSource::File(env.failover_file.clone());
    let failed = load_failover_overrides(&source).unwrap_err();
    env.env.state.app_config.failover.store(Arc::new(FailoverOverrides {
        source: Some(source),
        error: Some(failed.to_string()),
        ..FailoverOverrides::default()
    }));
    let router = env.router();

    for (method, uri, body) in [
        ("PUT", "/failover/overrides?target=Main", Some(save_body(None, excluding(TARGET, &["3"])))),
        ("DELETE", "/failover/overrides?target=Main&expected_etag=x", None),
    ] {
        let (status, body) = call(&router, method, uri, body).await;
        assert_eq!(status, StatusCode::LOCKED, "{method}");
        let message = parse::<serde_json::Value>(&body)["error"].as_str().unwrap_or_default().to_string();
        assert!(!message.is_empty(), "{method}");
    }
    assert_eq!(std::fs::read_to_string(&env.failover_file).unwrap(), "version: 1\ntargets: [not a mapping");
}

#[tokio::test]
async fn save_for_unknown_or_disabled_target_is_not_found() {
    let env = write_env();
    let router = env.router();
    for target in ["Plain", "Nope"] {
        let (status, _) = call(
            &router,
            "PUT",
            &format!("/failover/overrides?target={target}"),
            Some(save_body(None, excluding(target, &["3"]))),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{target}");
    }
    assert!(!env.failover_file.exists());
}

#[tokio::test]
async fn order_only_save_hot_applies() {
    let env = write_env();
    let applied = pinned_zdf(&[("p1", "1"), ("p2", "1")]);
    env.seed(vec![applied.clone()]);
    let meta = FailoverGroupMeta {
        overrides_etag: env.snapshot_etag(TARGET).map(Arc::from),
        applied_overrides: Some(applied),
        ..FailoverGroupMeta::default()
    };
    publish(&env.env, &persisted_groups(), &meta);
    let etag = env.snapshot_etag(TARGET);

    let reordered = pinned_zdf(&[("p2", "1"), ("p1", "1")]);
    let (status, body) =
        call(&env.router(), "PUT", "/failover/overrides?target=Main", Some(save_body(etag.as_deref(), reordered)))
            .await;

    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let saved: FailoverGroupSaveResultDto = parse(&body);
    assert!(saved.hot_applied);
    assert_ne!(saved.etag, etag);
    let data = FailoverGroupStore::load_target_data(&env.env.storage, 0).unwrap();
    let zdf = data.groups.get(&VirtualId::new(1000)).unwrap();
    let order: Vec<u32> = zdf.members.iter().map(|member| member.member_vid.0).collect();
    assert_eq!(order, vec![1002, 1001]);
    assert_eq!(data.meta.overrides_etag.as_deref(), saved.etag.as_deref());
}

#[tokio::test]
async fn structural_save_is_not_hot_applied() {
    let env = write_env();
    let applied = pinned_zdf(&[("p1", "1"), ("p2", "1")]);
    env.seed(vec![applied.clone()]);
    let meta = FailoverGroupMeta {
        overrides_etag: env.snapshot_etag(TARGET).map(Arc::from),
        applied_overrides: Some(applied.clone()),
        ..FailoverGroupMeta::default()
    };
    publish(&env.env, &persisted_groups(), &meta);
    let etag = env.snapshot_etag(TARGET);

    let mut changed = applied;
    changed.excluded.push(member_ref("p1", "3"));
    let (status, body) =
        call(&env.router(), "PUT", "/failover/overrides?target=Main", Some(save_body(etag.as_deref(), changed))).await;

    assert_eq!(status, StatusCode::OK);
    assert!(!parse::<FailoverGroupSaveResultDto>(&body).hot_applied);
}

#[tokio::test]
async fn delete_removes_section() {
    let env = write_env();
    env.seed(vec![excluding(TARGET, &["1"]), excluding("Other", &["2"])]);
    let etag = env.snapshot_etag(TARGET).unwrap();
    let router = env.router();
    let mut events = env.events();

    let (status, _) = call(&router, "DELETE", "/failover/overrides?target=Main&expected_etag=stale", None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(drain_changed(&mut events).is_empty());

    let (status, body) =
        call(&router, "DELETE", &format!("/failover/overrides?target=Main&expected_etag={etag}"), None).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let result: FailoverGroupSaveResultDto = parse(&body);
    assert_eq!((result.etag, result.file, result.hot_applied), (None, None, false));
    assert_eq!(env.file_targets(), vec!["Other".to_string()]);
    assert_eq!(env.snapshot_etag(TARGET), None);
    assert_eq!(drain_changed(&mut events), vec![vec![TARGET.to_string()]]);
}

#[tokio::test]
async fn reassign_moves_orphan_section() {
    let env = write_env();
    env.seed(vec![excluding("Gone", &["1"])]);
    let etag = env.snapshot_etag("Gone").unwrap();
    let router = env.router();
    let mut events = env.events();
    let request = |from: &str, to: &str, expected_etag: &str| {
        Some(
            serde_json::to_string(&ReassignOverridesRequestDto {
                from: from.to_string(),
                to: to.to_string(),
                expected_etag: expected_etag.to_string(),
            })
            .unwrap(),
        )
    };

    let (status, _) = call(&router, "POST", "/failover/overrides/reassign", request("Gone", "Missing", &etag)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the new owner must be a configured target");
    let (status, _) = call(&router, "POST", "/failover/overrides/reassign", request("Gone", "Plain", &etag)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the new owner must have failover groups enabled");
    assert_eq!(env.snapshot_etag("Gone"), Some(etag.clone()));
    let (status, body) = call(&router, "POST", "/failover/overrides/reassign", request("Gone", TARGET, "stale")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(parse::<FailoverGroupConflictDto>(&body).current_etag.as_deref(), Some(etag.as_str()));
    assert!(drain_changed(&mut events).is_empty());

    let (status, body) = call(&router, "POST", "/failover/overrides/reassign", request("Gone", TARGET, &etag)).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let result: FailoverGroupSaveResultDto = parse(&body);
    assert_eq!(result.etag, env.snapshot_etag(TARGET));
    assert!(!result.hot_applied);
    assert_eq!(env.file_targets(), vec![TARGET.to_string()]);
    assert_eq!(env.snapshot_etag("Gone"), None);
    assert_eq!(drain_changed(&mut events), vec![vec!["Gone".to_string(), TARGET.to_string()]]);
}

#[tokio::test]
async fn save_emits_overrides_changed_event() {
    let env = write_env();
    let mut events = env.events();

    let (status, _) =
        call(&env.router(), "PUT", "/failover/overrides?target=Main", Some(save_body(None, excluding(TARGET, &["3"]))))
            .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(drain_changed(&mut events), vec![vec![TARGET.to_string()]]);
}

#[tokio::test]
async fn apply_enqueues_target_update() {
    let mut env = write_env();
    let router = env.router();

    let (status, body) = call(&router, "POST", "/failover/apply?target=Main", None).await;

    assert_eq!(status, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&body));
    let accepted: OperationRunAccepted = parse(&body);
    let queued = env.updates.try_recv().unwrap();
    assert_eq!(Some(queued.run_id.clone()), accepted.run_id);
    assert_eq!(queued.targets.target_names, vec![TARGET.to_string()]);
    assert!(queued.input_action.is_none());

    for target in ["Plain", "Nope"] {
        let (status, _) = call(&router, "POST", &format!("/failover/apply?target={target}"), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{target}");
    }
    assert!(env.updates.try_recv().is_err(), "rejected requests enqueue nothing");
}

#[tokio::test]
async fn apply_busy_returns_409() {
    let env = write_env();
    let router = env.router();
    let (status, _) = call(&router, "POST", "/failover/apply?target=Main", None).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (status, body) = call(&router, "POST", "/failover/apply?target=Main", None).await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert!(parse::<serde_json::Value>(&body)["error"].is_string());
}

#[tokio::test]
async fn apply_without_worker_returns_503() {
    let WriteEnv { env, updates, .. } = write_env();
    drop(updates);

    let (status, _) = call(&router(&env), "POST", "/failover/apply?target=Main", None).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn write_routes_require_playlist_write() -> Result<(), Box<dyn std::error::Error>> {
    let env = write_env_with_config(|config| secured_config(Path::new(&config.storage_dir)));
    let config = env.env.state.app_config.config.load_full();
    let reader = token_with(&config, "reader", Permission::PlaylistRead)?;
    let writer = token_with(&config, "writer", Permission::PlaylistWrite)?;
    let router = failover_api_register_with_permissions(&env.env.state).with_state(Arc::clone(&env.env.state));
    let save =
        serde_json::to_vec(&SaveOverridesRequestDto { expected_etag: None, overrides: excluding(TARGET, &["3"]) })?;
    let reassign = serde_json::to_vec(&ReassignOverridesRequestDto {
        from: "Gone".to_string(),
        to: TARGET.to_string(),
        expected_etag: "x".to_string(),
    })?;

    for (method, uri, body) in [
        ("PUT", "/failover/overrides?target=Main", save),
        ("DELETE", "/failover/overrides?target=Main&expected_etag=x", Vec::new()),
        ("POST", "/failover/overrides/reassign", reassign),
        ("POST", "/failover/apply?target=Main", Vec::new()),
    ] {
        let denied = router.clone().oneshot(json_request(method, uri, &reader, body.clone())?).await?;
        assert_eq!(denied.status(), StatusCode::FORBIDDEN, "{method} {uri}");
        let allowed = router.clone().oneshot(json_request(method, uri, &writer, body)?).await?;
        assert_ne!(allowed.status(), StatusCode::FORBIDDEN, "{method} {uri}");
        assert_ne!(allowed.status(), StatusCode::UNAUTHORIZED, "{method} {uri}");
    }
    Ok(())
}
