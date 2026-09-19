use super::{
    build_input_update_card_models, build_input_update_run_views, InputUpdateCardStatus,
    PlaylistUpdateCardStatusAction, PlaylistUpdateCardStatuses,
};
use crate::services::WebSocketConnectionContext;
use serde::Deserialize;
use shared::model::{
    ConfigInputDto, InputPlaylistUpdateStatusDto, InputType, PersistedPlaylistUpdateInputResult,
    PlaylistUpdateProgressEvent, PlaylistUpdateRunStateEvent, PlaylistUpdateState, PlaylistUpdateStatusDto,
    SourcesConfigDto,
};
use std::rc::Rc;
use yew::Reducible;

// This exact fixture is checked against a real parent/staged acquisition,
// canonical status.json and exec_processing's emitted global completion in
// tuliprox-processing::...::staged_completion_tests (playlist_update_status).
#[derive(Deserialize)]
struct StagedTrace {
    progress: Vec<PlaylistUpdateProgressEvent>,
    completed: PlaylistUpdateRunStateEvent,
    persisted_input_result: PersistedPlaylistUpdateInputResult,
}

fn traces() -> Vec<StagedTrace> {
    let traces: Vec<_> = serde_json::from_str(include_str!("test_data/staged_completions.json")).unwrap();
    assert_eq!(traces.len(), 3, "processing fixture must cover Partial, Success and Failure");
    traces
}

fn sources() -> SourcesConfigDto {
    SourcesConfigDto {
        // Names and order differ from the captured events: identities must carry.
        inputs: [(8, "renamed-staged", InputType::Staged), (9, "other", InputType::M3u), (7, "parent", InputType::M3u)]
            .into_iter()
            .map(|(id, name, input_type)| ConfigInputDto {
                id,
                name: name.into(),
                input_type,
                staged_type: shared::model::StagedInputType::Xtream,
                enabled: true,
                ..ConfigInputDto::default()
            })
            .collect(),
        ..SourcesConfigDto::default()
    }
}

#[test]
fn playlist_update_status_pipeline_transparency_reload_restores_running_target_rebuild_without_inheriting_follow_up_facts(
) {
    for trace in traces() {
        let connection = WebSocketConnectionContext::new(2, true);
        let own =
            trace.progress.iter().find(|event| event.input_id == Some(8) && event.state.is_some()).unwrap().clone();
        let sources = sources();
        let status = PlaylistUpdateStatusDto::default();
        let cards = build_input_update_card_models(&sources, &status);
        let mut state = Rc::new(PlaylistUpdateCardStatuses::for_connection(connection));
        state = state.reduce(PlaylistUpdateCardStatusAction::RestoreActive {
            connection_context: connection,
            progress: vec![own.clone()],
        });
        let restored = build_input_update_run_views(&sources, &cards, &state, &status).remove(0);
        assert_eq!(restored.overall_state, InputUpdateCardStatus::Updating);
        assert_eq!(restored.run_id, own.run_id);
        assert_eq!(restored.execution_order, own.execution_order);
        assert_eq!(state.for_input(8).input_telemetry(), own.input_telemetry.as_ref());
        state = state.reduce(PlaylistUpdateCardStatusAction::Accepted(super::PlaylistUpdateAcceptedScope {
            connection_context: connection,
            run_id: "follow-up".into(),
            input_ids: vec![8],
            refresh_policy: shared::model::InputRefreshPolicy::FORCE,
        }));
        let rebuilding = build_input_update_run_views(&sources, &cards, &state, &status).remove(0);
        assert_eq!(rebuilding.run_id, own.run_id);
        assert_eq!(rebuilding.overall_state, InputUpdateCardStatus::Updating);
        assert_eq!(rebuilding.input_state, restored.input_state);
        state = state.reduce(PlaylistUpdateCardStatusAction::Completed {
            connection_context: connection,
            completed: trace.completed,
        });
        // Even a late HTTP response from before Completed cannot resurrect R1.
        state = state.reduce(PlaylistUpdateCardStatusAction::RestoreActive {
            connection_context: connection,
            progress: vec![own],
        });
        let queued = build_input_update_run_views(&sources, &cards, &state, &status).remove(0);
        assert_eq!(queued.run_id, Some("follow-up".into()));
        assert_eq!(queued.overall_state, InputUpdateCardStatus::Queued);
        assert!(queued.progress_details.is_empty());
        assert!(state.for_input(8).input_telemetry().is_none());
    }
}

#[test]
fn playlist_update_status_reload_restores_updating_but_never_overwrites_live_or_completed_run() {
    let connection = WebSocketConnectionContext::new(2, true);
    let snapshot = PlaylistUpdateProgressEvent::for_run_input("current".into(), 7.into(), 8, "staged", "snapshot");
    let restore = || PlaylistUpdateCardStatusAction::RestoreActive {
        connection_context: connection,
        progress: vec![snapshot.clone()],
    };
    let empty = || Rc::new(PlaylistUpdateCardStatuses::for_connection(connection));
    let restored = empty().reduce(restore());
    assert_eq!(restored.for_input(8).status(), InputUpdateCardStatus::Updating);
    assert_eq!(restored.for_input(8).action_status(), InputUpdateCardStatus::Updating);
    let live = PlaylistUpdateProgressEvent::input_completed(
        "current".into(),
        7.into(),
        8,
        PlaylistUpdateState::Partial,
        "staged",
        "newer live fact",
    );
    let progressed =
        empty().reduce(PlaylistUpdateCardStatusAction::Progress { connection_context: connection, progress: live });
    let after = progressed.clone().reduce(restore());
    assert_eq!(*after, *progressed);
    let completed = empty().reduce(PlaylistUpdateCardStatusAction::Completed {
        connection_context: connection,
        completed: PlaylistUpdateRunStateEvent::correlated("current".into(), 7.into(), PlaylistUpdateState::Failure),
    });
    let after = completed.clone().reduce(restore());
    assert_eq!(*after, *completed, "terminal received before the snapshot wins even for a previously unknown run");
    let new_socket =
        empty().reduce(PlaylistUpdateCardStatusAction::ConnectionChanged(WebSocketConnectionContext::new(3, true)));
    let after = new_socket.clone().reduce(restore());
    assert_eq!(*after, *new_socket);
}

