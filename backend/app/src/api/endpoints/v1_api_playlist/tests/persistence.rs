use super::{test_app_config, test_app_state};
use crate::model::{Config, ConfigInput, ConfigSource};
use axum::{extract::State, Json};
use shared::{model::InputType, utils::Internable};
use std::sync::Arc;
use tempfile::tempdir;

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
    assert!(status.active_updates.is_empty());
    assert_eq!(
        std::fs::read(path.join("status.json")).unwrap(),
        before,
        "read-only runtime projection never writes transient status"
    );
}
