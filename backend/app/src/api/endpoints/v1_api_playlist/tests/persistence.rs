use super::{test_app_config, test_app_state};
use crate::model::{Config, ConfigInput, ConfigSource};
use axum::{extract::State, Json};
use shared::{model::InputType, utils::Internable};
use std::sync::Arc;
use tempfile::tempdir;

#[tokio::test]
async fn playlist_update_scope_preserves_write_only_access_without_status_reads() {
    use crate::auth::create_jwt_web_user;
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
        routing::get,
        Router,
    };
    use shared::model::{
        Permission, PermissionSet, PlaylistUpdateStatusDto, PlaylistUpdateViewSnapshotDto, UserId, WebAuthConfigDto,
        WebUiConfigDto,
    };
    use tower::ServiceExt;
    let input = Arc::new(ConfigInput { id: 2, name: "source".intern(), ..ConfigInput::default() });
    let app_config = test_app_config(input, ConfigSource { inputs: vec!["source".intern()], targets: vec![] });
    let auth =
        WebAuthConfigDto { issuer: "local-fixture".into(), secret: "fixture signing key".into(), ..Default::default() };
    let web_ui = crate::model::WebUiConfig::from(&WebUiConfigDto { auth: Some(auth), ..Default::default() });
    let token = |permissions| {
        create_jwt_web_user(web_ui.auth.as_ref().unwrap(), "writer", permissions, 0, UserId::web("writer")).unwrap()
    };
    let mut writer = PermissionSet::from(Permission::PlaylistWrite);
    writer.set(Permission::SourceRead);
    let writer_token = token(writer);
    let source_only_token = token(Permission::SourceRead.into());
    let playlist_only_token = token(Permission::PlaylistWrite.into());
    let reader_token = token(Permission::PlaylistRead.into());
    app_config.config.store(Arc::new(Config { web_ui: Some(web_ui), ..Config::default() }));
    let app = test_app_state(Arc::new(app_config));
    let routes = Router::new().route("/status", get(super::super::playlist_update_status_protected)).with_state(app);
    let send = |url, token| {
        routes
            .clone()
            .oneshot(Request::get(url).header("authorization", format!("Bearer {token}")).body(Body::empty()).unwrap())
    };
    assert_eq!(send("/status", &writer_token).await.unwrap().status(), StatusCode::FORBIDDEN);
    assert_eq!(send("/status", &reader_token).await.unwrap().status(), StatusCode::OK);
    let url = "/status?include_scope=true";
    assert_eq!(send(url, &source_only_token).await.unwrap().status(), StatusCode::FORBIDDEN);
    assert_eq!(send(url, &playlist_only_token).await.unwrap().status(), StatusCode::FORBIDDEN);
    let response = send(url, &writer_token).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let snapshot: PlaylistUpdateViewSnapshotDto =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(snapshot.scope.inputs[0].id, 2);
    assert_eq!(snapshot.status, PlaylistUpdateStatusDto::default());
}

#[tokio::test]
async fn playlist_update_status_reload_rejects_progress_for_a_reused_input_id() {
    use shared::model::{EventMessage, PlaylistUpdateProgressEvent};
    let temp = tempdir().unwrap();
    let input = Arc::new(ConfigInput { id: 2, name: "original".intern(), ..ConfigInput::default() });
    let config = Arc::new(test_app_config(input, ConfigSource { inputs: vec!["original".intern()], targets: vec![] }));
    config
        .config
        .store(Arc::new(Config { storage_dir: temp.path().to_string_lossy().into_owned(), ..Config::default() }));
    let app = test_app_state(config.clone());
    let mut old_event =
        PlaylistUpdateProgressEvent::for_run_input("old-run".into(), 1.into(), 2, "Download", "old input");
    old_event.scope_revision = Some(config.sources.load().playlist_update_scope().revision());
    app.event_manager.send_event(EventMessage::PlaylistUpdateProgress(old_event.clone()));
    let Json(old) = super::super::playlist_update_status(State(app.clone())).await.unwrap();
    assert_eq!(old.active_updates, [old_event]);

    let mut sources = (*config.sources.load_full()).clone();
    Arc::make_mut(&mut sources.inputs[0]).name = "replacement".intern();
    sources.sources[0].inputs[0] = "replacement".intern();
    config.sources.store(Arc::new(sources));
    let Json(new) = super::super::playlist_update_status(State(app)).await.unwrap();
    assert_eq!(new.inputs[0].input_id, 2);
    assert_eq!(new.active_updates, [] as [PlaylistUpdateProgressEvent; 0]);
}

#[tokio::test]
async fn playlist_update_status_reload_returns_active_bus_facts_without_persisting_updating() {
    use shared::model::{EventMessage, PlaylistUpdateProgressEvent, PlaylistUpdateState, PlaylistUpdateSummary};
    let temp = tempdir().unwrap();
    let input = Arc::new(ConfigInput {
        id: 7,
        name: "reload-input".intern(),
        input_type: InputType::M3u,
        enabled: true,
        ..ConfigInput::default()
    });
    let config = test_app_config(input.clone(), ConfigSource { inputs: vec![input.name.clone()], targets: Vec::new() });
    config
        .config
        .store(Arc::new(Config { storage_dir: temp.path().to_string_lossy().into_owned(), ..Config::default() }));
    let app = test_app_state(Arc::new(config));
    let path =
        crate::processing::input_cache::resolve_input_storage_path(&temp.path().to_string_lossy(), &input.name).await;
    let persisted = crate::processing::input_cache::InputStatus {
        last_input_update: Some(shared::model::PersistedPlaylistUpdateInputResult {
            state: PlaylistUpdateState::Success,
            timestamp: 1,
        }),
        ..crate::processing::input_cache::InputStatus::default()
    };
    crate::processing::input_cache::save_input_status(&path, &persisted);
    let before = std::fs::read(path.join("status.json")).unwrap();
    let mut progress =
        PlaylistUpdateProgressEvent::for_run_input("running".into(), 17.into(), 7, "reload-input", "updating");
    app.event_manager.send_event(EventMessage::PlaylistUpdateProgress(progress.clone()));
    let unknown = PlaylistUpdateProgressEvent::for_run_input("other".into(), 18.into(), 99, "removed", "updating");
    app.event_manager.send_event(EventMessage::PlaylistUpdateProgress(unknown));
    let Json(status) = super::super::playlist_update_status(State(app.clone())).await.unwrap();
    assert_eq!(status.active_updates, vec![progress.clone()]);
    assert_eq!(status.inputs[0].last_input_update, persisted.last_input_update);
    progress.state = Some(PlaylistUpdateState::Partial);
    app.event_manager.send_event(EventMessage::PlaylistUpdateProgress(progress.clone()));
    let Json(status) = super::super::playlist_update_status(State(app.clone())).await.unwrap();
    assert_eq!(status.active_updates, vec![progress], "completed input still belongs to a running target rebuild");
    app.event_manager.send_event(EventMessage::PlaylistUpdate(PlaylistUpdateSummary::for_run(
        "running".into(),
        17.into(),
        PlaylistUpdateState::Failure,
    )));
    let Json(status) = super::super::playlist_update_status(State(app)).await.unwrap();
    assert_eq!(status.active_updates.len(), 0);
    assert_eq!(
        std::fs::read(path.join("status.json")).unwrap(),
        before,
        "read-only runtime projection never writes transient status"
    );
}