#[test]
fn playlist_update_status_pipeline_transparency_real_staged_trace_keeps_own_result_after_global_failure_and_reload() {
    for trace in traces() {
        let expected = match trace.persisted_input_result.state {
            PlaylistUpdateState::Success => InputUpdateCardStatus::Success,
            PlaylistUpdateState::Partial => InputUpdateCardStatus::Partial,
            PlaylistUpdateState::Failure => InputUpdateCardStatus::Failed,
        };
        let sources = sources();
        let persisted = PlaylistUpdateStatusDto {
            inputs: vec![InputPlaylistUpdateStatusDto {
                input_id: 8,
                last_update: Some(1),
                last_input_update: Some(trace.persisted_input_result),
                clusters: Vec::new(),
            }],
            ..PlaylistUpdateStatusDto::default()
        };
        let cards = build_input_update_card_models(&sources, &persisted);
        let connection = WebSocketConnectionContext::new(1, true);
        let mut state = Rc::new(PlaylistUpdateCardStatuses::for_connection(connection));
        for progress in trace.progress {
            state = state.reduce(PlaylistUpdateCardStatusAction::Progress { connection_context: connection, progress });
        }
        let before_terminal = build_input_update_run_views(&sources, &cards, &state, &persisted).remove(0);
        assert_eq!(before_terminal.input_state, expected);
        assert_eq!(before_terminal.overall_state, InputUpdateCardStatus::Updating);
        assert_eq!(before_terminal.input_id, 8);
        let staged_telemetry = state.for_input(8).input_telemetry().cloned().unwrap();
        assert_eq!(staged_telemetry.source, None, "must not inherit the M3U parent's provider label");
        assert_eq!(staged_telemetry.clusters.len(), 3);

        assert_eq!(trace.completed.state, PlaylistUpdateState::Failure);
        let run_id = trace.completed.run_id.clone();
        let order = trace.completed.execution_order;
        state = state.reduce(PlaylistUpdateCardStatusAction::Completed {
            connection_context: connection,
            completed: trace.completed,
        });
        let live = build_input_update_run_views(&sources, &cards, &state, &persisted).remove(0);
        assert_eq!(live.input_state, expected);
        assert_eq!(live.overall_state, InputUpdateCardStatus::Failed);
        assert_eq!(live.run_id, run_id);
        assert_eq!(live.execution_order, order);
        assert_eq!(state.for_input(8).input_telemetry(), Some(&staged_telemetry));
        assert_eq!(live.cluster_results.len(), staged_telemetry.clusters.len());
        for (visible, recorded) in live.cluster_results.iter().zip(&staged_telemetry.clusters) {
            assert_eq!(visible.cluster, recorded.cluster);
            assert_eq!(visible.outcome, recorded.decision);
            assert_eq!(visible.active_count, recorded.active_count);
        }

        let reloaded =
            state.reduce(PlaylistUpdateCardStatusAction::ConnectionChanged(WebSocketConnectionContext::new(2, true)));
        let reload = build_input_update_run_views(&sources, &cards, &reloaded, &persisted).remove(0);
        assert_eq!(reload.input_state, live.input_state);
        assert_eq!(
            reload.overall_state,
            InputUpdateCardStatus::Ready,
            "persisted input result proves no target/run result"
        );
        assert_eq!(reload.run_id, None);
        assert!(reload.progress_details.is_empty());
    }
}

#[test]
fn playlist_update_status_pipeline_transparency_unknown_completion_still_uses_global_failure_fallback() {
    let trace =
        traces().into_iter().find(|trace| trace.persisted_input_result.state == PlaylistUpdateState::Partial).unwrap();
    let connection = WebSocketConnectionContext::new(1, true);
    let mut state = Rc::new(PlaylistUpdateCardStatuses::for_connection(connection));
    let mut observed = false;
    for progress in trace.progress.into_iter().filter(|event| event.input_id != Some(8) || event.state.is_none()) {
        observed |= progress.input_id == Some(8);
        state = state.reduce(PlaylistUpdateCardStatusAction::Progress { connection_context: connection, progress });
    }
    assert!(observed, "the actual Rejection-progress observes the staged input without completing it");
    state = state.reduce(PlaylistUpdateCardStatusAction::Completed {
        connection_context: connection,
        completed: trace.completed,
    });
    let sources = sources();
    let persisted = PlaylistUpdateStatusDto {
        inputs: vec![InputPlaylistUpdateStatusDto {
            input_id: 8,
            last_update: Some(1),
            last_input_update: Some(trace.persisted_input_result),
            clusters: Vec::new(),
        }],
        ..PlaylistUpdateStatusDto::default()
    };
    let cards = build_input_update_card_models(&sources, &persisted);
    let live = build_input_update_run_views(&sources, &cards, &state, &persisted).remove(0);
    assert_eq!(
        live.input_state,
        InputUpdateCardStatus::Failed,
        "do not compensate missing runtime facts with persisted Partial"
    );
    assert_eq!(live.overall_state, InputUpdateCardStatus::Failed);
}
